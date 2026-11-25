//! MCP (Model Context Protocol) servers for agent sessions: the `ai.mcp_servers`
//! settings, handed to ACP agents in `session/new` / `session/load`, and a
//! small client Kuana's own agent uses them through. A server is a program
//! speaking JSON-RPC over stdio, one message per line (as ACP, so the same
//! [`Connection`]); the client starts it, `initialize`s, lists its tools and
//! calls them. Requests a server makes of the client (roots, sampling) are
//! declined. Dropping the client stops the server.

use std::{
    collections::BTreeMap,
    io::{BufRead as _, BufReader},
    process::{Child, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures::StreamExt as _;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::acp::rpc::{Connection, Incoming, RpcError};

/// How long a server has to start and list its tools.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a tool call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(300);
const PROTOCOL_VERSION: &str = "2025-06-18";

/// One `ai.mcp_servers` entry: the program and how to run it.
#[derive(Deserialize, JsonSchema, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ServerSettings {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// The servers as ACP's `mcpServers` (stdio ones).
pub fn acp_servers(servers: &BTreeMap<String, ServerSettings>) -> Vec<Value> {
    servers
        .iter()
        .filter(|(_, s)| !s.command.trim().is_empty())
        .map(|(name, s)| {
            json!({
                "name": name,
                "command": s.command,
                "args": s.args,
                "env": s.env.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// The servers back from ACP's `mcpServers` (what Kuana's agent was given).
pub fn from_acp(servers: &Value) -> Vec<(String, ServerSettings)> {
    servers
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let name = s["name"].as_str()?.to_string();
            let command = s["command"].as_str()?.to_string();
            let args = s["args"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)).collect();
            let env = s["env"].as_array().into_iter().flatten().filter_map(|e| Some((e["name"].as_str()?.to_string(), e["value"].as_str()?.to_string()))).collect();
            Some((name, ServerSettings { command, args, env }))
        })
        .collect()
}

/// A tool a server offers.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// A running server.
pub struct Client {
    pub name: String,
    rpc: Connection,
    child: Mutex<Option<Child>>,
    pub tools: Vec<Tool>,
}

impl Client {
    /// Starts `settings`' program in `cwd` and lists its tools.
    pub fn spawn(name: &str, settings: &ServerSettings, cwd: &std::path::Path, cancel: &AtomicBool) -> Result<Client, String> {
        let mut command = crate::process::command(&settings.command);
        command.args(&settings.args).current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        command.env("PATH", crate::lsp::servers::search_path());
        command.envs(&settings.env);
        let mut child = command.spawn().map_err(|e| format!("Couldn't start the MCP server {name} ({}): {e}", settings.command))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else { return Err(format!("The MCP server {name} has no stdio")) };
        // Its stderr is drained so a chatty server can't block on it.
        if let Some(stderr) = child.stderr.take() {
            let _ = std::thread::Builder::new().name("mcp stderr".into()).spawn(move || for _ in BufReader::new(stderr).split(b'\n') {});
        }
        let mut client = Client::connect(name, stdout, stdin, cancel)?;
        client.child = Mutex::new(Some(child));
        Ok(client)
    }

    /// Talks to a server on `read` / `write` (a process's stdio, or a test's pipe).
    pub fn connect(name: &str, read: impl std::io::Read + Send + 'static, write: impl std::io::Write + Send + 'static, cancel: &AtomicBool) -> Result<Client, String> {
        let (rpc, mut incoming) = Connection::start(read, write);
        let answering = rpc.clone();
        let _ = std::thread::Builder::new().name("mcp requests".into()).spawn(move || {
            futures::executor::block_on(async {
                while let Some(message) = incoming.next().await {
                    if let Incoming::Request { id, method, .. } = message {
                        // `ping` is the one request a client must answer.
                        let answer = if method == "ping" { Ok(json!({})) } else { Err(RpcError::new(RpcError::METHOD_NOT_FOUND, format!("Kuana doesn't serve {method}"))) };
                        answering.respond(id, answer);
                    }
                }
            })
        });
        let init = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "kuana", "title": "Kuana", "version": env!("CARGO_PKG_VERSION") },
        });
        wait(rpc.request("initialize", init), START_TIMEOUT, cancel).map_err(|e| format!("The MCP server {name} didn't start: {e}"))?;
        rpc.notify("notifications/initialized", json!({}));
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map_or_else(|| json!({}), |c| json!({ "cursor": c }));
            let page = wait(rpc.request("tools/list", params), START_TIMEOUT, cancel).map_err(|e| format!("The MCP server {name} didn't list its tools: {e}"))?;
            tools.extend(page["tools"].as_array().into_iter().flatten().filter_map(|t| {
                Some(Tool {
                    name: t["name"].as_str()?.to_string(),
                    description: t["description"].as_str().unwrap_or_default().to_string(),
                    schema: t.get("inputSchema").cloned().unwrap_or_else(|| json!({ "type": "object" })),
                })
            }));
            match page["nextCursor"].as_str() {
                Some(next) if !next.is_empty() && cursor.as_deref() != Some(next) => cursor = Some(next.to_string()),
                _ => break,
            }
        }
        Ok(Client { name: name.to_string(), rpc, child: Mutex::new(None), tools })
    }

    /// Calls a tool: its result as text, and whether it's an error. `cancel`
    /// (the user's Stop) ends the wait.
    pub fn call(&self, tool: &str, arguments: &Value, cancel: &AtomicBool) -> (String, bool) {
        let params = json!({ "name": tool, "arguments": arguments });
        match wait(self.rpc.request("tools/call", params), CALL_TIMEOUT, cancel) {
            Ok(result) => (result_text(&result), result["isError"].as_bool().unwrap_or(false)),
            Err(error) => (format!("The MCP server {} failed: {error}", self.name), true),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            crate::process::kill_tree(&mut child);
        }
    }
}

/// A `tools/call` result's content as text: text parts as they are, others
/// named (an image, a resource), structured content as JSON.
fn result_text(result: &Value) -> String {
    let mut parts: Vec<String> = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|part| match part["type"].as_str() {
            Some("text") => part["text"].as_str().unwrap_or_default().to_string(),
            Some("resource") => part["resource"]["text"].as_str().map_or_else(|| format!("[resource {}]", part["resource"]["uri"].as_str().unwrap_or("")), str::to_string),
            Some("resource_link") => format!("[resource {}]", part["uri"].as_str().unwrap_or("")),
            Some(other) => format!("[{other} content]"),
            None => String::new(),
        })
        .collect();
    if parts.iter().all(String::is_empty)
        && let Some(structured) = result.get("structuredContent")
    {
        parts = vec![structured.to_string()];
    }
    parts.join("\n")
}

/// Waits for `answer` at most `timeout`, or until `cancel` is set. A ticker
/// thread wakes the wait every 100 ms and ends with it.
fn wait(answer: impl Future<Output = Result<Value, RpcError>>, timeout: Duration, cancel: &AtomicBool) -> Result<Value, String> {
    let (tick, mut ticks) = futures::channel::mpsc::unbounded::<()>();
    let _ = std::thread::Builder::new().name("mcp wait".into()).spawn(move || {
        while tick.unbounded_send(()).is_ok() {
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let deadline = std::time::Instant::now() + timeout;
    futures::executor::block_on(async {
        let mut answer = std::pin::pin!(answer);
        loop {
            match futures::future::select(answer.as_mut(), ticks.next()).await {
                futures::future::Either::Left((answer, _)) => return answer.map_err(|e| e.message),
                futures::future::Either::Right(_) if cancel.load(Ordering::Relaxed) => return Err("stopped".to_string()),
                futures::future::Either::Right(_) if std::time::Instant::now() >= deadline => return Err(format!("no answer in {} s", timeout.as_secs())),
                futures::future::Either::Right(_) => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake server on a pipe: answers initialize, lists two tools over two
    /// pages and runs `echo`.
    fn fake_server() -> Client {
        let (client_read, server_write) = std::io::pipe().unwrap();
        let (server_read, client_write) = std::io::pipe().unwrap();
        let (server, mut incoming) = Connection::start(server_read, server_write);
        std::thread::spawn(move || {
            futures::executor::block_on(async {
                while let Some(message) = incoming.next().await {
                    let Incoming::Request { id, method, params } = message else { continue };
                    let answer = match method.as_str() {
                        "initialize" => json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": { "tools": {} }, "serverInfo": { "name": "fake" } }),
                        "tools/list" if params.get("cursor").is_none() => json!({ "tools": [{ "name": "echo", "description": "Says it back", "inputSchema": { "type": "object" } }], "nextCursor": "2" }),
                        "tools/list" => json!({ "tools": [{ "name": "fail" }] }),
                        "tools/call" if params["name"] == "echo" => json!({ "content": [{ "type": "text", "text": params["arguments"]["text"] }] }),
                        _ => json!({ "content": [{ "type": "text", "text": "broken" }], "isError": true }),
                    };
                    server.respond(id, Ok(answer));
                }
            })
        });
        Client::connect("fake", client_read, client_write, &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn tools_are_listed_and_called() {
        let client = fake_server();
        let names: Vec<&str> = client.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["echo", "fail"]);
        assert_eq!(client.tools[0].description, "Says it back");
        let go = AtomicBool::new(false);
        assert_eq!(client.call("echo", &json!({ "text": "hi" }), &go), ("hi".to_string(), false));
        assert_eq!(client.call("fail", &json!({}), &go), ("broken".to_string(), true));
        // Stop ends a call that gets no answer.
        let stopped = AtomicBool::new(true);
        let (text, failed) = client.call("silent", &json!({}), &stopped);
        assert!(failed && text.ends_with("stopped"), "{text}");
    }

    #[test]
    fn settings_become_acp_servers_and_back() {
        let mut servers = BTreeMap::new();
        servers.insert("db".to_string(), ServerSettings { command: "db-mcp".into(), args: vec!["--ro".into()], env: [("TOKEN".to_string(), "x".to_string())].into() });
        servers.insert("empty".to_string(), ServerSettings::default());
        let acp = acp_servers(&servers);
        assert_eq!(acp, vec![json!({ "name": "db", "command": "db-mcp", "args": ["--ro"], "env": [{ "name": "TOKEN", "value": "x" }] })]);
        assert_eq!(from_acp(&Value::Array(acp)), vec![("db".to_string(), servers["db"].clone())]);
    }

    #[test]
    fn results_read_as_text() {
        assert_eq!(result_text(&json!({ "content": [{ "type": "text", "text": "a" }, { "type": "image", "data": "…" }] })), "a\n[image content]");
        assert_eq!(result_text(&json!({ "content": [], "structuredContent": { "n": 1 } })), "{\"n\":1}");
    }
}
