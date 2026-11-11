//! Keyboard and absolute pointer as our own virtio-input devices (device 18), since
//! VZ's USB HID only takes events from a VZVirtualMachineView with VZ's own graphics.
//!
//! The catch: virtio-input's config space is a select/subsel query (the driver writes what
//! it wants, then reads size + data), and VZCustomVirtioDevice has no config-write callback:
//! the guest reads the SAME bytes for every query. So each device's one bitmap is laid out
//! to be right for everything the Linux driver (drivers/virtio/virtio_input.c) asks:
//!   - EV_KEY / EV_REL / EV_ABS / EV_MSC / EV_SW / EV_LED / EV_SND bits = the same bitmap
//!   - ABS_INFO for every abs axis = the first 20 bytes read as min/max/fuzz/flat/res
//!   - ID_DEVIDS = the first 8 bytes; ID_NAME = the bitmap as a string
//! keyboard: 16 bytes, keys 1..127 (all of a PC keyboard); its stray abs axes get
//!   min -2 / max -1 and ABS_MT_SLOT max -1 -> 0 slots (no MT).
//! pointer: byte 0 = 0x03 (ABS_X, ABS_Y), bytes 4..7 = 32767 (the axes' max; min = 3),
//!   byte 34 = BTN_LEFT/RIGHT/MIDDLE. Strays: REL_X/Y, a lid switch (reads "open"), a few keys.
//! Ported from macRDP's spike (spikes/s1-vz/src/input.rs, S1c).
//! Events go on eventq (0) as 8-byte virtio_input_event {le16 type, le16 code, le32 value}
//! into buffers the guest posted; statusq (1) (LEDs) is drained and ignored.

use std::collections::VecDeque;
use std::sync::Mutex;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, NSObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSData, NSError, NSObjectProtocol};

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_ABS: u16 = 3;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_TOUCH: u16 = 0x14a;
pub const BTN_STYLUS: u16 = 0x14b;
/// 32767. (0x4000 was tried to cut the slider axes these bits also make in vioinput's relative
/// mouse; the sliders turned out harmless, and Windows then mapped pen clicks at twice the X:
/// the middle landed on the right edge.)
pub const ABS_MAX: i32 = 32767;
pub const ABS_MIN: i32 = 3;

struct Dev {
    device: Option<Retained<AnyObject>>,
    queue: dispatch2::DispatchRetained<DispatchQueue>,
    pending: VecDeque<[u8; 8]>,
    sent: u64,
}
unsafe impl Send for Dev {}

/// 0 = keyboard, 1 = pointer
static DEVS: Mutex<Vec<Dev>> = Mutex::new(Vec::new());
static KEEP: Mutex<Vec<SendObj>> = Mutex::new(Vec::new());
struct SendObj(#[allow(dead_code)] Retained<AnyObject>);
unsafe impl Send for SendObj {}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "HyperMacInputConfigDelegate"]
    #[ivars = usize]
    struct ConfigDelegate;

    unsafe impl NSObjectProtocol for ConfigDelegate {}

    impl ConfigDelegate {
        #[unsafe(method(customVirtioConfiguration:didCreateDevice:))]
        fn did_create(&self, _cfg: &AnyObject, device: &AnyObject) {
            let i = *self.ivars();
            eprintln!("input{i}: device created");
            let d = DeviceDelegate::alloc().set_ivars(i);
            let d: Retained<DeviceDelegate> = unsafe { msg_send![super(d), init] };
            let _: () = unsafe { msg_send![device, setDelegate: &*d] };
            KEEP.lock().unwrap().push(SendObj(unsafe { Retained::cast_unchecked(d) }));
            DEVS.lock().unwrap()[i].device = unsafe { Retained::retain(device as *const _ as *mut AnyObject) };
        }
    }
);

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "HyperMacInputDeviceDelegate"]
    #[ivars = usize]
    struct DeviceDelegate;

    unsafe impl NSObjectProtocol for DeviceDelegate {}

    impl DeviceDelegate {
        #[unsafe(method(customVirtioDeviceDidAcceptDriverOk:))]
        fn driver_ok(&self, _device: &AnyObject) {
            eprintln!("input{}: DRIVER_OK", self.ivars());
        }

        #[unsafe(method(customVirtioDeviceWillReset:))]
        fn will_reset(&self, _device: &AnyObject) {
            eprintln!("input{}: reset", self.ivars());
            DEVS.lock().unwrap()[*self.ivars()].pending.clear();
        }

        #[unsafe(method(customVirtioDevice:didReceiveNotificationForQueue:))]
        fn notified(&self, _device: &AnyObject, queue: &AnyObject) {
            let qi: u16 = unsafe { msg_send![queue, queueIndex] };
            if qi == 1 {
                // statusq: LED state from the guest; not used
                loop {
                    let el: Option<Retained<AnyObject>> = unsafe { msg_send![queue, nextElement] };
                    let Some(el) = el else { break };
                    let _: () = unsafe { msg_send![&*el, returnToQueue] };
                }
            } else {
                flush(*self.ivars());
            }
        }
    }
);

/// Put pending events into the buffers the guest has posted (runs on the device's queue).
fn flush(i: usize) {
    let mut devs = DEVS.lock().unwrap();
    let d = &mut devs[i];
    let Some(device) = d.device.clone() else { return };
    let q: Option<Retained<AnyObject>> = unsafe { msg_send![&*device, queueAtIndex: 0u16] };
    let Some(q) = q else { return };
    while let Some(ev) = d.pending.front().copied() {
        let el: Option<Retained<AnyObject>> = unsafe { msg_send![&*q, nextElement] };
        let Some(el) = el else { break }; // no buffer: the next notification comes back here
        let data = NSData::with_bytes(&ev);
        let ok: bool = unsafe { msg_send![&*el, writeData: &*data, error: std::ptr::null_mut::<*mut NSError>()] };
        let _: () = unsafe { msg_send![&*el, returnToQueue] };
        if ok {
            d.pending.pop_front();
            d.sent += 1;
        } else {
            eprintln!("input{i}: writeData failed");
            d.pending.pop_front();
        }
    }
}

fn event(ty: u16, code: u16, value: i32) -> [u8; 8] {
    let mut e = [0u8; 8];
    e[0..2].copy_from_slice(&ty.to_le_bytes());
    e[2..4].copy_from_slice(&code.to_le_bytes());
    e[4..8].copy_from_slice(&value.to_le_bytes());
    e
}

/// Queue events (each followed by nothing; pass the SYN yourself) for device i.
pub fn send(i: usize, evs: &[(u16, u16, i32)]) -> u64 {
    let queue = {
        let mut devs = DEVS.lock().unwrap();
        let Some(d) = devs.get_mut(i) else { return 0 };
        for &(t, c, v) in evs {
            d.pending.push_back(event(t, c, v));
        }
        d.queue.clone()
    };
    queue.exec_async(move || flush(i));
    DEVS.lock().unwrap().get(i).map(|d| d.sent).unwrap_or(0)
}

/// which device takes key events / pointer events (Windows: one combined device, see configurations)
static KBD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static PTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

pub fn key(code: u16, down: bool) -> u64 {
    // the bhyve engine has its own devices (bhyve.rs)
    if crate::bhyve::active() {
        return crate::bhyve::key(code, down);
    }
    send(KBD.load(std::sync::atomic::Ordering::Relaxed), &[(EV_KEY, code, down as i32), (EV_SYN, 0, 0)])
}

/// Windows: our absolute axes become vioinput's tablet (a pen), which only moves the pointer
/// with the tip down (vioinput sets "in range" together with "tip", on BTN_LEFT only): hovering
/// does nothing. The same device is also a relative mouse (REL_X/REL_Y share the bits), so
/// moves go as relative steps in guest pixels and a click (the pen tip) lands exactly and puts
/// the pointer back where we think it is. Pointer acceleration in the guest skews the moves
/// between clicks ("Enhance pointer precision" should be off).
static WINDOWS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static REL_POS: Mutex<Option<(i32, i32)>> = Mutex::new(None);
pub const EV_REL: u16 = 2;

/// Relative steps to guest pixel (px, py) from where the pointer should be; vioinput's mouse
/// report has one signed byte per axis.
fn rel_to(px: i32, py: i32) {
    let mut pos = REL_POS.lock().unwrap();
    // closed loop: where the guest's pointer really is (hardware cursor reports), so the guest's
    // pointer acceleration can't make it drift; else where our own steps should have put it
    let (cx, cy) = match crate::vgpu::cursor_position().or(*pos) {
        Some(p) => p,
        None => {
            // unknown: push it into the top-left corner first
            let mut evs = Vec::new();
            for _ in 0..80 {
                evs.extend_from_slice(&[(EV_REL, 0, -127), (EV_REL, 1, -127), (EV_SYN, 0, 0)]);
            }
            send(PTR.load(std::sync::atomic::Ordering::Relaxed), &evs);
            (0, 0)
        }
    };
    let (mut dx, mut dy) = (px - cx, py - cy);
    let mut evs = Vec::new();
    while dx != 0 || dy != 0 {
        let sx = dx.clamp(-127, 127);
        let sy = dy.clamp(-127, 127);
        evs.extend_from_slice(&[(EV_REL, 0, sx), (EV_REL, 1, sy), (EV_SYN, 0, 0)]);
        dx -= sx;
        dy -= sy;
    }
    if !evs.is_empty() {
        send(PTR.load(std::sync::atomic::Ordering::Relaxed), &evs);
    }
    *pos = Some((px, py));
}

/// Windows maps vioinput's pen X at exactly twice what we send (measured 2026-11-10: 0.1 -> 0.2,
/// 0.25 -> 0.5, 0.4 -> 0.8 of the width; Y is right; independent of the aspect ratio). Cause not
/// found yet; the pen's X goes out halved.
fn pen_x(x: f64) -> f64 {
    if WINDOWS.load(std::sync::atomic::Ordering::Relaxed) { x / 2.0 } else { x }
}

/// x, y: 0..=1 of the screen
pub fn abs(x: f64, y: f64) -> u64 {
    // the bhyve engine has its own devices (bhyve.rs)
    if crate::bhyve::active() {
        return crate::bhyve::abs(x, y);
    }
    if WINDOWS.load(std::sync::atomic::Ordering::Relaxed) {
        let (w, h) = crate::vgpu::size().unwrap_or((1920, 1080));
        let px = (x.clamp(0.0, 1.0) * (w - 1) as f64).round() as i32;
        let py = (y.clamp(0.0, 1.0) * (h - 1) as f64).round() as i32;
        LAST_ABS.lock().unwrap().replace((x, y));
        if BUTTONS.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            // hovering: the pen can't, the relative mouse drifts with the guest's pointer
            // acceleration (user 2026-11-10: "not following is fine") -> off unless HYPERMAC_HOVER=1;
            // clicks and drags are exact anyway
            if std::env::var_os("HYPERMAC_HOVER").is_some() {
                rel_to(px, py);
            }
            return 0;
        }
        // a button is down: the pen tip is down, absolute positions work (exact drags)
        *REL_POS.lock().unwrap() = Some((px, py));
    }
    let s = |f: f64| ABS_MIN + ((f.clamp(0.0, 1.0)) * (ABS_MAX - ABS_MIN) as f64).round() as i32;
    send(PTR.load(std::sync::atomic::Ordering::Relaxed), &[(EV_ABS, 0, s(pen_x(x))), (EV_ABS, 1, s(y)), (EV_SYN, 0, 0)])
}

/// button 0 left, 1 right, 2 middle
static LAST_ABS: Mutex<Option<(f64, f64)>> = Mutex::new(None);
/// buttons held down (bit per button)
static BUTTONS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn button(b: u16, down: bool) -> u64 {
    // the bhyve engine has its own devices (bhyve.rs)
    if crate::bhyve::active() {
        return crate::bhyve::button(b, down);
    }
    let bit = 1u8 << b.min(7);
    // Windows: left and right clicks go to the pen only. BTN_LEFT would reach vioinput's
    // relative mouse too, which clicks wherever the relative moves left the pointer (off by the
    // guest's acceleration: 100+ px seen) and gets there first. BTN_TOUCH (the pen tip, at the
    // exact absolute position) is outside the mouse's button range; BTN_STYLUS + tip is the
    // pen's right click. The middle button stays on the mouse.
    if WINDOWS.load(std::sync::atomic::Ordering::Relaxed) && b != 2 {
        use std::sync::atomic::Ordering::Relaxed;
        if down {
            BUTTONS.fetch_or(bit, Relaxed);
        } else {
            BUTTONS.fetch_and(!bit, Relaxed);
        }
        let mut evs = Vec::new();
        if let Some((x, y)) = *LAST_ABS.lock().unwrap() {
            let s = |f: f64| ABS_MIN + ((f.clamp(0.0, 1.0)) * (ABS_MAX - ABS_MIN) as f64).round() as i32;
            evs.extend_from_slice(&[(EV_ABS, 0, s(pen_x(x))), (EV_ABS, 1, s(y))]);
            let (w, h) = crate::vgpu::size().unwrap_or((1920, 1080));
            *REL_POS.lock().unwrap() = Some(((x * (w - 1) as f64).round() as i32, (y * (h - 1) as f64).round() as i32));
        }
        let v = down as i32;
        if b == 1 {
            if down {
                evs.extend_from_slice(&[(EV_KEY, BTN_STYLUS, 1), (EV_KEY, BTN_TOUCH, 1)]);
            } else {
                evs.extend_from_slice(&[(EV_KEY, BTN_TOUCH, 0), (EV_KEY, BTN_STYLUS, 0)]);
            }
        } else {
            evs.push((EV_KEY, BTN_TOUCH, v));
        }
        evs.push((EV_SYN, 0, 0));
        return send(PTR.load(Relaxed), &evs);
    }
    if down {
        BUTTONS.fetch_or(bit, std::sync::atomic::Ordering::Relaxed);
    } else {
        BUTTONS.fetch_and(!bit, std::sync::atomic::Ordering::Relaxed);
    }
    let code = match b {
        1 => BTN_LEFT + 1,
        2 => BTN_LEFT + 2,
        _ => BTN_LEFT,
    };
    send(PTR.load(std::sync::atomic::Ordering::Relaxed), &[(EV_KEY, code, down as i32), (EV_SYN, 0, 0)])
}

fn config_bytes(bitmap: &[u8]) -> Vec<u8> {
    // struct virtio_input_config: select, subsel, size, reserved[5], union u[128]
    let mut c = vec![0u8; 136];
    c[2] = bitmap.len() as u8;
    c[8..8 + bitmap.len()].copy_from_slice(bitmap);
    c
}

fn one(i: usize, bitmap: &[u8]) -> anyhow::Result<Retained<AnyObject>> {
    let cls = |n: &std::ffi::CStr| AnyClass::get(n).ok_or_else(|| anyhow::anyhow!("{n:?}: needs macOS 27"));
    let queue = DispatchQueue::new(&format!("hypermac.input{i}"), None);
    DEVS.lock().unwrap().push(Dev { device: None, queue: queue.clone(), pending: VecDeque::new(), sent: 0 });
    unsafe {
        let c: Retained<AnyObject> = msg_send![cls(c"VZCustomVirtioDeviceConfiguration")?, new];
        let _: () = msg_send![&*c, setDeviceID: 18u16];
        let _: () = msg_send![&*c, setPCIClassID: 0x09u8];
        let _: () = msg_send![&*c, setPCISubclassID: 0x80u8];
        let _: () = msg_send![&*c, setVirtioQueueCount: 2u16];
        let sc: objc2::rc::Allocated<AnyObject> = msg_send![cls(c"VZVirtioDeviceSpecificConfiguration")?, alloc];
        let sc: Retained<AnyObject> = msg_send![sc, initWithConfigurationData: &*NSData::with_bytes(&config_bytes(bitmap))];
        let _: () = msg_send![&*c, setDeviceSpecificConfiguration: &*sc];
        let del = ConfigDelegate::alloc().set_ivars(i);
        let del: Retained<ConfigDelegate> = msg_send![super(del), init];
        let p: objc2::rc::Allocated<AnyObject> = msg_send![cls(c"VZCustomVirtioDeviceDelegateProvider")?, alloc];
        let p: Retained<AnyObject> = msg_send![p, initWithDeviceQueue: &*queue, delegate: &*del];
        let _: () = msg_send![&*c, setProvider: &*p];
        KEEP.lock().unwrap().push(SendObj(Retained::cast_unchecked(del)));
        Ok(c)
    }
}

/// The device configurations. Linux: [keyboard, pointer]. Windows: [pointer] only, which is
/// also the keyboard.
///
/// Why: Windows' vioinput (virtio-win, vioinput/sys/Hid.c) reads the one bitmap as EV_KEY,
/// EV_REL, EV_ABS and EV_MSC and probes mouse -> joystick -> tablet -> keyboard; the tablet
/// probe fails the whole device unless the absolute axes left over are exactly ABS_X and ABS_Y
/// (`uNumOfAbsAxes != 2`). The keyboard bitmap (all ones) leaves dozens -> the device is
/// refused and reset over and over. The pointer bitmap passes: REL_X/REL_Y (same bits) make a
/// relative mouse that eats the REL bits, the tablet gets ABS_X/ABS_Y, and its key bits make a
/// keyboard class too. vioinput sends any EV_KEY through the keyboard class without looking at
/// the bitmap, but sizes its pressed-keys array by the highest HID usage in it and has no bounds
/// check: so the pointer bitmap also carries the key with the highest usage, KEY_KPRIGHTPAREN
/// (180 -> 0xB7; above the Japanese keys 0x87..0x94 and LANG1/LANG2 0x90/0x91). It has to sit at
/// byte 20 or later: the same bytes are also read as each axis' ABS_INFO (min 0..3, max 4..7,
/// fuzz 8..11, flat 12..15, res 16..19), and key bits there (the first try put KEY_RO etc. at
/// byte 11 and LANG1/2 at byte 15) made fuzz and flat huge: the pointer stopped following moves
/// and clicks landed off. As an ABS code 180 is > 63 (the tablet sets it aside); as a REL code it
/// becomes one unused slider axis.
pub fn configurations(windows: bool) -> anyhow::Result<Vec<Retained<AnyObject>>> {
    use std::sync::atomic::Ordering::Relaxed;
    let mut ptr = [0u8; 35];
    ptr[0] = 0x03; // ABS_X, ABS_Y (and min = 3)
    ptr[4..8].copy_from_slice(&(ABS_MAX as u32).to_le_bytes()); // max
    ptr[34] = 0x07; // BTN_LEFT, BTN_RIGHT, BTN_MIDDLE
    let k = 180u16; // KEY_KPRIGHTPAREN
    ptr[(k / 8) as usize] |= 1 << (k % 8);
    WINDOWS.store(windows, Relaxed);
    if windows {
        KBD.store(0, Relaxed);
        PTR.store(0, Relaxed);
        return Ok(vec![one(0, &ptr)?]);
    }
    let mut kbd = [0xffu8; 16];
    kbd[0] = 0xfe; // no KEY_RESERVED (also keeps REL_X, ABS_X, SW_LID off)
    KBD.store(0, Relaxed);
    PTR.store(1, Relaxed);
    Ok(vec![one(0, &kbd)?, one(1, &ptr)?])
}


/// Debugging: one raw event (+ SYN) on the pointer device (ctl NAME "vraw TYPE CODE VALUE").
pub fn raw(ty: u16, code: u16, value: i32) -> u64 {
    send(PTR.load(std::sync::atomic::Ordering::Relaxed), &[(ty, code, value), (EV_SYN, 0, 0)])
}
