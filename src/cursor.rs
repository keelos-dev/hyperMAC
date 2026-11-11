//! The Mac's pointer as RDP pointer updates: the client draws it with its own (hardware)
//! pointer, so it moves with the client's mouse at once instead of with the picture (late,
//! at the picture's frame rate, blurred by the encoder). The capture leaves it out of the
//! picture (capture.rs, showsCursor off).
//!
//! The shape is the system-wide cursor (NSCursor currentSystemCursor: whatever app owns it,
//! an I-beam, a hand, a resize arrow), read every POLL and sent when it changed. It is drawn
//! at its point size: the session is the display at point size (1 point = 1 pixel there).
//! Apple marks currentSystemCursor deprecated ("nil in a future version"); on macOS 27.2 it
//! still answers (tested 2026-11-10). Should it go nil, nothing is sent and the client keeps
//! its default arrow.
//!
//! A move the Mac makes itself (a program warping the pointer, a dialog putting it on its
//! default button) moves the client's pointer too: a position update, sent only when the
//! client has not moved for MOVED and the Mac's pointer is more than NEAR points from where
//! the client (or the last update) put it, so it never fights the client's own mouse
//! (keel-rdp's pointer.rs rule; user 2026-11-10: "mac那边移动是这边也移动需要").
//!
//! Not done, by choice: hiding the client's pointer while typing (user: not needed).

use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

use ironrdp_pdu::pointer::PointerPositionAttribute;
use ironrdp_server::{DisplayUpdate, RGBAPointer};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGEvent, CGImage, CGInterpolationQuality};
use tracing::{debug, info, warn};

const POLL: Duration = Duration::from_millis(30);
/// The largest pointer RGBAPointer carries (Large Pointer capability, 96x96; mstsc and
/// kRDPclient have it). A bigger cursor is drawn scaled down to it.
const MAX: usize = 96;
/// The client must have held still this long before the Mac's own moves are sent.
const MOVED: Duration = Duration::from_millis(300);
/// A Mac pointer farther than this (points) from where the client has it was moved by the Mac.
const NEAR: f64 = 8.0;

/// Starts watching the cursor for one session; its updates arrive on the receiver, and the
/// thread ends when the receiver is dropped.
pub fn start() -> tokio::sync::mpsc::Receiver<DisplayUpdate> {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    if let Err(e) = std::thread::Builder::new().name("cursor".into()).spawn(move || run(tx)) {
        warn!("cursor thread: {e} (the client keeps its default pointer)");
    }
    rx
}

fn run(tx: tokio::sync::mpsc::Sender<DisplayUpdate>) {
    let mut last: Option<u64> = None;
    let mut said_nil = false;
    let mut follow = Follow::default();
    while !tx.is_closed() {
        if let Some(update) = follow.step(mac_pointer(), crate::input::client_pointer(), std::time::Instant::now()) {
            if tx.blocking_send(update).is_err() {
                return;
            }
        }
        let shape = objc2::rc::autoreleasepool(|_| current());
        match shape {
            Some((hash, update)) if last != Some(hash) => {
                if let DisplayUpdate::RGBAPointer(p) = &update {
                    debug!(w = p.width, h = p.height, hot_x = p.hot_x, hot_y = p.hot_y, "pointer shape");
                }
                last = Some(hash);
                if tx.blocking_send(update).is_err() {
                    return;
                }
            }
            Some(_) => {}
            None if !said_nil => {
                said_nil = true;
                info!("no system cursor to read (currentSystemCursor nil): the client keeps its own pointer");
            }
            None => {}
        }
        std::thread::sleep(POLL);
    }
}

/// Where the Mac's pointer is (global points, top-left origin: what input.rs posts in).
fn mac_pointer() -> Option<(f64, f64)> {
    let ev = CGEvent::new(None)?;
    let p = CGEvent::location(Some(&ev));
    Some((p.x, p.y))
}

/// Whether the client's pointer must follow a move the Mac made itself.
#[derive(Default)]
struct Follow {
    /// where the client has its pointer: where it put it, or where the last update moved it
    expected: Option<(f64, f64)>,
    /// the client's last move taken into `expected`
    seen: Option<std::time::Instant>,
}

impl Follow {
    fn step(
        &mut self,
        mac: Option<(f64, f64)>,
        client: Option<((f64, f64), std::time::Instant)>,
        now: std::time::Instant,
    ) -> Option<DisplayUpdate> {
        let mac = mac?;
        if let Some((pos, t)) = client
            && self.seen != Some(t)
        {
            self.seen = Some(t);
            self.expected = Some(pos);
        }
        if client.is_some_and(|(_, t)| now.duration_since(t) < MOVED) {
            return None;
        }
        let Some(expected) = self.expected else {
            // nothing known of the client's pointer yet: from here on
            self.expected = Some(mac);
            return None;
        };
        if (mac.0 - expected.0).abs() <= NEAR && (mac.1 - expected.1).abs() <= NEAR {
            return None;
        }
        self.expected = Some(mac);
        let clamp = |v: f64| v.round().clamp(0.0, f64::from(u16::MAX)) as u16;
        debug!(x = mac.0, y = mac.1, "the Mac moved the pointer: the client's follows");
        Some(DisplayUpdate::PointerPosition(PointerPositionAttribute { x: clamp(mac.0), y: clamp(mac.1) }))
    }
}

/// The system cursor now as an RDP pointer update (and a hash of it), None when it cannot be
/// read.
fn current() -> Option<(u64, DisplayUpdate)> {
    #[allow(deprecated)]
    let cursor = objc2_app_kit::NSCursor::currentSystemCursor()?;
    let image = cursor.image();
    let size = image.size();
    let hot = cursor.hotSpot();
    let cg = bitmap(&image, size.width.min(MAX as f64))?;
    shape(&cg, (size.width, size.height), (hot.x, hot.y))
}

/// The image's smallest bitmap at least `need` pixels wide (else its largest), as a CGImage.
/// Not NSImage's CGImageForProposedRect: that checks this process in with LaunchServices as
/// the app, and since the service runs hyperMAC.app's executable, LaunchServices then takes
/// it for a running hyperMAC: a double-click on hyperMAC.app only "reopens" the service and no
/// window opens (1.0 83fc9fc on the MacBook Air, 2026-11-10). The bitmap's own CGImage does
/// not check in. A cursor with no bitmap (never seen) gives None: the client keeps its arrow.
fn bitmap(image: &objc2_app_kit::NSImage, need: f64) -> Option<objc2::rc::Retained<CGImage>> {
    let reps: Vec<_> = image
        .representations()
        .iter()
        .filter_map(|r| r.downcast::<objc2_app_kit::NSBitmapImageRep>().ok())
        .collect();
    let fits = reps.iter().filter(|r| r.pixelsWide() as f64 >= need).min_by_key(|r| r.pixelsWide());
    let rep = fits.or_else(|| reps.iter().max_by_key(|r| r.pixelsWide()))?;
    rep.CGImage()
}

/// `cg` drawn at `size` points (scaled down to MAX when bigger) into a square of 32, 64 or 96
/// pixels, top-left; rows bottom-up, BGRA with straight alpha (MS-RDPBCGR 2.2.9.1.1.4.4).
/// A cursor with no visible pixel hides the client's.
fn shape(cg: &CGImage, size: (f64, f64), hot: (f64, f64)) -> Option<(u64, DisplayUpdate)> {
    let (w, h) = (size.0.max(1.0), size.1.max(1.0));
    let k = (MAX as f64 / w.max(h)).min(1.0);
    let (dw, dh) = ((w * k).ceil() as usize, (h * k).ceil() as usize);
    let n = [32, 64, MAX].into_iter().find(|&n| dw <= n && dh <= n).unwrap_or(MAX);
    let mut px = vec![0u8; n * n * 4];
    let space = CGColorSpace::new_device_rgb()?;
    // premultiplied first + 32-bit little endian = B G R A in memory
    const BGRA_PREMULTIPLIED: u32 = 2 | (2 << 12);
    let ctx = unsafe { CGBitmapContextCreate(px.as_mut_ptr().cast(), n, n, 8, n * 4, Some(&space), BGRA_PREMULTIPLIED) }?;
    CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
    // CG's origin is bottom-left: the image's top at the context's top
    let rect = CGRect { origin: CGPoint { x: 0.0, y: n as f64 - h * k }, size: CGSize { width: w * k, height: h * k } };
    CGContext::draw_image(Some(&ctx), rect, Some(cg));
    drop(ctx);
    // memory rows are top-down; RDP wants bottom-up and straight alpha
    let mut data = Vec::with_capacity(px.len());
    for row in px.chunks_exact(n * 4).rev() {
        for p in row.chunks_exact(4) {
            let a = p[3];
            let un = |c: u8| if a == 0 { 0 } else { ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 };
            data.extend_from_slice(&[un(p[0]), un(p[1]), un(p[2]), a]);
        }
    }
    if data.chunks_exact(4).all(|p| p[3] == 0) {
        return Some((0, DisplayUpdate::HidePointer));
    }
    let hx = ((hot.0 * k).round().max(0.0) as usize).min(n - 1);
    let hy = ((hot.1 * k).round().max(0.0) as usize).min(n - 1);
    let mut hash = DefaultHasher::new();
    (n, hx, hy, &data).hash(&mut hash);
    let ptr = RGBAPointer { cache_index: 0, width: n as u16, height: n as u16, hot_x: hx as u16, hot_y: hy as u16, data };
    Some((hash.finish() | 1, DisplayUpdate::RGBAPointer(ptr)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn follows_the_mac_only_when_the_client_is_still() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut f = Follow::default();
        // the client puts the pointer at (100, 100); the Mac follows it: nothing to send
        let client = Some(((100.0, 100.0), ms(0)));
        assert!(f.step(Some((100.0, 100.0)), client, ms(10)).is_none());
        // the Mac lags behind while the client still moves: nothing (no fighting)
        assert!(f.step(Some((60.0, 60.0)), client, ms(100)).is_none());
        // the client held still; the Mac's pointer within NEAR: nothing
        assert!(f.step(Some((105.0, 98.0)), client, ms(400)).is_none());
        // a program warps it far away: the client's pointer follows, once
        let moved = f.step(Some((500.0, 300.0)), client, ms(500));
        assert!(matches!(moved, Some(DisplayUpdate::PointerPosition(p)) if (p.x, p.y) == (500, 300)));
        assert!(f.step(Some((500.0, 300.0)), client, ms(530)).is_none());
        // the client moves again: that is where it is now
        let client = Some(((520.0, 310.0), ms(600)));
        assert!(f.step(Some((520.0, 310.0)), client, ms(1000)).is_none());
    }

    /// Needs a logged-in Mac (an ssh session in it is enough): `cargo test cursor -- --ignored`.
    #[test]
    #[ignore]
    fn reads_the_system_cursor() {
        let (_, update) = objc2::rc::autoreleasepool(|_| current()).expect("currentSystemCursor answered");
        let DisplayUpdate::RGBAPointer(p) = update else { panic!("the arrow is not hidden") };
        assert!([32, 64, 96].contains(&p.width) && p.width == p.height);
        assert!(p.hot_x < p.width && p.hot_y < p.height);
        assert_eq!(p.data.len(), usize::from(p.width) * usize::from(p.height) * 4);
        // the hot spot's own pixel (rows bottom-up) belongs to the arrow's tip
        let row = usize::from(p.height) - 1 - usize::from(p.hot_y);
        let a = p.data[(row * usize::from(p.width) + usize::from(p.hot_x)) * 4 + 3];
        println!("{}x{} hot ({}, {}) alpha at hot {a}", p.width, p.height, p.hot_x, p.hot_y);
        assert!(a > 0, "the hot spot is on the shape");
        // reading it must not check this process in with LaunchServices (see bitmap())
        let o = std::process::Command::new("/usr/bin/lsappinfo")
            .args(["info", "-only", "pid", &std::process::id().to_string()])
            .output()
            .expect("lsappinfo");
        assert!(!String::from_utf8_lossy(&o.stdout).contains("pid"), "checked in with LaunchServices");
    }
}
