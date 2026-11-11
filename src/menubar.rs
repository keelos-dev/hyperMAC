//! The menu bar icon (user 2026-11-10): `hypermac menu`, started at login by its own LaunchAgent
//! (main.rs `install_menu`), separate from the service. It is the app's one long-running GUI
//! process: the settings window (ui.rs) opens in it, there is no Dock icon.
//!
//! Why a process of its own: anything AppKit checks a process in with LaunchServices as the app
//! (cursor.rs `bitmap`), and the service runs hyperMAC.app's executable, so LaunchServices would
//! take the service for "hyperMAC is running" and a double-click would open nothing (1.0
//! 83fc9fc). This process is meant to be that running hyperMAC: a double-click on hyperMAC.app
//! comes here as a reopen and shows the window.
//!
//! The icon: `display` idle, a pause sign while paused, a warning badge while the service is
//! not running; always shown, also beside macOS's own screen-recording indicator while a client
//! is connected (whose Stop Sharing disconnects: capture.rs). The menu: Open hyperMAC…,
//! Disconnect (SIGUSR1 to the service, shutdown.rs), Pause / Resume Remote ("paused" in
//! settings.json; the service turns new connections away, the ones up stay). The state comes
//! from <state>/service.json (pid) and status.json (connections), read every second.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSStatusItem,
};
use objc2_foundation::{NSObject, NSTimer};

use crate::gui::{self, ns, t};

/// The file that asks the next menu bar process to open the window at once (after an update
/// or a hand-over from a double-click, which restart this process).
const OPEN_WINDOW: &str = "open-window";

static MENU_PROCESS: AtomicBool = AtomicBool::new(false);

/// Is this the menu bar process (the window's actions behave differently in it)?
pub fn is_menu_process() -> bool {
    MENU_PROCESS.load(Ordering::Relaxed)
}

struct Menu {
    state: PathBuf,
    item: Retained<NSStatusItem>,
    disconnect: Retained<NSMenuItem>,
    pause: Retained<NSMenuItem>,
    /// this executable's mtime at start: a newer app dragged over us restarts us on reopen
    exe_mtime: u64,
    /// what the icon shows now, so it is set only when it changes
    shown: Option<Status>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Status {
    pub running: bool,
    pub connections: u64,
    /// of the connections, those showing this Mac's screen (not a VM's)
    pub on_mac: u64,
    pub paused: bool,
}

thread_local! {
    static MENU: RefCell<Option<Menu>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "HyperMACMenu"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        // a double-click on hyperMAC.app (or `open`) while this process runs
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn reopen(&self, _app: &NSApplication, _visible: bool) -> bool {
            show_window();
            false
        }
    }

    impl Delegate {
        #[unsafe(method(open:))]
        fn open(&self, _sender: &AnyObject) {
            show_window();
        }

        #[unsafe(method(disconnect:))]
        fn disconnect(&self, _sender: &AnyObject) {
            disconnect(&state());
            refresh();
        }

        #[unsafe(method(pause:))]
        fn pause(&self, _sender: &AnyObject) {
            let st = state();
            set_paused(mtm(), &st, !status(&st).paused);
            refresh();
            crate::ui::refresh_if_open();
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &AnyObject) {
            refresh();
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().expect("main thread")
}

pub fn state() -> PathBuf {
    MENU.with(|m| m.borrow().as_ref().map(|m| m.state.clone())).unwrap_or_else(crate::state_dir)
}

/// The service's pid while it runs (service.json, checked with kill 0).
fn service_pid(state: &Path) -> Option<u32> {
    let r: gui::Running = serde_json::from_slice(&std::fs::read(state.join("service.json")).ok()?).ok()?;
    let alive = unsafe { libc::kill(r.pid as libc::pid_t, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    alive.then_some(r.pid)
}

pub fn status(state: &Path) -> Status {
    let running = service_pid(state).is_some();
    let v = running
        .then(|| std::fs::read(state.join("status.json")).ok())
        .flatten()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    let num = |k: &str| v.as_ref().and_then(|v| v.get(k)).and_then(|c| c.as_u64()).unwrap_or(0);
    let connections = num("connections");
    let on_mac = connections.saturating_sub(num("vm_sessions"));
    Status { running, connections, on_mac, paused: crate::settings::get(state, "paused", false) }
}

pub fn run(state: &Path) -> anyhow::Result<()> {
    MENU_PROCESS.store(true, Ordering::Relaxed);
    let mtm = MainThreadMarker::new().context("not on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    crate::gui::set_app_icon(&app);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let delegate = Delegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    let item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0); // NSVariableStatusItemLength
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let entry = |title: &str, action: objc2::runtime::Sel| {
        let i = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &ns(title), Some(action), &ns(""))
        };
        unsafe { i.setTarget(Some(&delegate)) };
        menu.addItem(&i);
        i
    };
    entry(t("打开 hyperMAC…", "Open hyperMAC…"), sel!(open:));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let disconnect = entry(t("断开当前连接", "Disconnect"), sel!(disconnect:));
    let pause = entry(t("暂停远程", "Pause Remote"), sel!(pause:));
    item.setMenu(Some(&menu));

    let exe = std::env::current_exe()?;
    MENU.with(|m| {
        *m.borrow_mut() = Some(Menu {
            state: state.to_path_buf(),
            item,
            disconnect,
            pause,
            exe_mtime: gui::exe_mtime(&exe),
            shown: None,
        })
    });
    refresh();
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(1.0, &delegate, sel!(tick:), None, true)
    };
    if std::fs::remove_file(state.join(OPEN_WINDOW)).is_ok() {
        show_window();
    }
    app.run();
    Ok(())
}

/// Disconnect every client (the menu's item, the window's button): SIGUSR1 to the service.
pub fn disconnect(state: &Path) {
    if let Some(pid) = service_pid(state) {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGUSR1) };
    }
}

/// Pause or resume remote access (the menu's item, the window's switch). Pausing while a client
/// is connected asks first: once that session ends, nobody gets in until resumed on this Mac.
/// false = cancelled or not saved.
pub fn set_paused(mtm: MainThreadMarker, state: &Path, paused: bool) -> bool {
    if paused && status(state).connections > 0 {
        let go = gui::ask(
            mtm,
            t("暂停远程？", "Pause remote access?"),
            t(
                "现在连着的会话不受影响；但断开以后就连不进来了，要在这台 Mac 上点「恢复远程」才行。如果你正是远程连着这台 Mac，请想清楚。",
                "The session connected now stays; but once it ends, nobody can connect until \"Resume Remote\" is chosen on this Mac. If you are connected to this Mac remotely right now, think twice.",
            ),
            &[t("取消", "Cancel"), t("暂停", "Pause")],
        );
        if go != 1 {
            return false;
        }
    }
    if let Err(e) = crate::settings::set(state, "paused", paused) {
        gui::error(mtm, t("没能保存", "Could not save"), &format!("{e:#}"));
        return false;
    }
    true
}

/// The icon, its tooltip and the menu's items, from the state now.
fn refresh() {
    let st = state();
    let now = status(&st);
    MENU.with(|m| {
        let mut m = m.borrow_mut();
        let Some(m) = m.as_mut() else { return };
        m.disconnect.setEnabled(now.running && now.connections > 0);
        m.pause.setTitle(&ns(if now.paused { t("恢复远程", "Resume Remote") } else { t("暂停远程", "Pause Remote") }));
        if m.shown == Some(now) {
            return;
        }
        m.shown = Some(now);
        // always shown (user 2026-11-10: "一直显示就行"): hiding it while a client is connected
        // left no way to open the window from that very session
        let (symbol, tip) = if !now.running {
            ("display.trianglebadge.exclamationmark", t("hyperMAC：服务没在运行", "hyperMAC: the service is not running"))
        } else if now.paused {
            ("pause.rectangle", t("hyperMAC：已暂停，不接受新连接", "hyperMAC: paused, no new connections"))
        } else if now.connections > 0 {
            ("inset.filled.rectangle.and.person.filled", t("hyperMAC：有人连着", "hyperMAC: connected"))
        } else {
            ("display", t("hyperMAC：等待连接", "hyperMAC: waiting for connections"))
        };
        // waiting / connected: hyperMAC's own mark (tools/mkmenuicon.py); paused and not
        // running keep their system symbols, they say what is wrong
        let image = (symbol == "display" || symbol == "inset.filled.rectangle.and.person.filled")
            .then(menubar_mark)
            .flatten()
            .or_else(|| NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns(symbol), Some(&ns("hyperMAC"))))
            .or_else(|| NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns("display"), Some(&ns("hyperMAC"))));
        if let Some(button) = m.item.button(mtm()) {
            if let Some(image) = image {
                image.setTemplate(true);
                button.setImage(Some(&image));
            } else {
                button.setTitle(&ns("RDP"));
            }
            button.setToolTip(Some(&ns(tip)));
        }
    });
}

/// The settings window, in this process; restarts this process first when a newer hyperMAC.app
/// has been put over the one it runs (the window would be the old one's otherwise).
fn show_window() {
    let st = state();
    let exe = std::env::current_exe().ok();
    let changed = exe.as_deref().is_some_and(|e| MENU.with(|m| m.borrow().as_ref().is_some_and(|m| m.exe_mtime != gui::exe_mtime(e))));
    if changed {
        restart(&st, true);
        NSApplication::sharedApplication(mtm()).terminate(None);
        return;
    }
    let page = gui::page_for(mtm(), &st);
    crate::ui::show(mtm(), st, page);
}

/// Restart the menu bar process (on the executable now in place), with the window open when
/// `window`. A detached shell asks launchd, so it outlives this process (launchd ends the
/// job's process group with it).
pub fn restart(state: &Path, window: bool) {
    if window {
        let _ = std::fs::write(state.join(OPEN_WINDOW), b"");
    }
    let uid = unsafe { libc::getuid() };
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(format!("sleep 1; /bin/launchctl kickstart -k gui/{uid}/{}", crate::MENU_LABEL));
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

/// Hand the window over to the menu bar process (a double-click while it was not running):
/// its LaunchAgent is written if missing (an update from 1.0, which had none) and the process
/// started — or restarted on this executable — with the window open.
pub fn hand_over(state: &Path, exe: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(state)?;
    std::fs::write(state.join(OPEN_WINDOW), b"")?;
    if crate::install_menu(state, exe)? {
        return Ok(()); // just loaded: it starts and finds the request
    }
    let uid = unsafe { libc::getuid() };
    let st = std::process::Command::new("/bin/launchctl")
        .args(["kickstart", "-k", &format!("gui/{uid}/{}", crate::MENU_LABEL)])
        .status()?;
    anyhow::ensure!(st.success(), "launchctl kickstart failed");
    Ok(())
}

/// The menu bar mark: the app icon's cards as a template (18 pt, from a 36 px PNG).
fn menubar_mark() -> Option<Retained<NSImage>> {
    static PNG: &[u8] = include_bytes!("../assets/menubar.png");
    use objc2::AllocAnyThread;
    let data = objc2_foundation::NSData::with_bytes(PNG);
    let img = NSImage::initWithData(NSImage::alloc(), &data)?;
    img.setSize(objc2_foundation::NSSize::new(18.0, 18.0));
    Some(img)
}
