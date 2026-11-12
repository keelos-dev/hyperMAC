//! Coherence: each window of a Windows guest as a window of the Mac. No guest desktop, no
//! taskbar: the guest's programs sit among the Mac's.
//!
//! How: the guest's screen gets the size of the Mac's (one guest pixel a point), so a place on
//! one is the same place on the other. An agent in the guest (vm/tools/win-agent/hmagent.cs)
//! says which windows are where; for each one this process keeps a borderless window at that
//! place whose content is that part of the guest's picture. The windows' frames are Windows'
//! own, drawn by the guest: a click on its close button is a click in the guest, a drag of its
//! title bar moves it in the guest, and the agent's next list moves the Mac's window after it.
//! The pointer and the keys go to the guest as they do from the console (vinput), by where the
//! pointer is on the Mac's screen.
//!
//! Where one guest window covers another, the picture has the upper one's pixels: the Mac's
//! windows are stacked like the guest's, so the upper one covers that part here too. A window
//! of another Mac program between two overlapping guest windows shows the limit of this: the
//! lower one then shows a piece of the upper one. (Each window's own pixels would need the
//! guest's compositor to hand them over one by one.)
//!
//! `ctl NAME coherence on|off|run COMMAND|status`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEvent, NSImage, NSResponder, NSScreen, NSStatusBar, NSStatusItem, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::CFData;
use objc2_core_graphics::{CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize, NSString};

/// the agent's port in the guest
const PORT: u16 = 47800;
/// Windows 11 rounds its windows' corners by this much; outside the curve the picture has the
/// desktop behind the window, which the Mac's window cuts away
const CORNER: f64 = 8.0;

/// One window as the agent tells it. x, y, w, h: guest pixels, from the screen's top left.
#[derive(Clone, Debug, PartialEq)]
struct Win {
    hwnd: u64,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    /// a menu, tooltip or drop-down: shown, never the key window
    popup: bool,
    title: String,
}

#[derive(Clone, Default)]
struct Snap {
    /// counts the lists received
    seq: u64,
    /// the guest's foreground window
    fg: u64,
    /// top first
    wins: Vec<Win>,
}

static ON: AtomicBool = AtomicBool::new(false);
static SNAP: Mutex<Snap> = Mutex::new(Snap { seq: 0, fg: 0, wins: Vec::new() });
/// the connection's writing side
static LINK: Mutex<Option<Conn>> = Mutex::new(None);
/// for `status`: what the connection is doing
static STATE: Mutex<String> = Mutex::new(String::new());
/// counts start(): a connection thread of an earlier start ends when it sees another number
static RUN: AtomicU64 = AtomicU64::new(0);
/// the Mac's screen the guest's is laid over: x, y, width, height (points, origin bottom left)
static SCREEN: Mutex<(f64, f64, f64, f64)> = Mutex::new((0.0, 0.0, 0.0, 0.0));
/// ... and the part of it the menu bar and the Dock leave
static VISIBLE: Mutex<(f64, f64, f64, f64)> = Mutex::new((0.0, 0.0, 0.0, 0.0));
/// the guest's screen size before, to go back to
static BEFORE: Mutex<Option<(u32, u32)>> = Mutex::new(None);
static PUMP: std::sync::Once = std::sync::Once::new();
/// a tick is queued on the main thread and not done yet
static QUEUED: AtomicBool = AtomicBool::new(false);
static WHEEL: Mutex<f64> = Mutex::new(0.0);

/// One line to the agent.
fn send(line: &str) {
    if let Some(s) = LINK.lock().unwrap().as_mut() {
        let _ = s.write_all(format!("{line}\n").as_bytes());
    }
}

/// The agent's list, as its lines come: Some when a list is complete.
fn parse(line: &str, building: &mut Option<Snap>) -> Option<Snap> {
    let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
    match kind {
        "S" => {
            let fg = rest.split(' ').nth(2).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0) as u64;
            *building = Some(Snap { seq: 0, fg, wins: Vec::new() });
        }
        "W" => {
            let (head, title) = rest.split_once('\t').unwrap_or((rest, ""));
            let f: Vec<&str> = head.split(' ').collect();
            let n = |i: usize| f.get(i).and_then(|v| v.parse::<i64>().ok());
            if let (Some(b), Some(hwnd), Some(x), Some(y), Some(w), Some(h), Some(flags)) = (building.as_mut(), n(0), n(1), n(2), n(3), n(4), n(5)) {
                b.wins.push(Win { hwnd: hwnd as u64, x: x as i32, y: y as i32, w: w as i32, h: h as i32, popup: flags & 1 != 0, title: title.to_string() });
            }
        }
        "E" => return building.take(),
        _ => {}
    }
    None
}

/// A way to the agent: the machine's own channel (a virtio-console port: a socket in the VM's
/// folder on this side, a named port in the guest; nothing of the network in it, so no
/// address, no firewall, and nothing macOS asks the user about), or TCP to the guest's address
/// for a guest without that port's driver.
enum Conn {
    Port(std::os::unix::net::UnixStream),
    Tcp(TcpStream),
}

impl Conn {
    /// `to`: "unix:PATH" or "HOST:PORT".
    fn open(to: &str) -> Option<Conn> {
        match to.strip_prefix("unix:") {
            Some(path) => std::os::unix::net::UnixStream::connect(path).ok().map(Conn::Port),
            None => {
                let a = to.to_socket_addrs().ok()?.next()?;
                let s = TcpStream::connect_timeout(&a, Duration::from_secs(2)).ok()?;
                let _ = s.set_nodelay(true);
                Some(Conn::Tcp(s))
            }
        }
    }
    fn write_all(&mut self, b: &[u8]) -> std::io::Result<()> {
        match self {
            Conn::Port(s) => s.write_all(b),
            Conn::Tcp(s) => s.write_all(b),
        }
    }
    fn reader(&self, timeout: Duration) -> std::io::Result<Box<dyn std::io::Read + Send>> {
        Ok(match self {
            Conn::Port(s) => {
                let r = s.try_clone()?;
                r.set_read_timeout(Some(timeout))?;
                Box::new(r)
            }
            Conn::Tcp(s) => {
                let r = s.try_clone()?;
                r.set_read_timeout(Some(timeout))?;
                Box::new(r)
            }
        })
    }
    fn shutdown(&self) {
        let _ = match self {
            Conn::Port(s) => s.shutdown(std::net::Shutdown::Both),
            Conn::Tcp(s) => s.shutdown(std::net::Shutdown::Both),
        };
    }
}

/// Keep a connection to the agent while coherence is on. `ways`: where it may be reached, the
/// best first; the one that answered is tried first the next time.
fn link(run: u64, mut ways: Vec<String>, token: String) {
    let live = move || ON.load(Ordering::Relaxed) && RUN.load(Ordering::Relaxed) == run;
    let say = |s: &str| *STATE.lock().unwrap() = s.to_string();
    let mut at = 0;
    while live() && !ways.is_empty() {
        let way = ways[at % ways.len()].clone();
        let mut heard = false;
        if let Some(mut s) = Conn::open(&way) {
            // A connection can die without a word (the guest was reset; on the port there is
            // no such thing as the other side hanging up at all): the agent is asked for its
            // list every second, and three seconds without a line from it is a dead
            // connection, to be made again. The same three seconds are how long an agent that
            // is not there (not installed, nobody logged on) gets to answer.
            if let (Ok(()), Ok(reader)) = (s.write_all(format!("T {token}\n").as_bytes()), s.reader(Duration::from_secs(3))) {
                *LINK.lock().unwrap() = Some(s);
                let up = std::sync::Arc::new(AtomicBool::new(true));
                let ping = up.clone();
                std::thread::spawn(move || {
                    while ping.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_secs(1));
                        send("R");
                    }
                });
                // the taskbar out of the way; the work area follows when the guest's size is known (tick)
                send("B 1");
                send("R");
                let mut building = None;
                for line in BufReader::new(reader).lines().map_while(Result::ok) {
                    if !live() {
                        break;
                    }
                    if let Some(mut snap) = parse(&line, &mut building) {
                        if !heard {
                            heard = true;
                            say("connected");
                        }
                        let mut g = SNAP.lock().unwrap();
                        snap.seq = g.seq + 1;
                        *g = snap;
                    }
                }
                up.store(false, Ordering::Relaxed);
                if let Some(s) = LINK.lock().unwrap().take() {
                    s.shutdown();
                }
            }
        }
        if !live() {
            break;
        }
        if heard {
            // it was there a moment ago: the same way again, at once
            say("connecting");
            let i = at % ways.len();
            ways.swap(0, i);
            at = 0;
        } else {
            at += 1;
            if at % ways.len() == 0 {
                say("no answer from the agent in the guest (is it installed there, and someone logged on?)");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

// ---- the Mac's windows -----------------------------------------------------------------------

struct WinIvars {
    hwnd: Cell<u64>,
    popup: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HyperMACCoWindow"]
    #[ivars = WinIvars]
    struct CoWindow;

    impl CoWindow {
        // (a borderless window refuses both by default)
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            !self.ivars().popup.get()
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            !self.ivars().popup.get()
        }

        // Where the guest has the window is where it is: also under the menu bar or over the
        // screen's edge, where AppKit would push a window back onto the screen.
        #[unsafe(method(constrainFrameRect:toScreen:))]
        fn constrain(&self, frame: NSRect, _screen: Option<&NSScreen>) -> NSRect {
            frame
        }

        // the keys go where the guest's foreground window is: keep that this one
        #[unsafe(method(becomeKeyWindow))]
        fn become_key(&self) {
            let _: () = unsafe { msg_send![super(self), becomeKeyWindow] };
            send(&format!("A {}", self.ivars().hwnd.get()));
        }
    }
);

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HyperMACCoView"]
    struct CoView;

    impl CoView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        // the click that brings the window forward is a click in the guest too
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, e: &NSEvent) {
            crate::console::key(e, true);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, e: &NSEvent) {
            crate::console::key(e, false);
        }

        // Command combinations come here and their keyUp never comes: the key is tapped
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, e: &NSEvent) -> bool {
            if let Some(code) = crate::console::mac_to_linux(e.keyCode()) {
                crate::vinput::key(code, true);
                crate::vinput::key(code, false);
            }
            true
        }

        // Here the left Command key is the guest's Control: Cmd-C copies, Cmd-V pastes, as in
        // the Mac's programs around these windows. The right one is the Windows key.
        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, e: &NSEvent) {
            if e.keyCode() == 0x37 {
                // NX_DEVICELCMDKEYMASK
                crate::vinput::key(29, e.modifierFlags().0 & 0x08 != 0);
            } else {
                crate::console::modifier(e);
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, e: &NSEvent) {
            pointer(e, None);
        }
        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _e: &NSEvent) {}
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, e: &NSEvent) {
            pointer(e, None);
        }
        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, e: &NSEvent) {
            pointer(e, None);
        }
        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, e: &NSEvent) {
            pointer(e, None);
        }
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            pointer(e, Some((0, true)));
        }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, e: &NSEvent) {
            pointer(e, Some((0, false)));
        }
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, e: &NSEvent) {
            pointer(e, Some((1, true)));
        }
        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, e: &NSEvent) {
            pointer(e, Some((1, false)));
        }
        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, e: &NSEvent) {
            pointer(e, Some((2, true)));
        }
        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, e: &NSEvent) {
            pointer(e, Some((2, false)));
        }
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, e: &NSEvent) {
            let d = if e.hasPreciseScrollingDeltas() { e.scrollingDeltaY() / 30.0 } else { e.scrollingDeltaY() };
            let mut acc = WHEEL.lock().unwrap();
            *acc += d;
            let steps = acc.trunc();
            if steps != 0.0 {
                *acc -= steps;
                pointer(e, None);
                crate::vinput::wheel(steps as i32);
            }
        }
    }
);

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventGetLocation(event: *const std::ffi::c_void) -> NSPoint;
}

/// A display point (from the top left of the screen with the menu bar) as 0..1 of the guest's screen.
fn display_point(p: NSPoint) -> (f64, f64) {
    let (sx, _, sw, sh) = *SCREEN.lock().unwrap();
    ((p.x - sx) / sw, p.y / sh)
}

/// The pointer is where it is on the Mac's screen: the same place on the guest's. By the
/// screen, not by the window under it: while a title bar is dragged the window runs after the
/// pointer, a little late. The place is the event's own (where the pointer was when it
/// happened: display points from the top left of the screen with the menu bar, which is the
/// screen the guest's is laid over).
fn pointer(e: &NSEvent, button: Option<(u16, bool)>) {
    let (sx, sy, sw, sh) = *SCREEN.lock().unwrap();
    if sw <= 0.0 || sh <= 0.0 {
        return;
    }
    let cg: *const std::ffi::c_void = unsafe { msg_send![e, CGEvent] };
    let (nx, ny) = if cg.is_null() {
        let p = NSEvent::mouseLocation();
        ((p.x - sx) / sw, (sy + sh - p.y) / sh)
    } else {
        display_point(unsafe { CGEventGetLocation(cg) })
    };
    crate::vinput::abs(nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0));
    if let Some((b, down)) = button {
        crate::vinput::button(b, down);
    }
}

struct HostWin {
    window: Retained<CoWindow>,
    view: Retained<CoView>,
    win: Win,
}

#[derive(Default)]
struct Host {
    wins: HashMap<u64, HostWin>,
    /// the guest's picture now, and its number and size
    image: Option<objc2_core_foundation::CFRetained<CGImage>>,
    frame: Option<u64>,
    guest: (u32, u32),
    /// the list and the foreground window the Mac's windows were last made to fit
    seq: u64,
    fg: u64,
    /// the guest size the agent was last told the work area for
    work_area_for: (u32, u32),
}

thread_local! {
    static HOST: RefCell<Host> = RefCell::new(Host::default());
}

// ---- the Start button in the menu bar ----------------------------------------------------------

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HyperMACCoStart"]
    struct StartTarget;

    impl StartTarget {
        // the Windows key: the Start menu comes up as a window like the others
        #[unsafe(method(start:))]
        fn start(&self, _sender: Option<&AnyObject>) {
            // The menu comes up as one of this program's windows: this program in front first,
            // or the menu opens behind the Mac's active program and without the keyboard.
            #[allow(deprecated)]
            NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
            start_menu();
        }

        // The guest's desktop is not shown, and what lies on it (shortcuts, files) with it:
        // its folder, as a window (user 2026-11-12: "桌面的快捷键怎么启动呢").
        #[unsafe(method(desktop:))]
        fn desktop(&self, _sender: Option<&AnyObject>) {
            #[allow(deprecated)]
            NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
            send("X explorer.exe shell:Desktop");
        }

        #[unsafe(method(files:))]
        fn files(&self, _sender: Option<&AnyObject>) {
            #[allow(deprecated)]
            NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
            send("X explorer.exe");
        }

        #[unsafe(method(off:))]
        fn off(&self, _sender: Option<&AnyObject>) {
            // (not from inside the menu's own action: the button goes away with it)
            std::thread::spawn(stop);
        }
    }
);

/// The Windows key, tapped: the Start menu opens or closes. A moment after the call (a window
/// here that became key just now tells the guest so, and a window taking the guest's
/// foreground closes the menu), and held for a moment (a press and release in one go is not
/// always seen as a key press).
fn start_menu() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(150));
        crate::vinput::key(125, true);
        std::thread::sleep(Duration::from_millis(60));
        crate::vinput::key(125, false);
    });
}

thread_local! {
    /// The menu bar's button while coherence is on, and what it sends its click to.
    static START: RefCell<Option<(Retained<NSStatusItem>, Retained<StartTarget>)>> = const { RefCell::new(None) };
}

/// With no guest desktop there is no taskbar to press Start on: a button in the Mac's menu bar
/// for it (the programs on the guest's desktop and in its Start menu are reached from there).
fn start_button(mtm: MainThreadMarker, show: bool) {
    START.with(|s| {
        let mut s = s.borrow_mut();
        if !show {
            if let Some((item, _)) = s.take() {
                NSStatusBar::systemStatusBar().removeStatusItem(&item);
            }
            return;
        }
        if s.is_some() {
            return;
        }
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0 /* NSVariableStatusItemLength */);
        let target: Retained<StartTarget> = unsafe { msg_send![StartTarget::alloc(mtm), init] };
        if let Some(button) = item.button(mtm) {
            let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str("window.vertical.closed"), Some(&NSString::from_str("Windows Start menu")))
                .or_else(|| NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str("square.grid.2x2.fill"), None));
            match image {
                Some(i) => button.setImage(Some(&i)),
                None => button.setTitle(&NSString::from_str("Start")),
            }
            button.setToolTip(Some(&NSString::from_str("Windows")));
        }
        // a click opens this menu
        let zh = objc2_foundation::NSLocale::preferredLanguages().firstObject().is_some_and(|l| l.to_string().starts_with("zh"));
        let t = |z: &'static str, e: &'static str| NSString::from_str(if zh { z } else { e });
        let menu = objc2_app_kit::NSMenu::new(mtm);
        let empty = NSString::from_str("");
        for (title, action) in [
            (t("开始菜单", "Start Menu"), Some(objc2::sel!(start:))),
            (t("桌面上的项目", "Items on the Desktop"), Some(objc2::sel!(desktop:))),
            (t("文件资源管理器", "File Explorer"), Some(objc2::sel!(files:))),
            (empty.clone(), None),
            (t("关闭融合模式", "End Coherence"), Some(objc2::sel!(off:))),
        ] {
            match action {
                Some(a) => {
                    let i = unsafe { menu.addItemWithTitle_action_keyEquivalent(&title, Some(a), &empty) };
                    unsafe { i.setTarget(Some(&target)) };
                }
                None => menu.addItem(&objc2_app_kit::NSMenuItem::separatorItem(mtm)),
            }
        }
        item.setMenu(Some(&menu));
        *s = Some((item, target));
    });
}

fn make_window(mtm: MainThreadMarker, w: &Win) -> (Retained<CoWindow>, Retained<CoView>) {
    let window: Retained<CoWindow> = unsafe {
        let this = CoWindow::alloc(mtm).set_ivars(WinIvars { hwnd: Cell::new(w.hwnd), popup: Cell::new(w.popup) });
        msg_send![
            super(this),
            initWithContentRect: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, 100.0)),
            styleMask: NSWindowStyleMask::Borderless,
            backing: NSBackingStoreType::Buffered,
            defer: false
        ]
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(false);
    window.setHasShadow(true);
    window.setAcceptsMouseMovedEvents(true);
    unsafe {
        let clear: *mut AnyObject = msg_send![objc2::class!(NSColor), clearColor];
        let _: () = msg_send![&*window, setBackgroundColor: clear];
        if w.popup {
            // NSPopUpMenuWindowLevel: above the window it belongs to, whichever is in front
            let _: () = msg_send![&*window, setLevel: 101isize];
        } else {
            // NSWindowCollectionBehaviorManaged: in Mission Control like any window
            let _: () = msg_send![&*window, setCollectionBehavior: 4usize];
        }
    }
    let view: Retained<CoView> = unsafe { msg_send![super(CoView::alloc(mtm).set_ivars(())), init] };
    view.setWantsLayer(true);
    unsafe {
        let layer: Option<Retained<AnyObject>> = msg_send![&*view, layer];
        if let Some(layer) = layer {
            // the part of the picture, stretched to the view (the same size, or the screens' ratio)
            let _: () = msg_send![&*layer, setContentsGravity: &*NSString::from_str("resize")];
            let _: () = msg_send![&*layer, setMasksToBounds: true];
            if !w.popup {
                let _: () = msg_send![&*layer, setCornerRadius: CORNER];
            }
        }
        // NSTrackingMouseEnteredAndExited | MouseMoved | ActiveAlways | InVisibleRect
        let opts: usize = 0x01 | 0x02 | 0x80 | 0x200;
        let ta: objc2::rc::Allocated<AnyObject> = msg_send![objc2::class!(NSTrackingArea), alloc];
        let ta: Retained<AnyObject> = msg_send![ta, initWithRect: NSRect::ZERO, options: opts, owner: &*view, userInfo: std::ptr::null::<AnyObject>()];
        let _: () = msg_send![&*view, addTrackingArea: &*ta];
    }
    window.setContentView(Some(&view));
    window.makeFirstResponder(Some(&view));
    (window, view)
}

/// Where a guest window is on the Mac's screen.
fn mac_frame(w: &Win, guest: (u32, u32)) -> NSRect {
    let (sx, sy, sw, sh) = *SCREEN.lock().unwrap();
    let (kx, ky) = (sw / guest.0 as f64, sh / guest.1 as f64);
    NSRect::new(NSPoint::new(sx + w.x as f64 * kx, sy + sh - (w.y + w.h) as f64 * ky), NSSize::new(w.w as f64 * kx, w.h as f64 * ky))
}

/// The guest's picture, when there is a new one.
fn new_image(h: &mut Host) -> bool {
    let Some((n, w, ht, pix)) = crate::vgpu::snapshot(h.frame) else { return false };
    if pix.is_empty() {
        return false;
    }
    let data = CFData::from_bytes(&pix);
    let Some(provider) = CGDataProvider::with_cf_data(Some(&data)) else { return false };
    let Some(space) = CGColorSpace::new_device_rgb() else { return false };
    // B,G,R,X in memory = 32-bit little-endian XRGB
    let info = CGBitmapInfo(CGImageAlphaInfo::NoneSkipFirst.0 | CGImageByteOrderInfo::Order32Little.0);
    let img = unsafe {
        CGImage::new(w as usize, ht as usize, 8, 32, w as usize * 4, Some(&space), info, Some(&provider), std::ptr::null(), false, CGColorRenderingIntent::RenderingIntentDefault)
    };
    let Some(img) = img else { return false };
    h.image = Some(img);
    h.frame = Some(n);
    h.guest = (w, ht);
    true
}

/// On the main thread, 60 times a second: the Mac's windows as the guest's are now.
fn tick(mtm: MainThreadMarker) {
    HOST.with(|host| {
        let mut h = host.borrow_mut();
        if !ON.load(Ordering::Relaxed) {
            for (_, hw) in h.wins.drain() {
                hw.window.orderOut(None);
            }
            *h = Host::default();
            return;
        }
        let fresh = new_image(&mut h);
        let guest = h.guest;
        if guest.0 == 0 || h.image.is_none() {
            return;
        }
        // where maximized windows go: what the Mac's menu bar and Dock leave, in guest pixels
        if h.work_area_for != guest && LINK.lock().unwrap().is_some() {
            let (sx, sy, sw, sh) = *SCREEN.lock().unwrap();
            let (vx, vy, vw, vh) = *VISIBLE.lock().unwrap();
            let (kx, ky) = (guest.0 as f64 / sw, guest.1 as f64 / sh);
            let (l, t) = ((vx - sx) * kx, (sy + sh - (vy + vh)) * ky);
            send(&format!("WA {} {} {} {}", l.round() as i32, t.round() as i32, (l + vw * kx).round() as i32, (t + vh * ky).round() as i32));
            h.work_area_for = guest;
        }
        let snap = SNAP.lock().unwrap().clone();
        let listed = snap.seq != h.seq;
        if !fresh && !listed {
            return;
        }
        if listed {
            h.seq = snap.seq;
            // gone in the guest: gone here
            let gone: Vec<u64> = h.wins.keys().copied().filter(|k| !snap.wins.iter().any(|w| w.hwnd == *k)).collect();
            for k in gone {
                if let Some(hw) = h.wins.remove(&k) {
                    hw.window.orderOut(None);
                }
            }
            // bottom first: windows that are new come to the front in the guest's order
            for w in snap.wins.iter().rev() {
                match h.wins.get_mut(&w.hwnd) {
                    Some(hw) => {
                        if hw.win != *w {
                            if (hw.win.x, hw.win.y, hw.win.w, hw.win.h) != (w.x, w.y, w.w, w.h) {
                                hw.window.setFrame_display(mac_frame(w, guest), true);
                            }
                            if hw.win.title != w.title {
                                hw.window.setTitle(&NSString::from_str(&w.title));
                            }
                            hw.win = w.clone();
                        }
                    }
                    None => {
                        let (window, view) = make_window(mtm, w);
                        window.setTitle(&NSString::from_str(&w.title));
                        window.setFrame_display(mac_frame(w, guest), true);
                        window.orderFront(None);
                        h.wins.insert(w.hwnd, HostWin { window, view, win: w.clone() });
                    }
                }
            }
            // The guest's foreground window changed (a click in one of these windows did it, or
            // a program in the guest): while this is the Mac's active program, that window is
            // the key one here too. Not otherwise: a guest program does not take the Mac's
            // keyboard from whatever the user is typing in.
            if snap.fg != h.fg {
                h.fg = snap.fg;
                if NSApplication::sharedApplication(mtm).isActive() {
                    if let Some(hw) = h.wins.get(&snap.fg).filter(|hw| !hw.win.popup && !hw.window.isKeyWindow()) {
                        hw.window.makeKeyAndOrderFront(None);
                    }
                }
            }
        }
        // every window shows its part of the picture
        let Some(img) = h.image.as_ref() else { return };
        let p: *const AnyObject = (&**img as *const CGImage).cast();
        for hw in h.wins.values() {
            let w = &hw.win;
            // (a layer's contentsRect counts from the bottom here, the guest from the top)
            let part = NSRect::new(
                NSPoint::new(w.x as f64 / guest.0 as f64, 1.0 - (w.y + w.h) as f64 / guest.1 as f64),
                NSSize::new(w.w as f64 / guest.0 as f64, w.h as f64 / guest.1 as f64),
            );
            unsafe {
                let layer: Option<Retained<AnyObject>> = msg_send![&*hw.view, layer];
                if let Some(layer) = layer {
                    let _: () = msg_send![&*layer, setContents: p];
                    let _: () = msg_send![&*layer, setContentsRect: part];
                }
            }
        }
    });
}

fn pump() {
    loop {
        std::thread::sleep(Duration::from_millis(16));
        if QUEUED.swap(true, Ordering::Relaxed) {
            continue;
        }
        DispatchQueue::main().exec_async(|| {
            if let Some(mtm) = MainThreadMarker::new() {
                tick(mtm);
            }
            QUEUED.store(false, Ordering::Relaxed);
        });
        // off, and the windows are gone by the tick above: nothing to do until it is on again
        while !ON.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// On: `addr` is the guest's address, `token` what its agent wants to hear.
fn start(ways: Vec<String>, token: String) {
    if ON.swap(true, Ordering::Relaxed) {
        return;
    }
    let run = RUN.fetch_add(1, Ordering::Relaxed) + 1;
    *STATE.lock().unwrap() = "connecting".into();
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else { return };
        // the screen with the menu bar
        let Some(screen) = NSScreen::screens(mtm).firstObject() else { return };
        let (f, v) = (screen.frame(), screen.visibleFrame());
        *SCREEN.lock().unwrap() = (f.origin.x, f.origin.y, f.size.width, f.size.height);
        *VISIBLE.lock().unwrap() = (v.origin.x, v.origin.y, v.size.width, v.size.height);
        // the guest's screen as large as the Mac's, where it can follow (the 3D adapter)
        if crate::bhyve::active() && crate::bhyve::resizable() {
            *BEFORE.lock().unwrap() = crate::vgpu::snapshot(Some(u64::MAX)).map(|(_, w, h, _)| (w, h));
            if let Err(e) = crate::bhyve::request_size(f.size.width.round() as u32, f.size.height.round() as u32) {
                eprintln!("coherence: {e:#}");
            }
        }
        // these windows are a program's: in the Dock and in Cmd-Tab
        crate::console::regular(mtm);
        start_button(mtm, true);
    });
    std::thread::spawn(move || link(run, ways, token));
    PUMP.call_once(|| {
        std::thread::spawn(pump);
    });
}

/// Is coherence on?
#[allow(dead_code)]
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// The process is about to end with coherence on: what of `stop` matters to the guest (its
/// screen back to the size it had; the agent undoes its own changes when the connection ends).
#[allow(dead_code)]
pub fn leave() {
    if !ON.swap(false, Ordering::Relaxed) {
        return;
    }
    if let Some(s) = LINK.lock().unwrap().take() {
        s.shutdown();
    }
    if let Some((w, h)) = BEFORE.lock().unwrap().take() {
        let _ = crate::bhyve::request_size(w, h);
    }
}

fn stop() {
    if !ON.swap(false, Ordering::Relaxed) {
        return;
    }
    // the agent puts the taskbar back when the connection ends
    if let Some(s) = LINK.lock().unwrap().take() {
        s.shutdown();
    }
    *SNAP.lock().unwrap() = Snap::default();
    *STATE.lock().unwrap() = "off".into();
    if let Some((w, h)) = BEFORE.lock().unwrap().take() {
        let _ = crate::bhyve::request_size(w, h);
    }
    DispatchQueue::main().exec_async(|| {
        let Some(mtm) = MainThreadMarker::new() else { return };
        tick(mtm);
        start_button(mtm, false);
        if !crate::console::shown() {
            NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        }
    });
}

/// ctl "coherence on|off|run COMMAND|status". `port`: the socket of the machine's channel to
/// its agent, when the machine has one; `addr`: the guest's address when it has one; `token`:
/// vm.json's "agent" (`hypermac agent NAME` makes it and the agent's CD).
pub fn ctl(line: &str, port: Option<&std::path::Path>, addr: Option<String>, token: Option<&str>) -> serde_json::Value {
    let rest = line.strip_prefix("coherence").unwrap_or("").trim();
    let (cmd, arg) = rest.split_once(' ').unwrap_or((rest, ""));
    match cmd {
        "on" => {
            let Some(token) = token else {
                return serde_json::json!({"error": "this machine has no agent yet: hypermac agent NAME plugs in a CD with it; run its install.cmd in Windows as administrator"});
            };
            let mut ways: Vec<String> = port.filter(|p| p.exists()).map(|p| format!("unix:{}", p.display())).into_iter().collect();
            ways.extend(addr.map(|a| format!("{a}:{PORT}")));
            if ways.is_empty() {
                return serde_json::json!({"error": "no way to the agent yet: the machine has no channel for it (it gets one at its next start) and the guest no network address"});
            }
            start(ways, token.to_string());
            serde_json::json!({"ok": true})
        }
        "off" => {
            stop();
            serde_json::json!({"ok": true})
        }
        "run" if !arg.is_empty() && ON.load(Ordering::Relaxed) => {
            send(&format!("X {arg}"));
            serde_json::json!({"ok": true})
        }
        // "click X Y": a left click at that display point, as one in a window here would send
        // it (for scripts and tests: no window has to be in front, the Mac's pointer stays)
        "click" if ON.load(Ordering::Relaxed) => {
            let v: Vec<f64> = arg.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            let [x, y] = v[..] else { return serde_json::json!({"error": "coherence click X Y"}) };
            let (nx, ny) = display_point(NSPoint::new(x, y));
            crate::vinput::abs(nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0));
            crate::vinput::button(0, true);
            std::thread::sleep(Duration::from_millis(40));
            crate::vinput::button(0, false);
            serde_json::json!({"ok": true})
        }
        // the Start menu: the Windows key (it comes up as a window like the others)
        "start" if ON.load(Ordering::Relaxed) => {
            start_menu();
            serde_json::json!({"ok": true})
        }
        _ => {
            let s = SNAP.lock().unwrap();
            serde_json::json!({
                "on": ON.load(Ordering::Relaxed),
                "state": *STATE.lock().unwrap(),
                "foreground": s.fg,
                "windows": s.wins.iter().map(|w| serde_json::json!({"hwnd": w.hwnd, "x": w.x, "y": w.y, "w": w.w, "h": w.h, "popup": w.popup, "title": w.title})).collect::<Vec<_>>(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agents_list_is_read() {
        let mut b = None;
        let lines = ["S 1920 1080 65800", "W 65800 100 50 800 600 2 4321 Notepad\tmemo.txt - Notepad", "W 131200 300 200 150 220 1 4321 #32768\t", "W bad", "E"];
        let snap = lines.iter().find_map(|l| parse(l, &mut b)).unwrap();
        assert_eq!(snap.fg, 65800);
        assert_eq!(snap.wins.len(), 2);
        assert_eq!(snap.wins[0], Win { hwnd: 65800, x: 100, y: 50, w: 800, h: 600, popup: false, title: "memo.txt - Notepad".into() });
        assert!(snap.wins[1].popup && snap.wins[1].title.is_empty());
        // a list is whole or not there
        assert!(parse("W 1 0 0 1 1 0 1 X\t", &mut b).is_none() && b.is_none());
    }
}
