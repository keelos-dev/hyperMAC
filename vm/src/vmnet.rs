//! Bridged networking: the guest sits on the Mac's own LAN with its own MAC and address.
//!
//! Virtualization.framework's bridged attachment needs the com.apple.vm.networking
//! entitlement, which Apple grants per developer account (an ad-hoc signed binary carrying it
//! is killed at launch). vmnet.framework's bridged mode needs no entitlement but must be
//! started by root. So: `hypermac netd` runs as root (a LaunchDaemon), starts a vmnet
//! interface bridged to a host interface on request, and hands the VM process one end of a
//! datagram socketpair (VZFileHandleNetworkDeviceAttachment takes it): one Ethernet frame
//! per datagram, relayed to and from vmnet here.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc;

use anyhow::{Context, anyhow};
use block2::RcBlock;

#[repr(C)]
struct VmPktDesc {
    size: usize,
    iov: *mut libc::iovec,
    iovcnt: u32,
    flags: u32,
}

type Xpc = *mut c_void;
type Queue = *mut c_void;
type Iface = *mut c_void;

const VMNET_SUCCESS: u32 = 1000;
const VMNET_BRIDGED_MODE: u64 = 1002;
const VMNET_INTERFACE_PACKETS_AVAILABLE: u32 = 1;
const MAX_PKT: usize = 1 << 16;
const BATCH: usize = 32;

#[link(name = "vmnet", kind = "framework")]
unsafe extern "C" {
    static vmnet_operation_mode_key: *const c_char;
    static vmnet_shared_interface_name_key: *const c_char;
    static vmnet_max_packet_size_key: *const c_char;
    fn vmnet_start_interface(desc: Xpc, q: Queue, handler: &block2::DynBlock<dyn Fn(u32, Xpc)>) -> Iface;
    fn vmnet_interface_set_event_callback(i: Iface, mask: u32, q: Queue, handler: &block2::DynBlock<dyn Fn(u32, Xpc)>) -> u32;
    fn vmnet_read(i: Iface, pkts: *mut VmPktDesc, count: *mut c_int) -> u32;
    fn vmnet_write(i: Iface, pkts: *mut VmPktDesc, count: *mut c_int) -> u32;
}

unsafe extern "C" {
    fn xpc_dictionary_create(keys: *const *const c_char, values: *const Xpc, count: usize) -> Xpc;
    fn xpc_dictionary_set_uint64(d: Xpc, key: *const c_char, v: u64);
    fn xpc_dictionary_set_string(d: Xpc, key: *const c_char, v: *const c_char);
    fn xpc_dictionary_get_uint64(d: Xpc, key: *const c_char) -> u64;
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> Queue;
}

/// The guest's IPv4 address, learned from the frames it sends (big-endian; 0 = not seen yet).
/// Asking the host's ARP table does not work: a process without the Local Network grant gets it empty.
pub static GUEST_IP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The sender's address of an ARP packet or the source of an IPv4 packet in an Ethernet frame.
fn sender_ip(f: &[u8]) -> Option<u32> {
    let ip = match (f.get(12)?, f.get(13)?) {
        (0x08, 0x06) => f.get(28..32)?,
        (0x08, 0x00) => f.get(26..30)?,
        _ => return None,
    };
    let ip = u32::from_be_bytes(ip.try_into().ok()?);
    // not 0.0.0.0 (DHCP discover) and not link-local 169.254/16
    (ip != 0 && ip >> 16 != 0xa9fe).then_some(ip)
}

/// Counters of the relay, for `ctl NAME info` ("the guest's network went quiet": which side?).
pub static TO_GUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static TO_GUEST_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static TO_GUEST_ERRNO: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
pub static TO_GUEST_MAX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static FROM_GUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static WRITE_FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct Ptr(*mut c_void);
unsafe impl Send for Ptr {}
unsafe impl Sync for Ptr {}

pub fn status_name(s: u32) -> &'static str {
    match s {
        1000 => "success",
        1001 => "failure (not root, or no such interface?)",
        1002 => "out of memory",
        1003 => "invalid argument",
        1004 => "setup incomplete",
        1005 => "permission denied",
        1006 => "packet too big",
        1007 => "buffer exhausted",
        1008 => "too many packets",
        1009 => "sharing service busy",
        1010 => "not authorized",
        _ => "?",
    }
}

/// Start a vmnet interface bridged to `host_if` (e.g. "en0") and relay it to a datagram
/// socketpair; returns the end for the VM. The relay lives as long as the process and stops
/// by itself when the VM's end is closed.
pub fn bridge(host_if: &str) -> anyhow::Result<OwnedFd> {
    let name = CString::new(host_if)?;
    let (tx, rx) = mpsc::channel::<(u32, u64)>();
    let iface = unsafe {
        let d = xpc_dictionary_create(std::ptr::null(), std::ptr::null(), 0);
        xpc_dictionary_set_uint64(d, vmnet_operation_mode_key, VMNET_BRIDGED_MODE);
        xpc_dictionary_set_string(d, vmnet_shared_interface_name_key, name.as_ptr());
        let q = dispatch_queue_create(c"hypermac.vmnet.start".as_ptr(), std::ptr::null());
        let started = RcBlock::new(move |status: u32, params: Xpc| {
            let max = if params.is_null() { 0 } else { xpc_dictionary_get_uint64(params, vmnet_max_packet_size_key) };
            let _ = tx.send((status, max));
        });
        vmnet_start_interface(d, q, &started)
    };
    anyhow::ensure!(!iface.is_null(), "vmnet_start_interface: null (bridged mode needs root)");
    let (status, max) = rx.recv_timeout(std::time::Duration::from_secs(10)).context("vmnet did not answer")?;
    anyhow::ensure!(status == VMNET_SUCCESS, "vmnet bridged on {host_if}: {} ({status})", status_name(status));
    let max = if max == 0 { 1514 } else { max as usize }.min(MAX_PKT);

    let mut sv = [0 as RawFd; 2];
    anyhow::ensure!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sv.as_mut_ptr()) } == 0, "socketpair");
    for &fd in &sv {
        // room for bursts in both directions (Apple's advice for the file-handle attachment:
        // the receive buffer several times the send buffer)
        for (opt, size) in [(libc::SO_SNDBUF, 1i32 << 20), (libc::SO_RCVBUF, 4i32 << 20)] {
            unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, opt, &size as *const _ as *const c_void, 4) };
        }
    }
    let (ours, theirs) = (sv[0], sv[1]);
    let iface = std::sync::Arc::new(Ptr(iface));

    // vmnet -> VM: on "packets available", read batches and send each frame
    unsafe {
        let q = dispatch_queue_create(c"hypermac.vmnet.rx".as_ptr(), std::ptr::null());
        let i2 = iface.clone();
        let on_packets = RcBlock::new(move |_ev: u32, _params: Xpc| {
            let mut bufs = vec![vec![0u8; max]; BATCH];
            loop {
                let mut iovs: Vec<libc::iovec> =
                    bufs.iter_mut().map(|b| libc::iovec { iov_base: b.as_mut_ptr() as *mut c_void, iov_len: max }).collect();
                let mut pkts: Vec<VmPktDesc> =
                    iovs.iter_mut().map(|v| VmPktDesc { size: max, iov: v, iovcnt: 1, flags: 0 }).collect();
                let mut n = BATCH as c_int;
                if vmnet_read(i2.0, pkts.as_mut_ptr(), &mut n) != VMNET_SUCCESS || n <= 0 {
                    break;
                }
                for (p, b) in pkts.iter().zip(&bufs).take(n as usize) {
                    // a full socket drops the frame, like a full NIC queue
                    use std::sync::atomic::Ordering::Relaxed;
                    TO_GUEST_MAX.fetch_max(p.size as u64, Relaxed);
                    if libc::send(ours, b.as_ptr() as *const c_void, p.size, libc::MSG_DONTWAIT) < 0 {
                        TO_GUEST_DROPPED.fetch_add(1, Relaxed);
                        TO_GUEST_ERRNO.store(std::io::Error::last_os_error().raw_os_error().unwrap_or(0), Relaxed);
                    } else {
                        TO_GUEST.fetch_add(1, Relaxed);
                    }
                }
                if (n as usize) < BATCH {
                    break;
                }
            }
        });
        let s = vmnet_interface_set_event_callback(iface.0, VMNET_INTERFACE_PACKETS_AVAILABLE, q, &on_packets);
        anyhow::ensure!(s == VMNET_SUCCESS, "vmnet event callback: {}", status_name(s));
        std::mem::forget(on_packets);
    }

    // VM -> vmnet
    std::thread::spawn(move || {
        let mut buf = vec![0u8; max];
        loop {
            let n = unsafe { libc::recv(ours, buf.as_mut_ptr() as *mut c_void, max, 0) };
            if n <= 0 {
                break; // the VM's end is closed
            }
            if let Some(ip) = sender_ip(&buf[..n as usize]) {
                GUEST_IP.store(ip, std::sync::atomic::Ordering::Relaxed);
            }
            let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut c_void, iov_len: n as usize };
            let mut pkt = VmPktDesc { size: n as usize, iov: &mut iov, iovcnt: 1, flags: 0 };
            let mut one: c_int = 1;
            use std::sync::atomic::Ordering::Relaxed;
            FROM_GUEST.fetch_add(1, Relaxed);
            if unsafe { vmnet_write(iface.0, &mut pkt, &mut one) } != VMNET_SUCCESS {
                WRITE_FAILED.fetch_add(1, Relaxed);
            }
        }
    });
    Ok(unsafe { OwnedFd::from_raw_fd(theirs) })
}

/// The interface the default route goes through ("en0").
pub fn default_interface() -> Option<String> {
    let out = std::process::Command::new("/sbin/route").args(["-n", "get", "default"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.trim().strip_prefix("interface: ").map(str::to_string))
}

pub const NETD_SOCK: &str = "/var/run/hypermac-netd.sock";

/// `hypermac netd`: the root helper. One connection = one bridged interface: the client sends
/// the host interface's name and a newline, gets "ok\n" with the socket (SCM_RIGHTS) or
/// "error: ...\n". The bridge goes away when the VM closes its end.
pub fn netd() -> anyhow::Result<()> {
    anyhow::ensure!(unsafe { libc::geteuid() } == 0, "netd must run as root (vmnet's bridged mode)");
    let _ = std::fs::remove_file(NETD_SOCK);
    let l = std::os::unix::net::UnixListener::bind(NETD_SOCK)?;
    // any local user may ask for a bridge: the same people who could plug a machine into this LAN
    unsafe { libc::chmod(CString::new(NETD_SOCK)?.as_ptr(), 0o666) };
    eprintln!("netd: listening on {NETD_SOCK}");
    for c in l.incoming().flatten() {
        std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            use std::os::fd::AsRawFd;
            let mut line = String::new();
            if std::io::BufReader::new(&c).read_line(&mut line).is_err() {
                return;
            }
            let host_if = line.trim().to_string();
            let mut c = c;
            if host_if.is_empty() || !host_if.chars().all(|ch| ch.is_ascii_alphanumeric()) {
                let _ = writeln!(c, "error: interface name?");
                return;
            }
            match bridge(&host_if) {
                Ok(fd) => {
                    eprintln!("netd: bridge on {host_if}");
                    if let Err(e) = send_fd(c.as_raw_fd(), fd.as_raw_fd(), b"ok\n") {
                        eprintln!("netd: {e:#}");
                    }
                    // our copy of the VM's end closes here; the VM holds the other reference
                }
                Err(e) => {
                    eprintln!("netd: {host_if}: {e:#}");
                    let _ = writeln!(c, "error: {e:#}");
                }
            }
        });
    }
    Ok(())
}

/// In the VM process: ask netd for a bridge on `host_if`.
pub fn request(host_if: &str) -> anyhow::Result<OwnedFd> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let mut c = std::os::unix::net::UnixStream::connect(NETD_SOCK)
        .with_context(|| format!("bridged networking needs the root helper: sudo hypermac netd-install ({NETD_SOCK})"))?;
    writeln!(c, "{host_if}")?;
    let (text, fd) = recv_fd(c.as_raw_fd())?;
    match fd {
        Some(fd) if text.starts_with("ok") => Ok(fd),
        _ => Err(anyhow!("netd: {}", text.trim())),
    }
}

fn send_fd(sock: RawFd, fd: RawFd, text: &[u8]) -> anyhow::Result<()> {
    unsafe {
        let mut iov = libc::iovec { iov_base: text.as_ptr() as *mut c_void, iov_len: text.len() };
        let mut cbuf = [0u8; 64];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut c_void;
        msg.msg_controllen = libc::CMSG_SPACE(4) as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(4) as _;
        std::ptr::copy_nonoverlapping(&fd as *const RawFd as *const u8, libc::CMSG_DATA(c), 4);
        anyhow::ensure!(libc::sendmsg(sock, &msg, 0) >= 0, "sendmsg: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn recv_fd(sock: RawFd) -> anyhow::Result<(String, Option<OwnedFd>)> {
    unsafe {
        let mut buf = [0u8; 512];
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut c_void, iov_len: buf.len() };
        let mut cbuf = [0u8; 64];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut c_void;
        msg.msg_controllen = cbuf.len() as _;
        let n = libc::recvmsg(sock, &mut msg, 0);
        anyhow::ensure!(n >= 0, "recvmsg: {}", std::io::Error::last_os_error());
        let mut fd = None;
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
            let mut raw: RawFd = -1;
            std::ptr::copy_nonoverlapping(libc::CMSG_DATA(c), &mut raw as *mut RawFd as *mut u8, 4);
            fd = Some(OwnedFd::from_raw_fd(raw));
        }
        Ok((String::from_utf8_lossy(&buf[..n as usize]).into_owned(), fd))
    }
}

/// `sudo hypermac netd-install`: copy this binary to /usr/local/libexec and start it at boot.
pub fn netd_install() -> anyhow::Result<()> {
    anyhow::ensure!(unsafe { libc::geteuid() } == 0, "run it with sudo");
    let dst = "/usr/local/libexec/hypermac-netd";
    std::fs::create_dir_all("/usr/local/libexec")?;
    std::fs::copy(std::env::current_exe()?, dst)?;
    let plist = "/Library/LaunchDaemons/dev.keelos.hypermac.netd.plist";
    std::fs::write(
        plist,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.keelos.hypermac.netd</string>
  <key>ProgramArguments</key><array><string>{dst}</string><string>netd</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>/var/log/hypermac-netd.log</string>
</dict></plist>
"#
        ),
    )?;
    let run = |args: &[&str]| std::process::Command::new("/bin/launchctl").args(args).status();
    let _ = run(&["bootout", "system/dev.keelos.hypermac.netd"]);
    anyhow::ensure!(run(&["bootstrap", "system", plist])?.success(), "launchctl bootstrap failed");
    println!("installed: {dst}, {plist} (log: /var/log/hypermac-netd.log)");
    Ok(())
}

pub fn _name(p: *const c_char) -> String {
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}
