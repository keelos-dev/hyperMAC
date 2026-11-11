//! A serial port without a UART: a text log in reserved guest RAM that the host reads.
//!
//! VZ's real UARTs (_VZPL011SerialPortConfiguration, _VZ16550...) refuse to start for us
//! ("Internal Virtualization error"), and nothing in EFI or Windows drives virtio-console. But
//! hyperMAC can map guest RAM (fbdev.rs), so: one header page
//!   u64 magic "HYMLOG01", u64 data address, u32 capacity, u32 bytes written
//! followed by the text. `hypermac ctl NAME log` saves it as DIR/hmlog.txt, also after a hang.

use core::fmt::Write;
use core::ptr;

use uefi::boot::{self, AllocateType, MemoryType};

const PAGES: usize = 64; // 4 KiB header + 252 KiB of text
static mut HDR: *mut u8 = ptr::null_mut();
static mut DATA: *mut u8 = ptr::null_mut();
static mut CAP: usize = 0;
static mut POS: usize = 0;

pub fn init() -> bool {
    match boot::allocate_pages(AllocateType::AnyPages, MemoryType::RESERVED, PAGES) {
        Ok(p) => unsafe {
            ptr::write_bytes(p.as_ptr(), 0, PAGES * 4096);
            HDR = p.as_ptr();
            DATA = HDR.add(4096);
            CAP = (PAGES - 1) * 4096;
            (HDR.add(8) as *mut u64).write(DATA as u64);
            (HDR.add(16) as *mut u32).write(CAP as u32);
            // the magic last: the host only trusts a complete header
            (HDR as *mut u64).write_volatile(u64::from_le_bytes(*b"HYMLOG01"));
            true
        },
        Err(_) => false,
    }
}

pub fn write(s: &str) {
    unsafe {
        if DATA.is_null() {
            return;
        }
        let n = s.len().min(CAP - POS);
        ptr::copy_nonoverlapping(s.as_ptr(), DATA.add(POS), n);
        POS += n;
        (HDR.add(20) as *mut u32).write_volatile(POS as u32);
    }
}

pub struct W;
impl Write for W {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        write(s);
        Ok(())
    }
}

#[macro_export]
macro_rules! hlog {
    ($($t:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::hmlog::W, $($t)*);
    }};
}
