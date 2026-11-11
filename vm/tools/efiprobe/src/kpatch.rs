//! Make the Windows kernel emulate user-mode PMU register accesses.
//!
//! User code reads PMCCNTR_EL0 (smss.exe seeds a random number with it, ntdll has 82 sites);
//! real hardware lets EL0 do that, VZ gives the guest no PMU, so it is an undefined instruction
//! and the critical process dies (bugcheck 0xC000021A). Those images come from disk after
//! ExitBootServices, so they can't be patched; the kernel's synchronous exception path can.
//!
//! Kernel-mode drivers loaded from disk later do it too (bugcheck 0x1E, C000001D, the
//! instruction d53b9d08 as parameter 4), so EL1 is handled the same way.
//! KiSynchronousException(x0 = KTRAP_FRAME, x1 = ...) gets a branch to `stub` as its first
//! instruction (it was `pacibsp`). The stub, for an EL0/EL1 exception with EC 0 (unknown):
//! reads the instruction (LDTR: unprivileged, so PAN doesn't stop it); if it is an MRS/MSR of a
//! PMU register, MRS PMCCNTR_EL0 -> CNTVCT_EL0, other reads -> 0, writes ignored; PC += 4; returns.
//! Anything else: `pacibsp` and on into the original function.
//! KTRAP_FRAME (this build): SPSR 0x90, ESR 0x94, X0..X18 0xa0.., X30 0x138, X29 0x140, PC 0x148.
//! X19..X28 are not in the trap frame; the caller saved them at x1 (x1 + (n - 19) * 8), which is
//! where KiEmulateReadRegister reads them and where they are restored from.
//! The stub goes into the zero tail of the kernel's .text (non-paged, executable).

core::arch::global_asm!(
    ".balign 8",
    ".global hm_kstub_start",
    "hm_kstub_start:",
    "  ldr  w16, [x0, #0x90]",
    "  and  w16, w16, #0xc", // 0 = from EL0, 4 = from EL1
    "  cmp  w16, #4",
    "  b.hi 90f",
    "  ldr  w9, [x0, #0x94]",
    "  lsr  w9, w9, #26",
    "  ldr  x10, [x0, #0x148]",
    "  cbz  w9, 4f",
    // other ECs we want to see (trapped MSR/MRS, illegal state, PC/SP alignment): record only
    "  cmp  w9, #0x18",
    "  b.eq 3f",
    "  cmp  w9, #0x0e",
    "  b.eq 3f",
    "  cmp  w9, #0x22",
    "  b.eq 3f",
    "  cmp  w9, #0x26",
    "  b.ne 90f",
    "3:",
    "  ldr  w11, [x0, #0x94]",
    "  b    85f",
    "4:",
    "  cbnz w16, 5f",
    "  ldtr w11, [x10]", // user code: unprivileged load (PAN)
    "  b    6f",
    "5:",
    "  ldr  w11, [x10]", // kernel code (drivers loaded after ExitBootServices)
    "6:",
    // EL0 MRS Xt, CurrentEL (UNDEFINED at EL0; _InterlockedDetectSupport probes with it and
    // skips it in an exception handler): Xt = 0 (= EL0), PC += 4
    "  cbnz w16, 7f",
    "  and  w12, w11, #0xffffffe0",
    "  movz w13, #0x4240",
    "  movk w13, #0xd538, lsl #16",
    "  cmp  w12, w13",
    "  b.ne 7f",
    "  and  w15, w11, #0x1f",
    "  mov  x14, #0",
    "  b    30f",
    "7:",
    // MRS/MSR (register), op0 = 3: (insn & 0xffd80000) == 0xd5180000
    "  movz w12, #0xffd8, lsl #16",
    "  and  w12, w11, w12",
    "  movz w13, #0xd518, lsl #16",
    "  cmp  w12, w13",
    "  b.ne 85f",
    "  ubfx w12, w11, #16, #3", // op1
    "  ubfx w13, w11, #12, #4", // CRn
    "  ubfx w14, w11, #8, #4",  // CRm
    "  cmp  w13, #9",
    "  b.ne 10f",
    "  cmp  w12, #3",
    "  b.eq 20f",
    "  cbz  w12, 20f",
    "  b    85f",
    "10:",
    "  cmp  w13, #14",
    "  b.ne 85f",
    "  cmp  w12, #3",
    "  b.ne 85f",
    "  cmp  w14, #8",
    "  b.lo 85f",
    "20:", // a PMU register
    "  tbz  w11, #21, 80f", // MSR: ignore
    "  and  w15, w11, #0x1f",
    "  mov  x14, #0",
    "  and  w12, w11, #0xffffffe0",
    "  movz w13, #0x9d00",
    "  movk w13, #0xd53b, lsl #16",
    "  cmp  w12, w13",
    "  b.ne 30f",
    "  mrs  x14, cntvct_el0",
    "30:",
    "  cmp  w15, #31",
    "  b.eq 80f",
    "  cmp  w15, #18",
    "  b.hi 40f",
    "  add  x12, x0, #0xa0",
    "  str  x14, [x12, w15, uxtw #3]",
    "  b    80f",
    "40:",
    // X19..X28: saved by the caller at x1 (KiEmulateReadRegister reads them there too)
    "  cmp  w15, #28",
    "  b.hi 45f",
    "  sub  w12, w15, #19",
    "  str  x14, [x1, w12, uxtw #3]",
    "  b    80f",
    "45:",
    "  cmp  w15, #29",
    "  b.ne 50f",
    "  str  x14, [x0, #0x140]",
    "  b    80f",
    "50:",
    "  cmp  w15, #30",
    "  b.ne 85f",
    "  str  x14, [x0, #0x138]",
    "80:", // emulated: PC += 4, count it
    "  add  x10, x10, #4",
    "  str  x10, [x0, #0x148]",
    "  adr  x12, hm_kstub_lit",
    "  ldr  x13, [x12]",
    "  add  x12, x12, x13",
    "  ldr  x13, [x12, #8]",
    "  add  x13, x13, #1",
    "  str  x13, [x12, #8]",
    "  ret",
    "85:", // EC 0 but not ours: record it (ring of 8: ELR, insn | mode << 32, ESR)
    "  adr  x12, hm_kstub_lit",
    "  ldr  x13, [x12]",
    "  add  x12, x12, x13",
    "  ldr  x13, [x12, #16]",
    "  add  x14, x13, #1",
    "  str  x14, [x12, #16]",
    "  mrs  x14, ttbr1_el1",
    "  str  x14, [x12, #24]",
    "  and  x13, x13, #7",
    "  mov  x14, #24",
    "  madd x13, x13, x14, x12",
    "  str  x10, [x13, #32]",
    "  orr  x14, x11, x16, lsl #32",
    "  str  x14, [x13, #40]",
    "  ldr  w14, [x0, #0x94]",
    "  str  x14, [x13, #48]",
    "90:",
    "  pacibsp",
    ".global hm_kstub_back",
    "hm_kstub_back:",
    "  b    .", // patched: -> KiSynchronousException + 4
    ".balign 8",
    ".global hm_kstub_lit",
    "hm_kstub_lit:",
    "  .quad 0", // patched: the record's address minus this one's
    ".global hm_kstub_end",
    "hm_kstub_end:",
);

unsafe extern "C" {
    static hm_kstub_start: u32;
    static hm_kstub_back: u32;
    static hm_kstub_lit: u32;
    static hm_kstub_end: u32;
}

/// this build of ntoskrnl.exe (boot.wim 26300, ntkrnlmp.pdb 7A9F162A...)
const KISYNC_RVA: usize = 0x2b69a0;
const KISYNC_PROLOGUE: [u32; 4] = [0xd503_237f, 0xa9bb_53f3, 0xa901_5bf5, 0xa902_63f7];
/// .text ends at 0x675974; the rest of its last page is zero
const CAVE_RVA: usize = 0x675980;
const CAVE_END: usize = 0x676000;

const REC_LEN: usize = 32 + 8 * 24;
/// where the record went (RVA in the kernel), for the log
pub static mut REC_RVA: usize = 0;

/// RVA of `len` zero bytes at the end of the kernel's .data section, inside its last page.
unsafe fn data_tail(base: *mut u8, len: usize) -> Option<usize> {
    unsafe {
        let pe = (base.add(0x3c) as *const u32).read_unaligned() as usize;
        let nsec = (base.add(pe + 6) as *const u16).read_unaligned() as usize;
        let opt = (base.add(pe + 20) as *const u16).read_unaligned() as usize;
        for i in 0..nsec {
            let sh = base.add(pe + 24 + opt + i * 40);
            if core::slice::from_raw_parts(sh, 8) != b".data\0\0\0" {
                continue;
            }
            let vsize = (sh.add(8) as *const u32).read_unaligned() as usize;
            let va = (sh.add(12) as *const u32).read_unaligned() as usize;
            let at = (va + vsize + 63) & !63;
            let page_end = (va + vsize + 0xfff) & !0xfff;
            if at + len > page_end || (0..len).any(|k| base.add(at + k).read() != 0) {
                return None;
            }
            return Some(at);
        }
        None
    }
}

fn b(from: usize, to: usize) -> u32 {
    0x1400_0000 | (((to as isize - from as isize) >> 2) as u32 & 0x03ff_ffff)
}

/// If `base` is that kernel: install the stub. Ok(stub length) / Err(reason).
pub unsafe fn patch_kernel(base: *mut u8) -> Result<usize, &'static str> {
    unsafe {
        let f = base.add(KISYNC_RVA) as *mut u32;
        for (i, w) in KISYNC_PROLOGUE.iter().enumerate() {
            if f.add(i).read() != *w {
                return Err("not this kernel");
            }
        }
        let start = &hm_kstub_start as *const u32 as *const u8;
        let back = &hm_kstub_back as *const u32 as usize - start as usize;
        let len = &hm_kstub_end as *const u32 as usize - start as usize;
        if CAVE_RVA + len > CAVE_END {
            return Err("stub too big");
        }
        let cave = base.add(CAVE_RVA);
        if (0..len).any(|i| cave.add(i).read() != 0) {
            return Err("cave not empty");
        }
        // the record: the zero tail of .data (writable, non-paged), host-readable at
        // kernel physical base + RVA: u64 magic "HMKSTUB1", emulated, unhandled, TTBR1, ring
        let lit = &hm_kstub_lit as *const u32 as usize - start as usize;
        let rec = data_tail(base, REC_LEN).ok_or("no room in .data")?;
        crate::exc::writable(cave, len, || {
            core::ptr::copy_nonoverlapping(start, cave, len);
            (cave.add(back) as *mut u32).write(b(CAVE_RVA + back, KISYNC_RVA + 4));
            (cave.add(lit) as *mut u64).write((rec as i64 - (CAVE_RVA + lit) as i64) as u64);
        });
        crate::exc::writable(base.add(rec), REC_LEN, || {
            (base.add(rec) as *mut u64).write(u64::from_le_bytes(*b"HMKSTUB1"));
        });
        REC_RVA = rec;
        crate::exc::writable(f as *mut u8, 4, || f.write(b(KISYNC_RVA, CAVE_RVA)));
        for off in (0..len).step_by(4) {
            core::arch::asm!("dc cvau, {0}", "ic ivau, {0}", in(reg) cave.add(off));
        }
        core::arch::asm!("dc cvau, {0}", "ic ivau, {0}", "dsb ish", "isb", in(reg) f);
        Ok(len)
    }
}
