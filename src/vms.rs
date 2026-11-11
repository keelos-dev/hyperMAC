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
            } else if vm.running {
                // a macOS guest with hyperMAC inside, paired in the window: the client goes there
                if let Some(d) = direct(&vm.name) {
                    g["rdp_direct"] = d;
                }
            }
            g
        })
        .collect()
}

// ---- a guest that serves its own desktop --------------------------------------------------------
// A macOS guest's picture cannot be taken on the host (Apple's graphics device shows only in a
// visible window), so hyperMAC is installed in the guest too and serves the guest's desktop
// itself. For the client to open it from this host's list, the host is paired with the guest
// once (the window: the guest's account and password, traded for a token of the guest's
// hyperMAC, kept in the VM's folder) and then publishes, in the guest's /v1/vms entry:
//
//   "rdp_direct": { "host": "192.168.100.42", "port": 3389, "token": "keel_…", "cert_sha256": "…" }
//
// host = the guest's address now (it must be one the client reaches: bridged networking);
// token = the RDP password (any user name; the client's usual one, the VM's name, is fine);
// cert_sha256 = SHA-256 of the guest's TLS certificate (DER), the one its RDP port presents too.
// "rdp" stays absent, so a client that does not know rdp_direct says "no RDP" as before.

const PAIRING: &str = "guest-rdp.json";

fn ctl_info(name: &str) -> Option<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    let mut s = std::os::unix::net::UnixStream::connect(dir(name).join("ctl.sock")).ok()?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(1))).ok()?;
    writeln!(s, "info").ok()?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn guest_ip(name: &str) -> Option<String> {
    ctl_info(name)?.get("ip")?.as_str().map(str::to_owned)
}

pub fn paired(name: &str) -> bool {
    dir(name).join(PAIRING).exists()
}

pub fn unpair(name: &str) {
    let _ = std::fs::remove_file(dir(name).join(PAIRING));
}

fn direct(name: &str) -> Option<serde_json::Value> {
    let p: serde_json::Value = serde_json::from_slice(&std::fs::read(dir(name).join(PAIRING)).ok()?).ok()?;
    let ip = guest_ip(name)?;
    Some(serde_json::json!({
        "host": ip,
        "port": p.get("rdp_port").and_then(|v| v.as_u64()).unwrap_or(3389),
        "token": p.get("token")?.as_str()?,
        "cert_sha256": p.get("cert_sha256").and_then(|v| v.as_str()).unwrap_or(""),
    }))
}

/// Pair with the hyperMAC inside the running guest `name`: log in to its API with the guest's
/// account (the password goes to curl on stdin, never into an argument), ask it for a token,
/// note its certificate's fingerprint, and keep both in the VM's folder (0600).
pub fn pair(name: &str, user: &str, password: &str) -> anyhow::Result<()> {
    use anyhow::{Context, bail, ensure};
    use std::io::Write;
    use std::process::{Command, Stdio};
    let ip = guest_ip(name).context("the virtual machine has no address yet")?;
    let base = format!("https://{ip}:443");
    // the certificate is the guest's own (self-signed): -k here, its fingerprint kept for the client
    let pem = Command::new("/usr/bin/openssl").args(["s_client", "-connect", &format!("{ip}:443")]).stdin(Stdio::null()).stderr(Stdio::null()).output()?;
    ensure!(!pem.stdout.is_empty(), "no answer from {ip}:443 — is hyperMAC installed and running in the virtual machine?");
    let mut fp = Command::new("/usr/bin/openssl").args(["x509", "-noout", "-fingerprint", "-sha256"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    fp.stdin.take().context("openssl")?.write_all(&pem.stdout)?;
    let fp = String::from_utf8_lossy(&fp.wait_with_output()?.stdout).to_string();
    let cert: String = fp.rsplit('=').next().unwrap_or("").chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_lowercase();
    ensure!(cert.len() == 64, "could not read the certificate of {ip}:443");

    let mut login = Command::new("/usr/bin/curl")
        .args(["-sk", "--max-time", "15", "-i", "-H", "Content-Type: application/json", "--data-binary", "@-", &format!("{base}/login")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    login.stdin.take().context("curl")?.write_all(serde_json::json!({"user": user, "password": password}).to_string().as_bytes())?;
    let out = String::from_utf8_lossy(&login.wait_with_output()?.stdout).to_string();
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let cookie = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
        .find_map(|(_, v)| v.trim().split(';').next().filter(|c| c.starts_with("keel_access=")).map(str::to_owned));
    let csrf = serde_json::from_str::<serde_json::Value>(body.trim()).ok().and_then(|v| v.get("csrf").and_then(|c| c.as_str()).map(str::to_owned));
    let (Some(cookie), Some(csrf)) = (cookie, csrf) else {
        if head.starts_with("HTTP/1.1 401") {
            bail!("the virtual machine's hyperMAC refused that user name and password");
        }
        bail!("the hyperMAC in the virtual machine did not log us in ({})", head.lines().next().unwrap_or("no answer"));
    };
    let host = std::process::Command::new("/bin/hostname").arg("-s").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let made = Command::new("/usr/bin/curl")
        .args(["-sk", "--max-time", "15", "-H", "Content-Type: application/json", "-H"])
        .arg(format!("Cookie: {cookie}"))
        .arg("-H")
        .arg(format!("X-Keel-CSRF: {csrf}"))
        .arg("--data-binary")
        .arg(serde_json::json!({"name": format!("host {host}")}).to_string())
        .arg(format!("{base}/v1/tokens"))
        .output()?;
    let token = serde_json::from_slice::<serde_json::Value>(&made.stdout)
        .ok()
        .and_then(|v| v.get("secret").and_then(|s| s.as_str()).map(str::to_owned))
        .context("the hyperMAC in the virtual machine gave no token")?;
    let path = dir(name).join(PAIRING);
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&serde_json::json!({"token": token, "cert_sha256": cert, "rdp_port": 3389}))?)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
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
