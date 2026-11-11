//! The app's switches for the service (gui.rs): <state>/settings.json, e.g.
//! {"video": true, "sound": true, "mute_mac": true, "clip": true}. Read at each new connection, so a change
//! applies from the next connection on, no restart. A MACRDP_VIDEO / _SOUND / _CLIP
//! environment variable (development) still wins over the file. Missing = on.
//! "paused" (the menu bar's Pause Remote; missing = off) is read at each new connection too:
//! while it is on, the service turns new connections away.

use std::path::Path;

fn read(state: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read(state.join("settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Is this feature on? `env` (e.g. "MACRDP_VIDEO") = "0" turns it off whatever the file says.
pub fn on(state: &Path, key: &str, env: &str) -> bool {
    if let Ok(v) = std::env::var(env) {
        return v != "0";
    }
    read(state).get(key).and_then(|v| v.as_bool()).unwrap_or(true)
}

/// "remote" (missing = on): is hyperMAC reachable from other computers at all? Off = the service
/// opens no port (RDP, the kRDPclient API, discovery): the virtual machines are used on this Mac
/// alone, in their Console windows. Read when the service starts.
pub fn remote(state: &Path) -> bool {
    get(state, "remote", true)
}

/// The ports the service listens on, read when it starts: "rdp_port" (3389) and "web_port"
/// (443, the API kRDPclient adds a host by; another program may have it).
pub fn ports(state: &Path) -> (u16, u16) {
    let m = read(state);
    let port = |key: &str, default: u16| {
        m.get(key).and_then(|v| v.as_u64()).and_then(|n| u16::try_from(n).ok()).filter(|n| *n > 0).unwrap_or(default)
    };
    (port("rdp_port", 3389), port("web_port", 443))
}

pub fn set_ports(state: &Path, rdp: u16, web: u16) -> anyhow::Result<()> {
    set_value(state, "rdp_port", rdp.into())?;
    set_value(state, "web_port", web.into())
}

/// "host_desktop" (missing = on): may clients connect to this Mac's own desktop? Off = hyperMAC
/// only serves its virtual machines: a login that is not a VM's is refused, /v1/vms lists the
/// VMs alone, and neither Screen Recording nor Accessibility is needed.
pub fn host_desktop(state: &Path) -> bool {
    on(state, "host_desktop", "MACRDP_HOST_DESKTOP")
}

/// A switch whose default is `default` (e.g. "paused", off unless set).
pub fn get(state: &Path, key: &str, default: bool) -> bool {
    read(state).get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

pub fn set(state: &Path, key: &str, value: bool) -> anyhow::Result<()> {
    set_value(state, key, value.into())
}

/// A text setting ("vm_dir": where the VMs are kept, vms.rs `storage`).
pub fn set_str(state: &Path, key: &str, value: &str) -> anyhow::Result<()> {
    set_value(state, key, value.into())
}

fn set_value(state: &Path, key: &str, value: serde_json::Value) -> anyhow::Result<()> {
    let mut m = read(state);
    m.insert(key.into(), value);
    std::fs::create_dir_all(state)?;
    let tmp = state.join("settings.json.new");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&m)?)?;
    std::fs::rename(&tmp, state.join("settings.json"))?;
    Ok(())
}
