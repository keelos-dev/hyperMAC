//! "Check for Updates…" in the app's window (user 2026-11-09: "和kRDPclient那样子处理升级";
//! kRDPclient src/update.rs is the model): checked only when asked, never by itself.
//!
//! - Which release: the releases of keelos-dev/hyperMAC; the newest by the UTC time in
//!   its tag (`hypermac-<major.minor>-<commit>-<time>-macos-arm64`, tools/package.sh). Once a
//!   full release exists, pre-releases are not offered.
//! - Newer: another commit, packed after this build was (MACRDP_TAG, set by package.sh). A
//!   build without a tag (development) takes any release.
//! - What changed: the release's notes and its changelog.json (package.sh: the newest commits),
//!   of which those between this build's commit and the new one.
//! - Install: the dmg is downloaded (the system's curl, its trust store), checked against the
//!   SHA-256 GitHub gives, mounted, and its app checked: signed by our team (2RVM7QR4WB) as
//!   dev.keelos.hypermac and accepted by Gatekeeper (notarized). Then it replaces the app in
//!   /Applications (copied in beside it, then swapped by renames); the caller restarts the
//!   service and the app. Same identifier and team, so the Privacy grants carry over.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

const RELEASES: &str = "https://api.github.com/repos/keelos-dev/hyperMAC/releases?per_page=30";
const DMG_SUFFIX: &str = "-macos-arm64.dmg";
const CHANGELOG: &str = "changelog.json";
const MAX_CHANGES: usize = 30;
/// What the new app must be signed as (identifier, Developer ID of our team).
const REQUIREMENT: &str =
    r#"=identifier "dev.keelos.hypermac" and anchor apple generic and certificate leaf[subject.OU] = "2RVM7QR4WB""#;

/// This build's release tag (package.sh), None for a development build.
pub fn running_tag() -> Option<&'static str> {
    option_env!("MACRDP_TAG")
}

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub notes: String,
    /// commit subjects between this build and the new one, newest first
    pub changes: Vec<String>,
    /// this build's commit was not found in the changelog: only the newest are listed
    pub partial: bool,
    url: String,
    sha256: String,
    pub size: u64,
}

#[derive(serde::Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GhAsset>,
}

#[derive(serde::Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    size: u64,
}

#[derive(serde::Deserialize)]
struct Changelog {
    commits: Vec<Commit>,
}

#[derive(serde::Deserialize)]
struct Commit {
    hash: String,
    subject: String,
}

/// The commit and the UTC time of a tag (or the running tag).
fn commit_and_time(tag: &str) -> Option<(&str, &str)> {
    let rest = tag.strip_prefix("hypermac-").unwrap_or(tag);
    let mut parts = rest.split('-');
    let _major_minor = parts.next()?;
    let commit = parts.next()?;
    let time = parts.next()?;
    (time.len() == 16 && time.ends_with('Z')).then_some((commit, time))
}

fn is_newer(tag: &str, running: Option<&str>) -> bool {
    let Some((commit, time)) = commit_and_time(tag) else { return false };
    match running.and_then(commit_and_time) {
        Some((own_commit, own_time)) => commit != own_commit && time > own_time,
        None => true,
    }
}

/// `curl` for a small answer (the API, the changelog).
fn fetch(url: &str) -> anyhow::Result<Vec<u8>> {
    let o = Command::new("/usr/bin/curl")
        .args(["-fsSL", "--max-time", "20", "-H", "Accept: application/vnd.github+json", "-A"])
        .arg(format!("hyperMAC/{}", env!("CARGO_PKG_VERSION")))
        .arg(url)
        .output()
        .context("curl")?;
    if !o.status.success() {
        bail!("{}", String::from_utf8_lossy(&o.stderr).trim().trim_start_matches("curl: "));
    }
    Ok(o.stdout)
}

/// The newest release, when it is newer than this build.
pub fn check() -> anyhow::Result<Option<Release>> {
    let all: Vec<GhRelease> =
        serde_json::from_slice(&fetch(RELEASES).context("cannot reach GitHub")?).context("GitHub's answer cannot be read")?;
    let usable: Vec<&GhRelease> = all.iter().filter(|r| !r.draft && commit_and_time(&r.tag_name).is_some()).collect();
    let stable_exists = usable.iter().any(|r| !r.prerelease);
    let Some(newest) = usable
        .into_iter()
        .filter(|r| !stable_exists || !r.prerelease)
        .max_by_key(|r| commit_and_time(&r.tag_name).map(|(_, t)| t.to_string()))
    else {
        return Ok(None);
    };
    if !is_newer(&newest.tag_name, running_tag()) {
        return Ok(None);
    }
    let dmg = newest
        .assets
        .iter()
        .find(|a| a.name.ends_with(DMG_SUFFIX))
        .with_context(|| format!("{} has no dmg", newest.tag_name))?;
    let sha256 = dmg
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .with_context(|| format!("GitHub gives no SHA-256 for {}", dmg.name))?
        .to_ascii_lowercase();
    let (changes, partial) = match newest.assets.iter().find(|a| a.name == CHANGELOG) {
        Some(log) => {
            let commits = fetch(&log.browser_download_url)
                .ok()
                .and_then(|b| serde_json::from_slice::<Changelog>(&b).ok())
                .map(|c| c.commits)
                .unwrap_or_default();
            let to = commit_and_time(&newest.tag_name).map_or("", |(c, _)| c);
            let from = running_tag().and_then(commit_and_time).map(|(c, _)| c);
            changes_between(&commits, to, from)
        }
        None => (Vec::new(), false),
    };
    Ok(Some(Release {
        tag: newest.tag_name.clone(),
        notes: newest.body.clone().unwrap_or_default().trim().to_string(),
        changes,
        partial,
        url: dmg.browser_download_url.clone(),
        sha256,
        size: dmg.size,
    }))
}

/// Subjects from `to` (or the newest) down to `from` (not included); true = cut short / `from` not found.
fn changes_between(commits: &[Commit], to: &str, from: Option<&str>) -> (Vec<String>, bool) {
    let same = |a: &str, b: &str| !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a));
    let start = commits.iter().position(|c| same(&c.hash, to)).unwrap_or(0);
    let mut out = Vec::new();
    for c in &commits[start..] {
        if from.is_some_and(|f| same(&c.hash, f)) {
            return (out, false);
        }
        if out.len() == MAX_CHANGES {
            return (out, true);
        }
        out.push(c.subject.clone());
    }
    (out, true)
}

/// Bytes received so far (the window shows them against Release::size).
#[derive(Default)]
pub struct Progress {
    pub received: AtomicU64,
}

/// Download, check and put the new app in place of `app` (the running one's bundle).
pub fn install(release: &Release, app: &Path, progress: Arc<Progress>) -> anyhow::Result<()> {
    let work = std::env::temp_dir().join("hypermac-update").join(&release.tag);
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("cannot create {}", work.display()))?;
    let dmg = work.join("hyperMAC.dmg");
    download(&release.url, &dmg, &progress)?;
    let got: String = Sha256::digest(std::fs::read(&dmg)?).iter().map(|b| format!("{b:02x}")).collect();
    if got != release.sha256 {
        bail!("the download is damaged (SHA-256 {got}, GitHub says {})", release.sha256);
    }
    let mnt = work.join("mnt");
    std::fs::create_dir_all(&mnt)?;
    run("hdiutil", &["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"], &[&mnt, &dmg])
        .context("cannot open the dmg")?;
    let result = (|| {
        let new = mnt.join("hyperMAC.app");
        run("codesign", &["--verify", "--deep", "--strict", "-R", REQUIREMENT], &[&new])
            .context("the new app is not signed by hyperMAC's developer")?;
        run("spctl", &["-a", "-t", "exec"], &[&new]).context("Gatekeeper does not accept the new app")?;
        swap(&new, app)
    })();
    let _ = run("hdiutil", &["detach", "-quiet", "-force"], &[&mnt]);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn run(cmd: &str, args: &[&str], paths: &[&Path]) -> anyhow::Result<()> {
    let o = Command::new(cmd).args(args).args(paths).output().with_context(|| cmd.to_string())?;
    if !o.status.success() {
        bail!("{cmd}: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

fn download(url: &str, to: &Path, progress: &Progress) -> anyhow::Result<()> {
    let mut child = Command::new("/usr/bin/curl")
        .args(["-fsSL", "--speed-limit", "1", "--speed-time", "30", "-o"])
        .arg(to)
        .arg(url)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("curl")?;
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut err);
                }
                bail!("the download broke off: {}", err.trim().trim_start_matches("curl: "));
            }
            return Ok(());
        }
        let n = std::fs::metadata(to).map(|m| m.len()).unwrap_or(0);
        progress.received.store(n, Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Put `new` (a hyperMAC.app outside /Applications: the one in the dmg, run from there) in place
/// of the installed /Applications/hyperMAC.app, after the same checks as a downloaded update.
/// Finder refuses that replacement while anything of the installed app runs (the menu bar icon,
/// a virtual machine): this does not, the running ones keep the files they have open.
pub fn install_from(new: &Path) -> anyhow::Result<PathBuf> {
    let app = PathBuf::from("/Applications/hyperMAC.app");
    run("codesign", &["--verify", "--deep", "--strict", "-R", REQUIREMENT], &[new])
        .context("this copy is not signed by hyperMAC's developer")?;
    run("spctl", &["-a", "-t", "exec"], &[new]).context("Gatekeeper does not accept this copy")?;
    swap(new, &app)?;
    Ok(app)
}

/// The new app goes in beside the old one, then two renames swap them; the old one is deleted.
fn swap(new: &Path, app: &Path) -> anyhow::Result<()> {
    let dir = app.parent().context("the app's folder")?;
    let staged: PathBuf = dir.join(".hyperMAC-update.app");
    let old: PathBuf = dir.join(format!(".hyperMAC-old-{}.app", std::process::id()));
    let _ = std::fs::remove_dir_all(&staged);
    run("ditto", &[], &[new, &staged]).with_context(|| format!("cannot write to {} (drag the new hyperMAC there yourself)", dir.display()))?;
    std::fs::rename(app, &old).with_context(|| format!("cannot move {}", app.display()))?;
    if let Err(e) = std::fs::rename(&staged, app) {
        let _ = std::fs::rename(&old, app); // put the old one back
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e).context("cannot put the new app in place");
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against the real releases: `cargo test -- --ignored update_check_live --nocapture`.
    #[test]
    #[ignore]
    fn update_check_live() {
        let r = check().unwrap().expect("a development build takes any release");
        println!("{} {} bytes sha256 {} changes {:?} partial {}", r.tag, r.size, r.sha256, r.changes.len(), r.partial);
        assert!(r.url.ends_with(DMG_SUFFIX));
        assert_eq!(r.sha256.len(), 64);
    }

    /// The whole install against the real newest release, into MACRDP_TEST_APP (an app in a
    /// scratch folder, never /Applications): `MACRDP_TEST_APP=/tmp/x/hyperMAC.app cargo test --
    /// --ignored update_install_live`.
    #[test]
    #[ignore]
    fn update_install_live() {
        let app = PathBuf::from(std::env::var("MACRDP_TEST_APP").expect("MACRDP_TEST_APP"));
        assert!(!app.starts_with("/Applications"));
        let r = check().unwrap().unwrap();
        install(&r, &app, Arc::new(Progress::default())).unwrap();
        run("codesign", &["--verify", "--deep", "--strict", "-R", REQUIREMENT], &[&app]).unwrap();
        let dir = app.parent().unwrap();
        let left: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "only the app is left: {left:?}");
    }

    #[test]
    fn newer_by_commit_and_time() {
        let a = "hypermac-0.1-c143f00-20261109T063909Z-macos-arm64";
        let b = "hypermac-0.1-8dd0ff0-20261109T064810Z-macos-arm64";
        assert!(is_newer(b, Some(a)));
        assert!(!is_newer(a, Some(b)));
        assert!(!is_newer(a, Some(a)));
        assert!(is_newer(a, None));
        assert!(!is_newer("something-else", None));
    }

    #[test]
    fn changes_down_to_the_running_commit() {
        let c = |h: &str, s: &str| Commit { hash: h.into(), subject: s.into() };
        let log = vec![c("aaaaaaa1", "three"), c("bbbbbbb2", "two"), c("ccccccc3", "one")];
        assert_eq!(changes_between(&log, "aaaaaaa", Some("ccccccc")), (vec!["three".into(), "two".into()], false));
        assert_eq!(changes_between(&log, "bbbbbbb", Some("zzz")).1, true);
    }
}
