//! Connecting to a host over SSH (`plans/remote-dev.md`, phase 3): the
//! system's `ssh` (so `~/.ssh/config`, agents, keys and jump hosts work as in
//! a terminal), one control master per host for the commands that follow,
//! then `kuana-server` installed if this version isn't there yet and started
//! with `--stdio`, its streams becoming a `RemoteHost` mounted under the
//! host's name. Questions SSH asks (a password, a passphrase, a new host
//! key) go through the askpass environment the caller passes.

use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{Arc, Mutex},
};

use super::{RemoteHost, client};

/// Where servers are installed on a host, under its home: one folder per version.
pub const SERVER_DIR: &str = ".kuana-server";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What `connect` needs from its caller.
pub struct Options<'a> {
    /// `ssh`, or a stand-in (tests).
    pub program: String,
    /// Where control sockets go (a short path: sockets have a length limit).
    pub control_dir: PathBuf,
    /// Askpass variables, so SSH asks its questions in Kuana; none: SSH
    /// mustn't ask (`BatchMode`).
    pub askpass: Vec<(String, String)>,
    /// The server binary for a host's system (`uname -s`, `uname -m`).
    pub server_binary: &'a dyn Fn(&str, &str) -> Result<Vec<u8>, String>,
    /// What's happening, for the status bar.
    pub progress: &'a dyn Fn(&str),
}

/// A connected host.
#[derive(Clone)]
pub struct Connected {
    pub name: String,
    /// Where its files are in Kuana (`fs::mount_root(name)`).
    pub root: PathBuf,
    /// Its user's home folder (the host's path).
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
    /// The `ssh` running the server: ending it ends the connection.
    ssh: Child,
}

static LIVE: Mutex<Option<HashMap<String, Live>>> = Mutex::new(None);

/// The host connected as `name`, if it still is.
pub fn connected(name: &str) -> Option<Connected> {
    let live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    live.as_ref()?.get(name).filter(|l| l.connected.host.is_connected()).map(|l| l.connected.clone())
}

/// Every host connected now.
pub fn all_connected() -> Vec<Connected> {
    let live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    live.iter().flat_map(|m| m.values()).filter(|l| l.connected.host.is_connected()).map(|l| l.connected.clone()).collect()
}

/// Ends the connection to `name` (its server stops) and unmounts it.
pub fn disconnect(name: &str) {
    let gone = LIVE.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|m| m.remove(name));
    if let Some(mut live) = gone {
        live.connected.host.disconnect();
        let _ = live.ssh.kill();
        let _ = live.ssh.wait();
        client::unmount(&live.connected.root);
    }
}

/// `ssh` with the shared control master and the askpass setup.
fn ssh(options: &Options, name: &str, remote_command: &str) -> std::process::Command {
    let mut command = crate::process::command(&options.program);
    let control = options.control_dir.join("%C");
    command.args(["-o", "ControlMaster=auto", "-o"]).arg(format!("ControlPath={}", control.display()));
    command.args(["-o", "ControlPersist=10m", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=4", "-T"]);
    if options.askpass.is_empty() {
        command.args(["-o", "BatchMode=yes"]);
    }
    command.envs(options.askpass.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    // Nothing reaches a terminal: SSH asks through askpass or fails.
    command.arg(name).arg(remote_command).stdin(Stdio::null());
    command
}

/// SSH's own error, the last line of what it said.
fn ssh_error(stderr: &[u8], doing: &str) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("ssh failed");
    format!("{doing}: {line}")
}

/// What the host is and whether this version's server is there.
struct Probe {
    os: String,
    arch: String,
    home: String,
    installed: bool,
}

fn probe(options: &Options, name: &str) -> Result<Probe, String> {
    let script = format!("uname -s; uname -m; printf '%s\\n' \"$HOME\"; test -x \"$HOME/{SERVER_DIR}/{VERSION}/kuana-server\" && echo installed || echo missing");
    let out = ssh(options, name, &script).stdout(Stdio::piped()).stderr(Stdio::piped()).output().map_err(|e| format!("Couldn't run {}: {e}", options.program))?;
    if !out.status.success() {
        return Err(ssh_error(&out.stderr, &format!("Couldn't connect to {name}")));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    // Login banners and the like come first: the last four lines are ours.
    let [os, arch, home, state] = lines.get(lines.len().saturating_sub(4)..).and_then(|l| <[&str; 4]>::try_from(l).ok()).ok_or_else(|| format!("{name} answered something unexpected: {text}"))?;
    Ok(Probe { os: os.to_string(), arch: arch.to_string(), home: home.to_string(), installed: state == "installed" })
}

/// Copies the server into place on the host (through a temporary name, so
/// a half-copied one never runs).
fn install(options: &Options, name: &str, probe: &Probe) -> Result<(), String> {
    (options.progress)(&format!("Installing kuana-server on {name}…"));
    let binary = (options.server_binary)(&probe.os, &probe.arch)?;
    let dir = format!("\"$HOME/{SERVER_DIR}/{VERSION}\"");
    let script = format!("mkdir -p {dir} && cat > {dir}/kuana-server.part && chmod 755 {dir}/kuana-server.part && mv -f {dir}/kuana-server.part {dir}/kuana-server");
    let mut child = ssh(options, name, &script).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let writer = std::thread::spawn(move || stdin.write_all(&binary));
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();
    if !out.status.success() {
        return Err(ssh_error(&out.stderr, &format!("Couldn't install kuana-server on {name}")));
    }
    Ok(())
}

/// Connects to `name` (a `~/.ssh/config` host or `user@host`), installing
/// the server if needed, and mounts it. Blocking: run it in the background.
pub fn connect(name: &str, options: &Options) -> Result<Connected, String> {
    if let Some(connected) = connected(name) {
        return Ok(connected);
    }
    std::fs::create_dir_all(&options.control_dir).map_err(|e| e.to_string())?;
    (options.progress)(&format!("Connecting to {name}…"));
    let probe = probe(options, name)?;
    if !probe.installed {
        install(options, name, &probe)?;
    }
    (options.progress)(&format!("Starting kuana-server on {name}…"));
    let command = format!("\"$HOME/{SERVER_DIR}/{VERSION}/kuana-server\" --stdio");
    let mut child = ssh(options, name, &command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Couldn't run {}: {e}", options.program))?;
    let (stdin, stdout, mut stderr) = (child.stdin.take().ok_or("no stdin")?, child.stdout.take().ok_or("no stdout")?, child.stderr.take().ok_or("no stderr")?);
    // What the server or SSH says on stderr, kept for the error if it fails.
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
        if said.is_empty() { format!("kuana-server on {name} didn't start: {e}") } else { ssh_error(&said, &format!("kuana-server on {name} didn't start")) }
    })?;
    if info.version != VERSION {
        let _ = child.kill();
        return Err(format!("{name} runs kuana-server {} instead of {VERSION}", info.version));
    }
    let root = client::mount(name, host.clone());
    let connected = Connected { name: name.to_string(), root, home: if info.home.is_empty() { probe.home } else { info.home }, host };
    LIVE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_default().insert(name.to_string(), Live { connected: connected.clone(), ssh: child });
    Ok(connected)
}

/// The name a path's host is mounted as, if it's under a mount (connected or not).
pub fn host_of(path: &Path) -> Option<String> {
    let text = path.to_string_lossy();
    let rest = text.strip_prefix(if cfg!(windows) { r"\\kuana-remote\" } else { "/kuana-remote/" })?;
    rest.split(['/', '\\']).next().filter(|n| !n.is_empty()).map(str::to_string)
}

/// `user@box: ~/project` for a mount path (the home folder shortened when known).
pub fn display(path: &Path) -> Option<String> {
    let name = host_of(path)?;
    let root = crate::fs::mount_root(&name);
    let host_path = crate::fs::local_to_host(path.strip_prefix(&root).ok()?);
    let shown = match connected(&name) {
        Some(c) if host_path == c.home => "~".to_string(),
        Some(c) if host_path.starts_with(&format!("{}/", c.home)) => format!("~{}", &host_path[c.home.len()..]),
        _ => host_path,
    };
    Some(format!("{name}: {shown}"))
}

/// The hosts `~/.ssh/config` names (and the files it includes), without
/// patterns, in order.
pub fn config_hosts(home: &Path) -> Vec<String> {
    let mut hosts = Vec::new();
    let mut seen = std::collections::HashSet::new();
    read_config(&home.join(".ssh/config"), home, &mut hosts, &mut seen, 0);
    hosts
}

fn read_config(file: &Path, home: &Path, hosts: &mut Vec<String>, seen: &mut std::collections::HashSet<PathBuf>, depth: usize) {
    if depth > 8 || !seen.insert(file.to_path_buf()) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(file) else { return };
    for line in text.lines() {
        let line = line.trim();
        let (key, value) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => (k.to_ascii_lowercase(), v.trim().trim_start_matches('=').trim()),
            None => continue,
        };
        match key.as_str() {
            "host" => {
                for name in value.split_whitespace() {
                    if !name.contains(['*', '?', '!']) && !hosts.iter().any(|h| h == name) {
                        hosts.push(name.to_string());
                    }
                }
            }
            "include" => {
                for pattern in value.split_whitespace() {
                    let pattern = match pattern.strip_prefix("~/") {
                        Some(rest) => home.join(rest),
                        None if Path::new(pattern).is_absolute() => PathBuf::from(pattern),
                        None => home.join(".ssh").join(pattern),
                    };
                    for file in expand(&pattern) {
                        read_config(&file, home, hosts, seen, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// An `Include`'s files: a `*` in its last part matches names in that folder.
fn expand(pattern: &Path) -> Vec<PathBuf> {
    let name = pattern.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.contains('*') {
        return vec![pattern.to_path_buf()];
    }
    let (prefix, suffix) = name.split_once('*').unwrap_or((&name, ""));
    let Some(dir) = pattern.parent() else { return Vec::new() };
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().map(|n| n.to_string_lossy()).is_some_and(|n| n.starts_with(prefix) && n.ends_with(suffix)))
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_come_from_the_config_and_its_includes() {
        let home = std::env::temp_dir().join(format!("kuana-ssh-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".ssh/conf.d")).unwrap();
        std::fs::create_dir_all(home.join(".orbstack/ssh")).unwrap();
        std::fs::write(
            home.join(".ssh/config"),
            "# mine\nHost devbox gpu-box\n  HostName 10.0.0.2\nHost *.internal !bad\n  User me\nInclude ~/.orbstack/ssh/config\nInclude conf.d/*.conf\nHost=eq-host\n",
        )
        .unwrap();
        std::fs::write(home.join(".orbstack/ssh/config"), "Host orb\n  Port 32222\n").unwrap();
        std::fs::write(home.join(".ssh/conf.d/work.conf"), "Host work\nHost devbox\n").unwrap();
        assert_eq!(config_hosts(&home), ["devbox", "gpu-box", "orb", "work", "eq-host"]);
        let _ = std::fs::remove_dir_all(&home);
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
