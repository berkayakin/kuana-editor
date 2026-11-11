//! Kuana's extension registry: open-source extensions in Zed's format, each
//! built from its author's repository at a reviewed commit by the
//! kuana-extensions repository, and published on its GitHub releases with an
//! index signed by the registry's key. Kuana reads the index (checking the
//! signature), searches it here, and installs a package after checking its
//! SHA-256. Packages come with their grammars and code compiled, so nothing
//! needs a compiler, and with their licenses.
//!
//! An install unpacks beside `extensions/<id>` and renames into place (an
//! update swaps the old one out), with a `.registry` marker: only extensions
//! installed from here get updates from here, never a Git checkout or a
//! linked folder.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::Deserialize;

use super::{extensions_dir, read_toml};

/// The index; its signature is beside it (`.minisig`).
const INDEX: &str = "https://github.com/berkayakin/kuana-extensions/releases/download/packages/index.json";
/// The registry's minisign key (not the one Kuana's releases are signed with).
const PUBLIC_KEY: &str = "RWQWuiIPQiFvuAujZzYq2V7nO8/tSwfUALkMe1hVE60+gBNrdGwgSHew";
/// The index format this Kuana reads.
const FORMAT: u32 = 1;
/// Marks an extension installed from the registry.
const MARKER: &str = ".registry";
/// How long a fetched index is used before it's fetched again.
const FRESH: Duration = Duration::from_secs(60 * 60);

/// An extension the registry has.
#[derive(Clone, Debug, Deserialize)]
pub struct Listed {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Its license (SPDX), which comes with the package.
    #[serde(default)]
    pub license: String,
    /// "languages", "grammars", "language-servers", "themes"…
    #[serde(default)]
    pub provides: Vec<String>,
    pub package: Package,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Package {
    pub url: String,
    pub sha256: String,
    pub size: Option<u64>,
}

#[derive(Deserialize)]
struct Index {
    format: u32,
    extensions: Vec<Listed>,
}

/// The index fetched last, and when.
static FETCHED: Mutex<Option<(Instant, Arc<Vec<Listed>>)>> = Mutex::new(None);

/// The registry's extensions, from an index at most an hour old. Blocking.
pub fn available() -> Result<Arc<Vec<Listed>>, String> {
    if let Some((at, listed)) = FETCHED.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        && at.elapsed() < FRESH
    {
        return Ok(listed.clone());
    }
    let index = index_url();
    let data = read(&index).map_err(|e| format!("Couldn't get the extension list: {e}"))?;
    let signature = read(&format!("{index}.minisig")).map_err(|e| format!("Couldn't get the extension list's signature: {e}"))?;
    let listed = Arc::new(parse(&data, &signature, PUBLIC_KEY)?);
    *FETCHED.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), listed.clone()));
    Ok(listed)
}

/// The index, if the registry's key signed it.
fn parse(data: &[u8], signature: &[u8], key: &str) -> Result<Vec<Listed>, String> {
    let key = minisign_verify::PublicKey::from_base64(key).map_err(|e| e.to_string())?;
    let signature = std::str::from_utf8(signature)
        .ok()
        .and_then(|s| minisign_verify::Signature::decode(s).ok())
        .ok_or("The extension list's signature is unreadable")?;
    key.verify(data, &signature, false).map_err(|_| "The extension list isn't signed by Kuana's registry key".to_string())?;
    let index: Index = serde_json::from_slice(data).map_err(|e| format!("The extension list didn't parse: {e}"))?;
    if index.format > FORMAT {
        return Err("The extension list is newer than this Kuana reads: update Kuana".into());
    }
    Ok(index.extensions)
}

/// Where the index is. Debug builds take `KUANA_REGISTRY` (an index on
/// disk, `file://…`, signed with the same key), for a registry built locally.
fn index_url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(index) = std::env::var("KUANA_REGISTRY") {
        return index;
    }
    INDEX.to_string()
}

/// A file of the registry's (HTTPS only, through curl); in debug builds also
/// one on disk.
fn read(url: &str) -> Result<Vec<u8>, String> {
    #[cfg(debug_assertions)]
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).map_err(|e| format!("{path}: {e}"));
    }
    crate::updater::fetch(url)
}

fn download(url: &str, to: &Path, label: &str, size: Option<u64>) -> Result<(), String> {
    #[cfg(debug_assertions)]
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::copy(path, to).map(|_| ()).map_err(|e| format!("{path}: {e}"));
    }
    crate::updater::download(url, to, label, size)
}

/// The extensions matching `query`, all of them for none: the one named so
/// first, then those whose name has it, then those whose description or
/// what they provide has it; by name within each.
pub fn search(listed: &[Listed], query: &str) -> Vec<Listed> {
    let query = query.trim().to_lowercase();
    let closeness = |e: &Listed| {
        let (id, name) = (e.id.to_lowercase(), e.name.to_lowercase());
        let has = |text: &str| text.to_lowercase().contains(&query);
        if query.is_empty() || id == query || name == query {
            Some(0)
        } else if id.contains(&query) || name.contains(&query) {
            Some(1)
        } else if has(&e.description) || e.provides.iter().any(|p| has(&p.replace('-', " "))) {
            Some(2)
        } else {
            None
        }
    };
    let mut found: Vec<(u8, Listed)> = listed.iter().filter_map(|e| Some((closeness(e)?, e.clone()))).collect();
    found.sort_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.name.to_lowercase().cmp(&y.name.to_lowercase())));
    found.into_iter().map(|(_, e)| e).collect()
}

/// How an installed extension stands with the registry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Installed {
    No,
    /// Installed from the registry, at this version or a newer one.
    Current,
    /// Installed from the registry, which has a newer version.
    Outdated,
    /// Installed another way (Git, a folder, linked from Zed): left alone.
    Elsewhere,
}

/// Whether `listed` is installed, and from where.
pub fn installed(listed: &Listed, extensions: &[super::ExtensionInfo]) -> Installed {
    let Some(extension) = extensions.iter().find(|e| e.id == listed.id) else { return Installed::No };
    if extension.path.is_symlink() || !extension.path.join(MARKER).is_file() {
        Installed::Elsewhere
    } else if crate::updater::is_newer(&listed.version, &extension.version) {
        Installed::Outdated
    } else {
        Installed::Current
    }
}

/// Installs `listed`, or updates the one the registry installed. Returns its
/// name. Blocking.
pub fn install(listed: &Listed) -> Result<String, String> {
    let id = listed.id.as_str();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
        return Err(format!("\"{id}\" isn't an extension id"));
    }
    let dir = extensions_dir().ok_or("no config folder")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let target = dir.join(id);
    if target.is_symlink() || (target.exists() && !target.join(MARKER).is_file()) {
        return Err(format!("{} is installed already, not from the registry", listed.name));
    }
    let pid = std::process::id();
    let archive = dir.join(format!(".download-{id}-{pid}.tar.gz"));
    let unpacked = dir.join(format!(".installing-{id}-{pid}"));
    let _ = std::fs::remove_dir_all(&unpacked);
    let result = (|| {
        download(&listed.package.url, &archive, &listed.name, listed.package.size).map_err(|e| format!("Couldn't download {}: {e}", listed.name))?;
        if crate::updater::sha256_hex(&archive).as_deref() != Some(listed.package.sha256.as_str()) {
            return Err(format!("{}'s download didn't match its checksum", listed.name));
        }
        std::fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
        crate::lsp::install::unzip(&archive, &unpacked)?;
        let manifest = read_toml(&unpacked.join("extension.toml")).map_err(|_| "The package has no extension.toml".to_string())?;
        if manifest.get("id").and_then(toml::Value::as_str) != Some(id) {
            return Err(format!("The package isn't {id}"));
        }
        std::fs::write(unpacked.join(MARKER), "").map_err(|e| e.to_string())?;
        replace(&target, &unpacked)?;
        Ok(manifest.get("name").and_then(toml::Value::as_str).unwrap_or(id).to_string())
    })();
    std::fs::remove_file(&archive).ok();
    std::fs::remove_dir_all(&unpacked).ok();
    result
}

/// Puts `new` at `target`, the old one (an update) out of the way first.
fn replace(target: &Path, new: &Path) -> Result<(), String> {
    if !target.exists() {
        return std::fs::rename(new, target).map_err(|e| e.to_string());
    }
    let old = target.with_file_name(format!(".replaced-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(target, &old).map_err(|e| e.to_string())?;
    if let Err(err) = std::fs::rename(new, target) {
        std::fs::rename(&old, target).ok();
        return Err(err.to_string());
    }
    std::fs::remove_dir_all(&old).ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(id: &str, description: &str, provides: &[&str]) -> Listed {
        Listed {
            id: id.into(),
            name: id.into(),
            version: "1.0.0".into(),
            description: description.into(),
            license: "MIT".into(),
            provides: provides.iter().map(|p| p.to_string()).collect(),
            package: Package { url: String::new(), sha256: String::new(), size: None },
        }
    }

    #[test]
    fn searches_here() {
        let all = [listed("zig", "Zig support.", &["languages"]), listed("dart-tools", "", &[]), listed("dart", "", &[]), listed("vue", "", &["language-servers"])];
        let ids = |found: Vec<Listed>| found.into_iter().map(|e| e.id).collect::<Vec<_>>();
        assert_eq!(ids(search(&all, "Dart")), ["dart", "dart-tools"]);
        assert_eq!(ids(search(&all, "support")), ["zig"]);
        assert_eq!(ids(search(&all, "language server")), ["vue"]);
        assert_eq!(ids(search(&all, "")), ["dart", "dart-tools", "vue", "zig"]);
        assert!(search(&all, "cobol").is_empty());
    }

    /// An index signed with the registry's key reads; one changed after
    /// signing, or signed with another key, doesn't.
    #[test]
    #[ignore = "needs the registry's secret key (~/.minisign/kuana-extensions.key) and minisign"]
    fn reads_only_signed_indexes() {
        let dir = std::env::temp_dir().join(format!("kuana-registry-sign-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let index = dir.join("index.json");
        std::fs::write(&index, r#"{"format": 1, "extensions": [{"id": "x", "name": "X", "version": "1.0.0", "package": {"url": "https://example.com/x.tar.gz", "sha256": "00", "size": 1}}]}"#).unwrap();
        let key = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".minisign/kuana-extensions.key");
        let signed = std::process::Command::new("minisign").arg("-S").arg("-s").arg(&key).arg("-m").arg(&index).status().unwrap();
        assert!(signed.success());
        let data = std::fs::read(&index).unwrap();
        let signature = std::fs::read(dir.join("index.json.minisig")).unwrap();
        assert_eq!(parse(&data, &signature, PUBLIC_KEY).unwrap()[0].id, "x");
        let mut changed = data.clone();
        changed[0] = b' ';
        assert!(parse(&changed, &signature, PUBLIC_KEY).is_err());
        assert!(parse(&data, &signature, "RWR5QO9LE08wWLXENZBBui1DNd0r9UFw0h8OV/EB2bWiFgHtIlRP0K70").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The published registry (or one built locally, its index signed:
    /// `KUANA_REGISTRY=file:///…/dist/index.json`): installs Dart from it,
    /// and refuses a package that doesn't match its checksum. Points the
    /// config folder elsewhere, so it runs alone:
    /// `cargo test installs_from_the_registry -- --ignored`.
    #[test]
    #[ignore = "downloads from the registry; changes XDG_CONFIG_HOME"]
    fn installs_from_the_registry() {
        let config = std::env::temp_dir().join(format!("kuana-registry-local-{}", std::process::id()));
        // SAFETY: this test runs on its own (it's ignored by default).
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &config) };
        let listed = available().unwrap();
        let dart = listed.iter().find(|e| e.id == "dart").expect("Dart in the registry");
        assert_eq!(install(dart), Ok("Dart".to_string()));
        let dir = extensions_dir().unwrap().join("dart");
        for file in ["extension.toml", "extension.wasm", "grammars/dart.wasm", "LICENSE", MARKER] {
            assert!(dir.join(file).is_file(), "{file}");
        }
        let mut tampered = dart.clone();
        tampered.package.sha256 = "0".repeat(64);
        assert!(install(&tampered).unwrap_err().contains("checksum"));
        assert!(dir.join("extension.wasm").is_file(), "the installed one stays");
        std::fs::remove_dir_all(&config).ok();
    }

    #[test]
    fn updates_only_what_the_registry_installed() {
        let dir = std::env::temp_dir().join(format!("kuana-registry-{}", std::process::id()));
        let from_registry = dir.join("x");
        std::fs::create_dir_all(&from_registry).unwrap();
        let info = |version: &str| super::super::ExtensionInfo {
            id: "x".into(),
            name: "X".into(),
            version: version.into(),
            description: String::new(),
            authors: Vec::new(),
            path: from_registry.clone(),
            languages: Vec::new(),
            themes: 0,
            snippets: false,
            servers: Vec::new(),
            building: Vec::new(),
            problems: Vec::new(),
            installers: Vec::new(),
            adapters: Vec::new(),
            ui: None,
        };
        let x = listed("x", "", &[]);
        assert_eq!(installed(&x, &[info("0.9.0")]), Installed::Elsewhere);
        std::fs::write(from_registry.join(MARKER), "").unwrap();
        assert_eq!(installed(&x, &[info("0.9.0")]), Installed::Outdated);
        assert_eq!(installed(&x, &[info("1.0.0")]), Installed::Current);
        assert_eq!(installed(&x, &[]), Installed::No);

        // An update swaps the folder.
        let new = dir.join("new");
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("v2"), "").unwrap();
        replace(&from_registry, &new).unwrap();
        assert!(from_registry.join("v2").is_file() && !from_registry.join(MARKER).exists() && !new.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
