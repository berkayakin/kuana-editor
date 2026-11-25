//! Installing registry agents into `<config>/agents/<id>/<version>/`, and the
//! command that starts an installed one. npm packages install with the
//! user's npm (else Kuana's own Node.js, as language servers do) and run
//! with `node`, binaries are downloaded and checked against the registry's
//! SHA-256, Python packages run through the user's `uvx`. What's installed is
//! noted in `<config>/agents/<id>/installed.json`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::client::Launch;
use super::registry::{Entry, Kind};
use crate::lsp::{install as lsp_install, servers};

pub fn agents_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!("kuana-test-agents-{}", std::process::id())));
    }
    crate::theme::config_dir().map(|d| d.join("agents"))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Installed {
    /// A Node.js script, run with `node`.
    Npm { version: String, script: PathBuf, args: Vec<String>, env: BTreeMap<String, String> },
    Binary { version: String, program: PathBuf, args: Vec<String>, env: BTreeMap<String, String> },
    /// Run with `uvx <package>`; uv keeps it.
    Uv { version: String, package: String, args: Vec<String>, env: BTreeMap<String, String> },
}

impl Installed {
    pub fn version(&self) -> &str {
        match self {
            Installed::Npm { version, .. } | Installed::Binary { version, .. } | Installed::Uv { version, .. } => version,
        }
    }
}

fn record_path(id: &str) -> Option<PathBuf> {
    Some(agents_dir()?.join(sanitize(id)).join("installed.json"))
}

/// An id as a folder name.
fn sanitize(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect()
}

/// What's installed of an agent, if anything still there.
pub fn installed(id: &str) -> Option<Installed> {
    let record: Installed = serde_json::from_slice(&std::fs::read(record_path(id)?).ok()?).ok()?;
    let there = match &record {
        Installed::Npm { script, .. } => script.is_file(),
        Installed::Binary { program, .. } => program.is_file(),
        Installed::Uv { .. } => true,
    };
    there.then_some(record)
}

/// Whether the registry has a newer version than the one installed.
pub fn update_available(entry: &Entry) -> bool {
    installed(&entry.id).is_some_and(|i| !entry.version.is_empty() && i.version() != entry.version)
}

/// Installs (or updates) an agent. Blocking: downloads and npm.
pub fn install(entry: &Entry) -> Result<Installed, String> {
    let kind = entry.install_kind().ok_or_else(|| format!("{} has no build for this computer", entry.name))?;
    let home = agents_dir().ok_or("Kuana has no config folder")?.join(sanitize(&entry.id));
    let record = match kind {
        Kind::Npm(package) => {
            let (name, pinned) = package.name_and_version();
            let version = match pinned {
                Some(v) => v.to_string(),
                None => lsp_install::npm_latest(name)?,
            };
            let dir = home.join(sanitize(&version));
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            lsp_install::npm_install(&dir, name, &version)?;
            let script = npm_bin(&dir, name)?;
            Installed::Npm { version, script, args: package.args.clone(), env: package.env.clone() }
        }
        Kind::Binary(binary) => {
            let version = if entry.version.is_empty() { "latest".to_string() } else { entry.version.clone() };
            let dir = home.join(sanitize(&version));
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let file = binary.archive.rsplit('/').next().unwrap_or("agent").to_string();
            let temp = dir.join(format!("{file}.download"));
            crate::updater::download(&binary.archive, &temp, &entry.name, None)?;
            if let Some(expected) = &binary.sha256
                && crate::updater::sha256_hex(&temp).as_deref() != Some(expected.to_ascii_lowercase().as_str())
            {
                std::fs::remove_file(&temp).ok();
                return Err(format!("The download of {} didn't match its checksum", entry.name));
            }
            let unpacked = lsp_install::unzip(&temp, &dir);
            std::fs::remove_file(&temp).ok();
            unpacked?;
            let program = dir.join(binary.cmd.trim_start_matches("./").replace('\\', "/"));
            if !program.is_file() {
                return Err(format!("{} didn't unpack as expected (no {})", entry.name, binary.cmd));
            }
            lsp_install::make_executable(&program)?;
            Installed::Binary { version, program, args: binary.args.clone(), env: binary.env.clone() }
        }
        Kind::Uv(package) => {
            if servers::which("uvx").is_none() {
                return Err(format!("{} runs with uv (docs.astral.sh/uv); install it, then try again", entry.name));
            }
            let (_, version) = package.name_and_version();
            Installed::Uv { version: version.unwrap_or("latest").to_string(), package: package.package.clone(), args: package.args.clone(), env: package.env.clone() }
        }
    };
    let path = record_path(&entry.id).ok_or("Kuana has no config folder")?;
    let json = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
    crate::state::write_atomic(&path, &json).map_err(|e| e.to_string())?;
    Ok(record)
}

/// The script an npm package runs, from its `package.json` `bin`.
fn npm_bin(prefix: &Path, name: &str) -> Result<PathBuf, String> {
    let package_dir = prefix.join("node_modules").join(name);
    let manifest: Value = serde_json::from_slice(&std::fs::read(package_dir.join("package.json")).map_err(|e| format!("{name} didn't install: {e}"))?)
        .map_err(|e| format!("{name}'s package.json didn't parse: {e}"))?;
    let short = name.rsplit('/').next().unwrap_or(name);
    let bin = match &manifest["bin"] {
        Value::String(path) => path.clone(),
        Value::Object(bins) => bins
            .get(short)
            .or_else(|| bins.iter().find(|(k, _)| k.contains("acp")).map(|(_, v)| v))
            .or_else(|| bins.values().next())
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{name} names no program"))?
            .to_string(),
        _ => return Err(format!("{name} names no program")),
    };
    Ok(package_dir.join(bin))
}

/// The command that starts an installed agent in `cwd`.
pub fn launch(installed: &Installed, cwd: &Path) -> Result<Launch, String> {
    let env_of = |env: &BTreeMap<String, String>| env.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>();
    Ok(match installed {
        Installed::Npm { script, args, env, .. } => {
            let node = lsp_install::node()?;
            let mut all = vec![script.display().to_string()];
            all.extend(args.iter().cloned());
            let mut env = env_of(env);
            // The agent's own child processes (npx, git…) find Node and the
            // user's tools.
            env.push(("PATH".into(), lsp_install::path_with(node.parent()).to_string_lossy().into_owned()));
            Launch { program: node, args: all, env, cwd: cwd.to_path_buf() }
        }
        Installed::Binary { program, args, env, .. } => {
            let mut env = env_of(env);
            env.push(("PATH".into(), servers::search_path().to_string_lossy().into_owned()));
            Launch { program: program.clone(), args: args.clone(), env, cwd: cwd.to_path_buf() }
        }
        Installed::Uv { package, args, env, .. } => {
            let uvx = servers::which("uvx").ok_or("uv isn't installed (docs.astral.sh/uv)")?;
            let mut all = vec![package.clone()];
            all.extend(args.iter().cloned());
            let mut env = env_of(env);
            env.push(("PATH".into(), servers::search_path().to_string_lossy().into_owned()));
            Launch { program: uvx, args: all, env, cwd: cwd.to_path_buf() }
        }
    })
}

/// Removes an agent's installation.
pub fn uninstall(id: &str) -> Result<(), String> {
    let dir = agents_dir().ok_or("Kuana has no config folder")?.join(sanitize(id));
    if dir.exists() {
        crate::file_history::move_to_trash(&dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_programs_come_from_package_json() {
        let prefix = agents_dir().unwrap().join("npm-bin-test");
        let package = prefix.join("node_modules/@scope/agent-acp");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("package.json"), r#"{ "bin": { "helper": "h.js", "agent-acp": "dist/index.js" } }"#).unwrap();
        assert_eq!(npm_bin(&prefix, "@scope/agent-acp").unwrap(), package.join("dist/index.js"));
        std::fs::write(package.join("package.json"), r#"{ "bin": "cli.js" }"#).unwrap();
        assert_eq!(npm_bin(&prefix, "@scope/agent-acp").unwrap(), package.join("cli.js"));
        std::fs::write(package.join("package.json"), r#"{ "name": "x" }"#).unwrap();
        assert!(npm_bin(&prefix, "@scope/agent-acp").is_err());
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn records_round_trip_and_ids_are_safe_folder_names() {
        assert_eq!(sanitize("../evil id"), ".._evil_id");
        let record = Installed::Uv { version: "1".into(), package: "fast-agent-acp==1".into(), args: vec!["-x".into()], env: BTreeMap::new() };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"kind\":\"uv\""));
        assert_eq!(serde_json::from_str::<Installed>(&json).unwrap(), record);
        assert_eq!(installed("never-installed"), None);
    }
}
