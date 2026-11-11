//! hyperMAC inside a macOS virtual machine of another hyperMAC: the guest serves its own desktop
//! (the host cannot take a macOS guest's picture), and tells its host how to reach it, with no
//! pairing step for the user (TODO-auto-pair.md, 2026-11-11).
//!
//! The channel is a directory the host shares with this VM alone (vm/src/vm.rs: a virtio-fs
//! device with macOS's automount tag, the VM's `share` folder on the host; the guest sees it
//! as /Volumes/My Shared Files). The host puts `hypermac-host` in it; this service, when it
//! runs in a virtual machine and finds that file, writes `hypermac-guest.json` beside it:
//!
//!   { "token": "keel_…", "cert_sha256": "…", "rdp_port": 3389, "build": "…" }
//!
//! token = one of this hyperMAC's own, made for the host and kept in <state>/host-pairing.json
//! so the same one is written each time; a new one when it was revoked here or when the
//! machine is another (a copy of the VM: kern.uuid differs). The host reads the file where it
//! read the manual pairing (src/vms.rs `direct`) and passes it on as `rdp_direct`.
//! Only somebody who can read the VM's folder on the host gets the token: an address on the
//! network proves nothing, this directory does.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub const SHARE: &str = "/Volumes/My Shared Files";
pub const HOST_MARK: &str = "hypermac-host";
pub const GUEST_FILE: &str = "hypermac-guest.json";

fn sysctl_string(name: &std::ffi::CStr) -> Option<String> {
    let mut buf = [0u8; 128];
    let mut len: libc::size_t = buf.len();
    let r = unsafe { libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    (r == 0).then(|| String::from_utf8_lossy(&buf[..len]).trim_end_matches('\0').to_string())
}

/// Is this Mac a virtual machine? (`kern.hv_vmm_present`)
pub fn is_guest() -> bool {
    let (mut v, mut len): (libc::c_int, libc::size_t) = (0, std::mem::size_of::<libc::c_int>());
    let r = unsafe { libc::sysctlbyname(c"kern.hv_vmm_present".as_ptr(), (&mut v as *mut libc::c_int).cast(), &mut len, std::ptr::null_mut(), 0) };
    r == 0 && v == 1
}

/// The token for the host: the one kept, while it is still good here and this is the machine
/// it was made on; else a new one (the old one revoked).
fn host_token(state: &Path) -> anyhow::Result<String> {
    let keep = state.join("host-pairing.json");
    let machine = sysctl_string(c"kern.uuid").unwrap_or_default();
    let kept: Option<serde_json::Value> = std::fs::read(&keep).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let old = kept.as_ref().and_then(|k| k.get("token")).and_then(|t| t.as_str()).map(str::to_owned);
    let hash_of = |t: &str| crate::web::hex(&Sha256::digest(t.as_bytes()));
    if let Some(t) = &old {
        let same_machine = kept.as_ref().and_then(|k| k.get("machine")).and_then(|m| m.as_str()) == Some(machine.as_str());
        let known = crate::web::read_tokens(&state.join("tokens.json")).sha256.iter().any(|h| h.eq_ignore_ascii_case(&hash_of(t)));
        if same_machine && known {
            return Ok(t.clone());
        }
        let _ = crate::web::revoke_token(state, &hash_of(t));
    }
    let token = crate::web::new_token(state, "host (automatic)")?;
    let tmp = keep.with_extension("new");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&serde_json::json!({"token": token, "machine": machine}))?)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, &keep)?;
    Ok(token)
}

/// SHA-256 of the certificate (DER) in a PEM file, lowercase hex.
fn cert_sha256(pem: &Path) -> Option<String> {
    let text = std::fs::read_to_string(pem).ok()?;
    let b64: String = text.lines().skip_while(|l| !l.starts_with("-----BEGIN")).skip(1).take_while(|l| !l.starts_with("-----END")).collect();
    let mut der = Vec::with_capacity(b64.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in b64.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue, // padding, white space
        };
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            der.push((acc >> bits) as u8);
        }
    }
    (!der.is_empty()).then(|| crate::web::hex(&Sha256::digest(&der)))
}

/// What goes into the shared directory now.
fn pairing(state: &Path, rdp_port: u16) -> anyhow::Result<String> {
    let token = host_token(state)?;
    let cert = cert_sha256(&state.join("tls/cert.pem")).unwrap_or_default();
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "token": token,
        "cert_sha256": cert,
        "rdp_port": rdp_port,
        "build": option_env!("MACRDP_COMMIT").unwrap_or("dev"),
    }))?)
}

/// One pass: write the file when the host's mark is there and what is written differs.
fn publish(share: &Path, state: &Path, rdp_port: u16) -> anyhow::Result<bool> {
    if !share.join(HOST_MARK).exists() {
        return Ok(false);
    }
    let want = pairing(state, rdp_port)?;
    let file = share.join(GUEST_FILE);
    if std::fs::read_to_string(&file).ok().as_deref() == Some(want.as_str()) {
        return Ok(false);
    }
    let tmp = share.join(format!("{GUEST_FILE}.new"));
    std::fs::write(&tmp, &want)?;
    std::fs::rename(&tmp, &file)?;
    Ok(true)
}

/// In the service, when this Mac is a virtual machine: keep the host told (the share may
/// mount a while after the login; a token revoked here or a new certificate is followed
/// within a quarter of a minute).
pub fn start(state: PathBuf, rdp_port: u16) {
    if !is_guest() {
        return;
    }
    std::thread::spawn(move || {
        let share = PathBuf::from(SHARE);
        let mut said = false;
        loop {
            match publish(&share, &state, rdp_port) {
                Ok(true) => tracing::info!("told the host how to reach this desktop ({SHARE}/{GUEST_FILE})"),
                Ok(false) => {}
                Err(e) if !said => {
                    said = true;
                    tracing::warn!("pairing with the host: {e:#}");
                }
                Err(_) => {}
            }
            std::thread::sleep(std::time::Duration::from_secs(15));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_once_and_follows_a_revoked_token() {
        let dir = std::env::temp_dir().join(format!("hm-guest-test-{}", std::process::id()));
        let (share, state) = (dir.join("share"), dir.join("state"));
        std::fs::create_dir_all(&share).unwrap();
        std::fs::create_dir_all(state.join("tls")).unwrap();
        // "AQID" is the bytes 1, 2, 3
        std::fs::write(state.join("tls/cert.pem"), "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n").unwrap();
        // no mark of a host: nothing is written
        assert!(!publish(&share, &state, 3389).unwrap());
        std::fs::write(share.join(HOST_MARK), "").unwrap();
        assert!(publish(&share, &state, 3389).unwrap());
        assert!(!publish(&share, &state, 3389).unwrap(), "the same again is not written again");
        let read = || serde_json::from_slice::<serde_json::Value>(&std::fs::read(share.join(GUEST_FILE)).unwrap()).unwrap();
        let first = read();
        let token = first["token"].as_str().unwrap().to_owned();
        assert!(crate::web::token_ok(&state.join("tokens.json"), &token));
        assert_eq!(first["cert_sha256"], crate::web::hex(&Sha256::digest([1u8, 2, 3])));
        // revoked in the guest: a new one, the old one no longer good
        crate::web::revoke_token(&state, &crate::web::hex(&Sha256::digest(token.as_bytes()))).unwrap();
        assert!(publish(&share, &state, 3389).unwrap());
        let second = read()["token"].as_str().unwrap().to_owned();
        assert_ne!(second, token);
        assert!(crate::web::token_ok(&state.join("tokens.json"), &second));
        assert!(!crate::web::token_ok(&state.join("tokens.json"), &token));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
