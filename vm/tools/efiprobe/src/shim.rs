//! A linear framebuffer in front of a BltOnly GOP.
//!
//! VZ's EFI (EDK II with a virtio-gpu GOP) reports PixelBltOnly: no framebuffer, drawing only
//! through Blt(). The Windows boot manager wants a linear framebuffer. So: allocate one in RAM,
//! make every GOP report it (BGRX, pitch = width), and copy it to the screen with Blt() from a
//! periodic timer. SetMode is hooked so a mode change keeps the fake framebuffer.
//! After ExitBootServices the timer is gone; the OS's own virtio-gpu driver has to take over.

use core::ffi::c_void;
use core::ptr::{self, NonNull};

use uefi::boot::{self, AllocateType, EventType, MemoryType, SearchType, TimerTrigger, Tpl};
use uefi::proto::console::gop::GraphicsOutput;
use uefi::{Event, Identify, Status};
use uefi_raw::protocol::console::{
    GraphicsOutputBltOperation, GraphicsOutputBltPixel, GraphicsOutputProtocol, GraphicsPixelFormat, PixelBitmask,
};

type SetMode = unsafe extern "efiapi" fn(*mut GraphicsOutputProtocol, u32) -> Status;
type Blt = unsafe extern "efiapi" fn(
    *mut GraphicsOutputProtocol,
    *mut GraphicsOutputBltPixel,
    GraphicsOutputBltOperation,
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
) -> Status;

const MAX: usize = 4;
static mut GOPS: [(*mut GraphicsOutputProtocol, Option<SetMode>); MAX] = [(ptr::null_mut(), None); MAX];
static mut NGOPS: usize = 0;
/// the GOP that really draws (the first one: the virtio-gpu's own, not the console splitter's)
static mut REAL: *mut GraphicsOutputProtocol = ptr::null_mut();
static mut REAL_BLT: Option<Blt> = None;
static mut FB: u64 = 0;
static mut FB_W: usize = 0;
static mut FB_H: usize = 0;
pub static mut TICKS: u64 = 0;
pub static mut TRACING: bool = false;

unsafe fn alloc_fb(w: usize, h: usize) -> bool {
    // one header page (for the host: hyperMAC's fbdev.rs finds the framebuffer by it) + pixels
    let pages = 1 + (w * h * 4).div_ceil(4096);
    // reserved: the OS must not hand these pages out while it still draws there
    match boot::allocate_pages(AllocateType::AnyPages, MemoryType::RESERVED, pages) {
        Ok(p) => unsafe {
            ptr::write_bytes(p.as_ptr(), 0, pages * 4096);
            let hdr = p.as_ptr();
            FB = hdr as u64 + 4096;
            FB_W = w;
            FB_H = h;
            (hdr as *mut u64).write(u64::from_le_bytes(*b"HYMFBFB1"));
            (hdr.add(8) as *mut u64).write(FB);
            (hdr.add(16) as *mut u32).write(w as u32);
            (hdr.add(20) as *mut u32).write(h as u32);
            (hdr.add(24) as *mut u32).write(w as u32);
            true
        },
        Err(_) => false,
    }
}

/// Make one GOP report the fake framebuffer for its current mode.
unsafe fn patch(g: *mut GraphicsOutputProtocol) {
    unsafe {
        let m = (*g).mode;
        let info = (*m).info;
        let (w, h) = ((*info).horizontal_resolution as usize, (*info).vertical_resolution as usize);
        if (w, h) != (FB_W, FB_H) {
            alloc_fb(w, h);
        }
        (*info).pixel_format = GraphicsPixelFormat::PIXEL_BLUE_GREEN_RED_RESERVED_8_BIT_PER_COLOR;
        (*info).pixel_information = PixelBitmask::default();
        (*info).pixels_per_scan_line = w as u32;
        (*m).frame_buffer_base = FB;
        (*m).frame_buffer_size = w * h * 4;
    }
}

unsafe extern "efiapi" fn set_mode_hook(g: *mut GraphicsOutputProtocol, n: u32) -> Status {
    unsafe {
        let mut st = Status::UNSUPPORTED;
        for i in 0..NGOPS {
            if GOPS[i].0 == g {
                if let Some(orig) = GOPS[i].1 {
                    st = orig(g, n);
                }
            }
        }
        for i in 0..NGOPS {
            patch(GOPS[i].0);
        }
        st
    }
}

unsafe extern "efiapi" fn tick(_e: Event, _ctx: Option<NonNull<c_void>>) {
    unsafe {
        if let (false, Some(blt)) = (REAL.is_null(), REAL_BLT) {
            let _ = blt(
                REAL,
                FB as *mut GraphicsOutputBltPixel,
                GraphicsOutputBltOperation::BLT_BUFFER_TO_VIDEO,
                0,
                0,
                0,
                0,
                FB_W,
                FB_H,
                FB_W * 4,
            );
            TICKS += 1;
        }
    }
}

/// Install the shim on every GOP. Returns (number of GOPs, framebuffer address, w, h).
pub fn install() -> Result<(usize, u64, usize, usize), Status> {
    unsafe {
        let handles = boot::locate_handle_buffer(SearchType::ByProtocol(&GraphicsOutput::GUID)).map_err(|e| e.status())?;
        for &h in handles.iter() {
            if NGOPS == MAX {
                break;
            }
            let p = boot::open_protocol::<GraphicsOutput>(
                boot::OpenProtocolParams { handle: h, agent: boot::image_handle(), controller: None },
                boot::OpenProtocolAttributes::GetProtocol,
            )
            .map_err(|e| e.status())?;
            // GraphicsOutput is a transparent wrapper of the raw protocol struct
            let g = &*p as *const GraphicsOutput as *mut GraphicsOutputProtocol;
            core::mem::forget(p);
            if REAL.is_null() {
                REAL = g;
                REAL_BLT = Some((*g).blt);
            }
            GOPS[NGOPS] = (g, Some((*g).set_mode));
            NGOPS += 1;
            (*g).set_mode = set_mode_hook;
            patch(g);
        }
        if REAL.is_null() {
            return Err(Status::NOT_FOUND);
        }
        let ev = boot::create_event(EventType::TIMER | EventType::NOTIFY_SIGNAL, Tpl::CALLBACK, Some(tick), None)
            .map_err(|e| e.status())?;
        // 100 ms (units of 100 ns)
        boot::set_timer(&ev, TimerTrigger::Periodic(1_000_000)).map_err(|e| e.status())?;
        core::mem::forget(ev);
        Ok((NGOPS, FB, FB_W, FB_H))
    }
}

/// Draw values as bit strips: one row per value, 64 squares of 24 px (MSB first), white = 1,
/// starting at (40, 100), rows 32 px apart, on a mid-grey panel.
pub unsafe fn draw_bits(vals: &[u64]) {
    unsafe {
        if FB == 0 {
            return;
        }
        let fb = core::slice::from_raw_parts_mut(FB as *mut u32, FB_W * FB_H);
        let (x0, y0, sq, gap) = (40usize, 100usize, 24usize, 32usize);
        for y in y0 - 8..y0 + vals.len() * gap + 8 {
            for x in x0 - 8..x0 + 64 * sq + 8 {
                fb[y * FB_W + x] = 0x0080_8080;
            }
        }
        for (row, v) in vals.iter().enumerate() {
            for bit in 0..64 {
                let on = (v >> (63 - bit)) & 1 == 1;
                let c = if on { 0x00ff_ffff } else { 0 };
                for y in 0..sq - 4 {
                    for x in 0..sq - 4 {
                        fb[(y0 + row * gap + y) * FB_W + x0 + bit * sq + x] = c;
                    }
                }
            }
        }
    }
}

/// Copy the framebuffer to the screen now (outside the timer).
pub unsafe fn present() {
    unsafe {
        if let (false, Some(blt)) = (REAL.is_null(), REAL_BLT) {
            let _ = blt(
                REAL,
                FB as *mut GraphicsOutputBltPixel,
                GraphicsOutputBltOperation::BLT_BUFFER_TO_VIDEO,
                0, 0, 0, 0, FB_W, FB_H, FB_W * 4,
            );
        }
    }
}

/// Copy rows y..y+h of the framebuffer to the screen.
pub unsafe fn present_rows(y: usize, h: usize) {
    unsafe {
        if let (false, Some(blt)) = (REAL.is_null(), REAL_BLT) {
            let _ = blt(
                REAL,
                FB as *mut GraphicsOutputBltPixel,
                GraphicsOutputBltOperation::BLT_BUFFER_TO_VIDEO,
                0, y, 0, y, FB_W, h.min(FB_H - y), FB_W * 4,
            );
        }
    }
}
