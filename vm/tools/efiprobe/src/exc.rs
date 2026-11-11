//! Catch a synchronous exception in the next image and show where it happened.
//!
//! Registers a handler through EFI_CPU_ARCH_PROTOCOL.RegisterInterruptHandler (ArmPkg's CpuDxe
//! passes AArch64 exception types through). The handler can't trust anything, so it draws the
//! registers as bit strips straight into the shim's framebuffer and Blts it once:
//! rows of 64 squares (white = 1, MSB first): magic, ELR, ESR, FAR, LR, SP, X0, X1.
//! `tools/efiprobe/decode.py` turns a screenshot back into numbers.

use core::ffi::c_void;
use core::ptr;

use uefi::{Guid, Status, guid};

const CPU_ARCH: Guid = guid!("26baccb1-6f42-11d4-bce7-0080c73c8881");

type Handler = unsafe extern "efiapi" fn(isize, *mut u64);

#[repr(C)]
struct CpuArch {
    flush_data_cache: usize,
    enable_interrupt: usize,
    disable_interrupt: usize,
    get_interrupt_state: usize,
    init: usize,
    register_interrupt_handler: unsafe extern "efiapi" fn(*mut CpuArch, isize, Option<Handler>) -> Status,
}

pub static mut HITS: u64 = 0;

/// EFI_SYSTEM_CONTEXT_AARCH64: X0..X28, FP, LR, SP, V0..V31 (16 bytes each), ELR, SPSR, FPSR, ESR, FAR
const LR: usize = 30;
const SP: usize = 31;
const ELR: usize = 32 + 64;
const ESR: usize = ELR + 3;
const FAR: usize = ELR + 4;

pub static mut EMULATED: u64 = 0;

/// PMU (and statistical profiling) system registers, op0 = 3:
/// op1=3 CRn=9 (PMCR_EL0 .. PMCCNTR_EL0, PMUSERENR, PMOVS..), op1=3 CRn=14 CRm 8..15
/// (PMEVCNTRn / PMEVTYPERn / PMCCFILTR; CRm 0..3 there is the generic timer: not ours),
/// op1=0 CRn=9 (PMINTENSET/CLR_EL1, SPE).
fn is_pmu(op1: u32, crn: u32, crm: u32) -> bool {
    (op1 == 3 && crn == 9) || (op1 == 3 && crn == 14 && crm >= 8) || (op1 == 0 && crn == 9)
}


/// Emulate an access to a PMU register (op0=3, op1=3, CRn=9 or 14): VZ gives the guest no PMU,
/// so these are UNDEFINED, but the Windows boot manager reads PMCCNTR_EL0 without checking.
/// MRS PMCCNTR_EL0 -> the generic timer's count; other PMU reads -> 0; writes ignored.
unsafe fn emulate_pmu(ctx: *mut u64, insn: u32) -> bool {
    // MRS/MSR (register): 1101 0101 00 L 1 op0[0] op1 CRn CRm op2 Rt ; op0 = 3 -> bit 19 set
    if insn & 0xFFD8_0000 != 0xD518_0000 {
        return false;
    }
    let read = insn & (1 << 21) != 0;
    let op1 = (insn >> 16) & 7;
    let crn = (insn >> 12) & 0xF;
    let crm = (insn >> 8) & 0xF;
    let op2 = (insn >> 5) & 7;
    let rt = (insn & 0x1F) as usize;
    if !is_pmu(op1, crn, crm) {
        return false;
    }
    if read && rt != 31 {
        let v = if crn == 9 && crm == 13 && op2 == 0 {
            let c: u64;
            unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) c) };
            c
        } else {
            0
        };
        unsafe { *ctx.add(rt) = v };
    }
    unsafe {
        *ctx.add(ELR) += 4;
        EMULATED += 1;
    }
    true
}

unsafe extern "efiapi" fn on_sync(_ty: isize, ctx: *mut u64) {
    unsafe {
        let esr = *ctx.add(ESR);
        let elr = *ctx.add(ELR);
        // EC 0 (unknown) or 0x18 (trapped MSR/MRS): look at the instruction
        let ec = esr >> 26;
        if ec == 0 || ec == 0x18 {
            let insn = core::ptr::read_volatile(elr as *const u32);
            if emulate_pmu(ctx, insn) {
                if EMULATED.is_power_of_two() {
                    crate::shim::draw_bits(&[0x0E40_0000_0000_0001u64, EMULATED, elr, *ctx.add(ELR), esr, insn as u64, 0, 0]);
                    crate::shim::present_rows(90, 280);
                }
                return; // the firmware's exception code restores the context and erets
            }
        }
        HITS += 1;
        let r = |i: usize| *ctx.add(i);
        let (vbar, ttbr0, ttbr1, sctlr, tcr): (u64, u64, u64, u64, u64);
        core::arch::asm!("mrs {}, vbar_el1", out(reg) vbar);
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0);
        core::arch::asm!("mrs {}, ttbr1_el1", out(reg) ttbr1);
        core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr);
        core::arch::asm!("mrs {}, tcr_el1", out(reg) tcr);
        hlog!("\n*** synchronous exception");
        hlog!("ELR {:#x} ESR {:#x} (EC {:#x}) FAR {:#x} SPSR {:#x}", r(ELR), r(ESR), r(ESR) >> 26, r(FAR), r(ELR + 1));
        hlog!("VBAR {vbar:#x} TTBR0 {ttbr0:#x} TTBR1 {ttbr1:#x} SCTLR {sctlr:#x} TCR {tcr:#x}");
        for i in (0..32).step_by(4) {
            hlog!("X{:<2} {:#018x} {:#018x} {:#018x} {:#018x}", i, r(i), r(i + 1), r(i + 2), r(i + 3));
        }
        let e = r(ELR) & !3;
        for k in 0..16u64 {
            let a = e - 32 + k * 4;
            hlog!("  {:#x}: {:08x}{}", a, core::ptr::read_volatile(a as *const u32), if a == e { "  <--" } else { "" });
        }
        let insn = core::ptr::read_volatile(elr as *const u32) as u64;
        let vals = [0xA5A5_5A5A_DEAD_BEEFu64, r(ELR), r(ESR), r(FAR), r(LR), r(SP), insn, EMULATED];
        crate::shim::draw_bits(&vals);
        crate::shim::present();
        loop {
            core::arch::asm!("wfe");
        }
    }
}

pub fn install() -> Result<(), Status> {
    unsafe {
        let mut p: *mut c_void = ptr::null_mut();
        let st = uefi::table::system_table_raw().unwrap().as_ptr();
        let bs = (*st).boot_services;
        let s = ((*bs).locate_protocol)(&CPU_ARCH as *const Guid as *const _, ptr::null_mut(), &mut p);
        if s != Status::SUCCESS {
            return Err(s);
        }
        let cpu = p as *mut CpuArch;
        // 0 = EXCEPT_AARCH64_SYNCHRONOUS_EXCEPTIONS; drop whatever is there first
        let _ = ((*cpu).register_interrupt_handler)(cpu, 0, None);
        let s = ((*cpu).register_interrupt_handler)(cpu, 0, Some(on_sync));
        if s != Status::SUCCESS {
            return Err(s);
        }
        Ok(())
    }
}

/// Rewrite PMU accesses in a loaded image's code (before it runs):
/// MRS Xt, PMCCNTR_EL0 -> MRS Xt, CNTVCT_EL0; other PMU reads -> MOVZ Xt, #0; PMU writes -> NOP.
/// Returns how many instructions were changed.
pub unsafe fn patch_pmu(base: *mut u8, size: usize, keep_vbar: bool) -> usize {
    let mut n = 0;
    let words = base as *mut u32;
    for i in 0..size / 4 {
        unsafe {
            let p = words.add(i);
            let insn = p.read_volatile();
            // diagnostic: keep the firmware's exception vectors (MSR VBAR_EL1, Xt -> NOP)
            if keep_vbar && insn & 0xFFFF_FFE0 == 0xD518_C000 {
                p.write_volatile(0xD503_201F);
                core::arch::asm!("dc cvau, {0}", "ic ivau, {0}", in(reg) p);
                n += 1;
                continue;
            }
            if insn & 0xFFD8_0000 != 0xD518_0000 {
                continue;
            }
            let read = insn & (1 << 21) != 0;
            let op1 = (insn >> 16) & 7;
            let crn = (insn >> 12) & 0xF;
            let crm = (insn >> 8) & 0xF;
            let op2 = (insn >> 5) & 7;
            let rt = insn & 0x1F;
            if !is_pmu(op1, crn, crm) {
                continue;
            }
            let new = if !read {
                0xD503_201F // NOP
            } else if crn == 9 && crm == 13 && op2 == 0 {
                0xD53B_E040 | rt // MRS Xt, CNTVCT_EL0
            } else {
                0xD280_0000 | rt // MOVZ Xt, #0
            };
            p.write_volatile(new);
            core::arch::asm!("dc cvau, {0}", "ic ivau, {0}", in(reg) p);
            n += 1;
        }
    }
    unsafe { core::arch::asm!("dsb ish", "isb") };
    n
}

/// Patch every executable section of the PE image at `b` (loaded: sections at their RVAs).
/// keep_vbar: also turn its MSR VBAR_EL1 into NOPs (EFI applications only, never the kernel).
/// Returns (subsystem, image size, instructions changed), or None if `b` is not a PE image.
pub unsafe fn patch_image(b: *mut u8, keep_vbar_for_apps: bool) -> Option<(u16, usize, usize)> {
    unsafe {
        if b.read() != b'M' || b.add(1).read() != b'Z' {
            return None;
        }
        let pe = (b.add(0x3c) as *const u32).read_unaligned() as usize;
        if pe < 0x40 || pe > 0x800 || (b.add(pe) as *const u32).read_unaligned() != 0x4550 {
            return None;
        }
        // machine: ARM64 only
        if (b.add(pe + 4) as *const u16).read_unaligned() != 0xAA64 {
            return None;
        }
        let nsec = (b.add(pe + 6) as *const u16).read_unaligned() as usize;
        let opt = (b.add(pe + 20) as *const u16).read_unaligned() as usize;
        let size_of_image = (b.add(pe + 24 + 56) as *const u32).read_unaligned() as usize;
        let subsystem = (b.add(pe + 24 + 68) as *const u16).read_unaligned();
        let keep = keep_vbar_for_apps && subsystem == 10;
        let sec0 = pe + 24 + opt;
        // A raw file buffer (what the boot manager reads, checksums and hashes) has the first
        // section at its file offset; a loaded image has zeros there (header padding, file
        // alignment 0x200 < section alignment 0x1000). Patch loaded images only.
        if nsec > 0 {
            let raw = (b.add(sec0 + 20) as *const u32).read_unaligned() as usize;
            let va = (b.add(sec0 + 12) as *const u32).read_unaligned() as usize;
            if raw != 0 && raw < va && (0..64).any(|i| b.add(raw + i).read() != 0) {
                return None;
            }
        }
        let mut n = 0;
        for i in 0..nsec.min(96) {
            let sh = b.add(sec0 + i * 40);
            let vsize = (sh.add(8) as *const u32).read_unaligned() as usize;
            let va = (sh.add(12) as *const u32).read_unaligned() as usize;
            let ch = (sh.add(36) as *const u32).read_unaligned();
            if ch & 0x2000_0000 != 0 && va + vsize <= size_of_image {
                n += writable(b.add(va), vsize, || patch_pmu(b.add(va), vsize, keep));
            }
        }
        Some((subsystem, size_of_image, n))
    }
}

/// EFI_MEMORY_ATTRIBUTE_PROTOCOL: the boot manager asks the firmware to map loaded code
/// read-only through it, so patching has to lift EFI_MEMORY_RO first (and put it back).
const MEM_ATTR: Guid = guid!("f4560cf6-40ec-4b4a-a192-bf1d57d0b189");
const EFI_MEMORY_RO: u64 = 0x2_0000;

#[repr(C)]
struct MemAttr {
    get: unsafe extern "efiapi" fn(*mut MemAttr, u64, u64, *mut u64) -> Status,
    set: unsafe extern "efiapi" fn(*mut MemAttr, u64, u64, u64) -> Status,
    clear: unsafe extern "efiapi" fn(*mut MemAttr, u64, u64, u64) -> Status,
}

static mut MA: *mut MemAttr = core::ptr::null_mut();

/// Run `f` with [addr, addr+len) writable (page-rounded), restoring read-only afterwards.
pub unsafe fn writable<R>(addr: *mut u8, len: usize, f: impl FnOnce() -> R) -> R {
    unsafe {
        if MA.is_null() {
            let mut p: *mut c_void = ptr::null_mut();
            let st = uefi::table::system_table_raw().unwrap().as_ptr();
            let bs = (*st).boot_services;
            if ((*bs).locate_protocol)(&MEM_ATTR as *const Guid as *const _, ptr::null_mut(), &mut p) == Status::SUCCESS {
                MA = p as *mut MemAttr;
            }
        }
        if MA.is_null() {
            return f();
        }
        let start = addr as u64 & !0xfff;
        let end = (addr as u64 + len as u64 + 0xfff) & !0xfff;
        let mut attrs = 0u64;
        let got = ((*MA).get)(MA, start, end - start, &mut attrs) == Status::SUCCESS;
        let ro = got && attrs & EFI_MEMORY_RO != 0;
        if ro {
            let _ = ((*MA).clear)(MA, start, end - start, EFI_MEMORY_RO);
        }
        let r = f();
        if ro {
            let _ = ((*MA).set)(MA, start, end - start, EFI_MEMORY_RO);
        }
        r
    }
}
