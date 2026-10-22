//! Installing language servers with one click.
//! A missing server's status bar item offers **Install**, which puts it in
//! Kuana's own folder (`<config>/servers/`): a prebuilt binary from its GitHub
//! release (checked against GitHub's SHA-256), an npm package (with the
//! user's Node.js, or one Kuana downloads once and checks against Node's
//! checksums), or `go install`. A server the user installed (in the project,
//! a venv or on PATH) always wins over Kuana's copy.
//!
//! Each install goes in a folder per version, with `current` naming the one
//! in use, so a newer version can arrive while the old one runs. Kuana looks
//! for one at most once a week, after starting a server it installed; the new
//! version runs from the next start.

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, RwLock},
    time::{Duration, SystemTime},
};

use gpui::{App, Global};
use serde_json::Value;

use super::servers::{self, ServerSpec};

/// A server Kuana can install: the command it provides (a candidate in a
/// `ServerSpec`) and where it comes from.
pub struct Installer {
    pub command: &'static str,
    source: Source,
}

impl PartialEq for Installer {
    fn eq(&self, other: &Self) -> bool {
        self.command == other.command
    }
}

impl std::fmt::Debug for Installer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Installer({})", self.command)
    }
}

enum Source {
    /// A release asset on GitHub: the binary itself, gzipped, or in a zip.
    GitHub { repo: &'static str, asset: Asset },
    Npm { package: &'static str },
    /// `go install <module>@latest`, with the user's Go.
    Go { module: &'static str },
}

/// Which release asset is this computer's.
enum Asset {
    Fn(fn() -> Option<String>),
    /// By `<os>-<arch>` as Rust names them: "macos-aarch64", "linux-x86_64".
    PerPlatform(Vec<(String, String)>),
}

impl Asset {
    fn name(&self) -> Option<String> {
        match self {
            Asset::Fn(asset) => asset(),
            Asset::PerPlatform(assets) => {
                let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
                assets.iter().find(|(p, _)| *p == platform).map(|(_, name)| name.clone())
            }
        }
    }
}

const INSTALLERS: &[Installer] = &[
    Installer { command: "rust-analyzer", source: Source::GitHub { repo: "rust-lang/rust-analyzer", asset: Asset::Fn(rust_analyzer_asset) } },
    Installer { command: "taplo", source: Source::GitHub { repo: "tamasfe/taplo", asset: Asset::Fn(taplo_asset) } },
    Installer { command: "marksman", source: Source::GitHub { repo: "artempyanykh/marksman", asset: Asset::Fn(marksman_asset) } },
    Installer { command: "vtsls", source: Source::Npm { package: "@vtsls/language-server" } },
    Installer { command: "basedpyright-langserver", source: Source::Npm { package: "basedpyright" } },
    Installer { command: "phpactor", source: Source::GitHub { repo: "phpactor/phpactor", asset: Asset::Fn(phpactor_asset) } },
    Installer { command: "intelephense", source: Source::Npm { package: "intelephense" } },
    Installer { command: "vscode-css-language-server", source: Source::Npm { package: "vscode-langservers-extracted" } },
    Installer { command: "vscode-html-language-server", source: Source::Npm { package: "vscode-langservers-extracted" } },
    Installer { command: "vscode-json-language-server", source: Source::Npm { package: "vscode-langservers-extracted" } },
    Installer { command: "yaml-language-server", source: Source::Npm { package: "yaml-language-server" } },
    Installer { command: "bash-language-server", source: Source::Npm { package: "bash-language-server" } },
    Installer { command: "gopls", source: Source::Go { module: "golang.org/x/tools/gopls" } },
    Installer { command: "copilot-language-server", source: Source::Npm { package: "@github/copilot-language-server" } },
];

/// Installers extensions declare (`[language_servers.<id>.installer]`).
static EXTENSION_INSTALLERS: RwLock<Vec<&'static Installer>> = RwLock::new(Vec::new());

/// Replaces the installers extensions declare (after they reload).
pub fn set_extension_installers(installers: Vec<&'static Installer>) {
    *EXTENSION_INSTALLERS.write().unwrap() = installers;
}

/// Kuana's own installers, then extensions'.
fn all() -> Vec<&'static Installer> {
    INSTALLERS.iter().chain(EXTENSION_INSTALLERS.read().unwrap().iter().copied()).collect()
}

impl Installer {
    /// From an extension's `[language_servers.<id>.installer]` for its
    /// `command`: `npm = "package"`, `go = "module"`, or
    /// `github = "owner/repo"` with `assets = { "macos-aarch64" = "…" }`.
    pub fn from_manifest(command: &'static str, table: &toml::Table) -> Result<Installer, String> {
        let text = |key: &str| table.get(key).and_then(toml::Value::as_str).map(|s| -> &'static str { Box::leak(s.to_string().into_boxed_str()) });
        let source = if let Some(package) = text("npm") {
            Source::Npm { package }
        } else if let Some(module) = text("go") {
            Source::Go { module }
        } else if let Some(repo) = text("github") {
            let assets = table.get("assets").and_then(toml::Value::as_table).ok_or("a GitHub installer needs assets")?;
            let assets = assets.iter().filter_map(|(platform, name)| Some((platform.clone(), name.as_str()?.to_string()))).collect();
            Source::GitHub { repo, asset: Asset::PerPlatform(assets) }
        } else {
            return Err("an installer needs npm, go or github".into());
        };
        Ok(Installer { command, source })
    }
}

fn rust_analyzer_asset() -> Option<String> {
    let arch = std::env::consts::ARCH;
    Some(match std::env::consts::OS {
        "macos" => format!("rust-analyzer-{arch}-apple-darwin.gz"),
        "linux" => format!("rust-analyzer-{arch}-unknown-linux-gnu.gz"),
        "windows" => format!("rust-analyzer-{arch}-pc-windows-msvc.zip"),
        _ => return None,
    })
}

fn taplo_asset() -> Option<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        _ => return None,
    };
    Some(format!("taplo-{os}-{}.gz", std::env::consts::ARCH))
}

/// phpactor's release is a PHP archive, which runs with the PHP on PATH (its
/// `#!/usr/bin/env php`); Windows can't start it that way.
fn phpactor_asset() -> Option<String> {
    (!cfg!(windows)).then(|| "phpactor.phar".into())
}

fn marksman_asset() -> Option<String> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => "marksman-macos".into(),
        ("linux", "x86_64") => "marksman-linux-x64".into(),
        ("linux", "aarch64") => "marksman-linux-arm64".into(),
        ("windows", "x86_64") => "marksman.exe".into(),
        _ => return None,
    })
}

impl Installer {
    /// What Install will do, for its tooltip.
    pub fn describe(&self) -> String {
        match self.source {
            Source::GitHub { repo, .. } => format!("Downloads {} from github.com/{repo}", self.command),
            Source::Npm { package } => {
                let node = if servers::which("node").is_some() { "" } else { " (and Node.js, which isn't installed)" };
                format!("Installs the npm package {package}{node}")
            }
            Source::Go { module } => format!("Runs go install {module}@latest"),
        }
    }

    /// Its folder in `<config>/servers/`, holding a folder per version.
    fn home(&self) -> Option<PathBuf> {
        let name = match self.source {
            Source::Npm { package } => package.trim_start_matches('@').replace('/', "-"),
            _ => self.command.to_string(),
        };
        Some(servers_dir()?.join(name))
    }

    /// The command inside a version folder.
    fn binary(&self, dir: &Path) -> PathBuf {
        match self.source {
            Source::Npm { .. } => dir.join("node_modules").join(".bin").join(self.command),
            _ => dir.join(exe_name(self.command)),
        }
    }
}

/// Where Kuana keeps the servers it installs.
pub fn servers_dir() -> Option<PathBuf> {
    crate::theme::config_dir().map(|d| d.join("servers"))
}

/// The installer for a server, by the first candidate Kuana can install.
pub fn for_spec(spec: &ServerSpec) -> Option<&'static Installer> {
    let installers = all();
    spec.candidates
        .iter()
        .filter(|(command, _)| servers::usable(command))
        .find_map(|(command, _)| installers.iter().find(|i| i.command == *command).copied())
}

/// Kuana's copy of `command`, if it installed one.
pub fn installed(command: &str) -> Option<PathBuf> {
    all().into_iter().filter(|i| i.command == command).find_map(|i| servers::executable(&i.binary(&current(&i.home()?)?)))
}

/// The version folder in use.
fn current(home: &Path) -> Option<PathBuf> {
    let name = std::fs::read_to_string(home.join("current")).ok()?;
    let dir = home.join(name.trim());
    dir.is_dir().then_some(dir)
}

/// Makes `version` the one in use and removes the others (one still running
/// may not go; a later update removes it).
fn set_current(home: &Path, version: &str) -> Result<(), String> {
    let temp = home.join("current.tmp");
    std::fs::write(&temp, version).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, home.join("current")).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(home).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() && entry.file_name() != version {
            std::fs::remove_dir_all(&path).ok();
        }
    }
    Ok(())
}

fn exe_name(command: &str) -> String {
    if cfg!(windows) { format!("{command}.exe") } else { command.to_string() }
}

// ---- Installing -----------------------------------------------------------------------

/// Servers being installed (by command), so a second click or window waits.
static ACTIVE: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

pub fn installing(command: &str) -> bool {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).contains(&command)
}

/// Bumped when a server was installed, so every window starts the servers it
/// was missing.
#[derive(Default)]
pub struct ServersInstalled(pub u64);
impl Global for ServersInstalled {}

pub fn installed_now(cx: &mut App) {
    let next = cx.try_global::<ServersInstalled>().map_or(1, |s| s.0 + 1);
    cx.set_global(ServersInstalled(next));
}

/// Installs the newest version. Blocking (downloads, npm, go).
pub fn install(installer: &'static Installer) -> Result<(), String> {
    {
        let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        if active.contains(&installer.command) {
            return Err(format!("{} is already being installed", installer.command));
        }
        active.push(installer.command);
    }
    let result = install_now(installer);
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).retain(|c| *c != installer.command);
    result
}

fn install_now(installer: &Installer) -> Result<(), String> {
    let home = installer.home().ok_or("Kuana has no config folder")?;
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    match &installer.source {
        Source::GitHub { repo, asset } => install_github(installer, &home, repo, asset),
        Source::Npm { package } => install_npm(installer, &home, package),
        Source::Go { module } => install_go(installer, &home, module),
    }
}

/// A file name safe for any version string (tags like `2026-10-12`, `v1.2`).
fn folder_name(version: &str) -> String {
    version.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect()
}

fn install_github(installer: &Installer, home: &Path, repo: &str, asset: &Asset) -> Result<(), String> {
    let asset = asset.name().ok_or_else(|| format!("{} has no build for this computer", installer.command))?;
    let release: Value = serde_json::from_slice(&crate::updater::fetch(&format!("https://api.github.com/repos/{repo}/releases/latest"))?)
        .map_err(|e| format!("GitHub's answer didn't parse: {e}"))?;
    let tag = release["tag_name"].as_str().ok_or("GitHub listed no release")?;
    let found = release["assets"].as_array().into_iter().flatten().find(|a| a["name"].as_str() == Some(asset.as_str()));
    let found = found.ok_or_else(|| format!("{repo}'s latest release has no {asset}"))?;
    let url = found["browser_download_url"].as_str().ok_or("the release lists no download address")?;
    let version = folder_name(tag);
    if installer.binary(&home.join(&version)).is_file() {
        return set_current(home, &version);
    }

    let temp = home.join(format!("{asset}.download"));
    crate::updater::download(url, &temp, installer.command, found["size"].as_u64())?;
    // GitHub's digest, where the release has one.
    if let Some(expected) = found["digest"].as_str().and_then(|d| d.strip_prefix("sha256:"))
        && crate::updater::sha256_hex(&temp).as_deref() != Some(expected)
    {
        std::fs::remove_file(&temp).ok();
        return Err(format!("{asset} didn't match its checksum"));
    }
    let installed = install_into(home, &version, |dir| {
        let binary = installer.binary(dir);
        if asset.ends_with(".gz") {
            let mut input = flate2::read::GzDecoder::new(std::fs::File::open(&temp).map_err(|e| e.to_string())?);
            let mut output = std::fs::File::create(&binary).map_err(|e| e.to_string())?;
            std::io::copy(&mut input, &mut output).map_err(|e| format!("couldn't unpack {asset}: {e}"))?;
        } else if asset.ends_with(".zip") {
            unzip(&temp, dir)?;
            // The binary may sit in a folder inside the archive.
            let name = exe_name(installer.command);
            let inside = find_file(dir, &name).ok_or_else(|| format!("{asset} has no {name}"))?;
            if inside != binary {
                std::fs::rename(&inside, &binary).map_err(|e| e.to_string())?;
            }
        } else {
            std::fs::copy(&temp, &binary).map_err(|e| e.to_string())?;
        }
        make_executable(&binary)
    });
    std::fs::remove_file(&temp).ok();
    installed
}

/// Fills version folder `version` through `fill`, which gets a folder beside
/// it that's renamed into place only once it's done, then makes it current:
/// an install that stops half way (Kuana quit, the disk filled) never looks
/// installed.
fn install_into(home: &Path, version: &str, fill: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), String> {
    let staging = home.join(format!("{version}.partial-{}", std::process::id()));
    std::fs::remove_dir_all(&staging).ok();
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let dir = home.join(version);
    let filled = fill(&staging).and_then(|()| {
        // One left by a Kuana from before installs were staged.
        std::fs::remove_dir_all(&dir).ok();
        std::fs::rename(&staging, &dir).map_err(|e| e.to_string())
    });
    if filled.is_err() {
        std::fs::remove_dir_all(&staging).ok();
    }
    filled?;
    set_current(home, version)
}

fn install_npm(installer: &Installer, home: &Path, package: &str) -> Result<(), String> {
    let version = npm_latest(package)?;
    let folder = folder_name(&version);
    if servers::executable(&installer.binary(&home.join(&folder))).is_some() {
        return set_current(home, &folder);
    }
    install_into(home, &folder, |dir| {
        npm_install(dir, package, &version)?;
        servers::executable(&installer.binary(dir)).map(drop).ok_or_else(|| format!("{package} has no {}", installer.command))
    })
}

/// The newest version of an npm package, from the registry.
pub(crate) fn npm_latest(package: &str) -> Result<String, String> {
    let latest: Value = serde_json::from_slice(&crate::updater::fetch(&format!("https://registry.npmjs.org/{package}/latest"))?)
        .map_err(|e| format!("npm's answer didn't parse: {e}"))?;
    latest["version"].as_str().map(str::to_string).ok_or_else(|| format!("npm knows no version of {package}"))
}

/// `npm install <package>@<version>` into `dir/node_modules` (with Kuana's
/// Node.js when the user has none). Blocking.
pub(crate) fn npm_install(dir: &Path, package: &str, version: &str) -> Result<(), String> {
    let (npm, node_bin) = npm()?;
    let output = crate::process::command(&npm)
        .args(["install", "--no-audit", "--no-fund", "--loglevel=error", "--prefix"])
        .arg(dir)
        .arg(format!("{package}@{version}"))
        .current_dir(dir)
        .env("PATH", path_with(node_bin.as_deref()))
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("couldn't run npm: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("it gave no reason").trim().to_string();
    Err(format!("npm couldn't install {package}: {detail}"))
}

fn install_go(installer: &Installer, home: &Path, module: &str) -> Result<(), String> {
    let go = servers::which("go").ok_or_else(|| format!("{} needs Go (go.dev/dl); install it, then try again", installer.command))?;
    // `go install` decides the version; this folder is replaced on a reinstall.
    let dir = home.join("latest");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let output = crate::process::command(&go)
        .args(["install", &format!("{module}@latest")])
        .env("GOBIN", &dir)
        .env("PATH", servers::search_path())
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("couldn't run go: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("it gave no reason").trim().to_string();
        return Err(format!("go install failed: {detail}"));
    }
    set_current(home, "latest")
}

// ---- Node.js ---------------------------------------------------------------------------

/// The folder with Kuana's own `node`, if it downloaded one.
pub fn node_bin() -> Option<PathBuf> {
    let dir = current(&servers_dir()?.join("node"))?;
    Some(if cfg!(windows) { dir } else { dir.join("bin") })
}

/// The search path with Kuana's Node.js first (npm packages start with
/// `#!/usr/bin/env node`).
pub fn path_with(node_bin: Option<&Path>) -> std::ffi::OsString {
    let path = servers::search_path();
    let Some(bin) = node_bin else { return path.clone() };
    std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(std::env::split_paths(path))).unwrap_or_else(|_| path.clone())
}

/// The `node` that runs npm servers: the user's, else Kuana's own
/// (downloaded now if it has none yet). Blocking.
pub(crate) fn node() -> Result<PathBuf, String> {
    let bin = match npm()? {
        (_, Some(bin)) => bin,
        (_, None) => return servers::which("node").ok_or_else(|| "Node.js isn't installed".to_string()),
    };
    servers::executable(&bin.join("node")).ok_or_else(|| "the Node.js download has no node".to_string())
}

/// npm, and the folder Kuana's `node` is in when the user has none.
fn npm() -> Result<(PathBuf, Option<PathBuf>), String> {
    if let (Some(npm), Some(_)) = (servers::which("npm"), servers::which("node")) {
        return Ok((npm, None));
    }
    let bin = match node_bin() {
        Some(bin) => bin,
        None => {
            install_node()?;
            node_bin().ok_or("Node.js didn't unpack as expected")?
        }
    };
    let npm = servers::executable(&bin.join("npm")).ok_or("the Node.js download has no npm")?;
    Ok((npm, Some(bin)))
}

/// Downloads the current LTS of Node.js into `<servers>/node/`, checked
/// against the release's SHASUMS256.txt.
fn install_node() -> Result<(), String> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("windows", "x86_64") => "win-x64",
        ("windows", "aarch64") => "win-arm64",
        _ => return Err("Node.js has no build for this computer; install it, then try again".into()),
    };
    let index: Value = serde_json::from_slice(&crate::updater::fetch("https://nodejs.org/dist/index.json")?)
        .map_err(|e| format!("nodejs.org's answer didn't parse: {e}"))?;
    let lts = index.as_array().into_iter().flatten().find(|r| r["lts"].is_string()).ok_or("nodejs.org listed no LTS release")?;
    let version = lts["version"].as_str().ok_or("nodejs.org listed no version")?;
    let name = format!("node-{version}-{platform}.{}", if cfg!(windows) { "zip" } else { "tar.gz" });
    let base = format!("https://nodejs.org/dist/{version}");
    let sums = String::from_utf8_lossy(&crate::updater::fetch(&format!("{base}/SHASUMS256.txt"))?).into_owned();
    let expected = sums
        .lines()
        .find_map(|l| l.strip_suffix(&format!("  {name}")).map(str::to_string))
        .ok_or_else(|| format!("SHASUMS256.txt doesn't list {name}"))?;

    let home = servers_dir().ok_or("Kuana has no config folder")?.join("node");
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let temp = home.join(format!("{name}.download"));
    crate::updater::download(&format!("{base}/{name}"), &temp, "Node.js", None)?;
    if crate::updater::sha256_hex(&temp).as_deref() != Some(expected.as_str()) {
        std::fs::remove_file(&temp).ok();
        return Err("the Node.js download didn't match its checksum".into());
    }
    let unpacked = unzip(&temp, &home);
    std::fs::remove_file(&temp).ok();
    unpacked?;
    set_current(&home, &format!("node-{version}-{platform}"))
}

// ---- Files ------------------------------------------------------------------------------

/// Unpacks a .zip or .tar.gz with `tar`, which macOS, Linux and Windows 10+
/// have (bsdtar reads zips too). GNU tar doesn't, so on Linux a zip (by its
/// first bytes: the name may not say) goes to `unzip`.
pub(crate) fn unzip(archive: &Path, into: &Path) -> Result<(), String> {
    let mut command = if cfg!(target_os = "linux") && is_zip(archive) {
        let mut unzip = crate::process::command("unzip");
        unzip.args(["-o", "-q"]).arg(archive).arg("-d").arg(into);
        unzip
    } else {
        let mut tar = crate::process::command("tar");
        tar.arg("-xf").arg(archive).arg("-C").arg(into);
        tar
    };
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command.stdin(std::process::Stdio::null()).output().map_err(|e| format!("couldn't run {program}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("couldn't unpack {}: {}", archive.display(), String::from_utf8_lossy(&output.stderr).trim()))
    }
}

fn is_zip(path: &Path) -> bool {
    let mut magic = [0; 4];
    std::fs::File::open(path).and_then(|mut file| std::io::Read::read_exact(&mut file, &mut magic)).is_ok() && magic == *b"PK\x03\x04"
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if entry.file_name() == name {
            return Some(path);
        }
    }
    None
}

pub(crate) fn make_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let _ = path;
    Ok(())
}

// ---- Updates ----------------------------------------------------------------------------

/// After Kuana starts a server it installed: at most once a week, installs a
/// newer version if there is one (it runs from the next start). Blocking;
/// errors are left for the next week.
pub fn check_for_update(program: &Path) {
    let Some(dir) = servers_dir() else { return };
    if !program.starts_with(&dir) {
        return;
    }
    let Some(installer) = all().into_iter().find(|i| i.home().is_some_and(|home| program.starts_with(home))) else { return };
    if matches!(installer.source, Source::Go { .. }) {
        return;
    }
    let Some(home) = installer.home() else { return };
    let stamp = home.join("checked");
    let fresh = std::fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .is_some_and(|age| age < Duration::from_secs(7 * 24 * 3600));
    if fresh || std::fs::write(&stamp, "").is_err() {
        return;
    }
    if let Err(err) = install(installer) {
        eprintln!("kuana: couldn't update {}: {err}", installer.command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_declare_installers() {
        let table = |text: &str| -> toml::Table { toml::from_str(text).unwrap() };
        let npm = Installer::from_manifest("mylang-ls", &table(r#"npm = "@me/mylang-ls""#)).unwrap();
        assert!(matches!(npm.source, Source::Npm { package: "@me/mylang-ls" }));
        let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let github = Installer::from_manifest("x", &table(&format!("github = \"me/x\"\nassets = {{ \"{platform}\" = \"x.gz\" }}"))).unwrap();
        match &github.source {
            Source::GitHub { repo, asset } => assert_eq!((*repo, asset.name()), ("me/x", Some("x.gz".to_string()))),
            _ => panic!("not GitHub"),
        }
        assert!(Installer::from_manifest("x", &table(r#"github = "me/x""#)).is_err());
        assert!(Installer::from_manifest("x", &table("")).is_err());
    }

    #[test]
    fn every_built_in_server_with_an_installer_is_known() {
        // Each installer names a command some server list asks for.
        for installer in INSTALLERS {
            let mut specs = crate::syntax::Language::ALL.iter().filter_map(|l| servers::spec(*l)).chain([&servers::COPILOT]);
            let wanted = specs.any(|s| s.candidates.iter().any(|(c, _)| *c == installer.command));
            assert!(wanted, "{} isn't a candidate of any built-in server", installer.command);
        }
        assert_eq!(folder_name("2026-10-12"), "2026-10-12");
        // Zips go by their first bytes (an extension's download has no suffix).
        let file = std::env::temp_dir().join(format!("kuana-is-zip-{}", std::process::id()));
        std::fs::write(&file, b"PK\x03\x04 rest").unwrap();
        assert!(is_zip(&file));
        std::fs::write(&file, b"\x1f\x8b gzip").unwrap();
        assert!(!is_zip(&file));
        std::fs::remove_file(&file).ok();
        assert_eq!(folder_name("v1.2/x"), "v1.2_x");
        assert!(rust_analyzer_asset().is_some() && taplo_asset().is_some());
        assert_eq!(phpactor_asset().is_some(), !cfg!(windows));
    }

    #[test]
    fn a_half_done_install_never_looks_installed() {
        let home = std::env::temp_dir().join(format!("kuana-install-staged-{}", std::process::id()));
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).unwrap();
        let failed = install_into(&home, "1.0", |dir| {
            std::fs::write(dir.join("server"), "half of it").unwrap();
            Err("the disk filled".into())
        });
        assert_eq!(failed, Err("the disk filled".into()));
        assert!(!home.join("1.0").exists() && current(&home).is_none());
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0, "nothing left behind");

        install_into(&home, "1.0", |dir| std::fs::write(dir.join("server"), "all of it").map_err(|e| e.to_string())).unwrap();
        assert_eq!(std::fs::read_to_string(home.join("1.0/server")).unwrap(), "all of it");
        assert_eq!(current(&home), Some(home.join("1.0")));
        std::fs::remove_dir_all(&home).ok();
    }

    /// The real thing: downloads marksman from GitHub into a temporary
    /// config folder. `cargo test -- --ignored installs_from_github`.
    #[test]
    #[ignore = "downloads from GitHub"]
    fn installs_from_github() {
        let config = std::env::temp_dir().join(format!("kuana-install-{}", std::process::id()));
        // SAFETY: tests that touch XDG_CONFIG_HOME run alone (--ignored, one test).
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &config) };
        let marksman = INSTALLERS.iter().find(|i| i.command == "marksman").unwrap();
        install(marksman).unwrap();
        let path = installed("marksman").expect("installed");
        let output = std::process::Command::new(&path).arg("--version").output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        std::fs::remove_dir_all(&config).ok();
    }
}
