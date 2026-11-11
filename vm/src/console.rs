//! The native console window for a VM on our own virtio-gpu / virtio-input (own_devices):
//! VZ's VZVirtualMachineView only shows VZ's own graphics and feeds VZ's USB HID.
//!
//! A plain NSView: the virtio-gpu scanout (vgpu::snapshot) as its layer's contents (aspect fit,
//! a frame pump thread at ~30 fps), and its key and mouse events -> vinput (Linux key codes,
//! absolute pointer). Command is the guest's Super/Windows key; Cmd-Tab and the like stay
//! with the host (no system-key capture: that needs the Accessibility grant).

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEvent, NSResponder, NSScreen, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_core_foundation::CFData;
use objc2_core_graphics::{CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

/// the window wants frames (it is open)
static SHOWN: AtomicBool = AtomicBool::new(false);
/// a frame is queued on the main thread and not drawn yet: skip, don't pile up
static QUEUED: AtomicBool = AtomicBool::new(false);
static PUMP: std::sync::Once = std::sync::Once::new();
static KEYS: Mutex<Option<HashSet<u16>>> = Mutex::new(None);

thread_local! {
    static CONSOLE: RefCell<Option<(Retained<NSWindow>, Retained<ConsoleView>)>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super(NSView, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HyperMACOwnConsoleView"]
    struct ConsoleView;

    impl ConsoleView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, e: &NSEvent) {
            key(e, true);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, e: &NSEvent) {
            key(e, false);
        }

        // Command (and Control) combinations come here first: they go to the guest, not to menus
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, e: &NSEvent) -> bool {
            key(e, true);
            true
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, e: &NSEvent) {
            modifier(e);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, e: &NSEvent) {
            self.pointer(e, None);
        }
        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _e: &NSEvent) {}
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, e: &NSEvent) {
            self.pointer(e, None);
        }
        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, e: &NSEvent) {
            self.pointer(e, None);
        }
        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, e: &NSEvent) {
            self.pointer(e, None);
        }
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            self.pointer(e, Some((0, true)));
        }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, e: &NSEvent) {
            self.pointer(e, Some((0, false)));
        }
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, e: &NSEvent) {
            self.pointer(e, Some((1, true)));
        }
        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, e: &NSEvent) {
            self.pointer(e, Some((1, false)));
        }
        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, e: &NSEvent) {
            self.pointer(e, Some((2, true)));
        }
        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, e: &NSEvent) {
            self.pointer(e, Some((2, false)));
        }
    }
);

impl ConsoleView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }

    /// The window point -> 0..1 of the guest picture (aspect fit inside the view), and a button.
    fn pointer(&self, e: &NSEvent, button: Option<(u16, bool)>) {
        let p = self.convertPoint_fromView(e.locationInWindow(), None);
        let b = self.bounds();
        let (gw, gh) = crate::vgpu::size().map(|(w, h)| (w as f64, h as f64)).unwrap_or((b.size.width, b.size.height));
        let k = (b.size.width / gw).min(b.size.height / gh);
        let (iw, ih) = (gw * k, gh * k);
        let (ox, oy) = ((b.size.width - iw) / 2.0, (b.size.height - ih) / 2.0);
        let x = (p.x - ox) / iw;
        let y = 1.0 - (p.y - oy) / ih; // the view's origin is bottom-left
        crate::vinput::abs(x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
        if let Some((btn, down)) = button {
            crate::vinput::button(btn, down);
        }
    }
}

fn key(e: &NSEvent, down: bool) {
    let Some(code) = mac_to_linux(e.keyCode()) else { return };
    let mut keys = KEYS.lock().unwrap();
    let keys = keys.get_or_insert_with(HashSet::new);
    // the guest repeats by itself; a key goes down once and up once
    if down == keys.contains(&code) {
        return;
    }
    if down {
        keys.insert(code);
    } else {
        keys.remove(&code);
    }
    crate::vinput::key(code, down);
}

/// flagsChanged: which modifier, and is it down now (device-dependent bits tell left from right).
fn modifier(e: &NSEvent) {
    let kc = e.keyCode();
    let flags = e.modifierFlags().0;
    let bit = match kc {
        0x38 => 0x02,    // left shift
        0x3c => 0x04,    // right shift
        0x3b => 0x01,    // left control
        0x3e => 0x2000,  // right control
        0x3a => 0x20,    // left option
        0x3d => 0x40,    // right option
        0x37 => 0x08,    // left command
        0x36 => 0x10,    // right command
        0x39 => {
            // Caps Lock reports its toggled state: one press per change
            if let Some(c) = mac_to_linux(kc) {
                crate::vinput::key(c, true);
                crate::vinput::key(c, false);
            }
            return;
        }
        _ => return,
    };
    key(e, flags & bit != 0);
}

/// macOS virtual key code (kVK_*) -> Linux input key code.
fn mac_to_linux(kc: u16) -> Option<u16> {
    Some(match kc {
        0x00 => 30, 0x01 => 31, 0x02 => 32, 0x03 => 33, 0x04 => 35, 0x05 => 34, 0x06 => 44, 0x07 => 45,
        0x08 => 46, 0x09 => 47, 0x0a => 86, 0x0b => 48, 0x0c => 16, 0x0d => 17, 0x0e => 18, 0x0f => 19,
        0x10 => 21, 0x11 => 20, 0x12 => 2, 0x13 => 3, 0x14 => 4, 0x15 => 5, 0x16 => 7, 0x17 => 6,
        0x18 => 13, 0x19 => 10, 0x1a => 8, 0x1b => 12, 0x1c => 9, 0x1d => 11, 0x1e => 27, 0x1f => 24,
        0x20 => 22, 0x21 => 26, 0x22 => 23, 0x23 => 25, 0x24 => 28, 0x25 => 38, 0x26 => 36, 0x27 => 40,
        0x28 => 37, 0x29 => 39, 0x2a => 43, 0x2b => 51, 0x2c => 53, 0x2d => 49, 0x2e => 50, 0x2f => 52,
        0x30 => 15, 0x31 => 57, 0x32 => 41, 0x33 => 14, 0x35 => 1,
        0x36 => 126, 0x37 => 125, 0x38 => 42, 0x39 => 58, 0x3a => 56, 0x3b => 29, 0x3c => 54, 0x3d => 100,
        0x3e => 97,
        0x41 => 83, 0x43 => 55, 0x45 => 78, 0x47 => 69, 0x4b => 98, 0x4c => 96, 0x4e => 74, 0x51 => 117,
        0x52 => 82, 0x53 => 79, 0x54 => 80, 0x55 => 81, 0x56 => 75, 0x57 => 76, 0x58 => 77, 0x59 => 71,
        0x5b => 72, 0x5c => 73,
        0x5d => 124, // JIS ¥
        0x5e => 89,  // JIS _ (ろ)
        0x5f => 121, // JIS keypad ,
        0x66 => 123, // JIS 英数 -> LANG2
        0x68 => 122, // JIS かな -> LANG1
        0x7a => 59, 0x78 => 60, 0x63 => 61, 0x76 => 62, 0x60 => 63, 0x61 => 64, 0x62 => 65, 0x64 => 66,
        0x65 => 67, 0x6d => 68, 0x67 => 87, 0x6f => 88,
        0x72 => 110, // Help -> Insert
        0x73 => 102, 0x74 => 104, 0x75 => 111, 0x77 => 107, 0x79 => 109,
        0x7b => 105, 0x7c => 106, 0x7d => 108, 0x7e => 103,
        _ => return None,
    })
}

/// Open (or bring forward) the console window. On the main thread.
pub fn open(title: &str) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    set_app_icon(&app);
    let existing = CONSOLE.with(|c| c.borrow().as_ref().map(|(w, _)| w.clone()));
    let window = match existing {
        Some(w) => w,
        None => {
            let (gw, gh) = crate::vgpu::size().map(|(w, h)| (w as f64, h as f64)).unwrap_or((1920.0, 1080.0));
            let (sw, sh) = NSScreen::mainScreen(mtm).map(|s| s.visibleFrame().size).map(|s| (s.width, s.height)).unwrap_or((1440.0, 900.0));
            let k = (sw * 0.9 / gw).min((sh * 0.9 - 30.0) / gh).min(1.0);
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new((gw * k).round(), (gh * k).round())),
                    NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable | NSWindowStyleMask::Resizable,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            unsafe { window.setReleasedWhenClosed(false) };
            window.setTitle(&NSString::from_str(title));
            window.setAcceptsMouseMovedEvents(true);
            let view = ConsoleView::new(mtm);
            view.setWantsLayer(true);
            unsafe {
                let layer: Option<Retained<AnyObject>> = msg_send![&*view, layer];
                if let Some(layer) = layer {
                    let _: () = msg_send![&*layer, setContentsGravity: &*NSString::from_str("resizeAspect")];
                    let black: *mut AnyObject = msg_send![objc2::class!(NSColor), blackColor];
                    let cg: *const std::ffi::c_void = msg_send![black, CGColor];
                    let _: () = msg_send![&*layer, setBackgroundColor: cg];
                }
            }
            // mouse moves without a button: a tracking area hands them to the view directly
            // (mouseMoved: otherwise only reaches the first responder, and not reliably)
            unsafe {
                // NSTrackingMouseEnteredAndExited | MouseMoved | ActiveAlways | InVisibleRect
                let opts: usize = 0x01 | 0x02 | 0x80 | 0x200;
                let ta: objc2::rc::Allocated<AnyObject> = msg_send![objc2::class!(NSTrackingArea), alloc];
                let ta: Retained<AnyObject> = msg_send![ta, initWithRect: NSRect::ZERO, options: opts, owner: &*view, userInfo: std::ptr::null::<AnyObject>()];
                let _: () = msg_send![&*view, addTrackingArea: &*ta];
            }
            window.setContentView(Some(&view));
            window.makeFirstResponder(Some(&view));
            window.center();
            CONSOLE.with(|c| *c.borrow_mut() = Some((window.clone(), view)));
            window
        }
    };
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    SHOWN.store(true, Ordering::Relaxed);
    PUMP.call_once(|| {
        std::thread::spawn(pump);
    });
}

/// ~30 fps: a new scanout frame -> the view's layer (on the main thread).
fn pump() {
    let mut last = None;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(33));
        if !SHOWN.load(Ordering::Relaxed) || QUEUED.load(Ordering::Relaxed) {
            continue;
        }
        let Some((n, w, h, pix)) = crate::vgpu::snapshot(last) else { continue };
        if pix.is_empty() {
            continue;
        }
        last = Some(n);
        QUEUED.store(true, Ordering::Relaxed);
        DispatchQueue::main().exec_async(move || {
            show(w, h, &pix);
            QUEUED.store(false, Ordering::Relaxed);
        });
    }
}

fn show(w: u32, h: u32, pix: &[u8]) {
    CONSOLE.with(|c| {
        let c = c.borrow();
        let Some((window, view)) = c.as_ref() else { return };
        if !window.isVisible() {
            SHOWN.store(false, Ordering::Relaxed);
            return;
        }
        let data = CFData::from_bytes(pix);
        let Some(provider) = CGDataProvider::with_cf_data(Some(&data)) else { return };
        let Some(space) = CGColorSpace::new_device_rgb() else { return };
        // B,G,R,X in memory = 32-bit little-endian XRGB
        let info = CGBitmapInfo(CGImageAlphaInfo::NoneSkipFirst.0 | CGImageByteOrderInfo::Order32Little.0);
        let img = unsafe {
            CGImage::new(w as usize, h as usize, 8, 32, w as usize * 4, Some(&space), info, Some(&provider), std::ptr::null(), false, CGColorRenderingIntent::RenderingIntentDefault)
        };
        let Some(img) = img else { return };
        unsafe {
            let layer: Option<Retained<AnyObject>> = msg_send![&**view, layer];
            if let Some(layer) = layer {
                let p: *const AnyObject = (&*img as *const CGImage).cast();
                let _: () = msg_send![&*layer, setContents: p];
            }
        }
    });
}

/// The Dock icon of this process: a VM started from the command line has no app bundle, so
/// its console windows would show a generic one.
pub fn set_app_icon(app: &NSApplication) {
    static PNG: &[u8] = include_bytes!("../../assets/hypermac-1024.png");
    let data = objc2_foundation::NSData::with_bytes(PNG);
    use objc2::AllocAnyThread;
    let img = objc2_app_kit::NSImage::initWithData(objc2_app_kit::NSImage::alloc(), &data);
    if let Some(img) = img {
        unsafe { app.setApplicationIconImage(Some(&img)) };
    }
}
