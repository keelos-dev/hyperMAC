//! Our own virtio-gpu (2D only) as a VZCustomVirtioDevice (macOS 27), so the guest's picture
//! lands in this process: no window, no virtual display. Linux uses its virtio_gpu driver,
//! Windows virtio-win's viogpudo. Ported from macRDP's spike (spikes/s1-vz/src/gpu.rs, S1b).
//!
//! virtio-gpu 2D (virtio spec 5.7): controlq 0, cursorq 1; the guest creates a resource,
//! attaches guest pages as its backing, TRANSFER_TO_HOST_2D copies a rect from the pages
//! into the host's copy, RESOURCE_FLUSH says "this rect of the scanout changed".
//! Here: the host copy is kept per resource; `shot` writes the scanout's resource as a BMP
//! (ctl NAME vshot); command counts go to stderr every 5 s.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, NSObject};
use objc2::{AllocAnyThread, define_class, msg_send};
use objc2_foundation::{NSData, NSError, NSObjectProtocol};

const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
const CMD_GET_EDID: u32 = 0x010a;
const RESP_OK_EDID: u32 = 0x1104;
/// VIRTIO_GPU_F_EDID: Windows (viogpudo) and Linux take the preferred mode from it
const F_EDID: u32 = 1 << 1;
const CMD_UPDATE_CURSOR: u32 = 0x0300;
const CMD_MOVE_CURSOR: u32 = 0x0301;
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_ERR_UNSPEC: u32 = 0x1200;
const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;
const FLAG_FENCE: u32 = 1;

/// One guest memory range of a resource's backing, mapped into this process.
struct Map {
    _obj: Retained<AnyObject>, // VZGuestMemoryMapping: keeps the mapping alive
    ptr: *mut u8,
    len: usize,
}

struct Res {
    w: u32,
    h: u32,
    fmt: u32,
    pix: Vec<u8>, // host copy, w*h*4, the guest's format (B8G8R8X8 for Linux XRGB8888)
    backing: Vec<Map>,
}

struct Gpu {
    width: u32,
    height: u32,
    res: HashMap<u32, Res>,
    scanout: u32,
    /// the part of that resource SET_SCANOUT showed (x, y, w, h): a guest's buffer may be
    /// larger than its screen (GNOME on Debian 13: 1280x832 for 1280x800, seen 2026-11-12)
    view: (u32, u32, u32, u32),
    counts: HashMap<u32, u64>,
    /// frames flushed to the scanout so far (cursor changes count too)
    pub frames: u64,
    log: Instant,
    /// the hardware cursor (cursorq): image (w, h, B,G,R,A), hot spot, position on the scanout
    cursor: Option<(u32, u32, Vec<u8>)>,
    hot: (i32, i32),
    cursor_pos: (i32, i32),
    /// the guest has reported a cursor position (viogpudo with HWCursor=1, Linux): its pointer
    /// position is known, input can correct relative moves against it
    cursor_seen: bool,
}
unsafe impl Send for Gpu {}

static GPU: Mutex<Option<Gpu>> = Mutex::new(None);
/// the device and our delegate objects (VZ holds its delegates weakly)
static KEEP: Mutex<Vec<SendObj>> = Mutex::new(Vec::new());
struct SendObj(#[allow(dead_code)] Retained<AnyObject>);
unsafe impl Send for SendObj {}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "HyperMacGpuConfigDelegate"]
    struct ConfigDelegate;

    unsafe impl NSObjectProtocol for ConfigDelegate {}

    impl ConfigDelegate {
        #[unsafe(method(customVirtioConfiguration:didCreateDevice:))]
        fn did_create(&self, _cfg: &AnyObject, device: &AnyObject) {
            eprintln!("gpu: device created");
            let d = DeviceDelegate::alloc().set_ivars(());
            let d: Retained<DeviceDelegate> = unsafe { msg_send![super(d), init] };
            let _: () = unsafe { msg_send![device, setDelegate: &*d] };
            let mut k = KEEP.lock().unwrap();
            k.push(SendObj(unsafe { Retained::cast_unchecked(d) }));
            k.push(SendObj(unsafe { Retained::retain(device as *const _ as *mut AnyObject) }.unwrap()));
        }
    }
);

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "HyperMacGpuDeviceDelegate"]
    struct DeviceDelegate;

    unsafe impl NSObjectProtocol for DeviceDelegate {}

    impl DeviceDelegate {
        #[unsafe(method(customVirtioDeviceDidAcceptDriverOk:))]
        fn driver_ok(&self, device: &AnyObject) {
            let nf: Option<Retained<AnyObject>> = unsafe { msg_send![device, negotiatedFeatures] };
            let f = nf.map(|n| unsafe { (msg_send![&*n, subset0], msg_send![&*n, subset1]) }).unwrap_or((0u32, 0u32));
            eprintln!("gpu: DRIVER_OK, features {:#x} {:#x}", f.0, f.1);
        }

        #[unsafe(method(customVirtioDeviceWillReset:))]
        fn will_reset(&self, _device: &AnyObject) {
            eprintln!("gpu: reset");
            if let Some(g) = GPU.lock().unwrap().as_mut() {
                g.res.clear();
                g.scanout = 0;
            }
        }

        #[unsafe(method(customVirtioDevice:didReceiveNotificationForQueue:))]
        fn notified(&self, device: &AnyObject, queue: &AnyObject) {
            let qi: u16 = unsafe { msg_send![queue, queueIndex] };
            loop {
                let el: Option<Retained<AnyObject>> = unsafe { msg_send![queue, nextElement] };
                let Some(el) = el else { break };
                let n: usize = unsafe { msg_send![&*el, readBuffersByteCount] };
                let data: Option<Retained<NSData>> = if n > 0 {
                    unsafe { msg_send![&*el, readBytesWithExactLength: n, error: std::ptr::null_mut::<*mut NSError>()] }
                } else {
                    None
                };
                let req = data.map(|d| d.to_vec()).unwrap_or_default();
                if qi == 0 {
                    let resp = command(device, &req);
                    let d = NSData::with_bytes(&resp);
                    let ok: bool = unsafe { msg_send![&*el, writeData: &*d, error: std::ptr::null_mut::<*mut NSError>()] };
                    if !ok {
                        let room: usize = unsafe { msg_send![&*el, writeBuffersByteCount] };
                        eprintln!("gpu: writeData failed ({} bytes, room {room})", resp.len());
                    }
                } else if req.len() >= 4 {
                    // cursorq: no response expected
                    let mut g = GPU.lock().unwrap();
                    if let Some(g) = g.as_mut() {
                        *g.counts.entry(le32(&req, 0)).or_default() += 1;
                        cursor_cmd(g, &req);
                    }
                    if req.len() >= 40 && std::env::var_os("VZ_LOG_CURSOR").is_some() {
                        // virtio_gpu_update_cursor: hdr, pos {scanout, x, y, pad}, resource, hot_x, hot_y
                        eprintln!("cursor {:#x} at {},{} res {} hot {},{}", le32(&req, 0), le32(&req, 28), le32(&req, 32), le32(&req, 40.min(req.len() - 4)), if req.len() >= 48 { le32(&req, 44) } else { 0 }, if req.len() >= 52 { le32(&req, 48) } else { 0 });
                    }
                }
                let _: () = unsafe { msg_send![&*el, returnToQueue] };
            }
            after();
        }
    }
);

/// cursorq: virtio_gpu_update_cursor { hdr, pos { scanout, x, y, pad }, resource, hot_x, hot_y }.
/// The guest draws its pointer here instead of into the frame; we put it on the picture.
fn cursor_cmd(g: &mut Gpu, req: &[u8]) {
    if req.len() < 40 {
        return;
    }
    let ty = le32(req, 0);
    g.cursor_pos = (le32(req, 28) as i32, le32(req, 32) as i32);
    g.cursor_seen = true;
    if ty == CMD_UPDATE_CURSOR && req.len() >= 52 {
        let id = le32(req, 40);
        g.hot = (le32(req, 44) as i32, le32(req, 48) as i32);
        g.cursor = if id == 0 { None } else { g.res.get(&id).map(|r| (r.w, r.h, r.pix.clone())) };
    }
    if ty == CMD_UPDATE_CURSOR || ty == CMD_MOVE_CURSOR {
        g.frames += 1;
    }
}

/// Alpha-blend the cursor (B,G,R,A) onto a B,G,R,X picture.
fn draw_cursor(g: &Gpu, pix: &mut [u8], w: u32, h: u32) {
    let Some((cw, ch, cp)) = g.cursor.as_ref() else { return };
    let (x0, y0) = (g.cursor_pos.0 - g.hot.0, g.cursor_pos.1 - g.hot.1);
    for cy in 0..*ch as i32 {
        let y = y0 + cy;
        if y < 0 || y >= h as i32 {
            continue;
        }
        for cx in 0..*cw as i32 {
            let x = x0 + cx;
            if x < 0 || x >= w as i32 {
                continue;
            }
            let s = ((cy * *cw as i32 + cx) * 4) as usize;
            let a = cp[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let d = ((y * w as i32 + x) * 4) as usize;
            for k in 0..3 {
                pix[d + k] = ((cp[s + k] as u32 * a + pix[d + k] as u32 * (255 - a)) / 255) as u8;
            }
        }
    }
}

/// One controlq request -> its response.
fn command(device: &AnyObject, req: &[u8]) -> Vec<u8> {
    let mut g = GPU.lock().unwrap();
    let g = g.as_mut().unwrap();
    if req.len() < 24 {
        return hdr(RESP_ERR_UNSPEC, 0, 0);
    }
    let (ty, flags, fence) = (le32(req, 0), le32(req, 4), le64(req, 8));
    *g.counts.entry(ty).or_default() += 1;
    let ok = |code: u32| hdr(code, flags & FLAG_FENCE, fence);
    match ty {
        CMD_GET_DISPLAY_INFO => {
            let mut r = ok(RESP_OK_DISPLAY_INFO);
            for i in 0..16u32 {
                let (w, h, en) = if i == 0 { (g.width, g.height, 1) } else { (0, 0, 0) };
                for v in [0, 0, w, h, en, 0] {
                    r.extend_from_slice(&u32::to_le_bytes(v));
                }
            }
            r
        }
        CMD_RESOURCE_CREATE_2D if req.len() >= 40 => {
            let (id, fmt, w, h) = (le32(req, 24), le32(req, 28), le32(req, 32), le32(req, 36));
            if id == 0 || w == 0 || h == 0 || w > 8192 || h > 8192 {
                return ok(RESP_ERR_INVALID_PARAMETER);
            }
            eprintln!("gpu: create resource {id} {w}x{h} format {fmt}");
            g.res.insert(id, Res { w, h, fmt, pix: vec![0; (w * h * 4) as usize], backing: vec![] });
            ok(RESP_OK_NODATA)
        }
        CMD_RESOURCE_UNREF if req.len() >= 28 => {
            let id = le32(req, 24);
            if g.scanout == id {
                g.scanout = 0;
            }
            match g.res.remove(&id) {
                Some(_) => ok(RESP_OK_NODATA),
                None => ok(RESP_ERR_INVALID_RESOURCE_ID),
            }
        }
        CMD_RESOURCE_ATTACH_BACKING if req.len() >= 32 => {
            let (id, n) = (le32(req, 24), le32(req, 28) as usize);
            if req.len() < 32 + n * 16 {
                return ok(RESP_ERR_INVALID_PARAMETER);
            }
            let mut maps = Vec::with_capacity(n);
            for i in 0..n {
                let o = 32 + i * 16;
                let (addr, len) = (le64(req, o), le32(req, o + 8) as usize);
                let m: Option<Retained<AnyObject>> =
                    unsafe { msg_send![device, guestMemoryMappingAtPhysicalAddress: addr, length: len] };
                let Some(m) = m else {
                    eprintln!("gpu: no mapping for {addr:#x}+{len:#x}");
                    return ok(RESP_ERR_UNSPEC);
                };
                let ptr: *mut std::ffi::c_void = unsafe { msg_send![&*m, mutableBytes] };
                maps.push(Map { _obj: m, ptr: ptr as *mut u8, len });
            }
            match g.res.get_mut(&id) {
                Some(r) => {
                    r.backing = maps;
                    ok(RESP_OK_NODATA)
                }
                None => ok(RESP_ERR_INVALID_RESOURCE_ID),
            }
        }
        CMD_RESOURCE_DETACH_BACKING if req.len() >= 28 => match g.res.get_mut(&le32(req, 24)) {
            Some(r) => {
                r.backing.clear();
                ok(RESP_OK_NODATA)
            }
            None => ok(RESP_ERR_INVALID_RESOURCE_ID),
        },
        CMD_SET_SCANOUT if req.len() >= 48 => {
            let (sid, id) = (le32(req, 40), le32(req, 44));
            let (w, h) = (le32(req, 32), le32(req, 36));
            eprintln!("gpu: set scanout {sid} -> resource {id} ({w}x{h})");
            if sid != 0 {
                return ok(RESP_ERR_INVALID_PARAMETER);
            }
            if id != 0 && !g.res.contains_key(&id) {
                return ok(RESP_ERR_INVALID_RESOURCE_ID);
            }
            g.scanout = id;
            g.view = (le32(req, 24), le32(req, 28), w, h);
            ok(RESP_OK_NODATA)
        }
        CMD_TRANSFER_TO_HOST_2D if req.len() >= 56 => {
            let (x, y, w, h) = (le32(req, 24), le32(req, 28), le32(req, 32), le32(req, 36));
            let (off, id) = (le64(req, 40) as usize, le32(req, 48));
            let Some(r) = g.res.get_mut(&id) else { return ok(RESP_ERR_INVALID_RESOURCE_ID) };
            if x + w > r.w || y + h > r.h {
                return ok(RESP_ERR_INVALID_PARAMETER);
            }
            let stride = r.w as usize * 4;
            for row in 0..h as usize {
                let src = off + row * stride;
                let dst = (y as usize + row) * stride + x as usize * 4;
                let n = w as usize * 4;
                gather(&r.backing, src, &mut r.pix[dst..dst + n]);
            }
            ok(RESP_OK_NODATA)
        }
        CMD_RESOURCE_FLUSH if req.len() >= 48 => {
            if le32(req, 40) == g.scanout {
                g.frames += 1;
            }
            ok(RESP_OK_NODATA)
        }
        CMD_GET_EDID => {
            // virtio_gpu_resp_edid: hdr, size, padding, edid[1024]
            let mut r = ok(RESP_OK_EDID);
            let e = edid(g.width, g.height);
            r.extend_from_slice(&(e.len() as u32).to_le_bytes());
            r.extend_from_slice(&[0; 4]);
            r.extend_from_slice(&e);
            r.resize(24 + 8 + 1024, 0);
            r
        }
        _ => {
            eprintln!("gpu: unsupported command {ty:#x} ({} bytes)", req.len());
            ok(RESP_ERR_UNSPEC)
        }
    }
}

/// Copy dst.len() bytes starting at `off` of the backing's linear space.
fn gather(backing: &[Map], mut off: usize, dst: &mut [u8]) {
    let mut done = 0;
    for m in backing {
        if done == dst.len() {
            break;
        }
        if off >= m.len {
            off -= m.len;
            continue;
        }
        let n = (m.len - off).min(dst.len() - done);
        unsafe { std::ptr::copy_nonoverlapping(m.ptr.add(off), dst[done..].as_mut_ptr(), n) };
        done += n;
        off = 0;
    }
}

fn hdr(ty: u32, flags: u32, fence: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(24);
    v.extend_from_slice(&ty.to_le_bytes());
    v.extend_from_slice(&flags.to_le_bytes());
    v.extend_from_slice(&fence.to_le_bytes());
    v.extend_from_slice(&[0; 8]);
    v
}

/// An EDID 1.4 base block whose preferred (first detailed) timing is w x h at 60 Hz
/// (CVT reduced-blanking-like numbers; guests only care about the active size).
pub fn edid(w: u32, h: u32) -> [u8; 128] {
    let mut e = [0u8; 128];
    e[0..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
    let id = |c: u8| (c - b'A' + 1) as u16;
    let m = (id(b'H') << 10) | (id(b'Y') << 5) | id(b'M');
    e[8..10].copy_from_slice(&m.to_be_bytes());
    e[10..12].copy_from_slice(&1u16.to_le_bytes()); // product
    e[16] = 1; // week
    e[17] = (2026 - 1990) as u8;
    e[18] = 1; // EDID 1.4
    e[19] = 4;
    e[20] = 0x80; // digital input
    e[21] = 52; // cm
    e[22] = 29;
    e[23] = 120; // gamma 2.2
    e[24] = 0x06; // sRGB default, preferred timing is native
    e[25..35].copy_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]); // sRGB
    for i in 0..8 {
        e[38 + 2 * i] = 1; // no standard timings
        e[39 + 2 * i] = 1;
    }
    // detailed timing descriptor 1: the preferred mode
    let (hb, hf, hs) = (160u32, 48u32, 32u32);
    let vb = ((h * 31).div_ceil(1080)).max(14);
    let (vf, vs) = (3u32, 5u32);
    let clock = ((w + hb) as u64 * (h + vb) as u64 * 60 / 10_000) as u32; // 10 kHz units
    let d = &mut e[54..72];
    d[0..2].copy_from_slice(&(clock.min(0xffff) as u16).to_le_bytes());
    d[2] = w as u8;
    d[3] = hb as u8;
    d[4] = (((w >> 8) & 0xf) << 4 | ((hb >> 8) & 0xf)) as u8;
    d[5] = h as u8;
    d[6] = vb as u8;
    d[7] = (((h >> 8) & 0xf) << 4 | ((vb >> 8) & 0xf)) as u8;
    d[8] = hf as u8;
    d[9] = hs as u8;
    d[10] = ((vf & 0xf) << 4 | (vs & 0xf)) as u8;
    d[11] = (((hf >> 8) & 3) << 6 | ((hs >> 8) & 3) << 4 | ((vf >> 4) & 3) << 2 | ((vs >> 4) & 3)) as u8;
    d[12] = 0x08; // 520 x 290 mm
    d[13] = 0x22;
    d[14] = 0x21;
    d[17] = 0x1a; // digital separate sync, +H, -V
    // descriptor 2: monitor name
    let n = &mut e[72..90];
    n[3] = 0xfc;
    n[5..18].copy_from_slice(b"hyperMAC\n    ");
    // descriptors 3, 4: dummy
    e[93] = 0x10;
    e[111] = 0x10;
    let sum: u32 = e[..127].iter().map(|&b| b as u32).sum();
    e[127] = ((256 - (sum % 256)) % 256) as u8;
    e
}

/// After a batch: the counts every 5 s.
fn after() {
    let mut g = GPU.lock().unwrap();
    let Some(g) = g.as_mut() else { return };
    if g.log.elapsed() >= Duration::from_secs(5) {
        g.log = Instant::now();
        let mut c: Vec<_> = g.counts.iter().map(|(k, v)| format!("{k:#x}:{v}")).collect();
        c.sort();
        eprintln!("gpu: counts {}", c.join(" "));
    }
}

/// The shown part of the scanout's resource: (x, y, w, h), inside the resource.
fn view(g: &Gpu, r: &Res) -> (usize, usize, usize, usize) {
    let (x, y, w, h) = g.view;
    if w == 0 || h == 0 || x + w > r.w || y + h > r.h {
        return (0, 0, r.w as usize, r.h as usize);
    }
    (x as usize, y as usize, w as usize, h as usize)
}

/// That part's pixels, as they are in the resource.
fn shown(g: &Gpu, r: &Res) -> (usize, usize, Vec<u8>) {
    let (x, y, w, h) = view(g, r);
    if (w, h) == (r.w as usize, r.h as usize) {
        return (w, h, r.pix.clone());
    }
    let stride = r.w as usize * 4;
    let mut pix = Vec::with_capacity(w * h * 4);
    for row in y..y + h {
        pix.extend_from_slice(&r.pix[row * stride + x * 4..row * stride + (x + w) * 4]);
    }
    (w, h, pix)
}

/// The scanout as a 32-bit top-down BMP (ctl NAME vshot).
pub fn shot(path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    let g = GPU.lock().unwrap();
    let g = g.as_ref().ok_or_else(|| anyhow::anyhow!("no virtio-gpu"))?;
    let r = g.res.get(&g.scanout).ok_or_else(|| anyhow::anyhow!("nothing on the scanout yet"))?;
    let rfirst = matches!(r.fmt, 67 | 68 | 121 | 134);
    let (w, h, pix) = shown(g, r);
    let mut out = Vec::with_capacity(54 + w * h * 4);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + w * h * 4) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(-(h as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for p in pix.chunks_exact(4) {
        if rfirst {
            out.extend_from_slice(&[p[2], p[1], p[0], 0]);
        } else {
            out.extend_from_slice(&[p[0], p[1], p[2], 0]);
        }
    }
    std::fs::write(path, out)?;
    // how many of each command so far: tells what a guest that shows nothing did draw
    let counts: std::collections::BTreeMap<String, u64> = g.counts.iter().map(|(k, v)| (format!("{k:#x}"), *v as u64)).collect();
    Ok(serde_json::json!({"ok": true, "w": w, "h": h, "format": r.fmt, "frames": g.frames, "path": path, "counts": counts}))
}

/// The configuration of the device, for VZVirtualMachineConfiguration.customVirtioDevices.
pub fn configuration(width: u32, height: u32) -> anyhow::Result<Retained<AnyObject>> {
    *GPU.lock().unwrap() = Some(Gpu {
        width,
        height,
        res: HashMap::new(),
        scanout: 0,
        view: (0, 0, 0, 0),
        counts: HashMap::new(),
        frames: 0,
        log: Instant::now(),
        cursor: None,
        hot: (0, 0),
        cursor_pos: (0, 0),
        cursor_seen: false,
    });
    let cls = |n: &std::ffi::CStr| AnyClass::get(n).ok_or_else(|| anyhow::anyhow!("{n:?}: needs macOS 27"));
    unsafe {
        let c: Retained<AnyObject> = msg_send![cls(c"VZCustomVirtioDeviceConfiguration")?, new];
        let _: () = msg_send![&*c, setDeviceID: 16u16];
        let _: () = msg_send![&*c, setPCIClassID: 0x03u8];
        let _: () = msg_send![&*c, setPCISubclassID: 0x80u8];
        let _: () = msg_send![&*c, setVirtioQueueCount: 2u16];
        let opt: Retained<AnyObject> = msg_send![&*c, optionalFeatures];
        let _: () = msg_send![&*opt, setSubset0: F_EDID];
        // struct virtio_gpu_config: events_read, events_clear, num_scanouts, num_capsets
        let mut cfg = Vec::new();
        for v in [0u32, 0, 1, 0] {
            cfg.extend_from_slice(&v.to_le_bytes());
        }
        let sc: objc2::rc::Allocated<AnyObject> = msg_send![cls(c"VZVirtioDeviceSpecificConfiguration")?, alloc];
        let sc: Retained<AnyObject> = msg_send![sc, initWithConfigurationData: &*NSData::with_bytes(&cfg)];
        let _: () = msg_send![&*c, setDeviceSpecificConfiguration: &*sc];

        let del = ConfigDelegate::alloc().set_ivars(());
        let del: Retained<ConfigDelegate> = msg_send![super(del), init];
        let queue = dispatch2::DispatchQueue::new("hypermac.gpu", None);
        let p: objc2::rc::Allocated<AnyObject> = msg_send![cls(c"VZCustomVirtioDeviceDelegateProvider")?, alloc];
        let p: Retained<AnyObject> = msg_send![p, initWithDeviceQueue: &*queue, delegate: &*del];
        let _: () = msg_send![&*c, setProvider: &*p];
        let mut k = KEEP.lock().unwrap();
        k.push(SendObj(Retained::cast_unchecked(del)));
        Ok(c)
    }
}

/// The scanout now: (frame number, width, height, pixels as B,G,R,X); the pixels are empty
/// when `last` is that same frame number. None before the guest has set up a scanout.
pub fn snapshot(last: Option<u64>) -> Option<(u64, u32, u32, Vec<u8>)> {
    if crate::bhyve::active() {
        return crate::bhyve::snapshot(last);
    }
    let g = GPU.lock().unwrap();
    let g = g.as_ref()?;
    let r = g.res.get(&g.scanout)?;
    if last == Some(g.frames) {
        let (_, _, w, h) = view(g, r);
        return Some((g.frames, w as u32, h as u32, Vec::new()));
    }
    let rfirst = matches!(r.fmt, 67 | 68 | 121 | 134);
    let (w, h, mut pix) = shown(g, r);
    if rfirst {
        for p in pix.chunks_exact_mut(4) {
            p.swap(0, 2);
            p[3] = 0;
        }
    }
    let (w, h) = (w as u32, h as u32);
    draw_cursor(g, &mut pix, w, h);
    Some((g.frames, w, h, pix))
}

/// The scanout's size, if there is one.
pub fn size() -> Option<(u32, u32)> {
    if crate::bhyve::active() {
        return crate::bhyve::size();
    }
    let g = GPU.lock().unwrap();
    let g = g.as_ref()?;
    g.res.get(&g.scanout).map(|r| {
        let (_, _, w, h) = view(g, r);
        (w as u32, h as u32)
    })
}

/// Where the guest says its pointer is (scanout pixels), if it uses the hardware cursor.
pub fn cursor_position() -> Option<(i32, i32)> {
    let g = GPU.lock().unwrap();
    let g = g.as_ref()?;
    g.cursor_seen.then_some(g.cursor_pos)
}
