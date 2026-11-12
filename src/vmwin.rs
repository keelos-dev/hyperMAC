//! The windows of a virtual machine on the bhyve engine, in a process of their own:
//! `hypermac vmwin NAME`, one per machine, started by the main window when someone asks for the
//! console or for coherence (user 2026-11-12: "console和实际虚拟机的进程不能是分开？").
//!
//! Before, they were windows of the VM program itself (vm/src/console.rs, coherence.rs). That
//! program is the machine: every change to a window meant restarting the guest, a crash in a
//! window was the machine's, its menu bar had no Quit, and what macOS allows it (the local
//! network, for coherence's agent) hung on whoever had started it. Here the same two files are
//! compiled into this program and fed from outside the machine:
//!   - the picture: <VM dir>/screen.shm, which the VM program writes while someone reads it
//!     (vms.rs `Shm`, the RDP server's reader);
//!   - keys and pointer: vkey / vabs / vbtn / vwheel lines to <VM dir>/ctl.sock;
//!   - the screen's size: ctl "size W H".
//! `vgpu`, `vinput` and `bhyve` below are those, under the names the two files call.
//!
//! Machines on Virtualization.framework (macOS, Linux) keep their console in the VM program:
//! Apple's view of a machine's display lives in the process the machine is in.
//!
//! Asked over <VM dir>/win.sock, a line each way: `console`, `coherence on|off|start|run CMD|
//! status`, `quit`. The process ends when the machine does, and a while after its last window
//! is gone.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

static NAME: OnceLock<String> = OnceLock::new();

fn name() -> &'static str {
    NAME.get().map(String::as_str).unwrap_or("")
}

fn dir() -> std::path::PathBuf {
    crate::vms::vm_dir(name())
}

/// One line to the machine's ctl.sock, and its answer.
fn ctl(cmd: &str) -> Option<serde_json::Value> {
    let mut s = UnixStream::connect(dir().join("ctl.sock")).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    writeln!(s, "{cmd}").ok()?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

/// The machine's picture (vm/src/vgpu.rs's part of the name).
pub mod vgpu {
    use super::*;
    use crate::vms::Shm;

    static SHM: Mutex<Option<Shm>> = Mutex::new(None);

    /// (frame number, width, height, pixels as B,G,R,X); the pixels are empty when `last` is
    /// that frame. None while the machine shows nothing yet.
    pub fn snapshot(last: Option<u64>) -> Option<(u64, u32, u32, Vec<u8>)> {
        let mut g = SHM.lock().unwrap();
        if g.is_none() {
            *g = Shm::open(name());
        }
        let s = g.as_ref()?;
        let (seq, w, h) = first(s)?;
        if last == Some(seq) {
            return Some((seq, w, h, Vec::new()));
        }
        match s.read(last.unwrap_or(u64::MAX)) {
            Some(frame) => Some(frame),
            // being written this instant: nothing new yet
            None => last.map(|n| (n, w, h, Vec::new())),
        }
    }

    pub fn size() -> Option<(u32, u32)> {
        let mut g = SHM.lock().unwrap();
        if g.is_none() {
            *g = Shm::open(name());
        }
        first(g.as_ref()?).map(|(_, w, h)| (w, h))
    }

    /// when the picture was last said to be watched
    static WANTED: Mutex<Option<Instant>> = Mutex::new(None);

    /// The picture's number and size. The VM program copies frames only while someone says
    /// they are watched, and what is in the file from before that is as old as the last time
    /// someone did (another size, even): the first question after a while waits for the frame
    /// the VM program writes for a new reader.
    fn first(s: &Shm) -> Option<(u64, u32, u32)> {
        let stale = WANTED.lock().unwrap().replace(Instant::now()).is_none_or(|t| t.elapsed() > Duration::from_millis(1500));
        let old = s.peek().0;
        s.want(2000);
        if stale {
            for _ in 0..50 {
                let (seq, _, _) = s.peek();
                if seq != old && seq % 2 == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let (seq, w, h) = s.peek();
        (w > 0 && h > 0).then_some((seq, w, h))
    }
}

/// Keys and pointer to the machine (vm/src/vinput.rs's part of the name): lines to its ctl.sock
/// over one connection kept open.
pub mod vinput {
    use super::*;

    static CTL: Mutex<Option<UnixStream>> = Mutex::new(None);

    fn send(line: &str) -> u64 {
        let mut g = CTL.lock().unwrap();
        for _ in 0..2 {
            if g.is_none() {
                let Ok(c) = UnixStream::connect(dir().join("ctl.sock")) else { return 0 };
                // (every line is answered; nobody here reads the answers)
                if let Ok(r) = c.try_clone() {
                    std::thread::spawn(move || for _ in BufReader::new(r).lines() {});
                }
                *g = Some(c);
            }
            if let Some(c) = g.as_mut() {
                if writeln!(c, "{line}").is_ok() {
                    return 1;
                }
            }
            *g = None;
        }
        0
    }

    pub fn key(code: u16, down: bool) -> u64 {
        send(&format!("vkey {code} {}", down as u8))
    }
    pub fn abs(x: f64, y: f64) -> u64 {
        send(&format!("vabs {x:.6} {y:.6}"))
    }
    pub fn button(b: u16, down: bool) -> u64 {
        send(&format!("vbtn {b} {}", down as u8))
    }
    pub fn wheel(steps: i32) -> u64 {
        send(&format!("vwheel {steps}"))
    }
}

/// What the two files ask the engine (vm/src/bhyve.rs's part of the name).
pub mod bhyve {
    use super::*;

    /// (asked on every step of a window being resized: remembered for a moment)
    static RESIZABLE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

    pub fn active() -> bool {
        true
    }

    /// Can the guest's screen take another size? (the 3D adapter; ctl status "resizable")
    pub fn resizable() -> bool {
        let mut g = RESIZABLE.lock().unwrap();
        if let Some((at, r)) = *g {
            if at.elapsed() < Duration::from_secs(3) {
                return r;
            }
        }
        let r = ctl("status").and_then(|s| s.get("resizable").and_then(|v| v.as_bool())).unwrap_or(false);
        *g = Some((Instant::now(), r));
        r
    }

    pub fn request_size(w: u32, h: u32) -> anyhow::Result<(u32, u32)> {
        let a = ctl(&format!("size {w} {h}")).context("the virtual machine does not answer")?;
        if let Some(e) = a.get("error").and_then(|v| v.as_str()) {
            anyhow::bail!("{e}");
        }
        let n = |k: &str| a.get(k).and_then(|v| v.as_u64()).map(|v| v as u32);
        Ok((n("w").unwrap_or(w), n("h").unwrap_or(h)))
    }
}

/// when something was last asked of this process, or one of its windows was last there
static BUSY: Mutex<Option<Instant>> = Mutex::new(None);
static QUITTING: AtomicBool = AtomicBool::new(false);

fn busy() {
    *BUSY.lock().unwrap() = Some(Instant::now());
}

/// Before the process ends, however it ends: the guest's screen back to its size, the socket
/// gone (the next request starts a new process).
extern "C" fn leave() {
    if QUITTING.swap(true, Ordering::Relaxed) {
        return;
    }
    crate::coherence::leave();
    let _ = ctl("viewer 0");
    let _ = std::fs::remove_file(dir().join("win.sock"));
}

fn answer(line: &str) -> serde_json::Value {
    busy();
    match line {
        "console" => {
            let title = format!("{} — hyperMAC", name());
            dispatch2::DispatchQueue::main().exec_async(move || crate::console::open(&title));
            serde_json::json!({"ok": true})
        }
        l if l.starts_with("coherence") => {
            // what the agent in the guest wants to hear, and where the guest is
            let token = std::fs::read(dir().join("vm.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .and_then(|c| c.get("agent").and_then(|v| v.as_str()).map(str::to_string));
            let addr = ctl("status").and_then(|s| s.get("ip").and_then(|v| v.as_str()).map(str::to_string));
            // (the machine's own channel to its agent first: vm/src/bhyve.rs AGENT_PORT)
            crate::coherence::ctl(l, Some(&dir().join("agent.sock")), addr, token.as_deref())
        }
        "quit" => {
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                leave();
                std::process::exit(0);
            });
            serde_json::json!({"ok": true})
        }
        _ => serde_json::json!({"error": "console | coherence on|off|start|run CMD|status | quit"}),
    }
}

/// `hypermac vmwin NAME`.
pub fn run(vm: &str) -> anyhow::Result<()> {
    let _ = NAME.set(vm.to_string());
    let sock = dir().join("win.sock");
    // one per machine
    if UnixStream::connect(&sock).is_ok() {
        return Ok(());
    }
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    busy();
    unsafe { libc::atexit(leave) };
    // the menu's Quit ends this process, not the machine
    crate::console::QUIT.store(true, Ordering::Relaxed);
    std::thread::spawn(move || {
        for c in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let Ok(mut w) = c.try_clone() else { return };
                for line in BufReader::new(c).lines().map_while(Result::ok) {
                    if writeln!(w, "{}", answer(line.trim())).is_err() {
                        break;
                    }
                }
            });
        }
    });
    // The machine gone: so is this. No window for a while: likewise (the Dock icon goes with
    // it). While there is one, the VM program hears that someone is at the machine on this Mac
    // (it plays the guest's sound then: vm/src/sound.rs).
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if UnixStream::connect(dir().join("ctl.sock")).is_err() {
                leave();
                std::process::exit(0);
            }
            if crate::console::shown() || crate::coherence::on() {
                busy();
                let _ = ctl("viewer 1");
            } else if BUSY.lock().unwrap().is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
                leave();
                std::process::exit(0);
            }
        }
    });
    let mtm = MainThreadMarker::new().context("not on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.run();
    Ok(())
}

// ---- the main window's side -------------------------------------------------------------------

fn ask(sock: &std::path::Path, cmd: &str) -> anyhow::Result<serde_json::Value> {
    let mut s = UnixStream::connect(sock)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(s, "{cmd}")?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

/// Ask the machine's window process, if there is one (nothing is started for a question).
pub fn peek(vm: &str, cmd: &str) -> Option<serde_json::Value> {
    ask(&crate::vms::vm_dir(vm).join("win.sock"), cmd).ok()
}

/// Ask the machine's window process, started first when it is not there.
pub fn request(vm: &str, cmd: &str) -> anyhow::Result<serde_json::Value> {
    let sock = crate::vms::vm_dir(vm).join("win.sock");
    if let Ok(a) = ask(&sock, cmd) {
        return Ok(a);
    }
    use std::os::unix::process::CommandExt;
    // From an app bundle where there is one beside this program (build.sh makes it for an
    // install from source): macOS lets a bundle ask for the local network, where the agent in
    // a Windows guest is; a bare executable started by launchd is refused without a question.
    // The packaged hyperMAC.app is a bundle itself.
    let exe = std::env::current_exe()?;
    let bundled = exe.parent().map(|d| d.join("hyperMAC Windows.app/Contents/MacOS/hypermac-win")).filter(|p| p.exists());
    let exe = bundled.unwrap_or(exe);
    let log = std::fs::OpenOptions::new().create(true).append(true).open(crate::vms::vm_dir(vm).join("win.log"))?;
    let mut c = std::process::Command::new(exe);
    c.args(["vmwin", vm]).stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    c.spawn()?;
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        std::thread::sleep(Duration::from_millis(100));
        match ask(&sock, cmd) {
            Ok(a) => return Ok(a),
            Err(e) if Instant::now() > until => return Err(e.context("the window process did not start")),
            Err(_) => {}
        }
    }
}
