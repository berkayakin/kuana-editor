//! Containers (`connection::Transport::Container`): `docker exec -i` into a
//! running one (any Docker-compatible CLI: `KUANA_DOCKER`), and dev
//! containers, a project's `.devcontainer/devcontainer.json` brought up as
//! a container with the project mounted in it.
//!
//! With the `devcontainer` CLI installed, `up` lets it do everything
//! (features, Docker Compose). Without it, a subset is done here: an `image`
//! or a Dockerfile `build`, the workspace mount, `containerEnv`, `mounts`,
//! `runArgs`, `containerUser` / `remoteUser`, `overrideCommand` and the
//! lifecycle commands. Containers carry the CLI's labels
//! (`devcontainer.local_folder`, `devcontainer.config_file`), so either finds
//! the other's.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use serde_json::Value;

use super::connection::tool_error;

/// A running container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Running {
    pub id: String,
    pub name: String,
    pub image: String,
}

/// The running containers, newest first.
pub fn running(docker: &str) -> Result<Vec<Running>, String> {
    let out = crate::process::command(docker)
        .args(["ps", "--format", "{{.ID}}\t{{.Names}}\t{{.Image}}"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Couldn't run {docker}: {e}"))?;
    if !out.status.success() {
        return Err(tool_error(&out.stderr, "Docker didn't list its containers"));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let (id, name, image) = (parts.next()?, parts.next()?, parts.next().unwrap_or(""));
            Some(Running { id: id.to_string(), name: name.to_string(), image: image.to_string() })
        })
        .collect())
}

/// A folder's dev container configuration: `.devcontainer/devcontainer.json`,
/// `.devcontainer.json`, else the first `.devcontainer/<name>/devcontainer.json`.
pub fn config_file(dir: &Path) -> Option<PathBuf> {
    let candidates = [dir.join(".devcontainer").join("devcontainer.json"), dir.join(".devcontainer.json")];
    if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(found);
    }
    let mut nested: Vec<PathBuf> = std::fs::read_dir(dir.join(".devcontainer")).ok()?.flatten().map(|e| e.path().join("devcontainer.json")).filter(|p| p.is_file()).collect();
    nested.sort();
    nested.into_iter().next()
}

/// A dev container that's up: what to connect to and what to open there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Up {
    pub container: String,
    /// Who works in it (`remoteUser`), else the container's default user.
    pub user: Option<String>,
    /// The project inside it.
    pub workspace: String,
    /// What was left out (features without the CLI).
    pub warnings: Vec<String>,
    /// Ports there to reach from here (`forwardPorts`; another service's, `db:5432`, aren't).
    pub forward_ports: Vec<u16>,
}

/// A configuration's `forwardPorts` that are ports of the container itself.
fn forward_ports(value: &Value) -> Vec<u16> {
    value
        .get("forwardPorts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_u64().and_then(|n| u16::try_from(n).ok()).or_else(|| p.as_str()?.parse().ok()))
        .filter(|&p| p > 0)
        .collect()
}

pub struct UpOptions<'a> {
    pub docker: String,
    /// The `devcontainer` CLI, when installed.
    pub cli: Option<PathBuf>,
    pub progress: &'a dyn Fn(&str),
}

/// Brings up `dir`'s dev container: the one made for it before (started
/// if stopped), else a new one. Blocking: run it in the background.
pub fn up(dir: &Path, options: &UpOptions) -> Result<Up, String> {
    let config = config_file(dir).ok_or_else(|| format!("{} has no dev container configuration", dir.display()))?;
    let text = std::fs::read_to_string(&config).map_err(|e| format!("Couldn't read {}: {e}", config.display()))?;
    let mut value: Value = serde_json::from_str(&crate::jsonc::strip(&text)).map_err(|e| format!("{}: {e}", config.display()))?;
    if let Some(cli) = &options.cli {
        // The CLI makes the container; forwarding is the client's job.
        return up_with_cli(cli, dir, &config, options).map(|up| Up { forward_ports: forward_ports(&value), ..up });
    }
    let basename = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let workspace = value.get("workspaceFolder").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("/workspaces/{basename}"));
    let vars = Variables { local: dir.to_string_lossy().into_owned(), container: substitute_text(&workspace, &Variables { local: dir.to_string_lossy().into_owned(), container: String::new() }) };
    substitute(&mut value, &vars);
    let workspace = vars.container.clone();
    let str_of = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    let user = str_of("remoteUser").or_else(|| str_of("containerUser"));
    let mut warnings = Vec::new();
    if value.get("features").and_then(Value::as_object).is_some_and(|f| !f.is_empty()) {
        warnings.push("Its features weren't installed: they need the devcontainer CLI (npm install -g @devcontainers/cli).".to_string());
    }
    let labels = [format!("devcontainer.local_folder={}", dir.display()), format!("devcontainer.config_file={}", config.display())];
    let docker = |args: &[&str], doing: &str| run(&options.docker, args, doing);

    // The container made for this folder before.
    let mut ps = vec!["ps".to_string(), "-a".into(), "--format".into(), "{{.ID}}\t{{.State}}".into()];
    for label in &labels {
        ps.extend(["--filter".to_string(), format!("label={label}")]);
    }
    let found = docker(&ps.iter().map(String::as_str).collect::<Vec<_>>(), "Docker didn't list its containers")?;
    if let Some((id, state)) = found.lines().next().and_then(|l| l.split_once('\t')) {
        let id = id.to_string();
        if state != "running" {
            (options.progress)("Starting the dev container…");
            docker(&["start", &id], "Couldn't start the dev container")?;
            lifecycle(options, &id, user.as_deref(), &workspace, &value, &["postStartCommand"])?;
        }
        return Ok(Up { container: id, user, workspace, warnings, forward_ports: forward_ports(&value) });
    }
    if value.get("dockerComposeFile").is_some() {
        return Err("This dev container uses Docker Compose, which needs the devcontainer CLI (npm install -g @devcontainers/cli).".into());
    }

    // An image: named, or built from a Dockerfile.
    let config_dir = config.parent().unwrap_or(dir);
    let build = value.get("build");
    let dockerfile = build.and_then(|b| b.get("dockerfile")).or_else(|| value.get("dockerFile")).and_then(Value::as_str);
    let image = match (str_of("image"), dockerfile) {
        (_, Some(dockerfile)) => {
            let context = build.and_then(|b| b.get("context")).or_else(|| value.get("context")).and_then(Value::as_str).unwrap_or(".");
            let tag = format!("kuana-devcontainer-{:016x}", fnv(dir.to_string_lossy().as_bytes()));
            let (dockerfile, context) = (config_dir.join(dockerfile), config_dir.join(context));
            let mut args = vec!["build".to_string(), "-f".into(), dockerfile.display().to_string(), "-t".into(), tag.clone()];
            for (k, v) in build.and_then(|b| b.get("args")).and_then(Value::as_object).into_iter().flatten() {
                args.extend(["--build-arg".to_string(), format!("{k}={}", v.as_str().unwrap_or_default())]);
            }
            if let Some(target) = build.and_then(|b| b.get("target")).and_then(Value::as_str) {
                args.extend(["--target".to_string(), target.to_string()]);
            }
            args.push(context.display().to_string());
            (options.progress)("Building the dev container's image…");
            docker(&args.iter().map(String::as_str).collect::<Vec<_>>(), "Couldn't build the dev container's image")?;
            tag
        }
        (Some(image), None) => image,
        (None, None) => return Err(format!("{} names no image or Dockerfile", config.display())),
    };

    // The container: the project mounted, kept running.
    let mut args = vec!["run".to_string(), "-d".into()];
    for label in &labels {
        args.extend(["--label".to_string(), label.clone()]);
    }
    let mount = str_of("workspaceMount").unwrap_or_else(|| format!("type=bind,source={},target={workspace}", dir.display()));
    args.extend(["--mount".to_string(), mount]);
    for (k, v) in value.get("containerEnv").and_then(Value::as_object).into_iter().flatten() {
        args.extend(["-e".to_string(), format!("{k}={}", v.as_str().unwrap_or_default())]);
    }
    for mount in value.get("mounts").and_then(Value::as_array).into_iter().flatten() {
        let text = match mount {
            Value::String(s) => s.clone(),
            Value::Object(m) => m.iter().map(|(k, v)| format!("{k}={}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect::<Vec<_>>().join(","),
            _ => continue,
        };
        args.extend(["--mount".to_string(), text]);
    }
    if let Some(user) = str_of("containerUser") {
        args.extend(["-u".to_string(), user]);
    }
    args.extend(value.get("runArgs").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string));
    if value.get("overrideCommand").and_then(Value::as_bool) != Some(false) {
        args.extend(["--entrypoint".to_string(), "/bin/sh".into(), image, "-c".into(), "trap 'exit 0' TERM; while sleep 1000 & wait $!; do :; done".into()]);
    } else {
        args.push(image);
    }
    (options.progress)("Starting the dev container…");
    let id = docker(&args.iter().map(String::as_str).collect::<Vec<_>>(), "Couldn't start the dev container")?;
    let id: String = id.trim().chars().take(12).collect();
    (options.progress)("Setting up the dev container…");
    lifecycle(options, &id, user.as_deref(), &workspace, &value, &["onCreateCommand", "updateContentCommand", "postCreateCommand", "postStartCommand"])?;
    Ok(Up { container: id, user, workspace, warnings, forward_ports: forward_ports(&value) })
}

fn up_with_cli(cli: &Path, dir: &Path, config: &Path, options: &UpOptions) -> Result<Up, String> {
    (options.progress)("Starting the dev container (devcontainer up)…");
    let out = crate::process::command(cli)
        .args(["up", "--workspace-folder"])
        .arg(dir)
        .arg("--config")
        .arg(config)
        .args(["--docker-path", &options.docker])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Couldn't run {}: {e}", cli.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let result: Option<Value> = text.lines().rev().find(|l| l.trim_start().starts_with('{')).and_then(|l| serde_json::from_str(l).ok());
    let field = |key: &str| result.as_ref().and_then(|r| r.get(key)).and_then(Value::as_str).map(str::to_string);
    match field("outcome").as_deref() {
        Some("success") => Ok(Up {
            container: field("containerId").map(|id| id.chars().take(12).collect()).ok_or("devcontainer up named no container")?,
            user: field("remoteUser"),
            workspace: field("remoteWorkspaceFolder").ok_or("devcontainer up named no workspace folder")?,
            warnings: Vec::new(),
            forward_ports: Vec::new(),
        }),
        _ => {
            let message = field("message").or_else(|| field("description"));
            match message {
                Some(message) => Err(format!("devcontainer up failed: {message}")),
                None => Err(tool_error(&out.stderr, "devcontainer up failed")),
            }
        }
    }
}

/// Runs the configuration's lifecycle commands (each a string for `sh`, an
/// array to run as is, or an object of either, run in order) in the workspace.
fn lifecycle(options: &UpOptions, id: &str, user: Option<&str>, workspace: &str, value: &Value, keys: &[&str]) -> Result<(), String> {
    for key in keys {
        let Some(command) = value.get(*key) else { continue };
        let commands: Vec<&Value> = match command {
            Value::Object(map) => map.values().collect(),
            other => vec![other],
        };
        for command in commands {
            let argv: Vec<String> = match command {
                Value::String(script) => vec!["/bin/sh".into(), "-c".into(), script.clone()],
                Value::Array(parts) => parts.iter().filter_map(Value::as_str).map(str::to_string).collect(),
                _ => continue,
            };
            if argv.is_empty() {
                continue;
            }
            (options.progress)(&format!("Running {key}…"));
            let mut args = vec!["exec".to_string(), "-w".into(), workspace.to_string()];
            if let Some(user) = user {
                args.extend(["-u".to_string(), user.to_string()]);
            }
            args.push(id.to_string());
            args.extend(argv);
            run(&options.docker, &args.iter().map(String::as_str).collect::<Vec<_>>(), &format!("The dev container's {key} failed"))?;
        }
    }
    Ok(())
}

/// Runs docker; its output, or its error.
fn run(docker: &str, args: &[&str], doing: &str) -> Result<String, String> {
    let out = crate::process::command(docker).args(args).stdin(Stdio::null()).output().map_err(|e| format!("Couldn't run {docker}: {e}"))?;
    if !out.status.success() {
        return Err(tool_error(&out.stderr, doing));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What `${…}` stands for in a configuration.
struct Variables {
    local: String,
    container: String,
}

fn substitute(value: &mut Value, vars: &Variables) {
    match value {
        Value::String(s) => *s = substitute_text(s, vars),
        Value::Array(items) => items.iter_mut().for_each(|v| substitute(v, vars)),
        Value::Object(map) => map.values_mut().for_each(|v| substitute(v, vars)),
        _ => {}
    }
}

/// `${localWorkspaceFolder}`, `${localWorkspaceFolderBasename}`,
/// `${containerWorkspaceFolder}`, `${containerWorkspaceFolderBasename}`,
/// `${localEnv:NAME}` / `${localEnv:NAME:default}`; others stay.
fn substitute_text(text: &str, vars: &Variables) -> String {
    let basename = |p: &str| p.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or_default().to_string();
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &rest[start + 2..start + end];
        let replaced = match name {
            "localWorkspaceFolder" => Some(vars.local.clone()),
            "localWorkspaceFolderBasename" => Some(basename(&vars.local)),
            "containerWorkspaceFolder" => Some(vars.container.clone()),
            "containerWorkspaceFolderBasename" => Some(basename(&vars.container)),
            _ => name.strip_prefix("localEnv:").or_else(|| name.strip_prefix("env:")).map(|env| {
                let (key, default) = env.split_once(':').unwrap_or((env, ""));
                std::env::var(key).unwrap_or_else(|_| default.to_string())
            }),
        };
        match replaced {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[start..start + end + 1]),
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

/// A stable name for a folder's image.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, &b| (hash ^ b as u64).wrapping_mul(0x100000001b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_containers_own_ports_are_forwarded() {
        let value = serde_json::json!({ "forwardPorts": [3000, "8080", "db:5432", 0, 70000] });
        assert_eq!(forward_ports(&value), [3000, 8080]);
    }

    #[test]
    fn variables_are_filled_in() {
        let vars = Variables { local: "/Users/me/web".into(), container: "/workspaces/web".into() };
        assert_eq!(
            substitute_text("source=${localWorkspaceFolder},target=${containerWorkspaceFolder},${localWorkspaceFolderBasename}/${unknown}", &vars),
            "source=/Users/me/web,target=/workspaces/web,web/${unknown}"
        );
        assert_eq!(substitute_text("${localEnv:KUANA_SURELY_UNSET:fallback}", &vars), "fallback");
    }

    #[test]
    fn configurations_are_found_where_the_spec_puts_them() {
        let dir = std::env::temp_dir().join(format!("kuana-devcontainer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".devcontainer/python")).unwrap();
        assert_eq!(config_file(&dir), None);
        std::fs::write(dir.join(".devcontainer/python/devcontainer.json"), "{}").unwrap();
        assert_eq!(config_file(&dir), Some(dir.join(".devcontainer/python/devcontainer.json")));
        std::fs::write(dir.join(".devcontainer.json"), "{}").unwrap();
        assert_eq!(config_file(&dir), Some(dir.join(".devcontainer.json")));
        std::fs::write(dir.join(".devcontainer/devcontainer.json"), "{}").unwrap();
        assert_eq!(config_file(&dir), Some(dir.join(".devcontainer/devcontainer.json")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
