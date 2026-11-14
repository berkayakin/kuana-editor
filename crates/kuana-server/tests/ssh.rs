//! Connecting over SSH, with a stand-in for `ssh` that runs the command here
//! with its own `HOME` (the "remote" user's): `kuana_core::remote::ssh`
//! probes, installs this build's `kuana-server` there (uploading it through
//! ssh's stdin), starts it and mounts it; then files and programs go through.

#![cfg(unix)]

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

use kuana_core::{
    exec::Command,
    fs::{self, PathExt},
    remote::ssh,
};

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kuana-ssh-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// A stand-in for `ssh`: options and the host skipped, the command run with
/// `HOME` the remote user's; it notes each command it ran.
fn fake_ssh(dir: &Path, home: &Path) -> PathBuf {
    let script = dir.join("ssh");
    let log = dir.join("commands.log");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in -o) shift 2;; -T) shift;; *) break;; esac; done\nshift\nprintf '%s\\n' \"$*\" >> '{}'\nHOME='{}' exec /bin/sh -c \"$*\"\n",
            log.display(),
            home.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[test]
fn connects_installs_starts_and_mounts() {
    let dir = temp("connect");
    let home = dir.join("home");
    std::fs::create_dir_all(home.join("project")).unwrap();
    std::fs::write(home.join("project/a.txt"), "remote text").unwrap();
    let program = fake_ssh(&dir, &home);
    let steps = Mutex::new(Vec::new());
    let asked = Mutex::new(Vec::new());
    let server = |os: &str, arch: &str| {
        asked.lock().unwrap().push((os.to_string(), arch.to_string()));
        std::fs::read(env!("CARGO_BIN_EXE_kuana-server")).map_err(|e| e.to_string())
    };
    let progress = |s: &str| steps.lock().unwrap().push(s.to_string());
    let options = ssh::Options { program: program.display().to_string(), control_dir: dir.join("control"), askpass: Vec::new(), server_binary: &server, progress: &progress };

    let connected = ssh::connect("box", &options).unwrap();
    assert_eq!(connected.home, home.display().to_string());
    // Installed for the host's system, into its home.
    let (os, arch) = asked.lock().unwrap()[0].clone();
    assert_eq!(os, if cfg!(target_os = "macos") { "Darwin" } else { "Linux" });
    assert!(!arch.is_empty());
    assert!(home.join(format!(".kuana-server/{}/kuana-server", env!("CARGO_PKG_VERSION"))).is_file());
    assert!(steps.lock().unwrap().iter().any(|s| s.contains("Installing")));

    // Its files and programs, through the mount.
    let project = connected.home_path().join("project");
    assert_eq!(fs::read_to_string(project.join("a.txt")).unwrap(), "remote text");
    fs::write(project.join("b.txt"), "from kuana").unwrap();
    assert_eq!(std::fs::read_to_string(home.join("project/b.txt")).unwrap(), "from kuana");
    let out = Command::new("sh").args(["-c", "ls"]).current_dir(&project).output().unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "a.txt\nb.txt\n");
    assert_eq!(ssh::display(&project).as_deref(), Some("box: ~/project"));
    // Connecting again uses the connection there is.
    assert!(ssh::connect("box", &options).is_ok());

    // Gone: unmounted, calls fail.
    ssh::disconnect("box");
    assert!(ssh::connected("box").is_none());
    assert!(!project.join("a.txt").exists_on_host());

    // The next time, the server is there already: no install.
    asked.lock().unwrap().clear();
    let again = ssh::connect("box", &options).unwrap();
    assert!(asked.lock().unwrap().is_empty());
    assert!(again.home_path().join("project/a.txt").is_file_on_host());
    ssh::disconnect("box");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_host_that_fails_says_why() {
    let dir = temp("fail");
    let script = dir.join("ssh");
    std::fs::write(&script, "#!/bin/sh\necho 'ssh: Could not resolve hostname nowhere: nodename nor servname provided' >&2\nexit 255\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let server = |_: &str, _: &str| Err("not needed".to_string());
    let options = ssh::Options { program: script.display().to_string(), control_dir: dir.join("control"), askpass: Vec::new(), server_binary: &server, progress: &|_| {} };
    let err = ssh::connect("nowhere", &options).err().unwrap();
    assert!(err.contains("Couldn't connect to nowhere") && err.contains("Could not resolve hostname"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Git asking for a password on the host: its askpass program is the server,
/// which relays the question to Kuana's asker and the answer back.
#[test]
fn questions_asked_there_are_answered_here() {
    let dir = temp("askpass");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let program = fake_ssh(&dir, &home);
    let server = |_: &str, _: &str| std::fs::read(env!("CARGO_BIN_EXE_kuana-server")).map_err(|e| e.to_string());
    let options = ssh::Options { program: program.display().to_string(), control_dir: dir.join("control"), askpass: Vec::new(), server_binary: &server, progress: &|_| {} };
    let connected = ssh::connect("askbox", &options).unwrap();
    let asked = std::sync::Arc::new(Mutex::new(Vec::new()));
    let noted = asked.clone();
    kuana_core::remote::client::set_asker(std::sync::Arc::new(move |prompt: &str, confirm: bool| {
        noted.lock().unwrap().push((prompt.to_string(), confirm));
        Some("s3cret".to_string())
    }));
    // As `git::asking` marks a command: Kuana's own askpass variables, which
    // the server swaps for its own.
    let out = Command::new("sh")
        .args(["-c", "\"$GIT_ASKPASS\" 'Password for https://me@example.com: '"])
        .current_dir(connected.home_path())
        .env("KUANA_ASKPASS", "1234:token")
        .env("GIT_ASKPASS", "/Applications/Kuana.app/Contents/MacOS/kuana")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "s3cret\n");
    assert_eq!(*asked.lock().unwrap(), [("Password for https://me@example.com: ".to_string(), false)]);
    ssh::disconnect("askbox");
    let _ = std::fs::remove_dir_all(&dir);
}
