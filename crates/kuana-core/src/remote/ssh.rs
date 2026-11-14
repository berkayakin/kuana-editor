//! SSH (`connection::Transport::Ssh`): the system's `ssh`, so
//! `~/.ssh/config`, agents, keys and jump hosts work as in a terminal, with
//! one control master per host for the commands that follow. Questions SSH
//! asks (a password, a passphrase, a new host key) go through the askpass
//! environment the caller passes.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use super::connection::{Options, Transport};

/// `ssh` with the shared control master and the askpass setup, running `script` on `host`.
pub(super) fn command(options: &Options, host: &str, script: &str) -> std::process::Command {
    let mut command = crate::process::command(options.program.clone().unwrap_or_else(|| Transport::Ssh(String::new()).default_program()));
    let control = options.control_dir.join("%C");
    command.args(["-o", "ControlMaster=auto", "-o"]).arg(format!("ControlPath={}", control.display()));
    command.args(["-o", "ControlPersist=10m", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=4", "-T"]);
    if options.askpass.is_empty() {
        command.args(["-o", "BatchMode=yes"]);
    }
    command.envs(options.askpass.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    command.arg(host).arg(script).stdin(Stdio::null());
    command
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
}
