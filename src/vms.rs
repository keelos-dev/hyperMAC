//! hyperMAC's virtual machines on this Mac (user 2026-11-10: hyperMAC and hyperMAC become one,
//! named hyperMAC; hyperMAC's features are the base, the VMs are added: "如果有虚拟机，多显示一台").
//!
//! The VMs themselves run in hyperMAC's own processes (`hypermac run NAME`, one per VM; the
//! repository keelos-dev/hyperMAC). Here only what kRDPclient needs to open one like a guest of
//! a keelOS host:
//! - the list (web.rs's /v1/vms): <hyperMAC>/vms/*/vm.json, running = its ctl.sock answers;
//! - the picture: a VM on hyperMAC's own virtio-gpu (vm.json "own_devices") exports its frames
//!   to <VM dir>/screen.shm (hyperMAC src/shm.rs; the layout is repeated below);
//! - the input: vkey / vabs / vbtn lines to <VM dir>/ctl.sock (hyperMAC src/vinput.rs; on a
//!   Windows guest clicks go to the pen, hovering does not move the pointer).
//! The RDP login picks the VM by user name (keel-rdp's way) and needs a token, as kRDPclient
//! sends for a keel host's guest.

use std::io::{BufRead, BufReader, Write};
use std::num::{NonZeroU16, NonZeroUsize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, KeyboardEvent, MouseButton, MouseEvent, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler, ServerResult,
};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

/// The VM a connection logged in to (its name); None: this Mac's own desktop.
pub type Slot = Arc<Mutex<Option<String>>>;

pub fn root() -> PathBuf {
    if let Some(s) = std::env::var_os("HYPERMAC_STATE") {
        return PathBuf::from(s);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Library/Application Support/hyperMAC")
}

/// (The same as vm/src/main.rs.) Where the VMs and the installers are kept, laid out as keelOS lays out its dataset:
/// `guests/NAME/` (one folder per VM), `iso/` (installers), `images/` (kept for disk images).
/// The folder is settings.json's "vm_dir" (the window's Virtual Machines tab sets it), else the
/// state folder. The sockets of a running VM are in its folder, so the path must stay short
/// (a Unix socket's path is at most 103 bytes).
pub fn storage() -> PathBuf {
    let state = root();
    std::fs::read(state.join("settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("vm_dir").and_then(|d| d.as_str()).map(PathBuf::from))
        .filter(|p| p.is_absolute())
        .unwrap_or(state)
}

/// `guests/` in it. A folder from before this layout has its VMs in `vms/`: used as it is
/// until someone renames it.
pub fn guests_dir() -> PathBuf {
    let s = storage();
    let g = s.join("guests");
    if !g.exists() && s.join("vms").is_dir() { s.join("vms") } else { g }
}

pub fn vm_dir(name: &str) -> PathBuf {
    dir(name)
}

fn dir(name: &str) -> PathBuf {
    guests_dir().join(name)
}

pub struct Vm {
    pub name: String,
    /// "macos", "linux", "windows", "freebsd"
    pub os: String,
    pub running: bool,
    /// on hyperMAC's own virtio-gpu / virtio-input: the only kind this RDP can show
    pub own_devices: bool,
    /// vm.json's os_version ("" when not known)
    pub os_version: String,
}

/// hyperMAC's VMs (none when hyperMAC is not there).
pub fn list() -> Vec<Vm> {
    let mut v: Vec<Vm> = std::fs::read_dir(guests_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let conf: serde_json::Value = serde_json::from_slice(&std::fs::read(e.path().join("vm.json")).ok()?).ok()?;
            Some(Vm {
                os: conf.get("os").and_then(|o| o.as_str()).unwrap_or("linux").to_string(),
                own_devices: conf.get("own_devices").and_then(|o| o.as_bool()).unwrap_or(false),
                os_version: conf.get("os_version").and_then(|o| o.as_str()).unwrap_or("").to_string(),
                running: std::os::unix::net::UnixStream::connect(e.path().join("ctl.sock")).is_ok(),
                name,
            })
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// The same UUID hyperMAC gives the VM (hyperMAC src/serve.rs uuid_of).
pub fn uuid_of(name: &str) -> String {
    let h = Sha256::digest(format!("hypermac:{name}").as_bytes());
    let mut b: [u8; 16] = h[..16].try_into().unwrap();
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x: String = b.iter().map(|v| format!("{v:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &x[0..8], &x[8..12], &x[12..16], &x[16..20], &x[20..32])
}

/// The keel-style entries for web.rs's /v1/vms.
pub fn guests(rdp_port: u16) -> Vec<serde_json::Value> {
    list()
        .into_iter()
        .map(|vm| {
            let (family, os_name, default_version) = match vm.os.as_str() {
                "macos" => ("macos", "macOS", ""),
                // Windows 11 ARM is the only Windows hyperMAC runs
                "windows" => ("windows", "Windows", "11"),
                "freebsd" => ("freebsd", "FreeBSD", ""),
                _ => ("linux", "Linux", ""),
            };
            let version = if vm.os_version.is_empty() { default_version } else { vm.os_version.as_str() };
            let mut g = serde_json::json!({
                "uuid": uuid_of(&vm.name),
                "alias": vm.name,
                "state": if vm.running { "running" } else { "stopped" },
                "os": { "family": family, "name": os_name, "version": version },
            });
            if vm.running && vm.own_devices {
                g["rdp"] = serde_json::json!({ "port": rdp_port });
            }
            g
        })
        .collect()
}

/// The VM a login's user name names, if it is one this RDP can show.
pub fn openable(user: &str) -> Option<Result<String, &'static str>> {
    let vm = list().into_iter().find(|v| v.name.eq_ignore_ascii_case(user))?;
    Some(if !vm.running {
        Err("the VM is not running")
    } else if !vm.own_devices {
        Err("the VM is not on hyperMAC's own display")
    } else {
        Ok(vm.name)
    })
}

// ---- screen.shm (hyperMAC src/shm.rs) ---------------------------------------------------------
// 4096-byte header: 0 magic "HMSCRN01", 8 seq (odd while written), 16 w, 20 h, 24 wanted-until
// (ms since the epoch, set by the reader), 32 frame; then B,G,R,X pixels, room for 3840x2160.

const HEADER: usize = 4096;
const MAX_PIXELS: usize = 3840 * 2160;
const SIZE: usize = HEADER + MAX_PIXELS * 4;
const MAGIC: u64 = u64::from_le_bytes(*b"HMSCRN01");

struct Shm {
    p: *mut u8,
}
unsafe impl Send for Shm {}

impl Shm {
    fn open(name: &str) -> Option<Shm> {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(dir(name).join("screen.shm")).ok()?;
        if f.metadata().ok()?.len() < SIZE as u64 {
            return None;
        }
        use std::os::fd::AsRawFd;
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), SIZE, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, f.as_raw_fd(), 0)
        };
        if p == libc::MAP_FAILED {
            return None;
        }
        let s = Shm { p: p as *mut u8 };
        (unsafe { (s.p as *const u64).read_volatile() } == MAGIC).then_some(s)
    }

    fn a64(&self, off: usize) -> &AtomicU64 {
        unsafe { &*(self.p.add(off) as *const AtomicU64) }
    }

    fn want(&self, ms: u64) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        self.a64(24).store(now + ms, Ordering::Relaxed);
    }

    fn peek(&self) -> (u64, u32, u32) {
        let seq = self.a64(8).load(Ordering::Acquire);
        unsafe { (seq, (self.p.add(16) as *const u32).read_volatile(), (self.p.add(20) as *const u32).read_volatile()) }
    }

    fn read(&self, last: u64) -> Option<(u64, u32, u32, Vec<u8>)> {
        for _ in 0..4 {
            let (seq, w, h) = self.peek();
            if seq == last || seq % 2 == 1 || w == 0 || h == 0 || (w as usize * h as usize) > MAX_PIXELS {
                return None;
            }
            let n = w as usize * h as usize * 4;
            let mut v = vec![0u8; n];
            unsafe { std::ptr::copy_nonoverlapping(self.p.add(HEADER), v.as_mut_ptr(), n) };
            if self.a64(8).load(Ordering::Acquire) == seq {
                return Some((seq, w, h, v));
            }
        }
        None
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.p as *mut _, SIZE) };
    }
}

// ---- the VM's display -----------------------------------------------------------------------

pub struct VmDisplay {
    slot: Slot,
}

impl VmDisplay {
    pub fn new(slot: Slot) -> Self {
        Self { slot }
    }
}

#[async_trait]
impl RdpServerDisplay for VmDisplay {
    async fn size(&mut self) -> DesktopSize {
        let name = self.slot.lock().unwrap().clone();
        if let Some(s) = name.as_deref().and_then(Shm::open) {
            s.want(3000);
            for _ in 0..30 {
                let (_, w, h) = s.peek();
                if w > 0 && h > 0 {
                    return DesktopSize { width: w as u16, height: h as u16 };
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        DesktopSize { width: 1920, height: 1080 }
    }

    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let name = self.slot.lock().unwrap().clone();
        // a thread copies frames out of the shared file (~60 Hz poll) for this connection
        std::thread::spawn(move || {
            let Some(s) = name.as_deref().and_then(Shm::open) else {
                return warn!(vm = name, "no screen.shm");
            };
            let (mut last, mut size) = (0u64, (0u32, 0u32));
            loop {
                s.want(2000);
                if let Some((seq, w, h, pix)) = s.read(last) {
                    last = seq;
                    if size != (w, h) && size != (0, 0) {
                        if tx.blocking_send(DisplayUpdate::Resize(DesktopSize { width: w as u16, height: h as u16 })).is_err() {
                            return;
                        }
                    }
                    size = (w, h);
                    let (Some(wn), Some(hn), Some(st)) =
                        (NonZeroU16::new(w as u16), NonZeroU16::new(h as u16), NonZeroUsize::new(w as usize * 4))
                    else {
                        continue;
                    };
                    let u = DisplayUpdate::Bitmap(BitmapUpdate {
                        x: 0,
                        y: 0,
                        width: wn,
                        height: hn,
                        format: PixelFormat::BgrX32,
                        data: bytes::Bytes::from(pix),
                        stride: st,
                    });
                    if tx.blocking_send(u).is_err() {
                        return; // the connection is gone
                    }
                }
                std::thread::sleep(Duration::from_millis(16));
            }
        });
        Ok(Box::new(VmUpdates { rx }))
    }
}

struct VmUpdates {
    rx: tokio::sync::mpsc::Receiver<DisplayUpdate>,
}

#[async_trait]
impl RdpServerDisplayUpdates for VmUpdates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        Ok(self.rx.recv().await)
    }
}

// ---- the VM's input -------------------------------------------------------------------------

pub struct VmInput {
    slot: Slot,
    ctl: Option<std::os::unix::net::UnixStream>,
    size: (u32, u32),
}

impl VmInput {
    pub fn new(slot: Slot) -> Self {
        Self { slot, ctl: None, size: (1920, 1080) }
    }

    /// One line to the VM's ctl.sock (kept open for the session; a thread drops its answers).
    fn send(&mut self, line: &str) {
        for _ in 0..2 {
            if self.ctl.is_none() {
                let Some(name) = self.slot.lock().unwrap().clone() else { return };
                let Ok(c) = std::os::unix::net::UnixStream::connect(dir(&name).join("ctl.sock")) else { return };
                if let Ok(r) = c.try_clone() {
                    std::thread::spawn(move || for _ in BufReader::new(r).lines() {});
                }
                if let Some(s) = Shm::open(&name) {
                    let (_, w, h) = s.peek();
                    if w > 0 && h > 0 {
                        self.size = (w, h);
                    }
                }
                self.ctl = Some(c);
            }
            if let Some(c) = self.ctl.as_mut() {
                if writeln!(c, "{line}").is_ok() {
                    return;
                }
            }
            self.ctl = None;
        }
    }

    fn at(&mut self, x: u16, y: u16) {
        let (w, h) = self.size;
        let fx = x as f64 / (w.max(2) - 1) as f64;
        let fy = y as f64 / (h.max(2) - 1) as f64;
        self.send(&format!("vabs {fx:.6} {fy:.6}"));
    }
}

impl Drop for VmInput {
    fn drop(&mut self) {
        if let Some(c) = &self.ctl {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl RdpServerInputHandler for VmInput {
    fn keyboard(&mut self, ev: KeyboardEvent) {
        let (code, ext, down) = match ev {
            KeyboardEvent::Pressed { code, extended } => (code, extended, true),
            KeyboardEvent::Released { code, extended } => (code, extended, false),
            _ => return,
        };
        if let Some(k) = scancode_to_linux(code, ext) {
            self.send(&format!("vkey {k} {}", down as u8));
        }
    }

    fn mouse(&mut self, ev: MouseEvent) {
        match ev {
            MouseEvent::Move { x, y } => self.at(x, y),
            MouseEvent::Button { x, y, button, pressed } => {
                self.at(x, y);
                let b = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    _ => return,
                };
                self.send(&format!("vbtn {b} {}", pressed as u8));
            }
            _ => {}
        }
    }
}

/// RDP (PC/AT set 1) scan code -> Linux input key code: the same numbers for the main block;
/// the E0-prefixed keys and the Japanese ones from a table.
fn scancode_to_linux(code: u8, extended: bool) -> Option<u16> {
    if extended {
        return Some(match code {
            0x1c => 96,
            0x1d => 97,
            0x35 => 98,
            0x37 => 99,
            0x38 => 100,
            0x47 => 102,
            0x48 => 103,
            0x49 => 104,
            0x4b => 105,
            0x4d => 106,
            0x4f => 107,
            0x50 => 108,
            0x51 => 109,
            0x52 => 110,
            0x53 => 111,
            0x5b => 125,
            0x5c => 126,
            0x5d => 127,
            0x1f => 122,
            0x1e => 123,
            _ => return None,
        });
    }
    Some(match code {
        0x01..=0x58 => code as u16,
        0x70 => 93,
        0x73 => 89,
        0x79 => 92,
        0x7b => 94,
        0x7d => 124,
        0x72 | 0xf2 => 122,
        0x71 | 0xf1 => 123,
        _ => return None,
    })
}

// ---- one connection: this Mac or a VM, decided by the login ---------------------------------

/// The display of a connection: the VM's when the login named one, else this Mac's.
pub struct Display<M> {
    pub slot: Slot,
    pub mac: M,
    pub vm: VmDisplay,
    /// set once the session turns out to be on a VM (status.json's vm_sessions)
    pub on_vm: Option<crate::shutdown::OnVm>,
}

#[async_trait]
impl<M: RdpServerDisplay + Send> RdpServerDisplay for Display<M> {
    async fn size(&mut self) -> DesktopSize {
        if self.slot.lock().unwrap().is_some() { self.vm.size().await } else { self.mac.size().await }
    }

    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        let vm = self.slot.lock().unwrap().clone();
        match vm {
            Some(name) => {
                info!(vm = name, "session on a VM");
                self.on_vm.get_or_insert_with(crate::shutdown::OnVm::new);
                self.vm.updates().await
            }
            None => self.mac.updates().await,
        }
    }
}

/// The input of a connection: to the VM when the login named one, else to this Mac.
pub struct Input<M> {
    pub slot: Slot,
    pub mac: M,
    pub vm: VmInput,
}

impl<M: RdpServerInputHandler> RdpServerInputHandler for Input<M> {
    fn keyboard(&mut self, ev: KeyboardEvent) {
        if self.slot.lock().unwrap().is_some() { self.vm.keyboard(ev) } else { self.mac.keyboard(ev) }
    }

    fn mouse(&mut self, ev: MouseEvent) {
        if self.slot.lock().unwrap().is_some() { self.vm.mouse(ev) } else { self.mac.mouse(ev) }
    }
}
