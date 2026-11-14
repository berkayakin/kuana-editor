//! `kuana-server`'s terminals: a pty per `pty/open` (alacritty's, which sets
//! up the session, the controlling terminal and window sizes), its output
//! streamed back as `pty/output`, `pty/exit` once the shell is gone and its
//! output read. Unix only (Windows hosts aren't remotes yet).

use std::{
    collections::HashMap,
    io::{self, Read, Write},
    os::fd::AsRawFd,
    sync::{Arc, Mutex},
};

use alacritty_terminal::{
    event::{OnResize, WindowSize},
    tty::{self, EventedPty, Shell},
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};

use super::rpc::Connection;

struct Open {
    pty: tty::Pty,
    /// Writes to the shell (the master side).
    input: std::fs::File,
}

#[derive(Default)]
pub(super) struct Ptys(Mutex<HashMap<u64, Arc<Mutex<Open>>>>);

fn window(cols: u64, rows: u64) -> WindowSize {
    WindowSize { num_cols: cols.clamp(2, 1000) as u16, num_lines: rows.clamp(1, 1000) as u16, cell_width: 8, cell_height: 16 }
}

impl Ptys {
    /// Starts `shell` (program and arguments) in a pty; its output goes to
    /// `connection` under `id`, the client's.
    pub fn open(&self, connection: Arc<Connection>, params: &Value, env: &[(String, String)]) -> io::Result<Value> {
        let id = params["id"].as_u64().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no id"))?;
        let mut words = params["shell"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string);
        let program = words.next().or_else(|| std::env::var("SHELL").ok()).unwrap_or_else(|| "/bin/sh".into());
        let args: Vec<String> = words.collect();
        let mut vars: HashMap<String, String> = env.iter().cloned().collect();
        for pair in params["env"].as_array().into_iter().flatten() {
            if let (Some(k), Some(v)) = (pair[0].as_str(), pair[1].as_str()) {
                vars.insert(k.to_string(), v.to_string());
            }
        }
        let options = tty::Options {
            shell: Some(Shell::new(program, args)),
            working_directory: params["cwd"].as_str().map(Into::into),
            drain_on_exit: true,
            env: vars,
        };
        let pty = tty::new(&options, window(params["cols"].as_u64().unwrap_or(80), params["rows"].as_u64().unwrap_or(24)), id)?;
        let master = pty.file();
        // alacritty's event loop wants it non-blocking; these threads block.
        // SAFETY: fcntl on a descriptor this pty owns.
        unsafe {
            let fd = master.as_raw_fd();
            libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL, 0) & !libc::O_NONBLOCK);
        }
        let (mut output, input) = (master.try_clone()?, master.try_clone()?);
        let pid = pty.child().id();
        let open = Arc::new(Mutex::new(Open { pty, input }));
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(id, open.clone());
        let reader = {
            let connection = connection.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0; 64 * 1024];
                // EIO once the shell and its children closed the pty.
                while let Ok(n) = output.read(&mut buf) {
                    if n == 0 || connection.notify("pty/output", json!({ "id": id, "data": BASE64.encode(&buf[..n]) })).is_err() {
                        break;
                    }
                }
            })
        };
        std::thread::spawn(move || {
            // alacritty's own check: SIGCHLD's pipe, then the child.
            let code = loop {
                let event = open.lock().unwrap_or_else(|e| e.into_inner()).pty.next_child_event();
                match event {
                    Some(tty::ChildEvent::Exited(status)) => break status.and_then(|s| s.code()),
                    None => std::thread::sleep(std::time::Duration::from_millis(50)),
                }
            };
            let _ = reader.join();
            let _ = connection.notify("pty/exit", json!({ "id": id, "code": code }));
        });
        Ok(json!({ "pid": pid }))
    }

    fn get(&self, id: u64) -> Option<Arc<Mutex<Open>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned()
    }

    pub fn input(&self, id: u64, data: &[u8]) {
        if let Some(open) = self.get(id) {
            let _ = open.lock().unwrap_or_else(|e| e.into_inner()).input.write_all(data);
        }
    }

    pub fn resize(&self, id: u64, cols: u64, rows: u64) {
        if let Some(open) = self.get(id) {
            open.lock().unwrap_or_else(|e| e.into_inner()).pty.on_resize(window(cols, rows));
        }
    }

    /// Ends the shell: hung up, as a closed terminal window does (the
    /// waiting thread then reports its exit).
    pub fn close(&self, id: u64) {
        let gone = self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        if let Some(open) = gone {
            let pid = open.lock().unwrap_or_else(|e| e.into_inner()).pty.child().id() as i32;
            // SAFETY: a signal to the shell this pty started.
            unsafe { libc::kill(pid, libc::SIGHUP) };
        }
    }

    /// The command in the pty's foreground when it isn't the shell (what
    /// closing the terminal would end), by name.
    pub fn foreground(&self, id: u64) -> Option<String> {
        let open = self.get(id)?;
        let open = open.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: tcgetpgrp on the master the pty owns.
        let group = unsafe { libc::tcgetpgrp(open.pty.file().as_raw_fd()) };
        let shell = open.pty.child().id() as i32;
        if group <= 0 || group == shell {
            return None;
        }
        let out = std::process::Command::new("ps").args(["-o", "comm=", "-p", &group.to_string()]).output().ok()?;
        let name = String::from_utf8_lossy(&out.stdout).trim().rsplit('/').next()?.to_string();
        (!name.is_empty()).then_some(name)
    }
}
