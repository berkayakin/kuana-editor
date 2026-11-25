//! The agents Kuana can run: the public ACP Registry
//! (`cdn.agentclientprotocol.com/registry/v1/latest/registry.json`, cached a
//! day in `<config>/agents/registry.json`), with a built-in list of the main
//! ones for when it can't be read, and how each is distributed (an npm
//! package run with `npx`, a Python one with `uvx`, or per-platform
//! binaries).

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

pub const REGISTRY_URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";
/// How long the cached registry is good for.
const FRESH_FOR: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Entry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub distribution: Distribution,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Distribution {
    #[serde(default)]
    pub npx: Option<Package>,
    #[serde(default)]
    pub uvx: Option<Package>,
    #[serde(default)]
    pub binary: Option<BTreeMap<String, Binary>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Package {
    /// `name@version` (a scoped name starts with `@`).
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl Package {
    /// The package's name and version, split.
    pub fn name_and_version(&self) -> (&str, Option<&str>) {
        let (scope, rest) = match self.package.strip_prefix('@') {
            Some(rest) => ("@", rest),
            None => ("", self.package.as_str()),
        };
        // npm's `name@1.2.3`, uv's `name==1.2.3`.
        let split = rest.split_once("==").or_else(|| rest.rsplit_once('@'));
        match split {
            Some((name, version)) if !name.is_empty() => (&self.package[..scope.len() + name.len()], Some(version)),
            _ => (&self.package, None),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Binary {
    pub archive: String,
    /// The program, relative to the unpacked archive.
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub sha256: Option<String>,
}

impl Entry {
    /// How it installs on this computer, preferring a binary (no Node
    /// needed), then npm, then uv.
    pub fn install_kind(&self) -> Option<Kind<'_>> {
        if let Some(binary) = platform().and_then(|p| self.distribution.binary.as_ref()?.get(p)) {
            return Some(Kind::Binary(binary));
        }
        if let Some(npx) = &self.distribution.npx {
            return Some(Kind::Npm(npx));
        }
        self.distribution.uvx.as_ref().map(Kind::Uv)
    }
}

pub enum Kind<'a> {
    Binary(&'a Binary),
    Npm(&'a Package),
    Uv(&'a Package),
}

/// The registry's name for this computer.
pub fn platform() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        ("windows", "x86_64") => "windows-x86_64",
        _ => return None,
    })
}

#[derive(Deserialize)]
struct Registry {
    #[serde(default)]
    agents: Vec<Entry>,
}

pub fn parse(json: &[u8]) -> Result<Vec<Entry>, String> {
    let registry: Registry = serde_json::from_slice(json).map_err(|e| format!("The agent registry didn't parse: {e}"))?;
    Ok(registry.agents)
}

/// The main agents, for when the registry can't be read: the npm packages
/// as the registry lists them, unversioned (the latest installs).
pub fn fallback() -> Vec<Entry> {
    let npx = |id: &str, name: &str, description: &str, package: &str, args: &[&str]| Entry {
        id: id.into(),
        name: name.into(),
        version: String::new(),
        description: description.into(),
        website: None,
        repository: None,
        distribution: Distribution {
            npx: Some(Package { package: package.into(), args: args.iter().map(|a| a.to_string()).collect(), env: BTreeMap::new() }),
            ..Distribution::default()
        },
    };
    vec![
        npx("claude-acp", "Claude Agent", "Anthropic's Claude, as in Claude Code", "@agentclientprotocol/claude-agent-acp", &[]),
        npx("codex-acp", "Codex", "OpenAI's coding agent", "@agentclientprotocol/codex-acp", &[]),
        npx("gemini", "Gemini CLI", "Google's Gemini CLI", "@google/gemini-cli", &["--acp"]),
        npx("github-copilot-cli", "GitHub Copilot", "GitHub Copilot's agent", "@github/copilot", &["--acp"]),
        npx("qwen-code", "Qwen Code", "Alibaba's Qwen Code", "@qwen-code/qwen-code", &["--acp"]),
    ]
}

fn cache_path() -> Option<PathBuf> {
    super::install::agents_dir().map(|d| d.join("registry.json"))
}

/// The registry: the cache while fresh, else downloaded (and cached), else
/// the stale cache, else the built-in list. Blocking.
pub fn load() -> Vec<Entry> {
    let cache = cache_path();
    let cached = cache.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| parse(&b).ok());
    let fresh = cache
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < FRESH_FOR);
    if fresh && let Some(agents) = &cached {
        return agents.clone();
    }
    if !cfg!(test)
        && let Ok(bytes) = crate::updater::fetch(REGISTRY_URL)
        && let Ok(agents) = parse(&bytes)
    {
        if let Some(path) = &cache {
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(path));
            let _ = crate::state::write_atomic(path, &bytes);
        }
        return agents;
    }
    cached.unwrap_or_else(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "version": "1.0.0",
        "agents": [
            { "id": "claude-acp", "name": "Claude Agent", "version": "0.110.0", "description": "ACP wrapper for Anthropic's Claude",
              "distribution": {
                "npx": { "package": "@agentclientprotocol/claude-agent-acp@0.110.0" },
                "binary": { "darwin-aarch64": { "archive": "https://x/claude-agent-acp-darwin-arm64.zip", "cmd": "./claude-agent-acp", "sha256": "ab" } } } },
            { "id": "gemini", "name": "Gemini CLI", "version": "0.71.1",
              "distribution": { "npx": { "package": "@google/gemini-cli@0.71.1", "args": ["--acp"] } } },
            { "id": "fast-agent", "name": "fast-agent", "version": "0.10.66",
              "distribution": { "uvx": { "package": "fast-agent-acp==0.10.66", "args": ["-x"], "env": { "FAST_AGENT_MODEL": "x" } } } },
            { "id": "odd", "name": "Odd", "future_key": 1 }
        ],
        "extensions": []
    }"#;

    #[test]
    fn the_registry_parses_and_says_how_each_installs() {
        let agents = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(agents.len(), 4);
        let claude = &agents[0];
        match claude.install_kind() {
            Some(Kind::Binary(b)) => {
                assert_eq!(platform(), Some("darwin-aarch64"), "only this platform has a binary in the sample");
                assert_eq!(b.cmd, "./claude-agent-acp");
            }
            Some(Kind::Npm(p)) => assert_eq!(p.name_and_version(), ("@agentclientprotocol/claude-agent-acp", Some("0.110.0"))),
            _ => panic!("claude has npm and a binary"),
        }
        assert!(matches!(agents[1].install_kind(), Some(Kind::Npm(p)) if p.args == ["--acp"]));
        assert!(matches!(agents[2].install_kind(), Some(Kind::Uv(p)) if p.name_and_version() == ("fast-agent-acp", Some("0.10.66"))));
        assert!(agents[3].install_kind().is_none());
    }

    #[test]
    fn package_names_split_from_versions() {
        let p = |s: &str| Package { package: s.into(), args: vec![], env: BTreeMap::new() };
        assert_eq!(p("@google/gemini-cli@0.71.1").name_and_version(), ("@google/gemini-cli", Some("0.71.1")));
        assert_eq!(p("@google/gemini-cli").name_and_version(), ("@google/gemini-cli", None));
        assert_eq!(p("cline@3.0.75").name_and_version(), ("cline", Some("3.0.75")));
        assert_eq!(p("droid").name_and_version(), ("droid", None));
    }

    #[test]
    fn the_fallback_has_the_main_agents() {
        let ids: Vec<String> = fallback().into_iter().map(|e| e.id).collect();
        for id in ["claude-acp", "codex-acp", "gemini"] {
            assert!(ids.contains(&id.to_string()));
        }
    }
}
