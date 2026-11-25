//! What Kuana's connections to terminal agents share: Claude Code's `/ide`
//! (`claude_ide.rs`, MCP over WebSocket) and Gemini CLI's IDE companion
//! protocol (`gemini_ide.rs`, MCP over HTTP). Both run a server per project
//! window on a loopback port, advertise it in a private file holding a
//! random token, point the window's terminals at it with variables, and hand
//! the tool calls they can't answer themselves to the window as
//! [`Request`]s; the window side is `workspace/agent_ide.rs`.

use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use futures::channel::{mpsc, oneshot};
use serde_json::{Value, json};

pub use kuana_core::private_files::{alive, private_dir, write_private};

/// Which agent a connection, a proposed change or a status item is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Agent {
    ClaudeCode,
    GeminiCli,
    /// Kuana's own model requests (Inline Assist), proposed like an agent's.
    Kuana,
}

impl Agent {
    pub fn name(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::GeminiCli => "Gemini CLI",
            Agent::Kuana => "Kuana AI",
        }
    }

    /// The status bar's label.
    pub fn short_name(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude",
            Agent::GeminiCli => "Gemini",
            Agent::Kuana => "Kuana",
        }
    }
}

/// Something the window answers.
pub enum Request {
    /// A client connected: it should hear the current selection.
    Connected,
    /// A client went (the agent ended, or left the window).
    Disconnected,
    Tool(ToolCall),
}

/// A tool call the window answers.
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

/// Hands a tool call to the window; the receiver gets its answer (or is
/// cancelled when the window drops it). None when the window is gone.
pub fn ask_window(requests: &mpsc::UnboundedSender<Request>, name: String, arguments: Value) -> Option<oneshot::Receiver<Result<Vec<String>, String>>> {
    let (answer, answered) = oneshot::channel();
    requests.unbounded_send(Request::Tool(ToolCall { name, arguments, answer })).ok()?;
    Some(answered)
}

/// Waits for the window's answer to a tool call (on the caller's thread).
pub fn wait_for(answered: Option<oneshot::Receiver<Result<Vec<String>, String>>>) -> Result<Vec<String>, String> {
    match answered {
        Some(answered) => futures::executor::block_on(answered).unwrap_or_else(|_| Err("Kuana couldn't answer".into())),
        None => Err("Kuana's window is closed".into()),
    }
}

/// Which window's server a terminal started in a folder should reach: each
/// route's project folders and the variables that point an agent at it. A
/// route has a key of its own ([`Routes::key`]): a local server's port and
/// one a remote host listens on may be the same number.
pub struct Routes(Mutex<Vec<Route>>);

struct Route {
    key: u64,
    folders: Vec<PathBuf>,
    env: Vec<(String, String)>,
}

static NEXT_ROUTE: AtomicU64 = AtomicU64::new(0);

impl Routes {
    pub const fn new() -> Routes {
        Routes(Mutex::new(Vec::new()))
    }

    /// A new route's key.
    pub fn key() -> u64 {
        NEXT_ROUTE.fetch_add(1, Ordering::Relaxed)
    }

    pub fn set(&self, key: u64, folders: Vec<PathBuf>, env: Vec<(String, String)>) {
        let mut routes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        routes.retain(|r| r.key != key);
        routes.push(Route { key, folders, env });
    }

    pub fn remove(&self, key: u64) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).retain(|r| r.key != key);
    }

    /// The variables for a shell started in `cwd`: those of the window whose
    /// folder holds it (the innermost, with nested projects open), if any.
    pub fn env_for(&self, cwd: &Path) -> Vec<(String, String)> {
        let routes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        routes
            .iter()
            .filter_map(|r| r.folders.iter().filter(|f| cwd.starts_with(f)).map(|f| f.as_os_str().len()).max().map(|depth| (depth, r)))
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, r)| r.env.clone())
            .unwrap_or_default()
    }
}

/// Compares without stopping at the first difference, so timing doesn't
/// tell how much of a guess was right.
pub fn same_token(given: &str, token: &str) -> bool {
    given.len() == token.len() && given.bytes().zip(token.bytes()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

// MCP over JSON-RPC 2.0, as both agents speak it.

/// What a JSON-RPC message from the agent calls for.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// Send this back.
    Reply(Value),
    /// A tool the window answers; its answer goes back with [`tool_result`].
    Tool { id: Value, name: String, arguments: Value },
    /// Nothing to answer: a notification, or a response to something sent.
    Nothing,
}

/// The MCP version answered when the client names none.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// Answers the MCP methods both servers answer alike; `tools/call` goes to
/// `call` with the request's id, the tool's name and its arguments (an
/// object, empty when none came).
pub fn handle(message: &Value, tools: impl FnOnce() -> Vec<Value>, call: impl FnOnce(Value, &str, Value) -> Incoming) -> Incoming {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        // A response to a request of ours (none are sent yet).
        return Incoming::Nothing;
    };
    let Some(id) = message.get("id").cloned() else {
        // `notifications/initialized` and the like.
        return Incoming::Nothing;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => initialize(&params),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "prompts/list" => json!({ "prompts": [] }),
        "resources/list" => json!({ "resources": [] }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            let arguments = params.get("arguments").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
            return call(id, name, arguments);
        }
        _ => return Incoming::Reply(error(id, -32601, &format!("Method not found: {method}"))),
    };
    Incoming::Reply(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// A tool call for the window once the string arguments it needs are there
/// (`required`), else its error.
pub fn window_tool(id: Value, name: &str, arguments: Value, required: &[&str]) -> Incoming {
    match required.iter().find(|arg| arguments.get(**arg).and_then(Value::as_str).is_none()) {
        Some(missing) => Incoming::Reply(tool_result(id, Err(format!("Missing argument: {missing}")))),
        None => Incoming::Tool { id, name: name.to_string(), arguments },
    }
}

fn initialize(params: &Value) -> Value {
    let version = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "logging": {}, "prompts": { "listChanged": true }, "tools": { "listChanged": true } },
        "serverInfo": { "name": "kuana", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// A tool's answer: text items (most of them JSON), or why it failed.
pub fn tool_result(id: Value, answer: Result<Vec<String>, String>) -> Value {
    let result = match answer {
        Ok(texts) => json!({ "content": texts.into_iter().map(|t| json!({ "type": "text", "text": t })).collect::<Vec<_>>() }),
        Err(message) => json!({ "content": [{ "type": "text", "text": message }], "isError": true }),
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A notification to send to the agent.
pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

pub fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A tool's entry in `tools/list`.
pub fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required, "additionalProperties": false },
    })
}

/// A file's `file://` URL, as agents name files. A remote host's path
/// (`/home/me/p`) gets one on Windows too, where it isn't a file path.
pub fn file_url(path: &Path) -> String {
    let host_url = || {
        let text = path.to_str().filter(|t| t.starts_with('/'))?;
        let mut url = url::Url::parse("file:///").ok()?;
        url.set_path(text);
        Some(url)
    };
    url::Url::from_file_path(path).ok().or_else(host_url).map(String::from).unwrap_or_else(|| format!("file://{}", path.display()))
}

/// The path a `file://` URL (or a plain path) names; a remote host's
/// (`file:///home/me/p`) on Windows too.
pub fn path_of(uri: &str) -> PathBuf {
    match url::Url::parse(uri).ok().filter(|u| u.scheme() == "file") {
        Some(url) => url.to_file_path().unwrap_or_else(|()| PathBuf::from(percent_encoding::percent_decode_str(url.path()).decode_utf8_lossy().into_owned())),
        None => PathBuf::from(uri),
    }
}

/// Paths as an agent sees them and as Kuana has them: the same for a local
/// project; for a remote one, the agent (on the host) names the host's paths
/// and Kuana the mount's (`/kuana-remote/box/home/me/p` is `/home/me/p`).
#[derive(Clone, Debug, Default)]
pub struct PathMap {
    /// The remote project's mount.
    mount: Option<PathBuf>,
}

impl PathMap {
    pub fn new(mount: Option<PathBuf>) -> PathMap {
        PathMap { mount }
    }

    /// A path of Kuana's as the agent names it.
    pub fn to_agent(&self, path: &Path) -> PathBuf {
        match self.mount.as_ref().and_then(|mount| path.strip_prefix(mount).ok()) {
            Some(rest) => PathBuf::from(kuana_core::fs::local_to_host(rest)),
            None => path.to_path_buf(),
        }
    }

    /// A path's `file://` URL as the agent names it.
    pub fn url(&self, path: &Path) -> String {
        file_url(&self.to_agent(path))
    }

    /// A path the agent names (a `file://` URL or a path) as Kuana has it; a
    /// relative one stays relative.
    pub fn from_agent(&self, given: &str) -> PathBuf {
        let path = path_of(given);
        let host_path = path.to_str().filter(|p| p.starts_with('/'));
        match (&self.mount, host_path) {
            (Some(mount), Some(host_path)) => kuana_core::fs::local_path(mount, host_path),
            _ => path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params })
    }

    fn answer(message: &Value) -> Incoming {
        handle(message, Vec::new, |id, name, arguments| window_tool(id, name, arguments, &["filePath"]))
    }

    fn reply(incoming: Incoming) -> Value {
        match incoming {
            Incoming::Reply(value) => value,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    #[test]
    fn initialize_echoes_the_clients_version() {
        let answered = reply(answer(&request("initialize", json!({ "protocolVersion": "2024-11-05" }))));
        assert_eq!(answered["id"], 7);
        assert_eq!(answered["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(answered["result"]["serverInfo"]["name"], "kuana");
        let answered = reply(answer(&request("initialize", json!({}))));
        assert_eq!(answered["result"]["protocolVersion"], DEFAULT_PROTOCOL_VERSION);
    }

    #[test]
    fn notifications_and_responses_get_no_answer() {
        assert_eq!(answer(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })), Incoming::Nothing);
        assert_eq!(answer(&json!({ "jsonrpc": "2.0", "id": 1, "result": {} })), Incoming::Nothing);
        assert_eq!(reply(answer(&request("nope", json!({}))))["error"]["code"], -32601);
    }

    #[test]
    fn window_tools_wait_for_their_arguments() {
        let call = request("tools/call", json!({ "name": "save", "arguments": { "filePath": "/a.rs", "line": 3 } }));
        match answer(&call) {
            Incoming::Tool { id, name, arguments } => assert_eq!((id, name.as_str(), &arguments["line"]), (json!(7), "save", &json!(3))),
            other => panic!("expected a tool call, got {other:?}"),
        }
        let missing = reply(answer(&request("tools/call", json!({ "name": "save" }))));
        assert_eq!(missing["result"]["isError"], true);
        assert_eq!(missing["result"]["content"][0]["text"], "Missing argument: filePath");
    }

    #[test]
    fn tool_results_carry_text_items_or_an_error() {
        let ok = tool_result(json!(3), Ok(vec!["a".into(), "b".into()]));
        assert_eq!(ok["result"]["content"][1]["text"], "b");
        assert!(ok["result"].get("isError").is_none());
        assert_eq!(tool_result(json!(3), Ok(Vec::new()))["result"]["content"], json!([]));
        assert_eq!(tool_result(json!(3), Err("no".into()))["result"]["isError"], true);
    }

    #[test]
    fn file_urls_go_both_ways() {
        let path = if cfg!(windows) { PathBuf::from(r"C:\work\a b.rs") } else { PathBuf::from("/work/a b.rs") };
        let url = file_url(&path);
        assert!(url.starts_with("file:///") && url.contains("a%20b.rs"), "{url}");
        assert_eq!(path_of(&url), path);
        assert_eq!(path_of("/plain/path.rs"), PathBuf::from("/plain/path.rs"));
        // A remote host's paths, on every system.
        assert_eq!(file_url(Path::new("/home/me/a b.rs")), "file:///home/me/a%20b.rs");
        assert_eq!(path_of("file:///home/me/a%20b.rs"), PathBuf::from("/home/me/a b.rs"));
    }

    #[test]
    fn remote_paths_are_the_hosts_to_agents() {
        let mount = kuana_core::fs::mount_root("box");
        let map = PathMap::new(Some(mount.clone()));
        let file = mount.join("home").join("me").join("a b.rs");
        assert_eq!(map.to_agent(&file), PathBuf::from("/home/me/a b.rs"));
        assert_eq!(map.url(&file), "file:///home/me/a%20b.rs");
        assert_eq!(map.from_agent("/home/me/a b.rs"), file);
        assert_eq!(map.from_agent("file:///home/me/a%20b.rs"), file);
        // Relative paths stay so (the window puts them under its folder).
        assert_eq!(map.from_agent("src/a.rs"), PathBuf::from("src/a.rs"));
        // Anything else of Kuana's stays as it is.
        let local = if cfg!(windows) { PathBuf::from(r"C:\work\a.rs") } else { PathBuf::from("/work/a.rs") };
        assert_eq!(map.to_agent(&local), local);
        // A local project's map changes nothing.
        let none = PathMap::default();
        assert_eq!((none.to_agent(&local), none.from_agent(&local.to_string_lossy())), (local.clone(), local));
    }

    #[test]
    fn terminals_reach_the_innermost_window() {
        let routes = Routes::new();
        let env = |port: u16| vec![("PORT".to_string(), port.to_string())];
        routes.set(1, vec![PathBuf::from("/work/app")], env(1));
        routes.set(2, vec![PathBuf::from("/work/app/sub")], env(2));
        assert_ne!(Routes::key(), Routes::key());
        assert_eq!(routes.env_for(Path::new("/work/app/src")), env(1));
        assert_eq!(routes.env_for(Path::new("/work/app/sub/x")), env(2));
        assert!(routes.env_for(Path::new("/elsewhere")).is_empty());
        routes.remove(2);
        assert_eq!(routes.env_for(Path::new("/work/app/sub/x")), env(1));
    }

    #[test]
    fn tokens_compare_whole() {
        assert!(same_token("abc", "abc"));
        assert!(!same_token("abd", "abc"));
        assert!(!same_token("ab", "abc"));
    }
}
