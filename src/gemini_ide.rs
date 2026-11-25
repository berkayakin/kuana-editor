//! Gemini CLI's IDE companion protocol (its `/ide enable`), as its VS Code
//! companion offers it: Gemini CLI running in a terminal knows which files
//! are open in Kuana and what's selected, and proposes edits as diffs to
//! accept or reject here. See `docs/ide-integration/ide-companion-spec.md`
//! in google-gemini/gemini-cli.
//!
//! Each project window runs a [`Server`]: MCP over HTTP ("Streamable HTTP")
//! at `http://127.0.0.1:<port>/mcp`, advertised by a discovery file,
//! `<temp>/gemini-ide-<uid>/gemini-ide-server-<pid>-<port>.json`, holding the
//! project's folders and a random token every request must carry as a
//! bearer token. Shells a window starts get [`terminal_env`], which settles
//! which window a `gemini` there connects to when several hold its folder.
//!
//! Threads, as for Claude Code (`claude_ide.rs`; what they share is in
//! `agent_ide.rs`): one accepting connections and one per connection. A
//! session's notifications go out on its event stream, a GET the client
//! keeps open, written by that connection's thread; the window's tool
//! answers are waited for on the thread of the POST that asked.

mod http;
mod protocol;

use std::{
    collections::HashMap,
    fs,
    io::{self, BufReader},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc as std_mpsc,
    },
    time::Duration,
};

use futures::channel::mpsc;
use serde_json::Value;

pub use crate::agent_ide::{Request, ToolCall};
pub use protocol::{OpenFile, caret, closed_diff, context};

use crate::agent_ide::{Routes, alive, same_token};

/// The variables that point a terminal's `gemini` at a window: the port
/// breaks ties between windows holding its folder, the process id names
/// the discovery file to read first (the CLI otherwise guesses it from its
/// process tree), and the folders and token stand in for the file.
const PORT_VARIABLE: &str = "GEMINI_CLI_IDE_SERVER_PORT";
const PID_VARIABLE: &str = "GEMINI_CLI_IDE_PID";
const WORKSPACE_VARIABLE: &str = "GEMINI_CLI_IDE_WORKSPACE_PATH";
const TOKEN_VARIABLE: &str = "GEMINI_CLI_IDE_AUTH_TOKEN";
/// How long a connection may sit between requests.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a write may wait for a client that stopped reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a quiet event stream gets a comment: Node's fetch gives up on a
/// body silent for 5 minutes, and Gemini CLI then counts itself disconnected.
const KEEPALIVE: Duration = Duration::from_secs(30);

/// Where Gemini CLI looks for discovery files.
pub fn discovery_dir() -> PathBuf {
    let temp = std::env::temp_dir();
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions.
        temp.join(format!("gemini-ide-{}", unsafe { libc::getuid() }))
    }
    #[cfg(not(unix))]
    {
        temp.join("gemini").join("ide")
    }
}

/// Whether Gemini CLI has run on this machine (its config folder exists).
pub fn gemini_installed() -> bool {
    let home = std::env::var_os("GEMINI_CLI_HOME").filter(|h| !h.is_empty()).map(PathBuf::from).or_else(crate::clone_dialog::home_dir);
    home.is_some_and(|home| home.join(".gemini").is_dir())
}

/// The servers running: which one a new terminal's `gemini` should reach.
static SERVERS: Routes = Routes::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// The variables for a shell started in `cwd`: those of the window whose
/// folder holds it (the innermost), if any.
pub fn terminal_env(cwd: &Path) -> Vec<(String, String)> {
    SERVERS.env_for(cwd)
}

/// A window's server. Dropping it stops it and removes its discovery file.
pub struct Server {
    shared: Arc<Shared>,
}

struct Shared {
    port: u16,
    /// Its route for terminals (`terminal_env`).
    route: u64,
    token: String,
    file: PathBuf,
    state: Mutex<State>,
    sessions: Mutex<HashMap<String, Session>>,
    /// Every open connection, to end it when the server stops.
    connections: Mutex<Vec<(u64, TcpStream)>>,
    requests: mpsc::UnboundedSender<Request>,
    stopped: AtomicBool,
}

struct State {
    folders: Vec<PathBuf>,
    /// The `ide/contextUpdate` params last sent; None until there's been one.
    context: Option<Value>,
}

/// An MCP session (one per `gemini` connected) and its event stream, while
/// it has one open.
#[derive(Default)]
struct Session {
    stream: Option<(u64, std_mpsc::Sender<Option<String>>)>,
}

impl Server {
    /// Listens on a free loopback port and writes the discovery file into
    /// `dir`; the window's requests come out of the receiver.
    pub fn start(folders: Vec<PathBuf>, dir: &Path) -> io::Result<(Server, mpsc::UnboundedReceiver<Request>)> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        crate::agent_ide::private_dir(dir, true)?;
        remove_stale_files(dir);
        let (requests, received) = mpsc::unbounded();
        let shared = Arc::new(Shared {
            port,
            route: Routes::key(),
            token: crate::instance::random_token(),
            file: dir.join(protocol::file_name(std::process::id(), port)),
            state: Mutex::new(State { folders, context: None }),
            sessions: Mutex::new(HashMap::new()),
            connections: Mutex::new(Vec::new()),
            requests,
            stopped: AtomicBool::new(false),
        });
        shared.write_file()?;
        shared.register();
        let accepting = shared.clone();
        let started = std::thread::Builder::new().name("gemini ide".into()).spawn(move || {
            for stream in listener.incoming() {
                if accepting.stopped.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let shared = accepting.clone();
                let _ = std::thread::Builder::new().name("gemini ide connection".into()).spawn(move || serve(stream, &shared));
            }
        });
        let server = Server { shared };
        // Dropped on failure: the discovery file goes with it.
        started?;
        Ok((server, received))
    }

    #[cfg(test)]
    fn port(&self) -> u16 {
        self.shared.port
    }

    /// Whether Gemini CLI is connected now (a session's stream is open).
    pub fn has_clients(&self) -> bool {
        self.shared.sessions.lock().unwrap_or_else(|e| e.into_inner()).values().any(|s| s.stream.is_some())
    }

    /// The project's folders changed.
    pub fn set_folders(&self, folders: Vec<PathBuf>) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.folders == folders {
            return;
        }
        state.folders = folders;
        drop(state);
        if let Err(err) = self.shared.write_file() {
            eprintln!("couldn't update Gemini CLI's discovery file: {err}");
        }
        self.shared.register();
    }

    /// What's open and selected (`protocol::context`): Gemini hears of it
    /// when it changed.
    pub fn set_context(&self, context: Value) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.context.as_ref() == Some(&context) {
            return;
        }
        state.context = Some(context.clone());
        drop(state);
        self.broadcast("ide/contextUpdate", context);
    }

    /// Sends a notification to every connected session.
    pub fn broadcast(&self, method: &str, params: Value) {
        let text = crate::agent_ide::notification(method, params).to_string();
        for session in self.shared.sessions.lock().unwrap_or_else(|e| e.into_inner()).values() {
            if let Some((_, outgoing)) = &session.stream {
                let _ = outgoing.send(Some(text.clone()));
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let shared = &self.shared;
        shared.stopped.store(true, Ordering::Relaxed);
        let _ = fs::remove_file(&shared.file);
        SERVERS.remove(shared.route);
        // Wakes the accepting thread, which then sees it's stopped.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, shared.port));
        shared.end_connections();
    }
}

impl Shared {
    fn end_connections(&self) {
        for session in self.sessions.lock().unwrap_or_else(|e| e.into_inner()).values() {
            if let Some((_, outgoing)) = &session.stream {
                let _ = outgoing.send(None);
            }
        }
        for (_, socket) in self.connections.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }

    fn folders(&self) -> Vec<PathBuf> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).folders.clone()
    }

    fn register(&self) {
        let folders = self.folders();
        let env = vec![
            (PORT_VARIABLE.to_string(), self.port.to_string()),
            (PID_VARIABLE.to_string(), std::process::id().to_string()),
            (WORKSPACE_VARIABLE.to_string(), protocol::workspace_path(&folders)),
            (TOKEN_VARIABLE.to_string(), self.token.clone()),
        ];
        SERVERS.set(self.route, folders, env);
    }

    fn write_file(&self) -> io::Result<()> {
        let content = protocol::discovery(self.port, &self.folders(), &self.token);
        crate::agent_ide::write_private(&self.file, content.to_string().as_bytes())
    }

    /// A session's stream opened or closed: the window counts connections.
    fn tell(&self, request: Request) {
        let _ = self.requests.unbounded_send(request);
    }
}

/// Removes the discovery files of Kuanas that ended without removing
/// theirs (a crash, a kill), so Gemini CLI doesn't try them.
fn remove_stale_files(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = protocol::pid_in(&name) else { continue };
        if pid == std::process::id() || alive(pid) {
            continue;
        }
        let path = entry.path();
        let ours = fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()).is_some_and(|c| protocol::is_kuanas(&c));
        if ours {
            let _ = fs::remove_file(&path);
        }
    }
}

/// What a request gets back.
struct Response {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Response {
    fn plain(status: u16, text: &str) -> Response {
        Response { status, headers: vec![("Content-Type", "text/plain".into())], body: text.as_bytes().to_vec() }
    }

    fn json(value: &Value) -> Response {
        Response { status: 200, headers: vec![("Content-Type", "application/json".into())], body: value.to_string().into_bytes() }
    }
}

/// One connection, from its first request until the client closes it, the
/// server stops, or it becomes an event stream and that ends.
fn serve(stream: TcpStream, shared: &Arc<Shared>) {
    if shared.stopped.load(Ordering::Relaxed) {
        return;
    }
    let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let (Ok(read), Ok(control)) = (stream.try_clone(), stream.try_clone()) else { return };
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    shared.connections.lock().unwrap_or_else(|e| e.into_inner()).push((id, control));
    // Stopped while this one connected: `Drop for Server` may have ended the
    // connections before it was one of them.
    if shared.stopped.load(Ordering::Relaxed) {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    let mut reader = BufReader::new(read);
    let mut out = stream;
    while let Ok(Some(request)) = http::read_request(&mut reader) {
        let close = request.closes();
        let response = match check(&request, shared) {
            Some(refused) => refused,
            None => match request.method.as_str() {
                "POST" => post(&request, shared),
                "GET" => match open_stream(&request, shared) {
                    Ok(session) => {
                        events(&mut out, &session, shared);
                        break;
                    }
                    Err(refused) => refused,
                },
                "DELETE" => end_session(&request, shared),
                _ => {
                    let mut refused = Response::plain(405, "Method not allowed");
                    refused.headers.push(("Allow", "GET, POST, DELETE".into()));
                    refused
                }
            },
        };
        let headers: Vec<(&str, &str)> = response.headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
        if http::respond(&mut out, response.status, &headers, &response.body, close).is_err() || close {
            break;
        }
    }
    shared.connections.lock().unwrap_or_else(|e| e.into_inner()).retain(|(c, _)| *c != id);
}

/// Refuses requests that aren't for this server from Gemini CLI: the token,
/// and against pages in a browser reaching the port (DNS rebinding), the
/// Host as the CLI names it and no Origin, as Gemini's VS Code companion does.
fn check(request: &http::Request, shared: &Shared) -> Option<Response> {
    let host = request.header("host").unwrap_or_default();
    let port = shared.port;
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return Some(Response::plain(403, "Invalid Host header"));
    }
    if request.header("origin").is_some() {
        return Some(Response::plain(403, "Request denied"));
    }
    let token = request.header("authorization").and_then(|v| v.strip_prefix("Bearer "));
    if !token.is_some_and(|token| same_token(token, &shared.token)) {
        return Some(Response::plain(401, "Unauthorized"));
    }
    if request.path != "/mcp" {
        return Some(Response::plain(404, "Not found"));
    }
    None
}

fn session_of(request: &http::Request) -> Option<&str> {
    request.header("mcp-session-id")
}

/// A JSON-RPC message: `initialize` starts a session, the rest need one.
fn post(request: &http::Request, shared: &Shared) -> Response {
    let Ok(message) = serde_json::from_slice::<Value>(&request.body) else {
        let mut refused = Response::json(&crate::agent_ide::error(Value::Null, -32700, "Parse error"));
        refused.status = 400;
        return refused;
    };
    let mut new_session = None;
    match session_of(request) {
        None if protocol::is_initialize(&message) => {
            let session = crate::instance::random_token();
            shared.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(session.clone(), Session::default());
            new_session = Some(session);
        }
        None => {
            let mut refused = Response::json(&crate::agent_ide::error(Value::Null, -32000, "Bad Request: no session"));
            refused.status = 400;
            return refused;
        }
        Some(session) if !shared.sessions.lock().unwrap_or_else(|e| e.into_inner()).contains_key(session) => {
            return Response::json(&crate::agent_ide::error(Value::Null, -32001, "Session not found")).with_status(404);
        }
        Some(_) => {}
    }
    let mut response = match protocol::handle(&message) {
        protocol::Incoming::Reply(reply) => Response::json(&reply),
        protocol::Incoming::Tool { id, name, arguments } => {
            let answered = crate::agent_ide::ask_window(&shared.requests, name, arguments);
            Response::json(&crate::agent_ide::tool_result(id, crate::agent_ide::wait_for(answered)))
        }
        protocol::Incoming::Nothing => Response { status: 202, headers: Vec::new(), body: Vec::new() },
    };
    if let Some(session) = new_session {
        response.headers.push(("mcp-session-id", session));
    }
    response
}

impl Response {
    fn with_status(mut self, status: u16) -> Response {
        self.status = status;
        self
    }
}

/// The session a GET opens an event stream for.
fn open_stream(request: &http::Request, shared: &Shared) -> Result<String, Response> {
    if !request.accepts("text/event-stream") {
        return Err(Response::plain(405, "Not an event stream request"));
    }
    match session_of(request) {
        Some(session) if shared.sessions.lock().unwrap_or_else(|e| e.into_inner()).contains_key(session) => Ok(session.to_string()),
        Some(_) => Err(Response::plain(404, "Session not found")),
        None => Err(Response::plain(400, "No session")),
    }
}

/// Writes a session's notifications to its stream until it ends: the client
/// went (its reader sees the connection close), the server stopped, or the
/// session opened another stream.
fn events(out: &mut TcpStream, session: &str, shared: &Shared) {
    if http::start_events(out, session).is_err() {
        return;
    }
    let (outgoing, waiting) = std_mpsc::channel::<Option<String>>();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let replaced = {
        let mut sessions = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = sessions.get_mut(session) else { return };
        entry.stream.replace((id, outgoing.clone()))
    };
    match replaced {
        Some((_, old)) => {
            let _ = old.send(None);
        }
        None => shared.tell(Request::Connected),
    }
    // The client sends nothing more on this connection: a read ends only
    // when it closes it.
    let _ = out.set_read_timeout(None);
    if let Ok(mut socket) = out.try_clone() {
        let closed = outgoing.clone();
        let _ = std::thread::Builder::new().name("gemini ide stream".into()).spawn(move || {
            let mut buf = [0u8; 256];
            while matches!(io::Read::read(&mut socket, &mut buf), Ok(n) if n > 0) {}
            let _ = closed.send(None);
        });
    }
    let context = shared.state.lock().unwrap_or_else(|e| e.into_inner()).context.clone();
    if let Some(context) = context {
        let _ = outgoing.send(Some(crate::agent_ide::notification("ide/contextUpdate", context).to_string()));
    }
    drop(outgoing);
    loop {
        let written = match waiting.recv_timeout(KEEPALIVE) {
            Ok(Some(text)) => http::event(out, Some(&text)),
            Err(std_mpsc::RecvTimeoutError::Timeout) => http::event(out, None),
            Ok(None) | Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if written.is_err() || shared.stopped.load(Ordering::Relaxed) {
            break;
        }
    }
    let _ = http::end_events(out);
    let _ = out.shutdown(std::net::Shutdown::Both);
    let mut sessions = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = sessions.get_mut(session)
        && entry.stream.as_ref().is_some_and(|(stream, _)| *stream == id)
    {
        entry.stream = None;
        drop(sessions);
        shared.tell(Request::Disconnected);
    }
}

fn end_session(request: &http::Request, shared: &Shared) -> Response {
    let removed = session_of(request).and_then(|s| shared.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(s));
    match removed {
        Some(session) => {
            // Its stream's thread finds it gone and tells nobody.
            if let Some((_, outgoing)) = session.stream {
                let _ = outgoing.send(None);
                shared.tell(Request::Disconnected);
            }
            Response { status: 200, headers: Vec::new(), body: Vec::new() }
        }
        None => Response::plain(404, "Session not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
    use serde_json::json;
    use std::io::{BufRead as _, Read as _, Write as _};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kuana-gemini-ide-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// A client like Gemini CLI's: one connection per request or stream.
    struct Client {
        port: u16,
        token: String,
        session: Option<String>,
    }

    struct Answer {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Answer {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap()
        }

        fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
        }
    }

    fn read_head(reader: &mut BufReader<TcpStream>) -> (u16, Vec<(String, String)>) {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let status = line.split(' ').nth(1).unwrap().parse().unwrap();
        let mut headers = Vec::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                return (status, headers);
            }
            let (name, value) = line.split_once(':').unwrap();
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    impl Client {
        fn connect(&self, method: &str, extra: &[(&str, &str)], body: &str) -> BufReader<TcpStream> {
            let stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut head = format!("{method} /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\n", self.port, body.len());
            for (name, value) in extra {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            if let Some(session) = &self.session {
                head.push_str(&format!("mcp-session-id: {session}\r\n"));
            }
            head.push_str(&format!("Authorization: Bearer {}\r\n\r\n{body}", self.token));
            (&stream).write_all(head.as_bytes()).unwrap();
            BufReader::new(stream)
        }

        fn send(&self, method: &str, extra: &[(&str, &str)], body: &str) -> Answer {
            let mut reader = self.connect(method, extra, body);
            let (status, headers) = read_head(&mut reader);
            let length: usize = headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("content-length")).map_or(0, |(_, v)| v.parse().unwrap());
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            Answer { status, headers, body }
        }

        fn post(&self, message: Value) -> Answer {
            self.send("POST", &[("Accept", "application/json, text/event-stream"), ("Content-Type", "application/json")], &message.to_string())
        }

        /// Opens the session's event stream.
        fn stream(&self) -> BufReader<TcpStream> {
            let mut reader = self.connect("GET", &[("Accept", "text/event-stream")], "");
            let (status, headers) = read_head(&mut reader);
            assert_eq!(status, 200);
            assert!(headers.iter().any(|(n, v)| n == "Content-Type" && v == "text/event-stream"));
            reader
        }
    }

    /// The next message on an event stream (comments skipped).
    fn next_event(reader: &mut BufReader<TcpStream>) -> Value {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).unwrap();
            let size = usize::from_str_radix(size.trim(), 16).unwrap();
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).unwrap();
            let text = String::from_utf8(chunk).unwrap();
            if let Some(data) = text.lines().find_map(|l| l.strip_prefix("data: ")) {
                return serde_json::from_str(data).unwrap();
            }
        }
    }

    fn request(id: u64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[test]
    fn writes_a_private_discovery_file_and_removes_it() {
        let dir = temp_dir("file");
        // A Kuana that crashed, and another IDE's file.
        fs::create_dir_all(&dir).unwrap();
        let gone = protocol::discovery(1, &[], "x");
        fs::write(dir.join("gemini-ide-server-2147483000-1.json"), gone.to_string()).unwrap();
        fs::write(dir.join("gemini-ide-server-2147483000-2.json"), json!({ "port": 2 }).to_string()).unwrap();
        let (server, _requests) = Server::start(vec![PathBuf::from("/work/app")], &dir).unwrap();
        assert_eq!(dir.join("gemini-ide-server-2147483000-1.json").exists(), !cfg!(unix));
        assert!(dir.join("gemini-ide-server-2147483000-2.json").exists());
        let file = dir.join(format!("gemini-ide-server-{}-{}.json", std::process::id(), server.port()));
        let content: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(content["port"], server.port());
        assert_eq!(content["workspacePath"], "/work/app");
        assert_eq!(content["ideInfo"]["name"], "kuana");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        server.set_folders(vec![PathBuf::from("/work/app"), PathBuf::from("/work/lib")]);
        let content: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(content["workspacePath"], protocol::workspace_path(&[PathBuf::from("/work/app"), PathBuf::from("/work/lib")]));
        drop(server);
        assert!(!file.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminals_get_the_windows_port_and_token() {
        let dir = temp_dir("env");
        let (server, _requests) = Server::start(vec![PathBuf::from("/kuana-gemini-env/app")], &dir).unwrap();
        let env = terminal_env(Path::new("/kuana-gemini-env/app/src"));
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(get(PORT_VARIABLE), Some(server.port().to_string()));
        assert_eq!(get(PID_VARIABLE), Some(std::process::id().to_string()));
        assert_eq!(get(WORKSPACE_VARIABLE).as_deref(), Some("/kuana-gemini-env/app"));
        assert_eq!(get(TOKEN_VARIABLE), Some(server.shared.token.clone()));
        assert!(terminal_env(Path::new("/elsewhere")).is_empty());
        drop(server);
        assert!(terminal_env(Path::new("/kuana-gemini-env/app")).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_requests_without_the_token_or_from_elsewhere() {
        let dir = temp_dir("auth");
        let (server, _requests) = Server::start(Vec::new(), &dir).unwrap();
        let initialize = request(1, "initialize", json!({}));
        let wrong = Client { port: server.port(), token: "nope".into(), session: None };
        assert_eq!(wrong.post(initialize.clone()).status, 401);
        let client = Client { port: server.port(), token: server.shared.token.clone(), session: None };
        let from_page = client.send("POST", &[("Origin", "https://example.com")], &initialize.to_string());
        assert_eq!(from_page.status, 403);
        // Without initializing first, there's no session.
        assert_eq!(client.post(request(2, "tools/list", json!({}))).status, 400);
        let stale = Client { session: Some("gone".into()), ..client };
        assert_eq!(stale.post(request(2, "tools/list", json!({}))).status, 404);
        assert!(!server.has_clients());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn talks_mcp_and_streams_notifications() {
        let dir = temp_dir("talk");
        let (server, mut requests) = Server::start(vec![PathBuf::from("/work/app")], &dir).unwrap();
        server.set_context(json!({ "workspaceState": { "openFiles": [] } }));
        let mut client = Client { port: server.port(), token: server.shared.token.clone(), session: None };

        let initialized = client.post(request(1, "initialize", json!({ "protocolVersion": "2025-06-18" })));
        assert_eq!(initialized.status, 200);
        assert_eq!(initialized.json()["result"]["serverInfo"]["name"], "kuana");
        client.session = Some(initialized.header("mcp-session-id").unwrap().to_string());
        assert_eq!(client.post(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).status, 202);

        // The stream starts with the last context; then the window hears of it.
        let mut stream = client.stream();
        assert_eq!(next_event(&mut stream)["method"], "ide/contextUpdate");
        assert!(matches!(futures::executor::block_on(requests.next()), Some(Request::Connected)));
        assert!(server.has_clients());

        let tools = client.post(request(2, "tools/list", json!({}))).json();
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 2);

        // A tool call waits for the window's answer.
        let port = server.port();
        let (token, session) = (client.token.clone(), client.session.clone());
        let asking = std::thread::spawn(move || {
            let client = Client { port, token, session };
            client.post(request(3, "tools/call", json!({ "name": "openDiff", "arguments": { "filePath": "/work/app/a.rs", "newContent": "x" } }))).json()
        });
        let Some(Request::Tool(call)) = futures::executor::block_on(requests.next()) else { panic!("no tool call") };
        assert_eq!((call.name.as_str(), &call.arguments["filePath"]), ("openDiff", &json!("/work/app/a.rs")));
        call.answer(Ok(Vec::new()));
        let answer = asking.join().unwrap();
        assert_eq!((answer["id"].clone(), answer["result"]["content"].clone()), (json!(3), json!([])));

        // Notifications reach the stream; the same context twice goes once.
        server.broadcast("ide/diffAccepted", json!({ "filePath": "/work/app/a.rs", "content": "x" }));
        assert_eq!(next_event(&mut stream)["params"]["content"], "x");
        server.set_context(json!({ "workspaceState": { "openFiles": [] } }));
        server.set_context(json!({ "workspaceState": { "openFiles": [{ "path": "/work/app/a.rs" }] } }));
        assert_eq!(next_event(&mut stream)["params"]["workspaceState"]["openFiles"][0]["path"], "/work/app/a.rs");

        // The client going is heard at once.
        drop(stream);
        assert!(matches!(futures::executor::block_on(requests.next()), Some(Request::Disconnected)));
        assert!(!server.has_clients());

        // A new stream for the session, then the server stops and ends it.
        let mut stream = client.stream();
        assert!(matches!(futures::executor::block_on(requests.next()), Some(Request::Connected)));
        drop(server);
        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest);
        assert!(matches!(futures::executor::block_on(requests.next()), Some(Request::Disconnected)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ending_a_session_forgets_it() {
        let dir = temp_dir("end");
        let (server, _requests) = Server::start(Vec::new(), &dir).unwrap();
        let mut client = Client { port: server.port(), token: server.shared.token.clone(), session: None };
        let initialized = client.post(request(1, "initialize", json!({})));
        client.session = Some(initialized.header("mcp-session-id").unwrap().to_string());
        assert_eq!(client.send("DELETE", &[], "").status, 200);
        assert_eq!(client.post(request(2, "tools/list", json!({}))).status, 404);
        let _ = fs::remove_dir_all(&dir);
    }
}
