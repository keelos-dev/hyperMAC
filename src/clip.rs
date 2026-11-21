//! The clipboard channel (CLIPRDR), text and pictures both ways (design.md "剪贴板"; keel-rdp
//! clip.rs is the model, minus the guest agent and files; pictures: clipimg.rs):
//!
//! - Mac → client: a thread watches NSPasteboard's changeCount; a new string there is offered
//!   as CF_UNICODETEXT (+ CF_TEXT), a picture as CF_DIB + "PNG"; the client asks for the data
//!   when the user pastes.
//! - Client → server: the client's format list arrives (on_remote_copy); text is taken first,
//!   else a picture ("PNG", then "JFIF", then CF_DIBV5 / CF_DIB). Text is fetched at once
//!   (SendInitiatePaste) and put on NSPasteboard, so a paste on the Mac finds it. A picture
//!   is only promised there and fetched when something on the Mac reads it (user,
//!   2026-11-21: a screenshot copied on a phone went over mobile data the moment it was
//!   taken, pasted or not): see `CHILD`. What we put there is remembered by changeCount so
//!   the watcher does not offer it back.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse,
    FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::impl_as_any;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

struct Inner {
    sender: Mutex<Option<UnboundedSender<ServerEvent>>>,
    /// the client's CLIPRDR is up (on_ready)
    ready: AtomicBool,
    /// NSPasteboard.changeCount last seen by the watcher, or produced by our own write
    seen: AtomicIsize,
    /// the format we asked the client for (on_remote_copy), answered in on_format_data_response
    asked: Mutex<Option<(ClipboardFormatId, Kind)>>,
    /// the client's address (see FROM_CLIENT)
    peer: std::net::IpAddr,
    /// a promised picture is being read on the Mac: where the client's answer goes
    wait: Mutex<Option<std::sync::mpsc::Sender<Option<Vec<u8>>>>>,
}

/// The picture a client copied, promised on the pasteboard and not fetched until it is read:
/// whose it is, the format to ask for, and its data once it was fetched (the two types are
/// asked for one after the other).
struct Pending {
    inner: Weak<Inner>,
    id: ClipboardFormatId,
    kind: Kind,
    fetched: Option<(Vec<u8>, Option<Vec<u8>>)>,
    /// the pasteboard's changeCount with the promise on it
    count: isize,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

/// How long a paste on the Mac waits for the client's picture (the reader itself gives up
/// after about six seconds).
const FETCH: Duration = Duration::from_secs(5);

/// The promise is held by a process of its own, this program run as `hypermac clip-promise`
/// (`promise_command`), not by the service: AppKit asks a promise's owner on the main thread's
/// run loop, and the service whose main thread ran its run loop for that was checked in with
/// LaunchServices the moment a client connected (2026-11-21; it must never be, cursor.rs
/// `bitmap`). The child says `ready <changeCount>` once the promise is on the pasteboard and
/// `want` when the picture is read; it is answered with two lengths (u32, little-endian: the
/// PNG's and the TIFF's, 0 0 for none) and the bytes. It ends when something else is copied,
/// or with the service.
static CHILD: Mutex<Option<std::process::Child>> = Mutex::new(None);

/// A picture is promised on the pasteboard; its changeCount.
fn promise() -> Option<isize> {
    use std::io::{BufRead, Write};
    let mut held = CHILD.lock().unwrap();
    if let Some(mut old) = held.take() {
        let _ = old.kill();
        let _ = old.wait();
    }
    let exe = std::env::current_exe().ok()?;
    let mut child = std::process::Command::new(exe)
        .arg("clip-promise")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .inspect_err(|e| warn!("CLIPRDR: clip-promise: {e}"))
        .ok()?;
    let mut answers = child.stdin.take()?;
    let mut says = std::io::BufReader::new(child.stdout.take()?);
    let mut line = String::new();
    says.read_line(&mut line).ok()?;
    let count = line.strip_prefix("ready ").and_then(|n| n.trim().parse().ok());
    if count.is_none() {
        warn!(line, "CLIPRDR: clip-promise did not start");
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    *held = Some(child);
    std::thread::Builder::new()
        .name("clip-promise".into())
        .spawn(move || {
            for line in says.lines() {
                if line.ok().as_deref() != Some("want") {
                    break;
                }
                let (png, tiff) = fetch().map(|(png, tiff)| (png, tiff.unwrap_or_default())).unwrap_or_default();
                let mut answer = Vec::with_capacity(8 + png.len() + tiff.len());
                answer.extend_from_slice(&(png.len() as u32).to_le_bytes());
                answer.extend_from_slice(&(tiff.len() as u32).to_le_bytes());
                answer.extend_from_slice(&png);
                answer.extend_from_slice(&tiff);
                if answers.write_all(&answer).and_then(|()| answers.flush()).is_err() {
                    break;
                }
            }
            // (the child ended: something else was copied)
            if let Some(child) = CHILD.lock().unwrap().as_mut() {
                let _ = child.try_wait();
            }
        })
        .ok()?;
    count
}

/// Set when the pasteboard has another owner: the child's work is done.
static OWNER_CHANGED: AtomicBool = AtomicBool::new(false);

objc2::define_class!(
    // SAFETY: NSObject has no subclassing requirements; the class has no state.
    #[unsafe(super(objc2::runtime::NSObject))]
    #[name = "HMClipOwner"]
    struct Owner;

    impl Owner {
        /// AppKit, on the child's main thread: something reads the promised picture.
        #[unsafe(method(pasteboard:provideDataForType:))]
        fn provide_data(&self, pb: &NSPasteboard, kind: &NSString) {
            provide(pb, crate::clipimg::is_tiff(kind));
        }

        #[unsafe(method(pasteboardChangedOwner:))]
        fn changed_owner(&self, _pb: &NSPasteboard) {
            OWNER_CHANGED.store(true, Ordering::Relaxed);
        }
    }
);

/// `hypermac clip-promise`: see `CHILD`. The main thread of a process of its own.
pub fn promise_command() -> anyhow::Result<()> {
    use std::io::Write;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const std::ffi::c_void;
        fn CFRunLoopRunInMode(mode: *const std::ffi::c_void, seconds: f64, return_after_source: u8) -> i32;
    }
    let owner: objc2::rc::Retained<Owner> = unsafe { objc2::msg_send![<Owner as objc2::AnyThread>::alloc(), init] };
    let count = crate::clipimg::promise_on(&NSPasteboard::generalPasteboard(), &owner);
    println!("ready {count}");
    std::io::stdout().flush()?;
    // until something else is copied, or the service is gone (then init is the parent)
    while !OWNER_CHANGED.load(Ordering::Relaxed) && unsafe { libc::getppid() } != 1 {
        // SAFETY: the main thread's own run loop: what is due is done.
        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.0, 0) };
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// In the child: the promised picture is read. The service is asked for it (once) and it is
/// given; a paste waits here for as long as the service waits for the client.
fn provide(pb: &NSPasteboard, tiff: bool) {
    use std::io::{Read, Write};
    static FORMS: Mutex<Option<(Vec<u8>, Vec<u8>)>> = Mutex::new(None);
    let mut forms = FORMS.lock().unwrap();
    if forms.is_none() {
        println!("want");
        let mut lens = [0u8; 8];
        let mut stdin = std::io::stdin().lock();
        if std::io::stdout().flush().is_err() || stdin.read_exact(&mut lens).is_err() {
            return;
        }
        let len = |at: usize| u32::from_le_bytes(lens[at..at + 4].try_into().unwrap()) as usize;
        let (mut png, mut tiff_data) = (vec![0; len(0)], vec![0; len(4)]);
        if stdin.read_exact(&mut png).is_err() || stdin.read_exact(&mut tiff_data).is_err() || png.is_empty() {
            return;
        }
        *forms = Some((png, tiff_data));
    }
    let Some((png, tiff_data)) = forms.as_ref() else { return };
    let data = if tiff { tiff_data } else { png };
    if !data.is_empty() {
        crate::clipimg::keep_promise(pb, tiff, data);
    }
}

fn fetch() -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let (inner, id, kind) = {
        let pending = PENDING.lock().unwrap();
        let p = pending.as_ref()?;
        if let Some(fetched) = &p.fetched {
            return Some(fetched.clone());
        }
        (p.inner.upgrade()?, p.id, p.kind)
    };
    let (tx, rx) = std::sync::mpsc::channel();
    *inner.wait.lock().unwrap() = Some(tx);
    let start = std::time::Instant::now();
    Clip(inner.clone()).send(ClipboardMessage::SendInitiatePaste(id));
    let answer = rx.recv_timeout(FETCH);
    inner.wait.lock().unwrap().take();
    let ms = start.elapsed().as_millis() as u64;
    let data = match answer {
        Ok(Some(data)) => data,
        // (told apart: a client that has it no longer, and one that does not answer)
        Ok(None) => {
            info!(?kind, ms, "CLIPRDR: a paste on the Mac asked for the client's picture: the client has none");
            return None;
        }
        Err(_) => {
            info!(?kind, ms, "CLIPRDR: a paste on the Mac asked for the client's picture: no answer in time");
            return None;
        }
    };
    let forms = match kind {
        Kind::Dib => crate::clipimg::dib_png(&data).and_then(|png| crate::clipimg::png_and_tiff(&png)),
        _ => crate::clipimg::png_and_tiff(&data),
    };
    info!(bytes = data.len(), ?kind, ms = start.elapsed().as_millis() as u64, ok = forms.is_some(), "CLIPRDR: the client's picture, fetched for a paste on the Mac");
    let forms = forms?;
    if let Some(p) = PENDING.lock().unwrap().as_mut()
        && p.id == id
        && Weak::ptr_eq(&p.inner, &Arc::downgrade(&inner))
    {
        p.fetched = Some(forms.clone());
    }
    Some(forms)
}

/// What is on the Mac's pasteboard came from a client: (changeCount of that write, the client's
/// address). It is not offered back to a connection from that address. A PC with two sessions
/// open (this Mac's desktop and a VM) otherwise went round for ever: one session's copy was put
/// on the pasteboard, the other session's watcher offered it to the same PC, which took it as a
/// new copy and sent it again: a 2.4 MB picture 2112 times in 13 minutes (2026-11-11).
/// Connections from other addresses are still offered it (copy on one PC, paste on another).
static FROM_CLIENT: Mutex<Option<(isize, std::net::IpAddr)>> = Mutex::new(None);

#[derive(Clone, Copy, Debug)]
enum Kind {
    Text,
    Png,
    /// a JPEG file's bytes (kRDPclient on a phone offers it as "JFIF": far smaller than a DIB)
    Jpeg,
    Dib,
}

/// The id we give the registered "PNG" format in our own format lists (any id from 0xC000 up;
/// the client maps it by name).
fn png_id() -> ClipboardFormatId {
    ClipboardFormatId::new(0xC0F0)
}

/// One connection's clipboard; the watcher thread holds a Weak and ends with it.
#[derive(Clone)]
pub struct Clip(Arc<Inner>);

pub struct Factory(Clip);

/// The Mac's general pasteboard text, if any.
/// NSPasteboard's general pasteboard is one object for the process and not safe to use from
/// two threads at once: with two connections (each has a watcher thread) the service crashed
/// in -[NSPasteboard _updateTypeCacheIfNeeded] (2026-11-11, three times). Every use takes this.
pub static PASTEBOARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn pasteboard_lock() -> std::sync::MutexGuard<'static, ()> {
    PASTEBOARD.lock().unwrap_or_else(|e| e.into_inner())
}

fn pasteboard_string() -> Option<String> {
    let _g = pasteboard_lock();
    let pb = NSPasteboard::generalPasteboard();
    let s = unsafe { pb.stringForType(NSPasteboardTypeString) }?;
    Some(s.to_string())
}

fn change_count() -> isize {
    let _g = pasteboard_lock();
    NSPasteboard::generalPasteboard().changeCount()
}

impl Clip {
    pub fn new(peer: std::net::IpAddr) -> (Self, Factory) {
        let c = Clip(Arc::new(Inner {
            sender: Mutex::new(None),
            ready: AtomicBool::new(false),
            seen: AtomicIsize::new(change_count()),
            asked: Mutex::new(None),
            peer: peer.to_canonical(),
            wait: Mutex::new(None),
        }));
        let weak = Arc::downgrade(&c.0);
        std::thread::Builder::new()
            .name("clip-watch".into())
            .spawn(move || watch(weak))
            .expect("clipboard watcher thread");
        (c.clone(), Factory(c))
    }

    fn send(&self, m: ClipboardMessage) {
        if let Some(tx) = &*self.0.sender.lock().unwrap() {
            let _ = tx.send(ServerEvent::Clipboard(m));
        }
    }

    /// Offer the Mac's current text to the client (its "copy" from our side).
    fn offer(&self) {
        if !self.0.ready.load(Ordering::Relaxed) {
            return;
        }
        // (what this client's own PC put there is not news to it)
        let now = change_count(); // (first: the two locks are never held together)
        if *FROM_CLIENT.lock().unwrap() == Some((now, self.0.peer)) {
            return;
        }
        let mut formats = Vec::new();
        if pasteboard_string().is_some() {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_TEXT));
        }
        if crate::clipimg::mac_has_image() {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
            formats.push(ClipboardFormat::new(png_id()).with_name(ClipboardFormatName::new_static("PNG")));
        }
        if !formats.is_empty() {
            debug!(n = formats.len(), "CLIPRDR: offering the Mac's clipboard");
            self.send(ClipboardMessage::SendInitiateCopy(formats));
        }
    }

    /// The client's text goes onto the Mac's pasteboard; our own change is remembered.
    fn put(&self, text: &str) {
        let _g = pasteboard_lock();
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        if !unsafe { pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString) } {
            warn!("CLIPRDR: NSPasteboard refused the text");
        }
        // (not change_count(): this thread holds the pasteboard's lock, taking it again would
        // wait for ever, and with it every later use of the clipboard, new connections too)
        let count = pb.changeCount();
        self.0.seen.store(count, Ordering::Relaxed);
        drop(_g);
        *FROM_CLIENT.lock().unwrap() = Some((count, self.0.peer));
        info!(chars = text.chars().count(), "CLIPRDR: the client's text is on the Mac's clipboard");
    }

    /// The client's picture goes onto the Mac's pasteboard (PNG + TIFF).
    fn put_image(&self, data: &[u8], kind: Kind) {
        let count = match kind {
            Kind::Png => crate::clipimg::put_png(data),
            _ => crate::clipimg::put_dib(data),
        };
        match count {
            Some(c) => {
                self.0.seen.store(c, Ordering::Relaxed);
                *FROM_CLIENT.lock().unwrap() = Some((c, self.0.peer));
                info!(bytes = data.len(), ?kind, "CLIPRDR: the client's picture is on the Mac's clipboard");
            }
            None => warn!(bytes = data.len(), ?kind, "CLIPRDR: could not take the client's picture"),
        }
    }
}

/// Polls NSPasteboard: a change that is not ours is offered to the client.
fn watch(weak: Weak<Inner>) {
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let Some(inner) = weak.upgrade() else { return };
        let now = change_count();
        if now != inner.seen.load(Ordering::Relaxed) {
            inner.seen.store(now, Ordering::Relaxed);
            Clip(inner).offer();
        }
    }
}

impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.0 .0.sender.lock().unwrap() = Some(sender);
    }
}

impl CliprdrBackendFactory for Factory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(Backend(self.0.clone()))
    }
}

impl CliprdrServerFactory for Factory {}

struct Backend(Clip);

impl_as_any!(Backend);

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clipboard").field("ready", &self.0 .0.ready.load(Ordering::Relaxed)).finish()
    }
}

/// UTF-16LE with the terminating NUL (CF_UNICODETEXT), or bytes with NUL (CF_TEXT: UTF-8
/// here; Windows reads it as the ANSI code page, so ASCII is what survives — the client
/// prefers CF_UNICODETEXT anyway).
fn encode(text: &str, format: ClipboardFormatId) -> Option<Vec<u8>> {
    match format {
        ClipboardFormatId::CF_UNICODETEXT => {
            let mut b: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
            b.extend_from_slice(&[0, 0]);
            Some(b)
        }
        ClipboardFormatId::CF_TEXT | ClipboardFormatId::CF_OEMTEXT => {
            let mut b = text.as_bytes().to_vec();
            b.push(0);
            Some(b)
        }
        _ => None,
    }
}

fn decode(data: &[u8], format: ClipboardFormatId) -> Option<String> {
    match format {
        ClipboardFormatId::CF_UNICODETEXT => {
            let u: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
            Some(String::from_utf16_lossy(&u))
        }
        ClipboardFormatId::CF_TEXT | ClipboardFormatId::CF_OEMTEXT => {
            let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
            Some(String::from_utf8_lossy(&data[..end]).into_owned())
        }
        _ => None,
    }
}

impl CliprdrBackend for Backend {
    fn temporary_directory(&self) -> &str {
        "/tmp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        info!("CLIPRDR: ready");
        self.0 .0.ready.store(true, Ordering::Relaxed);
        self.0 .0.seen.store(change_count(), Ordering::Relaxed);
        // what is on the Mac's clipboard now is pasteable on the client at once
        self.0.offer();
    }

    fn on_request_format_list(&mut self) {
        self.0.offer();
    }

    fn on_process_negotiated_capabilities(&mut self, _: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
        let has = |id: ClipboardFormatId| formats.iter().any(|f| f.id() == id);
        let text = [ClipboardFormatId::CF_UNICODETEXT, ClipboardFormatId::CF_TEXT, ClipboardFormatId::CF_OEMTEXT]
            .into_iter()
            .find(|w| has(*w))
            .map(|id| (id, Kind::Text));
        let png = formats
            .iter()
            .find(|f| f.name().is_some_and(|n| n.value().eq_ignore_ascii_case("PNG")))
            .map(|f| (f.id(), Kind::Png));
        let jpeg = formats
            .iter()
            .find(|f| f.name().is_some_and(|n| n.value().eq_ignore_ascii_case("JFIF")))
            .map(|f| (f.id(), Kind::Jpeg));
        let dib = [ClipboardFormatId::CF_DIBV5, ClipboardFormatId::CF_DIB].into_iter().find(|w| has(*w)).map(|id| (id, Kind::Dib));
        match text.or(png).or(jpeg).or(dib) {
            Some((id, kind @ Kind::Text)) => {
                debug!(?id, ?kind, "CLIPRDR: the client copied text, fetching it");
                *self.0 .0.asked.lock().unwrap() = Some((id, kind));
                self.0.send(ClipboardMessage::SendInitiatePaste(id));
            }
            // a picture: promised, and fetched when it is pasted
            Some((id, kind)) => {
                // (the pending one first, with no count yet: a reader may come at once)
                *PENDING.lock().unwrap() =
                    Some(Pending { inner: Arc::downgrade(&self.0 .0), id, kind, fetched: None, count: -1 });
                let Some(count) = promise() else {
                    *PENDING.lock().unwrap() = None;
                    return;
                };
                if let Some(p) = PENDING.lock().unwrap().as_mut() {
                    p.count = count;
                }
                self.0 .0.seen.store(count, Ordering::Relaxed);
                *FROM_CLIENT.lock().unwrap() = Some((count, self.0 .0.peer));
                info!(?kind, "CLIPRDR: the client copied a picture: on the Mac's clipboard as a promise");
            }
            None => debug!(n = formats.len(), "CLIPRDR: the client copied nothing we take (files?)"),
        }
    }

    fn on_format_data_request(&mut self, r: FormatDataRequest) {
        // What is on the pasteboard is a client's picture not fetched yet: it is not read
        // from the pasteboard here. Reading it there makes AppKit ask the promise's owner on
        // the main thread, which asks the client and waits, while this thread, the one that
        // would take the client's answer, waits for the read: the session stood still for
        // five seconds, twice (2026-11-21, a client asking for what was offered before its
        // own picture came). The client's own picture is not given back to it; another
        // client's is fetched from that one, as PNG.
        let promised = {
            let now = change_count();
            let pending = PENDING.lock().unwrap();
            pending.as_ref().filter(|p| p.count == now).map(|p| p.inner.as_ptr() == Arc::as_ptr(&self.0 .0))
        };
        if let Some(own) = promised {
            let reply = (!own && r.format == png_id()).then(fetch).flatten().map(|(png, _)| png);
            debug!(own, ok = reply.is_some(), "CLIPRDR: a client asked for a picture that is another's promise");
            self.0.send(ClipboardMessage::SendFormatData(match reply {
                Some(b) => FormatDataResponse::new_data(b),
                None => FormatDataResponse::new_error(),
            }));
            return;
        }
        let reply = match r.format {
            ClipboardFormatId::CF_DIB => crate::clipimg::mac_dib(),
            f if f == png_id() => crate::clipimg::mac_png(),
            f => pasteboard_string().and_then(|t| encode(&t, f)),
        };
        self.0.send(ClipboardMessage::SendFormatData(match reply {
            Some(b) => FormatDataResponse::new_data(b),
            None => FormatDataResponse::new_error(),
        }));
    }

    fn on_format_data_response(&mut self, r: FormatDataResponse<'_>) {
        // (a promised picture, asked for by a paste on the Mac: to who waits for it)
        if let Some(tx) = self.0 .0.wait.lock().unwrap().take() {
            let _ = tx.send((!r.is_error()).then(|| r.data().to_vec()));
            return;
        }
        let Some((id, kind)) = self.0 .0.asked.lock().unwrap().take() else { return };
        if r.is_error() {
            debug!("CLIPRDR: the client had no data after all");
            return;
        }
        match kind {
            Kind::Text => match decode(r.data(), id) {
                Some(t) => self.0.put(&t),
                None => debug!(?id, "CLIPRDR: data in a format we do not take"),
            },
            Kind::Png | Kind::Jpeg | Kind::Dib => self.0.put_image(r.data(), kind),
        }
    }

    fn on_file_contents_request(&mut self, _: FileContentsRequest) {
        debug!("CLIPRDR: files are not shared");
    }

    fn on_file_contents_response(&mut self, _: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _: LockDataId) {}

    fn on_unlock(&mut self, _: LockDataId) {}
}

#[cfg(test)]
mod pasteboard_tests {
    use super::*;

    /// A promised picture is asked of the client when it is read, once: what the child is
    /// answered with (the child and AppKit's part were tried by hand: a test's thread is not
    /// a main one).
    #[test]
    fn a_picture_is_fetched_when_it_is_read() {
        let (w, h) = (4usize, 3usize);
        let px: Vec<u8> = (0..w * h).flat_map(|i| [(i * 20) as u8, 100, (255 - i * 20) as u8, 255]).collect();
        let dib = crate::clipimg::to_dib(&crate::clipimg::Rgba { w, h, px: px.clone() });
        let inner = Arc::new(Inner {
            sender: Mutex::new(None),
            ready: AtomicBool::new(true),
            seen: AtomicIsize::new(0),
            asked: Mutex::new(None),
            peer: "192.168.1.10".parse().unwrap(),
            wait: Mutex::new(None),
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *inner.sender.lock().unwrap() = Some(tx);
        // the client: answers each request for the DIB, and counts them
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (client, count, answer) = (inner.clone(), asked.clone(), dib.clone());
        std::thread::spawn(move || {
            while let Some(event) = rx.blocking_recv() {
                if let ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(id)) = event {
                    assert_eq!(id, ClipboardFormatId::CF_DIB);
                    count.fetch_add(1, Ordering::Relaxed);
                    let mut backend = Backend(Clip(client.clone()));
                    backend.on_format_data_response(FormatDataResponse::new_data(answer.clone()));
                }
            }
        });
        *PENDING.lock().unwrap() =
            Some(Pending { inner: Arc::downgrade(&inner), id: ClipboardFormatId::CF_DIB, kind: Kind::Dib, fetched: None, count: -1 });
        assert_eq!(asked.load(Ordering::Relaxed), 0, "nothing is fetched for the promise");
        let (png, tiff) = fetch().expect("the picture");
        assert_eq!(&png[1..4], b"PNG");
        assert!(tiff.is_some_and(|tiff| !tiff.is_empty()));
        assert!(fetch().is_some());
        assert_eq!(asked.load(Ordering::Relaxed), 1, "one fetch, whoever asks again");
        // the client gone: a paste gets nothing, at once
        drop(inner);
        *PENDING.lock().unwrap() = Some(Pending { inner: Weak::new(), id: ClipboardFormatId::CF_DIB, kind: Kind::Dib, fetched: None, count: -1 });
        let start = std::time::Instant::now();
        assert!(fetch().is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
        *PENDING.lock().unwrap() = None;
    }

    /// Uses the Mac's real clipboard (and leaves a line of text on it): run by hand,
    /// `cargo test --release -- --ignored clipboard_text_and_echo`.
    /// - a client's text is put on the pasteboard and the call returns (it once waited for ever
    ///   on its own lock, and with it every later connection);
    /// - it is not offered back to a connection from the same address, and is to another one.
    #[test]
    #[ignore]
    fn clipboard_text_and_echo() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let here: std::net::IpAddr = "192.168.1.10".parse().unwrap();
            let conn = |ip: std::net::IpAddr| {
                let (c, _f) = Clip::new(ip);
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                *c.0.sender.lock().unwrap() = Some(tx);
                c.0.ready.store(true, Ordering::Relaxed);
                (c, rx)
            };
            let (a, mut a_rx) = conn(here);
            let (b, mut b_rx) = conn(here);
            let (other, mut other_rx) = conn("192.168.1.11".parse().unwrap());
            a.put("hyperMAC clipboard test");
            assert_eq!(pasteboard_string().as_deref(), Some("hyperMAC clipboard test"));
            a.offer();
            b.offer();
            other.offer();
            assert!(a_rx.try_recv().is_err(), "offered back to the session that sent it");
            assert!(b_rx.try_recv().is_err(), "offered to another session of the same PC");
            assert!(other_rx.try_recv().is_ok(), "not offered to another PC");
            let _ = done_tx.send(());
        });
        done_rx.recv_timeout(Duration::from_secs(10)).expect("the clipboard calls did not return (a lock held twice?) or an assertion failed");
    }
}
