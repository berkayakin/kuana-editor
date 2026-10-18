//! Updates from GitHub Releases. The release workflow attaches `latest.json`
//! (the version and, per platform, an archive's name and SHA-256) and signs it
//! with minisign; Kuana only trusts a manifest whose signature matches
//! `PUBLIC_KEY`, so a tampered download or a hijacked release can't install
//! anything. `releases/latest/download/…` always resolves to the newest
//! published release (drafts and pre-releases are skipped), so checking needs
//! no API calls. Downloads go through `curl`, which macOS, Windows 10+ and
//! Linux distributions ship.
//!
//! An update downloads in the background and waits for Restart to Update in
//! the status bar: that swaps the installed app for the new one, quits, and
//! starts the new version once this process is gone (hot exit brings the
//! project back). Builds from source (`cargo run`) never update themselves.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use gpui::{App, BorrowAppContext, Global, Task};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::settings::Settings;

const REPO: &str = "berkayakin/kuana-editor";
/// Release signing key (`~/.minisign/kin-release.pub` on the maintainer's
/// machine); the secret half is the repository secret `MINISIGN_SECRET_KEY`.
const PUBLIC_KEY: &str = "RWR5QO9LE08wWLXENZBBui1DNd0r9UFw0h8OV/EB2bWiFgHtIlRP0K70";
const FIRST_CHECK: Duration = Duration::from_secs(10);
const CHECK_EVERY: Duration = Duration::from_secs(4 * 60 * 60);
/// Where a downloaded archive is unpacked, next to the installed app so the
/// final swap is a rename on the same disk.
const STAGING: &str = ".kuana-update";

#[derive(Default)]
pub struct Updater {
    state: State,
    /// Start the new version once this process exits.
    relaunch: bool,
}

impl Global for Updater {}

#[derive(Clone, Default, PartialEq, Debug)]
enum State {
    #[default]
    Idle,
    Checking,
    /// Downloaded and verified, waiting for a restart; checked again against
    /// `sha256` right before it's unpacked.
    Ready { version: String, archive: PathBuf, sha256: String },
    /// Swapped in; it runs from the next start.
    Installed { version: String },
}

pub enum Check {
    UpToDate,
    /// This version is downloaded and installs on restart.
    Ready(String),
}

/// Whether this build updates itself: only builds from the release workflow
/// (which sets `KUANA_RELEASE`) on a platform it publishes.
pub fn supported() -> bool {
    option_env!("KUANA_RELEASE").is_some() && platform().is_some()
}

/// The `latest.json` asset key for this build.
fn platform() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("macos")
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        Some("windows-x86_64")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x86_64")
    } else {
        None
    }
}

/// Sets up the updater and, unless `auto_update` is off, checks shortly after
/// launch and every few hours.
pub fn init(cx: &mut App) {
    cx.set_global(Updater::default());
    if !supported() {
        return;
    }
    cx.background_executor().spawn(async { remove_leftovers() }).detach();
    cx.on_app_quit(|cx| {
        if cx.global::<Updater>().relaunch
            && let Err(err) = relaunch()
        {
            eprintln!("kuana: couldn't restart after the update: {err}");
        }
        async {}
    })
    .detach();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FIRST_CHECK).await;
        loop {
            let enabled = cx.update(|cx| Settings::get(cx).auto_update.unwrap_or(true)).unwrap_or(false);
            if enabled && let Ok(task) = cx.update(check) {
                // Background checks fail quietly; Check for Updates reports errors.
                task.await.ok();
            }
            cx.background_executor().timer(CHECK_EVERY).await;
        }
    })
    .detach();
}

/// The version waiting for a restart, if any.
pub fn pending_version(cx: &App) -> Option<&str> {
    match &cx.try_global::<Updater>()?.state {
        State::Ready { version, .. } | State::Installed { version } => Some(version),
        State::Idle | State::Checking => None,
    }
}

/// Looks for a newer release and downloads it. The task has to be awaited
/// (dropping it cancels the check).
pub fn check(cx: &mut App) -> Task<Result<Check, String>> {
    if !supported() {
        return Task::ready(Err("This build of Kuana was built from source, so it doesn't update itself.".into()));
    }
    match &cx.global::<Updater>().state {
        State::Checking => return Task::ready(Err("Kuana is already checking for updates.".into())),
        State::Installed { version } => return Task::ready(Ok(Check::Ready(version.clone()))),
        State::Idle | State::Ready { .. } => {}
    }
    let previous = cx.global::<Updater>().state.clone();
    cx.update_global(|u: &mut Updater, _| u.state = State::Checking);
    cx.spawn(async move |cx| {
        let result = cx.background_executor().spawn(async move { download_latest() }).await;
        let state = match &result {
            Ok(Some(update)) => State::Ready { version: update.version.clone(), archive: update.archive.clone(), sha256: update.sha256.clone() },
            // A failed check keeps an update that's already downloaded.
            Ok(None) | Err(_) => previous,
        };
        cx.update_global(|u: &mut Updater, _| u.state = state).ok();
        result.map(|update| match update {
            Some(update) => Check::Ready(update.version),
            None => Check::UpToDate,
        })
    })
}

/// Installs the downloaded update and quits; the new version starts once
/// Kuana has exited. If quitting is cancelled, it starts next time instead.
pub fn restart(cx: &mut App) -> Result<(), String> {
    let state = cx.global::<Updater>().state.clone();
    match state {
        State::Ready { version, archive, sha256 } => {
            let installed = install(&archive, &sha256);
            if installed.is_err() {
                // The next check downloads it again if it changed.
                cx.update_global(|u: &mut Updater, _| u.state = State::Idle);
            }
            installed?;
            std::fs::remove_file(&archive).ok();
            cx.update_global(|u: &mut Updater, _| u.state = State::Installed { version });
        }
        State::Installed { .. } => {}
        State::Idle | State::Checking => return Err("There's no update to install yet.".into()),
    }
    cx.update_global(|u: &mut Updater, _| u.relaunch = true);
    cx.defer(crate::workspace::quit);
    Ok(())
}

// ---- Checking and downloading ---------------------------------------------------

#[derive(Deserialize, Debug)]
struct Manifest {
    version: String,
    /// By platform key (`platform()`).
    assets: HashMap<String, Asset>,
}

#[derive(Deserialize, Debug)]
struct Asset {
    name: String,
    sha256: String,
}

/// A downloaded update.
struct Downloaded {
    version: String,
    archive: PathBuf,
    sha256: String,
}

/// Where updates download to: in the config folder, which only this user
/// can write to (the shared temporary folder is anyone's, so another user
/// could swap the archive between its check and its unpacking).
fn downloads_dir() -> Result<PathBuf, String> {
    crate::theme::config_dir().map(|d| d.join("updates")).ok_or_else(|| "Kuana has no config folder.".into())
}

/// Downloads the newest release's archive for this platform if it's newer
/// than this build. Blocking.
fn download_latest() -> Result<Option<Downloaded>, String> {
    let base = format!("https://github.com/{REPO}/releases/latest/download");
    let manifest = fetch(&format!("{base}/latest.json"))?;
    let signature = fetch(&format!("{base}/latest.json.minisig"))?;
    let manifest = verified_manifest(&manifest, &signature, PUBLIC_KEY)?;
    if !is_newer(&manifest.version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    // A release whose build failed for this platform simply isn't offered.
    let Some(asset) = platform().and_then(|p| manifest.assets.get(p)) else {
        return Ok(None);
    };
    let dir = downloads_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let archive = dir.join(&asset.name);
    // Only one update is kept; an earlier download isn't needed any more.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten().filter(|e| e.path() != archive) {
            std::fs::remove_file(entry.path()).ok();
        }
    }
    if sha256_hex(&archive).as_deref() != Some(asset.sha256.as_str()) {
        let url = format!("https://github.com/{REPO}/releases/download/v{}/{}", manifest.version, asset.name);
        let partial = dir.join(format!("{}.part", asset.name));
        download(&url, &partial, &format!("Kuana {}", manifest.version), None)?;
        if sha256_hex(&partial).as_deref() != Some(asset.sha256.as_str()) {
            std::fs::remove_file(&partial).ok();
            return Err(format!("{} doesn't match the release's checksum.", asset.name));
        }
        std::fs::rename(&partial, &archive).map_err(|e| e.to_string())?;
    }
    Ok(Some(Downloaded { version: manifest.version, archive, sha256: asset.sha256.clone() }))
}

fn verified_manifest(data: &[u8], signature: &[u8], public_key: &str) -> Result<Manifest, String> {
    let key = minisign_verify::PublicKey::from_base64(public_key).map_err(|e| e.to_string())?;
    let signature = std::str::from_utf8(signature)
        .ok()
        .and_then(|s| minisign_verify::Signature::decode(s).ok())
        .ok_or("The update's signature is unreadable.")?;
    key.verify(data, &signature, false)
        .map_err(|_| "The update isn't signed by Kuana's release key.".to_string())?;
    let manifest: Manifest = serde_json::from_slice(data).map_err(|e| format!("latest.json: {e}"))?;
    // Names become file paths and URLs.
    if manifest.assets.values().any(|a| a.name.is_empty() || a.name.starts_with('.') || a.name.contains(['/', '\\'])) {
        return Err("latest.json names an invalid file.".into());
    }
    Ok(manifest)
}

/// Whether `candidate` is a later version than `current` ("1.2.3", with an
/// optional "-pre" suffix that sorts before the release).
pub(crate) fn is_newer(candidate: &str, current: &str) -> bool {
    fn parse(version: &str) -> Option<(Vec<u64>, bool)> {
        let version = version.trim().trim_start_matches('v');
        let (numbers, pre) = match version.split_once('-') {
            Some((numbers, _)) => (numbers, true),
            None => (version, false),
        };
        let numbers = numbers.split('.').map(|n| n.parse().ok()).collect::<Option<Vec<u64>>>()?;
        Some((numbers, !pre))
    }
    matches!((parse(candidate), parse(current)), (Some(a), Some(b)) if a > b)
}

pub(crate) fn sha256_hex(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).ok()?;
    Some(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn curl() -> Command {
    let mut cmd = command("curl");
    // Follow GitHub's redirect to its download host, but only over HTTPS.
    cmd.args(["--fail", "--silent", "--show-error", "--location", "--proto", "=https", "--connect-timeout", "20"]);
    cmd
}

pub(crate) fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let output = curl().args(["--max-time", "60", url]).output().map_err(|e| format!("couldn't run curl: {e}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(curl_error(&output.stderr))
    }
}

/// Downloads `url` to `to`, shown as `label` in the status bar while it
/// runs (from curl's progress bar on stderr, which also carries errors).
pub(crate) fn download(url: &str, to: &Path, label: &str, size: Option<u64>) -> Result<(), String> {
    let mut child = command("curl")
        .args(["--fail", "--show-error", "--location", "--proto", "=https", "--connect-timeout", "20"])
        .args(["--progress-bar", "--max-time", "900", "--output"])
        .arg(to)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run curl: {e}"))?;
    let tracker = crate::downloads::start(label, to, size);
    let mut errors = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let (mut buffer, mut pending) = ([0u8; 1024], String::new());
        while let Ok(n) = std::io::Read::read(&mut stderr, &mut buffer) {
            if n == 0 {
                break;
            }
            pending.push_str(&String::from_utf8_lossy(&buffer[..n]));
            if let Some(fraction) = crate::downloads::parse_curl_progress(&pending) {
                tracker.set_fraction(fraction);
            }
            // Keeps the unfinished segment; error lines are kept whole.
            if let Some(end) = pending.rfind(['\r', '\n']) {
                errors.extend(pending[..end].split(['\r', '\n']).filter(|l| l.starts_with("curl:")).map(|l| format!("{l}\n")));
                pending.drain(..=end);
            }
        }
        errors.push_str(&pending);
    }
    let status = child.wait().map_err(|e| format!("curl failed: {e}"))?;
    if status.success() { Ok(()) } else { Err(curl_error(errors.as_bytes())) }
}

fn curl_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim().trim_start_matches("curl: ");
    if text.contains("404") {
        "No published release was found.".into()
    } else {
        format!("Download failed: {text}")
    }
}

/// A command without a console window (Kuana is a GUI app on Windows).
fn command(program: &str) -> Command {
    let mut cmd = crate::process::command(program);
    cmd.stdin(Stdio::null());
    cmd
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let output = cmd.output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

// ---- Installing ----------------------------------------------------------------

/// What gets replaced: `Kuana.app` on macOS, the executable elsewhere.
fn installed_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = crate::paths::canonicalize(&exe).unwrap_or(exe);
    if cfg!(target_os = "macos") {
        let app = exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .ok_or("Kuana isn't running from Kuana.app.")?;
        // A quarantined app opened from Downloads runs from a read-only copy.
        if app.to_string_lossy().contains("/AppTranslocation/") {
            return Err("Move Kuana to the Applications folder first, then restart it.".into());
        }
        Ok(app.to_path_buf())
    } else {
        Ok(exe)
    }
}

/// Unpacks `archive` next to the installed app and swaps it in. The running
/// process keeps its files open, so it carries on until it quits.
fn install(archive: &Path, sha256: &str) -> Result<(), String> {
    install_to(archive, sha256, &installed_path()?)
}

fn install_to(archive: &Path, sha256: &str, target: &Path) -> Result<(), String> {
    // It waited for a restart since it was checked.
    if sha256_hex(archive).as_deref() != Some(sha256) {
        return Err("The downloaded update changed since it was checked. Check for updates again.".into());
    }
    let dir = target.parent().ok_or("Kuana's folder wasn't found.")?;
    let staging = dir.join(STAGING);
    let cant = |e: std::io::Error| format!("Couldn't update {}: {e}", target.display());
    std::fs::remove_dir_all(&staging).ok();
    std::fs::create_dir_all(&staging).map_err(cant)?;
    let result = (|| {
        if cfg!(target_os = "macos") {
            run(command("ditto").args(["-x", "-k"]).arg(archive).arg(&staging))?;
        } else {
            // Windows 10+ ships bsdtar, which also reads zip files.
            run(command("tar").arg("-xf").arg(archive).arg("-C").arg(&staging))?;
        }
        // What the release workflow packs (the installed copy may be renamed).
        let name = if cfg!(target_os = "macos") {
            "Kuana.app"
        } else if cfg!(windows) {
            "kuana.exe"
        } else {
            "kuana"
        };
        let new = staging.join(name);
        if !new.exists() {
            return Err(format!("The update doesn't contain {name}."));
        }
        swap(target, &new).map_err(cant)
    })();
    std::fs::remove_dir_all(&staging).ok();
    result
}

#[cfg(unix)]
fn swap(target: &Path, new: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        // An .app bundle can't be renamed over, so move the old one aside first.
        let old = target.with_file_name(format!(".{}-old", file_name(target)));
        std::fs::remove_dir_all(&old).ok();
        std::fs::rename(target, &old)?;
        if let Err(err) = std::fs::rename(new, target) {
            std::fs::rename(&old, target).ok();
            return Err(err);
        }
        std::fs::remove_dir_all(&old).ok();
        Ok(())
    } else {
        // Replaces the running binary atomically; this process keeps the old inode.
        std::fs::rename(new, target)
    }
}

/// Windows can't overwrite a running executable but can rename it; the old
/// one is deleted on the next start (`remove_leftovers`).
#[cfg(windows)]
fn swap(target: &Path, new: &Path) -> std::io::Result<()> {
    let old = old_exe(target);
    std::fs::remove_file(&old).ok();
    std::fs::rename(target, &old)?;
    if let Err(err) = std::fs::rename(new, target) {
        std::fs::rename(&old, target).ok();
        return Err(err);
    }
    Ok(())
}

#[cfg(windows)]
fn old_exe(target: &Path) -> PathBuf {
    target.with_file_name(format!("{}.old", file_name(target)))
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Clears what an interrupted or finished update left behind.
fn remove_leftovers() {
    let Ok(target) = installed_path() else { return };
    let Some(dir) = target.parent() else { return };
    std::fs::remove_dir_all(dir.join(STAGING)).ok();
    #[cfg(unix)]
    std::fs::remove_dir_all(target.with_file_name(format!(".{}-old", file_name(&target)))).ok();
    #[cfg(windows)]
    std::fs::remove_file(old_exe(&target)).ok();
}

/// Starts the installed Kuana once this process has exited, so the new one
/// restores the session this one saved.
fn relaunch() -> Result<(), String> {
    let target = installed_path()?;
    let pid = std::process::id().to_string();
    #[cfg(unix)]
    let mut cmd = {
        let start = if cfg!(target_os = "macos") { r#"exec open "$1""# } else { r#"exec "$1""# };
        let mut cmd = command("/bin/sh");
        cmd.arg("-c")
            .arg(format!(r#"while kill -0 "$0" 2>/dev/null; do sleep 0.2; done; {start}"#))
            .arg(&pid)
            .arg(&target);
        cmd
    };
    #[cfg(windows)]
    let mut cmd = {
        let path = target.to_string_lossy().replace('\'', "''");
        let mut cmd = command("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command"]).arg(format!(
            "Wait-Process -Id {pid} -ErrorAction SilentlyContinue; Start-Process -FilePath '{path}'"
        ));
        cmd
    };
    cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(drop).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.10.0", "0.9.3"));
        assert!(is_newer("1.0.0", "1.0.0-beta.2"));
        assert!(!is_newer("1.0.0-beta.2", "1.0.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    // Signed with a throwaway key: `minisign -G -W`, then `minisign -S -m latest.json`.
    // Signed when Kuana was called Kin, so the signed text keeps that name.
    const TEST_KEY: &str = "RWQSuCW4OWCdK1g9tgG25m9bouG+gU4rO3ykgVBmOafxtkB6VTclBJ5+";
    const TEST_MANIFEST: &str = r#"{"version":"0.2.0","assets":{"macos":{"name":"Kin-0.2.0-macos.zip","sha256":"00"}}}"#;
    const TEST_SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RUQSuCW4OWCdKw4dnruOJeFrs8gGgDT3dmSvK78mkY+4BMCu+AioCT7bpMyU4oveUI+mwTpozVDFrslVnZwXqV5bQ/qVd2XNsg0=
trusted comment: Kin 0.2.0
AxrB0JdAasRgQFsgqpgRL89NottkeVOjXZ4tCR71ugQX75l253CoRpkYNv4ZOlrhGWODeFXKIfyqi75TZ1m/BA==
";

    #[test]
    fn checks_the_manifest_signature() {
        let manifest = verified_manifest(TEST_MANIFEST.as_bytes(), TEST_SIGNATURE.as_bytes(), TEST_KEY).unwrap();
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.assets["macos"].name, "Kin-0.2.0-macos.zip");
        let tampered = TEST_MANIFEST.replace("0.2.0", "9.9.9");
        assert!(verified_manifest(tampered.as_bytes(), TEST_SIGNATURE.as_bytes(), TEST_KEY).is_err());
        // The real key didn't sign this.
        assert!(verified_manifest(TEST_MANIFEST.as_bytes(), TEST_SIGNATURE.as_bytes(), PUBLIC_KEY).is_err());
    }

    #[test]
    fn installs_only_the_archive_that_was_checked() {
        let dir = std::env::temp_dir().join(format!("kuana-update-check-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let (archive, target) = (dir.join("Kuana.zip"), dir.join("kuana"));
        std::fs::write(&archive, "swapped after the check").unwrap();
        std::fs::write(&target, "old").unwrap();
        let checked = "0".repeat(64);
        assert!(install_to(&archive, &checked, &target).unwrap_err().contains("changed"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(!dir.join(STAGING).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn swaps_in_the_new_app() {
        let dir = std::env::temp_dir().join(format!("kuana-update-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let bundle = |root: &Path, text: &str| {
            let exe = root.join("Kuana.app/Contents/MacOS");
            std::fs::create_dir_all(&exe).unwrap();
            std::fs::write(exe.join("kuana"), text).unwrap();
        };
        bundle(&dir.join("Applications"), "old");
        bundle(&dir.join("build"), "new");
        let archive = dir.join("Kuana.zip");
        run(command("ditto").args(["-c", "-k", "--keepParent"]).arg(dir.join("build/Kuana.app")).arg(&archive)).unwrap();

        // Installed under another name, as users sometimes do.
        let target = dir.join("Applications/Kuana Editor.app");
        std::fs::rename(dir.join("Applications/Kuana.app"), &target).unwrap();
        assert!(install_to(&archive, "00", &target).is_err(), "not the archive that was checked");
        assert_eq!(std::fs::read_to_string(target.join("Contents/MacOS/kuana")).unwrap(), "old");
        install_to(&archive, &sha256_hex(&archive).unwrap(), &target).unwrap();
        assert_eq!(std::fs::read_to_string(target.join("Contents/MacOS/kuana")).unwrap(), "new");
        let left: Vec<_> = std::fs::read_dir(dir.join("Applications")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("Kuana Editor.app")]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
