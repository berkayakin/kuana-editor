//! Connecting to a machine `kuana-server` runs on (`plans/remote-dev.md`):
//! over SSH, into a WSL distribution or into a container. A `Transport` only
//! says how a shell command reaches the other side (`ssh host …`, `wsl.exe -d
//! distro --exec sh -c …`, `docker exec -i container sh -c …`); the rest is
//! the same for all: what system it is and whether this version's server is
//! there (`probe`), copying the server in through the command's stdin
//! (`install`), then `kuana-server --stdio`, whose streams become a
//! `RemoteHost` mounted under the transport's name.

use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{Arc, Mutex},
};

use super::{RemoteHost, client};

/// Where servers are installed there, under the home: one folder per version.
pub const SERVER_DIR: &str = ".kuana-server";
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Before every command: a container's user may have no home.
const HOME: &str = "export HOME=\"${HOME:-/tmp}\"; ";

/// How Kuana reaches a machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    /// A host `ssh` knows (a `~/.ssh/config` entry or `user@host`).
    Ssh(String),
    /// A WSL distribution (Windows).
    Wsl(String),
    /// A running container, by name or id, as `user` or its default one.
    Container { name: String, user: Option<String> },
}

impl Transport {
    /// From a mount name (`name()`): `wsl+Ubuntu`, `container+[user@]web`,
    /// anything else an SSH host.
    pub fn from_name(name: &str) -> Transport {
        if let Some(distro) = name.strip_prefix("wsl+") {
            Transport::Wsl(distro.to_string())
        } else if let Some(container) = name.strip_prefix("container+") {
            match container.split_once('@') {
                Some((user, name)) => Transport::Container { name: name.to_string(), user: Some(user.to_string()) },
                None => Transport::Container { name: container.to_string(), user: None },
            }
        } else {
            Transport::Ssh(name.to_string())
        }
    }

    /// What it's mounted as: one path component, the same for the same machine.
    pub fn name(&self) -> String {
        match self {
            Transport::Ssh(host) => host.clone(),
            Transport::Wsl(distro) => format!("wsl+{distro}"),
            Transport::Container { name, user: Some(user) } => format!("container+{user}@{name}"),
            Transport::Container { name, user: None } => format!("container+{name}"),
        }
    }

    /// As people read it: `box`, `Ubuntu (WSL)`, `web (container)`.
    pub fn label(&self) -> String {
        match self {
            Transport::Ssh(host) => host.clone(),
            Transport::Wsl(distro) => format!("{distro} (WSL)"),
            Transport::Container { name, .. } => format!("{name} (container)"),
        }
    }

    /// The program it runs: `KUANA_SSH`, `KUANA_WSL`, `KUANA_DOCKER` name another.
    pub fn default_program(&self) -> String {
        let (variable, program) = match self {
            Transport::Ssh(_) => ("KUANA_SSH", "ssh"),
            Transport::Wsl(_) => ("KUANA_WSL", "wsl.exe"),
            Transport::Container { .. } => ("KUANA_DOCKER", "docker"),
        };
        std::env::var(variable).ok().filter(|p| !p.is_empty()).unwrap_or_else(|| program.to_string())
    }
}

/// What `connect` needs from its caller.
pub struct Options<'a> {
    /// The transport's program (`ssh`, `wsl.exe`, `docker`); none: its default.
    pub program: Option<String>,
    /// Where SSH's control sockets go (a short path: sockets have a length limit).
    pub control_dir: PathBuf,
    /// Askpass variables, so SSH asks its questions in Kuana; none: SSH
    /// mustn't ask (`BatchMode`).
    pub askpass: Vec<(String, String)>,
    /// The server binary for a system (`uname -s`, `uname -m`).
    pub server_binary: &'a dyn Fn(&str, &str) -> Result<Vec<u8>, String>,
    /// What's happening, for the status bar.
    pub progress: &'a dyn Fn(&str),
}

impl Options<'_> {
    fn program(&self, transport: &Transport) -> String {
        self.program.clone().unwrap_or_else(|| transport.default_program())
    }

    /// A command running `script` with `sh` there; nothing reaches a
    /// terminal (SSH asks through askpass or fails).
    fn command(&self, transport: &Transport, script: &str) -> std::process::Command {
        let script = format!("{HOME}{script}");
        let mut command = match transport {
            Transport::Ssh(host) => super::ssh::command(self, host, &script),
            Transport::Wsl(distro) => {
                let mut command = crate::process::command(self.program(transport));
                command.args(["-d", distro, "--exec", "/bin/sh", "-c", &script]);
                command
            }
            Transport::Container { name, user } => {
                let mut command = crate::process::command(self.program(transport));
                command.args(["exec", "-i"]);
                if let Some(user) = user {
                    command.args(["-u", user]);
                }
                command.args([name, "/bin/sh", "-c", &script]);
                command
            }
        };
        command.stdin(Stdio::null());
        command
    }
}

/// A connected machine.
#[derive(Clone)]
pub struct Connected {
    /// The mount name (`Transport::name`).
    pub name: String,
    pub transport: Transport,
    /// Where its files are in Kuana (`fs::mount_root(name)`).
    pub root: PathBuf,
    /// Its user's home folder (the machine's path).
    pub home: String,
    pub host: Arc<RemoteHost>,
}

impl Connected {
    /// The home folder under the mount.
    pub fn home_path(&self) -> PathBuf {
        crate::fs::local_path(&self.root, &self.home)
    }
}

struct Live {
    connected: Connected,
    /// The transport's process running the server: ending it ends the connection.
    child: Child,
}

static LIVE: Mutex<Option<HashMap<String, Live>>> = Mutex::new(None);

/// The machine connected as `name` (a mount name), if it still is.
pub fn connected(name: &str) -> Option<Connected> {
    let live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    live.as_ref()?.get(name).filter(|l| l.connected.host.is_connected()).map(|l| l.connected.clone())
}

/// Every machine connected now.
pub fn all_connected() -> Vec<Connected> {
    let live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    live.iter().flat_map(|m| m.values()).filter(|l| l.connected.host.is_connected()).map(|l| l.connected.clone()).collect()
}

/// Ends the connection to `name` (its server stops) and unmounts it.
pub fn disconnect(name: &str) {
    let gone = LIVE.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|m| m.remove(name));
    if let Some(mut live) = gone {
        live.connected.host.disconnect();
        let _ = live.child.kill();
        let _ = live.child.wait();
        client::unmount(&live.connected.root);
    }
}

/// A tool's own error: the last line it printed.
pub(crate) fn tool_error(stderr: &[u8], doing: &str) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("it failed without saying why");
    format!("{doing}: {line}")
}

/// What the machine is and whether this version's server is there.
struct Probe {
    os: String,
    arch: String,
    home: String,
    installed: bool,
}

fn probe(options: &Options, transport: &Transport) -> Result<Probe, String> {
    let label = transport.label();
    let script = format!("uname -s; uname -m; printf '%s\\n' \"$HOME\"; test -x \"$HOME/{SERVER_DIR}/{VERSION}/kuana-server\" && echo installed || echo missing");
    let program = options.program(transport);
    let mut out = options.command(transport, &script).stdout(Stdio::piped()).stderr(Stdio::piped()).output().map_err(|e| format!("Couldn't run {program}: {e}"))?;
    // A stopped container is started (a dev container after a restart).
    if !out.status.success()
        && let Transport::Container { name, .. } = transport
        && String::from_utf8_lossy(&out.stderr).contains("is not running")
    {
        (options.progress)(&format!("Starting {label}…"));
        let started = crate::process::command(&program).args(["start", name]).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
        if !started.status.success() {
            return Err(tool_error(&started.stderr, &format!("Couldn't start {label}")));
        }
        out = options.command(transport, &script).stdout(Stdio::piped()).stderr(Stdio::piped()).output().map_err(|e| e.to_string())?;
    }
    if !out.status.success() {
        return Err(tool_error(&out.stderr, &format!("Couldn't connect to {label}")));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    // Login banners and the like come first: the last four lines are ours.
    let [os, arch, home, state] = lines.get(lines.len().saturating_sub(4)..).and_then(|l| <[&str; 4]>::try_from(l).ok()).ok_or_else(|| format!("{label} answered something unexpected: {text}"))?;
    Ok(Probe { os: os.to_string(), arch: arch.to_string(), home: home.to_string(), installed: state == "installed" })
}

/// Copies the server into place (through a temporary name, so a
/// half-copied one never runs).
fn install(options: &Options, transport: &Transport, probe: &Probe) -> Result<(), String> {
    let label = transport.label();
    (options.progress)(&format!("Installing kuana-server on {label}…"));
    let binary = (options.server_binary)(&probe.os, &probe.arch)?;
    let dir = format!("\"$HOME/{SERVER_DIR}/{VERSION}\"");
    let script = format!("mkdir -p {dir} && cat > {dir}/kuana-server.part && chmod 755 {dir}/kuana-server.part && mv -f {dir}/kuana-server.part {dir}/kuana-server");
    let mut child = options.command(transport, &script).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let writer = std::thread::spawn(move || stdin.write_all(&binary));
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();
    if !out.status.success() {
        return Err(tool_error(&out.stderr, &format!("Couldn't install kuana-server on {label}")));
    }
    Ok(())
}

/// Connects, installing the server if needed, and mounts the machine.
/// Blocking: run it in the background.
pub fn connect(transport: &Transport, options: &Options) -> Result<Connected, String> {
    let name = transport.name();
    let label = transport.label();
    if let Some(connected) = connected(&name) {
        return Ok(connected);
    }
    if let Transport::Ssh(_) = transport {
        std::fs::create_dir_all(&options.control_dir).map_err(|e| e.to_string())?;
    }
    (options.progress)(&format!("Connecting to {label}…"));
    let probe = probe(options, transport)?;
    if !probe.installed {
        install(options, transport, &probe)?;
    }
    (options.progress)(&format!("Starting kuana-server on {label}…"));
    let command = format!("exec \"$HOME/{SERVER_DIR}/{VERSION}/kuana-server\" --stdio");
    let mut child = options
        .command(transport, &command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Couldn't run {}: {e}", options.program(transport)))?;
    let (stdin, stdout, mut stderr) = (child.stdin.take().ok_or("no stdin")?, child.stdout.take().ok_or("no stdout")?, child.stderr.take().ok_or("no stderr")?);
    // What the server or the transport says on stderr, kept for the error if it fails.
    let said = Arc::new(Mutex::new(Vec::new()));
    let keep = said.clone();
    std::thread::spawn(move || {
        let mut buf = [0; 4096];
        while let Ok(n) = stderr.read(&mut buf) {
            if n == 0 {
                break;
            }
            let mut kept = keep.lock().unwrap_or_else(|e| e.into_inner());
            kept.extend_from_slice(&buf[..n]);
            let excess = kept.len().saturating_sub(16 * 1024);
            kept.drain(..excess);
        }
    });
    let host = RemoteHost::start(stdout, stdin);
    let info = host.info().map_err(|e| {
        let _ = child.kill();
        let said = said.lock().unwrap_or_else(|e| e.into_inner());
        if said.is_empty() { format!("kuana-server on {label} didn't start: {e}") } else { tool_error(&said, &format!("kuana-server on {label} didn't start")) }
    })?;
    if info.version != VERSION {
        let _ = child.kill();
        return Err(format!("{label} runs kuana-server {} instead of {VERSION}", info.version));
    }
    let root = client::mount(&name, host.clone());
    let connected = Connected { name: name.clone(), transport: transport.clone(), root, home: if info.home.is_empty() { probe.home } else { info.home }, host };
    LIVE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_default().insert(name, Live { connected: connected.clone(), child });
    Ok(connected)
}

/// The name a path's machine is mounted as, if it's under a mount (connected or not).
pub fn host_of(path: &Path) -> Option<String> {
    let text = path.to_string_lossy();
    let rest = text.strip_prefix(if cfg!(windows) { r"\\kuana-remote\" } else { "/kuana-remote/" })?;
    rest.split(['/', '\\']).next().filter(|n| !n.is_empty()).map(str::to_string)
}

/// `user@box: ~/project`, `Ubuntu (WSL): ~/project` for a mount path (the
/// home folder shortened when known).
pub fn display(path: &Path) -> Option<String> {
    let name = host_of(path)?;
    let root = crate::fs::mount_root(&name);
    let host_path = crate::fs::local_to_host(path.strip_prefix(&root).ok()?);
    let shown = match connected(&name) {
        Some(c) if host_path == c.home => "~".to_string(),
        Some(c) if host_path.starts_with(&format!("{}/", c.home)) => format!("~{}", &host_path[c.home.len()..]),
        _ => host_path,
    };
    Some(format!("{}: {shown}", Transport::from_name(&name).label()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_say_how_to_reach_a_machine() {
        for transport in [
            Transport::Ssh("me@box".into()),
            Transport::Wsl("Ubuntu-24.04".into()),
            Transport::Container { name: "web".into(), user: None },
            Transport::Container { name: "3f2a9c".into(), user: Some("vscode".into()) },
        ] {
            assert_eq!(Transport::from_name(&transport.name()), transport);
        }
        assert_eq!(Transport::Wsl("Ubuntu".into()).label(), "Ubuntu (WSL)");
        let root = crate::fs::mount_root("wsl+Ubuntu");
        assert_eq!(display(&root.join("home").join("me")).as_deref(), Some("Ubuntu (WSL): /home/me"));
    }

    #[test]
    fn mount_paths_name_their_host() {
        let root = crate::fs::mount_root("devbox");
        let path = root.join("home").join("me").join("p");
        assert_eq!(host_of(&path).as_deref(), Some("devbox"));
        assert_eq!(display(&path).as_deref(), Some("devbox: /home/me/p"));
        assert_eq!(host_of(Path::new("/Users/me/p")), None);
    }
}
