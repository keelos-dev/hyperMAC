//! Remote-desktop input handler: the connected RDP client's keyboard and mouse are applied
//! to this Mac's session, the same way Screen Sharing / VNC apply a remote operator's input.
//! Uses the OS input-synthesis API (CGEvent) and so needs the Accessibility (辅助功能)
//! permission in addition to Screen Recording.
//!
//! Text is delivered as Unicode (the client's already-composed characters), which sidesteps
//! keyboard-layout guessing; a small set of non-text keys (Return, Tab, arrows, Esc, …) and
//! the modifier keys are delivered as Mac virtual key codes. Mouse coordinates are already
//! global display points (the session is the primary display captured at point size), so no
//! scaling is applied.
//!
//! Click count: RDP sends only down/up, and a synthesized CGEvent always says "click 1" — the
//! WindowServer does not count clicks for posted events (Windows does, for SendInput). So a
//! double click never selected a word, a triple click never a line/paragraph (user 2026-11-09:
//! "鼠标三击不能全选"). The count is kept here: the same button pressed again within the
//! system's double-click interval and a few points of the last press counts up.

use ironrdp_server::{KeyboardEvent, MouseButton, MouseEvent, RdpServerInputHandler};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, CGScrollEventUnit,
};
use std::time::Instant;

/// Where a client last put the pointer, and when (cursor.rs tells a move the Mac made itself
/// from one the client made).
static CLIENT_POINTER: std::sync::Mutex<Option<((f64, f64), Instant)>> = std::sync::Mutex::new(None);

pub fn client_pointer() -> Option<((f64, f64), Instant)> {
    *CLIENT_POINTER.lock().unwrap_or_else(|e| e.into_inner())
}

fn note_client_pointer(p: CGPoint) {
    *CLIENT_POINTER.lock().unwrap_or_else(|e| e.into_inner()) = Some(((p.x, p.y), Instant::now()));
}

/// How far (points) the pointer may wander between the presses of one multi-click.
const CLICK_SLOP: f64 = 4.0;
use tracing::{debug, warn};

/// RDP set-1 scancode (+ extended bit) -> Mac virtual key code (kVK_*, Carbon HIToolbox
/// Events.h). Every physical key is forwarded, so the Mac's own keyboard layout and input
/// method compose the text — this is what lets a Mac-side IME (e.g. 拼音) work, the same as
/// a real keyboard or a VNC session. Modifier scancodes are handled by `modifier`, not here.
fn keycode(code: u8, extended: bool) -> Option<u16> {
    Some(match (code, extended) {
        (0x01, _) => 53,                                                          // Esc
        (0x02, _) => 18, (0x03, _) => 19, (0x04, _) => 20, (0x05, _) => 21,       // 1 2 3 4
        (0x06, _) => 23, (0x07, _) => 22, (0x08, _) => 26, (0x09, _) => 28,       // 5 6 7 8
        (0x0a, _) => 25, (0x0b, _) => 29,                                         // 9 0
        (0x0c, _) => 27, (0x0d, _) => 24,                                         // - =
        (0x0e, _) => 51,                                                          // Backspace
        (0x0f, _) => 48,                                                          // Tab
        (0x10, _) => 12, (0x11, _) => 13, (0x12, _) => 14, (0x13, _) => 15,       // q w e r
        (0x14, _) => 17, (0x15, _) => 16, (0x16, _) => 32, (0x17, _) => 34,       // t y u i
        (0x18, _) => 31, (0x19, _) => 35,                                         // o p
        (0x1a, _) => 33, (0x1b, _) => 30,                                         // [ ]
        (0x1c, false) => 36, (0x1c, true) => 76,                                  // Return / keypad Enter
        (0x1e, _) => 0, (0x1f, _) => 1, (0x20, _) => 2, (0x21, _) => 3,           // a s d f
        (0x22, _) => 5, (0x23, _) => 4, (0x24, _) => 38, (0x25, _) => 40,         // g h j k
        (0x26, _) => 37,                                                          // l
        (0x27, _) => 41, (0x28, _) => 39, (0x29, _) => 50,                        // ; ' `
        (0x2b, _) => 42,                                                          // backslash
        (0x2c, _) => 6, (0x2d, _) => 7, (0x2e, _) => 8, (0x2f, _) => 9,           // z x c v
        (0x30, _) => 11, (0x31, _) => 45, (0x32, _) => 46,                        // b n m
        (0x33, _) => 43, (0x34, _) => 47,                                         // , .
        (0x35, false) => 44, (0x35, true) => 75,                                  // / and keypad /
        (0x37, false) => 67,                                                      // keypad *
        (0x39, _) => 49,                                                          // Space
        (0x3b, _) => 122, (0x3c, _) => 120, (0x3d, _) => 99, (0x3e, _) => 118, (0x3f, _) => 96, // F1-F5
        (0x40, _) => 97, (0x41, _) => 98, (0x42, _) => 100, (0x43, _) => 101, (0x44, _) => 109, // F6-F10
        (0x47, false) => 89, (0x48, false) => 91, (0x49, false) => 92,            // keypad 7 8 9
        (0x4a, _) => 78,                                                          // keypad -
        (0x4b, false) => 86, (0x4c, false) => 87, (0x4d, false) => 88,            // keypad 4 5 6
        (0x4e, _) => 69,                                                          // keypad +
        (0x4f, false) => 83, (0x50, false) => 84, (0x51, false) => 85,            // keypad 1 2 3
        (0x52, false) => 82, (0x53, false) => 65,                                 // keypad 0 .
        (0x57, _) => 103, (0x58, _) => 111,                                       // F11 F12
        (0x47, true) => 115, (0x4f, true) => 119,                                 // Home End
        (0x49, true) => 116, (0x51, true) => 121,                                 // PgUp PgDn
        (0x52, true) => 114, (0x53, true) => 117,                                 // Insert->Help  Delete->fwd-del
        (0x48, true) => 126, (0x50, true) => 125,                                 // Up Down
        (0x4b, true) => 123, (0x4d, true) => 124,                                 // Left Right
        (0x5d, true) => 110,                                                      // Menu -> contextual menu
        _ => return None,
    })
}

/// A modifier scancode -> its CGEventFlags bit. Win=Command, Alt=Option (design.md).
fn modifier(code: u8, extended: bool) -> Option<CGEventFlags> {
    Some(match (code, extended) {
        (0x2a, _) | (0x36, _) => CGEventFlags::MaskShift,
        (0x1d, _) => CGEventFlags::MaskControl,
        (0x38, _) => CGEventFlags::MaskAlternate,
        (0x5b, true) | (0x5c, true) => CGEventFlags::MaskCommand,
        _ => return None,
    })
}

pub struct Input {
    flags: CGEventFlags,
    buttons: u8, // bit0 left, bit1 right, bit2 middle
    pos: CGPoint,
    /// (vertical px, horizontal px, modifier flags) to the `scroll_pump` thread.
    scroll_tx: std::sync::mpsc::Sender<(f64, f64, u64)>,
    /// the last press: (when, where, button bit), for the click count
    last_down: Option<(Instant, CGPoint, u8)>,
    /// click count of the current press (1 single, 2 double, 3 triple, …): set on the down,
    /// its drags and its up
    clicks: i64,
}

impl Input {
    pub fn new() -> Self {
        let (scroll_tx, scroll_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("scroll-pump".into())
            .spawn(move || scroll_pump(scroll_rx))
            .expect("spawn scroll pump thread");
        Self { flags: CGEventFlags::empty(), buttons: 0, pos: CGPoint { x: 0.0, y: 0.0 }, scroll_tx, last_down: None, clicks: 1 }
    }

    fn send(&self, ev: Option<objc2_core_foundation::CFRetained<CGEvent>>, what: &str) {
        match ev {
            Some(ev) => unsafe {
                CGEvent::set_flags(Some(&ev), self.flags);
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
            },
            None => warn!(what, "CGEvent not made (is Accessibility allowed for the app running hypermac?)"),
        }
    }

    fn type_unicode(&self, u: u16, down: bool) {
        let ev = CGEvent::new_keyboard_event(None, 0, down);
        if let Some(ev) = &ev {
            unsafe { CGEvent::keyboard_set_unicode_string(Some(ev), 1, &u) };
        }
        self.send(ev, "unicode");
    }

    /// A real Mac keyboard tags the navigation keys with Fn (and the arrows also with
    /// NumericPad); `send` overwrites the event's flags, so those tags are put back here —
    /// without them some apps don't take Home/End/PgUp/PgDn (or Shift+arrow) as such.
    fn virtual_key(&self, key: u16, down: bool) {
        let extra = match key {
            123..=126 => CGEventFlags::MaskSecondaryFn | CGEventFlags::MaskNumericPad, // arrows
            114..=117 | 119 | 121 => CGEventFlags::MaskSecondaryFn, // Help Home PgUp FwdDel End PgDn
            65 | 67 | 69 | 75 | 76 | 78 | 81..=92 => CGEventFlags::MaskNumericPad, // keypad
            _ => CGEventFlags::empty(),
        };
        let ev = CGEvent::new_keyboard_event(None, key, down);
        match ev {
            Some(ev) => unsafe {
                CGEvent::set_flags(Some(&ev), self.flags | extra);
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
            },
            None => warn!("CGEvent not made (is Accessibility allowed for the app running hypermac?)"),
        }
    }

    fn post_mouse(&self, kind: CGEventType, button: CGMouseButton) {
        let ev = CGEvent::new_mouse_event(None, kind, self.pos, button);
        if let Some(ev) = &ev {
            if kind != CGEventType::MouseMoved {
                CGEvent::set_integer_value_field(Some(ev), CGEventField::MouseEventClickState, self.clicks);
            }
        }
        self.send(ev, "mouse");
    }

    /// A press of `bit`: one more click when it follows the last press of the same button
    /// closely enough in time and place, else a new single click.
    fn count_click(&mut self, bit: u8) {
        let now = Instant::now();
        let interval = objc2_app_kit::NSEvent::doubleClickInterval();
        let again = self.last_down.is_some_and(|(t, p, b)| {
            b == bit
                && now.duration_since(t).as_secs_f64() <= interval
                && (p.x - self.pos.x).abs() <= CLICK_SLOP
                && (p.y - self.pos.y).abs() <= CLICK_SLOP
        });
        self.clicks = if again { self.clicks + 1 } else { 1 };
        self.last_down = Some((now, self.pos, bit));
    }
}

impl RdpServerInputHandler for Input {
    fn keyboard(&mut self, ev: KeyboardEvent) {
        match ev {
            KeyboardEvent::UnicodePressed(u) => self.type_unicode(u, true),
            KeyboardEvent::UnicodeReleased(u) => self.type_unicode(u, false),
            KeyboardEvent::Pressed { code, extended } | KeyboardEvent::Released { code, extended } => {
                let down = matches!(ev, KeyboardEvent::Pressed { .. });
                if let Some(bit) = modifier(code, extended) {
                    self.flags.set(bit, down);
                    // A lone modifier change still needs an event so the OS sees the chord.
                    self.send(CGEvent::new(None), "flags");
                } else if let Some(key) = keycode(code, extended) {
                    self.virtual_key(key, down);
                } else {
                    debug!(code, extended, "no Mac key for this scancode");
                }
            }
            other => debug!(?other, "keyboard event ignored"),
        }
    }

    fn mouse(&mut self, ev: MouseEvent) {
        match ev {
            MouseEvent::Move { x, y } => {
                self.pos = CGPoint { x: x as f64, y: y as f64 };
                note_client_pointer(self.pos);
                let kind = if self.buttons & 1 != 0 {
                    CGEventType::LeftMouseDragged
                } else if self.buttons & 2 != 0 {
                    CGEventType::RightMouseDragged
                } else {
                    CGEventType::MouseMoved
                };
                self.post_mouse(kind, CGMouseButton::Left);
            }
            MouseEvent::Button { x, y, button, pressed } => {
                self.pos = CGPoint { x: x as f64, y: y as f64 };
                note_client_pointer(self.pos);
                let (bit, cg, down_kind, up_kind) = match button {
                    MouseButton::Left => (1u8, CGMouseButton::Left, CGEventType::LeftMouseDown, CGEventType::LeftMouseUp),
                    MouseButton::Right => (2, CGMouseButton::Right, CGEventType::RightMouseDown, CGEventType::RightMouseUp),
                    MouseButton::Middle => (4, CGMouseButton::Center, CGEventType::OtherMouseDown, CGEventType::OtherMouseUp),
                    _ => return,
                };
                if pressed {
                    self.buttons |= bit;
                    self.count_click(bit);
                } else {
                    self.buttons &= !bit;
                }
                self.post_mouse(if pressed { down_kind } else { up_kind }, cg);
            }
            MouseEvent::VerticalScroll { value } => self.scroll(value, 0),
            MouseEvent::HorizontalScroll { value } => self.scroll(0, value),
            MouseEvent::Scroll { y, .. } => self.scroll(y.clamp(-10, 10) as i16, 0),
            other => debug!(?other, "mouse event ignored"),
        }
    }
}

impl Input {
    /// RDP wheel steps are 120 per notch and arrive as one lump. Posting that as a single
    /// line-unit event makes the Mac jump a few lines at once (Mac apps do not animate
    /// wheel scrolling; their smoothness comes from the trackpad's stream of small pixel
    /// deltas). So the wheel amount is converted to pixels and handed to `scroll_pump`,
    /// which pays it out as a stream of small pixel-unit events.
    fn scroll(&self, vertical: i16, horizontal: i16) {
        let px = scroll_px_per_notch() / 120.0;
        let msg = (vertical as f64 * px, horizontal as f64 * px, self.flags.bits());
        if self.scroll_tx.send(msg).is_err() {
            warn!("scroll pump thread is gone; wheel event dropped");
        }
    }
}

/// Pixels scrolled per wheel notch (MACRDP_SCROLL_PX, default 100 — what Chrome on Windows
/// scrolls per notch).
fn scroll_px_per_notch() -> f64 {
    static PX: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *PX.get_or_init(|| {
        std::env::var("MACRDP_SCROLL_PX").ok().and_then(|s| s.parse().ok()).filter(|v: &f64| *v > 0.0).unwrap_or(100.0)
    })
}

/// Pays the pending wheel distance out as pixel-unit scroll events, one every 8 ms, each
/// carrying a fixed share of what is left (ease-out: a notch is ~90 % done in 90 ms).
/// Notches that arrive mid-animation just add to the pending distance, so fast spinning
/// speeds the scroll up instead of queueing. Sub-pixel remainders are kept for the next
/// event, so high-resolution wheels (deltas below 120) are not lost.
fn scroll_pump(rx: std::sync::mpsc::Receiver<(f64, f64, u64)>) {
    const TICK: std::time::Duration = std::time::Duration::from_millis(8);
    const SHARE: f64 = 0.2;
    fn step(pending: &mut f64) -> i32 {
        if pending.abs() < 1.0 {
            return 0;
        }
        let mut s = (*pending * SHARE).round();
        if s == 0.0 {
            s = pending.signum();
        }
        *pending -= s;
        s as i32
    }

    let (mut pv, mut ph, mut flags) = (0.0f64, 0.0f64, 0u64);
    loop {
        if pv.abs() < 1.0 && ph.abs() < 1.0 {
            match rx.recv() {
                Ok((v, h, f)) => {
                    pv += v;
                    ph += h;
                    flags = f;
                }
                Err(_) => return, // the session's Input was dropped
            }
        }
        while let Ok((v, h, f)) = rx.try_recv() {
            pv += v;
            ph += h;
            flags = f;
        }
        let (sv, sh) = (step(&mut pv), step(&mut ph));
        if sv != 0 || sh != 0 {
            match CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, sv, sh, 0) {
                Some(ev) => unsafe {
                    CGEvent::set_flags(Some(&ev), CGEventFlags::from_bits_retain(flags));
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
                },
                None => warn!("scroll CGEvent not made (is Accessibility allowed for the app running hypermac?)"),
            }
        }
        std::thread::sleep(TICK);
    }
}
