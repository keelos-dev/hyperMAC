//! Clones and snapshots of a stopped VM.
//!
//!   hypermac clone SRC NEW               a second machine from SRC's disk, with its own identity
//!   hypermac snapshot NAME [LABEL...]    keep the machine as it is now
//!   hypermac snapshots NAME              list them
//!   hypermac restore NAME ID             the machine goes back to that snapshot
//!   hypermac snapshot-rm NAME ID
//!
//! Both are copies of the files that make the machine (`STATE`), made with clonefile(2): on
//! APFS a copy shares its blocks with the original until one of them writes, so a 64 GB disk is
//! copied at once and takes no room. Elsewhere (no clonefile) the files are copied in full.
//! A snapshot is a folder, <VM>/snapshots/ID/, with those files and meta.json; deleting the VM
//! deletes its snapshots.
//!
//! Only a stopped machine: the disk of a running one is in the middle of things. A machine
//! suspended to a file (Virtualization.framework) is stopped in this sense, and its snapshot
//! keeps the file: restored, it goes on where it was.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use objc2_virtualization::{VZGenericMachineIdentifier, VZMACAddress, VZMacMachineIdentifier};

use crate::{load, vm_dir};

/// The files that make a machine: its disk, its firmware's memory, what it is, its settings.
/// (Not: sockets, logs, the picture, the pairing with a guest's hyperMAC, the install's leftovers.)
const STATE: &[&str] = &["vm.json", "disk.img", "aux.img", "nvram", "vars.fd", "hardware-model", "machine-id", crate::vm::SAVED];
/// ... and the folders: the TPM's state (a Windows guest's BitLocker keys are sealed to it)
const STATE_DIRS: &[&str] = &["tpm"];

const SNAPSHOTS: &str = "snapshots";

fn running(dir: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(dir.join("ctl.sock")).is_ok()
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 40 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `from` -> `to` (which is not there): a clone where the file system has them, else a copy.
fn copy_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let (f, t) = (std::ffi::CString::new(from.as_os_str().as_bytes())?, std::ffi::CString::new(to.as_os_str().as_bytes())?);
    if unsafe { libc::clonefile(f.as_ptr(), t.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    // not APFS, or two volumes: the whole file, then
    ensure!(matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::EXDEV)), "{} -> {}: {e}", from.display(), to.display());
    eprintln!("{}: no clones on this volume, copying the whole file", from.display());
    std::fs::copy(from, to).with_context(|| format!("{} -> {}", from.display(), to.display()))?;
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let (p, t) = (e.path(), to.join(e.file_name()));
        let ty = e.file_type()?;
        if ty.is_dir() {
            copy_dir(&p, &t)?;
        } else if ty.is_file() {
            copy_file(&p, &t)?;
        }
        // (sockets and the like are a running machine's: not state)
    }
    Ok(())
}

/// The machine's state, `from` -> `to` (a new folder).
fn copy_state(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for f in STATE {
        if from.join(f).is_file() {
            copy_file(&from.join(f), &to.join(f))?;
        }
    }
    for d in STATE_DIRS {
        if from.join(d).is_dir() {
            copy_dir(&from.join(d), &to.join(d))?;
        }
    }
    Ok(())
}

fn stopped(name: &str) -> anyhow::Result<PathBuf> {
    let dir = vm_dir(name);
    load(name)?;
    ensure!(!running(&dir), "{name} is running: shut it down first");
    ensure!(!dir.join("installing").exists(), "{name} is still being installed");
    Ok(dir)
}

/// hypermac clone SRC NEW
pub fn clone(src: &str, new: &str) -> anyhow::Result<()> {
    ensure!(valid_name(new), "{new:?}: a name takes letters, digits, - and _");
    let from = stopped(src)?;
    let to = vm_dir(new);
    ensure!(!to.exists(), "VM {new:?} exists ({})", to.display());
    let made = (|| {
        copy_state(&from, &to)?;
        // The clone is a machine of its own: its own network address (two machines with one
        // MAC address get one DHCP lease and take each other's packets), its own console
        // password, its own machine identifier. What is on the disk stays the same: the
        // system's name and its accounts are the user's to change in the clone.
        let mut conf = load(new)?;
        conf.mac = unsafe { VZMACAddress::randomLocallyAdministeredAddress().string() }.to_string();
        let mut pw = [0u8; 4];
        std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut pw))?;
        conf.vnc_password = pw.iter().map(|b| format!("{b:02x}")).collect();
        conf.autostart = false;
        // an installer's CD that was made for the source (Windows' setup.iso) stays there
        if conf.drivers.as_deref().is_some_and(|d| Path::new(d).starts_with(&from)) {
            conf.drivers = None;
        }
        std::fs::write(to.join("vm.json"), serde_json::to_vec_pretty(&conf)?)?;
        if to.join("machine-id").exists() {
            let id = unsafe {
                if conf.os == "macos" { VZMacMachineIdentifier::new().dataRepresentation() } else { VZGenericMachineIdentifier::new().dataRepresentation() }
            };
            std::fs::write(to.join("machine-id"), id.to_vec())?;
        }
        // a suspended source: its memory belongs to the old identity; the clone starts from
        // the disk, as after a power cut at that moment
        if std::fs::remove_file(to.join(crate::vm::SAVED)).is_ok() {
            eprintln!("{src} is suspended: {new} starts from its disk, not from the suspended state");
        }
        Ok(())
    })();
    if made.is_err() {
        let _ = std::fs::remove_dir_all(&to);
    }
    made.map(|()| eprintln!("cloned. Next: hypermac run {new}"))
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Meta {
    pub label: String,
    /// seconds since 1970
    pub taken: u64,
}

fn snap_dir(dir: &Path, id: &str) -> anyhow::Result<PathBuf> {
    ensure!(!id.is_empty() && id.chars().all(|c| c.is_ascii_digit() || c == '-'), "{id:?}: not a snapshot's id");
    Ok(dir.join(SNAPSHOTS).join(id))
}

/// The snapshots of the VM in `dir`, oldest first: (id, meta).
pub fn list(dir: &Path) -> Vec<(String, Meta)> {
    let mut v: Vec<(String, Meta)> = std::fs::read_dir(dir.join(SNAPSHOTS))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let meta = serde_json::from_slice(&std::fs::read(e.path().join("meta.json")).ok()?).ok()?;
            Some((e.file_name().to_string_lossy().into_owned(), meta))
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// "20261112-153012": local time, so the folder's name says when.
fn new_id(now: u64) -> String {
    let t = now as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// hypermac snapshot NAME [LABEL...]
pub fn take(name: &str, label: &str) -> anyhow::Result<()> {
    let dir = stopped(name)?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let mut id = new_id(now);
    // two in one second
    for n in 2.. {
        if !dir.join(SNAPSHOTS).join(&id).exists() {
            break;
        }
        id = format!("{}-{n}", new_id(now));
    }
    let to = dir.join(SNAPSHOTS).join(&id);
    // made under another name: a snapshot that is listed is whole
    let tmp = dir.join(SNAPSHOTS).join(format!(".{id}.part"));
    let _ = std::fs::remove_dir_all(&tmp);
    let made = (|| {
        copy_state(&dir, &tmp)?;
        std::fs::write(tmp.join("meta.json"), serde_json::to_vec_pretty(&Meta { label: label.trim().to_string(), taken: now })?)?;
        std::fs::rename(&tmp, &to)?;
        Ok(())
    })();
    if made.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    made.map(|()| println!("{id}"))
}

/// hypermac restore NAME ID: the machine as the snapshot has it. What it is now is gone
/// (take a snapshot first to keep it).
pub fn restore(name: &str, id: &str) -> anyhow::Result<()> {
    let dir = stopped(name)?;
    let from = snap_dir(&dir, id)?;
    ensure!(from.join("meta.json").exists() && from.join("disk.img").exists(), "{name} has no snapshot {id}");
    // every file under another name first: when one cannot be copied (the volume is full),
    // the machine is still what it was
    let part = |f: &str| dir.join(format!(".{f}.restore"));
    let clean = || {
        for f in STATE.iter().chain(STATE_DIRS) {
            let p = part(f);
            let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
        }
    };
    clean();
    let staged = (|| {
        for f in STATE {
            if from.join(f).is_file() {
                copy_file(&from.join(f), &part(f))?;
            }
        }
        for d in STATE_DIRS {
            if from.join(d).is_dir() {
                copy_dir(&from.join(d), &part(d))?;
            }
        }
        anyhow::Ok(())
    })();
    if let Err(e) = staged {
        clean();
        return Err(e);
    }
    for f in STATE {
        // a file the snapshot does not have (the suspended state of later) goes
        match part(f).exists() {
            true => std::fs::rename(part(f), dir.join(f))?,
            false => {
                let _ = std::fs::remove_file(dir.join(f));
            }
        }
    }
    for d in STATE_DIRS {
        let _ = std::fs::remove_dir_all(dir.join(d));
        if part(d).exists() {
            std::fs::rename(part(d), dir.join(d))?;
        }
    }
    eprintln!("{name} is as it was at {id}");
    Ok(())
}

/// hypermac snapshot-rm NAME ID
pub fn remove(name: &str, id: &str) -> anyhow::Result<()> {
    let dir = vm_dir(name);
    load(name)?;
    let d = snap_dir(&dir, id)?;
    if !d.exists() {
        bail!("{name} has no snapshot {id}");
    }
    std::fs::remove_dir_all(&d)?;
    Ok(())
}

/// hypermac snapshots NAME
pub fn print(name: &str) -> anyhow::Result<()> {
    load(name)?;
    for (id, m) in list(&vm_dir(name)) {
        println!("{id}\t{}", m.label);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hm-snap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn copy_state_takes_the_machine_and_nothing_else() {
        let d = scratch("state");
        let (a, b) = (d.join("a"), d.join("b"));
        std::fs::create_dir_all(a.join("tpm")).unwrap();
        std::fs::create_dir_all(a.join("share")).unwrap();
        for f in ["vm.json", "disk.img", "vars.fd", "run.log", "tpm/state", "share/hypermac-guest.json"] {
            std::fs::write(a.join(f), f).unwrap();
        }
        copy_state(&a, &b).unwrap();
        for f in ["vm.json", "disk.img", "vars.fd", "tpm/state"] {
            assert_eq!(std::fs::read_to_string(b.join(f)).unwrap(), f);
        }
        assert!(!b.join("run.log").exists() && !b.join("share").exists());
        // a copy is its own file: writing to it leaves the original
        std::fs::write(b.join("disk.img"), "changed").unwrap();
        assert_eq!(std::fs::read_to_string(a.join("disk.img")).unwrap(), "disk.img");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ids_sort_by_time_and_are_checked() {
        assert!(new_id(1_800_000_000) < new_id(1_800_000_001));
        assert_eq!(new_id(0).len(), 15);
        let d = Path::new("/x");
        assert!(snap_dir(d, "20261112-153012").is_ok());
        assert!(snap_dir(d, "../other").is_err() && snap_dir(d, "").is_err());
    }
}
