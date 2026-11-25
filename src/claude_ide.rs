//! Claude Code's editor integration (its `/ide` command), as its VS Code and
//! JetBrains plugins offer it: Claude Code running in a terminal sees the
//! selection in Kuana, opens files here, reads the problems Kuana shows and
//! proposes edits as diffs to accept or reject here.
//! See `plans/claude-code-bridge.md`.
//!
//! Each project window runs a [`Server`]: a WebSocket server on a loopback
//! port speaking MCP ([`protocol`]), advertised to `claude` by a lock file,
//! `~/.claude/ide/<port>.lock` (`$CLAUDE_CONFIG_DIR/ide` when set), holding
//! the project's folders and a random token Claude must send back. Shells a
//! window starts get [`terminal_env`], so `claude` there connects to that
//! window without asking; one started elsewhere offers the window whose
//! folders hold its working directory.
//!
//! Threads, like `askpass`: one accepting connections and one per client
//! (Claude Code keeps one open per session). Tool calls that need the window
//! go to it as [`Request`]s, each waiting on its own thread so a slow answer
//! holds up nothing else. What this shares with Gemini CLI's connection is in
//! `agent_ide.rs`.
//!
//! A remote project's window (SSH, WSL, a container) runs its server here
//! too, without a lock file: `kuana-server` listens on the host's loopback
//! and relays each connection to it (`RemoteHost::listen_back`), and writes
//! the lock file there; the host's terminals get that port ([`Advertised`]).
//! Claude on the host names the host's paths (`agent_ide::PathMap`).

mod protocol;

use std::{
    fs, io,
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc as std_mpsc,
    },
    time::Duration,
};

use futures::channel::mpsc;
use ropey::Rope;
use serde_json::{Value, json};
use tungstenite::{
    Message,
    handshake::server::{ErrorResponse, Request as Handshake, Response},
    http::{HeaderValue, StatusCode, header::SEC_WEBSOCKET_PROTOCOL},
};

pub use crate::agent_ide::{Request, ToolCall};

use crate::agent_ide::{PathMap, Routes, same_token};
use crate::lint::{Diagnostic, Severity};
use crate::lsp::protocol::{Encoding, offset_to_position};

/// The header Claude Code sends the lock file's token in.
const AUTH_HEADER: &str = "x-claude-code-ide-authorization";
/// The variables that point a terminal's `claude` at a window.
const PORT_VARIABLE: &str = "CLAUDE_CODE_SSE_PORT";
const ENABLE_VARIABLE: &str = "ENABLE_IDE_INTEGRATION";
/// How long a connection may take to say who it is.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a write may wait for a client that stopped reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Where Claude Code looks for IDEs' lock files.
pub fn lock_dir() -> Option<PathBuf> {
    let config = std::env::var("CLAUDE_CONFIG_DIR").ok();
    kuana_core::claude_lock::lock_dir(config.as_deref(), crate::clone_dialog::home_dir().as_deref())
}

/// Whether Claude Code has run on this machine (its config folder exists),
/// so people without it don't get a folder made in their home.
pub fn claude_installed() -> bool {
    lock_dir().is_some_and(|dir| kuana_core::claude_lock::installed(&dir))
}

/// The servers running: which one a new terminal's `claude` should reach
/// (`terminal_env`).
static SERVERS: Routes = Routes::new();
static NEXT_CLIENT: AtomicU64 = AtomicU64::new(0);

/// The variables for a shell started in `cwd`: the port of the window whose
/// folder holds it (the innermost, with nested projects open), if any.
pub fn terminal_env(cwd: &Path) -> Vec<(String, String)> {
    SERVERS.env_for(cwd)
}

fn env_for(port: u16) -> Vec<(String, String)> {
    vec![(PORT_VARIABLE.to_string(), port.to_string()), (ENABLE_VARIABLE.to_string(), "true".to_string())]
}

/// A port terminals in some folders point `claude` at (a remote project's
/// host's, with the mount's folders), until dropped.
pub struct Advertised {
    route: u64,
}

impl Advertised {
    pub fn new(port: u16, folders: Vec<PathBuf>) -> Advertised {
        let advertised = Advertised { route: Routes::key() };
        advertised.set_folders(port, folders);
        advertised
    }

    pub fn set_folders(&self, port: u16, folders: Vec<PathBuf>) {
        SERVERS.set(self.route, folders, env_for(port));
    }
}

impl Drop for Advertised {
    fn drop(&mut self) {
        SERVERS.remove(self.route);
    }
}

/// A window's server. Dropping it stops it and removes its lock file.
pub struct Server {
    shared: Arc<Shared>,
}

struct Shared {
    port: u16,
    /// Its route for terminals (`terminal_env`), when it has a lock file.
    route: u64,
    token: String,
    /// Its lock file, when it has one (a local window's).
    lock_path: Option<PathBuf>,
    state: Mutex<protocol::State>,
    clients: Mutex<Vec<Client>>,
    requests: mpsc::UnboundedSender<Request>,
    stopped: AtomicBool,
}

/// A connected client: where its messages go, and its socket, to end it
/// when the server stops.
struct Client {
    id: u64,
    outgoing: std_mpsc::Sender<Option<String>>,
    socket: TcpStream,
}

impl Server {
    /// Listens on a free loopback port and, with `lock_dir`, writes the lock
    /// file there and has terminals in `folders` point at it (a local
    /// window's; a remote one's is advertised on its host); the window's
    /// requests come out of the receiver. `folders` are as Claude names them.
    pub fn start(folders: Vec<PathBuf>, lock_dir: Option<&Path>) -> io::Result<(Server, mpsc::UnboundedReceiver<Request>)> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        if let Some(dir) = lock_dir {
            kuana_core::claude_lock::remove_stale(dir);
        }
        let (requests, received) = mpsc::unbounded();
        let shared = Arc::new(Shared {
            port,
            route: Routes::key(),
            token: crate::instance::random_token(),
            lock_path: lock_dir.map(|dir| dir.join(format!("{port}.lock"))),
            state: Mutex::new(protocol::State { folders, selection: None }),
            clients: Mutex::new(Vec::new()),
            requests,
            stopped: AtomicBool::new(false),
        });
        shared.write_lock()?;
        shared.register();
        let accepting = shared.clone();
        let started = std::thread::Builder::new().name("claude ide".into()).spawn(move || {
            for stream in listener.incoming() {
                if accepting.stopped.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let shared = accepting.clone();
                let _ = std::thread::Builder::new().name("claude ide client".into()).spawn(move || serve(stream, &shared));
            }
        });
        let server = Server { shared };
        // Dropped on failure: the lock file goes with it.
        started?;
        Ok((server, received))
    }

    /// Where it listens (loopback).
    pub fn address(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.shared.port))
    }

    #[cfg(test)]
    fn port(&self) -> u16 {
        self.shared.port
    }

    /// What Claude must send to connect (the lock file's `authToken`).
    pub fn token(&self) -> &str {
        &self.shared.token
    }

    /// Whether Claude is connected now.
    pub fn has_clients(&self) -> bool {
        !self.shared.clients.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// The project's folders changed.
    pub fn set_folders(&self, folders: Vec<PathBuf>) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.folders == folders {
            return;
        }
        state.folders = folders;
        drop(state);
        if let Err(err) = self.shared.write_lock() {
            eprintln!("couldn't update Claude Code's lock file: {err}");
        }
        self.shared.register();
    }

    /// The active editor's selection (`selection_payload`): Claude hears of
    /// it when it changed.
    pub fn set_selection(&self, selection: Value) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.selection.as_ref() == Some(&selection) {
            return;
        }
        state.selection = Some(selection.clone());
        drop(state);
        self.broadcast("selection_changed", selection);
    }

    /// Sends a notification to every connected client.
    pub fn broadcast(&self, method: &str, params: Value) {
        let text = crate::agent_ide::notification(method, params).to_string();
        for client in self.shared.clients.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            let _ = client.outgoing.send(Some(text.clone()));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let shared = &self.shared;
        shared.stopped.store(true, Ordering::Relaxed);
        if let Some(path) = &shared.lock_path {
            let _ = fs::remove_file(path);
        }
        SERVERS.remove(shared.route);
        // Wakes the accepting thread, which then sees it's stopped, and ends
        // the clients' connections.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, shared.port));
        shared.end_clients();
    }
}

impl Shared {
    /// Closes every client's socket: its reader's wait ends, and with it
    /// the writer.
    fn end_clients(&self) {
        for client in self.clients.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            let _ = client.socket.shutdown(std::net::Shutdown::Both);
        }
    }

    fn folders(&self) -> Vec<PathBuf> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).folders.clone()
    }

    /// Terminals in its folders point at it, when it has a lock file.
    fn register(&self) {
        if self.lock_path.is_some() {
            SERVERS.set(self.route, self.folders(), env_for(self.port));
        }
    }

    /// Writes the lock file, when it has one.
    fn write_lock(&self) -> io::Result<()> {
        let Some(dir) = self.lock_path.as_ref().and_then(|p| p.parent()) else { return Ok(()) };
        let folders: Vec<String> = self.folders().iter().map(|f| f.display().to_string()).collect();
        kuana_core::claude_lock::write(dir, self.port, std::process::id(), &folders, &self.token).map(|_| ())
    }
}

/// Lets a WebSocket upgrade through only with the token, and echoes the
/// subprotocol asked for: Claude's client drops a connection without it.
// The error type is tungstenite's callback's.
#[allow(clippy::result_large_err)]
fn authorize(request: &Handshake, mut response: Response, token: &str) -> Result<Response, ErrorResponse> {
    let given = request.headers().get(AUTH_HEADER).and_then(|v| v.to_str().ok());
    if !given.is_some_and(|given| same_token(given, token)) {
        let mut refused = ErrorResponse::new(Some("unauthorized".into()));
        *refused.status_mut() = StatusCode::UNAUTHORIZED;
        return Err(refused);
    }
    let asked = request.headers().get(SEC_WEBSOCKET_PROTOCOL).and_then(|v| v.to_str().ok());
    if let Some(first) = asked.and_then(|p| p.split(',').next()).map(str::trim).filter(|p| !p.is_empty())
        && let Ok(value) = HeaderValue::from_str(first)
    {
        response.headers_mut().insert(SEC_WEBSOCKET_PROTOCOL, value);
    }
    Ok(response)
}

/// A client's socket as its two threads use it: reads go straight to the
/// stream (only the reader reads), and each write goes out whole under one
/// lock, so a message the writer sends and a pong the reader answers never
/// interleave. tungstenite hands a write all the frames it has, whole.
struct Duplex {
    read: TcpStream,
    write: Arc<Mutex<TcpStream>>,
}

impl io::Read for Duplex {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read.read(buf)
    }
}

impl io::Write for Duplex {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write.lock().unwrap_or_else(|e| e.into_inner()).write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.write.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

/// One client, from its handshake until it goes or the server stops: this
/// thread waits for what Claude sends, a writer thread for what's queued
/// for Claude (answers, notifications), so neither wakes for nothing.
#[allow(clippy::result_large_err)]
fn serve(stream: TcpStream, shared: &Arc<Shared>) {
    if shared.stopped.load(Ordering::Relaxed) {
        return;
    }
    let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let (Ok(write), Ok(writer_read), Ok(control)) = (stream.try_clone(), stream.try_clone(), stream.try_clone()) else { return };
    let write = Arc::new(Mutex::new(write));
    let duplex = Duplex { read: stream, write: write.clone() };
    let Ok(mut socket) = tungstenite::accept_hdr(duplex, |request: &Handshake, response| authorize(request, response, &shared.token)) else {
        return;
    };
    // Reads now wait for Claude as long as it takes (the socket's clones
    // share the setting).
    let _ = control.set_read_timeout(None);
    // `None` ends the writer, once this thread is done.
    let (outgoing, waiting) = std_mpsc::channel::<Option<String>>();
    let mut writer = tungstenite::WebSocket::from_raw_socket(Duplex { read: writer_read, write }, tungstenite::protocol::Role::Server, None);
    let writing = std::thread::Builder::new().name("claude ide writer".into()).spawn(move || {
        while let Ok(Some(text)) = waiting.recv() {
            if writer.send(Message::text(text)).is_err() {
                break;
            }
        }
    });
    if writing.is_err() {
        return;
    }
    let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
    let selection = shared.state.lock().unwrap_or_else(|e| e.into_inner()).selection.clone();
    if let Some(selection) = selection {
        let _ = outgoing.send(Some(crate::agent_ide::notification("selection_changed", selection).to_string()));
    }
    shared.clients.lock().unwrap_or_else(|e| e.into_inner()).push(Client { id, outgoing: outgoing.clone(), socket: control });
    let _ = shared.requests.unbounded_send(Request::Connected);
    // Stopped while this one connected: `Drop for Server` may have ended the
    // clients before it was one of them.
    if shared.stopped.load(Ordering::Relaxed) {
        shared.end_clients();
    }
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let state = shared.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                match protocol::handle(text.as_str(), &state) {
                    protocol::Incoming::Reply(reply) => {
                        let _ = outgoing.send(Some(reply.to_string()));
                    }
                    protocol::Incoming::Tool { id, name, arguments } => ask_window(shared, outgoing.clone(), id, name, arguments),
                    protocol::Incoming::Nothing => {}
                }
            }
            // A ping's pong goes out at the next read; a close is answered
            // there too, and the read after it fails.
            Ok(_) => {}
            Err(_) => break,
        }
    }
    shared.clients.lock().unwrap_or_else(|e| e.into_inner()).retain(|client| client.id != id);
    let _ = outgoing.send(None);
    let _ = shared.requests.unbounded_send(Request::Disconnected);
}

/// Hands a tool call to the window and sends back its answer, waiting on a
/// thread of its own.
fn ask_window(shared: &Shared, outgoing: std_mpsc::Sender<Option<String>>, id: Value, name: String, arguments: Value) {
    let answered = crate::agent_ide::ask_window(&shared.requests, name, arguments);
    let _ = std::thread::Builder::new().name("claude ide tool".into()).spawn(move || {
        let answer = crate::agent_ide::wait_for(answered);
        let _ = outgoing.send(Some(crate::agent_ide::tool_result(id, answer).to_string()));
    });
}

/// What `selection_changed` says of a selection (`range`, bytes) in a file:
/// zero-based lines and UTF-16 columns, as VS Code counts them.
pub fn selection_payload(map: &PathMap, path: &Path, rope: &Rope, range: std::ops::Range<usize>) -> Value {
    let start = offset_to_position(rope, range.start, Encoding::Utf16);
    let end = offset_to_position(rope, range.end, Encoding::Utf16);
    let text = rope.byte_slice(range.start.min(rope.len_bytes())..range.end.min(rope.len_bytes())).to_string();
    json!({
        "text": text,
        "filePath": map.to_agent(path),
        "fileUrl": map.url(path),
        "selection": {
            "start": { "line": start.line, "character": start.character },
            "end": { "line": end.line, "character": end.character },
            "isEmpty": range.is_empty(),
        },
    })
}

/// `at_mentioned`'s params, which add a file to Claude's prompt: with a
/// selection (`range`, bytes), its zero-based lines, the line it ends at the
/// start of left out.
pub fn mention(map: &PathMap, path: &Path, rope: &Rope, range: std::ops::Range<usize>) -> Value {
    let mut params = json!({ "filePath": map.to_agent(path) });
    if !range.is_empty() {
        let (start, end) = (range.start.min(rope.len_bytes()), range.end.min(rope.len_bytes()));
        let first = rope.byte_to_line(start);
        let mut last = rope.byte_to_line(end);
        if last > first && rope.line_to_byte(last) == end {
            last -= 1;
        }
        params["lineStart"] = json!(first);
        params["lineEnd"] = json!(last);
    }
    params
}

/// What `openFile` asks to select, as Claude's own plugins read it.
#[derive(Default)]
pub struct OpenSelection<'a> {
    /// 1-based lines, `end_line` included (`start_line` alone selects it).
    pub start_line: Option<u64>,
    pub end_line: Option<u64>,
    /// From the first `start_text` to the first `end_text` after it (its
    /// end included); `start_text` alone selects itself. Wins over lines.
    pub start_text: Option<&'a str>,
    pub end_text: Option<&'a str>,
    /// The selection goes on to the end of its last line.
    pub to_line_end: bool,
}

impl OpenSelection<'_> {
    pub fn read(arguments: &Value) -> OpenSelection<'_> {
        let line = |key| arguments.get(key).and_then(Value::as_u64).filter(|l| *l > 0);
        let text = |key| arguments.get(key).and_then(Value::as_str).filter(|t| !t.is_empty());
        OpenSelection {
            start_line: line("startLine"),
            end_line: line("endLine"),
            start_text: text("startText"),
            end_text: text("endText"),
            to_line_end: arguments.get("selectToEndOfLine").and_then(Value::as_bool).unwrap_or(false),
        }
    }

    /// The bytes to select in `rope`; None selects nothing (the text isn't
    /// there, or nothing was asked).
    pub fn range(&self, rope: &Rope) -> Option<std::ops::Range<usize>> {
        let last_line = rope.len_lines().saturating_sub(1);
        let (start, end) = if let Some(start_text) = self.start_text {
            let text = rope.to_string();
            let start = text.find(start_text)?;
            let after = start + start_text.len();
            let end = match self.end_text {
                Some(end_text) => after + text[after..].find(end_text)? + end_text.len(),
                None => after,
            };
            (start, end)
        } else {
            let first = (self.start_line? as usize - 1).min(last_line);
            let last = self.end_line.map_or(first, |l| (l as usize - 1).min(last_line)).max(first);
            (rope.line_to_byte(first), line_end(rope, last))
        };
        let end = if self.to_line_end { line_end(rope, rope.byte_to_line(end)) } else { end };
        Some(start..end.max(start))
    }
}

/// Where line `line`'s text ends, before its line break.
fn line_end(rope: &Rope, line: usize) -> usize {
    let text = rope.line(line);
    let mut len = text.len_bytes();
    for ending in ['\n', '\r'] {
        if len > 0 && text.byte(len - 1) == ending as u8 {
            len -= 1;
        }
    }
    rope.line_to_byte(line) + len
}

/// One of a document's problems as `getDiagnostics` lists it.
pub fn diagnostic_json(rope: &Rope, diagnostic: &Diagnostic) -> Value {
    let start = offset_to_position(rope, diagnostic.range.start, Encoding::Utf16);
    let end = offset_to_position(rope, diagnostic.range.end, Encoding::Utf16);
    let mut value = json!({
        "message": &*diagnostic.message,
        "severity": severity_name(diagnostic.severity),
        "range": { "start": start.to_json(), "end": end.to_json() },
    });
    if !diagnostic.rule.is_empty() {
        value["code"] = json!(&*diagnostic.rule);
    }
    value
}

/// A language server's diagnostic (LSP's form, for a file that isn't open)
/// as `getDiagnostics` lists it: the severity named, the rest as sent.
pub fn lsp_diagnostic_json(diagnostic: &Value, server: &str) -> Value {
    let severity = match diagnostic.get("severity").and_then(Value::as_u64) {
        Some(2) => Severity::Warning,
        Some(3) => Severity::Info,
        Some(4) => Severity::Hint,
        _ => Severity::Error,
    };
    let mut value = json!({
        "message": diagnostic.get("message").cloned().unwrap_or_default(),
        "severity": severity_name(severity),
        "range": diagnostic.get("range").cloned().unwrap_or_default(),
        "source": diagnostic.get("source").and_then(Value::as_str).unwrap_or(server),
    });
    if let Some(code) = diagnostic.get("code") {
        value["code"] = code.clone();
    }
    value
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "Error",
        Severity::Warning => "Warning",
        Severity::Info => "Information",
        Severity::Hint => "Hint",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
    use tungstenite::client::IntoClientRequest as _;

    fn temp_lock_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kuana-claude-ide-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn lock_of(server: &Server) -> Value {
        serde_json::from_slice(&fs::read(server.shared.lock_path.as_ref().unwrap()).unwrap()).unwrap()
    }

    type Client = tungstenite::WebSocket<TcpStream>;

    fn connect(server: &Server, token: &str) -> Result<Client, tungstenite::Error> {
        let mut request = format!("ws://127.0.0.1:{}", server.port()).into_client_request().unwrap();
        request.headers_mut().insert(AUTH_HEADER, HeaderValue::from_str(token).unwrap());
        request.headers_mut().insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("mcp"));
        let stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let (socket, response) = tungstenite::client(request, stream).map_err(|e| match e {
            tungstenite::HandshakeError::Failure(e) => e,
            tungstenite::HandshakeError::Interrupted(_) => tungstenite::Error::ConnectionClosed,
        })?;
        assert_eq!(response.headers().get(SEC_WEBSOCKET_PROTOCOL).unwrap(), "mcp");
        Ok(socket)
    }

    fn send(client: &mut Client, value: Value) {
        client.send(Message::text(value.to_string())).unwrap();
    }

    fn receive(client: &mut Client) -> Value {
        loop {
            if let Message::Text(text) = client.read().unwrap() {
                return serde_json::from_str(text.as_str()).unwrap();
            }
        }
    }

    #[test]
    fn writes_a_private_lock_file_and_removes_it() {
        let dir = temp_lock_dir("lock");
        let (server, _requests) = Server::start(vec![PathBuf::from("/work/app")], Some(&dir)).unwrap();
        let lock = lock_of(&server);
        assert_eq!(lock["ideName"], "Kuana");
        assert_eq!(lock["transport"], "ws");
        assert_eq!(lock["pid"], std::process::id());
        assert_eq!(lock["workspaceFolders"], json!(["/work/app"]));
        assert_eq!(lock["authToken"].as_str().unwrap().len(), server.shared.token.len());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(fs::metadata(server.shared.lock_path.as_ref().unwrap()).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        server.set_folders(vec![PathBuf::from("/work/app"), PathBuf::from("/work/lib")]);
        assert_eq!(lock_of(&server)["workspaceFolders"], json!(["/work/app", "/work/lib"]));
        let path = server.shared.lock_path.clone().unwrap();
        drop(server);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminals_reach_the_window_holding_their_folder() {
        let dir = temp_lock_dir("env");
        let (outer, _a) = Server::start(vec![PathBuf::from("/kuana-env-test/app")], Some(&dir)).unwrap();
        let (inner, _b) = Server::start(vec![PathBuf::from("/kuana-env-test/app/sub")], Some(&dir)).unwrap();
        let port_for = |cwd: &str| terminal_env(Path::new(cwd)).into_iter().find(|(k, _)| k == PORT_VARIABLE).map(|(_, v)| v);
        assert_eq!(port_for("/kuana-env-test/app/src"), Some(outer.port().to_string()));
        assert_eq!(port_for("/kuana-env-test/app/sub/x"), Some(inner.port().to_string()));
        assert_eq!(port_for("/kuana-env-test/elsewhere"), None);
        assert!(terminal_env(Path::new("/kuana-env-test/app")).contains(&(ENABLE_VARIABLE.to_string(), "true".to_string())));
        drop(inner);
        assert_eq!(port_for("/kuana-env-test/app/sub/x"), Some(outer.port().to_string()));
        drop(outer);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_a_client_without_the_token() {
        let dir = temp_lock_dir("auth");
        let (server, _requests) = Server::start(Vec::new(), Some(&dir)).unwrap();
        assert!(connect(&server, "not-the-token").is_err());
        assert!(!server.has_clients());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn talks_mcp_and_hands_tools_to_the_window() {
        let dir = temp_lock_dir("talk");
        let (server, mut requests) = Server::start(vec![PathBuf::from("/work/app")], Some(&dir)).unwrap();
        server.set_selection(json!({ "text": "fn main" }));
        let mut client = connect(&server, &server.shared.token.clone()).unwrap();
        // The last selection comes first, then the window hears of the client.
        let selection = receive(&mut client);
        assert_eq!((selection["method"].as_str(), selection["params"]["text"].as_str()), (Some("selection_changed"), Some("fn main")));
        assert!(matches!(futures::executor::block_on(requests.next()), Some(Request::Connected)));
        assert!(server.has_clients());

        send(&mut client, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } }));
        assert_eq!(receive(&mut client)["result"]["serverInfo"]["name"], "kuana");

        send(&mut client, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "checkDocumentDirty", "arguments": { "filePath": "/work/app/a.rs" } } }));
        let Some(Request::Tool(call)) = futures::executor::block_on(requests.next()) else { panic!("no tool call") };
        assert_eq!((call.name.as_str(), &call.arguments["filePath"]), ("checkDocumentDirty", &json!("/work/app/a.rs")));
        call.answer(Ok(vec![json!({ "isDirty": true }).to_string()]));
        let answer = receive(&mut client);
        assert_eq!(answer["id"], 2);
        assert_eq!(answer["result"]["content"][0]["text"], json!({ "isDirty": true }).to_string());

        // A ping is answered while the writer may be writing too.
        client.send(Message::Ping(vec![1, 2].into())).unwrap();
        let pong = loop {
            match client.read().unwrap() {
                Message::Pong(data) => break data,
                Message::Text(_) => continue,
                other => panic!("expected a pong, got {other:?}"),
            }
        };
        assert_eq!(&pong[..], &[1, 2]);

        // A new selection is pushed; the same one again isn't.
        server.set_selection(json!({ "text": "fn main" }));
        server.set_selection(json!({ "text": "let x" }));
        assert_eq!(receive(&mut client)["params"]["text"], "let x");

        drop(server);
        // The server closes the connection, and the window hears the client went.
        let closed = (0..100).any(|_| matches!(client.read(), Err(_) | Ok(Message::Close(_))));
        assert!(closed);
        assert!(matches!(futures::executor::block_on(requests.next()), Some(Request::Disconnected)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn selections_are_zero_based_with_utf16_columns() {
        let rope = Rope::from_str("fn main() {}\nlet é = \"😀x\";\n");
        let line = "fn main() {}\n".len();
        let start = line + "let é = \"".len();
        let payload = selection_payload(&PathMap::default(), Path::new("/work/a.rs"), &rope, start..start + "😀x".len());
        assert_eq!(payload["text"], "😀x");
        assert_eq!(payload["selection"]["start"], json!({ "line": 1, "character": 9 }));
        assert_eq!(payload["selection"]["end"], json!({ "line": 1, "character": 12 }));
        assert_eq!(payload["selection"]["isEmpty"], false);
        assert_eq!(selection_payload(&PathMap::default(), Path::new("/work/a.rs"), &rope, 3..3)["selection"]["isEmpty"], true);
    }

    #[test]
    fn mentions_name_the_selected_lines() {
        let rope = Rope::from_str("one\ntwo\nthree\n");
        let path = Path::new("/work/a.txt");
        assert_eq!(mention(&PathMap::default(), path, &rope, 2..2), json!({ "filePath": "/work/a.txt" }));
        assert_eq!(mention(&PathMap::default(), path, &rope, 4..6), json!({ "filePath": "/work/a.txt", "lineStart": 1, "lineEnd": 1 }));
        // Whole lines, the selection ending at the next line's start.
        assert_eq!(mention(&PathMap::default(), path, &rope, 0..8), json!({ "filePath": "/work/a.txt", "lineStart": 0, "lineEnd": 1 }));
        assert_eq!(mention(&PathMap::default(), path, &rope, 2..10)["lineEnd"], 2);
    }

    #[test]
    fn a_remote_windows_server_writes_no_lock_file_here() {
        let dir = temp_lock_dir("remote");
        let (server, _requests) = Server::start(vec![PathBuf::from("/home/me/app")], None).unwrap();
        assert!(!dir.exists() && server.shared.lock_path.is_none());
        assert!(terminal_env(Path::new("/home/me/app")).is_empty());
        // Its host's port is advertised to that host's terminals, by the mount's paths.
        let mount = kuana_core::fs::mount_root("box-env-test");
        let advertised = Advertised::new(43210, vec![mount.join("home/me/app")]);
        assert!(terminal_env(&mount.join("home/me/app/src")).contains(&(PORT_VARIABLE.to_string(), "43210".to_string())));
        assert!(terminal_env(Path::new("/home/me/app/src")).is_empty());
        drop(advertised);
        assert!(terminal_env(&mount.join("home/me/app/src")).is_empty());
        let mut client = connect(&server, server.token()).unwrap();
        send(&mut client, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "getWorkspaceFolders" } }));
        let folders: Value = serde_json::from_str(receive(&mut client)["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(folders["rootPath"], "/home/me/app");
    }

    #[test]
    fn payloads_name_a_remote_projects_files_by_the_hosts_paths() {
        let mount = kuana_core::fs::mount_root("box");
        let map = PathMap::new(Some(mount.clone()));
        let file = mount.join("home").join("me").join("a b.rs");
        let rope = Rope::from_str("one\ntwo\n");
        let payload = selection_payload(&map, &file, &rope, 4..7);
        assert_eq!((payload["filePath"].as_str(), payload["fileUrl"].as_str()), (Some("/home/me/a b.rs"), Some("file:///home/me/a%20b.rs")));
        assert_eq!(mention(&map, &file, &rope, 0..0)["filePath"], "/home/me/a b.rs");
    }

    #[test]
    fn open_file_selects_lines_or_text() {
        let rope = Rope::from_str("one\r\ntwo three\nfour\n");
        let select = |arguments: Value| OpenSelection::read(&arguments).range(&rope).map(|r| rope.byte_slice(r).to_string());
        assert_eq!(select(json!({})), None);
        assert_eq!(select(json!({ "startLine": 2 })), Some("two three".into()));
        assert_eq!(select(json!({ "startLine": 1, "endLine": 2 })), Some("one\r\ntwo three".into()));
        // Past the end: the last line (empty, after the final break).
        assert_eq!(select(json!({ "startLine": 3, "endLine": 99 })), Some("four\n".into()));
        assert_eq!(select(json!({ "startText": "two" })), Some("two".into()));
        assert_eq!(select(json!({ "startText": "o", "endText": "e" })), Some("one".into()));
        assert_eq!(select(json!({ "startText": "two", "selectToEndOfLine": true })), Some("two three".into()));
        assert_eq!(select(json!({ "startText": "two", "endText": "missing" })), None);
        assert_eq!(select(json!({ "startText": "absent", "startLine": 1 })), None);
    }

    #[test]
    fn diagnostics_name_their_severity() {
        let rope = Rope::from_str("let x = 1;\n");
        let diagnostic = Diagnostic {
            range: 4..5,
            severity: Severity::Warning,
            message: "unused".into(),
            rule: "no-unused-vars".into(),
            unnecessary: true,
        };
        let value = diagnostic_json(&rope, &diagnostic);
        assert_eq!(value["severity"], "Warning");
        assert_eq!(value["code"], "no-unused-vars");
        assert_eq!(value["range"]["start"], json!({ "line": 0, "character": 4 }));
        let published = json!({ "message": "bad", "severity": 1, "range": { "start": { "line": 2, "character": 0 } } });
        let value = lsp_diagnostic_json(&published, "rust-analyzer");
        assert_eq!((value["severity"].as_str(), value["source"].as_str()), (Some("Error"), Some("rust-analyzer")));
    }
}
