//! Claude Code's editor integration (its `/ide` command), as its VS Code and
//! JetBrains plugins offer it: Claude Code running in a terminal sees the
//! selection in Kuana, opens files here and reads the problems Kuana shows.
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
//! holds up nothing else.

mod protocol;

use std::{
    fs,
    io::{self, Write as _},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc as std_mpsc,
    },
    time::Duration,
};

use futures::channel::{mpsc, oneshot};
use ropey::Rope;
use serde_json::{Value, json};
use tungstenite::{
    Message,
    handshake::server::{ErrorResponse, Request as Handshake, Response},
    http::{HeaderValue, StatusCode, header::SEC_WEBSOCKET_PROTOCOL},
};

pub use protocol::{file_url, path_of};

use crate::lint::{Diagnostic, Severity};
use crate::lsp::protocol::{Encoding, offset_to_position};

/// The header Claude Code sends the lock file's token in.
const AUTH_HEADER: &str = "x-claude-code-ide-authorization";
/// The variables that point a terminal's `claude` at a window.
const PORT_VARIABLE: &str = "CLAUDE_CODE_SSE_PORT";
const ENABLE_VARIABLE: &str = "ENABLE_IDE_INTEGRATION";
/// How Claude Code lists Kuana in `/ide`.
const IDE_NAME: &str = "Kuana";
/// How long a connection may take to say who it is.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a connection looks for something to send while waiting for
/// Claude: answers from the window and notifications wait at most this.
const TICK: Duration = Duration::from_millis(50);

/// Where Claude Code looks for IDEs' lock files.
pub fn lock_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir).join("ide")),
        None => crate::clone_dialog::home_dir().map(|home| home.join(".claude").join("ide")),
    }
}

/// Whether Claude Code has run on this machine (its config folder exists),
/// so people without it don't get a folder made in their home.
pub fn claude_installed() -> bool {
    lock_dir().and_then(|dir| dir.parent().map(Path::is_dir)).unwrap_or(false)
}

/// Something the window answers.
pub enum Request {
    /// A client connected: it should hear the current selection.
    Connected,
    Tool(ToolCall),
}

/// A tool call the window answers (`protocol::WINDOW_TOOLS`).
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    answer: oneshot::Sender<Result<Vec<String>, String>>,
}

impl ToolCall {
    /// Text items (most of them JSON), or why it failed.
    pub fn answer(self, answer: Result<Vec<String>, String>) {
        let _ = self.answer.send(answer);
    }
}

/// The servers running, by port, with their windows' folders: which one a
/// new terminal's `claude` should reach (`terminal_env`).
static SERVERS: Mutex<Vec<(u16, Vec<PathBuf>)>> = Mutex::new(Vec::new());
static NEXT_CLIENT: AtomicU64 = AtomicU64::new(0);

/// The variables for a shell started in `cwd`: the port of the window whose
/// folder holds it (the innermost, with nested projects open), if any.
pub fn terminal_env(cwd: &Path) -> Vec<(String, String)> {
    let servers = SERVERS.lock().unwrap_or_else(|e| e.into_inner());
    let best = servers
        .iter()
        .filter_map(|(port, folders)| folders.iter().filter(|f| cwd.starts_with(f)).map(|f| f.as_os_str().len()).max().map(|depth| (depth, *port)))
        .max();
    best.map(|(_, port)| vec![(PORT_VARIABLE.to_string(), port.to_string()), (ENABLE_VARIABLE.to_string(), "true".to_string())])
        .unwrap_or_default()
}

/// A window's server. Dropping it stops it and removes its lock file.
pub struct Server {
    shared: Arc<Shared>,
}

struct Shared {
    port: u16,
    token: String,
    lock_path: PathBuf,
    state: Mutex<protocol::State>,
    clients: Mutex<Vec<(u64, std_mpsc::Sender<String>)>>,
    requests: mpsc::UnboundedSender<Request>,
    stopped: AtomicBool,
}

impl Server {
    /// Listens on a free loopback port and writes the lock file into
    /// `lock_dir`; the window's requests come out of the receiver.
    pub fn start(folders: Vec<PathBuf>, lock_dir: &Path) -> io::Result<(Server, mpsc::UnboundedReceiver<Request>)> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        remove_stale_locks(lock_dir);
        let (requests, received) = mpsc::unbounded();
        let shared = Arc::new(Shared {
            port,
            token: crate::instance::random_token(),
            lock_path: lock_dir.join(format!("{port}.lock")),
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

    #[cfg(test)]
    fn port(&self) -> u16 {
        self.shared.port
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
        let text = protocol::notification(method, params).to_string();
        for (_, client) in self.shared.clients.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            let _ = client.send(text.clone());
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let shared = &self.shared;
        shared.stopped.store(true, Ordering::Relaxed);
        let _ = fs::remove_file(&shared.lock_path);
        SERVERS.lock().unwrap_or_else(|e| e.into_inner()).retain(|(port, _)| *port != shared.port);
        // Wakes the accepting thread, which then sees it's stopped; clients
        // see it on their next tick.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, shared.port));
    }
}

impl Shared {
    fn folders(&self) -> Vec<PathBuf> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).folders.clone()
    }

    fn register(&self) {
        let mut servers = SERVERS.lock().unwrap_or_else(|e| e.into_inner());
        servers.retain(|(port, _)| *port != self.port);
        servers.push((self.port, self.folders()));
    }

    /// Writes the lock file whole (a temp file renamed over it), readable
    /// only by the user: it holds the token.
    fn write_lock(&self) -> io::Result<()> {
        let folders: Vec<String> = self.folders().iter().map(|f| f.display().to_string()).collect();
        let content = json!({
            "pid": std::process::id(),
            "workspaceFolders": folders,
            "ideName": IDE_NAME,
            "transport": "ws",
            "authToken": self.token,
        });
        let dir = self.lock_path.parent().ok_or_else(|| io::Error::other("no lock folder"))?;
        create_private_dir(dir)?;
        let temp = dir.join(format!(".{}.{}.tmp", self.port, std::process::id()));
        let _ = fs::remove_file(&temp);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let written = options.open(&temp).and_then(|mut file| {
            file.write_all(content.to_string().as_bytes())?;
            file.sync_all()
        });
        match written.and_then(|()| fs::rename(&temp, &self.lock_path)) {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = fs::remove_file(&temp);
                Err(err)
            }
        }
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Removes the lock files of Kuanas that ended without removing theirs (a
/// crash, a kill), so `/ide` doesn't offer them.
fn remove_stale_locks(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "lock") {
            continue;
        }
        let Some(lock) = fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) else { continue };
        let Some(pid) = lock.get("pid").and_then(Value::as_u64) else { continue };
        if lock.get("ideName").and_then(Value::as_str) == Some(IDE_NAME) && pid != u64::from(std::process::id()) && !alive(pid as u32) {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // Not a real process id (0 and negative ones name process groups).
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn alive(_: u32) -> bool {
    true
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

/// Compares without stopping at the first difference, so timing doesn't
/// tell how much of a guess was right.
fn same_token(given: &str, token: &str) -> bool {
    given.len() == token.len() && given.bytes().zip(token.bytes()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

/// One client, from its handshake until it goes or the server stops.
#[allow(clippy::result_large_err)]
fn serve(stream: TcpStream, shared: &Arc<Shared>) {
    if shared.stopped.load(Ordering::Relaxed) {
        return;
    }
    let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT));
    let Ok(mut socket) = tungstenite::accept_hdr(stream, |request: &Handshake, response| authorize(request, response, &shared.token)) else {
        return;
    };
    // From now on reads time out every tick, to send what's waiting.
    let _ = socket.get_ref().set_read_timeout(Some(TICK));
    let (outgoing, waiting) = std_mpsc::channel::<String>();
    let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
    let selection = shared.state.lock().unwrap_or_else(|e| e.into_inner()).selection.clone();
    if let Some(selection) = selection {
        let _ = outgoing.send(protocol::notification("selection_changed", selection).to_string());
    }
    shared.clients.lock().unwrap_or_else(|e| e.into_inner()).push((id, outgoing.clone()));
    let _ = shared.requests.unbounded_send(Request::Connected);
    'connection: loop {
        if shared.stopped.load(Ordering::Relaxed) {
            let _ = socket.close(None);
            let _ = socket.flush();
            break;
        }
        let mut wrote = false;
        while let Ok(text) = waiting.try_recv() {
            if socket.write(Message::text(text)).is_err() {
                break 'connection;
            }
            wrote = true;
        }
        if wrote && socket.flush().is_err() {
            break;
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                let state = shared.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                match protocol::handle(text.as_str(), &state) {
                    protocol::Incoming::Reply(reply) => {
                        let _ = outgoing.send(reply.to_string());
                    }
                    protocol::Incoming::Tool { id, name, arguments } => ask_window(shared, outgoing.clone(), id, name, arguments),
                    protocol::Incoming::Nothing => {}
                }
            }
            // Pings are answered by tungstenite; a close is answered on the
            // next read, which then fails.
            Ok(_) => {}
            Err(tungstenite::Error::Io(err)) if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
    shared.clients.lock().unwrap_or_else(|e| e.into_inner()).retain(|(client, _)| *client != id);
}

/// Hands a tool call to the window and sends back its answer, waiting on a
/// thread of its own.
fn ask_window(shared: &Shared, outgoing: std_mpsc::Sender<String>, id: Value, name: String, arguments: Value) {
    let (answer, answered) = oneshot::channel();
    if shared.requests.unbounded_send(Request::Tool(ToolCall { name, arguments, answer })).is_err() {
        let _ = outgoing.send(protocol::tool_result(id, Err("Kuana's window is closed".into())).to_string());
        return;
    }
    let _ = std::thread::Builder::new().name("claude ide tool".into()).spawn(move || {
        let answer = futures::executor::block_on(answered).unwrap_or_else(|_| Err("Kuana couldn't answer".into()));
        let _ = outgoing.send(protocol::tool_result(id, answer).to_string());
    });
}

/// What `selection_changed` says of a selection (`range`, bytes) in a file:
/// zero-based lines and UTF-16 columns, as VS Code counts them.
pub fn selection_payload(path: &Path, rope: &Rope, range: std::ops::Range<usize>) -> Value {
    let start = offset_to_position(rope, range.start, Encoding::Utf16);
    let end = offset_to_position(rope, range.end, Encoding::Utf16);
    let text = rope.byte_slice(range.start.min(rope.len_bytes())..range.end.min(rope.len_bytes())).to_string();
    json!({
        "text": text,
        "filePath": path,
        "fileUrl": file_url(path),
        "selection": {
            "start": { "line": start.line, "character": start.character },
            "end": { "line": end.line, "character": end.character },
            "isEmpty": range.is_empty(),
        },
    })
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
        serde_json::from_slice(&fs::read(&server.shared.lock_path).unwrap()).unwrap()
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
        let (server, _requests) = Server::start(vec![PathBuf::from("/work/app")], &dir).unwrap();
        let lock = lock_of(&server);
        assert_eq!(lock["ideName"], "Kuana");
        assert_eq!(lock["transport"], "ws");
        assert_eq!(lock["pid"], std::process::id());
        assert_eq!(lock["workspaceFolders"], json!(["/work/app"]));
        assert_eq!(lock["authToken"].as_str().unwrap().len(), server.shared.token.len());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(fs::metadata(&server.shared.lock_path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        server.set_folders(vec![PathBuf::from("/work/app"), PathBuf::from("/work/lib")]);
        assert_eq!(lock_of(&server)["workspaceFolders"], json!(["/work/app", "/work/lib"]));
        let path = server.shared.lock_path.clone();
        drop(server);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn removes_lock_files_of_kuanas_that_are_gone() {
        let dir = temp_lock_dir("stale");
        fs::create_dir_all(&dir).unwrap();
        // A pid no process has (above the usual limits).
        let gone = json!({ "pid": 2_147_483_000u32, "ideName": "Kuana" });
        let other_ide = json!({ "pid": 2_147_483_000u32, "ideName": "Visual Studio Code" });
        fs::write(dir.join("1.lock"), gone.to_string()).unwrap();
        fs::write(dir.join("2.lock"), other_ide.to_string()).unwrap();
        let (server, _requests) = Server::start(Vec::new(), &dir).unwrap();
        assert_eq!(dir.join("1.lock").exists(), !cfg!(unix));
        assert!(dir.join("2.lock").exists());
        drop(server);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminals_reach_the_window_holding_their_folder() {
        let dir = temp_lock_dir("env");
        let (outer, _a) = Server::start(vec![PathBuf::from("/kuana-env-test/app")], &dir).unwrap();
        let (inner, _b) = Server::start(vec![PathBuf::from("/kuana-env-test/app/sub")], &dir).unwrap();
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
        let (server, _requests) = Server::start(Vec::new(), &dir).unwrap();
        assert!(connect(&server, "not-the-token").is_err());
        assert!(!server.has_clients());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn talks_mcp_and_hands_tools_to_the_window() {
        let dir = temp_lock_dir("talk");
        let (server, mut requests) = Server::start(vec![PathBuf::from("/work/app")], &dir).unwrap();
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

        // A new selection is pushed; the same one again isn't.
        server.set_selection(json!({ "text": "fn main" }));
        server.set_selection(json!({ "text": "let x" }));
        assert_eq!(receive(&mut client)["params"]["text"], "let x");

        drop(server);
        // The server closes the connection.
        let closed = (0..100).any(|_| matches!(client.read(), Err(_) | Ok(Message::Close(_))));
        assert!(closed);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn selections_are_zero_based_with_utf16_columns() {
        let rope = Rope::from_str("fn main() {}\nlet é = \"😀x\";\n");
        let line = "fn main() {}\n".len();
        let start = line + "let é = \"".len();
        let payload = selection_payload(Path::new("/work/a.rs"), &rope, start..start + "😀x".len());
        assert_eq!(payload["text"], "😀x");
        assert_eq!(payload["selection"]["start"], json!({ "line": 1, "character": 9 }));
        assert_eq!(payload["selection"]["end"], json!({ "line": 1, "character": 12 }));
        assert_eq!(payload["selection"]["isEmpty"], false);
        assert_eq!(selection_payload(Path::new("/work/a.rs"), &rope, 3..3)["selection"]["isEmpty"], true);
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
