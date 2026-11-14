//! Kuana's side of the connection to `kuana-server`: `RemoteHost` is the
//! `fs::Host` and `exec::Runner` of a remote mount. Its calls block until
//! the server answers (as `std::fs` blocks on a disk); programs' output and
//! file changes come as notifications, read on the connection's thread and
//! handed to channels.

use std::{
    collections::HashMap,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};

use super::{
    rpc::{Connection, Incoming, not_connected},
    server::{entry_of, meta_of},
};
use crate::{
    exec::{ExitStatus, RemoteProcess, Runner, Spawned, Spec},
    fs::{Host, HostEntry, HostMetadata},
};

/// A program's streams as they come.
struct Streams {
    stdout: Option<mpsc::Sender<Vec<u8>>>,
    stderr: Option<mpsc::Sender<Vec<u8>>>,
    exit: Option<mpsc::Sender<Option<i32>>>,
}

type WatchSink = Box<dyn Fn(Vec<(String, bool)>) + Send + Sync>;

/// Answers a question Git or SSH asks on the host (`askpass`): the prompt,
/// whether it's yes/no; None cancels.
pub type Asker = Arc<dyn Fn(&str, bool) -> Option<String> + Send + Sync>;

static ASKER: Mutex<Option<Asker>> = Mutex::new(None);

/// How hosts' questions are answered (the app's askpass dialog).
pub fn set_asker(asker: Asker) {
    *ASKER.lock().unwrap_or_else(|e| e.into_inner()) = Some(asker);
}

/// Where a remote terminal's output and end go.
pub struct PtySinks {
    pub output: Box<dyn Fn(Vec<u8>) + Send + Sync>,
    pub exit: Box<dyn Fn(Option<i32>) + Send + Sync>,
}

/// What the server told about the machine.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub version: String,
    pub os: String,
    pub arch: String,
    pub home: String,
}

pub struct RemoteHost {
    connection: Arc<Connection>,
    /// Program ids are chosen here, so their output (which may come before
    /// the answer to `proc/spawn`) finds its streams.
    next_process: std::sync::atomic::AtomicU64,
    processes: Mutex<HashMap<u64, Streams>>,
    watches: Mutex<HashMap<u64, WatchSink>>,
    ptys: Mutex<HashMap<u64, PtySinks>>,
    /// Local ends of forwarded connections (`forward`), by id.
    sockets: Mutex<HashMap<u64, std::net::TcpStream>>,
}

impl RemoteHost {
    /// Talks to a server on `reader` / `writer` (an `ssh` process's stdout
    /// and stdin), reading on a thread of its own until the stream ends.
    pub fn start(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) -> Arc<RemoteHost> {
        let host = Arc::new(RemoteHost {
            connection: Arc::new(Connection::new(Box::new(writer))),
            next_process: std::sync::atomic::AtomicU64::new(1),
            processes: Mutex::default(),
            watches: Mutex::default(),
            ptys: Mutex::default(),
            sockets: Mutex::default(),
        });
        let weak = Arc::downgrade(&host);
        let connection = host.connection.clone();
        std::thread::Builder::new()
            .name("kuana-remote".into())
            .spawn(move || {
                let answering = connection.clone();
                connection.read_loop(reader, |incoming| match (weak.upgrade(), incoming) {
                    (Some(host), Incoming::Notification { method, params }) => host.notified(&method, &params),
                    // The server's questions wait for the user: on threads of their own.
                    (_, Incoming::Request { id, method, params }) => {
                        let connection = answering.clone();
                        std::thread::spawn(move || {
                            let answer = match method.as_str() {
                                "askpass" => {
                                    let asker = ASKER.lock().unwrap_or_else(|e| e.into_inner()).clone();
                                    let prompt = params["prompt"].as_str().unwrap_or("");
                                    let answer = asker.and_then(|ask| ask(prompt, params["confirm"].as_bool() == Some(true)));
                                    Ok(json!({ "answer": answer }))
                                }
                                _ => Err(io::Error::new(io::ErrorKind::Unsupported, format!("Kuana doesn't answer {method}"))),
                            };
                            let _ = connection.respond(id, answer);
                        });
                    }
                    _ => {}
                });
                // Gone: programs end, their readers see the end of their output.
                if let Some(host) = weak.upgrade() {
                    host.processes.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    for (_, sinks) in host.ptys.lock().unwrap_or_else(|e| e.into_inner()).drain() {
                        (sinks.exit)(None);
                    }
                }
            })
            .expect("a thread for the connection");
        host
    }

    pub fn info(&self) -> io::Result<Info> {
        let v = self.connection.request("server/info", Value::Null)?;
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        Ok(Info { version: s("version"), os: s("os"), arch: s("arch"), home: s("home") })
    }

    pub fn is_connected(&self) -> bool {
        self.connection.is_open()
    }

    /// Ends the connection (the server, seeing its input end, stops).
    pub fn disconnect(&self) {
        self.connection.shutdown();
    }

    fn call(&self, method: &str, params: Value) -> io::Result<Value> {
        self.connection.request(method, params)
    }

    fn notified(&self, method: &str, params: &Value) {
        let id = params["id"].as_u64().unwrap_or(0);
        match method {
            "proc/output" => {
                let data = BASE64.decode(params["data"].as_str().unwrap_or("")).unwrap_or_default();
                let processes = self.processes.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(streams) = processes.get(&id) {
                    let sink = if params["stream"].as_u64() == Some(2) { &streams.stderr } else { &streams.stdout };
                    if let Some(sink) = sink {
                        let _ = sink.send(data);
                    }
                }
            }
            "proc/exit" => {
                // Dropping the senders ends the readers, after what came before.
                if let Some(mut streams) = self.processes.lock().unwrap_or_else(|e| e.into_inner()).remove(&id)
                    && let Some(exit) = streams.exit.take()
                {
                    let _ = exit.send(params["code"].as_i64().map(|c| c as i32));
                }
            }
            "pty/output" => {
                let data = BASE64.decode(params["data"].as_str().unwrap_or("")).unwrap_or_default();
                if let Some(sinks) = self.ptys.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
                    (sinks.output)(data);
                }
            }
            "pty/exit" => {
                if let Some(sinks) = self.ptys.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
                    (sinks.exit)(params["code"].as_i64().map(|c| c as i32));
                }
            }
            "tcp/data" => {
                let data = BASE64.decode(params["data"].as_str().unwrap_or("")).unwrap_or_default();
                if let Some(socket) = self.sockets.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&id) {
                    let _ = socket.write_all(&data);
                }
            }
            "tcp/closed" => {
                if let Some(socket) = self.sockets.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
                    let _ = socket.shutdown(std::net::Shutdown::Both);
                }
            }
            "watch/changes" => {
                let changes: Vec<(String, bool)> =
                    params["changes"].as_array().into_iter().flatten().filter_map(|c| Some((c[0].as_str()?.to_string(), c[1].as_bool().unwrap_or(true)))).collect();
                if let Some(sink) = self.watches.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
                    sink(changes);
                }
            }
            _ => {}
        }
    }

    /// Watches `path` (recursively); `sink` gets the host's changed paths.
    pub fn watch(&self, path: &str, sink: WatchSink) -> io::Result<u64> {
        let id = self.call("watch/start", json!({ "path": path }))?.as_u64().ok_or_else(|| io::Error::other("no watcher id"))?;
        self.watches.lock().unwrap_or_else(|e| e.into_inner()).insert(id, sink);
        Ok(id)
    }

    pub fn watch_add(&self, id: u64, path: &str) -> io::Result<()> {
        self.call("watch/add", json!({ "id": id, "path": path })).map(|_| ())
    }

    pub fn unwatch(&self, id: u64) {
        self.watches.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        let _ = self.connection.notify("watch/stop", json!({ "id": id }));
    }

    /// Starts `shell` (program and arguments; empty: the host's login shell)
    /// in a terminal there, `cols` × `rows`; its output and end go to `sinks`.
    pub fn pty_open(&self, shell: &[String], cwd: Option<&str>, env: &[(String, String)], size: (u16, u16), sinks: PtySinks) -> io::Result<u64> {
        let id = self.next_process.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Ready before it starts: its prompt may come before the answer.
        self.ptys.lock().unwrap_or_else(|e| e.into_inner()).insert(id, sinks);
        let env: Vec<Value> = env.iter().map(|(k, v)| json!([k, v])).collect();
        let params = json!({ "id": id, "shell": shell, "cwd": cwd, "env": env, "cols": size.0, "rows": size.1 });
        if let Err(err) = self.call("pty/open", params) {
            self.ptys.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            return Err(err);
        }
        Ok(id)
    }

    pub fn pty_input(&self, id: u64, data: &[u8]) {
        let _ = self.connection.notify("pty/input", json!({ "id": id, "data": BASE64.encode(data) }));
    }

    pub fn pty_resize(&self, id: u64, cols: u16, rows: u16) {
        let _ = self.connection.notify("pty/resize", json!({ "id": id, "cols": cols, "rows": rows }));
    }

    pub fn pty_close(&self, id: u64) {
        let _ = self.connection.notify("pty/close", json!({ "id": id }));
    }

    /// What runs in the terminal's foreground when it isn't the shell.
    pub fn pty_foreground(&self, id: u64) -> Option<String> {
        self.call("pty/foreground", json!({ "id": id })).ok()?.as_str().map(str::to_string)
    }

    /// A local port relayed to `port` on the host's loopback (a debug
    /// adapter listening there). Each connection to it opens one there,
    /// retried for a while: the program may still be starting.
    pub fn forward(self: &Arc<Self>, port: u16) -> io::Result<std::net::SocketAddr> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let address = listener.local_addr()?;
        let weak = Arc::downgrade(self);
        std::thread::Builder::new().name("kuana-forward".into()).spawn(move || {
            for local in listener.incoming().flatten() {
                let Some(host) = weak.upgrade() else { break };
                std::thread::spawn(move || host.relay(local, port));
            }
        })?;
        Ok(address)
    }

    fn relay(&self, local: std::net::TcpStream, port: u16) {
        let id = self.next_process.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Ok(writer) = local.try_clone() else { return };
        self.sockets.lock().unwrap_or_else(|e| e.into_inner()).insert(id, writer);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match self.call("tcp/connect", json!({ "id": id, "port": port })) {
                Ok(_) => break,
                Err(_) if std::time::Instant::now() < deadline && self.is_connected() => std::thread::sleep(std::time::Duration::from_millis(200)),
                Err(_) => {
                    if let Some(socket) = self.sockets.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
                        let _ = socket.shutdown(std::net::Shutdown::Both);
                    }
                    return;
                }
            }
        }
        let mut local = local;
        let mut buf = vec![0; 64 * 1024];
        while let Ok(n) = local.read(&mut buf) {
            if n == 0 || self.connection.notify("tcp/send", json!({ "id": id, "data": BASE64.encode(&buf[..n]) })).is_err() {
                break;
            }
        }
        let _ = self.connection.notify("tcp/close", json!({ "id": id }));
        self.sockets.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    }

    /// The host's login shell (`$SHELL` there).
    pub fn shell(&self) -> Option<String> {
        self.call("server/info", Value::Null).ok()?["shell"].as_str().filter(|s| !s.is_empty()).map(str::to_string)
    }

    /// A project's files under `path` (host paths), with whether each is ignored.
    pub fn scan(&self, path: &str) -> io::Result<Vec<(String, bool)>> {
        let files = self.call("scan", json!({ "path": path }))?;
        Ok(files.as_array().into_iter().flatten().filter_map(|f| Some((f[0].as_str()?.to_string(), f[1].as_bool().unwrap_or(false)))).collect())
    }
}

impl Host for RemoteHost {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        let data = self.call("fs/read", json!({ "path": path }))?;
        BASE64.decode(data.as_str().unwrap_or("")).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.call("fs/write", json!({ "path": path, "data": BASE64.encode(data) })).map(|_| ())
    }

    fn create(&self, path: &str, data: &[u8], create_new: bool) -> io::Result<()> {
        self.call("fs/write", json!({ "path": path, "data": BASE64.encode(data), "create_new": create_new })).map(|_| ())
    }

    fn metadata(&self, path: &str, follow: bool) -> io::Result<HostMetadata> {
        self.call("fs/metadata", json!({ "path": path, "follow": follow })).map(|v| meta_of(&v))
    }

    fn read_dir(&self, path: &str) -> io::Result<Vec<HostEntry>> {
        let entries = self.call("fs/read_dir", json!({ "path": path }))?;
        Ok(entries.as_array().into_iter().flatten().filter_map(entry_of).collect())
    }

    fn create_dir(&self, path: &str, all: bool) -> io::Result<()> {
        self.call("fs/create_dir", json!({ "path": path, "all": all })).map(|_| ())
    }

    fn remove_file(&self, path: &str) -> io::Result<()> {
        self.call("fs/remove_file", json!({ "path": path })).map(|_| ())
    }

    fn remove_dir(&self, path: &str, all: bool) -> io::Result<()> {
        self.call("fs/remove_dir", json!({ "path": path, "all": all })).map(|_| ())
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.call("fs/rename", json!({ "from": from, "to": to })).map(|_| ())
    }

    fn copy(&self, from: &str, to: &str) -> io::Result<u64> {
        self.call("fs/copy", json!({ "from": from, "to": to })).map(|v| v.as_u64().unwrap_or(0))
    }

    fn canonicalize(&self, path: &str) -> io::Result<String> {
        self.call("fs/canonicalize", json!({ "path": path })).map(|v| v.as_str().unwrap_or(path).to_string())
    }

    fn read_link(&self, path: &str) -> io::Result<String> {
        self.call("fs/read_link", json!({ "path": path })).map(|v| v.as_str().unwrap_or("").to_string())
    }

    fn trash(&self, path: &str) -> io::Result<String> {
        self.call("fs/trash", json!({ "path": path })).map(|v| v.as_str().unwrap_or("").to_string())
    }

    fn connected(&self) -> bool {
        self.connection.is_open()
    }

    fn scan(&self, path: &str) -> Option<io::Result<Vec<(String, bool)>>> {
        Some(RemoteHost::scan(self, path))
    }

    fn watch(&self, path: &str, sink: WatchSink) -> Option<io::Result<u64>> {
        Some(RemoteHost::watch(self, path, sink))
    }

    fn watch_add(&self, id: u64, path: &str) -> io::Result<()> {
        RemoteHost::watch_add(self, id, path)
    }

    fn unwatch(&self, id: u64) {
        RemoteHost::unwatch(self, id)
    }
}

/// Reads what arrives on a channel until its sender goes.
struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    at: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.at == self.buf.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.buf = chunk;
                    self.at = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.at);
        out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Writes to a remote program's stdin; dropped, it closes it.
struct RemoteStdin {
    connection: Arc<Connection>,
    id: u64,
}

impl Write for RemoteStdin {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.connection.notify("proc/stdin", json!({ "id": self.id, "data": BASE64.encode(data) }))?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for RemoteStdin {
    fn drop(&mut self) {
        let _ = self.connection.notify("proc/close_stdin", json!({ "id": self.id }));
    }
}

struct Remote {
    connection: Arc<Connection>,
    id: u64,
    pid: u32,
    exit: mpsc::Receiver<Option<i32>>,
    done: Option<ExitStatus>,
}

impl RemoteProcess for Remote {
    fn id(&self) -> u32 {
        self.pid
    }

    fn kill(&mut self) -> io::Result<()> {
        self.connection.notify("proc/kill", json!({ "id": self.id }))
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(done) = self.done {
            return Ok(done);
        }
        let code = self.exit.recv().map_err(|_| not_connected())?;
        let status = ExitStatus { code };
        self.done = Some(status);
        Ok(status)
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(done) = self.done {
            return Ok(Some(done));
        }
        match self.exit.try_recv() {
            Ok(code) => {
                self.done = Some(ExitStatus { code });
                Ok(self.done)
            }
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(not_connected()),
        }
    }
}

impl Runner for RemoteHost {
    fn which(&self, name: &str) -> io::Result<Option<String>> {
        Ok(self.call("which", json!({ "name": name }))?.as_str().map(str::to_string))
    }

    fn spawn(&self, spec: &Spec) -> io::Result<Spawned> {
        let channel = |on: bool| if on { let (tx, rx) = mpsc::channel(); (Some(tx), Some(rx)) } else { (None, None) };
        let (stdout_tx, stdout_rx) = channel(spec.stdout);
        let (stderr_tx, stderr_rx) = channel(spec.stderr);
        let (exit_tx, exit_rx) = mpsc::channel();
        let env: Vec<Value> = spec.env.iter().map(|(k, v)| json!([k, v])).collect();
        let id = self.next_process.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let params = json!({ "id": id, "spec": {
            "program": spec.program, "args": spec.args, "cwd": spec.cwd, "env": env,
            "stdin": spec.stdin, "stdout": spec.stdout, "stderr": spec.stderr,
        } });
        // Registered before it starts: its first output may come before the answer.
        self.processes.lock().unwrap_or_else(|e| e.into_inner()).insert(id, Streams { stdout: stdout_tx, stderr: stderr_tx, exit: Some(exit_tx) });
        let started = match self.call("proc/spawn", params) {
            Ok(started) => started,
            Err(err) => {
                self.processes.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                return Err(err);
            }
        };
        let pid = started["pid"].as_u64().unwrap_or(0) as u32;
        let reader = |rx: Option<mpsc::Receiver<Vec<u8>>>| rx.map(|rx| Box::new(ChannelReader { rx, buf: Vec::new(), at: 0 }) as Box<dyn Read + Send>);
        Ok(Spawned {
            stdin: spec.stdin.then(|| Box::new(RemoteStdin { connection: self.connection.clone(), id }) as Box<dyn Write + Send>),
            stdout: reader(stdout_rx),
            stderr: reader(stderr_rx),
            process: Box::new(Remote { connection: self.connection.clone(), id, pid, exit: exit_rx, done: None }),
        })
    }
}

static HOSTS: Mutex<Vec<(PathBuf, Arc<RemoteHost>)>> = Mutex::new(Vec::new());

/// Mounts a connected host: its files at `fs::mount_root(name)`, programs run there too.
pub fn mount(name: &str, host: Arc<RemoteHost>) -> PathBuf {
    let root = crate::fs::mount(name, host.clone());
    crate::exec::mount_runner(&root, host.clone());
    let mut hosts = HOSTS.lock().unwrap_or_else(|e| e.into_inner());
    hosts.retain(|(r, _)| r != &root);
    hosts.push((root.clone(), host));
    root
}

pub fn unmount(root: &Path) {
    crate::fs::unmount(root);
    crate::exec::unmount_runner(root);
    HOSTS.lock().unwrap_or_else(|e| e.into_inner()).retain(|(r, _)| r != root);
}

/// The connected host a path is on (for what `fs` and `exec` don't cover: terminals).
pub fn host_for(path: &Path) -> Option<(PathBuf, Arc<RemoteHost>)> {
    let hosts = HOSTS.lock().unwrap_or_else(|e| e.into_inner());
    hosts.iter().find(|(root, _)| path.starts_with(root)).map(|(root, host)| (root.clone(), host.clone()))
}
