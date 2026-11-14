//! `kuana-server`'s side of the connection: answers file requests, runs
//! programs (their output streamed back as `proc/output`, the exit sent only
//! once all of it is), watches folders and scans projects. Requests are
//! answered on threads of their own, so a slow one (a big file, a scan)
//! doesn't hold up the rest.

use std::{
    collections::HashMap,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};

use super::rpc::{Connection, Incoming};
use crate::{
    exec::{Command, Stdio},
    fs::{self, HostEntry},
};

/// Where `fs/trash` puts things (None: the platform's trash folder).
#[derive(Clone, Default)]
pub struct Options {
    pub trash: Option<PathBuf>,
}

struct Process {
    stdin: Option<Box<dyn Write + Send>>,
    child: Arc<Mutex<Option<crate::exec::Child>>>,
}

struct State {
    connection: Arc<Connection>,
    options: Options,
    processes: Arc<Mutex<HashMap<u64, Process>>>,
    watchers: Mutex<HashMap<u64, crate::watcher::Watcher>>,
    next: Mutex<u64>,
}

/// Serves one connection until it ends.
pub fn serve(input: impl Read, output: impl Write + Send + 'static, options: Options) {
    let connection = Arc::new(Connection::new(Box::new(output)));
    let state = Arc::new(State { connection: connection.clone(), options, processes: Arc::default(), watchers: Mutex::default(), next: Mutex::new(1) });
    // Requests being answered: the server finishes them before it exits.
    let busy = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    connection.read_loop(input, |incoming| match incoming {
        Incoming::Request { id, method, params } => {
            let (state, busy) = (state.clone(), busy.clone());
            busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::spawn(move || {
                let answer = state.handle(&method, &params);
                let _ = state.connection.respond(id, answer);
                busy.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        // In order, on this thread: input to a program mustn't be reordered.
        Incoming::Notification { method, params } => state.notified(&method, &params),
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while busy.load(std::sync::atomic::Ordering::SeqCst) > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // The client went: its programs end with it.
    for (_, process) in state.processes.lock().unwrap_or_else(|e| e.into_inner()).drain() {
        if let Some(child) = process.child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            child.kill_tree();
        }
    }
}

fn path(params: &Value, key: &str) -> io::Result<PathBuf> {
    params[key].as_str().map(PathBuf::from).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("no {key}")))
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl State {
    fn id(&self) -> u64 {
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        *next += 1;
        *next
    }

    fn handle(&self, method: &str, params: &Value) -> io::Result<Value> {
        let p = || path(params, "path");
        match method {
            "server/info" => Ok(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "home": std::env::var("HOME").unwrap_or_default(),
            })),
            "fs/read" => Ok(json!(BASE64.encode(std::fs::read(p()?)?))),
            "fs/write" => {
                let data = BASE64.decode(params["data"].as_str().unwrap_or("")).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                if params["create_new"].as_bool() == Some(true) {
                    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(p()?)?;
                    file.write_all(&data)?;
                } else {
                    std::fs::write(p()?, data)?;
                }
                Ok(Value::Null)
            }
            "fs/metadata" => {
                let path = p()?;
                let meta = if params["follow"].as_bool() == Some(false) { fs::symlink_metadata(&path)? } else { fs::metadata(&path)? };
                Ok(serde_json::to_value(meta_json(&meta.to_host()))?)
            }
            "fs/read_dir" => {
                let mut entries: Vec<Value> = Vec::new();
                for entry in std::fs::read_dir(p()?)? {
                    let entry = entry?;
                    // A link's own type, but whether it leads to a folder too.
                    let mut meta = fs::Metadata::Local(entry.metadata()?).to_host();
                    if entry.file_type()?.is_symlink() {
                        meta.is_symlink = true;
                        if let Ok(target) = std::fs::metadata(entry.path()) {
                            meta.is_dir = target.is_dir();
                            meta.is_file = target.is_file();
                        }
                    }
                    entries.push(json!({ "name": entry.file_name().to_string_lossy(), "metadata": meta_json(&meta) }));
                }
                Ok(Value::Array(entries))
            }
            "fs/create_dir" => {
                if params["all"].as_bool() == Some(true) { std::fs::create_dir_all(p()?)? } else { std::fs::create_dir(p()?)? }
                Ok(Value::Null)
            }
            "fs/remove_file" => std::fs::remove_file(p()?).map(|_| Value::Null),
            "fs/remove_dir" => {
                if params["all"].as_bool() == Some(true) { std::fs::remove_dir_all(p()?)? } else { std::fs::remove_dir(p()?)? }
                Ok(Value::Null)
            }
            "fs/rename" => std::fs::rename(path(params, "from")?, path(params, "to")?).map(|_| Value::Null),
            "fs/copy" => std::fs::copy(path(params, "from")?, path(params, "to")?).map(|n| json!(n)),
            "fs/canonicalize" => std::fs::canonicalize(p()?).map(|c| json!(text(&c))),
            "fs/read_link" => std::fs::read_link(p()?).map(|c| json!(text(&c))),
            "fs/trash" => self.trash(&p()?).map(|to| json!(text(&to))),
            "scan" => {
                let files = crate::project::scan_files(&p()?);
                Ok(Value::Array(files.into_iter().map(|(path, ignored)| json!([text(&path), ignored])).collect()))
            }
            "proc/spawn" => self.spawn(params),
            "watch/start" => self.watch(&p()?),
            "watch/add" => {
                let id = params["id"].as_u64().unwrap_or(0);
                let mut watchers = self.watchers.lock().unwrap_or_else(|e| e.into_inner());
                let watcher = watchers.get_mut(&id).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such watcher"))?;
                watcher.add(&p()?).map_err(io::Error::other)?;
                Ok(Value::Null)
            }
            _ => Err(io::Error::new(io::ErrorKind::Unsupported, format!("kuana-server doesn't know {method}"))),
        }
    }

    fn notified(&self, method: &str, params: &Value) {
        let id = params["id"].as_u64().unwrap_or(0);
        match method {
            "proc/stdin" => {
                let data = BASE64.decode(params["data"].as_str().unwrap_or("")).unwrap_or_default();
                let mut processes = self.processes.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(stdin) = processes.get_mut(&id).and_then(|p| p.stdin.as_mut())
                    && stdin.write_all(&data).and_then(|_| stdin.flush()).is_err()
                {
                    processes.get_mut(&id).map(|p| p.stdin.take());
                }
            }
            "proc/close_stdin" => {
                if let Some(process) = self.processes.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&id) {
                    process.stdin = None;
                }
            }
            "proc/kill" => {
                let child = self.processes.lock().unwrap_or_else(|e| e.into_inner()).get(&id).map(|p| p.child.clone());
                if let Some(child) = child
                    && let Some(child) = child.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
                {
                    child.kill_tree();
                }
            }
            "watch/stop" => {
                self.watchers.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            }
            _ => {}
        }
    }

    /// Starts a program; its output comes as `proc/output`, then `proc/exit`.
    fn spawn(&self, params: &Value) -> io::Result<Value> {
        let spec = &params["spec"];
        let program = spec["program"].as_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no program"))?;
        let mut command = Command::new(program);
        command.args(spec["args"].as_array().into_iter().flatten().filter_map(Value::as_str));
        if let Some(cwd) = spec["cwd"].as_str() {
            command.current_dir(cwd);
        }
        for pair in spec["env"].as_array().into_iter().flatten() {
            match (pair[0].as_str(), pair[1].as_str()) {
                (Some(k), Some(v)) => command.env(k, v),
                (Some(k), None) => command.env_remove(k),
                _ => &mut command,
            };
        }
        let io = |on: bool| if on { Stdio::piped() } else { Stdio::null() };
        command.stdin(io(spec["stdin"].as_bool() == Some(true)));
        command.stdout(io(spec["stdout"].as_bool() == Some(true)));
        command.stderr(io(spec["stderr"].as_bool() == Some(true)));
        let mut child = command.spawn()?;
        // The client's id: it has the program's streams ready under it.
        let id = params["id"].as_u64().unwrap_or_else(|| self.id());
        let pid = child.id();
        let readers: Vec<_> = [(child.stdout.take(), 1), (child.stderr.take(), 2)]
            .into_iter()
            .filter_map(|(pipe, stream)| pipe.map(|p| (p, stream)))
            .map(|(mut pipe, stream)| {
                let connection = self.connection.clone();
                std::thread::spawn(move || {
                    let mut buf = vec![0; 64 * 1024];
                    loop {
                        match pipe.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let _ = connection.notify("proc/output", json!({ "id": id, "stream": stream, "data": BASE64.encode(&buf[..n]) }));
                            }
                        }
                    }
                })
            })
            .collect();
        let stdin = child.stdin.take();
        let child = Arc::new(Mutex::new(Some(child)));
        self.processes.lock().unwrap_or_else(|e| e.into_inner()).insert(id, Process { stdin, child: child.clone() });
        let (connection, processes) = (self.connection.clone(), self.processes.clone());
        std::thread::spawn(move || {
            for reader in readers {
                let _ = reader.join();
            }
            // Polled, not waited on: the program stays reachable for `proc/kill`.
            let code = loop {
                let status = match child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    Some(c) => c.try_wait(),
                    None => break None,
                };
                match status {
                    Ok(Some(status)) => break status.code(),
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                    Err(_) => break None,
                }
            };
            child.lock().unwrap_or_else(|e| e.into_inner()).take();
            processes.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            let _ = connection.notify("proc/exit", json!({ "id": id, "code": code }));
        });
        Ok(json!({ "id": id, "pid": pid }))
    }

    fn watch(&self, root: &Path) -> io::Result<Value> {
        let (watcher, mut changes) = crate::watcher::Watcher::new(root).map_err(io::Error::other)?;
        let id = self.id();
        self.watchers.lock().unwrap_or_else(|e| e.into_inner()).insert(id, watcher);
        let connection = self.connection.clone();
        std::thread::spawn(move || {
            while let Some(batch) = futures::executor::block_on(futures::StreamExt::next(&mut changes)) {
                let changes: Vec<Value> = batch.iter().map(|c| json!([text(&c.path), c.structural])).collect();
                if connection.notify("watch/changes", json!({ "id": id, "changes": changes })).is_err() {
                    break;
                }
            }
        });
        Ok(json!(id))
    }

    /// Moves `path` into the trash folder, under a name not taken there.
    fn trash(&self, path: &Path) -> io::Result<PathBuf> {
        let folder = match &self.options.trash {
            Some(folder) => folder.clone(),
            None => trash_folder()?,
        };
        std::fs::create_dir_all(&folder)?;
        let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no name"))?.to_string_lossy().into_owned();
        let mut to = folder.join(&name);
        let mut n = 1;
        while to.symlink_metadata().is_ok() {
            n += 1;
            to = folder.join(format!("{name} {n}"));
        }
        std::fs::rename(path, &to)?;
        // freedesktop's trash keeps where each item came from.
        if let Some(info) = folder.parent().map(|p| p.join("info"))
            && folder.ends_with("files")
            && std::fs::create_dir_all(&info).is_ok()
        {
            let entry = format!("[Trash Info]\nPath={}\n", text(path));
            let _ = std::fs::write(info.join(format!("{}.trashinfo", to.file_name().unwrap_or_default().to_string_lossy())), entry);
        }
        Ok(to)
    }
}

/// The user's trash folder: freedesktop's, or macOS's.
fn trash_folder() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| io::Error::other("no home folder"))?;
    if cfg!(target_os = "macos") {
        return Ok(home.join(".Trash"));
    }
    let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
    Ok(data.join("Trash/files"))
}

pub(crate) fn meta_json(m: &fs::HostMetadata) -> Value {
    json!({
        "is_dir": m.is_dir, "is_file": m.is_file, "is_symlink": m.is_symlink, "len": m.len,
        "modified_ms": m.modified_ms, "readonly": m.readonly, "executable": m.executable,
    })
}

pub(crate) fn meta_of(v: &Value) -> fs::HostMetadata {
    fs::HostMetadata {
        is_dir: v["is_dir"].as_bool().unwrap_or(false),
        is_file: v["is_file"].as_bool().unwrap_or(false),
        is_symlink: v["is_symlink"].as_bool().unwrap_or(false),
        len: v["len"].as_u64().unwrap_or(0),
        modified_ms: v["modified_ms"].as_u64(),
        readonly: v["readonly"].as_bool().unwrap_or(false),
        executable: v["executable"].as_bool().unwrap_or(false),
    }
}

pub(crate) fn entry_of(v: &Value) -> Option<HostEntry> {
    Some(HostEntry { name: v["name"].as_str()?.to_string(), metadata: meta_of(&v["metadata"]) })
}
