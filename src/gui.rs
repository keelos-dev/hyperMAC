//! Double-clicking hyperMAC.app (main.rs routes here when the executable sits in an .app and has
//! no arguments): the window (ui.rs) — first-time setup, then the settings page. This file has
//! what the window uses: the checks (grants, service, SSH, addresses), the small modal dialogs
//! (password again, token, SSH, uninstall), and `upgrade`: a newer app dragged over the old one
//! replaces the running service on the next double-click.
//!
//! The service itself (launchd, with --state) never comes here and shows nothing.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::Context;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSAlert, NSAlertStyle, NSApplication, NSApplicationActivationPolicy, NSPasteboard, NSPasteboardTypeString,
    NSSecureTextField, NSTextField, NSView,
};
use objc2_foundation::{NSDictionary, NSLocale, NSNumber, NSPoint, NSRect, NSSize, NSString};

use crate::{AGENT_LABEL, agent_plist, app_bundle, app_in_applications};

pub const KRDPCLIENT_URL: &str = "https://github.com/keelos-dev/kRDPclient-releases";

pub const SETTINGS_SCREEN: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
pub const SETTINGS_AX: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
pub const SETTINGS_SHARING: &str = "x-apple.systempreferences:com.apple.Sharing-Settings.extension";

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> u8;
    fn CGRequestScreenCaptureAccess() -> u8;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: *const AnyObject) -> u8;
}

/// Chinese when the first preferred language is Chinese, English otherwise.
pub fn zh() -> bool {
    static ZH: OnceLock<bool> = OnceLock::new();
    *ZH.get_or_init(|| match std::env::var("MACRDP_UI_LANG") {
        Ok(l) => l.starts_with("zh"), // for looking at the window in the other language
        Err(_) => NSLocale::preferredLanguages().firstObject().is_some_and(|l| l.to_string().starts_with("zh")),
    })
}

pub fn t(zh_text: &'static str, en: &'static str) -> &'static str {
    if zh() { zh_text } else { en }
}

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// A modal alert; returns the index of the button pressed (0 = the first = the default one).
fn alert(
    mtm: MainThreadMarker,
    title: &str,
    text: &str,
    buttons: &[&str],
    accessory: Option<&NSView>,
    warning: bool,
) -> usize {
    let a = NSAlert::new(mtm);
    a.setMessageText(&ns(title));
    a.setInformativeText(&ns(text));
    if warning {
        a.setAlertStyle(NSAlertStyle::Warning);
    }
    for b in buttons {
        a.addButtonWithTitle(&ns(b));
    }
    if let Some(v) = accessory {
        a.setAccessoryView(Some(v));
    }
    #[allow(deprecated)] // activate() is macOS 14+
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    let r = a.runModal();
    // NSAlertFirstButtonReturn = 1000, the second 1001, ...
    (r - 1000).clamp(0, buttons.len().saturating_sub(1) as isize) as usize
}

pub fn open(url: &str) {
    let _ = std::process::Command::new("open").arg(url).status();
}

pub fn run(state: &Path) -> anyhow::Result<()> {
    let mtm = MainThreadMarker::new().context("not on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    set_app_icon(&app);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let exe = std::env::current_exe()?.canonicalize()?;
    let bundle = app_bundle(&exe).context("not in an app")?;
    // MACRDP_UI_PREVIEW=welcome|permissions|ssh|main: that page directly, wherever the app is
    // (for looking at the window; run it with a scratch HOME so it touches nothing real)
    let preview = std::env::var("MACRDP_UI_PREVIEW").ok().and_then(|p| match p.as_str() {
        "welcome" => Some(crate::ui::Page::Welcome),
        "permissions" => Some(crate::ui::Page::Permissions),
        "ssh" => Some(crate::ui::Page::Ssh),
        "main" => Some(crate::ui::Page::Main),
        _ => None,
    });
    if let Some(p) = preview {
        crate::ui::run(mtm, state.to_path_buf(), p);
        return Ok(());
    }
    // Set up already: the window belongs to the menu bar process (menubar.rs). A double-click
    // only lands here when that process is not running (else it gets the reopen).
    let set_up = agent_plist().is_ok_and(|p| p.exists())
        && app_in_applications(&bundle)
        && crate::account::read(state).is_ok_and(|c| c.is_system());
    if set_up {
        match crate::menubar::hand_over(state, &exe) {
            Ok(()) => return Ok(()),
            Err(e) => tracing::warn!("menu bar: {e:#}; the window opens here"),
        }
    }
    crate::ui::run(mtm, state.to_path_buf(), page_for(mtm, state));
    Ok(())
}

/// The page the window opens on: not in /Applications, the first-time setup, or the main page
/// (after `upgrade`). A development build (no app) goes to the main page.
pub fn page_for(mtm: MainThreadMarker, state: &Path) -> crate::ui::Page {
    let exe = std::env::current_exe().and_then(|e| e.canonicalize()).unwrap_or_default();
    match app_bundle(&exe) {
        Some(b) if !app_in_applications(&b) => crate::ui::Page::NotInApplications,
        _ if !crate::account::read(state).is_ok_and(|c| c.is_system()) => {
            // not set up, or an early build's separate password (no users then): the Mac account
            crate::ui::Page::Welcome
        }
        Some(_) => {
            upgrade(mtm, state, &exe);
            crate::ui::Page::Main
        }
        None => crate::ui::Page::Main,
    }
}

/// The Mac user's own login password, checked by the system, into credentials.json as the
/// account (account.rs). Asked once: mstsc's NLA needs the password's NT hash, which macOS
/// does not keep. false = cancelled.
pub fn password_dialog(mtm: MainThreadMarker, state: &Path) -> anyhow::Result<bool> {
    let first = false;
    let user = crate::account::mac_user();
    let field = NSSecureTextField::initWithFrame(
        NSSecureTextField::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(300.0, 24.0)),
    );
    field.setPlaceholderString(Some(&ns(t("这台 Mac 的登录密码", "This Mac's login password"))));
    let mut note = String::new();
    loop {
        let text = if zh() {
            format!(
                "远程连 hyperMAC 用的就是这台 Mac 的账号：\n\n用户名：{user}\n密码：你登录这台 Mac 的密码\n\n\
                 请输入一次登录密码，由系统核对。hyperMAC 不保存密码本身；只存 mstsc 登录要用的一个哈希\
                 （在只有你能读的文件里）。以后改了 Mac 密码，用新密码从 kRDPclient 登录一次（或者再打开 hyperMAC 输一次），mstsc 也就跟上了。{note}"
            )
        } else {
            format!(
                "Connecting to hyperMAC uses this Mac's own account:\n\nUser name: {user}\nPassword: the one you log in to this Mac with\n\n\
                 Type the login password once; the system checks it. hyperMAC does not keep the password itself, only a hash \
                 that mstsc's login needs (in a file only you can read). After you change the Mac's password, log in once \
                 with the new one from kRDPclient (or open hyperMAC and type it here) and mstsc follows.{note}"
            )
        };
        let title = if first { t("hyperMAC：用这台 Mac 的账号", "hyperMAC: use this Mac's account") } else { t("重新输入 Mac 登录密码", "Type the Mac's login password again") };
        let r = alert(mtm, title, &text, &[t("继续", "Continue"), t("取消", "Cancel")], Some(&field), false);
        if r == 1 {
            return Ok(false);
        }
        let pw = field.stringValue().to_string();
        note = if pw.is_empty() {
            t(
                "\n\n⚠️ 请输入密码。如果这台 Mac 没设登录密码，先在「系统设置 → 用户与群组」里设一个：远程登录不能没有密码。",
                "\n\n⚠️ Type the password. If this Mac has no login password, set one in System Settings → Users & Groups first: remote login needs one.",
            )
            .into()
        } else if crate::account::set_system(state, &user, &pw).is_ok() {
            return Ok(true);
        } else {
            t("\n\n⚠️ 密码不对，请再试一次。", "\n\n⚠️ That is not the password, try again.").into()
        };
        field.setStringValue(&ns(""));
    }
}

/// The service's grants as it last wrote them (status.json), for the menu bar's copy of the
/// binary: its own would be asked about the wrong executable. None: not that copy.
fn service_grants() -> Option<(bool, bool)> {
    if !crate::is_menu_copy() {
        return None;
    }
    let v = std::fs::read(crate::menubar::state().join("status.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    let get = |k: &str| v.as_ref().and_then(|v| v.get(k)).and_then(|b| b.as_bool()).unwrap_or(false);
    Some((get("screen"), get("ax")))
}

pub fn screen_ok() -> bool {
    service_grants().map_or_else(|| unsafe { CGPreflightScreenCaptureAccess() != 0 }, |g| g.0)
}

pub fn ax_ok() -> bool {
    service_grants().map_or_else(|| unsafe { AXIsProcessTrusted() != 0 }, |g| g.1)
}

/// Ask the system for both grants once (first run): Screen Recording shows its own prompt;
/// Accessibility adds hyperMAC to its list (switched off) with the system's hint. Not from the
/// menu bar's copy: that would ask for the copy, not the service.
pub fn request_permissions() {
    if crate::is_menu_copy() {
        return;
    }
    if !screen_ok() {
        unsafe { CGRequestScreenCaptureAccess() };
    }
    if !ax_ok() {
        let key = ns("AXTrustedCheckOptionPrompt"); // kAXTrustedCheckOptionPrompt
        let yes = NSNumber::new_bool(true);
        let opts = NSDictionary::from_slices(&[&*key], &[&*yes]);
        unsafe { AXIsProcessTrustedWithOptions(Retained::as_ptr(&opts).cast()) };
    }
}

/// A modal question; the index of the button pressed.
pub fn ask(mtm: MainThreadMarker, title: &str, text: &str, buttons: &[&str]) -> usize {
    alert(mtm, title, text, buttons, None, false)
}

pub fn confirm_uninstall(mtm: MainThreadMarker) -> bool {
    alert(
        mtm,
        t("停止 hyperMAC 并取消自动运行？", "Stop hyperMAC and no longer run it at login?"),
        t(
            "服务会停下，登录时也不再启动；正在连着的会话会断开。设置（账号哈希、证书、token）留在 ~/Library/Application Support/hyperMAC，要彻底删就删掉那个文件夹和 hyperMAC.app。\n\n远程登录（SSH）是系统设置，不会被关掉。",
            "The service stops and no longer starts at login; a session in progress is disconnected. The settings (account hash, certificate, tokens) stay in ~/Library/Application Support/hyperMAC: delete that folder and hyperMAC.app to remove everything.\n\nRemote Login (SSH) is a system setting and stays as it is.",
        ),
        &[t("取消", "Cancel"), t("停止", "Stop")],
        None,
        true,
    ) == 1
}

/// Remote Login (the system's sshd) answering on this Mac's port 22.
pub fn ssh_on() -> bool {
    std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], 22).into(), std::time::Duration::from_millis(500)).is_ok()
}

/// Ask whether to turn on Remote Login; the default answer is no. Yes takes the user to
/// System Settings → General → Sharing, where the system itself asks for an administrator
/// (hyperMAC never sees that password and changes no sshd setting).
pub fn ssh_dialog(mtm: MainThreadMarker, first: bool) {
    let r = alert(
        mtm,
        t("是否打开远程登录（SSH）？", "Turn on Remote Login (SSH)?"),
        t(
            "打开后，可以在 kRDPclient 里直接开这台 Mac 的终端（树里这台 Mac 下面的「SSH」）。\n\n\
             • SSH 用的是这台 Mac 的系统账号和密码（或密钥），不是刚才设的 hyperMAC 密码。\n\
             • 打开后，局域网里能连到这台 Mac 22 端口的人都可以尝试登录。\n\n\
             选「打开」会带你到「系统设置 → 通用 → 共享」，在那里打开「远程登录」（系统会要管理员密码）。不需要的话选「不打开」，以后还能在 hyperMAC 里再开。",
            "With it on, kRDPclient can open a terminal on this Mac (the \"SSH\" node under this Mac in its tree).\n\n\
             • SSH logs in with this Mac's own account and password (or a key), not the hyperMAC password you just set.\n\
             • Anyone who can reach this Mac's port 22 on the network can then try to log in.\n\n\
             \"Turn On\" takes you to System Settings → General → Sharing; switch on Remote Login there (the system asks for an administrator). If you do not need it, choose \"Not Now\"; hyperMAC can turn it on later.",
        ),
        &[if first { t("不打开", "Not Now") } else { t("取消", "Cancel") }, t("打开…", "Turn On…")],
        None,
        false,
    );
    if r != 1 {
        return;
    }
    open(SETTINGS_SHARING);
    alert(
        mtm,
        t("在「共享」里打开「远程登录」", "Switch on Remote Login in Sharing"),
        t(
            "在刚打开的「系统设置 → 通用 → 共享」里打开「远程登录」，弄好后点「好」。",
            "In System Settings → General → Sharing, which just opened, switch on Remote Login, then click OK.",
        ),
        &[t("好", "OK")],
        None,
        false,
    );
}

pub fn error(mtm: MainThreadMarker, title: &str, text: &str) {
    alert(mtm, title, text, &[t("好", "OK")], None, true);
}

/// What the service wrote at its start (main.rs `serve`): its pid and the executable's
/// modification time, so a newer app (dragged over the old one) is noticed.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Running {
    pub pid: u32,
    pub exe_mtime: u64,
    pub version: String,
}

pub fn exe_mtime(exe: &Path) -> u64 {
    std::fs::metadata(exe)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// The LaunchAgent's pid while it runs.
pub fn service_pid() -> Option<u32> {
    let o = std::process::Command::new("launchctl")
        .args(["print", &format!("gui/{}/{AGENT_LABEL}", uid())])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&o.stdout);
    s.lines().find_map(|l| l.trim().strip_prefix("pid = ")).and_then(|p| p.trim().parse().ok())
}

pub fn restart_service() {
    let _ = std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &format!("gui/{}/{AGENT_LABEL}", uid())])
        .status();
}

/// After an upgrade: point the LaunchAgent at this app if it points elsewhere (a development
/// copy, an old location), or restart it if it still runs an older executable.
fn upgrade(mtm: MainThreadMarker, state: &Path, exe: &Path) {
    let Ok(plist) = agent_plist() else { return };
    let Ok(xml) = std::fs::read_to_string(&plist) else { return }; // not installed: nothing to replace
    if !xml.contains(&format!("<string>{}</string>", exe.display())) {
        // another hypermac runs as the service (a development copy, another location): replace
        // it only when asked — a session in progress over it is disconnected
        let other = xml
            .split("<key>ProgramArguments</key><array>")
            .nth(1)
            .and_then(|s| s.split("<string>").nth(1))
            .and_then(|s| s.split("</string>").next())
            .unwrap_or("?")
            .to_string();
        let text = if zh() {
            format!("现在的 hyperMAC 服务跑的是：\n{other}\n\n换成这个 app（{}）吗？正在通过 hyperMAC 连着的会话会断开，几秒后可以重连。", exe.display())
        } else {
            format!("The hyperMAC service now runs:\n{other}\n\nSwitch it to this app ({})? A session in progress over hyperMAC is disconnected; reconnect after a few seconds.", exe.display())
        };
        let r = alert(
            mtm,
            t("换成这个 hyperMAC？", "Switch the service to this hyperMAC?"),
            &text,
            &[t("先不换", "Not Now"), t("换", "Switch")],
            None,
            true,
        );
        if r != 1 {
            return;
        }
        if let Err(e) = crate::install(state) {
            error(mtm, t("没能更新自启", "Could not update the login item"), &format!("{e:#}"));
        }
        return;
    }
    let running: Option<Running> =
        std::fs::read(state.join("service.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    if let (Some(pid), Some(r)) = (service_pid(), running) {
        if r.pid == pid && r.exe_mtime != exe_mtime(exe) {
            restart_service();
        }
    }
}

/// IPv4 addresses of this Mac (no loopback, no link-local).
pub fn addresses() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let mut ifa: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifa) != 0 {
            return out;
        }
        let mut p = ifa;
        while !p.is_null() {
            let a = (*p).ifa_addr;
            if !a.is_null() && (*a).sa_family as i32 == libc::AF_INET {
                let sin = &*(a as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !ip.is_loopback() && !ip.is_link_local() && !out.contains(&ip.to_string()) {
                    out.push(ip.to_string());
                }
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifa);
    }
    out
}

/// SHA-256 of the certificate (what a client shows on its first connection).
pub fn fingerprint(state: &Path) -> Option<String> {
    let o = std::process::Command::new("openssl")
        .args(["x509", "-noout", "-fingerprint", "-sha256", "-in"])
        .arg(state.join("tls/cert.pem"))
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&o.stdout);
    s.trim().split_once('=').map(|(_, f)| f.to_string())
}

pub fn version() -> String {
    match option_env!("MACRDP_COMMIT") {
        Some(c) => format!("{} ({c})", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// "Connect to the hyperMAC inside…": the guest's account, to trade for a token there.
pub fn pair_dialog(mtm: MainThreadMarker, vm: &str) -> Option<(String, String)> {
    let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(300.0, 60.0)));
    let label = |y: f64, name: &str| {
        let l = NSTextField::labelWithString(&ns(name), mtm);
        l.setFrame(NSRect::new(NSPoint::new(0.0, y + 2.0), NSSize::new(100.0, 20.0)));
        view.addSubview(&l);
    };
    label(34.0, t("用户名", "User name"));
    label(2.0, t("密码", "Password"));
    let user = NSTextField::initWithFrame(NSTextField::alloc(mtm), NSRect::new(NSPoint::new(110.0, 34.0), NSSize::new(180.0, 24.0)));
    let pass = NSSecureTextField::initWithFrame(NSSecureTextField::alloc(mtm), NSRect::new(NSPoint::new(110.0, 2.0), NSSize::new(180.0, 24.0)));
    view.addSubview(&user);
    view.addSubview(&pass);
    let text = if zh() {
        format!("先在虚拟机 {vm} 里装好 hyperMAC 并完成它的首次设置。这里输一次虚拟机里那个 Mac 账号的用户名和密码：只用来向它换一个 token，密码不保存。之后 kRDPclient 在这台 Mac 的列表里双击 {vm} 就直接进它的桌面。")
    } else {
        format!("Install hyperMAC inside the virtual machine {vm} first and finish its first-run setup. Type the user name and password of the Mac account in the virtual machine once: they are traded for a token there, the password is not kept. After that kRDPclient opens {vm}'s desktop from this Mac's list.")
    };
    let r = alert(mtm, t("连接虚拟机里的 hyperMAC", "Connect to the hyperMAC inside"), &text, &[t("配对", "Pair"), t("取消", "Cancel")], Some(&view), false);
    let (u, p) = (user.stringValue().to_string().trim().to_string(), pass.stringValue().to_string());
    (r == 0 && !u.is_empty() && !p.is_empty()).then_some((u, p))
}

/// "Ports…": the two ports the service listens on. None = cancelled or not two different ports.
pub fn ports_dialog(mtm: MainThreadMarker, rdp: u16, web: u16) -> Option<(u16, u16)> {
    let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(300.0, 60.0)));
    let field = |y: f64, name: &str, value: u16| {
        let l = NSTextField::labelWithString(&ns(name), mtm);
        l.setFrame(NSRect::new(NSPoint::new(0.0, y + 2.0), NSSize::new(190.0, 20.0)));
        let f = NSTextField::initWithFrame(NSTextField::alloc(mtm), NSRect::new(NSPoint::new(200.0, y), NSSize::new(90.0, 24.0)));
        f.setStringValue(&ns(&value.to_string()));
        view.addSubview(&l);
        view.addSubview(&f);
        f
    };
    let rdp_f = field(34.0, t("RDP（mstsc、kRDPclient 的画面）", "RDP (mstsc, kRDPclient's picture)"), rdp);
    let web_f = field(2.0, t("kRDPclient 添加主机（HTTPS）", "kRDPclient's Add host (HTTPS)"), web);
    let r = alert(
        mtm,
        t("端口", "Ports"),
        t(
            "默认是 3389 和 443。443 被别的程序占着时改这里；改了之后在 kRDPclient 里添加主机要写「地址:端口」。保存后服务会重启，正在连着的会话会断开。",
            "The defaults are 3389 and 443. Change 443 here when another program has it; kRDPclient then adds this host as address:port. Saving restarts the service: sessions in progress drop.",
        ),
        &[t("保存并重启服务", "Save and Restart"), t("取消", "Cancel")],
        Some(&view),
        false,
    );
    if r != 0 {
        return None;
    }
    let num = |f: &NSTextField| f.stringValue().to_string().trim().parse::<u16>().ok().filter(|n| *n > 0);
    match (num(&rdp_f), num(&web_f)) {
        (Some(a), Some(b)) if a != b => Some((a, b)),
        _ => {
            error(mtm, t("端口没有改", "The ports were not changed"), t("要两个不同的数字，1–65535。", "Two different numbers are needed, 1–65535."));
            None
        }
    }
}

pub fn token_dialog(mtm: MainThreadMarker, state: &Path) {
    let token = match crate::web::new_token(state, "hyperMAC") {
        Ok(t) => t,
        Err(e) => return error(mtm, t("没能生成 token", "Could not make a token"), &format!("{e:#}")),
    };
    let f = NSTextField::initWithFrame(
        NSTextField::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(420.0, 24.0)),
    );
    f.setStringValue(&ns(&token));
    f.setEditable(false);
    f.setSelectable(true);
    let r = alert(
        mtm,
        t("kRDPclient token", "kRDPclient token"),
        t(
            "在 kRDPclient 里「添加 keelOS 主机」，地址填这台 Mac，token 填下面这串。它只显示这一次（hyperMAC 只存它的哈希）。",
            "In kRDPclient, \"Add keelOS host\" with this Mac's address and the token below. It is shown only this once (hyperMAC keeps only its hash).",
        ),
        &[t("拷贝并关闭", "Copy and Close"), t("关闭", "Close")],
        Some(&f),
        false,
    );
    if r == 0 {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        unsafe { pb.setString_forType(&ns(&token), NSPasteboardTypeString) };
    }
}

/// hyperMAC's icon (assets/hypermac-1024.png, tools/mkicon.swift) as this process's: the
/// service and the menu bar run as a bare executable (no app bundle) in a development install.
pub fn set_app_icon(app: &NSApplication) {
    static PNG: &[u8] = include_bytes!("../assets/hypermac-1024.png");
    use objc2::AllocAnyThread;
    let data = objc2_foundation::NSData::with_bytes(PNG);
    if let Some(img) = objc2_app_kit::NSImage::initWithData(objc2_app_kit::NSImage::alloc(), &data) {
        unsafe { app.setApplicationIconImage(Some(&img)) };
    }
}
