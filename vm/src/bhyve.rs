//! The bhyve engine (vm.json `"engine": "bhyve"`): FreeBSD's bhyve on Hypervisor.framework
//! (vm/bhyve), which runs what Virtualization.framework cannot: Windows 11 ARM from Microsoft's
//! ISO with its boot chain untouched (no efiprobe).
//!
//! `hypermac run NAME` is then a supervisor around one bhyve process:
//! - bhyve exits 0 when the guest reboots: start it again; 1 or 2 = the guest powered off;
//! - the picture: bhyve's framebuffer speaks VNC on a loopback port; this process is its only
//!   client (`viewer`) and hands the frames to what the VZ engine's own virtio-gpu feeds
//!   (vgpu::snapshot -> screen.shm for hyperMAC's RDP, rfb.rs's VNC server with the VM's
//!   password, the console window);
//! - keyboard and pointer (vinput::key / abs / button, so ctl.sock's vkey / vabs / vbtn, the
//!   console window and rfb.rs) go back over the same connection to bhyve's USB keyboard and tablet;
//! - the guest's serial console is a pty: DIR/console.log;
//! - ctl.sock: info, stop (the ACPI power button: SIGTERM to bhyve), kill, console, vnc.
//!
//! `hypermac create NAME --os windows` (`create_windows`): the ISO as a CD, a second small CD
//! (setup.iso, made here) with autounattend.xml and virtio-win's drivers. Setup runs by itself
//! through its reboots and powers off at the first desktop. With `--manual` the VM is only
//! made: the user starts it and answers setup in the console; setup.iso then only switches off
//! the hardware checks and brings the drivers.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, Ordering};
use std::time::Duration;

use anyhow::{Context, bail, ensure};

use crate::winsetup as ws;
use crate::{VmConf, num, opt, vm_dir};

const AUTOUNATTEND: &str = include_str!("../tools/win11-bhyve/autounattend.xml");
const AUTOUNATTEND_MANUAL: &str = include_str!("../tools/win11-bhyve/autounattend-manual.xml");
const HYPERMAC_CMD: &str = include_str!("../tools/win11-usb/hypermac.cmd");

/// virtio-win's drivers for bhyve's devices: the NIC, the RNG, and the serial port that is the
/// channel to hyperMAC's agent (AGENT_PORT). The disk is NVMe, the keyboard and pointer USB
/// HID, the display the firmware's framebuffer: all Windows' own drivers.
const DRIVERS: &[&str] = &["NetKVM", "viorng", "vioserial"];

/// bhyve's framebuffer device takes this much at most
const MAX_W: isize = 1920;
const MAX_H: isize = 1200;

pub fn is(conf: &VmConf) -> bool {
    conf.engine.as_deref() == Some("bhyve")
}

/// A file of the bhyve engine: $`env`, beside this program (build.sh installs it there), or in
/// the source tree this program was built in (vm/target/release/hypermac -> vm/bhyve/`dev`).
fn find(env: &str, name: &str, dev: &str) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(env).map(PathBuf::from).filter(|p| p.exists()) {
        return Some(p);
    }
    let exe = std::env::current_exe().ok()?;
    let beside = exe.parent()?.join(name);
    if beside.exists() {
        return Some(beside);
    }
    // hyperMAC.app: programs in Contents/MacOS, the firmware (not code) in Contents/Resources
    let res = exe.parent()?.parent()?.join("Resources").join(name);
    if res.exists() {
        return Some(res);
    }
    let tree = exe.parent()?.parent()?.parent()?.join("bhyve").join(dev);
    tree.exists().then_some(tree)
}

pub fn program() -> Option<PathBuf> {
    find("HYPERMAC_BHYVE", "bhyve", "build/bhyve")
}

/// The 3D engine (vm.json `"gpu3d": true`): bhyve built with virglrenderer (`make VIRGL=PREFIX`
/// in vm/bhyve makes build/bhyve-3d) and what it starts for every guest process that draws:
/// `virgl_render_server`, which loads Direct3D-on-Metal from `libdxmt-native.dylib`.
pub struct Gpu3d {
    pub bhyve: PathBuf,
    server: PathBuf,
    dxmt: PathBuf,
    /// the guest's driver (Triton: viogpu3d.inf and its files), when it is there: a new
    /// Windows VM's setup CD takes it
    pub driver: Option<PathBuf>,
}

/// Those two are looked for in $HYPERMAC_GPU3D, else `gpu3d/` beside bhyve-3d (hyperMAC.app:
/// in Contents/Resources): flat, or as virglrenderer's and DXMT's `meson install` leave them
/// (lib/, libexec/).
pub fn gpu3d() -> Option<Gpu3d> {
    let bhyve = find("HYPERMAC_BHYVE3D", "bhyve-3d", "build/bhyve-3d")?;
    let beside = bhyve.with_file_name("gpu3d");
    let in_app = bhyve.parent().and_then(|p| p.parent()).map(|p| p.join("Resources/gpu3d"));
    let dir = std::env::var_os("HYPERMAC_GPU3D").map(PathBuf::from).unwrap_or_else(|| match in_app {
        Some(a) if !beside.exists() && a.exists() => a,
        _ => beside,
    });
    let pick = |sub: &str, name: &str| [dir.join(name), dir.join(sub).join(name)].into_iter().find(|p| p.exists());
    let driver = Some(dir.join("driver")).filter(|d| d.join("viogpu3d.inf").exists());
    Some(Gpu3d { bhyve, server: pick("libexec", "virgl_render_server")?, dxmt: pick("lib", "libdxmt-native.dylib")?, driver })
}

impl Gpu3d {
    /// What the renderer reads from the environment. Not DYLD_*: the system takes those away
    /// from a program started through /bin/sh, and names what is loaded less exactly.
    fn env(&self, cmd: &mut Command) {
        cmd.env("RENDER_SERVER_EXEC_PATH", &self.server).env("NPT_BACKEND", "dxmt");
        // DXMT writes a log file into the current directory otherwise
        if std::env::var_os("DXMT_LOG_PATH").is_none() {
            cmd.env("DXMT_LOG_PATH", "none");
        }
        for k in ["NPT_D3D11_LIBRARY_PATH", "NPT_DXGI_LIBRARY_PATH", "NPT_D3D12_LIBRARY_PATH"] {
            cmd.env(k, &self.dxmt);
        }
    }
}

/// swtpm (Homebrew's, or beside this program): the software TPM behind a Windows VM's TPM 2.0.
pub fn swtpm() -> Option<PathBuf> {
    if let Some(p) = find("HYPERMAC_SWTPM", "swtpm", "build/swtpm") {
        return Some(p);
    }
    ["/opt/homebrew/bin/swtpm", "/usr/local/bin/swtpm"].iter().map(PathBuf::from).find(|p| p.exists())
}

/// Does this bhyve have the TPM built in (vm/bhyve/src/tpm_emul_libtpms.c: libtpms linked in)?
/// Asked of the program itself: its text has the backend's message.
fn tpm_builtin() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| {
        let needle = b"libtpms needs tpm.path";
        program().and_then(|p| std::fs::read(p).ok()).is_some_and(|b| b.windows(needle.len()).any(|w| w == needle))
    })
}

/// A Windows VM gets a TPM 2.0: bhyve's own (libtpms inside it), else an swtpm process when
/// one is installed.
pub fn tpm_available() -> bool {
    tpm_builtin() || swtpm().is_some()
}

/// Start the VM's TPM for one bhyve run: swtpm on DIR/tpm.sock, its state in DIR/tpm (made on
/// the first start). A new process per run: a machine reset must reset the TPM (its PCRs) too.
/// None: no swtpm on this Mac, or it did not come up; the VM then runs without a TPM.
fn start_tpm(dir: &Path) -> Option<(std::process::Child, PathBuf)> {
    let exe = swtpm()?;
    let (state, sock) = (dir.join("tpm"), dir.join("tpm.sock"));
    std::fs::create_dir_all(&state).ok()?;
    stop_tpm(dir); // one left behind would hold the state's lock
    let _ = std::fs::remove_file(&sock);
    let mut child = Command::new(exe)
        .args(["socket", "--tpm2", "--tpmstate"])
        .arg(format!("dir={}", state.display()))
        .arg("--server")
        .arg(format!("type=unixio,path={}", sock.display()))
        // no firmware here starts the TPM (EDK2 is built without TPM support): swtpm does
        // TPM2_Startup itself, and needs no init over a control channel
        .args(["--flags", "not-need-init,startup-clear"])
        .stdin(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let _ = std::fs::write(dir.join("tpm.pid"), child.id().to_string());
    // up when it answers a command: TPM2_GetRandom(8). The first command after a start is slow
    // (self tests; much slower when the state is new), and the guest's driver gives a command
    // 2 s (Linux's did time out, 2026-11-11).
    let ready = (0..100).any(|_| {
        std::thread::sleep(Duration::from_millis(100));
        let Ok(mut s) = std::os::unix::net::UnixStream::connect(&sock) else { return false };
        let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
        let mut rsp = [0u8; 64];
        s.write_all(&[0x80, 0x01, 0, 0, 0, 12, 0, 0, 0x01, 0x7b, 0, 8]).is_ok() && s.read(&mut rsp).is_ok_and(|n| n >= 10)
    });
    if !ready {
        eprintln!("tpm: swtpm did not come up; no TPM this run");
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    Some((child, sock))
}

/// End the swtpm DIR/tpm.pid names (this run's at a kill, or one a killed supervisor left).
fn stop_tpm(dir: &Path) {
    let f = dir.join("tpm.pid");
    let Some(pid) = std::fs::read_to_string(&f).ok().and_then(|s| s.trim().parse::<i32>().ok()) else { return };
    let comm = Command::new("/bin/ps").args(["-p", &pid.to_string(), "-o", "comm="]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    if comm.ends_with("swtpm") {
        unsafe { libc::kill(pid, libc::SIGKILL) };
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = std::fs::remove_file(f);
}

pub fn firmware() -> Option<PathBuf> {
    find("HYPERMAC_BHYVE_FW", "QEMU_EFI.fd", "firmware/QEMU_EFI.fd")
}

/// The UEFI variables a new Windows VM starts with: Secure Boot on, Microsoft's certificates
/// enrolled (firmware/README.md). Without it the VM's variables start empty: no Secure Boot.
pub fn vars_template() -> Option<PathBuf> {
    find("HYPERMAC_BHYVE_VARS", "QEMU_VARS.secboot.fd", "firmware/QEMU_VARS.secboot.fd")
}

/// Setup's hardware checks are switched off (the registry's LabConfig, set from the answer
/// file) only for a machine that does not meet Windows 11's requirements: no TPM (swtpm is not
/// installed), no Secure Boot, under 4 GB or 64 GB. Else Microsoft's installer runs as it is.
/// HYPERMAC_WIN_BYPASS=1 switches them off anyway.
fn labconfig(conf: &VmConf, disk_bytes: u64, secure_boot: bool) -> String {
    let meets = tpm_available() && secure_boot && conf.mem_gb >= 4 && disk_bytes >= 64_000_000_000 && conf.cpus >= 2;
    if meets && std::env::var_os("HYPERMAC_WIN_BYPASS").is_none() {
        return String::new();
    }
    let mut s = String::from("      <RunSynchronous>\n");
    for (i, check) in ["TPM", "SecureBoot", "RAM", "Storage", "CPU"].iter().enumerate() {
        s += &format!(
            "        <RunSynchronousCommand wcm:action=\"add\"><Order>{}</Order><Path>reg add HKLM\\SYSTEM\\Setup\\LabConfig /v Bypass{check}Check /t REG_DWORD /d 1 /f</Path></RunSynchronousCommand>\n",
            i + 1
        );
    }
    s + "      </RunSynchronous>\n"
}

// ---- the picture and the input: a VNC client of bhyve's framebuffer --------------------------

struct Screen {
    w: u32,
    h: u32,
    frame: u64,
    /// B,G,R,X rows
    pix: Vec<u8>,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SCREEN: Mutex<Option<Screen>> = Mutex::new(None);
/// the connection's writing side: key and pointer events
static INPUT: Mutex<Option<TcpStream>> = Mutex::new(None);
/// pointer: x, y (pixels), RFB button mask
static POINTER: Mutex<(u16, u16, u8)> = Mutex::new((0, 0, 0));

/// This process runs a bhyve VM: vgpu's and vinput's entry points come here.
/// until when (ms since the epoch) someone is at this machine in a window of another process
static VIEWER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn watched() -> bool {
    VIEWER.load(Ordering::Relaxed) > now_ms()
}

pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

// With 3D the picture is taken where the device puts it: its shared memory (/keel-gpu-NAME,
// struct kgpu_shm in vm/bhyve/keel/pci_virtio_gpu.h), not through bhyve's VNC server, which
// sends a screen that changes everywhere (a game) about 7 times a second. Input still goes
// through the VNC connection.
const KGPU_PIXELS: usize = 4096;
const KGPU_SIZE: usize = KGPU_PIXELS + 3840 * 2160 * 4 + 64 * 64 * 4;
/// frame numbers of this source: apart from SCREEN's
const KGPU_FRAME: u64 = 1 << 48;

struct Kgpu(*const u8);
unsafe impl Send for Kgpu {}

/// the VM's name when it runs with 3D
static KGPU_NAME: Mutex<Option<String>> = Mutex::new(None);
/// (the mapping, when it was last tried). Each bhyve start makes a new object: view() drops this.
static KGPU: Mutex<(Option<Kgpu>, Option<std::time::Instant>)> = Mutex::new((None, None));

fn kgpu_u32(p: *const u8, off: usize) -> u32 {
    unsafe { (p.add(off) as *const u32).read_volatile() }
}

/// The 3D device's scanout, when its driver shows one: None = take bhyve's framebuffer.
fn kgpu_snapshot(last: Option<u64>) -> Option<(u64, u32, u32, Vec<u8>)> {
    let mut g = KGPU.lock().unwrap();
    if g.0.is_none() {
        if g.1.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
            return None;
        }
        g.1 = Some(std::time::Instant::now());
        let name = std::ffi::CString::new(format!("/keel-gpu-{}", KGPU_NAME.lock().unwrap().as_deref()?)).ok()?;
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
        if fd < 0 {
            return None;
        }
        let p = unsafe { libc::mmap(std::ptr::null_mut(), KGPU_SIZE, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
        unsafe { libc::close(fd) };
        if p == libc::MAP_FAILED {
            return None;
        }
        g.0 = Some(Kgpu(p as *const u8));
    }
    let p = g.0.as_ref()?.0;
    // magic "kgp1", seq, active, width, height
    let (active, w, h) = (kgpu_u32(p, 8), kgpu_u32(p, 12), kgpu_u32(p, 16));
    if kgpu_u32(p, 0) != 0x3170676b || active == 0 || w == 0 || h == 0 || w > 3840 || h > 2160 {
        return None;
    }
    let mut seq = kgpu_u32(p, 4);
    if last == Some(KGPU_FRAME | seq as u64) {
        return Some((KGPU_FRAME | seq as u64, w, h, Vec::new()));
    }
    let n = w as usize * h as usize * 4;
    let mut pix = vec![0u8; n];
    // (the device writes a frame's rows and then counts seq up: a copy that a frame crossed is
    // taken once more; a second crossing is shown as it is, the next poll replaces it)
    for _ in 0..2 {
        unsafe { std::ptr::copy_nonoverlapping(p.add(KGPU_PIXELS), pix.as_mut_ptr(), n) };
        let now = kgpu_u32(p, 4);
        if now == seq {
            break;
        }
        seq = now;
    }
    Some((KGPU_FRAME | seq as u64, w, h, pix))
}

/// Ask the guest for another screen size (the console's window was resized; ctl "size W H").
/// Only a machine with the 3D adapter can follow: the request goes into the adapter's shared
/// memory (struct kgpu_shm: req_width, req_height, req_seq), the device makes it the preferred
/// mode and tells the guest's driver that the display changed. Whether the picture then
/// changes is the guest's doing. The framebuffer (no 3D) has the size the firmware started with.
pub fn request_size(w: u32, h: u32) -> anyhow::Result<(u32, u32)> {
    let name = KGPU_NAME.lock().unwrap().clone().context("this machine runs without the 3D adapter: its screen keeps the size it started with")?;
    // (the device rounds the width down to 8 pixels itself; said here so the answer is what happens)
    let (w, h) = ((w.clamp(640, 3840)) & !7, h.clamp(480, 2160));
    let cname = std::ffi::CString::new(format!("/keel-gpu-{name}"))?;
    let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0) };
    ensure!(fd >= 0, "the 3D adapter's shared memory: {}", std::io::Error::last_os_error());
    let p = unsafe { libc::mmap(std::ptr::null_mut(), KGPU_PIXELS, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
    unsafe { libc::close(fd) };
    ensure!(p != libc::MAP_FAILED, "the 3D adapter's shared memory: {}", std::io::Error::last_os_error());
    let r = (|| {
        let b = p as *mut u8;
        ensure!(kgpu_u32(b, 0) == 0x3170676b, "the 3D adapter is not up yet");
        unsafe {
            // req_seq at 20, req_width at 24, req_height at 28: the size first, then the count
            (b.add(24) as *mut u32).write_volatile(w);
            (b.add(28) as *mut u32).write_volatile(h);
            (b.add(20) as *mut u32).write_volatile(kgpu_u32(b, 20).wrapping_add(1));
        }
        Ok((w, h))
    })();
    unsafe { libc::munmap(p, KGPU_PIXELS) };
    r
}

/// Can this machine's screen follow a window (see `request_size`)?
pub fn resizable() -> bool {
    KGPU_NAME.lock().unwrap().is_some()
}

/// The 3D adapter's scanout size, while its driver shows one (and this process has looked at it).
fn kgpu_size() -> Option<(u32, u32)> {
    let g = KGPU.lock().unwrap();
    let p = g.0.as_ref()?.0;
    let (active, w, h) = (kgpu_u32(p, 8), kgpu_u32(p, 12), kgpu_u32(p, 16));
    (kgpu_u32(p, 0) == 0x3170676b && active != 0 && w != 0 && h != 0).then_some((w, h))
}

/// vgpu::snapshot for this engine.
pub fn snapshot(last: Option<u64>) -> Option<(u64, u32, u32, Vec<u8>)> {
    if let Some(f) = kgpu_snapshot(last) {
        return Some(f);
    }
    let s = SCREEN.lock().unwrap();
    let s = s.as_ref()?;
    if last == Some(s.frame) {
        return Some((s.frame, s.w, s.h, Vec::new()));
    }
    Some((s.frame, s.w, s.h, s.pix.clone()))
}

pub fn size() -> Option<(u32, u32)> {
    SCREEN.lock().unwrap().as_ref().map(|s| (s.w, s.h))
}

fn send(msg: &[u8]) -> u64 {
    let mut g = INPUT.lock().unwrap();
    g.as_mut().map(|s| s.write_all(msg).is_ok() as u64).unwrap_or(0)
}

/// Linux input key code -> PC set 1 scan code, 0x80 | code for the E0-prefixed keys (what
/// QEMU's extended key event carries; bhyve's USB keyboard maps it to a HID usage).
fn linux_to_set1(code: u16) -> Option<u32> {
    Some(match code {
        1..=88 => code as u32,
        // the Japanese keys: ro, katakana/hiragana, henkan, muhenkan, yen
        89 => 0x73,
        93 => 0x70,
        92 => 0x79,
        94 => 0x7b,
        124 => 0x7d,
        96 => 0x80 | 0x1c,  // keypad Enter
        97 => 0x80 | 0x1d,  // right Ctrl
        98 => 0x80 | 0x35,  // keypad /
        99 => 0x80 | 0x37,  // Print Screen
        100 => 0x80 | 0x38, // right Alt
        102 => 0x80 | 0x47, // Home
        103 => 0x80 | 0x48, // Up
        104 => 0x80 | 0x49, // Page Up
        105 => 0x80 | 0x4b, // Left
        106 => 0x80 | 0x4d, // Right
        107 => 0x80 | 0x4f, // End
        108 => 0x80 | 0x50, // Down
        109 => 0x80 | 0x51, // Page Down
        110 => 0x80 | 0x52, // Insert
        111 => 0x80 | 0x53, // Delete
        125 => 0x80 | 0x5b, // left Windows
        126 => 0x80 | 0x5c, // right Windows
        127 => 0x80 | 0x5d, // Menu
        _ => return None,
    })
}

/// vinput::key for this engine (a Linux input key code).
pub fn key(code: u16, down: bool) -> u64 {
    let Some(sc) = linux_to_set1(code) else { return 0 };
    // QEMU extended key event: 255, 0, down (u16), keysym (u32, none), scan code (u32)
    let mut m = vec![255u8, 0, 0, down as u8, 0, 0, 0, 0];
    m.extend_from_slice(&sc.to_be_bytes());
    send(&m)
}

fn pointer(p: (u16, u16, u8)) -> u64 {
    let mut m = vec![5u8, p.2];
    m.extend_from_slice(&p.0.to_be_bytes());
    m.extend_from_slice(&p.1.to_be_bytes());
    send(&m)
}

/// vinput::abs for this engine: x, y in 0..=1 of the screen.
pub fn abs(x: f64, y: f64) -> u64 {
    let (w, h) = size().unwrap_or((1920, 1080));
    let mut p = POINTER.lock().unwrap();
    p.0 = (x.clamp(0.0, 1.0) * (w.max(2) - 1) as f64).round() as u16;
    p.1 = (y.clamp(0.0, 1.0) * (h.max(2) - 1) as f64).round() as u16;
    pointer(*p)
}

/// vinput::button for this engine: 0 left, 1 right, 2 middle.
pub fn button(b: u16, down: bool) -> u64 {
    // RFB's mask: bit 0 left, 1 middle, 2 right
    let bit = match b {
        1 => 4u8,
        2 => 2,
        _ => 1,
    };
    let mut p = POINTER.lock().unwrap();
    if down {
        p.2 |= bit;
    } else {
        p.2 &= !bit;
    }
    pointer(*p)
}

/// vinput::wheel for this engine: `steps` notches, up when positive. RFB's buttons 4 and 5 (mask
/// 8 and 16) are the wheel; bhyve's tablet turns each pointer event that has one into a report
/// with one wheel step of its own (keel/patch-usbmouse.py), so one event per notch and then the
/// buttons as they are.
///
/// The notches go out one every WHEEL_GAP, from a thread of their own. Sent all at once, a
/// Windows guest scrolled further than asked: 3 notches gave 4, 5 gave 7, while single notches
/// were always exact (2026-11-11, a terminal's lines counted). The reports that wait in the
/// tablet for the guest's next transfer are the ones that count twice; why is not known yet
/// (the xHCI's serving of queued transfers, patches/keel/0001, is the suspect). At this pace
/// each report finds the guest's transfer waiting, as a single notch does.
pub fn wheel(steps: i32) -> u64 {
    static TX: std::sync::OnceLock<std::sync::mpsc::Sender<i32>> = std::sync::OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<i32>();
        std::thread::spawn(move || {
            // what is still to send; a turn the other way cancels what is left of this one
            let mut left = 0i32;
            loop {
                if left == 0 {
                    match rx.recv() {
                        Ok(n) => left = n,
                        Err(_) => return,
                    }
                }
                while let Ok(n) = rx.try_recv() {
                    left = if (left > 0) == (n > 0) { left + n } else { n };
                }
                left = left.clamp(-WHEEL_MAX, WHEEL_MAX);
                if left == 0 {
                    continue;
                }
                let p = *POINTER.lock().unwrap();
                pointer((p.0, p.1, p.2 | if left > 0 { 8 } else { 16 }));
                left -= left.signum();
                if left == 0 {
                    let p = *POINTER.lock().unwrap();
                    pointer(p);
                }
                std::thread::sleep(WHEEL_GAP);
            }
        });
        tx
    });
    if steps != 0 && tx.send(steps).is_ok() { steps.unsigned_abs() as u64 } else { 0 }
}

/// One wheel notch to the guest at a time, this far apart (the tablet is polled every 8 ms or so).
const WHEEL_GAP: std::time::Duration = std::time::Duration::from_millis(16);
/// More notches than this waiting are not worth sending: the hand has stopped long ago.
const WHEEL_MAX: i32 = 40;

fn exact(s: &mut TcpStream, n: usize) -> std::io::Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    s.read_exact(&mut b)?;
    Ok(b)
}

fn be16(b: &[u8], o: usize) -> usize {
    u16::from_be_bytes([b[o], b[o + 1]]) as usize
}

/// One connection to bhyve's VNC server, until it ends (bhyve exited).
fn view(port: u16) -> anyhow::Result<()> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    // a new bhyve: its 3D device's shared memory is a new object
    if let Some(k) = KGPU.lock().unwrap().0.take() {
        unsafe { libc::munmap(k.0 as *mut _, KGPU_SIZE) };
    }
    s.set_nodelay(true)?;
    exact(&mut s, 12)?;
    s.write_all(b"RFB 003.008\n")?;
    let n = exact(&mut s, 1)?[0] as usize;
    ensure!(exact(&mut s, n)?.contains(&1), "bhyve's VNC wants a password");
    s.write_all(&[1])?;
    ensure!(exact(&mut s, 4)? == [0, 0, 0, 0], "VNC handshake");
    s.write_all(&[1])?; // shared
    let init = exact(&mut s, 24)?;
    let (mut w, mut h) = (be16(&init, 0), be16(&init, 2));
    let name_len = u32::from_be_bytes([init[20], init[21], init[22], init[23]]) as usize;
    exact(&mut s, name_len)?;
    // 32 bits a pixel, little endian, red at bit 16: B,G,R,X in memory, what SCREEN keeps
    s.write_all(&[0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0])?;
    // encodings: raw, DesktopSize (-223), QEMU extended key events (-258)
    let mut m = vec![2u8, 0, 0, 3];
    for e in [0i32, -223, -258] {
        m.extend_from_slice(&e.to_be_bytes());
    }
    s.write_all(&m)?;
    let resize = |w: usize, h: usize| {
        let mut g = SCREEN.lock().unwrap();
        let frame = g.as_ref().map(|s| s.frame + 1).unwrap_or(1);
        *g = Some(Screen { w: w as u32, h: h as u32, frame, pix: vec![0; w * h * 4] });
    };
    resize(w, h);
    *INPUT.lock().unwrap() = Some(s.try_clone()?);
    let request = |s: &mut TcpStream, incremental: bool, w: usize, h: usize| -> std::io::Result<()> {
        let mut m = vec![3u8, incremental as u8, 0, 0, 0, 0];
        m.extend_from_slice(&(w as u16).to_be_bytes());
        m.extend_from_slice(&(h as u16).to_be_bytes());
        s.write_all(&m)
    };
    request(&mut s, false, w, h)?;
    loop {
        match exact(&mut s, 1)?[0] {
            0 => {
                let hd = exact(&mut s, 3)?;
                let mut whole = true;
                for _ in 0..be16(&hd, 1) {
                    let r = exact(&mut s, 12)?;
                    let (x, y, rw, rh) = (be16(&r, 0), be16(&r, 2), be16(&r, 4), be16(&r, 6));
                    match i32::from_be_bytes([r[8], r[9], r[10], r[11]]) {
                        0 => {
                            let data = exact(&mut s, rw * rh * 4)?;
                            let mut g = SCREEN.lock().unwrap();
                            if let Some(sc) = g.as_mut() {
                                if x + rw <= w && y + rh <= h {
                                    for row in 0..rh {
                                        let d = ((y + row) * w + x) * 4;
                                        sc.pix[d..d + rw * 4].copy_from_slice(&data[row * rw * 4..(row + 1) * rw * 4]);
                                    }
                                    sc.frame += 1;
                                }
                            }
                        }
                        -223 => {
                            (w, h) = (rw, rh);
                            resize(w, h);
                            whole = false;
                        }
                        -258 => {}
                        e => bail!("VNC: encoding {e}?"),
                    }
                }
                // ~60 a second at most; after a new size, the whole picture again
                std::thread::sleep(Duration::from_millis(15));
                request(&mut s, whole, w, h)?;
            }
            // bell
            2 => {}
            // the server's clipboard
            3 => {
                let hd = exact(&mut s, 7)?;
                exact(&mut s, u32::from_be_bytes([hd[3], hd[4], hd[5], hd[6]]) as usize)?;
            }
            t => bail!("VNC: message {t}?"),
        }
    }
}

/// Keep a connection to bhyve's VNC server on `port` (each bhyve start is a new one).
fn viewer(port: u16) {
    std::thread::spawn(move || {
        loop {
            if let Err(e) = view(port) {
                // refused = bhyve is not up (yet, or between a reboot's two runs)
                // (the end of the stream is bhyve going away: a reboot or the power-off)
                let was_up = INPUT.lock().unwrap().take().is_some();
                let eof = e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::UnexpectedEof);
                if was_up && !eof {
                    eprintln!("display: {e:#}");
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    });
}

// ---- run ------------------------------------------------------------------------------------

static CHILD: AtomicI32 = AtomicI32::new(0);
/// ctl "pause": the bhyve process is stopped (SIGSTOP), so the guest's CPUs and devices stand
/// still and its memory stays. The guest's clock does not stand still (the virtual timer runs
/// on the Mac's): after "resume" the guest finds that time has passed, as after the Mac slept.
static PAUSED: AtomicBool = AtomicBool::new(false);
/// this bhyve run: Enter at the CD's "Press any key to boot from CD or DVD"
static PRESS_KEY: AtomicBool = AtomicBool::new(false);
static VNC_PORT: AtomicU16 = AtomicU16::new(0);

extern "C" fn on_term(_: libc::c_int) {
    // SIGTERM to the supervisor = the guest's power button, like SIGTERM to bhyve itself
    let p = CHILD.load(Ordering::Relaxed);
    if p > 0 {
        unsafe { libc::kill(p, libc::SIGTERM) };
    }
}

fn free_port() -> anyhow::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

/// A pty for the guest's serial console: (the master, the slave's path). The slave stays open
/// here so the master does not see the end between two bhyve runs.
fn pty() -> anyhow::Result<(std::fs::File, String)> {
    use std::os::fd::FromRawFd;
    let (mut m, mut s) = (0, 0);
    let mut name = [0 as libc::c_char; 128];
    let r = unsafe { libc::openpty(&mut m, &mut s, name.as_mut_ptr(), std::ptr::null_mut(), std::ptr::null_mut()) };
    ensure!(r == 0, "openpty: {}", std::io::Error::last_os_error());
    let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_string_lossy().into_owned();
    Ok((unsafe { std::fs::File::from_raw_fd(m) }, path))
}

/// A bhyve this VM left behind (its supervisor was killed): end it, two on one disk would ruin it.
fn kill_stale(dir: &Path) {
    let Some(pid) = std::fs::read_to_string(dir.join("bhyve.pid")).ok().and_then(|s| s.trim().parse::<i32>().ok()) else { return };
    let comm = Command::new("/bin/ps").args(["-p", &pid.to_string(), "-o", "comm="]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    if comm.ends_with("bhyve") {
        eprintln!("ending a bhyve left behind (pid {pid})");
        unsafe { libc::kill(pid, libc::SIGKILL) };
        std::thread::sleep(Duration::from_secs(1));
    }
    let _ = std::fs::remove_file(dir.join("bhyve.pid"));
}

fn arguments(name: &str, dir: &Path, conf: &VmConf, fw: &Path, tty: &str, fb_port: u16, iso: Option<&str>, gpu3d: bool) -> Vec<String> {
    let d = |f: &str| dir.join(f).to_string_lossy().into_owned();
    let mut a: Vec<String> = vec!["-c".into(), conf.cpus.to_string(), "-m".into(), format!("{}G", conf.mem_gb)];
    let mut o = |v: String| a.extend(["-o".into(), v]);
    o(format!("console={tty}"));
    o(format!("rundir={}", dir.display()));
    o(format!("bootrom={}", fw.display()));
    o(format!("bootvars={}", d("vars.fd")));
    if conf.os == "windows" {
        // Windows keeps the clock in local time
        o("rtc.use_localtime=true".into());
    }
    // (FreeBSD too runs with the ACPI tables: that is where the power button is, so Stop is a
    // clean shutdown. It finds no MSI controller there (with a GICv3 it knows only the ITS, and
    // Hypervisor.framework's GIC has none) and uses the devices' legacy interrupts, which the
    // NVMe disk has since patches/upstream/0022. Until 2026-11-11 FreeBSD VMs ran by the
    // device tree, where Stop cut the power; they start this way now, their disk is the same.)
    // HYPERMAC_BHYVE_O="key=value key=value": more -o options, for looking into a problem
    // (e.g. keel.xhci_trace=true: the xHCI's events and interrupts on stderr, into run.log)
    for kv in std::env::var("HYPERMAC_BHYVE_O").unwrap_or_default().split_whitespace() {
        o(kv.to_string());
    }
    let mut s = |v: String| a.extend(["-s".into(), v]);
    s("0,hostbridge".into());
    s(format!("2,nvme,{}", d("disk.img")));
    for (slot, cd) in iso.into_iter().chain(conf.drivers.as_deref()).enumerate().take(2) {
        s(format!("{},ahci-cd,{cd},ro", 3 + slot));
    }
    s("5,xhci,tablet,kbd".into());
    s("6,virtio-rnd".into());
    s(format!("7,fbuf,rfb=127.0.0.1:{fb_port},w={},h={}", conf.w.clamp(640, MAX_W), conf.h.clamp(480, MAX_H)));
    let net = match conf.net.as_deref().unwrap_or("nat") {
        "nat" => "nat".to_string(),
        n => match n.strip_prefix("bridged:") {
            Some(i) => format!("bridged,if={i}"),
            None => "bridged".into(),
        },
    };
    s(format!("8,virtio-net,{net},mac={}", conf.mac));
    if conf.os == "windows" {
        // The channel to hyperMAC's agent in the guest (tools/win-agent: coherence): a
        // virtio-console port, here a socket in the VM's folder, in Windows the port
        // \\.\hypermac.agent once the vioserial driver is there. Nothing of the network in
        // it. A guest without the driver has one device more that it has no use for.
        s(format!("{AGENT_SLOT},virtio-console,{AGENT_PORT}={}", d("agent.sock")));
    }
    if gpu3d {
        // The 3D adapter, a device of its own beside fbuf (the layout Triton's driver knows from
        // QEMU): the firmware, setup and a guest without the driver stay on fbuf; once the driver
        // shows a picture, that is what fbuf's VNC (our viewer) gets. hostmem: the window the
        // guest's processes share memory with the renderer in, a 64-bit BAR.
        s(format!("9,virtio-gpu,3d=on,hostmem=4096,w={},h={}", conf.w.clamp(640, MAX_W), conf.h.clamp(480, MAX_H)));
    }
    if hda(conf) {
        // The sound card: HD Audio, which Windows has its own driver for (hdaudio.sys). What the
        // guest plays goes into a ring in a file (keelOS's: bhyve/keel/audio.h, 16-bit stereo at
        // the guest's rate); nothing reads it yet. A slot of its own, away from the ones above,
        // so that Windows keeps the same card when those change. HYPERMAC_HDA=0: no card.
        s(format!("{HDA_SLOT},hda,play=keel:{}", d("sound.ring")));
    }
    a.push(name.into());
    a
}

/// The agent's channel: its PCI slot, and the port's name (hmagent.cs opens \\.\NAME).
const AGENT_SLOT: u32 = 10;
const AGENT_PORT: &str = "hypermac.agent";

/// The VM gets a sound card.
fn hda(conf: &VmConf) -> bool {
    conf.os == "windows" && std::env::var("HYPERMAC_HDA").as_deref() != Ok("0")
}

/// The sound card's PCI slot.
const HDA_SLOT: u32 = 16;

/// `hypermac run NAME` for a VM on this engine: until the guest powers off or `ctl NAME kill`.
/// `--install`: the first start is Windows setup from the CD (answers its "Press any key").
pub fn run(name: &str, args: &[String], conf: VmConf) -> anyhow::Result<()> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    let dir = vm_dir(name);
    let mut exe = program().context("bhyve is missing: run build.sh (it builds vm/bhyve)")?;
    // (not while Windows setup runs: it stays on the framebuffer, with nothing else to find)
    let g3 = if conf.gpu3d && !dir.join("installing").exists() { gpu3d() } else { None };
    match &g3 {
        Some(g) => {
            eprintln!("gpu3d: {}", g.bhyve.display());
            *KGPU_NAME.lock().unwrap() = Some(name.to_string());
            exe = g.bhyve.clone();
        }
        None if conf.gpu3d && !dir.join("installing").exists() => eprintln!("gpu3d: the 3D engine (bhyve-3d, gpu3d/) is not installed: starting without"),
        None => {}
    }
    let fw = firmware().context("the UEFI firmware (QEMU_EFI.fd) is missing: run build.sh")?;
    let sock = dir.join("ctl.sock");
    kill_stale(&dir);
    // (bhyve makes the agent's channel anew; one left from a machine that was killed is in its way)
    let _ = std::fs::remove_file(dir.join("agent.sock"));

    crate::no_app_nap();
    ACTIVE.store(true, Ordering::Relaxed);
    if let Err(e) = crate::shm::export(&dir) {
        eprintln!("screen.shm: {e:#}");
    }
    if hda(&conf) {
        // the guest's sound on this Mac while its window is open (sound.rs)
        crate::sound::start(&dir, || crate::console::shown() || watched());
    }
    let vnc = crate::rfb::start(num(args, "--vnc-port", 0) as u16, conf.vnc_password.clone())?;
    VNC_PORT.store(vnc, Ordering::Relaxed);
    eprintln!("console: vnc://127.0.0.1:{vnc}");
    let fb_port = free_port()?;
    viewer(fb_port);

    let (mut master, tty) = pty()?;
    let install = args.iter().any(|a| a == "--install");
    {
        // the serial console -> console.log; at an install's first start, Enter at the CD's
        // "Press any key to boot from CD or DVD" (the boot manager reads the serial console
        // there; the USB keyboard is not up yet). The prompt comes with an empty disk too
        // (tested 2026-11-11: without a key it times out and nothing boots).
        let mut log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("console.log"))?;
        let mut input = master.try_clone()?;
        std::thread::spawn(move || {
            let (mut buf, mut tail) = ([0u8; 4096], Vec::<u8>::new());
            while let Ok(n) = master.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let _ = log.write_all(&buf[..n]);
                if PRESS_KEY.load(Ordering::Relaxed) {
                    tail.extend_from_slice(&buf[..n]);
                    if tail.windows(7).any(|w| w == b"DVD ROM") {
                        PRESS_KEY.store(false, Ordering::Relaxed);
                        tail.clear();
                        let mut input = input.try_clone().unwrap();
                        std::thread::spawn(move || {
                            for _ in 0..8 {
                                std::thread::sleep(Duration::from_millis(700));
                                let _ = input.write_all(b"\r");
                            }
                        });
                    }
                    let keep = tail.len().saturating_sub(16);
                    tail.drain(..keep);
                }
            }
            let _ = input.flush();
        });
    }

    unsafe { libc::signal(libc::SIGTERM, on_term as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    {
        let (name, dir, conf, sock) = (name.to_string(), dir.clone(), conf.clone(), sock.clone());
        let iso = opt(args, "--iso").map(str::to_string).or(conf.iso.clone());
        std::thread::spawn(move || {
            let mut runs = 0;
            let code = loop {
                runs += 1;
                // Boot the installer CD (answer its "Press any key") while the disk has no
                // partitions: an unattended install's first start, and a manual install's
                // starts until setup has partitioned the disk. Looked at before every start:
                // the boot manager may reset the machine once before it gets anywhere (it
                // writes a disk signature to a new disk first; seen 2026-11-11).
                // (Windows only: FreeBSD's and Linux's CDs boot without a key, and the Enters
                // would answer their installers on the serial console)
                PRESS_KEY.store(conf.os == "windows" && iso.is_some() && ((install && runs == 1) || unpartitioned(&dir.join("disk.img"))), Ordering::Relaxed);
                let mut a = arguments(&name, &dir, &conf, &fw, &tty, fb_port, iso.as_deref(), g3.is_some());
                // Windows gets a TPM 2.0 where swtpm is installed
                // the TPM inside bhyve when it has one (its state in DIR/tpm), else an swtpm process
                let builtin = conf.os == "windows" && tpm_builtin();
                let mut tpm = if conf.os == "windows" && !builtin { start_tpm(&dir) } else { None };
                if builtin {
                    let vm_name = a.pop().unwrap_or_default();
                    a.extend(["-o".into(), "tpm.type=libtpms".into(), "-o".into(), format!("tpm.path={}", dir.join("tpm").display()), "-o".into(), "tpm.version=2.0".into(), vm_name]);
                    eprintln!("tpm: built in, state in {}", dir.join("tpm").display());
                }
                if let Some((_, sock)) = &tpm {
                    let vm_name = a.pop().unwrap_or_default();
                    a.extend(["-o".into(), "tpm.type=swtpm".into(), "-o".into(), format!("tpm.path={}", sock.display()), "-o".into(), "tpm.version=2.0".into(), vm_name]);
                    if runs == 1 {
                        eprintln!("tpm: swtpm, state in {}", dir.join("tpm").display());
                    }
                }
                let mut cmd = Command::new(&exe);
                cmd.args(&a).stdin(std::process::Stdio::null());
                if let Some(g) = &g3 {
                    g.env(&mut cmd);
                }
                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("bhyve: {e}");
                        break 1;
                    }
                };
                CHILD.store(child.id() as i32, Ordering::Relaxed);
                let _ = std::fs::write(dir.join("bhyve.pid"), child.id().to_string());
                PAUSED.store(false, Ordering::Relaxed);
                eprintln!("vm started (bhyve pid {}, run {runs})", child.id());
                let st = child.wait();
                CHILD.store(0, Ordering::Relaxed);
                if let Some((mut t, sock)) = tpm.take() {
                    let _ = t.kill();
                    let _ = t.wait();
                    let _ = std::fs::remove_file(sock);
                    let _ = std::fs::remove_file(dir.join("tpm.pid"));
                }
                match st.ok().and_then(|s| s.code()) {
                    // the guest rebooted: bhyve starts again with the same arguments
                    Some(0) => {
                        eprintln!("the guest rebooted");
                        // an install's first reboot: files copied, the rest runs from the disk
                        let mark = dir.join("installing");
                        if std::fs::read_to_string(&mark).is_ok_and(|m| m.trim() == "win:phase1") {
                            let _ = std::fs::write(&mark, "win:phase2");
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                    Some(c @ (1 | 2)) => {
                        eprintln!("vm stopped (the guest {})", if c == 1 { "powered off" } else { "halted" });
                        break 0;
                    }
                    Some(c) => {
                        eprintln!("bhyve failed (exit status {c})");
                        break 1;
                    }
                    None => {
                        eprintln!("bhyve was killed");
                        break 1;
                    }
                }
            };
            let _ = std::fs::remove_file(dir.join("bhyve.pid"));
            let _ = std::fs::remove_file(&sock);
            std::process::exit(code);
        });
    }

    let l = std::os::unix::net::UnixListener::bind(&sock)?;
    let title = format!("{name} — hyperMAC");
    if args.iter().any(|a| a == "--console") {
        crate::console::open(&title);
    }
    let handle = std::sync::Arc::new(move |c: std::os::unix::net::UnixStream| {
        let Ok(mut w) = c.try_clone() else { return };
        for line in BufReader::new(c).lines().map_while(Result::ok) {
            let r = match line.trim() {
                l if l.starts_with("vkey ") || l.starts_with("vabs ") || l.starts_with("vbtn ") || l.starts_with("vwheel ") => {
                    let a: Vec<&str> = l.split_whitespace().collect();
                    let f = |i: usize| a.get(i).and_then(|x| x.parse::<f64>().ok());
                    let sent = match (a[0], f(1), f(2)) {
                        ("vwheel", Some(n), _) => Some(wheel(n as i32)),
                        ("vkey", Some(k), Some(d)) => Some(key(k as u16, d != 0.0)),
                        ("vabs", Some(x), Some(y)) => Some(abs(x, y)),
                        ("vbtn", Some(b), Some(d)) => Some(button(b as u16, d != 0.0)),
                        _ => None,
                    };
                    match sent {
                        Some(n) => serde_json::json!({"ok": true, "sent": n}),
                        None => serde_json::json!({"error": "vkey CODE 1|0 | vabs X Y | vbtn B 1|0 | vwheel STEPS"}),
                    }
                }
                // the picture as DIR/vscreen.bmp
                "vshot" => match shot(&dir.join("vscreen.bmp")) {
                    Ok(v) => v,
                    Err(e) => serde_json::json!({"error": e.to_string()}),
                },
                "kill" => {
                    let p = CHILD.load(Ordering::Relaxed);
                    if p > 0 {
                        unsafe { libc::kill(p, libc::SIGKILL) };
                    }
                    stop_tpm(&dir);
                    let _ = std::fs::remove_file(dir.join("bhyve.pid"));
                    let _ = std::fs::remove_file(&sock);
                    std::process::exit(0);
                }
                cmd @ ("pause" | "resume") => {
                    let (p, pause) = (CHILD.load(Ordering::Relaxed), cmd == "pause");
                    if p > 0 && unsafe { libc::kill(p, if pause { libc::SIGSTOP } else { libc::SIGCONT }) } == 0 {
                        PAUSED.store(pause, Ordering::Relaxed);
                        eprintln!("vm {}", if pause { "paused" } else { "resumed" });
                        serde_json::json!({"ok": true})
                    } else {
                        serde_json::json!({"error": "bhyve is not running"})
                    }
                }
                // "viewer 1|0": the main program's window process (src/vmwin.rs) shows this
                // machine on the Mac (said every second while it does; believed for five)
                l if l.starts_with("viewer") => {
                    let until = if l.ends_with('1') { now_ms() + 5000 } else { 0 };
                    VIEWER.store(until, Ordering::Relaxed);
                    serde_json::json!({"ok": true})
                }
                // each of the guest's windows as a window of the Mac (coherence.rs)
                l if l.starts_with("coherence") => crate::coherence::ctl(l, Some(&dir.join("agent.sock")), crate::lease(&conf.mac), conf.agent.as_deref()),
                // "size W H": another screen size, where the guest can follow (request_size)
                l if l.starts_with("size ") => {
                    let a: Vec<u32> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
                    match a.as_slice() {
                        [w, h] => match request_size(*w, *h) {
                            Ok((w, h)) => serde_json::json!({"ok": true, "w": w, "h": h}),
                            Err(e) => serde_json::json!({"error": format!("{e:#}")}),
                        },
                        _ => serde_json::json!({"error": "size W H"}),
                    }
                }
                "suspend" => serde_json::json!({"error": "suspending to a file is not there yet for machines on the bhyve engine: pause keeps one in memory"}),
                // the ACPI power button; the process ends when the guest is down
                "stop" => {
                    // a stopped process takes no power button
                    if PAUSED.swap(false, Ordering::Relaxed) {
                        let p = CHILD.load(Ordering::Relaxed);
                        if p > 0 {
                            unsafe { libc::kill(p, libc::SIGCONT) };
                        }
                    }
                    on_term(0);
                    serde_json::json!({"ok": true})
                }
                "console" => {
                    let title = title.clone();
                    dispatch2::DispatchQueue::main().exec_async(move || crate::console::open(&title));
                    serde_json::json!({"ok": true})
                }
                // our VNC server in the Mac's Screen Sharing
                "vnc" => {
                    let url = format!("vnc://:{}@127.0.0.1:{}", conf.vnc_password, VNC_PORT.load(Ordering::Relaxed));
                    let _ = Command::new("/usr/bin/open").arg(&url).status();
                    serde_json::json!({"ok": true})
                }
                _ => serde_json::json!({
                    "pid": std::process::id(),
                    "bhyve_pid": CHILD.load(Ordering::Relaxed),
                    "engine": "bhyve",
                    "os": conf.os,
                    "vnc_port": VNC_PORT.load(Ordering::Relaxed),
                    "vnc_password": conf.vnc_password,
                    "mac": conf.mac,
                    "net": conf.net.clone().unwrap_or("nat".into()),
                    "ip": crate::lease(&conf.mac),
                    "screen": kgpu_size().or_else(size).map(|(w, h)| format!("{w}x{h}")),
                    "resizable": resizable(),
                    "paused": PAUSED.load(Ordering::Relaxed),
                    "can_suspend": false,
                }),
            };
            if writeln!(w, "{r}").is_err() {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let h = handle.clone();
            std::thread::spawn(move || h(c));
        }
    });
    // an app without a Dock icon: it has a window only while the console is open
    let mtm = MainThreadMarker::new().context("not on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    // (`--console` has opened the window by now, and made this a program with a Dock icon)
    if !crate::console::shown() {
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
    app.run();
    Ok(())
}

/// No partitions on the disk: no GPT header and an empty MBR partition table. (Not "all
/// zero": Windows' boot manager writes a disk signature to a new disk the first time it sees it.)
fn unpartitioned(disk: &Path) -> bool {
    let mut b = [0u8; 1024];
    std::fs::File::open(disk).and_then(|mut f| f.read_exact(&mut b)).is_ok() && &b[512..520] != b"EFI PART" && b[446..510].iter().all(|x| *x == 0)
}

/// The picture as a 32-bit BMP (ctl "vshot").
fn shot(path: &Path) -> anyhow::Result<serde_json::Value> {
    let (frame, w, h, pix) = snapshot(None).context("no picture yet")?;
    let mut f = Vec::with_capacity(54 + pix.len());
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&((54 + pix.len()) as u32).to_le_bytes());
    f.extend_from_slice(&[0; 4]);
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&(w as i32).to_le_bytes());
    f.extend_from_slice(&(-(h as i32)).to_le_bytes()); // top row first
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&32u16.to_le_bytes());
    f.extend_from_slice(&[0; 24]);
    f.extend_from_slice(&pix);
    std::fs::write(path, f)?;
    Ok(serde_json::json!({"ok": true, "w": w, "h": h, "frame": frame, "file": path}))
}

// ---- create: Windows 11 ARM from Microsoft's ISO --------------------------------------------

/// The newest stable virtio-win.iso (the Fedora project's build of the virtio drivers for
/// Windows: the network card's, and what else `DRIVERS` names).
const VIRTIO_URL: &str = "https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/virtio-win.iso";

/// A virtio-win ISO in the storage folder's iso/, downloaded when none is there (about 840 MB).
/// `progress`: the folder of the VM being created (its `installing` file gets "dlv:PERCENT").
pub fn fetch_virtio(progress: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = virtio_here() {
        return Ok(p);
    }
    // The stable address names the current version by a redirect, and that redirect goes to
    // plain HTTP. Only the name is taken from it: the file itself is asked for over HTTPS.
    let out = Command::new("/usr/bin/curl").args(["-sIL", "--max-time", "30", "-o", "/dev/null", "-w", "%{url_effective}", VIRTIO_URL]).output().context("curl")?;
    let found = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let url = match found.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => found,
    };
    ensure!(url.starts_with("https://fedorapeople.org/") && url.ends_with(".iso"), "virtio-win.iso: {VIRTIO_URL} leads to {url:?}; is this Mac online?");
    let file = url.rsplit('/').next().unwrap_or("virtio-win.iso").to_string();
    eprintln!("virtio-win: {url}");
    crate::vm::fetch(&url, &file, progress.map(|d| (d, "dlv")), true)
}

/// The virtio-win ISO in the storage folder's iso/, if one is there (the newest by name).
fn virtio_here() -> Option<PathBuf> {
    let iso = crate::storage().join("iso");
    let mut here: Vec<PathBuf> = std::fs::read_dir(&iso)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| {
            let n = n.to_string_lossy().to_lowercase();
            n.starts_with("virtio-win") && n.ends_with(".iso")
        }))
        .collect();
    here.sort();
    here.pop()
}

/// `hypermac create NAME --os windows --iso WINDOWS.iso --virtio virtio-win.iso --user U`
/// (the password in HYPERMAC_WIN_PASSWORD) on this engine.
pub fn create_windows(name: &str, args: &[String]) -> anyhow::Result<()> {
    let iso = PathBuf::from(opt(args, "--iso").context("--iso PATH (Microsoft's Windows 11 ARM ISO)")?);
    // no --virtio (or "download"): the one in the storage folder's iso/, downloaded when none is there
    let virtio = opt(args, "--virtio").filter(|v| *v != "download").map(PathBuf::from);
    // --manual: setup asks its questions in the console; else the account for the unattended install
    let account = if args.iter().any(|a| a == "--manual") {
        None
    } else {
        let user = opt(args, "--user").unwrap_or("win").to_string();
        let password = std::env::var("HYPERMAC_WIN_PASSWORD").ok().filter(|p| !p.is_empty()).context("HYPERMAC_WIN_PASSWORD: the account's password (or --manual)")?;
        ensure!(ws::valid_user(&user), "{user:?}: letters, digits, - _ . only (20 at most)");
        Some((user, password))
    };
    ensure!(iso.exists(), "{}: no such file", iso.display());
    if let Some(v) = &virtio {
        ensure!(v.exists(), "{}: no such file", v.display());
    }
    program().context("bhyve is missing: run build.sh (it builds vm/bhyve)")?;
    firmware().context("the UEFI firmware (QEMU_EFI.fd) is missing: run build.sh")?;

    // --gpu3d: Direct3D on the Mac's GPU (vm.json "gpu3d"), where the 3D engine is installed
    let want3d = args.iter().any(|a| a == "--gpu3d");
    if want3d {
        let g = gpu3d().context("--gpu3d: the 3D engine (bhyve-3d and gpu3d/ beside it) is not installed")?;
        if g.driver.is_none() {
            eprintln!("gpu3d: no driver for the guest (gpu3d/driver/viogpu3d.inf): install Triton's viogpu3d in Windows yourself");
        }
    }
    let dir = vm_dir(name);
    ensure!(!dir.join("vm.json").exists(), "VM {name:?} exists ({})", dir.display());
    std::fs::create_dir_all(&dir)?;
    ws::mark(&dir, "win:stick");
    // vm.json now, so the window lists the VM (with the progress) from the start
    let mut cargs: Vec<String> = args.to_vec();
    ws::set_arg(&mut cargs, "--os", "windows");
    crate::vm::create_linux(name, &cargs)?;
    let setup = dir.join("setup.iso");
    ws::edit_conf(name, |c| {
        c.engine = Some("bhyve".into());
        // hyperMAC's RDP shows VMs with this set (screen.shm, ctl.sock's vkey / vabs / vbtn)
        c.own_devices = true;
        c.drivers = Some(setup.to_string_lossy().into_owned());
        c.w = c.w.clamp(640, MAX_W);
        c.h = c.h.clamp(480, MAX_H);
        c.os_version.get_or_insert_with(|| "11".into());
        c.gpu3d = want3d;
    })?;
    // every new Windows machine gets the agent (coherence.rs) with its setup
    let token = agent_token()?;
    ws::edit_conf(name, |c| c.agent = Some(token))?;
    // Secure Boot: the VM's UEFI variables start as the template with the keys in it
    let secure_boot = match vars_template() {
        Some(t) => std::fs::copy(&t, dir.join("vars.fd")).is_ok(),
        None => false,
    };
    let disk = std::fs::metadata(dir.join("disk.img")).map(|m| m.len()).unwrap_or(0);
    let virtio = match virtio {
        Some(v) => v,
        None => {
            ws::mark(&dir, "dlv:0");
            match fetch_virtio(Some(&dir)) {
                Ok(v) => v,
                // deleted in the window meanwhile: nothing to mark
                Err(e) if !dir.exists() => return Err(e),
                Err(e) => {
                    ws::fail(&dir, &format!("{e:#}"));
                    return Err(e);
                }
            }
        }
    };
    ws::mark(&dir, "win:stick");
    let lab = labconfig(&crate::load(name)?, disk, secure_boot);
    eprintln!("Secure Boot: {}; setup's hardware checks: {}", if secure_boot { "on" } else { "off (no key template)" }, if lab.is_empty() { "on" } else { "switched off" });
    if let Err(e) = setup_cd(name, &iso, &virtio, account.as_ref(), &lab, &setup) {
        ws::fail(&dir, &format!("{e:#}"));
        return Err(e);
    }
    if account.is_none() {
        // like a Linux VM from here: Start it, install in the Console, eject the installer
        let _ = std::fs::remove_file(dir.join("installing"));
        eprintln!("created. Next: hypermac run {name} --console, and install Windows there");
        return Ok(());
    }
    install(name, &dir)
}

/// What the driver CD's install.cmd is (Windows: run as administrator).
const GPU3D_CMD: &str = "@echo off\r\nrem hyperMAC: the 3D adapter's driver (Triton, viogpu3d). Run as administrator.\r\npnputil /add-driver \"%~dp0viogpu3d\\*.inf\" /subdirs /install\r\npause\r\n";

/// `hypermac gpu3d NAME on|off`: 3D for a Windows VM that was made without it (the window's
/// Edit… does this). On: vm.json "gpu3d", and a CD with the guest's driver (gpu3d-driver.iso in
/// the VM's folder; volume HMGPU3D) plugged in at the next starts: in Windows, run its
/// install.cmd as administrator once. Until the driver is in, Windows stays on the plain
/// framebuffer. Off: the adapter and the CD go.
pub fn set_gpu3d(name: &str, on: bool) -> anyhow::Result<()> {
    let conf = crate::load(name)?;
    let dir = vm_dir(name);
    ensure!(is(&conf) && conf.os == "windows", "{name}: 3D acceleration is for Windows machines on the bhyve engine");
    ensure!(std::os::unix::net::UnixStream::connect(dir.join("ctl.sock")).is_err(), "{name} is running: shut it down first");
    let cd = dir.join("gpu3d-driver.iso");
    let cd_s = cd.to_string_lossy().into_owned();
    if !on {
        ws::edit_conf(name, |c| {
            c.gpu3d = false;
            if c.iso.as_deref() == Some(&cd_s) {
                c.iso = None;
            }
            if c.drivers.as_deref() == Some(&cd_s) {
                c.drivers = None;
            }
        })?;
        let _ = std::fs::remove_file(&cd);
        return Ok(());
    }
    let g = gpu3d().context("the 3D engine (bhyve-3d and gpu3d/ beside it) is not installed")?;
    let Some(driver) = g.driver else {
        // no driver here to hand over: the adapter alone, the driver is the user's to find
        eprintln!("gpu3d: no driver for the guest (gpu3d/driver/viogpu3d.inf): install Triton's viogpu3d in Windows yourself");
        return ws::edit_conf(name, |c| c.gpu3d = true);
    };
    let tmp = std::env::temp_dir().join(format!("hypermac-g3-{}", std::process::id()));
    let stage = tmp.join("cd");
    let made = (|| {
        ws::copy_tree(&driver, &stage.join("viogpu3d"), &|_| false)?;
        std::fs::write(stage.join("install.cmd"), GPU3D_CMD)?;
        let _ = std::fs::remove_file(&cd);
        ws::run(Command::new("/usr/bin/hdiutil").args(["makehybrid", "-quiet", "-iso", "-joliet", "-default-volume-name", "HMGPU3D", "-o"]).arg(&cd).arg(&stage))?;
        anyhow::Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    made?;
    ws::edit_conf(name, |c| {
        c.gpu3d = true;
        plug_cd(c, &cd_s);
    })
}

/// Plug a CD of hyperMAC's own into one of the machine's two CD drives: the one that is free
/// (an installer still plugged in keeps its own; another of hyperMAC's CDs gives way).
fn plug_cd(c: &mut VmConf, cd: &str) {
    let ours = |p: &str| p.ends_with("/gpu3d-driver.iso") || p.ends_with("/agent.iso");
    if c.iso.as_deref() == Some(cd) || c.drivers.as_deref() == Some(cd) {
    } else if c.drivers.as_deref().is_none_or(|d| !Path::new(d).exists()) {
        c.drivers = Some(cd.to_string());
    } else if c.iso.is_none() {
        c.iso = Some(cd.to_string());
    } else if c.drivers.as_deref().is_some_and(ours) {
        c.drivers = Some(cd.to_string());
    } else {
        eprintln!("both CD drives are taken: eject the installer, then plug in {cd}");
    }
}

const AGENT_CS: &str = include_str!("../tools/win-agent/hmagent.cs");
const AGENT_CMD: &str = include_str!("../tools/win-agent/install.cmd");

/// A new token for a machine's agent.
fn agent_token() -> anyhow::Result<String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// The agent's files into `dir`: its source, the script that compiles and installs it, the token.
fn agent_files(dir: &Path, token: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("hmagent.cs"), AGENT_CS)?;
    // (cmd.exe reads its scripts with CR LF line ends)
    std::fs::write(dir.join("install.cmd"), AGENT_CMD.replace("\r\n", "\n").replace('\n', "\r\n"))?;
    std::fs::write(dir.join("agent.token"), token)?;
    Ok(())
}

/// `hypermac agent NAME`: a CD with the agent that coherence needs (coherence.rs), plugged in
/// at the next starts: agent.iso in the VM's folder, volume HMAGENT, with the agent's source,
/// install.cmd (run it in Windows as administrator, once: it compiles and installs the agent)
/// and the token the agent will ask of the host. The token is made once and kept in vm.json.
pub fn agent_cd(name: &str) -> anyhow::Result<()> {
    let conf = crate::load(name)?;
    let dir = vm_dir(name);
    ensure!(is(&conf) && conf.os == "windows", "{name}: the agent is for Windows machines on the bhyve engine");
    let token = match conf.agent.clone() {
        Some(t) => t,
        None => agent_token()?,
    };
    let cd = dir.join("agent.iso");
    let tmp = std::env::temp_dir().join(format!("hypermac-agent-{}", std::process::id()));
    let stage = tmp.join("cd");
    let made = (|| {
        agent_files(&stage, &token)?;
        // the driver of the agent's channel (AGENT_PORT), from the virtio-win ISO a Windows
        // machine was made with: install.cmd adds it. Without the ISO the agent is reached
        // over the network as before. (Not downloaded for this: 840 MB behind a button.)
        match virtio_here().context("no virtio-win ISO in the storage folder's iso/").and_then(|iso| {
            let m = ws::Mounted::iso(&iso, &tmp)?;
            let from = m.path.join("vioserial/w11/ARM64");
            ensure!(from.exists(), "{}: no vioserial/w11/ARM64", iso.display());
            ws::copy_tree(&from, &stage.join("vioserial"), &|p| p.extension().is_some_and(|e| e == "pdb"))
        }) {
            Ok(_) => {}
            Err(e) => eprintln!("the agent's channel driver is not on the CD ({e:#}): the agent will be reached over the network"),
        }
        let _ = std::fs::remove_file(&cd);
        ws::run(Command::new("/usr/bin/hdiutil").args(["makehybrid", "-quiet", "-iso", "-joliet", "-default-volume-name", "HMAGENT", "-o"]).arg(&cd).arg(&stage))?;
        anyhow::Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    made?;
    let running = std::os::unix::net::UnixStream::connect(dir.join("ctl.sock")).is_ok();
    let cd_s = cd.to_string_lossy().into_owned();
    ws::edit_conf(name, |c| {
        c.agent = Some(token.clone());
        plug_cd(c, &cd_s);
    })?;
    eprintln!("{}: the CD HMAGENT is plugged in{}. In Windows, run its install.cmd as administrator.", name, if running { " from the next start" } else { "" });
    Ok(())
}

/// setup.iso: autounattend.xml (setup looks for it on every drive), hypermac.cmd, the drivers.
/// `account`: the unattended install's user and password; None = the manual install's file.
fn setup_cd(name: &str, iso: &Path, virtio: &Path, account: Option<&(String, String)>, lab: &str, out: &Path) -> anyhow::Result<()> {
    // a VM with "gpu3d": the 3D adapter's driver goes with the others (hypermac.cmd installs
    // every .inf under \drivers); Windows binds it when it first sees the adapter
    let driver3d = if crate::load(name)?.gpu3d { gpu3d().and_then(|g| g.driver) } else { None };
    let tmp = std::env::temp_dir().join(format!("hypermac-win-{}", std::process::id()));
    let stage = tmp.join("cd");
    std::fs::create_dir_all(&stage)?;
    let r = (|| {
        let lang = {
            let m = ws::Mounted::iso(iso, &tmp)?;
            ensure!(ws::find_ci(&m.path, &["efi", "boot", "bootaa64.efi"]).is_some(), "the ISO has no efi/boot/bootaa64.efi: not a Windows ARM64 installer?");
            ws::lang_of(&m.path).unwrap_or_else(|| "en-US".into())
        };
        eprintln!("Windows ISO: language {lang}");
        let xml = match account {
            Some((user, password)) => AUTOUNATTEND
                .replace("{{LABCONFIG}}", lab)
                .replace("{{LANG}}", &lang)
                .replace("{{COMPUTER}}", &ws::computer_name(name))
                .replace("{{TIMEZONE}}", &ws::time_zone())
                .replace("{{USER}}", &ws::xml_escape(user))
                .replace("{{PASSWORD}}", &ws::xml_escape(password)),
            None if lab.is_empty() => AUTOUNATTEND_MANUAL.replace("{{LABCONFIG_PASS}}", ""),
            None => AUTOUNATTEND_MANUAL.replace(
                "{{LABCONFIG_PASS}}",
                &format!("  <settings pass=\"windowsPE\">\n    <component name=\"Microsoft-Windows-Setup\" processorArchitecture=\"arm64\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\">\n{lab}    </component>\n  </settings>\n"),
            ),
        };
        std::fs::write(stage.join("autounattend.xml"), xml)?;
        std::fs::write(stage.join("hypermac.cmd"), HYPERMAC_CMD)?;
        let m = ws::Mounted::iso(virtio, &tmp)?;
        for d in DRIVERS {
            let from = m.path.join(d).join("w11/ARM64");
            ensure!(from.exists(), "{}: no {d}/w11/ARM64: not virtio-win.iso?", virtio.display());
            ws::copy_tree(&from, &stage.join("drivers").join(d), &|_| false)?;
        }
        if let Some(d) = &driver3d {
            ws::copy_tree(d, &stage.join("drivers").join("viogpu3d"), &|_| false)?;
            eprintln!("gpu3d: the guest's driver from {}", d.display());
        }
        // the agent for coherence (coherence.rs): hypermac.cmd installs it during setup
        if let Some(token) = crate::load(name)?.agent {
            agent_files(&stage.join("agent"), &token)?;
        }
        let _ = std::fs::remove_file(out);
        ws::run(Command::new("/usr/bin/hdiutil").args(["makehybrid", "-quiet", "-iso", "-joliet", "-default-volume-name", "HMSETUP", "-o"]).arg(out).arg(&stage))?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    r
}

/// Run setup to the first desktop (`hypermac run NAME --install`: one process through all of
/// setup's reboots, until the guest powers off), then start the VM for good.
fn install(name: &str, dir: &Path) -> anyhow::Result<()> {
    ws::mark(dir, "win:phase1");
    ws::run_vm(name, &["--install"])?;
    if !dir.exists() {
        return Ok(()); // deleted meanwhile
    }
    if !done(dir) {
        let why = "setup did not finish (the VM stopped before the first logon)";
        ws::fail(dir, why);
        bail!(why);
    }
    ws::edit_conf(name, |c| {
        c.iso = None;
        c.drivers = None;
    })?;
    let _ = std::fs::remove_file(dir.join("setup.iso"));
    let _ = std::fs::remove_file(dir.join("installing"));
    eprintln!("Windows is installed; starting {name}");
    ws::start_detached(name)
}

/// `hypermac win-resume NAME` on this engine: setup again from where the disk is (a start
/// from the disk if the boot manager is on it already, else from the CD).
pub fn resume(name: &str) -> anyhow::Result<()> {
    let dir = vm_dir(name);
    ensure!(dir.join("setup.iso").exists(), "{name}: no Windows install to go on with");
    install(name, &dir)
}

/// The first logon's mark, C:\hypermac-done.txt (hypermac.cmd): on one of the disk's volumes
/// (macOS mounts NTFS read only).
fn done(dir: &Path) -> bool {
    let Ok(out) = ws::run(Command::new("/usr/bin/hdiutil").args(["attach", "-imagekey", "diskimage-class=CRawDiskImage", "-nobrowse", "-readonly"]).arg(dir.join("disk.img"))) else {
        return false;
    };
    let found = out
        .lines()
        .filter_map(|l| l.split('\t').map(str::trim).filter(|c| c.starts_with('/') && !c.starts_with("/dev/")).last())
        .any(|mp| ws::find_ci(Path::new(mp), &["hypermac-done.txt"]).is_some());
    if let Some(dev) = out.split_whitespace().next() {
        ws::detach(dev);
    }
    found
}
