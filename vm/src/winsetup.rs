//! `hypermac create NAME --os windows --iso WINDOWS.iso --virtio virtio-win.iso --user U` (the
//! password in HYPERMAC_WIN_PASSWORD): a Windows 11 ARM VM installed from Microsoft's ISO with
//! nobody at the keyboard, to the desktop. The steps README.md "Windows 11 ARM guest" did by hand:
//!
//! 1. The install stick (`setup.img`, MBR + FAT32: VZ's EFI boots a USB stick, not an ISO): the
//!    ISO's files, install.wim split into .swm under FAT32's 4 GB (wimlib-imagex), efiprobe in front
//!    of the boot manager (\efi\boot\bootaa64.efi -> winboot.efi), the drivers (virtio-win, w11/ARM64),
//!    hypermac.cmd, an autounattend.xml made for this VM (language from the ISO, the account given,
//!    the Pro edition), and boot.wim's WinPE starting hm-setup.cmd instead of setup (winpeshl.ini).
//! 2. The drivers stick (`drivers.img`): hypermac.cmd and the drivers only. With the whole install
//!    medium still plugged in after the first phase, setup stops at "it looks like you started an
//!    upgrade and booted from installation media".
//! 3. Phase 1 (WinPE): hm-setup.cmd runs setup with /noreboot and shuts down. A guest reboot never
//!    reaches the host (VZ restarts the VM inside itself, tested 2026-11-10), a shutdown does: the
//!    VM process ends. The host then puts efiprobe in front of the boot manager on the disk's EFI
//!    partition (`patch_esp`; in WinPE, mountvol S: /s did not get it, 2026-11-10).
//! 4. Phase 2: the drivers stick instead; the guest reboots by itself through specialize and OOBE
//!    (efiprobe on the disk patches each boot), logs on, and at the first logon hypermac.cmd writes
//!    \hypermac-done.txt and shuts down.
//! 5. Done: the sticks go, the VM starts for good.
//!
//! The `installing` file in the VM's directory tells the window where it is: `win:stick`,
//! `win:phase1`, `win:phase2`, or `win:failed:WHY`. Deleting the VM (its directory) ends this.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, bail, ensure};

use crate::{opt, vm_dir};

const AUTOUNATTEND: &str = include_str!("../tools/win11-usb/autounattend.xml");
const HYPERMAC_CMD: &str = include_str!("../tools/win11-usb/hypermac.cmd");
const HM_SETUP_CMD: &str = include_str!("../tools/win11-usb/hm-setup.cmd");
const WINPESHL_INI: &str = include_str!("../tools/win11-usb/winpeshl.ini");

/// virtio-win's drivers that go in (w11/ARM64 of each): the NIC, our virtio-gpu and -input
/// (viogpudo, vioinput), the RNG and the balloon. Not vioserial: it binds to VZ's console and
/// bugchecks (0x7E); the disk is NVMe (Windows' own driver), not viostor.
const DRIVERS: &[&str] = &["NetKVM", "viogpudo", "vioinput", "viorng", "Balloon"];

/// install.wim pieces: under FAT32's 4 GB file limit
const SPLIT_MB: &str = "3800";

pub fn create(name: &str, args: &[String]) -> anyhow::Result<()> {
    let iso = PathBuf::from(opt(args, "--iso").context("--iso PATH (Microsoft's Windows 11 ARM ISO)")?);
    let virtio = PathBuf::from(opt(args, "--virtio").context("--virtio PATH (virtio-win.iso)")?);
    let user = opt(args, "--user").unwrap_or("win").to_string();
    let password = std::env::var("HYPERMAC_WIN_PASSWORD").ok().filter(|p| !p.is_empty()).context("HYPERMAC_WIN_PASSWORD: the account's password")?;
    ensure!(iso.exists(), "{}: no such file", iso.display());
    ensure!(virtio.exists(), "{}: no such file", virtio.display());
    ensure!(valid_user(&user), "{user:?}: letters, digits, - _ . only (20 at most)");
    let wimlib = wimlib().context("wimlib-imagex is missing: brew install wimlib")?;
    let efiprobe = efiprobe().context("efiprobe.efi is missing: run build.sh (it builds vm/tools/efiprobe)")?;

    let dir = vm_dir(name);
    ensure!(!dir.join("vm.json").exists(), "VM {name:?} exists ({})", dir.display());
    std::fs::create_dir_all(&dir)?;
    let setup_img = dir.join("setup.img");
    std::fs::File::create(&setup_img)?;
    mark(&dir, "win:stick");
    // vm.json now, so the window lists the VM (with the progress) from the start
    let mut cargs: Vec<String> = args.to_vec();
    set_arg(&mut cargs, "--iso", &setup_img.to_string_lossy());
    set_arg(&mut cargs, "--os", "windows");
    crate::vm::create_linux(name, &cargs)?;
    edit_conf(name, |c| {
        c.own_devices = true;
        c.os_version.get_or_insert_with(|| "11".into());
    })?;

    if let Err(e) = prepare(name, &dir, &iso, &virtio, &user, &password, &wimlib, &efiprobe) {
        fail(&dir, &format!("{e:#}"));
        return Err(e);
    }
    supervise(name, &dir)
}

/// Both sticks.
#[allow(clippy::too_many_arguments)]
fn prepare(name: &str, dir: &Path, iso: &Path, virtio: &Path, user: &str, password: &str, wimlib: &Path, efiprobe: &Path) -> anyhow::Result<()> {
    let tmp = std::env::temp_dir().join(format!("hypermac-win-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let isomount = Mounted::iso(iso, &tmp)?;
    let virtiomount = Mounted::iso(virtio, &tmp)?;
    let src = isomount.path.clone();

    // what the ISO holds: its language, the Pro edition's index, install.wim or .esd
    let lang = lang_of(&src).unwrap_or_else(|| "en-US".into());
    let image = ["sources/install.wim", "sources/install.esd"].iter().map(|p| src.join(p)).find(|p| p.exists()).context("the ISO has no sources/install.wim: not a Windows installer?")?;
    let index = pro_index(wimlib, &image).unwrap_or(1);
    eprintln!("Windows ISO: language {lang}, edition index {index}, {}", image.display());
    let xml = AUTOUNATTEND
        .replace("{{LANG}}", &lang)
        .replace("{{COMPUTER}}", &computer_name(name))
        .replace("{{TIMEZONE}}", &time_zone())
        .replace("{{USER}}", &xml_escape(user))
        .replace("{{PASSWORD}}", &xml_escape(password))
        .replace("{{INDEX}}", &index.to_string());

    // the install stick: the ISO's size and room for the extras
    let iso_bytes = std::fs::metadata(iso)?.len();
    let size = (iso_bytes + (2u64 << 30)).div_ceil(1 << 30) << 30;
    std::fs::OpenOptions::new().write(true).open(dir.join("setup.img"))?.set_len(size)?;
    {
        let stick = Mounted::new_fat(&dir.join("setup.img"), "WINSETUP")?;
        let dst = &stick.path;
        eprintln!("copying the ISO to the install stick");
        copy_tree(&src, dst, &|rel| {
            let r = rel.to_string_lossy().to_lowercase();
            r == "sources/install.wim" || r == "sources/install.esd"
        })?;
        let rel_image = image.strip_prefix(&src)?.to_path_buf();
        if std::fs::metadata(&image)?.len() >= 4_000_000_000 {
            eprintln!("splitting {} for FAT32", rel_image.display());
            run(Command::new(wimlib).arg("split").arg(&image).arg(dst.join("sources/install.swm")).arg(SPLIT_MB))?;
        } else {
            copy_file(&image, &dst.join(&rel_image))?;
        }
        // efiprobe in front of the boot manager (it chainloads WINBOOT.EFI beside itself)
        let boot = find_ci(dst, &["efi", "boot", "bootaa64.efi"]).context("the ISO has no efi/boot/bootaa64.efi: not an ARM64 installer?")?;
        std::fs::rename(&boot, boot.with_file_name("WINBOOT.EFI"))?;
        copy_file(efiprobe, &boot.with_file_name("BOOTAA64.EFI"))?;
        // WinPE starts hm-setup.cmd (winpeshl.ini) instead of setup: boot.wim's setup image (2)
        let peshl = tmp.join("winpeshl.ini");
        let hmsetup = tmp.join("hm-setup.cmd");
        std::fs::write(&peshl, WINPESHL_INI)?;
        std::fs::write(&hmsetup, HM_SETUP_CMD)?;
        let bootwim = find_ci(dst, &["sources", "boot.wim"]).context("no sources/boot.wim")?;
        let mut up = Command::new(wimlib);
        up.arg("update").arg(&bootwim).arg("2").stdin(Stdio::piped());
        let mut child = up.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        {
            use std::io::Write;
            let mut stdin = child.stdin.take().context("stdin")?;
            writeln!(stdin, "add '{}' /Windows/System32/winpeshl.ini", peshl.display())?;
            writeln!(stdin, "add '{}' /Windows/System32/hm-setup.cmd", hmsetup.display())?;
        }
        let out = child.wait_with_output()?;
        ensure!(out.status.success(), "wimlib-imagex update boot.wim: {}", String::from_utf8_lossy(&out.stderr).trim());
        std::fs::write(dst.join("autounattend.xml"), &xml)?;
        put_drivers(&virtiomount.path, dst)?;
    }
    // the drivers stick
    std::fs::File::create(dir.join("drivers.img"))?.set_len(512 << 20)?;
    {
        let stick = Mounted::new_fat(&dir.join("drivers.img"), "HMDRIVERS")?;
        put_drivers(&virtiomount.path, &stick.path)?;
    }
    drop(isomount);
    drop(virtiomount);
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

/// \hypermac.cmd and \drivers\NAME (virtio-win's w11/ARM64) on a stick.
fn put_drivers(virtio: &Path, stick: &Path) -> anyhow::Result<()> {
    std::fs::write(stick.join("hypermac.cmd"), HYPERMAC_CMD)?;
    for d in DRIVERS {
        let from = virtio.join(d).join("w11/ARM64");
        if !from.exists() {
            eprintln!("virtio-win has no {d}/w11/ARM64: left out");
            continue;
        }
        copy_tree(&from, &stick.join("drivers").join(d), &|_| false)?;
    }
    Ok(())
}

/// Run the two phases, then the VM for good. Where an earlier run got to is read off the disk
/// (`hypermac win-resume NAME` after a failure or a host restart): an installed boot manager on
/// its EFI partition = phase 1 done.
fn supervise(name: &str, dir: &Path) -> anyhow::Result<()> {
    let efiprobe = efiprobe().context("efiprobe.efi is missing: run build.sh")?;
    let phase1_done = crate::load(name)?.iso.is_some_and(|i| i.ends_with("drivers.img")) || patch_esp(dir, &efiprobe).unwrap_or(false);
    if !phase1_done {
        mark(dir, "win:phase1");
        run_vm(name, &[])?;
        if !dir.exists() {
            return Ok(()); // deleted meanwhile
        }
        // the host puts efiprobe in front of the installed boot manager: WinPE's own try at it
        // (mountvol) did not work, and what it wrote to the stick was lost at its power-off
        match patch_esp(dir, &efiprobe) {
            Ok(true) => {}
            Ok(false) => return failed(dir, "phase 1 did not finish (no boot manager on the disk's EFI partition)"),
            Err(e) => return failed(dir, &format!("phase 1: the disk's EFI partition: {e:#}")),
        }
    }
    edit_conf(name, |c| c.iso = Some(dir.join("drivers.img").to_string_lossy().into_owned()))?;
    mark(dir, "win:phase2");
    run_vm(name, &[])?;
    if !dir.exists() {
        return Ok(());
    }
    if !install_done(dir) {
        return failed(dir, "phase 2 did not finish (the VM stopped before the first logon)");
    }
    edit_conf(name, |c| c.iso = None)?;
    let _ = std::fs::remove_file(dir.join("setup.img"));
    let _ = std::fs::remove_file(dir.join("drivers.img"));
    let _ = std::fs::remove_file(dir.join("installing"));
    eprintln!("Windows is installed; starting {name}");
    start_detached(name)
}

/// `hypermac win-resume NAME`: go on with an install that stopped (its sticks are still there).
pub fn resume(name: &str) -> anyhow::Result<()> {
    let dir = vm_dir(name);
    if crate::load(name).is_ok_and(|c| crate::bhyve::is(&c)) {
        return crate::bhyve::resume(name);
    }
    ensure!(dir.join("setup.img").exists() && dir.join("drivers.img").exists(), "{name}: no Windows install to go on with");
    supervise(name, &dir)
}

fn failed(dir: &Path, why: &str) -> anyhow::Result<()> {
    fail(dir, why);
    bail!(why.to_string())
}

/// Put efiprobe in front of the installed boot manager on the VM's disk (stopped): the EFI
/// partition's \EFI\Microsoft\Boot\bootmgfw.efi -> winboot.efi and \EFI\Boot\bootaa64.efi ->
/// WINBOOT.EFI, efiprobe in their place (it chainloads the one beside it). false: no installed
/// boot manager there (phase 1 has not run through).
fn patch_esp(dir: &Path, efiprobe: &Path) -> anyhow::Result<bool> {
    let out = run(Command::new("/usr/bin/hdiutil").args(["attach", "-imagekey", "diskimage-class=CRawDiskImage", "-nomount", "-nobrowse"]).arg(dir.join("disk.img")))?;
    let dev = out.split_whitespace().next().context("hdiutil attach: no device")?.to_string();
    let _guard = Detach(dev.clone());
    // GPT: the EFI partition is the first (autounattend.xml makes it so)
    let esp = format!("{dev}s1");
    if !run(Command::new("/usr/sbin/diskutil").args(["info", &esp]))?.contains("EFI") {
        return Ok(false);
    }
    run(Command::new("/usr/sbin/diskutil").args(["mount", &esp]))?;
    let info = run(Command::new("/usr/sbin/diskutil").args(["info", &esp]))?;
    let mp = PathBuf::from(info.lines().find_map(|l| l.trim().strip_prefix("Mount Point:").map(|m| m.trim().to_string())).filter(|m| !m.is_empty()).context("the EFI partition did not mount")?);
    let Some(mgr) = find_ci(&mp, &["EFI", "Microsoft", "Boot", "bootmgfw.efi"]) else {
        return Ok(false);
    };
    let real = mgr.with_file_name("winboot.efi");
    if !real.exists() {
        std::fs::rename(&mgr, &real)?;
    }
    copy_file(efiprobe, &mgr)?;
    if let Some(fallback) = find_ci(&mp, &["EFI", "Boot", "bootaa64.efi"]) {
        let real = fallback.with_file_name("WINBOOT.EFI");
        if !real.exists() {
            std::fs::rename(&fallback, &real)?;
        }
        copy_file(efiprobe, &fallback)?;
    }
    eprintln!("efiprobe is in front of the boot manager on the disk");
    Ok(true)
}

/// The first logon's mark (hypermac.cmd done): on the drivers stick, or on C: (a stick's last
/// writes can be lost at the guest's power-off).
fn install_done(dir: &Path) -> bool {
    if read_from_stick(&dir.join("drivers.img"), "hypermac-done.txt").is_some() {
        return true;
    }
    // C: is the disk's third partition (NTFS, mounted read only by macOS)
    let Ok(m) = Mounted::raw(&dir.join("disk.img"), true) else { return false };
    find_ci(&m.path, &["hypermac-done.txt"]).is_some()
}

struct Detach(String);

impl Drop for Detach {
    fn drop(&mut self) {
        detach(&self.0);
    }
}

/// `hypermac run NAME` until the guest shuts down; its output goes to run.log.
pub(crate) fn run_vm(name: &str, more: &[&str]) -> anyhow::Result<()> {
    let log = std::fs::OpenOptions::new().create(true).append(true).open(vm_dir(name).join("run.log"))?;
    let st = Command::new(std::env::current_exe()?)
        .args(["run", name])
        .args(more)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .status()?;
    eprintln!("{name}: the VM ended ({st})");
    Ok(())
}

pub(crate) fn start_detached(name: &str) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(vm_dir(name).join("run.log"))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["run", name]).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

pub(crate) fn mark(dir: &Path, what: &str) {
    let _ = std::fs::write(dir.join("installing"), what);
}

pub(crate) fn fail(dir: &Path, why: &str) {
    eprintln!("Windows install failed: {why}");
    if dir.exists() {
        mark(dir, &format!("win:failed:{why}"));
    }
}

pub(crate) fn edit_conf(name: &str, f: impl FnOnce(&mut crate::VmConf)) -> anyhow::Result<()> {
    let mut c = crate::load(name)?;
    f(&mut c);
    std::fs::write(vm_dir(name).join("vm.json"), serde_json::to_vec_pretty(&c)?)?;
    Ok(())
}

pub(crate) fn set_arg(args: &mut Vec<String>, key: &str, value: &str) {
    match args.iter().position(|a| a == key) {
        Some(i) if i + 1 < args.len() => args[i + 1] = value.to_string(),
        _ => {
            args.push(key.to_string());
            args.push(value.to_string());
        }
    }
}

// ---- what the ISO says ----------------------------------------------------------------------

/// sources/lang.ini's first UI language ("ja-jp = 3" -> "ja-JP").
pub(crate) fn lang_of(src: &Path) -> Option<String> {
    let ini = std::fs::read_to_string(find_ci(src, &["sources", "lang.ini"])?).ok()?;
    let mut in_section = false;
    for l in ini.lines() {
        let l = l.trim();
        if l.starts_with('[') {
            in_section = l.eq_ignore_ascii_case("[Available UI Languages]");
            continue;
        }
        if in_section {
            let tag = l.split('=').next()?.trim();
            let (a, b) = tag.split_once('-')?;
            return Some(format!("{}-{}", a.to_lowercase(), b.to_uppercase()));
        }
    }
    None
}

/// The Professional edition's index in install.wim (wimlib-imagex info).
fn pro_index(wimlib: &Path, image: &Path) -> Option<u32> {
    let out = Command::new(wimlib).arg("info").arg(image).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut index = None;
    let mut first = None;
    for l in text.lines() {
        let (k, v) = match l.split_once(':') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue,
        };
        match k {
            "Index" => {
                index = v.parse().ok();
                first = first.or(index);
            }
            "Edition ID" if v.eq_ignore_ascii_case("Professional") => return index,
            _ => {}
        }
    }
    first
}

/// The VM's name as a Windows computer name: A–Z, 0–9, -; 15 at most.
pub(crate) fn computer_name(name: &str) -> String {
    let n: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(15).collect::<String>().to_uppercase();
    if n.is_empty() { "HYPERMAC-WIN".into() } else { n }
}

/// This Mac's time zone as Windows names it (the common ones; else UTC).
pub(crate) fn time_zone() -> String {
    let tz = std::fs::read_link("/etc/localtime").ok().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let tz = tz.rsplit("zoneinfo/").next().unwrap_or("");
    match tz {
        "Asia/Tokyo" => "Tokyo Standard Time",
        "Asia/Shanghai" | "Asia/Chongqing" | "Asia/Hong_Kong" | "Asia/Macau" => "China Standard Time",
        "Asia/Taipei" => "Taipei Standard Time",
        "Asia/Seoul" => "Korea Standard Time",
        "Asia/Singapore" => "Singapore Standard Time",
        "America/Los_Angeles" => "Pacific Standard Time",
        "America/Denver" => "Mountain Standard Time",
        "America/Chicago" => "Central Standard Time",
        "America/New_York" => "Eastern Standard Time",
        "Europe/London" => "GMT Standard Time",
        "Europe/Berlin" | "Europe/Paris" | "Europe/Amsterdam" | "Europe/Rome" | "Europe/Madrid" => "W. Europe Standard Time",
        _ => "UTC",
    }
    .into()
}

pub fn valid_user(u: &str) -> bool {
    !u.is_empty() && u.len() <= 20 && u.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && !u.eq_ignore_ascii_case("administrator")
}

pub(crate) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

// ---- tools ----------------------------------------------------------------------------------

/// wimlib-imagex (Homebrew).
pub fn wimlib() -> Option<PathBuf> {
    let mut dirs = vec![PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin")];
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    dirs.into_iter().map(|d| d.join("wimlib-imagex")).find(|p| p.exists())
}

/// efiprobe.efi: beside this program (build.sh installs it there), or HYPERMAC_EFIPROBE.
pub fn efiprobe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HYPERMAC_EFIPROBE").map(PathBuf::from).filter(|p| p.exists()) {
        return Some(p);
    }
    let beside = std::env::current_exe().ok()?.parent()?.join("efiprobe.efi");
    beside.exists().then_some(beside)
}

pub(crate) fn run(cmd: &mut Command) -> anyhow::Result<String> {
    let out = cmd.output().with_context(|| format!("{cmd:?}"))?;
    ensure!(out.status.success(), "{:?}: {}", cmd.get_program(), String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A path under `root` by its components, ignoring case (FAT and ISO 9660 names vary).
pub(crate) fn find_ci(root: &Path, parts: &[&str]) -> Option<PathBuf> {
    let mut p = root.to_path_buf();
    for part in parts {
        let hit = std::fs::read_dir(&p).ok()?.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(part))?;
        p = hit.path();
    }
    Some(p)
}

/// Copy a tree without macOS metadata (no ._ files on FAT); `skip` gets paths relative to `from`.
pub(crate) fn copy_tree(from: &Path, to: &Path, skip: &dyn Fn(&Path) -> bool) -> anyhow::Result<()> {
    fn walk(base: &Path, dir: &Path, to: &Path, skip: &dyn Fn(&Path) -> bool) -> anyhow::Result<()> {
        for e in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
            let e = e?;
            let p = e.path();
            let rel = p.strip_prefix(base)?;
            if skip(rel) || e.file_name().to_string_lossy().starts_with("._") {
                continue;
            }
            let ft = e.file_type()?;
            if ft.is_dir() {
                std::fs::create_dir_all(to.join(rel))?;
                walk(base, &p, to, skip)?;
            } else if ft.is_file() {
                copy_file(&p, &to.join(rel))?;
            }
        }
        Ok(())
    }
    std::fs::create_dir_all(to)?;
    walk(from, from, to, skip)
}

fn copy_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    if let Some(d) = to.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut r = std::fs::File::open(from).with_context(|| format!("open {}", from.display()))?;
    let mut w = std::fs::File::create(to).with_context(|| format!("create {}", to.display()))?;
    std::io::copy(&mut r, &mut w).with_context(|| format!("copy to {}", to.display()))?;
    Ok(())
}

/// A file from a stick image's FAT partition (read only), if it is there.
fn read_from_stick(img: &Path, file: &str) -> Option<String> {
    let m = Mounted::raw(img, true).ok()?;
    std::fs::read_to_string(find_ci(&m.path, &[file])?).ok()
}

/// A disk image attached (and its file system mounted) while this lives.
pub(crate) struct Mounted {
    pub(crate) dev: String,
    pub(crate) path: PathBuf,
}

impl Mounted {
    /// An ISO, read only, mounted under `tmp`.
    pub(crate) fn iso(iso: &Path, tmp: &Path) -> anyhow::Result<Mounted> {
        let out = run(Command::new("/usr/bin/hdiutil").args(["attach", "-readonly", "-nobrowse", "-mountrandom"]).arg(tmp).arg(iso))?;
        Self::parse_or_detach(&out).with_context(|| format!("{}: nothing mounted", iso.display()))
    }

    /// A raw stick image (MBR + one FAT partition), mounted.
    fn raw(img: &Path, readonly: bool) -> anyhow::Result<Mounted> {
        let mut cmd = Command::new("/usr/bin/hdiutil");
        cmd.args(["attach", "-imagekey", "diskimage-class=CRawDiskImage", "-nobrowse"]);
        if readonly {
            cmd.arg("-readonly");
        }
        let out = run(cmd.arg(img))?;
        Self::parse_or_detach(&out).with_context(|| format!("{}: nothing mounted", img.display()))
    }

    /// Partition a sized, empty raw image as a stick (MBR + FAT32 `label`) and mount it.
    fn new_fat(img: &Path, label: &str) -> anyhow::Result<Mounted> {
        let out = run(Command::new("/usr/bin/hdiutil").args(["attach", "-imagekey", "diskimage-class=CRawDiskImage", "-nomount", "-nobrowse"]).arg(img))?;
        let dev = out.split_whitespace().next().context("hdiutil attach: no device")?.to_string();
        let parted = run(Command::new("/usr/sbin/diskutil").args(["partitionDisk", &dev, "MBR", "FAT32", label, "100%"]));
        if let Err(e) = parted {
            detach(&dev);
            return Err(e);
        }
        let info = run(Command::new("/usr/sbin/diskutil").args(["info", &format!("{dev}s1")]))?;
        let mp = info.lines().find_map(|l| l.trim().strip_prefix("Mount Point:").map(|m| m.trim().to_string())).filter(|m| !m.is_empty());
        match mp {
            Some(mp) => Ok(Mounted { dev, path: PathBuf::from(mp) }),
            None => {
                detach(&dev);
                bail!("{}: the new FAT32 volume did not mount", img.display())
            }
        }
    }

    fn parse_or_detach(out: &str) -> Option<Mounted> {
        let m = Self::parse(out);
        if m.is_none() {
            if let Some(dev) = out.split_whitespace().next() {
                detach(dev);
            }
        }
        m
    }

    /// hdiutil attach's table: the device on the first line, a mount point in the last column.
    fn parse(out: &str) -> Option<Mounted> {
        let dev = out.split_whitespace().next()?.to_string();
        let path = out.lines().filter_map(|l| l.split('\t').map(str::trim).filter(|c| c.starts_with('/') && !c.starts_with("/dev/")).last()).last()?;
        Some(Mounted { dev, path: PathBuf::from(path) })
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        detach(&self.dev);
    }
}

pub(crate) fn detach(dev: &str) {
    for force in [false, true] {
        let mut cmd = Command::new("/usr/bin/hdiutil");
        cmd.args(["detach", dev]);
        if force {
            cmd.arg("-force");
        }
        if cmd.stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}
