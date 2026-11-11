//! Who may log in (user 2026-11-09: "就是用户自己的密码"): the Mac user's own account and
//! login password. macOS never hands out the password (only a salted hash, root-only), but it
//! checks one that is typed: OpenDirectory's `verifyPassword`.
//!
//! <state>/credentials.json (0600), one account:
//!   {"user": "ben", "nt_hash": "<32 hex>"}   the Mac account (made by the app's setup). The
//!       password is checked by the system at every TLS / web login, so a changed Mac password
//!       works at once there. NLA (mstsc) can only work from the password's NT hash, which
//!       macOS does not keep: we keep it, made from the password typed at setup, and renewed
//!       whenever a login with the password succeeds through the system (so after a password
//!       change one kRDPclient login, or reopening the app, brings mstsc along).
//!       With it, "pw_set" = the account's passwordLastSetTime when the hash was made: once the
//!       Mac password changes, the kept hash is dead at once (NLA refuses), so the OLD password
//!       never keeps working for mstsc. Only the current Mac password ever logs in.
//!   {"user": "ben", "password": "..."}        a hyperMAC-only password (`hypermac passwd USER`,
//!       development and older installs), compared as before.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use objc2::rc::autoreleasepool;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::msg_send;
use objc2_foundation::NSString;
use sha2::{Digest, Sha256};
use tracing::warn;

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
pub struct Creds {
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nt_hash: Option<String>,
    /// passwordLastSetTime of the Mac account when nt_hash was made
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pw_set: Option<String>,
}

impl Creds {
    /// The Mac's own account (no hyperMAC-only password).
    pub fn is_system(&self) -> bool {
        self.password.is_none()
    }

    /// The Mac account's kept NT hash still belongs to its current password.
    pub fn hash_current(&self) -> bool {
        self.nt_hash.is_some() && self.pw_set.is_some() && self.pw_set == password_set_time(&self.user)
    }
}

/// When the account's password was last set (accountPolicyData's passwordLastSetTime, which
/// the user itself may read), as macOS writes it. None: not readable.
pub fn password_set_time(user: &str) -> Option<String> {
    let o = std::process::Command::new("/usr/bin/dscl")
        .args([".", "-read", &format!("/Users/{user}"), "accountPolicyData"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&o.stdout);
    let after = s.split("<key>passwordLastSetTime</key>").nth(1)?;
    let v = after.split('>').nth(1)?.split('<').next()?.trim();
    (!v.is_empty()).then(|| v.to_string())
}

pub fn path(state: &Path) -> PathBuf {
    state.join("credentials.json")
}

pub fn read(state: &Path) -> anyhow::Result<Creds> {
    serde_json::from_slice(
        &std::fs::read(path(state))
            .with_context(|| format!("{} (open hyperMAC.app to set it up, or: hypermac passwd USER)", path(state).display()))?,
    )
    .context("credentials.json")
}

/// Written whole to a new file (0600 from the start) and renamed over the old one.
pub fn write(state: &Path, c: &Creds) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(state)?;
    let p = path(state);
    let tmp = state.join("credentials.json.new");
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(c)?)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &p)?;
    Ok(p)
}

/// The short name of the user this process runs as (the Mac account the service serves).
pub fn mac_user() -> String {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_name.is_null() {
            if let Ok(s) = std::ffi::CStr::from_ptr((*pw).pw_name).to_str() {
                return s.to_string();
            }
        }
    }
    std::env::var("USER").unwrap_or_default()
}

/// NTLM's NT hash: MD4 of the UTF-16LE password (MS-NLMP 3.3.1), for the NLA/CredSSP path
/// (keel-rdp auth.rs). NLA validates the password by NTLM against this hash.
pub fn nt_hash(password: &str) -> [u8; 16] {
    use md4::{Digest as _, Md4};
    let b: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    Md4::digest(&b).into()
}

/// The user part of a "DOMAIN\user" / "user@host" spelling some clients send.
pub fn user_part(username: &str) -> &str {
    username.rsplit('\\').next().unwrap_or("").split('@').next().unwrap_or("")
}

#[link(name = "OpenDirectory", kind = "framework")]
unsafe extern "C" {}

/// Does macOS accept this password for this local account? (OpenDirectory, the check the
/// login window and Screen Sharing use; the system's own failed-login policy applies.)
pub fn verify_system_password(user: &str, password: &str) -> bool {
    if user.is_empty() || password.is_empty() {
        return false;
    }
    autoreleasepool(|_| unsafe {
        let (Some(session_cls), Some(node_cls)) = (AnyClass::get(c"ODSession"), AnyClass::get(c"ODNode")) else {
            warn!("OpenDirectory classes not found");
            return false;
        };
        let session: *mut AnyObject = msg_send![session_cls, defaultSession];
        if session.is_null() {
            return false;
        }
        // "/Search": the authentication search node (kODNodeTypeAuthentication by name; the
        // selector with type: would need `type` as a keyword in msg_send!)
        let err: *mut *mut AnyObject = std::ptr::null_mut();
        let search = NSString::from_str("/Search");
        let node: *mut AnyObject = msg_send![node_cls, nodeWithSession: session, name: &*search, error: err];
        if node.is_null() {
            return false;
        }
        let kind = NSString::from_str("dsRecTypeStandard:Users"); // kODRecordTypeUsers
        let name = NSString::from_str(user);
        let none: *const AnyObject = std::ptr::null();
        let record: *mut AnyObject =
            msg_send![node, recordWithRecordType: &*kind, name: &*name, attributes: none, error: err];
        if record.is_null() {
            return false;
        }
        let pw = NSString::from_str(password);
        let ok: Bool = msg_send![record, verifyPassword: &*pw, error: err];
        ok.as_bool()
    })
}

/// Failed password logins (RDP's TLS path; the web login keeps its own count): this many within
/// the window refuse every password until it ends. Tokens are not counted or refused.
const FAILS: u32 = 5;
const WINDOW: std::time::Duration = std::time::Duration::from_secs(300);
static FAILED: std::sync::Mutex<(u32, Option<std::time::Instant>)> = std::sync::Mutex::new((0, None));

pub fn locked() -> bool {
    let mut f = FAILED.lock().unwrap_or_else(|e| e.into_inner());
    if f.1.is_some_and(|since| since.elapsed() > WINDOW) {
        *f = (0, None);
    }
    f.0 >= FAILS
}

fn count(ok: bool) {
    let mut f = FAILED.lock().unwrap_or_else(|e| e.into_inner());
    if ok {
        *f = (0, None);
    } else {
        f.0 += 1;
        f.1.get_or_insert_with(std::time::Instant::now);
    }
}

/// A user name (None = any, the web login sends none) and password against the account.
/// The Mac account: the system checks the password, and a success renews the NT hash kept for
/// NLA when the Mac password has changed.
pub fn check(state: &Path, user: Option<&str>, password: &str) -> bool {
    let Ok(c) = read(state) else { return false };
    if user.is_some_and(|u| !user_part(u).eq_ignore_ascii_case(&c.user)) || password.is_empty() {
        return false;
    }
    let ok = match &c.password {
        // compared by digest, so the time taken does not tell how many characters matched
        Some(want) => Sha256::digest(password.as_bytes()) == Sha256::digest(want.as_bytes()),
        None => {
            let ok = verify_system_password(&c.user, password);
            let h = crate::web::hex(&nt_hash(password));
            let set = password_set_time(&c.user);
            if ok && (c.nt_hash.as_deref() != Some(h.as_str()) || c.pw_set != set) {
                let renewed = Creds { nt_hash: Some(h), pw_set: set, ..c.clone() };
                if let Err(e) = write(state, &renewed) {
                    warn!("could not renew the NLA hash: {e:#}");
                }
            }
            ok
        }
    };
    count(ok);
    ok
}

/// The NT hash for the NLA path, by the user name the client sent (None: not our user, or no
/// hash yet: the account was never confirmed with its password since it was set up).
pub fn nla_hash(state: &Path, user: &str) -> Option<[u8; 16]> {
    let c = read(state).ok()?;
    if !user_part(user).eq_ignore_ascii_case(&c.user) {
        return None;
    }
    match (&c.password, &c.nt_hash) {
        (Some(p), _) => Some(nt_hash(p)),
        (None, Some(_)) if !c.hash_current() => {
            warn!(user, "NLA: the Mac password changed since its hash was kept: log in once with the new one (kRDPclient, or the app)");
            None
        }
        (None, Some(h)) => {
            let b: Vec<u8> = (0..h.len() / 2).filter_map(|i| u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok()).collect();
            b.try_into().ok()
        }
        (None, None) => None,
    }
}

/// The app's setup: the Mac account with its login password, checked by the system first.
pub fn set_system(state: &Path, user: &str, password: &str) -> anyhow::Result<()> {
    if password.is_empty() {
        bail!("empty password");
    }
    if !verify_system_password(user, password) {
        bail!("not this Mac's password for {user}");
    }
    write(
        state,
        &Creds {
            user: user.into(),
            password: None,
            nt_hash: Some(crate::web::hex(&nt_hash(password))),
            pw_set: password_set_time(user),
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_password_refuses_wrong_ones() {
        let me = mac_user();
        assert!(!me.is_empty());
        assert!(!verify_system_password(&me, "definitely not the password 9f8e7d"));
        assert!(!verify_system_password("no-such-user-hypermac-test", "x"));
        assert!(!verify_system_password(&me, ""));
    }

    #[test]
    fn nla_hash_from_either_kind() {
        let dir = std::env::temp_dir().join(format!("hypermac-acct-{}", std::process::id()));
        write(&dir, &Creds { user: "ben".into(), password: Some("pw".into()), nt_hash: None, pw_set: None }).unwrap();
        assert_eq!(nla_hash(&dir, "MAC\\Ben"), Some(nt_hash("pw")));
        assert_eq!(nla_hash(&dir, "other"), None);
        // the Mac account: the hash counts only while the password is the one it was made from
        let me = mac_user();
        let now = password_set_time(&me);
        assert!(now.is_some(), "passwordLastSetTime readable");
        let h = crate::web::hex(&nt_hash("sys"));
        write(&dir, &Creds { user: me.clone(), password: None, nt_hash: Some(h.clone()), pw_set: now }).unwrap();
        assert_eq!(nla_hash(&dir, &me), Some(nt_hash("sys")));
        write(&dir, &Creds { user: me.clone(), password: None, nt_hash: Some(h), pw_set: Some("1".into()) }).unwrap();
        assert_eq!(nla_hash(&dir, &me), None); // password changed since: refused
        write(&dir, &Creds { user: "ben".into(), password: None, nt_hash: Some("00".repeat(16)), pw_set: None }).unwrap();
        assert!(!check(&dir, None, "sys")); // the system says no: ben is not this Mac's account
        let _ = std::fs::remove_dir_all(&dir);
    }
}
