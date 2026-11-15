//! WSL and containers, with stand-ins for `wsl.exe` and `docker` that note
//! their arguments and run the `sh -c` script here with the "remote" user's
//! `HOME`: the same probe, install and `--stdio` as SSH, reached their way.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};

use kuana_core::{
    fs,
    remote::connection::{self, Options, Transport},
};

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kuana-transport-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// A stand-in: notes its arguments (one line each call), answers `start`
/// (a container), refuses `exec` until started when `stopped`, and runs
/// what follows `-c`.
fn stand_in(dir: &Path, home: &Path, stopped: bool) -> PathBuf {
    let script = dir.join("tool");
    let log = dir.join("calls.log");
    let started = dir.join("started");
    let guard = if stopped { format!("[ -e '{}' ] || {{ echo 'Error response from daemon: container web is not running' >&2; exit 1; }}\n", started.display()) } else { String::new() };
    common::write_script(
        &script,
        &format!(
            "#!/bin/sh\nprintf '%s|' \"$@\" >> '{log}'; echo >> '{log}'\nif [ \"$1\" = start ]; then touch '{started}'; exit 0; fi\n{guard}while [ \"$1\" != -c ]; do shift; done\nHOME='{home}' exec /bin/sh -c \"$2\"\n",
            log = log.display(),
            started = started.display(),
            home = home.display()
        ),
    );
    script
}

fn server(_: &str, _: &str) -> Result<Vec<u8>, String> {
    std::fs::read(env!("CARGO_BIN_EXE_kuana-server")).map_err(|e| e.to_string())
}

fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default().lines().map(str::to_string).collect()
}

#[test]
fn a_wsl_distribution_is_reached_through_wsl_exe() {
    let dir = temp("wsl");
    let home = dir.join("home");
    std::fs::create_dir_all(home.join("p")).unwrap();
    std::fs::write(home.join("p/a.txt"), "in wsl").unwrap();
    let options = Options { program: Some(stand_in(&dir, &home, false).display().to_string()), control_dir: dir.join("control"), askpass: Vec::new(), server_binary: &server, progress: &|_| {} };
    let transport = Transport::Wsl("Ubuntu".into());
    let connected = connection::connect(&transport, &options).unwrap();
    assert_eq!(connected.name, "wsl+Ubuntu");
    let file = connected.home_path().join("p/a.txt");
    assert_eq!(fs::read_to_string(&file).unwrap(), "in wsl");
    assert_eq!(connection::display(&connected.home_path().join("p")).as_deref(), Some("Ubuntu (WSL): ~/p"));
    // Every call names the distribution and runs `sh` there directly.
    let calls = calls(&dir);
    assert!(calls.len() >= 3, "{calls:?}");
    assert!(calls.iter().all(|c| c.starts_with("-d|Ubuntu|--exec|/bin/sh|-c|")), "{calls:?}");
    // No SSH control folder for it.
    assert!(!dir.join("control").exists());
    connection::disconnect(&connected.name);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stopped_container_is_started_and_entered_as_its_user() {
    let dir = temp("container");
    let home = dir.join("home");
    std::fs::create_dir_all(home.join("workspace")).unwrap();
    let steps = std::sync::Mutex::new(Vec::new());
    let progress = |s: &str| steps.lock().unwrap().push(s.to_string());
    let options = Options { program: Some(stand_in(&dir, &home, true).display().to_string()), control_dir: dir.join("control"), askpass: Vec::new(), server_binary: &server, progress: &progress };
    let transport = Transport::Container { name: "web".into(), user: Some("dev".into()) };
    let connected = connection::connect(&transport, &options).unwrap();
    assert_eq!(connected.name, "container+dev@web");
    assert!(fs::metadata(connected.home_path().join("workspace")).unwrap().is_dir());
    let calls = calls(&dir);
    assert!(calls.iter().any(|c| c == "start|web|"), "{calls:?}");
    assert!(calls.iter().filter(|c| c.starts_with("exec")).all(|c| c.starts_with("exec|-i|-u|dev|web|/bin/sh|-c|")), "{calls:?}");
    assert!(steps.lock().unwrap().iter().any(|s| s == "Starting web (container)…"));
    connection::disconnect(&connected.name);
    let _ = std::fs::remove_dir_all(&dir);
}
