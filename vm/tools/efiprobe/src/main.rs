//! efiprobe: what does VZ's EFI give a Windows boot manager?
//!
//! Put it on the boot stick as \EFI\BOOT\BOOTAA64.EFI (the original renamed to WINBOOT.EFI).
//! It writes \hmprobe.txt on the same volume: firmware, config tables, ACPI table signatures,
//! GOP modes (and paints the screen green if there is one), how many handles carry the usual
//! protocols. Then it starts \EFI\BOOT\WINBOOT.EFI and appends what that returned.

#![no_main]
// single-threaded firmware code: the statics are plain globals
#![allow(static_mut_refs, dangerous_implicit_autorefs)]
#![no_std]

extern crate alloc;

#[macro_use]
mod hmlog;
mod exc;
mod kpatch;

/// diagnostic: NOP the boot apps' MSR VBAR_EL1 so their exceptions reach our handler (exc.rs).
/// Off: Windows handles some exceptions itself on purpose.
pub const KEEP_VBAR: bool = false;
mod shim;
mod trace;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use uefi::boot::{self, LoadImageSource, OpenProtocolAttributes, OpenProtocolParams, SearchType};
use uefi::prelude::*;
use uefi::proto::console::gop::GraphicsOutput;
use uefi::proto::console::serial::Serial;
use uefi::proto::console::text::Output;
use uefi::proto::device_path::DevicePath;
use uefi::proto::device_path::build::{self, DevicePathBuilder};
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::block::BlockIO;
use uefi::proto::media::file::{File, FileAttribute, FileMode};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::{CString16, Identify, cstr16};

fn save(log: &str) {
    // the part not sent yet also goes to the host-readable log (hmlog.rs)
    static mut SENT: usize = 0;
    unsafe {
        if log.len() > SENT {
            hmlog::write(&log[SENT..]);
            SENT = log.len();
        }
    }
    save_as(cstr16!("hmprobe.txt"), log);
}

pub fn save_as(name: &uefi::CStr16, text: &str) {
    let Ok(mut fs) = boot::get_image_file_system(boot::image_handle()) else { return };
    let Ok(mut root) = fs.open_volume() else { return };
    if let Ok(old) = root.open(name, FileMode::ReadWrite, FileAttribute::empty()) {
        let _ = old.delete();
    }
    if let Ok(f) = root.open(name, FileMode::CreateReadWrite, FileAttribute::empty()) {
        if let Some(mut f) = f.into_regular_file() {
            let _ = f.write(text.as_bytes());
            let _ = f.flush();
        }
    }
}

fn count<P: Identify + uefi::proto::ProtocolPointer + ?Sized>(log: &mut String, name: &str) {
    let n = boot::locate_handle_buffer(SearchType::ByProtocol(&P::GUID)).map(|h| h.len()).unwrap_or(0);
    let _ = writeln!(log, "handles with {name}: {n}");
}

fn acpi(log: &mut String, rsdp: *const u8) {
    unsafe {
        let sig = core::slice::from_raw_parts(rsdp, 8);
        let rev = *rsdp.add(15);
        let _ = writeln!(log, "RSDP {:?} rev {rev}", core::str::from_utf8(sig).unwrap_or("?"));
        if rev < 2 {
            return;
        }
        let xsdt = (rsdp.add(24) as *const u64).read_unaligned() as *const u8;
        let len = (xsdt.add(4) as *const u32).read_unaligned() as usize;
        let n = (len - 36) / 8;
        let mut names = String::new();
        for i in 0..n {
            let t = (xsdt.add(36 + i * 8) as *const u64).read_unaligned() as *const u8;
            let s = core::slice::from_raw_parts(t, 4);
            let _ = write!(names, "{} ", core::str::from_utf8(s).unwrap_or("?"));
        }
        let _ = writeln!(log, "XSDT {n} tables: {names}");
    }
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();
    hmlog::init();
    let mut log = String::new();
    let _ = writeln!(log, "efiprobe");
    save(&log);
    let _ = writeln!(
        log,
        "firmware: {} rev {:#x}, UEFI {:?}",
        uefi::system::firmware_vendor(),
        uefi::system::firmware_revision(),
        uefi::system::uefi_revision()
    );

    let tables: Vec<(uefi::Guid, usize)> = uefi::system::with_config_table(|t| t.iter().map(|e| (e.guid, e.address as usize)).collect());
    for (guid, addr) in tables {
        let _ = writeln!(log, "config table {guid}");
        if guid == uefi::table::cfg::ACPI2_GUID {
            acpi(&mut log, addr as *const u8);
        }
    }

    save(&log);
    count::<GraphicsOutput>(&mut log, "GraphicsOutput");
    count::<Output>(&mut log, "SimpleTextOutput");
    count::<Serial>(&mut log, "SerialIo");
    count::<BlockIO>(&mut log, "BlockIO");
    count::<SimpleFileSystem>(&mut log, "SimpleFileSystem");

    save(&log);
    // GOP: modes, framebuffer, paint it green
    if let Ok(handles) = boot::locate_handle_buffer(SearchType::ByProtocol(&GraphicsOutput::GUID)) {
        for &h in handles.iter() {
            let _ = writeln!(log, "GOP handle {:?}: opening", h);
            if let Ok(dp) = unsafe { boot::open_protocol::<DevicePath>(OpenProtocolParams { handle: h, agent: boot::image_handle(), controller: None }, OpenProtocolAttributes::GetProtocol) } {
                let _ = writeln!(log, "  path {}", dp.to_string(uefi::proto::device_path::text::DisplayOnly(true), uefi::proto::device_path::text::AllowShortcuts(true)).map(|s| String::from(&*s)).unwrap_or_default());
            }
            save(&log);
            let gop = unsafe {
                boot::open_protocol::<GraphicsOutput>(
                    OpenProtocolParams { handle: h, agent: boot::image_handle(), controller: None },
                    OpenProtocolAttributes::GetProtocol,
                )
            };
            match gop {
                Ok(mut g) => {
                    let _ = writeln!(log, "  opened");
                    save(&log);
                    let mi = g.current_mode_info();
                    let _ = writeln!(log, "  mode {:?} {:?} stride {}", mi.resolution(), mi.pixel_format(), mi.stride());
                    save(&log);
                    if mi.pixel_format() != uefi::proto::console::gop::PixelFormat::BltOnly {
                        let mut fb = g.frame_buffer();
                        let _ = writeln!(log, "  framebuffer {:p} size {:#x}", fb.as_mut_ptr(), fb.size());
                        save(&log);
                        unsafe { fb.write_value(0, 0x0000_c000u32) };
                        let _ = writeln!(log, "  wrote 1 pixel");
                        save(&log);
                    }
                    // Blt (the firmware's own path) instead of raw writes
                    let px = uefi::proto::console::gop::BltPixel::new(0, 0xc0, 0);
                    let r = g.blt(uefi::proto::console::gop::BltOp::VideoFill { color: px, dest: (0, 0), dims: mi.resolution() });
                    let _ = writeln!(log, "  blt fill: {:?}", r.map_err(|e| e.status()));
                    save(&log);
                }
                Err(e) => {
                    let _ = writeln!(log, "GOP open: {:?}", e.status());
                }
            }
        }
    }
    save(&log);
    uefi::println!("efiprobe: hello from the console");

    // the linear framebuffer shim; paint it blue so a screenshot shows it works
    match shim::install() {
        Ok((n, fb, w, h)) => {
            let _ = writeln!(log, "shim: {n} GOPs -> linear framebuffer {fb:#x} {w}x{h}");
            unsafe { core::slice::from_raw_parts_mut(fb as *mut u32, w * h).fill(0x0000_00c0) };
            boot::stall(1_500_000);
            let _ = writeln!(log, "shim: {} ticks in 1.5 s", unsafe { shim::TICKS });
        }
        Err(e) => {
            let _ = writeln!(log, "shim: {e:?}");
        }
    }
    if let Ok(h) = boot::get_handle_for_protocol::<GraphicsOutput>() {
        if let Ok(g) = unsafe { boot::open_protocol::<GraphicsOutput>(OpenProtocolParams { handle: h, agent: boot::image_handle(), controller: None }, OpenProtocolAttributes::GetProtocol) } {
            let mi = g.current_mode_info();
            let _ = writeln!(log, "GOP now: {:?} {:?} stride {}", mi.resolution(), mi.pixel_format(), mi.stride());
        }
    }
    save(&log);

    // chainload the Windows boot manager from the same device
    let mut status = String::new();
    let li = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle()).unwrap();
    let mut dev = li.device().unwrap();
    drop(li);
    // An installed Windows (bootmgfw.efi on another volume, the disk's ESP) goes first; the
    // stick's WinPE only when there is none. The stick stays attached: setup's later passes take
    // drivers from it (\hypermac.cmd).
    let mut installed: Option<&str> = None;
    if let Ok(hs) = boot::locate_handle_buffer(SearchType::ByProtocol(&SimpleFileSystem::GUID)) {
        for &h in hs.iter() {
            if h == dev {
                continue;
            }
            let Ok(mut fs) = (unsafe { boot::open_protocol::<SimpleFileSystem>(OpenProtocolParams { handle: h, agent: boot::image_handle(), controller: None }, OpenProtocolAttributes::GetProtocol) }) else { continue };
            let Ok(mut root) = fs.open_volume() else { continue };
            // winboot.efi = the real boot manager where efiprobe took bootmgfw.efi's place
            for (f, n) in [(cstr16!("\\EFI\\Microsoft\\Boot\\winboot.efi"), "winboot.efi"), (cstr16!("\\EFI\\Microsoft\\Boot\\bootmgfw.efi"), "bootmgfw.efi")] {
                if root.open(f, FileMode::Read, FileAttribute::empty()).is_ok() {
                    dev = h;
                    installed = Some(n);
                    let _ = writeln!(log, "installed Windows found: booting its {n}");
                    break;
                }
            }
            if installed.is_some() {
                break;
            }
        }
    }
    // GetProtocol, not exclusive: an exclusive open disconnects the stick's file system driver
    let devpath = unsafe { boot::open_protocol::<DevicePath>(OpenProtocolParams { handle: dev, agent: boot::image_handle(), controller: None }, OpenProtocolAttributes::GetProtocol) };
    match devpath {
        Ok(dp) => {
            let mut buf = Vec::new();
            let mut b = DevicePathBuilder::with_vec(&mut buf);
            for node in dp.node_iter() {
                b = b.push(&node).unwrap();
            }
            // WINBOOT.EFI next to this program: \EFI\BOOT\ on the stick, or
            // \EFI\Microsoft\Boot\ when installed in place of bootmgfw.efi on the disk
            let mut dir = String::from("\\EFI\\BOOT\\");
            if let Ok(me) = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle()) {
                if let Some(fp) = me.file_path() {
                    if let Ok(t) = fp.to_string(uefi::proto::device_path::text::DisplayOnly(true), uefi::proto::device_path::text::AllowShortcuts(false)) {
                        let t = String::from(&*t);
                        let t = String::from(t.rsplit('/').next().unwrap_or(&t));
                        if let Some(i) = t.rfind('\\') {
                            dir = String::from(&t[..=i]);
                        }
                    }
                }
            }
            let _ = writeln!(log, "my directory: {dir}");
            let file = match installed {
                Some(n) => alloc::format!("\\EFI\\Microsoft\\Boot\\{n}"),
                None => alloc::format!("{dir}WINBOOT.EFI"),
            };
            let name = CString16::try_from(file.as_str()).unwrap();
            b = b.push(&build::media::FilePath { path_name: &name }).unwrap();
            let full = b.finalize().unwrap();
            let _ = writeln!(log, "loading {}", full.to_string(uefi::proto::device_path::text::DisplayOnly(true), uefi::proto::device_path::text::AllowShortcuts(true)).map(|s| String::from(&*s)).unwrap_or_default());
            save(&log);
            match boot::load_image(boot::image_handle(), LoadImageSource::FromDevicePath { device_path: full, boot_policy: uefi::proto::BootPolicy::ExactMatch }) {
                Ok(img) => {
                    if let Ok(li) = unsafe { boot::open_protocol::<LoadedImage>(OpenProtocolParams { handle: img, agent: boot::image_handle(), controller: None }, OpenProtocolAttributes::GetProtocol) } {
                        let (base, size) = li.info();
                        let _ = writeln!(log, "WINBOOT.EFI loaded at {:p} size {:#x}", base, size);
                        let r = unsafe { exc::patch_image(base as *mut u8, KEEP_VBAR) };
                        let _ = writeln!(log, "patched WINBOOT.EFI: {r:?}");
                        unsafe { trace::mark_patched(base as u64) };
                        // ImgpFilterValidationFailure -> always "ignore" (return 0): the boot manager
                        // must accept the loader after we patch it in memory. RVA from the public PDB
                        // of this build (bootmgfw.pdb C6B0B2AA...); checked against its prologue.
                        unsafe {
                            let f = (base as *mut u8).add(0x1cf418) as *mut u32;
                            if f.read() == 0xa9be_53f3 && f.add(1).read() == 0xa901_5bf5 {
                                f.write(0x5280_0000); // mov w0, #0
                                f.add(1).write(0xd65f_03c0); // ret
                                core::arch::asm!("dc cvau, {0}", "ic ivau, {0}", "dsb ish", "isb", in(reg) f);
                                let _ = writeln!(log, "ImgpFilterValidationFailure -> return 0");
                            } else {
                                let _ = writeln!(log, "ImgpFilterValidationFailure: not this build, not patched");
                            }
                        }
                    }
                    let _ = writeln!(log, "exception handler: {:?}", exc::install());
                    let _ = writeln!(log, "load_image ok; tracing; start_image...");
                    save(&log);
                    unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) trace::FW_TTBR0) };
                    let _ = writeln!(log, "firmware TTBR0 {:#x}", unsafe { trace::FW_TTBR0 });
                    save(&log);
                    trace::install();
                    unsafe { shim::TRACING = true };
                    let r = boot::start_image(img);
                    let _ = writeln!(status, "start_image returned {:?}", r);
                }
                Err(e) => {
                    let _ = writeln!(status, "load_image: {:?}", e.status());
                }
            }
        }
        Err(e) => {
            let _ = writeln!(status, "device path: {:?}", e.status());
        }
    }
    log.push_str(&status);
    save(&log);
    uefi::println!("{}", status);
    boot::stall(600_000_000);
    Status::SUCCESS
}

/// rust-lld wants it (UCS-2 strings in the uefi crate); there is no C library here
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcslen(s: *const u16) -> usize {
    let mut n = 0;
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    n
}
