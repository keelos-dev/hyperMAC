//! Windows guests: read the guest's framebuffer straight out of guest memory.
//!
//! VZ's EFI only has a Blt-only GOP and Windows has no virtio-gpu driver in the box, so the
//! boot stick's EFI shim (tools/efiprobe) gives Windows a linear framebuffer in ordinary RAM.
//! After ExitBootServices nothing copies it to the virtio-gpu any more; the host reads it instead
//! (QEMU's ramfb idea). Guest memory is reachable from the host through a custom virtio device
//! (macOS 27 `VZCustomVirtioDevice.guestMemoryMappingAtPhysicalAddress:length:`); the device
//! itself does nothing and no guest driver binds to it.
//!
//! The shim puts a header page right before the framebuffer:
//!   u64 magic "HYMFBFB1", u64 framebuffer address, u32 width, u32 height, u32 pixels per line.

use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, NSObject};
use objc2::{AllocAnyThread, define_class, msg_send};
use objc2_foundation::{NSData, NSObjectProtocol};

pub const MAGIC: u64 = u64::from_le_bytes(*b"HYMFBFB1");
/// where VZ puts guest RAM on Apple silicon (generic platform)
pub const RAM_BASE: u64 = 0x7000_0000;

struct Dev(Option<Retained<AnyObject>>);
unsafe impl Send for Dev {}
static DEVICE: Mutex<Dev> = Mutex::new(Dev(None));
static KEEP: Mutex<Vec<Keep>> = Mutex::new(Vec::new());
struct Keep(#[allow(dead_code)] Retained<AnyObject>);
unsafe impl Send for Keep {}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "HyperMacFbConfigDelegate"]
    struct ConfigDelegate;

    unsafe impl NSObjectProtocol for ConfigDelegate {}

    impl ConfigDelegate {
        #[unsafe(method(customVirtioConfiguration:didCreateDevice:))]
        fn did_create(&self, _cfg: &AnyObject, device: &AnyObject) {
            DEVICE.lock().unwrap().0 = unsafe { Retained::retain(device as *const _ as *mut AnyObject) };
        }
    }
);

/// The device's configuration (None before macOS 27).
pub fn configuration() -> Option<Retained<AnyObject>> {
    let cls = |n: &std::ffi::CStr| AnyClass::get(n);
    unsafe {
        let c: Retained<AnyObject> = msg_send![cls(c"VZCustomVirtioDeviceConfiguration")?, new];
        // an unassigned virtio device ID: nothing in the guest binds to it
        let _: () = msg_send![&*c, setDeviceID: 63u16];
        let _: () = msg_send![&*c, setPCIClassID: 0xFFu8];
        let _: () = msg_send![&*c, setPCISubclassID: 0x00u8];
        let _: () = msg_send![&*c, setVirtioQueueCount: 1u16];
        let sc: objc2::rc::Allocated<AnyObject> = msg_send![cls(c"VZVirtioDeviceSpecificConfiguration")?, alloc];
        let sc: Retained<AnyObject> = msg_send![sc, initWithConfigurationData: &*NSData::with_bytes(&[0u8; 4])];
        let _: () = msg_send![&*c, setDeviceSpecificConfiguration: &*sc];
        let del: Retained<ConfigDelegate> = msg_send![ConfigDelegate::alloc(), init];
        let queue = dispatch2::DispatchQueue::new("hypermac.fbdev", None);
        let p: objc2::rc::Allocated<AnyObject> = msg_send![cls(c"VZCustomVirtioDeviceDelegateProvider")?, alloc];
        let p: Retained<AnyObject> = msg_send![p, initWithDeviceQueue: &*queue, delegate: &*del];
        let _: () = msg_send![&*c, setProvider: &*p];
        KEEP.lock().unwrap().push(Keep(Retained::cast_unchecked(del)));
        Some(c)
    }
}

/// A view of guest RAM [RAM_BASE, RAM_BASE + len).
fn map(len: u64) -> anyhow::Result<(Retained<AnyObject>, *const u8)> {
    let d = DEVICE.lock().unwrap().0.clone().ok_or_else(|| anyhow::anyhow!("no framebuffer device"))?;
    let m: Option<Retained<AnyObject>> =
        unsafe { msg_send![&*d, guestMemoryMappingAtPhysicalAddress: RAM_BASE, length: len as usize] };
    let m = m.ok_or_else(|| anyhow::anyhow!("guest memory not mappable"))?;
    let p: *mut std::ffi::c_void = unsafe { msg_send![&*m, mutableBytes] };
    Ok((m, p as *const u8))
}

/// The offset (from RAM_BASE) of the header page with `magic` whose next u64 is the address of
/// the page after it. The EFI shim allocates high: search from the top down.
fn find_page(p: *const u8, mem: u64, magic: u64) -> Option<u64> {
    let rd64 = |o: u64| unsafe { (p.add(o as usize) as *const u64).read_unaligned() };
    let mut o = (mem - 4096) & !4095;
    loop {
        if rd64(o) == magic && rd64(o + 8) == RAM_BASE + o + 4096 {
            return Some(o);
        }
        if o == 0 {
            return None;
        }
        o -= 4096;
    }
}

/// Find the shim's header page; (framebuffer address, w, h, pixels per line).
pub fn find(mem: u64) -> anyhow::Result<(u64, u32, u32, u32)> {
    let (_m, p) = map(mem)?;
    let rd32 = |o: u64| unsafe { (p.add(o as usize) as *const u32).read_unaligned() };
    let o = find_page(p, mem, MAGIC).ok_or_else(|| anyhow::anyhow!("no framebuffer header in guest memory"))?;
    Ok((RAM_BASE + o + 4096, rd32(o + 16), rd32(o + 20), rd32(o + 24)))
}

/// The EFI shim's log ("serial port in RAM", tools/efiprobe/src/hmlog.rs) -> a text file.
pub fn log(mem: u64, path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    let (_m, p) = map(mem)?;
    let o = find_page(p, mem, u64::from_le_bytes(*b"HYMLOG01"))
        .ok_or_else(|| anyhow::anyhow!("no log in guest memory"))?;
    let rd32 = |o: u64| unsafe { (p.add(o as usize) as *const u32).read_unaligned() };
    let (cap, n) = (rd32(o + 16) as usize, rd32(o + 20) as usize);
    let text = unsafe { std::slice::from_raw_parts(p.add(o as usize + 4096), n.min(cap)) };
    std::fs::write(path, text)?;
    Ok(serde_json::json!({"ok": true, "bytes": n, "path": path}))
}

/// Save the framebuffer as a 32-bit BMP.
pub fn shot(mem: u64, path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    let (fb, w, h, pitch) = find(mem)?;
    let (_m, p) = map(mem)?;
    let off = (fb - RAM_BASE) as usize;
    let (w, h, pitch) = (w as usize, h as usize, pitch as usize);
    let mut out = Vec::with_capacity(54 + w * h * 4);
    let size = (54 + w * h * 4) as u32;
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(-(h as i32)).to_le_bytes()); // top-down
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for y in 0..h {
        let row = unsafe { std::slice::from_raw_parts(p.add(off + y * pitch * 4), w * 4) };
        out.extend_from_slice(row);
    }
    std::fs::write(path, out)?;
    Ok(serde_json::json!({"ok": true, "fb": format!("{fb:#x}"), "w": w, "h": h, "path": path}))
}

/// Guest physical memory [addr, addr+len) as hex (ctl NAME peek ADDR LEN).
pub fn peek(mem: u64, addr: u64, len: usize) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(addr >= RAM_BASE && addr + len as u64 <= RAM_BASE + mem, "outside guest RAM");
    let (_m, p) = map(mem)?;
    let b = unsafe { std::slice::from_raw_parts(p.add((addr - RAM_BASE) as usize), len.min(1 << 20)) };
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(serde_json::json!({"addr": format!("{addr:#x}"), "hex": hex}))
}
