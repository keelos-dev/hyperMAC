//! Saying goodbye (user 2026-11-09: when the Mac restarts, every connected client should be
//! told, and what a client makes of it is the client's business). Without this, a restart of
//! the Mac or of the service just kills the process: the clients see a dead connection and
//! cannot tell a shutdown from a network failure (and when the network goes first, they wait
//! for a timeout in front of a frozen picture).
//!
//! launchd ends us with SIGTERM (service restart, logout, shutdown, restart). On it — and on
//! SIGINT, for a run in a terminal — every RDP connection gets a Server Set Error Info PDU
//! (MS-RDPBCGR 2.2.5.1.1) with the code `ServerShutdown` (0x19, "the remote server is busy
//! shutting down") and is then disconnected the regular way: a standard RDP message, so any
//! client can show it. A client that did not ask for error info PDUs (MS-RDPBCGR 3.3.5.7.1)
//! is disconnected without the reason. Then the signal's default action runs, so the process
//! still ends "by SIGTERM" as launchd expects.
//!
//! Best effort: at a real shutdown the system gives little time, and a client behind a dead
//! network hears nothing either way.
//!
//! SIGUSR1 is the menu bar's "Disconnect" (menubar.rs): every connection gets the code
//! `RpcInitiatedDisconnect` (0x1, "disconnected by an administrative tool on the server") and
//! is disconnected, and the service keeps running. kRDPclient connects again by itself only
//! after ServerShutdown / ServerReboot (its server_end.rs), so it stays disconnected.
//!
//! The number of connections that are up goes to <state>/status.json ({"connections": n})
//! each time it changes, for the menu bar's icon.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tracing::info;

/// How long the connections get to send their goodbye before the process ends.
const GRACE: Duration = Duration::from_millis(1000);

/// The write end of the pipe the signal handler pokes (a handler may do little else).
static PIPE_W: AtomicI32 = AtomicI32::new(-1);
/// RDP connections that are up, so the wait can end as soon as all have gone.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
/// Of those, the ones on a VM (vms.rs): they do not show this Mac's screen, so the menu bar
/// icon stays (it steps aside only for macOS's screen-recording indicator).
static ON_VM: AtomicUsize = AtomicUsize::new(0);
/// <state>/status.json, once main has said where the state is.
static STATUS: OnceLock<PathBuf> = OnceLock::new();

/// Where the connection count goes (main, at startup); writes the first one (0).
pub fn status_file(path: PathBuf) {
    let _ = STATUS.set(path);
    write_status();
    // the grants can change while the service runs (a switch in System Settings)
    std::thread::spawn(|| {
        let mut last = grants();
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let now = grants();
            if now != last {
                last = now;
                write_status();
            }
        }
    });
}

/// This process's Screen Recording and Accessibility: the window shows these when it runs in
/// the menu bar's copy of the binary (another executable to TCC, main.rs `write_menu_binary`).
fn grants() -> (bool, bool) {
    (crate::gui::screen_ok(), crate::gui::ax_ok())
}

fn write_status() {
    let Some(path) = STATUS.get() else { return };
    let (screen, ax) = grants();
    let body = format!(
        "{{\"connections\": {}, \"vm_sessions\": {}, \"screen\": {screen}, \"ax\": {ax}}}\n",
        ACTIVE.load(Ordering::Relaxed),
        ON_VM.load(Ordering::Relaxed)
    );
    let tmp = path.with_extension("json.new");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// What a connection is told to end by: the process ending (true once) and the menu bar's
/// Disconnect (a counter that goes up each time).
pub struct Signals {
    pub ending: tokio::sync::watch::Receiver<bool>,
    pub kick: tokio::sync::watch::Receiver<u64>,
}

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = PIPE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = sig as u8;
        unsafe { libc::write(fd, (&raw const b).cast(), 1) };
    }
}

/// Catches SIGTERM and SIGINT (`ending` turns true once) and SIGUSR1 (`kick` goes up). If the
/// pipe cannot be made the signals keep their default action and the receivers never change.
pub fn install() -> Signals {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let (kick_tx, kick_rx) = tokio::sync::watch::channel(0u64);
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        // keep the senders alive, or every waiter would see the channel close
        std::mem::forget(tx);
        std::mem::forget(kick_tx);
        return Signals { ending: rx, kick: kick_rx };
    }
    PIPE_W.store(fds[1], Ordering::Relaxed);
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::signal(libc::SIGUSR1, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }
    let read_end = fds[0];
    std::thread::spawn(move || {
        let mut b = [0u8; 1];
        let sig = loop {
            let n = unsafe { libc::read(read_end, b.as_mut_ptr().cast(), 1) };
            if n == 1 {
                let sig = libc::c_int::from(b[0]);
                if sig == libc::SIGUSR1 {
                    info!(connections = ACTIVE.load(Ordering::Relaxed), "disconnect asked (menu bar)");
                    kick_tx.send_modify(|k| *k += 1);
                    continue;
                }
                break sig;
            }
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // keep the kick sender alive: the connections' waits must not see it close
            std::mem::forget(kick_tx);
            return;
        };
        let up = ACTIVE.load(Ordering::Relaxed);
        info!(signal = sig, connections = up, "ending: telling the connected clients");
        let _ = tx.send(true);
        let deadline = Instant::now() + GRACE;
        while ACTIVE.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // let the last bytes leave the sockets, then end the way the signal would have
        std::thread::sleep(Duration::from_millis(50));
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    });
    Signals { ending: rx, kick: kick_rx }
}

/// Disconnect every connection, as the menu bar's Disconnect does (SIGUSR1 to this process).
pub fn disconnect_all() {
    unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) };
}

/// One RDP connection is on a VM for as long as this lives.
pub struct OnVm;

impl OnVm {
    pub fn new() -> Self {
        ON_VM.fetch_add(1, Ordering::Relaxed);
        write_status();
        OnVm
    }
}

impl Drop for OnVm {
    fn drop(&mut self) {
        ON_VM.fetch_sub(1, Ordering::Relaxed);
        write_status();
    }
}

/// One RDP connection is up for as long as this lives.
pub struct Up;

impl Up {
    pub fn new() -> Self {
        ACTIVE.fetch_add(1, Ordering::Relaxed);
        write_status();
        Up
    }
}

impl Drop for Up {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::Relaxed);
        write_status();
    }
}
