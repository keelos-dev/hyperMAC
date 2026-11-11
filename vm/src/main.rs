//! hypermac: macOS (and later Linux) guests on an Apple Silicon Mac, headless.
//!
//!   hypermac create NAME --ipsw PATH [--cpus N] [--mem-gb G] [--disk-gb G] [--w W --h H]
//!       make a macOS VM and install macOS from the IPSW (takes 10-20 minutes)
//!   hypermac create NAME --os linux [--iso PATH] [--os-version V] [...]
//!       make a Linux VM (boots the ISO as a USB stick: install from there)
//!   hypermac create NAME --os freebsd --iso PATH [...]
//!       make a FreeBSD VM on the bhyve engine (boots the ISO as a CD: install from there)
//!   hypermac create NAME --os windows --iso WIN11.iso --virtio virtio-win.iso --user U [...]
//!       make a Windows 11 VM and install it unattended (password in HYPERMAC_WIN_PASSWORD). On
//!       bhyve (bhyve.rs) when it is installed; --engine vz for the old way (winsetup.rs)
//!       --manual instead of --user: only make the VM; install Windows yourself in its console
//!   hypermac win-resume NAME             go on with a Windows install that stopped
//!   hypermac run NAME [--vnc-port P] [--console]   run it; no window unless --console / ctl console.
//!                                         For other machines: VNC on port P
//!   hypermac ctl NAME info|console|stop|kill   talk to a running VM (ctl.sock); console = open its window
//!   hypermac ip NAME                      the guest's address (the host's DHCP leases)
//!   hypermac net NAME nat|bridged[:IF]    NAT (default) or on the host's LAN (vmnet.rs)
//!   sudo hypermac netd-install            the root helper bridged networking needs
//!   hypermac list
//!
//! A VM is a directory, <storage>/guests/NAME (storage: settings.json "vm_dir", else ~/Library/Application Support/hyperMAC):
//!   vm.json, disk.img (sparse raw), aux.img, hardware-model, machine-id, ctl.sock, run.log

mod bhyve;
mod console;
mod fbdev;
mod rfb;
mod shm;
mod sound;
mod vgpu;
mod vinput;
mod vm;
mod vmnet;
mod vnc;
mod winsetup;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VmConf {
    pub os: String,
    pub cpus: usize,
    pub mem_gb: u64,
    pub w: isize,
    pub h: isize,
    /// the NIC's MAC address: fixed, so the host's DHCP gives the guest the same address
    pub mac: String,
    /// VNC console password (RFB's own authentication: 8 characters count)
    pub vnc_password: String,
    /// Linux: the install ISO, attached as a USB stick at every start until this is removed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso: Option<String>,
    /// Windows: a driver ISO (virtio-win: the virtio NIC's driver), a second USB stick
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drivers: Option<String>,
    /// "nat" (default: 192.168.64.x behind the host) or "bridged" / "bridged:en0" (on the
    /// host's LAN with its own address; needs the root helper, `sudo hypermac netd-install`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<String>,
    /// our own virtio-gpu / virtio-input and VNC server (vgpu.rs, vinput.rs, rfb.rs) instead of
    /// VZ's graphics and USB HID; HYPERMAC_VIRTIO=1 turns it on for one run
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub own_devices: bool,
    /// the guest system's version for kRDPclient's list ("26.5.2"; /v1/vms os.version): a macOS
    /// VM's comes from its IPSW at create; others are set by hand (Windows without one: "11")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    /// what runs the VM: absent = Virtualization.framework; "bhyve" = vm/bhyve on
    /// Hypervisor.framework (bhyve.rs), which is what runs Windows
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// bhyve engine, Windows: Direct3D on the Mac's GPU. A second display adapter (virtio-gpu
    /// with 3D, `-s 9`) that the guest's Triton driver (viogpu3d) takes; the calls are executed
    /// by virglrenderer + DXMT on Metal (bhyve.rs `gpu3d`). Without the 3D engine installed
    /// the VM starts as before, on the framebuffer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gpu3d: bool,
}

impl VmConf {
    pub fn own_devices(&self) -> bool {
        self.own_devices || std::env::var_os("HYPERMAC_VIRTIO").is_some()
    }
}

/// A VM's process is not to be napped: without a window in front macOS (App Nap) lowers its
/// priority and stretches its timers, and the picture it hands on (shm.rs, rfb.rs, the console)
/// fell from 56 to 13 frames a second some minutes after the start (2026-11-11). The Mac may
/// still go to sleep when idle.
pub fn no_app_nap() {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    // NSActivityUserInitiatedAllowingIdleSystemSleep | NSActivityLatencyCritical
    let options: u64 = 0x00EF_FFFF | 0xFF_0000_0000;
    let reason = objc2_foundation::NSString::from_str("a virtual machine is running");
    let token: Option<Retained<AnyObject>> = unsafe {
        let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
        msg_send![info, beginActivityWithOptions: options, reason: &*reason]
    };
    std::mem::forget(token); // for as long as the process lives
}

pub fn root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::env::var_os("HYPERMAC_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join("Library/Application Support/hyperMAC"))
}

/// Where the VMs and the installers are kept, laid out as keelOS lays out its dataset:
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
pub fn guests() -> PathBuf {
    let s = storage();
    let g = s.join("guests");
    if !g.exists() && s.join("vms").is_dir() { s.join("vms") } else { g }
}

pub fn vm_dir(name: &str) -> PathBuf {
    guests().join(name)
}

pub fn load(name: &str) -> anyhow::Result<VmConf> {
    let p = vm_dir(name).join("vm.json");
    Ok(serde_json::from_slice(&std::fs::read(&p).with_context(|| format!("no VM {name:?} ({})", p.display()))?)?)
}

pub fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

pub fn num(args: &[String], name: &str, default: u64) -> u64 {
    opt(args, name).and_then(|s| s.parse().ok()).unwrap_or(default)
}

/// As many open files as the system lets a process have, for this process and the ones it starts
/// (bhyve and the 3D renderer inherit it). Started from the app, a process has launchd's limit of
/// 256; the 3D engine holds a few descriptors for every guest process that draws (shared memory,
/// pipes, sockets) and reached it eight minutes after Windows was up, with Edge open: no new
/// context or surface could be made, and the picture fell back to the firmware's (2026-11-11).
/// Started from a terminal the limit was a million, so it had not shown before.
fn raise_open_files() {
    unsafe {
        let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut l) != 0 {
            return;
        }
        // (macOS refuses a soft limit above kern.maxfilesperproc, even when the hard one is "unlimited")
        let mut per_proc: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        let name = c"kern.maxfilesperproc";
        if libc::sysctlbyname(name.as_ptr(), (&mut per_proc as *mut libc::c_int).cast(), &mut len, std::ptr::null_mut(), 0) != 0 || per_proc <= 0 {
            per_proc = 10240;
        }
        let want = (per_proc as libc::rlim_t).min(l.rlim_max);
        if want > l.rlim_cur {
            let was = l.rlim_cur;
            l.rlim_cur = want;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &l) != 0 {
                eprintln!("open files: the limit stays at {was} ({})", std::io::Error::last_os_error());
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    raise_open_files();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let name = || args.get(1).cloned().context("VM name?");
    match args.first().map(String::as_str) {
        Some("create") => vm::create(&name()?, &args[2..]),
        Some("win-resume") => winsetup::resume(&name()?),
        Some("run") => vm::run(&name()?, &args[2..]),
        Some("ctl") => ctl(&name()?, args.get(2).map(String::as_str).unwrap_or("info")),
        Some("net") => {
            // hypermac net NAME nat|bridged[:IF]
            let n = name()?;
            let mut c = load(&n)?;
            let v = args.get(2).context("nat | bridged[:IF]")?;
            anyhow::ensure!(v == "nat" || v == "bridged" || v.starts_with("bridged:"), "nat | bridged[:IF]");
            c.net = if v == "nat" { None } else { Some(v.clone()) };
            std::fs::write(vm_dir(&n).join("vm.json"), serde_json::to_vec_pretty(&c)?)?;
            println!("{n}: {v} (from the next start)");
            Ok(())
        }
        Some("vmnet-test") => {
            // can this user start a bridged vmnet interface at all? (root can)
            let i = args.get(1).cloned().or_else(vmnet::default_interface).context("interface?")?;
            vmnet::bridge(&i).map(|_| println!("bridged vmnet on {i}: ok"))
        }
        Some("netd") => vmnet::netd(),
        Some("netd-install") => vmnet::netd_install(),
        Some("ip") => {
            let n = name()?;
            let c = load(&n)?;
            let ip = if c.net.is_none() {
                guest_ip(&c)
            } else {
                // bridged: the running VM process knows
                let mut s = std::os::unix::net::UnixStream::connect(vm_dir(&n).join("ctl.sock")).context("not running")?;
                writeln!(s, "info")?;
                let mut line = String::new();
                BufReader::new(s).read_line(&mut line)?;
                serde_json::from_str::<serde_json::Value>(&line)?.get("ip").and_then(|v| v.as_str()).map(str::to_string)
            };
            println!("{}", ip.context("no address yet (is the guest up?)")?);
            Ok(())
        }
        Some("list") => {
            for e in std::fs::read_dir(guests()).into_iter().flatten().flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                let Ok(c) = load(&n) else { continue };
                let up = std::os::unix::net::UnixStream::connect(e.path().join("ctl.sock")).is_ok();
                println!("{n}\t{}\t{} cpu\t{} GB\t{}", c.os, c.cpus, c.mem_gb, if up { "running" } else { "stopped" });
            }
            Ok(())
        }
        _ => anyhow::bail!("usage: hypermac create NAME --ipsw PATH [...] | create NAME --os linux --iso PATH [...] | run NAME [--vnc-port P] | ctl NAME info|stop|kill|pause|resume|suspend | ip NAME | net NAME nat|bridged[:IF] | list | serve [--listen ADDR:PORT] | token new [NAME]|list|revoke ID | netd-install (sudo)"),
    }
}

fn ctl(name: &str, cmd: &str) -> anyhow::Result<()> {
    let mut s = std::os::unix::net::UnixStream::connect(vm_dir(name).join("ctl.sock")).context("not running")?;
    writeln!(s, "{cmd}")?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    print!("{line}");
    Ok(())
}

/// One ctl.sock request to a running VM, its JSON answer (the window uses it; 1 s at most).
pub fn ctl_request(name: &str, cmd: &str) -> anyhow::Result<serde_json::Value> {
    let mut s = std::os::unix::net::UnixStream::connect(vm_dir(name).join("ctl.sock")).context("not running")?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
    writeln!(s, "{cmd}")?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

/// The address the host's DHCP server (vmnet's bootpd) leased to this MAC address.
/// /var/db/dhcpd_leases writes each octet without the leading zero ("a:1b:..."), so compare by value.
pub fn lease(mac: &str) -> Option<String> {
    let want: Vec<u8> = mac.split(':').filter_map(|x| u8::from_str_radix(x, 16).ok()).collect();
    let text = std::fs::read_to_string("/var/db/dhcpd_leases").ok()?;
    let (mut ip, mut found) = (None, None);
    for l in text.lines().map(str::trim) {
        if l == "{" {
            ip = None;
        } else if let Some(v) = l.strip_prefix("ip_address=") {
            ip = Some(v.to_string());
        } else if let Some(v) = l.strip_prefix("hw_address=") {
            // "1,a:1b:2c:..."
            let m: Vec<u8> =
                v.split(',').nth(1).unwrap_or("").split(':').filter_map(|x| u8::from_str_radix(x, 16).ok()).collect();
            if m == want && ip.is_some() {
                found = ip.clone(); // later entries are newer
            }
        }
    }
    found
}

/// The guest's IPv4 address. NAT: the host's DHCP lease. Bridged: the LAN's DHCP server gave
/// it, and only the running VM process knows (it sees the guest's frames, vmnet.rs).
pub fn guest_ip(c: &VmConf) -> Option<String> {
    if c.net.as_deref().unwrap_or("nat") == "nat" {
        return lease(&c.mac);
    }
    match vmnet::GUEST_IP.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        ip => Some(std::net::Ipv4Addr::from(ip).to_string()),
    }
}

#[cfg(test)]
mod limit_tests {
    /// From launchd's 256 the limit goes up to what the system allows a process.
    #[test]
    fn open_files_limit_is_raised() {
        unsafe {
            let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut l), 0);
            l.rlim_cur = 256;
            assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &l), 0);
            super::raise_open_files();
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut l), 0);
            assert!(l.rlim_cur >= 10240, "{}", l.rlim_cur);
        }
    }
}
