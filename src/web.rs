//! The keelOS-host face of hyperMAC (user 2026-11-09: "可以将hyperMAC等同keelOS的角色"):
//! just enough of keel's API that kRDPclient adds this Mac like a keelOS host and shows
//! one "guest" — the Mac's desktop. Same TLS certificate as the RDP side, so the
//! fingerprint kRDPclient pins from the web page matches the RDP connection.
//!
//! Served (kRDPclient's client is reqwest, HTTP/1.1 over TLS, no ALPN here):
//!   GET /v1/version                 no token; kRDPclient only wants the certificate
//!   GET /v1/vms                     Bearer token -> [ the one guest ]
//!   GET /v1/vms/{uuid}              the same guest
//!   GET /v1/vms/{uuid}/consoles     {"running":true,"vnc":false}
//!   POST /login                     {"password", "user"?} = the hyperMAC account (account.rs:
//!                                   this Mac's user and login password) -> a short web session
//!                                   (cookie + csrf)
//!   POST /v1/tokens                 with that session -> {"secret": a new token}
//!   POST /logout                    ends the session
//! Everything else is 404 (kRDPclient treats /v1/host as optional), power actions 403.
//!
//! The three POSTs are keel's own login flow, as far as kRDPclient's join uses it (its
//! keel/join.rs: a password typed where the token goes in Add keelOS Host): the password is
//! traded for a token named for that PC, the password itself is not kept by the client.
//! keel's login has no user name (its one administrator), and kRDPclient sends none; hyperMAC
//! has one account too, so the password alone names it. A client that does send "user" must
//! send the account's name (compared like the RDP login: case-insensitive, DOMAIN\ and @host
//! dropped).

use std::sync::Arc;

use anyhow::Context;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, warn};

pub struct State {
    pub uuid: String,
    pub alias: String,
    pub os_version: String,
    pub rdp_port: u16,
    pub tokens: std::path::PathBuf,
    /// <state>/credentials.json, read at each login so `hypermac passwd` takes effect at once
    pub credentials: std::path::PathBuf,
    /// web sessions made by POST /login: access cookie -> (csrf, made at)
    pub sessions: std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
    /// failed logins: how many, and since when (LOGIN_FAILS within LOGIN_WINDOW locks the door)
    pub fails: std::sync::Mutex<(u32, Option<std::time::Instant>)>,
}

/// A web session lives this long; kRDPclient's join ends it itself within a second.
const SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(600);
/// This many refused logins within LOGIN_WINDOW refuse every login until the window ends.
const LOGIN_FAILS: u32 = 5;
const LOGIN_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);
/// keel's names (keel/internal/web/server.go), which kRDPclient's join.rs uses.
const ACCESS_COOKIE: &str = "keel_access";
const CSRF_HEADER: &str = "x-keel-csrf";

#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct Tokens {
    pub sha256: Vec<String>,
    /// by hash: what the window's token list shows. A token from before this has none.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub info: std::collections::BTreeMap<String, TokenInfo>,
}

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
pub struct TokenInfo {
    /// the 12 hex digits in the token itself (keel_<id>_…): how its owner can tell it
    pub id: String,
    /// who asked for it: kRDPclient's name for itself, or "hyperMAC" (the window, the CLI)
    #[serde(default)]
    pub name: String,
    /// seconds since 1970; 0 = not known
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub last_used: u64,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Take a token away (the window's list): its hash and what is known of it.
pub fn revoke_token(state: &std::path::Path, hash: &str) -> anyhow::Result<()> {
    let path = state.join("tokens.json");
    let mut t = read_tokens(&path);
    t.sha256.retain(|h| !h.eq_ignore_ascii_case(hash));
    t.info.remove(hash);
    std::fs::write(&path, serde_json::to_vec_pretty(&t)?)?;
    Ok(())
}

/// Is this bearer token one of ours? (tokens.json holds only SHA-256 of each token.)
pub fn token_ok(tokens: &std::path::Path, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let want = hex(&Sha256::digest(token.as_bytes()));
    let mut t = read_tokens(tokens);
    if !t.sha256.iter().any(|h| h.eq_ignore_ascii_case(&want)) {
        return false;
    }
    // "last used" for the window's list, to the minute (kRDPclient asks every few seconds)
    let now = now();
    let info = t.info.entry(want).or_default();
    if now > info.last_used + 60 {
        info.last_used = now;
        if let Ok(b) = serde_json::to_vec_pretty(&t) {
            let _ = std::fs::write(tokens, b);
        }
    }
    true
}

pub fn read_tokens(path: &std::path::Path) -> Tokens {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `hypermac token`: make a token, remember its hash, print it once. keel's own spelling
/// (`keel_<12 hex>_<43 base64url>`) so kRDPclient's looks_like_token takes it as a token,
/// not as an administrator password (hosts.rs; the join flow is not served here).
pub fn make_token(state: &std::path::Path) -> anyhow::Result<()> {
    let token = new_token(state, "hyperMAC")?;
    println!("{token}");
    eprintln!("(its SHA-256 went into {}; the token itself is not stored)", state.join("tokens.json").display());
    Ok(())
}

/// A new token, its hash added to <state>/tokens.json (the CLI's `hypermac token` and the app's button).
pub fn new_token(state: &std::path::Path, name: &str) -> anyhow::Result<String> {
    let (mut id, mut secret) = ([0u8; 6], [0u8; 32]);
    getrandom(&mut id)?;
    getrandom(&mut secret)?;
    let token = format!("keel_{}_{}", hex(&id), b64url(&secret));
    let path = state.join("tokens.json");
    let mut t = read_tokens(&path);
    let hash = hex(&Sha256::digest(token.as_bytes()));
    t.info.insert(hash.clone(), TokenInfo { id: hex(&id), name: name.chars().take(60).collect(), created: now(), last_used: 0 });
    t.sha256.push(hash);
    std::fs::create_dir_all(state)?;
    std::fs::write(&path, serde_json::to_vec_pretty(&t)?)?;
    Ok(token)
}

fn getrandom(b: &mut [u8]) -> anyhow::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(b).context("urandom")
}

/// base64url without padding (RFC 4648 §5): 32 bytes -> the 43 characters keel's token carries.
fn b64url(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for i in 0..1 + c.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// status, JSON body, a Set-Cookie value
type Answer = (u16, serde_json::Value, Option<String>);

fn refusal(code: u16, tag: &str, error: &str) -> Answer {
    (code, serde_json::json!({"code": tag, "error": error}), None)
}

/// POST /login: the hyperMAC account for a web session. Which account: the one in
/// credentials.json. "user" is optional (see the module text); a wrong name and a wrong
/// password are refused alike, so the answer does not tell which it was.
fn login(st: &State, body: &[u8]) -> Answer {
    let now = std::time::Instant::now();
    let mut fails = st.fails.lock().unwrap_or_else(|e| e.into_inner());
    if fails.1.is_some_and(|since| now.duration_since(since) > LOGIN_WINDOW) {
        *fails = (0, None);
    }
    if fails.0 >= LOGIN_FAILS {
        return refusal(429, "too_many_logins", "too many failed logins: wait a few minutes");
    }
    let field = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
    let sent = serde_json::from_slice::<serde_json::Value>(body).unwrap_or_default();
    let state_dir = st.credentials.parent().unwrap_or(std::path::Path::new("."));
    let user = field(&sent, "user").filter(|u| !u.is_empty());
    // the Mac account's password is checked by the system (account.rs)
    let ok = crate::account::check(state_dir, user.as_deref(), &field(&sent, "password").unwrap_or_default());
    if !ok {
        fails.0 += 1;
        fails.1.get_or_insert(now);
        warn!(failed = fails.0, "web login refused");
        return refusal(401, "bad_password", "wrong password");
    }
    *fails = (0, None);
    drop(fails);
    let (mut access, mut csrf) = ([0u8; 32], [0u8; 16]);
    if getrandom(&mut access).is_err() || getrandom(&mut csrf).is_err() {
        return refusal(500, "internal", "no random numbers");
    }
    let (access, csrf) = (hex(&access), hex(&csrf));
    let mut sessions = st.sessions.lock().unwrap_or_else(|e| e.into_inner());
    sessions.retain(|_, (_, made)| now.duration_since(*made) < SESSION_TTL);
    sessions.insert(access.clone(), (csrf.clone(), now));
    info!("web login");
    (
        200,
        serde_json::json!({"csrf": csrf}),
        Some(format!("{ACCESS_COOKIE}={access}; Path=/; Secure; HttpOnly; SameSite=Strict")),
    )
}

/// Does this request carry a live web session (the cookie and its csrf)? -> the cookie's value.
fn session(st: &State, headers: &[(String, String)]) -> Option<String> {
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    let access = header("cookie")?
        .split(';')
        .find_map(|c| c.trim().strip_prefix(ACCESS_COOKIE)?.strip_prefix('='))?
        .to_owned();
    let csrf = header(CSRF_HEADER)?;
    let sessions = st.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let (want, made) = sessions.get(&access)?;
    (want == csrf && made.elapsed() < SESSION_TTL).then_some(access)
}

/// One connection: read one request, answer it, close (Connection: close).
async fn handle(stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>, st: &State) {
    let mut stream = stream;
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
        if buf.len() > 16 * 1024 {
            return;
        }
    }
    let head_len = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(0) + 4;
    let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
    let mut lines = head.lines();
    let (method, path) = match lines.next().and_then(|l| {
        let mut it = l.split_whitespace();
        Some((it.next()?.to_owned(), it.next()?.to_owned()))
    }) {
        Some(x) => x,
        None => return,
    };
    let path = path.split('?').next().unwrap_or("");
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    let bearer = header("authorization")
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .unwrap_or("")
        .to_owned();
    // the body, for the POSTs (a password or a token's name: small)
    let want = header("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    if want > 16 * 1024 {
        return;
    }
    while buf.len() < head_len + want {
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    let body_in = &buf[head_len..head_len + want];

    let vm = serde_json::json!({
        "uuid": st.uuid,
        "alias": st.alias,
        "state": "running",
        "os": { "family": "macos", "name": "macOS", "version": st.os_version },
        "rdp": { "port": st.rdp_port },
    });
    let plain = |code: u16, body: serde_json::Value| -> Answer { (code, body, None) };
    let (code, body, cookie) = if method != "GET" && path.starts_with("/v1/vms/") {
        plain(403, serde_json::json!({"error": "hyperMAC: the Mac's power is not keel's to switch"}))
    } else if path == "/v1/version" {
        // build: the commit in the release's name (a development build has none)
        plain(200, serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "product": "hyperMAC", "build": option_env!("MACRDP_COMMIT").unwrap_or("dev")}))
    } else if method == "POST" && path == "/login" {
        login(st, body_in)
    } else if method == "POST" && path == "/login/setup" {
        refusal(409, "setup_done", "hyperMAC uses this Mac's own account: log in with its password")
    } else if method == "POST" && path == "/v1/tokens" {
        let state_dir = st.tokens.parent().unwrap_or(std::path::Path::new("."));
        let name = serde_json::from_slice::<serde_json::Value>(body_in)
            .ok()
            .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_owned))
            .unwrap_or_default();
        match session(st, &headers).map(|_| new_token(state_dir, if name.is_empty() { "kRDPclient" } else { &name })) {
            Some(Ok(token)) => {
                info!(name, "web: token made");
                plain(200, serde_json::json!({"secret": token, "name": name}))
            }
            Some(Err(e)) => plain(500, serde_json::json!({"error": format!("{e:#}")})),
            None => plain(401, serde_json::json!({"error": "log in first (POST /login), then send the cookie and X-Keel-CSRF"})),
        }
    } else if method == "POST" && path == "/logout" {
        if let Some(access) = session(st, &headers) {
            st.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(&access);
        }
        plain(200, serde_json::json!({}))
    } else if !token_ok(&st.tokens, &bearer) {
        plain(401, serde_json::json!({"error": "a token made by `hypermac token` goes in Authorization: Bearer"}))
    } else if path == "/v1/vms" {
        // this Mac (unless its own desktop is switched off, settings.rs), then hyperMAC's VMs
        let host = st.tokens.parent().is_none_or(crate::settings::host_desktop);
        let mut all = if host { vec![vm] } else { Vec::new() };
        all.extend(crate::vms::guests(st.rdp_port));
        plain(200, serde_json::Value::Array(all))
    } else if path == format!("/v1/vms/{}", st.uuid) {
        plain(200, vm)
    } else if path == format!("/v1/vms/{}/consoles", st.uuid) {
        plain(200, serde_json::json!({"running": true, "vnc": false}))
    } else {
        plain(404, serde_json::json!({"error": "hyperMAC serves /v1/version, /v1/vms, /login and /v1/tokens only"}))
    };
    debug!(method, path, code, "web");
    let body = body.to_string();
    let reason = match code {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Not Found",
    };
    let cookie = cookie.map(|c| format!("Set-Cookie: {c}\r\n")).unwrap_or_default();
    let _ = stream
        .write_all(
            format!(
                "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\n{cookie}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;
    let _ = stream.shutdown().await;
}

/// The web face, on the RDP certificate. Runs on the main runtime.
pub async fn serve(
    listen: std::net::SocketAddr,
    acceptor: tokio_rustls::TlsAcceptor,
    st: Arc<State>,
) -> anyhow::Result<()> {
    let l = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("web: listen on {listen}"))?;
    info!("web API on https://{listen} (add this address in kRDPclient)");
    loop {
        let Ok((tcp, peer)) = l.accept().await else { continue };
        let acceptor = acceptor.clone();
        let st = st.clone();
        tokio::spawn(async move {
            match acceptor.accept(tcp).await {
                Ok(tls) => handle(tls, &st).await,
                Err(e) => warn!(%peer, "web TLS: {e}"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(dir: &std::path::Path) -> State {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("credentials.json"), br#"{"user":"ben","password":"s3cret"}"#).unwrap();
        State {
            uuid: "u".into(),
            alias: "mac".into(),
            os_version: "26".into(),
            rdp_port: 3389,
            tokens: dir.join("tokens.json"),
            credentials: dir.join("credentials.json"),
            sessions: Default::default(),
            fails: Default::default(),
        }
    }

    #[test]
    fn password_becomes_a_session_and_a_token() {
        let dir = std::env::temp_dir().join(format!("hypermac-web-test-{}", std::process::id()));
        let st = state(&dir);
        assert_eq!(login(&st, br#"{"password":"nope"}"#).0, 401);
        assert_eq!(login(&st, br#"{"password":"s3cret","user":"someone"}"#).0, 401);
        assert_eq!(login(&st, br#"{"password":"s3cret","user":"MAC\\Ben"}"#).0, 200);
        let (code, body, cookie) = login(&st, br#"{"password":"s3cret"}"#);
        assert_eq!(code, 200);
        let csrf = body["csrf"].as_str().unwrap().to_owned();
        let cookie = cookie.unwrap().split(';').next().unwrap().to_owned();
        let with = |csrf: &str| vec![("cookie".to_owned(), cookie.clone()), (CSRF_HEADER.to_owned(), csrf.to_owned())];
        assert!(session(&st, &with("wrong")).is_none());
        assert!(session(&st, &with(&csrf)).is_some());
        let token = new_token(&dir, "test").unwrap();
        assert!(token_ok(&st.tokens, &token));
        // five refusals lock the door, even for the right password
        for _ in 0..LOGIN_FAILS {
            login(&st, br#"{"password":"nope"}"#);
        }
        assert_eq!(login(&st, br#"{"password":"s3cret"}"#).0, 429);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
