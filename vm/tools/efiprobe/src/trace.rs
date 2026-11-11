//! Trace what the next image (the Windows boot manager) asks the firmware for: some boot
//! services are replaced by wrappers that record (call, argument) in a ring, then call on.
//! The shim's timer writes the ring to \hmtrace.txt every few seconds, so after a hang the
//! file shows the last calls, and whether the timer itself still ran.

use core::ffi::c_void;
use core::fmt::Write;
use core::ptr;

use alloc::string::String;
use uefi::Status;
use uefi_raw::table::boot::BootServices;
use uefi_raw::{Guid, Handle};

const N: usize = 48;
static mut RING: [(u8, u64, u64); N] = [(0, 0, 0); N];
pub static mut COUNT: u64 = 0;
pub static mut QUIET: bool = false;
static mut LAST_IDS: u64 = 0;
static mut BS: *mut BootServices = ptr::null_mut();

const NAMES: [&str; 14] = [
    "?", "HandleProtocol", "LocateProtocol", "OpenProtocol", "LocateHandleBuffer", "AllocatePages", "ExitBootServices",
    "Stall", "WaitForEvent", "GetMemoryMap", "SetWatchdogTimer", "LoadImage", "StartImage", "CheckEvent",
];

fn rec(id: u8, a: u64, b: u64) {
    unsafe {
        if QUIET {
            return;
        }
        RING[(COUNT as usize) % N] = (id, a, b);
        COUNT += 1;
        if COUNT % 16 == 0 {
            QUIET = true;
            scan(true);
            QUIET = false;
        }
        // we are in firmware context here (the caller switched back to call us): show it now
        LAST_IDS = (LAST_IDS << 8) | id as u64;
    }
}

fn g(guid: *const Guid) -> u64 {
    if guid.is_null() { 0 } else { unsafe { u32::from_le_bytes((*guid).to_bytes()[0..4].try_into().unwrap()) as u64 } }
}

macro_rules! orig {
    ($name:ident : $ty:ty) => {
        static mut $name: Option<$ty> = None;
    };
}

type HandleProtocolFn = unsafe extern "efiapi" fn(Handle, *const Guid, *mut *mut c_void) -> Status;
type LocateProtocolFn = unsafe extern "efiapi" fn(*const Guid, *mut c_void, *mut *mut c_void) -> Status;
type OpenProtocolFn = unsafe extern "efiapi" fn(Handle, *const Guid, *mut *mut c_void, Handle, Handle, u32) -> Status;
type LocateHandleBufferFn = unsafe extern "efiapi" fn(u32, *const Guid, *const c_void, *mut usize, *mut *mut Handle) -> Status;
type AllocatePagesFn = unsafe extern "efiapi" fn(u32, u32, usize, *mut u64) -> Status;
type ExitBootServicesFn = unsafe extern "efiapi" fn(Handle, usize) -> Status;
type StallFn = unsafe extern "efiapi" fn(usize) -> Status;
type WaitForEventFn = unsafe extern "efiapi" fn(usize, *mut *mut c_void, *mut usize) -> Status;
type SetWatchdogFn = unsafe extern "efiapi" fn(usize, u64, usize, *const u16) -> Status;
type CheckEventFn = unsafe extern "efiapi" fn(*mut c_void) -> Status;

orig!(HP: HandleProtocolFn);
orig!(LP: LocateProtocolFn);
orig!(OP: OpenProtocolFn);
orig!(LHB: LocateHandleBufferFn);
orig!(AP: AllocatePagesFn);
orig!(EBS: ExitBootServicesFn);
orig!(ST: StallFn);
orig!(WFE: WaitForEventFn);
orig!(SW: SetWatchdogFn);
orig!(CE: CheckEventFn);

unsafe extern "efiapi" fn hp(h: Handle, guid: *const Guid, i: *mut *mut c_void) -> Status {
    let r = unsafe { HP.unwrap()(h, guid, i) };
    rec(1, g(guid), r.0 as u64);
    r
}
unsafe extern "efiapi" fn lp(guid: *const Guid, reg: *mut c_void, i: *mut *mut c_void) -> Status {
    let r = unsafe { LP.unwrap()(guid, reg, i) };
    rec(2, g(guid), r.0 as u64);
    r
}
unsafe extern "efiapi" fn op(h: Handle, guid: *const Guid, i: *mut *mut c_void, a: Handle, c: Handle, at: u32) -> Status {
    let r = unsafe { OP.unwrap()(h, guid, i, a, c, at) };
    rec(3, g(guid), r.0 as u64);
    r
}
unsafe extern "efiapi" fn lhb(t: u32, guid: *const Guid, k: *const c_void, n: *mut usize, b: *mut *mut Handle) -> Status {
    let r = unsafe { LHB.unwrap()(t, guid, k, n, b) };
    rec(4, g(guid), r.0 as u64);
    r
}
unsafe extern "efiapi" fn ap(t: u32, m: u32, pages: usize, addr: *mut u64) -> Status {
    let r = unsafe { AP.unwrap()(t, m, pages, addr) };
    rec(5, pages as u64, r.0 as u64);
    if r == Status::SUCCESS && !unsafe { QUIET } {
        unsafe {
            if NALLOC < ALLOCS.len() {
                ALLOCS[NALLOC] = (*addr, pages as u64);
                NALLOC += 1;
            }
        }
    }
    r
}

/// Memory the boot manager / loader got from the firmware: images they load land in it.
static mut ALLOCS: [(u64, u64); 4096] = [(0, 0); 4096];
static mut NALLOC: usize = 0;
static mut PATCHED: [u64; 512] = [0; 512];
static mut NPATCHED: usize = 0;
pub static mut IMAGES: u64 = 0;
pub static mut INSNS: u64 = 0;
/// where the kernel was found and the stub length (kpatch.rs)
pub static mut KERNEL: u64 = 0;
pub static mut KSTUB: u64 = 0;
/// (base, subsystem << 48 | size << 16 | changed) of the last few images, for the screen
pub static mut LAST: [(u64, u64); 4] = [(0, 0); 4];

pub unsafe fn mark_patched(base: u64) {
    unsafe {
        if NPATCHED < PATCHED.len() {
            PATCHED[NPATCHED] = base;
            NPATCHED += 1;
        }
    }
}

/// The firmware's TTBR0 (identity map, all RAM writable), saved before the boot manager runs.
pub static mut FW_TTBR0: u64 = 0;

/// Run `f` on the firmware's page tables. The boot manager's own tables map the images it has
/// loaded read-only, and patching them there faults (a permission fault on our own write).
pub unsafe fn on_fw_tables<R>(f: impl FnOnce() -> R) -> R {
    unsafe {
        let (cur, daif): (u64, u64);
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) cur);
        if FW_TTBR0 == 0 || cur == FW_TTBR0 {
            return f();
        }
        core::arch::asm!("mrs {}, daif", "msr daifset, #0xf", out(reg) daif);
        core::arch::asm!("dsb ish", "msr ttbr0_el1, {}", "isb", "tlbi vmalle1", "dsb ish", "isb", in(reg) FW_TTBR0);
        let r = f();
        core::arch::asm!("dsb ish", "msr ttbr0_el1, {}", "isb", "tlbi vmalle1", "dsb ish", "isb", in(reg) cur);
        core::arch::asm!("msr daif, {}", in(reg) daif);
        r
    }
}

/// Look for PE images (page-aligned "MZ") in the recorded allocations; patch new ones.
/// apps_only: only Windows boot applications (subsystem 16, the loader). The kernel, HAL and
/// drivers are patched only at ExitBootServices: the loader has checked their hashes by then.
pub unsafe fn scan(apps_only: bool) {
    unsafe { on_fw_tables(|| scan_here(apps_only)) }
}

unsafe fn scan_here(apps_only: bool) {
    unsafe {
        for i in 0..NALLOC {
            let (start, pages) = ALLOCS[i];
            for pg in 0..pages {
                let b = start + pg * 4096;
                if (b as *const u16).read_volatile() != 0x5A4D {
                    continue;
                }
                if PATCHED[..NPATCHED].contains(&b) {
                    continue;
                }
                let pe = ((b + 0x3c) as *const u32).read_unaligned() as u64;
                if pe < 0x800 && apps_only && ((b + pe + 24 + 68) as *const u16).read_unaligned() != 16 {
                    continue;
                }
                if let Some((sub, size, n)) = crate::exc::patch_image(b as *mut u8, crate::KEEP_VBAR) {
                    // nothing found while the loader is still copying the sections in: look again
                    // next time (the headers arrive first; the code may still be zeros)
                    if n == 0 && apps_only {
                        continue;
                    }
                    mark_patched(b);
                    hlog!("image {b:#x}: subsystem {sub} size {size:#x}, {n} PMU instructions patched");
                    if !apps_only && KERNEL == 0 {
                        let r = crate::kpatch::patch_kernel(b as *mut u8);
                        if let Err(e) = r {
                            if sub == 1 && size > 0x800000 {
                                hlog!("  kernel? {e}");
                            }
                        }
                        if let Ok(len) = r {
                            KERNEL = b;
                            KSTUB = len as u64;
                            hlog!("  = the kernel: KiSynchronousException stub installed ({len} bytes), record at {:#x}", b + crate::kpatch::REC_RVA as u64);
                        }
                    }
                    IMAGES += 1;
                    INSNS += n as u64;
                    LAST.copy_within(0..3, 1);
                    LAST[0] = (b, ((sub as u64) << 48) | ((size as u64) << 16) | n as u64);
                }
            }
        }
    }
}
unsafe extern "efiapi" fn ebs(h: Handle, key: usize) -> Status {
    rec(6, key as u64, 0);
    hlog!("ExitBootServices (map key {key:#x}): scanning everything");
    unsafe {
        QUIET = true;
        scan(false);
        QUIET = false;
    }
    unsafe { EBS.unwrap()(h, key) }
}
unsafe extern "efiapi" fn st(us: usize) -> Status {
    rec(7, us as u64, 0);
    unsafe { ST.unwrap()(us) }
}
unsafe extern "efiapi" fn wfe(n: usize, e: *mut *mut c_void, idx: *mut usize) -> Status {
    rec(8, n as u64, 0);
    unsafe { WFE.unwrap()(n, e, idx) }
}
unsafe extern "efiapi" fn sw(t: usize, code: u64, n: usize, d: *const u16) -> Status {
    rec(10, t as u64, 0);
    unsafe { SW.unwrap()(t, code, n, d) }
}
unsafe extern "efiapi" fn ce(e: *mut c_void) -> Status {
    let r = unsafe { CE.unwrap()(e) };
    // CheckEvent is polled in loops: only count it, one entry per 1000 calls
    static mut N_CE: u64 = 0;
    unsafe {
        N_CE += 1;
        if N_CE % 1000 == 1 {
            rec(13, N_CE, r.0 as u64);
        }
    }
    r
}

/// Replace the boot services in the system table (and fix its CRC).
pub fn install() {
    unsafe {
        let systab = uefi::table::system_table_raw().unwrap().as_ptr();
        let bs = (*systab).boot_services;
        BS = bs;
        macro_rules! hook {
            ($slot:ident, $orig:ident, $f:ident) => {
                $orig = Some(core::mem::transmute((*bs).$slot));
                (*bs).$slot = core::mem::transmute($f as *const ());
            };
        }
        hook!(handle_protocol, HP, hp);
        hook!(locate_protocol, LP, lp);
        hook!(open_protocol, OP, op);
        hook!(locate_handle_buffer, LHB, lhb);
        hook!(allocate_pages, AP, ap);
        hook!(exit_boot_services, EBS, ebs);
        hook!(stall, ST, st);
        hook!(wait_for_event, WFE, wfe);
        hook!(set_watchdog_timer, SW, sw);
        hook!(check_event, CE, ce);
        (*bs).header.crc = 0;
        let mut crc = 0u32;
        let _ = ((*bs).calculate_crc32)(bs as *const c_void, (*bs).header.size as usize, &mut crc);
        (*bs).header.crc = crc;
    }
}

/// Write the ring (oldest first) to \hmtrace.txt.
pub unsafe fn flush() {
    unsafe {
        let mut s = String::new();
        let (ticks, count) = (crate::shim::TICKS, COUNT);
        let _ = writeln!(s, "ticks {ticks} calls {count}");
        let start = COUNT.saturating_sub(N as u64);
        for i in start..COUNT {
            let (id, a, b) = (*ptr::addr_of!(RING))[(i as usize) % N];
            let name = NAMES.get(id as usize).copied().unwrap_or("?");
            let _ = writeln!(s, "{i:6} {name:<18} {a:#x} {b:#x}");
        }
        QUIET = true;
        crate::save_as(uefi::cstr16!("hmtrace.txt"), &s);
        QUIET = false;
    }
}
