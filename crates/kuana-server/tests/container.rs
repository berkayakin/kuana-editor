//! Against real Docker and a real Linux container: attaching to a running
//! one, and bringing up a dev container from its configuration. Ignored by
//! default; it needs Docker and a Linux build of `kuana-server` for the
//! containers' architecture:
//!
//! ```sh
//! KUANA_LINUX_SERVER=<path to kuana-server> cargo test -p kuana-server --test container -- --ignored
//! ```
//!
//! `KUANA_E2E_IMAGE` picks the image (`ubuntu:26.04`; it needs `sh`, `perl`
//! for the port test, and the server's libc).

#![cfg(unix)]

use std::{
    io::Read,
    path::PathBuf,
    sync::{Mutex, mpsc},
    time::{Duration, Instant},
};

use kuana_core::{
    exec::Command,
    fs::{self, PathExt},
    remote::{
        client::PtySinks,
        connection::{self, Options, Transport},
        container::{self, UpOptions},
    },
};

fn image() -> String {
    std::env::var("KUANA_E2E_IMAGE").unwrap_or_else(|_| "ubuntu:26.04".into())
}

fn docker() -> String {
    std::env::var("KUANA_DOCKER").unwrap_or_else(|_| "docker".into())
}

fn server(os: &str, _: &str) -> Result<Vec<u8>, String> {
    assert_eq!(os, "Linux", "asked for a server for {os}");
    let path = std::env::var("KUANA_LINUX_SERVER").map_err(|_| "KUANA_LINUX_SERVER isn't set".to_string())?;
    std::fs::read(path).map_err(|e| e.to_string())
}

fn options(progress: &dyn Fn(&str)) -> Options<'_> {
    Options { program: Some(docker()), control_dir: std::env::temp_dir(), askpass: Vec::new(), server_binary: &server, progress }
}

fn run(args: &[&str]) -> String {
    let out = std::process::Command::new(docker()).args(args).output().unwrap();
    assert!(out.status.success(), "docker {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Removes the container however the test ends.
struct Removed(String);

impl Drop for Removed {
    fn drop(&mut self) {
        let _ = std::process::Command::new(docker()).args(["rm", "-f", &self.0]).output();
    }
}

#[test]
#[ignore]
fn a_running_container_is_worked_in() {
    let name = format!("kuana-e2e-{}", std::process::id());
    run(&["run", "-d", "--name", &name, &image(), "sleep", "infinity"]);
    let _removed = Removed(name.clone());
    let steps = Mutex::new(Vec::new());
    let progress = |s: &str| steps.lock().unwrap().push(s.to_string());
    let transport = Transport::Container { name: name.clone(), user: None };
    let connected = connection::connect(&transport, &options(&progress)).unwrap();
    assert!(steps.lock().unwrap().iter().any(|s| s.starts_with("Installing")), "{:?}", steps.lock().unwrap());
    let home = connected.home_path();

    // Files.
    let project = home.join("proj");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    fs::write(project.join(".gitignore"), "build/\n").unwrap();
    fs::create_dir_all(project.join("build")).unwrap();
    fs::write(project.join("build/out.o"), "x").unwrap();
    assert_eq!(fs::read_to_string(project.join("src/main.c")).unwrap(), "int main(void) { return 0; }\n");
    assert_eq!(run(&["exec", &name, "cat", "/root/proj/src/main.c"]), "int main(void) { return 0; }");
    let mut names: Vec<String> = fs::read_dir(&project).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, [".gitignore", "build", "src"]);

    // Programs, with the login shell's environment.
    let out = Command::new("sh").args(["-c", "uname -s; echo \"$HOME\"; pwd; command -v sh"]).current_dir(&project).output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[..3], ["Linux", "/root", "/root/proj"], "{text}");
    assert!(lines[3].ends_with("/sh"), "{text}");
    assert!(matches!(kuana_core::exec::which_on(&project, "sh"), Some(Some(_))));
    assert_eq!(kuana_core::exec::which_on(&project, "surely-not-a-program"), Some(None));

    // A terminal: the shell there, its output, its exit code.
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    let (exit_tx, exit_rx) = mpsc::channel::<Option<i32>>();
    let (out_tx, exit_tx) = (Mutex::new(out_tx), Mutex::new(exit_tx));
    let sinks = PtySinks {
        output: Box::new(move |data| {
            let _ = out_tx.lock().unwrap().send(data);
        }),
        exit: Box::new(move |code| {
            let _ = exit_tx.lock().unwrap().send(code);
        }),
    };
    let id = connected.host.pty_open(&["/bin/sh".into()], Some("/root/proj"), &[("TERM".into(), "xterm-256color".into())], (80, 24), sinks).unwrap();
    connected.host.pty_input(id, b"echo \"sum=$((40+2)) in $(pwd)\"; exit 3\r");
    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !seen.contains("sum=42 in /root/proj") {
        let chunk = out_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap_or_else(|_| panic!("terminal said {seen:?}"));
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert_eq!(exit_rx.recv_timeout(Duration::from_secs(10)).unwrap(), Some(3));

    // A port there, reached from here.
    let port = 41234;
    let mut listener = Command::new("perl")
        .args([
            "-MIO::Socket::INET",
            "-e",
            &format!("$s = IO::Socket::INET->new(LocalAddr => '127.0.0.1', LocalPort => {port}, Listen => 1, ReuseAddr => 1) or die; print qq(listening\\n); $| = 1; $c = $s->accept; print $c qq(hello from linux\\n); close $c;"),
        ])
        .current_dir(&project)
        .stdout(kuana_core::exec::Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = [0u8; 10];
    listener.stdout.as_mut().unwrap().read_exact(&mut ready).unwrap();
    let forwarded = connected.host.forward_on(0, port).unwrap();
    let mut stream = std::net::TcpStream::connect(forwarded.address).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    assert_eq!(answer, "hello from linux\n");
    let _ = listener.wait();

    // Git and SSH questions there come back here (the server is the askpass program).
    let asked = std::sync::Arc::new(Mutex::new(Vec::new()));
    let noted = asked.clone();
    kuana_core::remote::client::set_asker(std::sync::Arc::new(move |prompt: &str, _| {
        noted.lock().unwrap().push(prompt.to_string());
        Some("pa55".to_string())
    }));
    let out = Command::new("sh")
        .args(["-c", "\"$GIT_ASKPASS\" 'Password: '"])
        .current_dir(&project)
        .env("KUANA_ASKPASS", "1:token")
        .env("GIT_ASKPASS", "/Applications/Kuana.app/Contents/MacOS/kuana")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "pa55\n", "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(*asked.lock().unwrap(), ["Password: "]);

    // The project's files, scanned there.
    let scanned = connected.host.scan("/root/proj").unwrap();
    assert!(scanned.iter().any(|(p, ignored)| p.ends_with("src/main.c") && !ignored), "{scanned:?}");

    // Deleting goes to its trash, which a rename undoes.
    let trashed = fs::trash_remote(&project.join("src/main.c")).unwrap().unwrap();
    assert!(!project.join("src/main.c").exists_on_host());
    fs::rename(&trashed, project.join("src/main.c")).unwrap();
    assert!(project.join("src/main.c").exists_on_host());

    connection::disconnect(&connected.name);
    // The connection went: so did the server and what it ran.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(run(&["exec", &name, "sh", "-c", "pgrep -c kuana-server || true"]), "0");
}

/// An image's own PATH (`ENV PATH=/usr/local/cargo/bin:…`) survives a login
/// shell whose `/etc/profile` sets PATH afresh, as Debian's does for root:
/// programs there are still found and run.
#[test]
#[ignore]
fn an_images_path_survives_a_login_shell_that_resets_it() {
    let name = format!("kuana-e2e-path-{}", std::process::id());
    let path = "/opt/kuana-tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
    run(&["run", "-d", "--name", &name, "-e", &format!("PATH={path}"), &image(), "sleep", "infinity"]);
    let _removed = Removed(name.clone());
    run(&[
        "exec",
        &name,
        "sh",
        "-c",
        "mkdir -p /opt/kuana-tools/bin && printf '#!/bin/sh\\necho tool ran\\n' > /opt/kuana-tools/bin/kuana-tool && chmod 755 /opt/kuana-tools/bin/kuana-tool && printf 'PATH=/usr/bin:/bin\\nexport PATH\\n' > /etc/profile",
    ]);
    let transport = Transport::Container { name: name.clone(), user: None };
    let connected = connection::connect(&transport, &options(&|_| {})).unwrap();
    let home = connected.home_path();
    assert!(matches!(kuana_core::exec::which_on(&home, "kuana-tool"), Some(Some(_))));
    let out = Command::new("kuana-tool").current_dir(&home).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "tool ran\n");
    connection::disconnect(&connected.name);
}

#[test]
#[ignore]
fn a_dev_container_is_brought_up_and_found_again() {
    let dir: PathBuf = std::env::temp_dir().join(format!("kuana-devcontainer-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".devcontainer")).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    std::fs::write(dir.join("hello.txt"), "from the host\n").unwrap();
    std::fs::write(
        dir.join(".devcontainer/devcontainer.json"),
        format!(
            "{{\n  // Kuana's end-to-end test\n  \"name\": \"Kuana E2E\",\n  \"image\": \"{}\",\n  \"containerEnv\": {{ \"E2E\": \"yes\" }},\n  \"postCreateCommand\": \"echo made > /tmp/made\",\n  \"postStartCommand\": [\"sh\", \"-c\", \"date +%s%N >> /tmp/starts\"],\n  \"forwardPorts\": [8000, \"db:5432\"],\n}}\n",
            image()
        ),
    )
    .unwrap();
    let options = UpOptions { docker: docker(), cli: None, progress: &|_| {} };
    let up = container::up(&dir, &options).unwrap();
    let _removed = Removed(up.container.clone());
    assert!(up.container.starts_with("kuana-e2e-"), "{}", up.container);
    let basename = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(up.workspace, format!("/workspaces/{basename}"));
    assert_eq!(up.forward_ports, [8000]);
    assert_eq!(up.user, None);

    // Kuana works in it: the project is mounted, its environment and setup are there.
    let transport = Transport::Container { name: up.container.clone(), user: up.user.clone() };
    let connected = connection::connect(&transport, &options_for_up()).unwrap();
    let project = fs::local_path(&connected.root, &up.workspace);
    assert_eq!(fs::read_to_string(project.join("hello.txt")).unwrap(), "from the host\n");
    fs::write(project.join("made-there.txt"), "from the container\n").unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("made-there.txt")).unwrap(), "from the container\n");
    let out = Command::new("sh").args(["-c", "echo $E2E; cat /tmp/made; wc -l < /tmp/starts"]).current_dir(&project).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).split_whitespace().collect::<Vec<_>>(), ["yes", "made", "1"]);
    connection::disconnect(&connected.name);

    // Asked again: the same container, set up once.
    let again = container::up(&dir, &options).unwrap();
    assert_eq!(again.container, up.container);
    // Stopped (a restart of the machine): started again, its start command run again.
    run(&["stop", "-t", "0", &up.container]);
    let started = container::up(&dir, &options).unwrap();
    assert_eq!(started.container, up.container);
    assert_eq!(run(&["inspect", "--format", "{{.State.Running}}", &up.container]), "true");
    assert_eq!(run(&["exec", &up.container, "sh", "-c", "wc -l < /tmp/starts"]), "2");
    let _ = std::fs::remove_dir_all(&dir);
}

fn options_for_up() -> Options<'static> {
    Options { program: Some(docker()), control_dir: std::env::temp_dir(), askpass: Vec::new(), server_binary: &server, progress: &|_| {} }
}
