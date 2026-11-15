//! The whole remote path, minus SSH: a `kuana-server` (`server::serve`) on a
//! thread, connected over pipes, mounted; then what Kuana does (`crate::fs`,
//! `crate::exec`, the watcher, the project scan, Find in Files) on a folder
//! the "remote" side has, through the mount.

use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
#[cfg(unix)]
use std::{sync::atomic::AtomicBool, time::Instant};

use super::{RemoteHost, client, server};
#[cfg(unix)]
use crate::{
    exec::{Command, Stdio},
    fs::{self, PathExt},
};

/// A server on a thread and a mounted host talking to it. The server's
/// trash is a folder of its own. Dropping the returned writer ends the connection.
fn connect(name: &str, trash: &Path) -> (PathBuf, Arc<RemoteHost>, std::thread::JoinHandle<()>) {
    let (server_in, client_out) = io::pipe().unwrap();
    let (client_in, server_out) = io::pipe().unwrap();
    let options = server::Options { trash: Some(trash.to_path_buf()) };
    let serving = std::thread::spawn(move || server::serve(server_in, server_out, options));
    let host = RemoteHost::start(client_in, client_out);
    let root = client::mount(name, host.clone());
    (root, host, serving)
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kuana-remote-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The server sees real paths (macOS's /var is /private/var).
    std::fs::canonicalize(&dir).unwrap()
}

/// `real` (on the "remote" machine) as Kuana sees it, under the mount. The
/// remote side is always Linux or macOS (no `kuana-server` is built for
/// Windows), so tests that serve this machine's folders are Unix-only: on
/// Windows `C:\…` pushed onto the mount replaces it, and the "remote" files
/// would quietly be local ones.
#[cfg(unix)]
fn mounted(root: &Path, real: &Path) -> PathBuf {
    fs::local_path(root, &real.to_string_lossy())
}

#[cfg(unix)]
#[test]
fn files_go_through_the_server() {
    let dir = temp("files");
    let trash = temp("files-trash");
    let (root, host, _serving) = connect("files-test", &trash);
    assert_eq!(host.info().unwrap().version, env!("CARGO_PKG_VERSION"));
    let project = mounted(&root, &dir);

    fs::write(project.join("a.txt"), "hello").unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "hello");
    assert_eq!(fs::read_to_string(project.join("a.txt")).unwrap(), "hello");
    // Bytes that aren't text come through whole.
    let binary: Vec<u8> = (0..=255).collect();
    fs::write(project.join("b.bin"), &binary).unwrap();
    assert_eq!(fs::read(project.join("b.bin")).unwrap(), binary);
    let mut read = String::new();
    fs::open(project.join("a.txt")).unwrap().read_to_string(&mut read).unwrap();
    assert_eq!(read, "hello");

    // Errors keep their kind.
    assert_eq!(fs::read(project.join("missing")).unwrap_err().kind(), io::ErrorKind::NotFound);
    assert_eq!(fs::create_new(project.join("a.txt")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);

    fs::create_dir_all(project.join("src/deep")).unwrap();
    fs::write(project.join("src/main.rs"), "fn main() {}").unwrap();
    assert!(project.join("src").is_dir_on_host() && project.join("src/main.rs").is_file_on_host());
    assert!(!project.join("nope").exists_on_host());
    let meta = fs::metadata(project.join("src/main.rs")).unwrap();
    assert_eq!(meta.len(), 12);
    assert!(meta.modified().unwrap() > std::time::SystemTime::UNIX_EPOCH);
    let mut names: Vec<(String, bool)> =
        fs::read_dir(&project).unwrap().map(|e| e.unwrap()).map(|e| (e.file_name().into_string().unwrap(), e.file_type().unwrap().is_dir())).collect();
    names.sort();
    assert_eq!(names, [("a.txt".into(), false), ("b.bin".into(), false), ("src".into(), true)]);
    let entry = fs::read_dir(project.join("src")).unwrap().map(|e| e.unwrap()).find(|e| e.file_name() == "main.rs").unwrap();
    assert_eq!(entry.path(), project.join("src/main.rs"));

    fs::rename(project.join("a.txt"), project.join("src/a.txt")).unwrap();
    fs::copy(project.join("src/a.txt"), project.join("copy.txt")).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("copy.txt")).unwrap(), "hello");
    // Copies cross between hosts.
    let here = std::env::temp_dir().join(format!("kuana-remote-here-{}.txt", std::process::id()));
    fs::copy(project.join("copy.txt"), &here).unwrap();
    assert_eq!(std::fs::read_to_string(&here).unwrap(), "hello");
    std::fs::remove_file(&here).unwrap();
    assert_eq!(fs::canonicalize(project.join("src/../copy.txt")).unwrap(), project.join("copy.txt"));

    // The trash keeps it, and renaming back restores it.
    let trashed = fs::trash_remote(&project.join("copy.txt")).unwrap().unwrap();
    assert!(!dir.join("copy.txt").exists());
    assert_eq!(trashed, mounted(&root, &trash.join("copy.txt")));
    fs::rename(&trashed, project.join("copy.txt")).unwrap();
    assert!(dir.join("copy.txt").is_file());

    fs::remove_file(project.join("copy.txt")).unwrap();
    fs::remove_dir_all(project.join("src")).unwrap();
    assert!(!dir.join("src").exists());
    client::unmount(&root);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn programs_run_there_with_their_output_and_exit_code() {
    let dir = temp("exec");
    let (root, _host, _serving) = connect("exec-test", &temp("exec-trash"));
    let project = mounted(&root, &dir);
    std::fs::write(dir.join("f.txt"), "x").unwrap();

    // The folder, and paths in arguments, are the host's.
    let out = Command::new("sh").args(["-c", "pwd; ls \"$1\"; echo oops >&2; exit 3", "sh"]).arg(project.join("f.txt")).current_dir(&project).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().collect::<Vec<_>>(), [dir.to_str().unwrap(), dir.join("f.txt").to_str().unwrap()]);
    assert_eq!(String::from_utf8(out.stderr).unwrap(), "oops\n");

    // Input, and much output (more than one chunk), in order.
    let mut child = Command::new("cat").current_dir(&project).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let text: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
    let mut stdin = child.stdin.take().unwrap();
    let input = text.clone();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), text);

    // Killing ends it.
    let mut sleeper = Command::new("sleep").arg("30").current_dir(&project).spawn().unwrap();
    let started = Instant::now();
    sleeper.kill().unwrap();
    let status = sleeper.wait().unwrap();
    assert!(!status.success() && started.elapsed() < Duration::from_secs(10));

    // A program that isn't there fails as it would here.
    assert_eq!(Command::new("no-such-program-kuana").current_dir(&project).output().unwrap_err().kind(), io::ErrorKind::NotFound);
    client::unmount(&root);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn watching_scanning_and_searching_happen_there() {
    let dir = temp("watch");
    let (root, _host, _serving) = connect("watch-test", &temp("watch-trash"));
    let project = mounted(&root, &dir);
    // A repository: `.gitignore` counts only in one.
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn needle() {}\n").unwrap();
    std::fs::write(dir.join("target/out.rs"), "needle\n").unwrap();

    // The scan runs there, with the project's ignore rules.
    let scanned: Vec<(PathBuf, bool)> = crate::project::scan_files(&project);
    assert!(scanned.contains(&(project.join("src/lib.rs"), false)), "{scanned:?}");
    assert!(!scanned.iter().any(|(p, _)| p.starts_with(project.join("target"))), "{scanned:?}");
    let snapshot = crate::project::ProjectSnapshot::scan(&project);
    assert!(snapshot.openable().contains(&project.join("src/lib.rs")));

    // Find in Files over the remote files.
    let files = crate::search::project_files(&project);
    assert_eq!(files, [project.join(".gitignore"), project.join("src/lib.rs")]);
    let query = crate::search::SearchQuery::new("needle", Default::default()).unwrap().unwrap();
    let mut found = Vec::new();
    crate::search::search_project(&crate::search::relative_to(&project), files, &query, &Default::default(), &AtomicBool::new(false), |m, _| found.extend(m));
    assert_eq!(found.iter().map(|m| m.path.clone()).collect::<Vec<_>>(), [project.join("src/lib.rs")]);

    // Changes made there come back with mount paths.
    let (watcher, mut changes) = crate::watcher::Watcher::new(&project).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(dir.join("src/new.rs"), "x").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while Instant::now() < deadline && !seen.iter().any(|c: &crate::watcher::FsChange| c.path.ends_with("new.rs")) {
        match changes.try_recv() {
            Ok(batch) => seen.extend(batch),
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let change = seen.iter().find(|c| c.path.ends_with("new.rs")).unwrap_or_else(|| panic!("no change came: {seen:?}"));
    assert!(change.path.starts_with(&project) && change.structural, "{change:?}");
    drop(watcher);
    client::unmount(&root);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_dropped_connection_fails_calls_instead_of_hanging() {
    let dir = temp("drop");
    let (server_in, client_out) = io::pipe().unwrap();
    let (client_in, server_out) = io::pipe().unwrap();
    let serving = std::thread::spawn(move || server::serve(server_in, server_out, Default::default()));
    let host = RemoteHost::start(client_in, client_out);
    let root = client::mount("drop-test", host.clone());
    let project = mounted(&root, &dir);
    fs::write(project.join("a"), "1").unwrap();
    // The connection ends: the server stops, calls fail at once.
    host.disconnect();
    serving.join().unwrap();
    assert_eq!(fs::read(project.join("a")).unwrap_err().kind(), io::ErrorKind::NotConnected);
    assert!(!project.join("a").exists_on_host());
    client::unmount(&root);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn terminals_run_there() {
    use std::sync::mpsc;

    let dir = temp("pty");
    let (root, host, _serving) = connect("pty-test", &temp("pty-trash"));
    let (out_tx, out) = mpsc::channel::<Vec<u8>>();
    let (exit_tx, exit) = mpsc::channel::<Option<i32>>();
    let out_tx = std::sync::Mutex::new(out_tx);
    let exit_tx = std::sync::Mutex::new(exit_tx);
    let sinks = super::client::PtySinks {
        output: Box::new(move |data| {
            let _ = out_tx.lock().unwrap().send(data);
        }),
        exit: Box::new(move |code| {
            let _ = exit_tx.lock().unwrap().send(code);
        }),
    };
    let shell = vec!["/bin/sh".to_string(), "-c".to_string(), "stty -echo; echo ready; read x; echo \"got $x in $(pwd)\"; exit 4".to_string()];
    let id = host.pty_open(&shell, Some(&dir.to_string_lossy()), &[("KUANA_TEST".into(), "1".into())], (80, 24), sinks).unwrap();
    let mut seen = String::new();
    let wait_for = |what: &str, seen: &mut String| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !seen.contains(what) {
            let left = deadline.saturating_duration_since(Instant::now());
            let chunk = out.recv_timeout(left).unwrap_or_else(|_| panic!("no {what:?} in {seen:?}"));
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
    };
    wait_for("ready", &mut seen);
    host.pty_resize(id, 120, 40);
    host.pty_input(id, b"abc\r");
    wait_for(&format!("got abc in {}", dir.display()), &mut seen);
    assert_eq!(exit.recv_timeout(Duration::from_secs(10)).unwrap(), Some(4));

    // A shell waiting for input ends when its terminal closes.
    let (exit_tx, exit) = mpsc::channel::<Option<i32>>();
    let exit_tx = std::sync::Mutex::new(exit_tx);
    let sinks = super::client::PtySinks {
        output: Box::new(|_| {}),
        exit: Box::new(move |code| {
            let _ = exit_tx.lock().unwrap().send(code);
        }),
    };
    let waiting = host.pty_open(&["/bin/sh".to_string(), "-c".to_string(), "read x".to_string()], None, &[], (80, 24), sinks).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(host.pty_foreground(waiting), None);
    host.pty_close(waiting);
    assert!(exit.recv_timeout(Duration::from_secs(10)).is_ok());
    client::unmount(&root);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ports_there_are_reached_from_here() {
    let (root, host, _serving) = connect("tcp-test", &temp("tcp-trash"));
    // A program there listens (an echo server, upper-casing), after a while.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0; 64];
        let n = stream.read(&mut buf).unwrap();
        stream.write_all(&buf[..n].to_ascii_uppercase()).unwrap();
    });
    let address = host.forward(port).unwrap();
    // Connected before it listens: the relay waits for it.
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream.write_all(b"hello there").unwrap();
    let mut answer = [0; 11];
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"HELLO THERE");
    client::unmount(&root);
}

/// A port forwarded on purpose (a web app there): reached on the local port
/// asked for, refused at once while nothing listens there, gone once dropped.
#[test]
fn forwarded_ports_stop_when_dropped() {
    let (root, host, _serving) = connect("tcp-forward-test", &temp("tcp-forward-trash"));
    let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (remote, local) = (free(), free());
    let forwarded = host.forward_on(local, remote).unwrap();
    assert_eq!(forwarded.address.port(), local);
    // Nothing there yet: closed without waiting.
    let started = std::time::Instant::now();
    let mut stream = std::net::TcpStream::connect(forwarded.address).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    assert_eq!(stream.read(&mut [0; 8]).unwrap_or(0), 0);
    assert!(started.elapsed() < Duration::from_secs(3));
    // Now a program listens there.
    let listener = std::net::TcpListener::bind(("127.0.0.1", remote)).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"pong").unwrap();
    });
    let mut stream = std::net::TcpStream::connect(forwarded.address).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut answer = [0; 4];
    stream.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"pong");
    drop(forwarded);
    std::thread::sleep(Duration::from_millis(100));
    assert!(std::net::TcpStream::connect(("127.0.0.1", local)).is_err(), "still listening");
    client::unmount(&root);
}
