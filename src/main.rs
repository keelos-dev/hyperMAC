//! hyperMAC: RDP server for the Mac's physical desktop (docs/design.md). M0: picture only —
//! TLS + a configured user/password, ScreenCaptureKit full frames in, IronRDP's encoder
//! does the diffing and RemoteFX; keyboard and mouse are logged, not injected yet.
//!
//!     hypermac [--listen ADDR:PORT] [--state DIR]
//!     hypermac passwd USER          # reads the password from stdin, stores it in <state>
//!
//! State (certificate, credentials) lives in --state, default
//! ~/Library/Application Support/hyperMAC. The connection skeleton follows keel-rdp
//! (keelOS/rdp/src/main.rs): one thread + current-thread runtime per connection, because
//! RdpServer's connection future is not Send.

mod account;
mod capture;
mod cursor;
mod clip;
mod clipimg;
mod display;
mod encoder;
mod gui;
mod input;
mod shutdown;
mod l2;
mod menubar;
mod settings;
mod sound;
mod tap;
mod ui;
mod update;
mod video;
mod vms;
mod web;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use ironrdp_server::{
    CredentialDecision, CredentialValidationError, CredentialValidator, Credentials, RdpServer, TlsIdentityCtx,
};
use tokio::net::TcpListener;
use tracing::{info, warn};

struct Args {
    listen: SocketAddr,
    /// the keelOS-host face (web.rs); kRDPclient adds this address
    web: SocketAddr,
    state: PathBuf,
}

fn state_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/tmp".into());
    home.join("Library/Application Support/hyperMAC")
}

fn args(it: impl Iterator<Item = String>) -> anyhow::Result<Args> {
    let mut a = Args { listen: "0.0.0.0:3389".parse()?, web: "0.0.0.0:443".parse()?, state: state_dir() };
    let (mut listen_given, mut web_given) = (false, false);
    let mut it = it.peekable();
    while let Some(k) = it.next() {
        let mut v = || it.next().with_context(|| format!("{k} needs a value"));
        match k.as_str() {
            "--listen" => {
                a.listen = v()?.parse().context("--listen")?;
                listen_given = true;
            }
            "--web" => {
                a.web = v()?.parse().context("--web")?;
                web_given = true;
            }
            "--state" => a.state = v()?.into(),
            _ => bail!("usage: hypermac [--listen ADDR:PORT] [--web ADDR:PORT] [--state DIR] | hypermac passwd USER | hypermac token | hypermac check-password [USER]"),
        }
    }
    // the ports set in the window (settings.rs), unless the command line names them
    let (rdp, web) = settings::ports(&a.state);
    if !listen_given {
        a.listen.set_port(rdp);
    }
    if !web_given {
        a.web.set_port(web);
    }
    Ok(a)
}

/// This Mac as the one "guest" the web face lists: a uuid minted once into <state>/uuid,
/// the short host name as the alias, the macOS version.
fn host_identity(state: &Path) -> anyhow::Result<(String, String, String)> {
    let path = state.join("uuid");
    let uuid = match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            use std::io::Read;
            let mut b = [0u8; 16];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
            (b[6], b[8]) = (b[6] & 0x0f | 0x40, b[8] & 0x3f | 0x80); // v4
            let h = web::hex(&b);
            let uuid = format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]);
            std::fs::create_dir_all(state)?;
            std::fs::write(&path, &uuid)?;
            uuid
        }
    };
    let out = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let alias = out("hostname", &["-s"]).unwrap_or_else(|| "mac".into()).to_lowercase();
    let os_version = out("sw_vers", &["-productVersion"]).unwrap_or_default();
    Ok((uuid, alias, os_version))
}

/// `hypermac passwd USER`: a hyperMAC-only password (development, scripts) from stdin's first line
/// into <state>/credentials.json (0600), instead of the Mac account the app sets up.
fn passwd(user: &str, state: &Path) -> anyhow::Result<()> {
    use std::io::BufRead;
    eprint!("password for {user}: ");
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let password = line.trim_end_matches(['\r', '\n']).to_string();
    if password.is_empty() {
        bail!("empty password");
    }
    let p = account::write(state, &account::Creds { user: user.into(), password: Some(password), nt_hash: None, pw_set: None })?;
    eprintln!("written to {}", p.display());
    Ok(())
}

/// The TLS-only path: the account of <state>/credentials.json (account.rs: the Mac user and
/// its login password, checked by the system; user name compared case-insensitively, a
/// "DOMAIN\user" or "user@x" spelling reduced to the user part, keel-rdp style), or
/// kRDPclient's way (keelOS docs/19): any user name with a token made by `hypermac token` as
/// the password. The NLA path uses the lookup in main().
struct Login {
    state: PathBuf,
    tokens: PathBuf,
    /// set when the user name names one of hyperMAC's VMs (vms.rs): the session shows it
    vm: vms::Slot,
}

#[async_trait::async_trait]
impl CredentialValidator for Login {
    async fn validate(&self, c: &Credentials) -> Result<CredentialDecision, CredentialValidationError> {
        let user = account::user_part(&c.username);
        let by_token = web::token_ok(&self.tokens, &c.password);
        // a VM of hyperMAC's (vms.rs): user name = the VM's name, password = a token (what
        // kRDPclient sends for a keel host's guest)
        if let Some(vm) = vms::openable(&user) {
            return match (vm, by_token) {
                (Ok(name), true) => {
                    info!(vm = name, "login to a VM");
                    *self.vm.lock().unwrap() = Some(name);
                    Ok(CredentialDecision::Accept)
                }
                (Ok(name), false) => {
                    warn!(vm = name, "VM login refused: a VM takes a token as its password");
                    Ok(CredentialDecision::Reject)
                }
                (Err(why), _) => {
                    warn!(user, "VM login refused: {why}");
                    Ok(CredentialDecision::Reject)
                }
            };
        }
        if !settings::host_desktop(&self.state) {
            warn!(user, "login refused: this Mac's own desktop is switched off (only the VMs are served)");
            return Ok(CredentialDecision::Reject);
        }
        let by_password = !by_token && !account::locked() && account::check(&self.state, Some(&c.username), &c.password);
        if by_password || by_token {
            info!(user, token = by_token, "login");
            Ok(CredentialDecision::Accept)
        } else {
            warn!(user, "login refused");
            Ok(CredentialDecision::Reject)
        }
    }
}

const AGENT_LABEL: &str = "dev.keelos.hypermac";

/// Where the LaunchAgent's copy of the binary lives: <state>/bin/hypermac.
fn installed_binary(state: &Path) -> PathBuf {
    state.join("bin/hypermac")
}

/// The menu bar's copy beside it: <state>/bin/hypermac-menu (`write_menu_binary`).
fn installed_menu_binary(state: &Path) -> PathBuf {
    state.join("bin/hypermac-menu")
}

/// Is this process the menu bar's copy (a development install)? To TCC it is another
/// executable: the grants that count are the service's (gui.rs `screen_ok`).
pub fn is_menu_copy() -> bool {
    std::env::current_exe().is_ok_and(|e| e.file_name().is_some_and(|n| n == "hypermac-menu"))
}

/// Write the menu bar's copy of `src` to `dst`: the same program with its own bundle id,
/// dev.keelos.hypermac.menu. macOS takes the menu bar icons of an app that is recording the
/// screen into its recording indicator, by bundle id (2026-11-10, seen: while a client was
/// connected the icon was gone, and so was a test program's with the same id); with an id of
/// its own the icon stays. build.rs leaves five spaces after the id for ".menu". Signed ad
/// hoc: the menu bar process needs no Screen Recording or Accessibility of its own.
fn write_menu_binary(src: &Path, dst: &Path) -> anyhow::Result<()> {
    // made at run time, so this program's own constants never match it
    let from = format!("<string>{AGENT_LABEL}</string>{}", " ".repeat(5)).into_bytes();
    let to = format!("<string>{MENU_LABEL}</string>").into_bytes();
    anyhow::ensure!(from.len() == to.len());
    let mut bin = std::fs::read(src).with_context(|| format!("read {}", src.display()))?;
    let mut found = 0;
    let mut i = 0;
    while let Some(p) = bin[i..].windows(from.len()).position(|w| w == from.as_slice()) {
        bin[i + p..i + p + to.len()].copy_from_slice(&to);
        found += 1;
        i += p + to.len();
    }
    anyhow::ensure!(found > 0, "{}: no bundle id to change (built before the menu bar copy?)", src.display());
    let tmp = dst.with_extension("new");
    std::fs::write(&tmp, &bin).with_context(|| format!("write {}", tmp.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    let st = std::process::Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-", "--identifier", MENU_LABEL])
        .arg(&tmp)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    anyhow::ensure!(st.success(), "codesign {}", tmp.display());
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

/// `hypermac menu-binary` (build.sh, after refreshing the installed copy): remake the menu
/// bar's copy from <state>/bin/hypermac and point its LaunchAgent at it.
fn refresh_menu_binary(state: &Path) -> anyhow::Result<()> {
    let menu = installed_menu_binary(state);
    write_menu_binary(&installed_binary(state), &menu)?;
    install_menu(state, &menu)?;
    Ok(())
}

/// The hyperMAC.app this executable sits in (`X.app/Contents/MacOS/hypermac`), if any.
fn app_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension().is_some_and(|e| e.eq_ignore_ascii_case("app")))
    .then(|| app.to_path_buf())
}

/// An app the LaunchAgent may point at: one in /Applications (not inside the dmg, not in
/// Downloads, not an App Translocation copy, all of which go away or move).
fn app_in_applications(app: &Path) -> bool {
    app.parent() == Some(Path::new("/Applications"))
}

fn agent_plist() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).context("HOME")?;
    Ok(home.join(format!("Library/LaunchAgents/{AGENT_LABEL}.plist")))
}

/// `hypermac install`: write a per-user LaunchAgent that starts hypermac at login and keeps it
/// up, then load it. A LaunchAgent (not a LaunchDaemon) because ScreenCaptureKit and CGEvent
/// need the user's GUI (Aqua) session (docs/design.md "会话与生命周期").
///
/// TCC caveat: launchd runs the binary directly, so the Screen Recording and Accessibility
/// grants given to Terminal do NOT carry over — grant them to this binary (its path shows in
/// the two Privacy lists). An ad-hoc-signed binary's grant is lost on the next rebuild; a
/// stable code-signing identity (later) would keep it.
fn install(state: &Path) -> anyhow::Result<()> {
    let built = std::env::current_exe()?.canonicalize()?;
    // asked from the menu bar's window: that process is the menu copy, the service is the other
    let built = if is_menu_copy() { installed_binary(state).canonicalize()? } else { built };
    let mut menu_exe = None;
    let exe = match app_bundle(&built) {
        // The release: the LaunchAgent runs the executable inside the app, so the Privacy
        // lists show hyperMAC.app (one grant for the app and the service). Only from
        // /Applications: the dmg is unmounted, a download is moved or deleted.
        Some(app) => {
            if !app_in_applications(&app) {
                bail!(
                    "{} is not in /Applications: drag hyperMAC.app into /Applications first, then run it from there",
                    app.display()
                );
            }
            built.clone()
        }
        // A development build: the service runs a copy at a fixed path, never the build
        // output: a rebuild into target/release while the service runs from it changes the
        // running process's file, its signature no longer validates and TCC refuses the
        // capture (2026-11-09, seen live). build.sh refreshes this copy (signed) when it exists.
        None => {
            let exe = installed_binary(state);
            std::fs::create_dir_all(exe.parent().context("bin dir")?)?;
            if built != exe {
                std::fs::copy(&built, &exe).with_context(|| format!("copy to {}", exe.display()))?;
            }
            let menu = installed_menu_binary(state);
            write_menu_binary(&exe, &menu)?;
            menu_exe = Some(menu);
            exe
        }
    };
    let plist = agent_plist()?;
    std::fs::create_dir_all(plist.parent().context("LaunchAgents dir")?)?;
    let log = state.join("hypermac.log");
    std::fs::create_dir_all(state)?;
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{AGENT_LABEL}</string>
  <key>ProgramArguments</key><array>
    <string>{exe}</string>
    <string>--state</string><string>{state}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>EnvironmentVariables</key><dict><key>RUST_LOG</key><string>info,ironrdp=warn</string></dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#,
        exe = exe.display(),
        state = state.display(),
        log = log.display(),
    );
    std::fs::write(&plist, xml)?;
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}/{AGENT_LABEL}");
    reload_agent(&target, &plist)?;
    // hyperMAC.app: the app's own executable for now, so its icon still steps aside for the
    // recording indicator while a client is connected (a helper app with its own id would not)
    install_menu(state, menu_exe.as_deref().unwrap_or(&exe))?;
    eprintln!("installed {}", plist.display());
    eprintln!("it starts at login and is running now.");
    eprintln!();
    if app_bundle(&exe).is_some() {
        eprintln!("Screen Recording and Accessibility go to hyperMAC.app (System Settings > Privacy & Security).");
        return Ok(());
    }
    eprintln!("IMPORTANT — grant these to the installed binary (NOT Terminal; TCC keys a bare");
    eprintln!("executable by its path, so this is needed once for this path), or the screen is");
    eprintln!("black and the keyboard/mouse do nothing:");
    eprintln!("  System Settings > Privacy & Security > Screen Recording  -> add + enable:");
    eprintln!("    {}", exe.display());
    eprintln!("  System Settings > Privacy & Security > Accessibility      -> add + enable it too");
    eprintln!("then: launchctl kickstart -k gui/{uid}/{AGENT_LABEL}");
    Ok(())
}

/// Is this launchd job loaded?
fn agent_loaded(target: &str) -> bool {
    std::process::Command::new("launchctl")
        .args(["print", target])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// (Re)load a LaunchAgent from its plist.
fn reload_agent(target: &str, plist: &Path) -> anyhow::Result<()> {
    let uid = unsafe { libc::getuid() };
    // Reload if already there: bootout returns before the old instance is gone, and a
    // bootstrap in that window fails with "5: Input/output error" (seen 2026-11-09) — wait
    // until launchctl no longer knows the service, then load.
    let _ = std::process::Command::new("launchctl").args(["bootout", target]).status();
    for _ in 0..50 {
        if !agent_loaded(target) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let st = std::process::Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{uid}"), plist.to_str().context("plist path")?])
        .status()?;
    anyhow::ensure!(st.success(), "launchctl bootstrap failed");
    Ok(())
}

/// The menu bar icon's LaunchAgent (menubar.rs): `hypermac menu` at login, in the GUI session.
pub const MENU_LABEL: &str = "dev.keelos.hypermac.menu";

fn menu_plist() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).context("HOME")?;
    Ok(home.join(format!("Library/LaunchAgents/{MENU_LABEL}.plist")))
}

/// Write the menu bar's LaunchAgent for `exe` and load it, unless it is already loaded as
/// written. Exiting on purpose (exit 0) leaves it off until the next login; a crash restarts it.
/// Called from the menu bar process itself (the window's switches), a changed plist is only
/// written: unloading the job would end this very process before it could load the new one.
/// true = (re)loaded now (so it is starting).
fn install_menu(state: &Path, exe: &Path) -> anyhow::Result<bool> {
    let plist = menu_plist()?;
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{MENU_LABEL}</string>
  <key>ProgramArguments</key><array>
    <string>{exe}</string>
    <string>menu</string>
    <string>--state</string><string>{state}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#,
        exe = exe.display(),
        state = state.display(),
        log = state.join("menu.log").display(),
    );
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}/{MENU_LABEL}");
    let same = std::fs::read_to_string(&plist).is_ok_and(|old| old == xml);
    if same && agent_loaded(&target) {
        return Ok(false);
    }
    std::fs::create_dir_all(plist.parent().context("LaunchAgents dir")?)?;
    std::fs::write(&plist, xml)?;
    if menubar::is_menu_process() {
        return Ok(false);
    }
    reload_agent(&target, &plist)?;
    Ok(true)
}

/// `hypermac uninstall`: unload and remove the LaunchAgents (the service, then the menu bar —
/// last, since the window asking for this may be the menu bar process itself).
fn uninstall() -> anyhow::Result<()> {
    let uid = unsafe { libc::getuid() };
    for (label, plist) in [(AGENT_LABEL, agent_plist()?), (MENU_LABEL, menu_plist()?)] {
        if plist.exists() {
            std::fs::remove_file(&plist)?;
        }
        eprintln!("removed {}", plist.display());
        let _ = std::process::Command::new("launchctl").args(["bootout", &format!("gui/{uid}/{label}")]).status();
    }
    Ok(())
}

/// The certificate to serve: <state>/tls/cert.pem + key.pem, self-signed, made on first
/// start. The client trusts it by fingerprint on first connect (kRDPclient's flow).
fn ensure_cert(state: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let dir = state.join("tls");
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    if !cert.exists() || !key.exists() {
        std::fs::create_dir_all(&dir)?;
        let host = std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "hypermac".into());
        let ck = rcgen::generate_simple_self_signed(vec![host])?;
        std::fs::write(&cert, ck.cert.pem())?;
        std::fs::write(&key, ck.signing_key.serialize_pem())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
        }
        info!("made a self-signed certificate in {}", dir.display());
    }
    Ok((cert, key))
}

/// keelOS::Video v2's second connection (docs/71 §4.8.1): TLS with the RDP certificate, then
/// `KEELVID2` + u32 session + 16-byte token from a TRANSPORT_OFFER; we answer u32 0 and from
/// then on write the FRAMEs here and read FRAME_ACK / KEYFRAME_REQUEST from here. The RDP
/// connection keeps CAPS/START/STOP, the sound, the input. Ends when either side closes.
async fn video_transport(
    acceptor: tokio_rustls::TlsAcceptor,
    tcp: tokio::net::TcpStream,
    offers: video::Offers,
    peer: SocketAddr,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut tls = acceptor.accept(tcp).await.context("TLS")?;
    let mut hello = [0u8; 28];
    tokio::time::timeout(std::time::Duration::from_secs(5), tls.read_exact(&mut hello))
        .await
        .context("hello timeout")?
        .context("hello")?;
    if &hello[..8] != video::VIDEO_MAGIC {
        bail!("not a video connection (bad magic)");
    }
    let session = u32::from_le_bytes(hello[8..12].try_into().unwrap());
    let video = {
        let mut o = offers.lock().unwrap();
        match o.remove(&session) {
            Some((token, v, until)) if token == hello[12..28] && until > std::time::Instant::now() => v,
            _ => None.context("unknown session or bad/expired token")?,
        }
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    if !video.attach_transport(session, tx) {
        tls.write_all(&1u32.to_le_bytes()).await?;
        bail!("session no longer wants a transport");
    }
    tls.write_all(&0u32.to_le_bytes()).await?;
    info!(%peer, session, "video transport up");
    let (mut rd, mut wr) = tokio::io::split(tls);
    // The reader is a task of its own: a read_exact future dropped by select! halfway through
    // a header would lose those bytes and misalign the stream (kRDPclient hit this on its
    // side, 2026-11-09). A JoinHandle is safe to select on.
    let mut reader = {
        let video = video.clone();
        tokio::spawn(async move {
            let mut head = [0u8; 8];
            loop {
                rd.read_exact(&mut head).await.context("read")?;
                let len = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
                anyhow::ensure!(len <= 1 << 20, "message too long");
                let mut whole = Vec::with_capacity(8 + len);
                whole.extend_from_slice(&head);
                whole.resize(8 + len, 0);
                rd.read_exact(&mut whole[8..]).await.context("read body")?;
                video.from_transport(&whole);
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        })
    };
    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
                m = rx.recv() => match m {
                    Some(m) => wr.write_all(&m).await.context("write")?,
                    None => return Ok(()), // the channel closed (DVC gone): done
                },
                r = &mut reader => {
                    // the client closed the connection (released it) or it broke
                    return match r {
                        Ok(inner) => inner,
                        Err(e) => Err(anyhow::anyhow!("reader task: {e}")),
                    };
                }
            }
        }
    }
    .await;
    reader.abort();
    video.detach_transport();
    info!(%peer, session, "video transport down");
    result
}

/// RemoteFX besides plain bitmaps (keel-rdp's default); MACRDP_CODECS=bitmap for plain only.
fn codecs() -> ironrdp_pdu::rdp::capability_sets::BitmapCodecs {
    use ironrdp_pdu::rdp::capability_sets::BitmapCodecs;
    match std::env::var("MACRDP_CODECS").as_deref() {
        Ok("bitmap") => BitmapCodecs(Vec::new()),
        _ => ironrdp_pdu::rdp::capability_sets::server_codecs_capabilities(&[]).expect("no options"),
    }
}

fn main() -> anyhow::Result<()> {
    // Finder (older macOS) adds a -psn_ argument to a double-clicked app; it means nothing here.
    let argv: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("-psn_")).collect();
    // Double-clicked hyperMAC.app: the setup / status window (gui.rs). The service itself is
    // started by launchd with --state (install), so it never gets here.
    if argv.is_empty() && std::env::current_exe().ok().and_then(|e| e.canonicalize().ok()).and_then(|e| app_bundle(&e)).is_some() {
        return gui::run(&state_dir());
    }
    // The menu bar icon (menubar.rs), started at login by its own LaunchAgent: an AppKit app,
    // so never the service process (see cursor.rs `bitmap`).
    if argv.first().map(String::as_str) == Some("menu") {
        let state = argv.iter().position(|s| s == "--state").and_then(|i| argv.get(i + 1)).map(PathBuf::from);
        return menubar::run(&state.unwrap_or_else(state_dir));
    }
    serve()
}

#[tokio::main]
async fn serve() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,ironrdp=warn".into()),
        )
        .init();
    let argv: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("-psn_")).collect();
    let sub_state = || {
        argv.iter()
            .position(|s| s == "--state")
            .and_then(|i| argv.get(i + 1))
            .map(PathBuf::from)
            .unwrap_or_else(state_dir)
    };
    match argv.first().map(String::as_str) {
        Some("passwd") => return passwd(argv.get(1).context("hypermac passwd USER")?, &sub_state()),
        Some("token") => return web::make_token(&sub_state()),
        Some("check-password") => {
            // does the system take this password for the Mac account? (stdin; nothing stored)
            use std::io::BufRead;
            let user = argv.get(1).cloned().unwrap_or_else(account::mac_user);
            eprint!("login password of {user}: ");
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            let ok = account::verify_system_password(&user, line.trim_end_matches(['\r', '\n']));
            println!("{}", if ok { "accepted" } else { "refused" });
            return Ok(());
        }
        Some("install") => return install(&sub_state()),
        Some("menu-binary") => return refresh_menu_binary(&sub_state()),
        Some("uninstall") => return uninstall(),
        _ => {}
    }
    let a = args(argv.into_iter())?;
    account::read(&a.state)?; // set up at all? (the logins read it afresh each time)
    // for the app's status page: which executable this service runs (gui.rs `upgrade`)
    if let Ok(exe) = std::env::current_exe() {
        let r = gui::Running { pid: std::process::id(), exe_mtime: gui::exe_mtime(&exe), version: gui::version() };
        let _ = std::fs::write(a.state.join("service.json"), serde_json::to_vec(&r).unwrap_or_default());
    }
    // how many connections are up, for the menu bar (shutdown.rs)
    shutdown::status_file(a.state.join("status.json"));
    // Remote access switched off (settings.rs): no port is opened. The process stays, so
    // launchd does not start it over and over; the window restarts it when the switch changes.
    if !settings::remote(&a.state) {
        info!("remote access is off: no port opened");
        return std::future::pending().await;
    }
    let (cert, key) = ensure_cert(&a.state)?;
    let tls = TlsIdentityCtx::init_from_paths(&cert, &key).context("TLS certificate")?;

    // The primary display: asking ScreenCaptureKit once up front also makes the missing
    // Screen Recording permission fail here, at startup, with a readable error.
    // (With this Mac's own desktop switched off the permission is not needed: the VMs' screens
    // do not come from ScreenCaptureKit, and no login reaches the Mac's.)
    let screen = match capture::primary() {
        Ok(s) => s,
        Err(_) if !settings::host_desktop(&a.state) => capture::Screen { id: 0, width: 1920, height: 1080 },
        Err(e) => return Err(e.context("ScreenCaptureKit (is Screen Recording allowed for hypermac?)")),
    };
    info!(id = screen.id, w = screen.width, h = screen.height, "primary display");

    let listener = TcpListener::bind(a.listen).await.with_context(|| format!("listen on {}", a.listen))?;
    info!("listening on {}", a.listen);

    // The keelOS-host face (web.rs): kRDPclient adds this address like a keelOS host and
    // sees one guest, this Mac. Its failure (the port taken, no tokens yet) leaves RDP alone.
    let (uuid, alias, os_version) = host_identity(&a.state)?;
    // kRDPclient's Connection > Discover keelOS finds this Mac too (l2/mod.rs): an OFFER to
    // each DISCOVER, and the web face below reachable over the L2 channel. Its failure leaves
    // the rest alone. MACRDP_L2=0 turns it off.
    if std::env::var("MACRDP_L2").as_deref() != Ok("0") {
        let model = std::process::Command::new("sysctl")
            .args(["-n", "hw.model"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let id = l2::Identity {
            uuid: uuid.clone(),
            hostname: alias.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            model,
            https_port: a.web.port(),
            spki: std::fs::read_to_string(&cert).ok().and_then(|pem| l2::spki_sha256_of_pem(&pem)),
        };
        if let Err(e) = l2::serve(id, a.web.port()) {
            warn!("L2 discovery: {e:#} — kRDPclient's Discover will not list this Mac; adding it by address still works");
        }
    }
    {
        let st = Arc::new(web::State {
            uuid,
            alias,
            os_version,
            rdp_port: a.listen.port(),
            tokens: a.state.join("tokens.json"),
            credentials: a.state.join("credentials.json"),
            sessions: Default::default(),
            fails: Default::default(),
        });
        let acceptor = tls.make_acceptor()?;
        let web_addr = a.web;
        tokio::spawn(async move {
            if let Err(e) = web::serve(web_addr, acceptor, st).await {
                warn!("web API: {e:#} — kRDPclient cannot add this host, RDP itself still works");
            }
        });
    }

    // keelOS::Video v2 (docs/71 §4.8.1): the frames may travel on a second TCP connection to
    // this same port, so the sound and input on the RDP connection never queue behind them
    // (TCP-only WAN: a lost video packet then stalls only the video). MACRDP_V2=0 turns it off.
    // SIGTERM (a restart of the Mac or of the service): every client is told (shutdown.rs)
    let signals = shutdown::install();
    let offers: Option<video::Offers> =
        (std::env::var("MACRDP_V2").as_deref() != Ok("0")).then(|| Arc::new(std::sync::Mutex::new(Default::default())));
    loop {
        let (stream, peer) = listener.accept().await?;
        // the menu bar's Pause Remote: new connections are turned away, the ones up stay
        if settings::get(&a.state, "paused", false) {
            info!(%peer, "paused (menu bar): connection refused");
            drop(stream);
            continue;
        }
        if let Err(e) = stream.set_nodelay(true) {
            warn!(%peer, "TCP_NODELAY: {e}");
        }
        // One port, two kinds of connection, told apart by the first byte: RDP starts with a
        // TPKT header (0x03); the video connection starts with TLS (ClientHello, 0x16).
        let mut first = [0u8; 1];
        let kind = tokio::time::timeout(std::time::Duration::from_secs(5), stream.peek(&mut first)).await;
        if let (Ok(Ok(1)), 0x16, Some(offers)) = (&kind, first[0], &offers) {
            let acceptor = tls.make_acceptor()?;
            let offers = offers.clone();
            tokio::spawn(async move {
                if let Err(e) = video_transport(acceptor, stream, offers, peer).await {
                    warn!(%peer, "video transport: {e:#}");
                }
            });
            continue;
        }
        let acceptor = tls.make_acceptor()?;
        let pub_key = tls.pub_key.clone();
        let listen = a.listen;
        let state = a.state.clone();
        let offers = offers.clone();
        let tokens = a.state.join("tokens.json");
        let stream = stream.into_std()?;
        let ending = signals.ending.clone();
        let kick = signals.kick.clone();
        // RdpServer's connection future is not Send: one thread with its own
        // single-threaded runtime per connection (keel-rdp's pattern).
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => return warn!(%peer, "runtime: {e}"),
            };
            rt.block_on(async move {
                let stream = match tokio::net::TcpStream::from_std(stream) {
                    Ok(s) => s,
                    Err(e) => return warn!(%peer, "{e}"),
                };
                let vm_slot: vms::Slot = Default::default();
                let login = Login { state: state.clone(), tokens, vm: vm_slot.clone() };
                // the Mac's system sound on RDPSND (sound.rs); the app's switch or MACRDP_SOUND=0 turns it off.
                let sound = settings::on(&state, "sound", "MACRDP_SOUND").then(sound::Sound::new);
                let (sound, sound_factory) = match sound {
                    Some((s, f)) => (Some(s), Some(Box::new(f) as Box<dyn ironrdp_server::SoundServerFactory>)),
                    None => (None, None),
                };
                // keelOS::Video (video.rs): kRDPclient opens it and gets H.264; mstsc refuses it
                // and stays on EGFX. The app's switch (settings.rs) or MACRDP_VIDEO=0 turns it off.
                // It also carries kRDPclient's sound switch (SOUND) to this connection's sound.
                let video = settings::on(&state, "video", "MACRDP_VIDEO").then(|| video::Video::new(offers.clone(), sound.clone()));
                let builder = RdpServer::builder()
                    .with_addr(listen)
                    // Hybrid = NLA (CredSSP, what mstsc wants) and TLS-only clients both (keel-rdp);
                    // plain TLS left mstsc with an empty user name and a protocol error
                    .with_hybrid(acceptor, pub_key)
                    // this Mac, or the VM the login named (vms.rs)
                    .with_input_handler(vms::Input { slot: vm_slot.clone(), mac: input::Input::new(), vm: vms::VmInput::new(vm_slot.clone()) })
                    .with_display_handler(vms::Display {
                        slot: vm_slot.clone(),
                        mac: display::Display::new(
                            screen,
                            video.as_ref().map(|(v, _)| v.clone()),
                            sound,
                            // "mute the Mac while remote" (tap.rs), its own switch, read live
                            tap::MuteSwitch(state.to_path_buf()),
                        ),
                        vm: vms::VmDisplay::new(vm_slot.clone()),
                        on_vm: None,
                    })
                    .with_credential_validator(Some(Arc::new(login)))
                    .with_bitmap_codecs(codecs())
                    .with_sound_factory(sound_factory)
                    // the clipboard, text both ways (clip.rs); the app's switch or MACRDP_CLIP=0 turns it off
                    .with_cliprdr_factory(settings::on(&state, "clip", "MACRDP_CLIP").then(|| {
                        let (_clip, factory) = clip::Clip::new();
                        Box::new(factory) as Box<dyn ironrdp_server::CliprdrServerFactory>
                    }));
                // MS-RDPEUDP2 reliable UDP beside TCP (keel-rdp, keelOS docs/71 §4.9.1): the
                // client that takes it (kRDPclient, mstsc) gets EGFX and keelOS::Video over UDP
                // on the same port number, so the big video frames no longer sit in front of the
                // sound waves and input on TCP (386 audio underruns in one session, 2026-11-09).
                // UDP that does not get through leaves everything on TCP. MACRDP_UDP=0 turns it off.
                let builder = match std::env::var("MACRDP_UDP").as_deref() {
                    Ok("0") => builder,
                    _ => builder.with_udp_transport(listen),
                };
                let mut server = builder.build();
                server.set_connection_local_addr(stream.local_addr().ok());
                if let Some((_, factory)) = video {
                    server.add_dvc_factory(Box::new(factory));
                }
                // Consume the client's Frame Acknowledge PDUs; without this each one is logged
                // as an "unexpected share data pdu" (keel-rdp main.rs does the same).
                server.set_display_refresh_handle(Some(std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0))));
                // The NLA path: NTLM needs the password's NT hash by user name. A wrong password
                // fails inside NTLM; a wrong user name returns None here.
                // The Mac account's hash is the one kept from its password (account.rs).
                let lookup: ironrdp_server::CredentialsLookup = Arc::new(move |user: &str| {
                    match account::nla_hash(&state, user) {
                        Some(h) => {
                            info!(user, "NLA login");
                            Some(ironrdp_server::LookupSecret::NtHash(h))
                        }
                        None => {
                            warn!(user, "NLA login refused: unknown user (or no password hash yet)");
                            None
                        }
                    }
                });
                server.set_credentials_lookup(Some(lookup));
                // told when the process is ending: Set Error Info (ServerShutdown), then the
                // regular disconnect; the menu bar's Disconnect: RpcInitiatedDisconnect, and the
                // service stays (shutdown.rs)
                let goodbye = {
                    let handle = server.error_info_disconnect_handle();
                    let (mut ending, mut kick) = (ending, kick);
                    kick.mark_unchanged(); // only a Disconnect asked from now on
                    tokio::spawn(async move {
                        use ironrdp_pdu::rdp::server_error_info::{ErrorInfo, ProtocolIndependentCode};
                        let code = tokio::select! {
                            r = ending.changed() => r.ok().map(|_| ProtocolIndependentCode::ServerShutdown),
                            r = kick.changed() => r.ok().map(|_| ProtocolIndependentCode::RpcInitiatedDisconnect),
                        };
                        if let Some(code) = code {
                            let _ = handle.disconnect(ErrorInfo::ProtocolIndependentCode(code));
                        }
                    })
                };
                let up = shutdown::Up::new();
                match server.run_connection(stream).await {
                    Ok(()) => info!(%peer, "disconnected"),
                    Err(e) => warn!(%peer, "connection ended: {e}"),
                }
                drop(up);
                goodbye.abort();
            });
        });
    }
}
