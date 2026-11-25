//! Kuana's own coding agent: the model providers (`crate::ai`) and a set of
//! tools, as an ACP agent that runs in-process on a pipe, so the session
//! view, permissions, review and history treat it like any other agent.
//!
//! It reads and writes files and runs commands *through the client*
//! (`fs/read_text_file`, `fs/write_text_file`, `terminal/*`): Kuana answers
//! those with open documents (unsaved edits included), one undo step per
//! write and its own terminals. Problems come from Kuana too, through the
//! extension method `_kuana/problems`. Listing and searching read the
//! project's files directly (honoring .gitignore).
//!
//! Modes: Ask (asks before each edit and command), Edit (edits freely, asks
//! before commands), Plan (reads only). "Always allow" lasts the session.
//!
//! Each session's conversation with the model (tool calls and results
//! included) is kept in `<config>/agents/kuana/<session>.json` after every
//! turn, so `session/load` continues it after Kuana restarts. Kuana keeps the
//! transcript itself, so nothing is replayed.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures::{StreamExt as _, channel::oneshot};
use serde_json::{Value, json};

use crate::acp::rpc::{Connection, Incoming, RpcError};
use crate::ai::{self, Message, Part, Role, StopReason, StreamEvent, ToolSpec};

/// Model calls in one turn before it stops (a loop guard).
const MAX_STEPS: usize = 120;
/// The most a tool result carries back to the model.
const MAX_RESULT: usize = 60_000;
/// How long a command may run before it's stopped, unless it asks for more.
const COMMAND_TIMEOUT: u64 = 120;
const MAX_TOKENS: u32 = 32_000;
/// Project instruction files read into the system prompt, first found wins
/// per name.
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".kuana/agent.md"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Ask,
    Edit,
    Plan,
}

impl Mode {
    fn id(self) -> &'static str {
        match self {
            Mode::Ask => "ask",
            Mode::Edit => "edit",
            Mode::Plan => "plan",
        }
    }

    fn from_id(id: &str) -> Option<Mode> {
        match id {
            "ask" => Some(Mode::Ask),
            "edit" => Some(Mode::Edit),
            "plan" => Some(Mode::Plan),
            _ => None,
        }
    }
}

fn modes_json(current: Mode) -> Value {
    json!({
        "currentModeId": current.id(),
        "availableModes": [
            { "id": "ask", "name": "Ask", "description": "Asks before each edit and command" },
            { "id": "edit", "name": "Edit", "description": "Edits files freely, asks before commands" },
            { "id": "plan", "name": "Plan", "description": "Reads and plans, changes nothing" },
        ],
    })
}

struct Session {
    cwd: PathBuf,
    messages: Vec<Message>,
    mode: Mode,
    model: Option<String>,
    models: Vec<(String, String)>,
    /// What "always allow" covered: `edit`, or `run:<program>`.
    allowed: HashSet<String>,
    cancelled: Arc<AtomicBool>,
}

type Sessions = Arc<Mutex<HashMap<String, Session>>>;

/// What's kept of a session between runs.
#[derive(serde::Serialize, serde::Deserialize)]
struct Kept {
    cwd: PathBuf,
    messages: Vec<Message>,
    mode: Mode,
    #[serde(default)]
    model: Option<String>,
}

fn kept_file(id: &str) -> Option<PathBuf> {
    let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    (!safe.is_empty()).then(|| crate::acp::install::agents_dir().map(|d| d.join("kuana").join(format!("{safe}.json"))))?
}

fn keep(id: &str, session: &Session) {
    let Some(path) = kept_file(id) else { return };
    let kept = Kept { cwd: session.cwd.clone(), messages: session.messages.clone(), mode: session.mode, model: session.model.clone() };
    let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| crate::state::write_atomic(&path, &serde_json::to_vec(&kept).unwrap_or_default()));
    if let Err(err) = written {
        eprintln!("couldn't keep the Kuana Agent session: {err}");
    }
}

/// Removes what's kept of a session (it was deleted).
pub fn forget(id: &str) {
    if let Some(path) = kept_file(id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Picks the provider and model a session runs on (the AI settings', read
/// when a session starts and on each prompt).
pub type Chooser = Arc<dyn Fn() -> Result<ai::Choice, String> + Send + Sync>;

/// Starts the agent on a thread; Kuana connects to the returned pipe ends
/// (`acp::Agent::connect(read, write)`).
pub fn start(choose: Chooser) -> std::io::Result<(std::io::PipeReader, std::io::PipeWriter)> {
    let (client_read, agent_write) = std::io::pipe()?;
    let (agent_read, client_write) = std::io::pipe()?;
    let (conn, mut incoming) = Connection::start(agent_read, agent_write);
    let sessions: Sessions = Arc::default();
    std::thread::Builder::new().name("kuana agent".into()).spawn(move || {
        futures::executor::block_on(async {
            while let Some(message) = incoming.next().await {
                match message {
                    Incoming::Notification { method, params } if method == "session/cancel" => {
                        let id = params["sessionId"].as_str().unwrap_or_default();
                        if let Some(session) = sessions.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
                            session.cancelled.store(true, Ordering::Relaxed);
                        }
                    }
                    Incoming::Notification { .. } => {}
                    Incoming::Request { id, method, params } => {
                        // Each request on a thread of its own: a turn runs
                        // long, and cancels and option changes come meanwhile.
                        let (conn, sessions, choose) = (conn.clone(), sessions.clone(), choose.clone());
                        let _ = std::thread::Builder::new().name("kuana agent request".into()).spawn(move || {
                            let result = handle(&conn, &sessions, &choose, &method, params);
                            conn.respond(id, result);
                        });
                    }
                }
            }
        })
    })?;
    Ok((client_read, client_write))
}

fn handle(conn: &Connection, sessions: &Sessions, choose: &Chooser, method: &str, params: Value) -> Result<Value, RpcError> {
    let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": 1,
            "agentCapabilities": { "loadSession": true, "promptCapabilities": { "image": true, "embeddedContext": true } },
            "agentInfo": { "name": "kuana", "title": "Kuana Agent", "version": env!("CARGO_PKG_VERSION") },
            "authMethods": [],
        })),
        "session/new" => {
            let choice = choose().map_err(RpcError::internal)?;
            let cwd = PathBuf::from(params["cwd"].as_str().unwrap_or("."));
            let models = choice.endpoint.models().unwrap_or_default();
            let id = format!("kuana-{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
            let session = Session {
                cwd,
                messages: Vec::new(),
                mode: Mode::Ask,
                model: None,
                models,
                allowed: HashSet::new(),
                cancelled: Arc::default(),
            };
            let options = config_options(&session, &choice.model);
            sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), session);
            Ok(json!({ "sessionId": id, "modes": modes_json(Mode::Ask), "configOptions": options }))
        }
        "session/load" => {
            let kept: Kept = kept_file(&session_id)
                .and_then(|path| std::fs::read(path).ok())
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .ok_or_else(not_found)?;
            let choice = choose().map_err(RpcError::internal)?;
            let session = Session {
                cwd: params["cwd"].as_str().map_or(kept.cwd, PathBuf::from),
                messages: kept.messages,
                mode: kept.mode,
                model: kept.model,
                models: choice.endpoint.models().unwrap_or_default(),
                allowed: HashSet::new(),
                cancelled: Arc::default(),
            };
            let (mode, options) = (session.mode, config_options(&session, &choice.model));
            sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(session_id, session);
            Ok(json!({ "modes": modes_json(mode), "configOptions": options }))
        }
        "session/set_mode" => {
            let mode = Mode::from_id(params["modeId"].as_str().unwrap_or_default()).ok_or_else(|| RpcError::new(RpcError::INVALID_PARAMS, "Unknown mode"))?;
            let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
            let session = sessions.get_mut(&session_id).ok_or_else(not_found)?;
            session.mode = mode;
            conn.notify("session/update", json!({ "sessionId": session_id, "update": { "sessionUpdate": "current_mode_update", "currentModeId": mode.id() } }));
            Ok(json!({}))
        }
        "session/set_config_option" => {
            let value = params["value"].as_str().unwrap_or_default().to_string();
            let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
            let session = sessions.get_mut(&session_id).ok_or_else(not_found)?;
            if params["configId"] != "model" {
                return Err(RpcError::new(RpcError::INVALID_PARAMS, "Unknown option"));
            }
            session.model = Some(value);
            let default = choose().map(|c| c.model).unwrap_or_default();
            Ok(json!({ "configOptions": config_options(session, &default) }))
        }
        "session/prompt" => {
            let blocks = params["prompt"].as_array().cloned().unwrap_or_default();
            let (cancelled, model) = {
                let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
                let session = sessions.get_mut(&session_id).ok_or_else(not_found)?;
                session.cancelled.store(false, Ordering::Relaxed);
                let parts = prompt_parts(&blocks);
                // Consecutive user messages (after a turn that ended on
                // tool results) join into one.
                match session.messages.last_mut() {
                    Some(last) if last.role == Role::User => last.content.extend(parts),
                    _ => session.messages.push(Message { role: Role::User, content: parts }),
                }
                (session.cancelled.clone(), session.model.clone())
            };
            let mut choice = choose().map_err(RpcError::internal)?;
            if let Some(model) = model {
                choice.model = model;
            }
            let stop = run_turn(conn, sessions, &session_id, &choice, &cancelled);
            if let Some(session) = sessions.lock().unwrap_or_else(|e| e.into_inner()).get(&session_id) {
                keep(&session_id, session);
            }
            Ok(json!({ "stopReason": stop? }))
        }
        _ => Err(RpcError::new(RpcError::METHOD_NOT_FOUND, format!("Kuana's agent doesn't serve {method}"))),
    }
}

fn not_found() -> RpcError {
    RpcError::new(RpcError::NOT_FOUND, "No such session")
}

fn config_options(session: &Session, default: &str) -> Value {
    let current = session.model.clone().unwrap_or_else(|| default.to_string());
    let mut options: Vec<Value> = session.models.iter().map(|(id, name)| json!({ "value": id, "name": name })).collect();
    if !session.models.iter().any(|(id, _)| *id == current) {
        options.insert(0, json!({ "value": current, "name": current }));
    }
    json!([{ "id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": current, "options": options }])
}

/// A prompt's content blocks as the model's message parts.
fn prompt_parts(blocks: &[Value]) -> Vec<Part> {
    let mut parts = Vec::new();
    for block in blocks {
        match block["type"].as_str().unwrap_or_default() {
            "text" => parts.push(Part::text(block["text"].as_str().unwrap_or_default())),
            "image" => parts.push(Part::Image {
                media_type: block["mimeType"].as_str().unwrap_or("image/png").to_string(),
                data: block["data"].as_str().unwrap_or_default().to_string(),
            }),
            "resource" => {
                let resource = &block["resource"];
                let uri = resource["uri"].as_str().unwrap_or_default();
                let path = uri.strip_prefix("file://").unwrap_or(uri);
                if let Some(text) = resource["text"].as_str() {
                    parts.push(Part::text(format!("<context source=\"{path}\">\n{text}\n</context>")));
                }
            }
            "resource_link" => {
                let uri = block["uri"].as_str().unwrap_or_default();
                parts.push(Part::text(format!("(The user refers to {})", uri.strip_prefix("file://").unwrap_or(uri))));
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        parts.push(Part::text("(empty message)"));
    }
    parts
}

fn system_prompt(cwd: &Path, mode: Mode) -> String {
    let mut prompt = format!(
        "You are Kuana Agent, the coding agent built into the Kuana code editor, working in the user's project at {root} \
         on {os}.\n\n\
         Use the tools to explore and change the project: list_files and search to find things, read_file before \
         editing a file, edit_file for precise changes (replace exact text), write_file for new files or full rewrites, \
         run_command for builds, tests and other commands, get_problems for the errors and warnings Kuana's language \
         servers and linters report. Paths are relative to the project root.\n\n\
         Work carefully: keep changes focused on what was asked, follow the project's style, check problems after \
         editing and run relevant tests when they're quick. If something is ambiguous, ask instead of guessing. \
         End with a short summary of what you changed (or found).",
        root = cwd.display(),
        os = std::env::consts::OS,
    );
    prompt.push_str(match mode {
        Mode::Plan => "\n\nYou are in Plan mode: read and investigate only. Do not try to edit files or run commands; \
                       describe the changes you would make as a plan.",
        Mode::Ask => "\n\nEach edit and command waits for the user's approval; they may reject it, then adapt.",
        Mode::Edit => "\n\nEdits apply without asking; commands wait for the user's approval.",
    });
    for name in INSTRUCTION_FILES {
        if let Ok(text) = std::fs::read_to_string(cwd.join(name)) {
            let text: String = text.chars().take(20_000).collect();
            prompt.push_str(&format!("\n\n<project-instructions file=\"{name}\">\n{text}\n</project-instructions>"));
        }
    }
    prompt
}

fn tool_specs(mode: Mode) -> Vec<ToolSpec> {
    let spec = |name: &str, description: &str, schema: Value| ToolSpec { name: name.into(), description: description.into(), schema };
    let mut tools = vec![
        spec(
            "list_files",
            "List the project's files (honoring .gitignore), optionally under a folder and matching a glob.",
            json!({ "type": "object", "properties": {
                "path": { "type": "string", "description": "A folder, relative to the project root (default: the root)" },
                "glob": { "type": "string", "description": "Only paths matching this glob, e.g. **/*.rs" }
            }, "additionalProperties": false }),
        ),
        spec(
            "search",
            "Search the project's files for text or a regular expression; lists matching lines as path:line: text.",
            json!({ "type": "object", "properties": {
                "query": { "type": "string" },
                "regex": { "type": "boolean", "description": "Treat query as a regular expression (default false)" },
                "case_sensitive": { "type": "boolean" },
                "path": { "type": "string", "description": "Only under this folder" }
            }, "required": ["query"], "additionalProperties": false }),
        ),
        spec(
            "read_file",
            "Read a file as the editor has it (including unsaved edits). Long files: give a 1-based start line and a line count.",
            json!({ "type": "object", "properties": {
                "path": { "type": "string" },
                "line": { "type": "integer", "minimum": 1 },
                "limit": { "type": "integer", "minimum": 1 }
            }, "required": ["path"], "additionalProperties": false }),
        ),
        spec(
            "get_problems",
            "The errors and warnings Kuana shows (language servers, linters), for one file or the whole project.",
            json!({ "type": "object", "properties": { "path": { "type": "string" } }, "additionalProperties": false }),
        ),
    ];
    if mode != Mode::Plan {
        tools.push(spec(
            "edit_file",
            "Replace exact text in a file. old_text must match once (include enough surrounding lines), unless replace_all.",
            json!({ "type": "object", "properties": {
                "path": { "type": "string" },
                "old_text": { "type": "string" },
                "new_text": { "type": "string" },
                "replace_all": { "type": "boolean" }
            }, "required": ["path", "old_text", "new_text"], "additionalProperties": false }),
        ));
        tools.push(spec(
            "write_file",
            "Write a whole file (creating it and its folders if needed).",
            json!({ "type": "object", "properties": { "path": { "type": "string" }, "content": { "type": "string" } },
                "required": ["path", "content"], "additionalProperties": false }),
        ));
        tools.push(spec(
            "run_command",
            "Run a shell command in the project root (or a folder under it) and get its output and exit code.",
            json!({ "type": "object", "properties": {
                "command": { "type": "string" },
                "cwd": { "type": "string", "description": "A folder, relative to the project root" },
                "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": 1800 }
            }, "required": ["command"], "additionalProperties": false }),
        ));
    }
    tools
}

/// Runs model calls and tools until the model ends its turn; the stop
/// reason for `session/prompt`.
fn run_turn(conn: &Connection, sessions: &Sessions, session_id: &str, choice: &ai::Choice, cancelled: &Arc<AtomicBool>) -> Result<&'static str, RpcError> {
    for _ in 0..MAX_STEPS {
        if cancelled.load(Ordering::Relaxed) {
            return Ok("cancelled");
        }
        let (request, cwd, mode) = {
            let sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
            let session = sessions.get(session_id).ok_or_else(not_found)?;
            let request = ai::Request {
                model: choice.model.clone(),
                system: system_prompt(&session.cwd, session.mode),
                messages: session.messages.clone(),
                tools: tool_specs(session.mode),
                max_tokens: MAX_TOKENS,
                effort: choice.effort,
            };
            (request, session.cwd.clone(), session.mode)
        };
        let stream = choice.stream(request);
        let notify = |update: Value| conn.notify("session/update", json!({ "sessionId": session_id, "update": update }));
        let reply = futures::executor::block_on(async {
            let mut acc = ai::message::Accumulator::default();
            let mut events = stream;
            while let Some(event) = events.events.next().await {
                if cancelled.load(Ordering::Relaxed) {
                    return None;
                }
                match &event {
                    StreamEvent::Text { text, .. } => notify(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } })),
                    StreamEvent::Thinking { text, .. } => notify(json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": text } })),
                    _ => {}
                }
                acc.push(&event);
            }
            Some(acc.finish())
        });
        let Some(reply) = reply else { return Ok("cancelled") };
        let reply = reply.map_err(RpcError::internal)?;
        if reply.usage.context() > 0 {
            notify(json!({ "sessionUpdate": "usage_update", "used": reply.usage.context(), "size": context_window(&choice.model) }));
        }
        let calls: Vec<(String, String, Value)> = reply.tool_calls().map(|(id, name, input)| (id.to_string(), name.to_string(), input.clone())).collect();
        with_session(sessions, session_id, |s| s.messages.push(Message { role: Role::Assistant, content: reply.parts.clone() }))?;
        if !calls.is_empty() {
            // Every call gets a result, or the conversation can't go on.
            let mut results = Vec::new();
            for (id, name, input) in calls {
                let (content, is_error) = if reply.invalid_tool_inputs.contains(&id) {
                    ("The arguments were cut off or weren't valid JSON; try again with a smaller change.".to_string(), true)
                } else if cancelled.load(Ordering::Relaxed) {
                    ("Stopped by the user.".to_string(), true)
                } else {
                    let tools = Tools { conn, session_id, sessions, cwd: &cwd, mode, cancelled };
                    tools.run(&id, &name, &input)
                };
                results.push(Part::ToolResult { id, content: clip(&content), is_error });
            }
            with_session(sessions, session_id, |s| s.messages.push(Message { role: Role::User, content: results }))?;
            if cancelled.load(Ordering::Relaxed) {
                return Ok("cancelled");
            }
            if reply.stop == StopReason::ToolUse {
                continue;
            }
        }
        return Ok(match reply.stop {
            StopReason::PauseTurn => continue,
            StopReason::MaxTokens => "max_tokens",
            StopReason::Refusal => "refusal",
            _ => "end_turn",
        });
    }
    Ok("max_turn_requests")
}

fn with_session<T>(sessions: &Sessions, id: &str, f: impl FnOnce(&mut Session) -> T) -> Result<T, RpcError> {
    let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
    sessions.get_mut(id).map(f).ok_or_else(not_found)
}

/// Tokens a model's context holds, for the session's meter.
fn context_window(model: &str) -> u64 {
    if crate::ai::anthropic::adaptive_thinking(model) { 1_000_000 } else { 200_000 }
}

fn clip(text: &str) -> String {
    if text.len() <= MAX_RESULT {
        return text.to_string();
    }
    let mut end = MAX_RESULT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (cut: {} more bytes)", &text[..end], text.len() - end)
}

struct Tools<'a> {
    conn: &'a Connection,
    session_id: &'a str,
    sessions: &'a Sessions,
    cwd: &'a Path,
    mode: Mode,
    cancelled: &'a Arc<AtomicBool>,
}

impl Tools<'_> {
    fn notify(&self, update: Value) {
        self.conn.notify("session/update", json!({ "sessionId": self.session_id, "update": update }));
    }

    fn request(&self, method: &str, mut params: Value) -> Result<Value, RpcError> {
        params["sessionId"] = json!(self.session_id);
        futures::executor::block_on(self.conn.request(method, params))
    }

    fn path(&self, input: &Value, key: &str) -> Result<PathBuf, String> {
        let given = input[key].as_str().filter(|p| !p.trim().is_empty()).ok_or_else(|| format!("`{key}` is required"))?;
        let path = Path::new(given);
        Ok(if path.is_absolute() { path.to_path_buf() } else { self.cwd.join(path) })
    }

    fn rel(&self, path: &Path) -> String {
        path.strip_prefix(self.cwd).unwrap_or(path).display().to_string()
    }

    /// Runs one tool call, reporting it to the client as it goes.
    fn run(&self, id: &str, name: &str, input: &Value) -> (String, bool) {
        let (title, kind, locations) = self.describe(name, input);
        self.notify(json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": kind, "status": "in_progress",
            "locations": locations, "rawInput": input }));
        let result = match name {
            "list_files" => self.list_files(input),
            "search" => self.search(input),
            "read_file" => self.read_file(input),
            "get_problems" => self.get_problems(input),
            "edit_file" | "write_file" if self.mode != Mode::Plan => self.write(id, name, input),
            "run_command" if self.mode != Mode::Plan => self.run_command(id, input),
            _ => Err(format!("No tool named {name} here")),
        };
        let (text, failed) = match result {
            Ok(text) => (text, false),
            Err(error) => (error, true),
        };
        // The terminal's or diff's content stays; a text result shows short.
        let mut update = json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": if failed { "failed" } else { "completed" } });
        if failed || matches!(name, "list_files" | "search" | "get_problems") {
            let shown: String = text.lines().take(40).collect::<Vec<_>>().join("\n");
            update["content"] = json!([{ "type": "content", "content": { "type": "text", "text": shown } }]);
        }
        self.notify(update);
        (text, failed)
    }

    fn describe(&self, name: &str, input: &Value) -> (String, &'static str, Value) {
        let path = self.path(input, "path").ok();
        let rel = path.as_deref().map(|p| self.rel(p)).unwrap_or_default();
        let locations = path.as_ref().map_or(json!([]), |p| json!([{ "path": p, "line": input["line"].as_u64() }]));
        match name {
            "list_files" => (format!("List {}", if rel.is_empty() { "files".into() } else { format!("`{rel}`") }), "search", json!([])),
            "search" => (format!("Search for `{}`", input["query"].as_str().unwrap_or_default()), "search", json!([])),
            "read_file" => (format!("Read `{rel}`"), "read", locations),
            "get_problems" => (if rel.is_empty() { "Check problems".into() } else { format!("Check problems in `{rel}`") }, "read", locations),
            "edit_file" => (format!("Edit `{rel}`"), "edit", locations),
            "write_file" => (format!("Write `{rel}`"), "edit", locations),
            "run_command" => (format!("Run `{}`", input["command"].as_str().unwrap_or_default()), "execute", json!([])),
            other => (other.to_string(), "other", json!([])),
        }
    }

    fn list_files(&self, input: &Value) -> Result<String, String> {
        let root = match input["path"].as_str() {
            Some(_) => self.path(input, "path")?,
            None => self.cwd.to_path_buf(),
        };
        let glob = match input["glob"].as_str() {
            Some(g) => Some(globset::Glob::new(g).map_err(|e| e.to_string())?.compile_matcher()),
            None => None,
        };
        let files = crate::search::project_files(&root);
        let mut out = Vec::new();
        for file in &files {
            let rel = self.rel(file);
            if glob.as_ref().is_none_or(|g| g.is_match(&rel)) {
                out.push(rel);
            }
        }
        let total = out.len();
        out.truncate(2000);
        let mut text = out.join("\n");
        if total > 2000 {
            text.push_str(&format!("\n… and {} more (narrow it with path or glob)", total - 2000));
        }
        Ok(if text.is_empty() { "No files".into() } else { text })
    }

    fn search(&self, input: &Value) -> Result<String, String> {
        let query = input["query"].as_str().unwrap_or_default();
        let options = crate::search::SearchOptions {
            case_sensitive: input["case_sensitive"].as_bool().unwrap_or(false),
            whole_word: false,
            regex: input["regex"].as_bool().unwrap_or(false),
        };
        let query = crate::search::SearchQuery::new(query, options)?.ok_or("The query is empty")?;
        let root = match input["path"].as_str() {
            Some(_) => self.path(input, "path")?,
            None => self.cwd.to_path_buf(),
        };
        let files = crate::search::project_files(&root);
        let mut lines = Vec::new();
        let label = crate::search::relative_to(self.cwd);
        let end = crate::search::search_project(&label, files, &query, &HashMap::new(), self.cancelled, |found, _| {
            for m in found {
                if lines.len() < 300 {
                    lines.push(format!("{}:{}: {}", m.relative, m.line + 1, m.line_text.trim()));
                }
            }
        });
        let mut text = lines.join("\n");
        if end.truncated || lines.len() >= 300 {
            text.push_str("\n… more matches (narrow the search)");
        }
        Ok(if text.is_empty() { "No matches".into() } else { text })
    }

    fn read_file(&self, input: &Value) -> Result<String, String> {
        let path = self.path(input, "path")?;
        let mut params = json!({ "path": path });
        if let Some(line) = input["line"].as_u64() {
            params["line"] = json!(line);
        }
        if let Some(limit) = input["limit"].as_u64() {
            params["limit"] = json!(limit);
        }
        let answer = self.request("fs/read_text_file", params).map_err(|e| format!("Couldn't read {}: {e}", self.rel(&path)))?;
        let content = answer["content"].as_str().unwrap_or_default();
        let first = input["line"].as_u64().unwrap_or(1).max(1) as usize;
        // Numbered, so edits can refer to what's where.
        let numbered: Vec<String> = content.lines().enumerate().map(|(i, l)| format!("{:>5}\t{l}", first + i)).collect();
        Ok(if numbered.is_empty() { "(empty file)".into() } else { numbered.join("\n") })
    }

    fn get_problems(&self, input: &Value) -> Result<String, String> {
        let mut params = json!({});
        if input["path"].is_string() {
            params["path"] = json!(self.path(input, "path")?);
        }
        let answer = self.request("_kuana/problems", params).map_err(|e| format!("Couldn't get problems: {e}"))?;
        let problems = answer["problems"].as_array().cloned().unwrap_or_default();
        if problems.is_empty() {
            return Ok("No problems".into());
        }
        Ok(problems
            .iter()
            .take(200)
            .map(|p| {
                let path = p["path"].as_str().map(|s| self.rel(Path::new(s))).unwrap_or_default();
                format!("{path}:{}: {}: {}", p["line"].as_u64().unwrap_or(0), p["severity"].as_str().unwrap_or("error"), p["message"].as_str().unwrap_or_default())
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// `edit_file` / `write_file`: the new text, asked for when the mode
    /// says, then written through the client.
    fn write(&self, id: &str, name: &str, input: &Value) -> Result<String, String> {
        let path = self.path(input, "path")?;
        let rel = self.rel(&path);
        let old = match self.request("fs/read_text_file", json!({ "path": path })) {
            Ok(answer) => Some(answer["content"].as_str().unwrap_or_default().to_string()),
            Err(e) if e.code == RpcError::NOT_FOUND || e.message.to_lowercase().contains("no such file") || e.message.to_lowercase().contains("not found") => None,
            Err(e) if name == "write_file" => {
                // A file the client can't read may still be writable as new.
                let _ = e;
                None
            }
            Err(e) => return Err(format!("Couldn't read {rel}: {e}")),
        };
        let new = if name == "edit_file" {
            let old_text = input["old_text"].as_str().unwrap_or_default();
            let new_text = input["new_text"].as_str().unwrap_or_default();
            let current = old.as_deref().ok_or_else(|| format!("{rel} doesn't exist; use write_file to create it"))?;
            replace(current, old_text, new_text, input["replace_all"].as_bool().unwrap_or(false)).map_err(|e| format!("{e} in {rel}"))?
        } else {
            input["content"].as_str().unwrap_or_default().to_string()
        };
        if old.as_deref() == Some(new.as_str()) {
            return Ok(format!("{rel} already has that text; nothing changed"));
        }
        let diff = json!([{ "type": "diff", "path": path, "oldText": old, "newText": new }]);
        self.notify(json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "content": diff }));
        if self.mode == Mode::Ask && !self.allowed("edit") {
            self.ask(id, &format!("{} `{rel}`", if old.is_some() { "Edit" } else { "Create" }), "edit", diff, "Always allow edits", "edit")?;
        }
        self.request("fs/write_text_file", json!({ "path": path, "content": new })).map_err(|e| format!("Couldn't write {rel}: {e}"))?;
        let lines = new.lines().count();
        Ok(match old {
            Some(_) => format!("Edited {rel}"),
            None => format!("Created {rel} ({lines} lines)"),
        })
    }

    fn allowed(&self, key: &str) -> bool {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).get(self.session_id).is_some_and(|s| s.allowed.contains(key))
    }

    /// Asks the user; Err when they reject (or stopped the turn).
    fn ask(&self, id: &str, title: &str, kind: &str, content: Value, always: &str, always_key: &str) -> Result<(), String> {
        let answer = self.request(
            "session/request_permission",
            json!({
                "toolCall": { "toolCallId": id, "title": title, "kind": kind, "status": "pending", "content": content },
                "options": [
                    { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "always", "name": always, "kind": "allow_always" },
                    { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
                ],
            }),
        );
        let outcome = answer.map_err(|e| format!("Couldn't ask the user: {e}"))?;
        match outcome["outcome"]["optionId"].as_str() {
            Some("allow") => Ok(()),
            Some("always") => {
                if let Some(s) = self.sessions.lock().unwrap_or_else(|e| e.into_inner()).get_mut(self.session_id) {
                    s.allowed.insert(always_key.to_string());
                }
                Ok(())
            }
            _ if self.cancelled.load(Ordering::Relaxed) => Err("Stopped by the user.".into()),
            _ => Err("The user rejected this. Ask what they'd like instead, or try another way.".into()),
        }
    }

    fn run_command(&self, id: &str, input: &Value) -> Result<String, String> {
        let command = input["command"].as_str().filter(|c| !c.trim().is_empty()).ok_or("`command` is required")?;
        let cwd = match input["cwd"].as_str() {
            Some(_) => self.path(input, "cwd")?,
            None => self.cwd.to_path_buf(),
        };
        let program = command.split_whitespace().next().unwrap_or_default().rsplit('/').next().unwrap_or_default().to_string();
        let key = format!("run:{program}");
        if !self.allowed(&key) {
            self.ask(id, &format!("Run `{command}`"), "execute", json!([]), &format!("Always allow `{program}`"), &key)?;
        }
        let (shell, args) = if cfg!(windows) {
            ("powershell".to_string(), vec!["-NoProfile".to_string(), "-Command".to_string(), command.to_string()])
        } else {
            ("sh".to_string(), vec!["-c".to_string(), command.to_string()])
        };
        let created = self
            .request("terminal/create", json!({ "command": shell, "args": args, "cwd": cwd, "outputByteLimit": 256 * 1024 }))
            .map_err(|e| format!("Couldn't run it: {e}"))?;
        let terminal = created["terminalId"].as_str().unwrap_or_default().to_string();
        self.notify(json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "content": [{ "type": "terminal", "terminalId": terminal }] }));
        let timeout = Duration::from_secs(input["timeout_seconds"].as_u64().unwrap_or(COMMAND_TIMEOUT).clamp(1, 1800));
        let waited = self.wait(&terminal, timeout);
        let output = self.request("terminal/output", json!({ "terminalId": terminal })).unwrap_or_default();
        let _ = self.request("terminal/release", json!({ "terminalId": terminal }));
        let text = output["output"].as_str().unwrap_or_default().to_string();
        let truncated = if output["truncated"].as_bool() == Some(true) { "(output cut to its end)\n" } else { "" };
        match waited {
            Waited::Exited(code) => {
                let status = code.map_or("ended by a signal".to_string(), |c| format!("exit code {c}"));
                Ok(format!("{truncated}{text}\n[{status}]"))
            }
            Waited::TimedOut => Ok(format!("{truncated}{text}\n[stopped after {}s: still running]", timeout.as_secs())),
            Waited::Cancelled => Err("Stopped by the user.".into()),
        }
    }

    /// Waits for a terminal to exit, killing it after `timeout` or when the
    /// turn is stopped.
    fn wait(&self, terminal: &str, timeout: Duration) -> Waited {
        let waiting = {
            let mut params = json!({ "terminalId": terminal });
            params["sessionId"] = json!(self.session_id);
            self.conn.request("terminal/wait_for_exit", params)
        };
        let (tick_tx, tick_rx) = oneshot::channel::<()>();
        let deadline = std::time::Instant::now() + timeout;
        let flag = self.cancelled.clone();
        // A watcher: wakes the wait when time's up or the turn is stopped,
        // and ends by itself once the command exits (the receiver drops).
        let _ = std::thread::Builder::new().name("kuana agent timer".into()).spawn(move || {
            while std::time::Instant::now() < deadline && !flag.load(Ordering::Relaxed) && !tick_tx.is_canceled() {
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = tick_tx.send(());
        });
        let result = futures::executor::block_on(async {
            futures::pin_mut!(waiting);
            match futures::future::select(waiting, tick_rx).await {
                futures::future::Either::Left((answer, _)) => Some(answer),
                futures::future::Either::Right(_) => None,
            }
        });
        match result {
            Some(Ok(answer)) => Waited::Exited(answer["exitCode"].as_i64()),
            Some(Err(_)) => Waited::Exited(None),
            None => {
                let _ = self.request("terminal/kill", json!({ "terminalId": terminal }));
                if self.cancelled.load(Ordering::Relaxed) { Waited::Cancelled } else { Waited::TimedOut }
            }
        }
    }
}

enum Waited {
    Exited(Option<i64>),
    TimedOut,
    Cancelled,
}

/// `old` replaced by `new` in `text`: once (it must occur exactly once), or
/// everywhere.
fn replace(text: &str, old: &str, new: &str, all: bool) -> Result<String, String> {
    if old.is_empty() {
        return Err("old_text is empty".into());
    }
    let count = text.matches(old).count();
    match count {
        0 => {
            // Line endings differ (CRLF file, LF text): try with the file's.
            if text.contains("\r\n") && !old.contains("\r\n") {
                let (old_crlf, new_crlf) = (old.replace('\n', "\r\n"), new.replace('\n', "\r\n"));
                if text.contains(&old_crlf) {
                    return replace(text, &old_crlf, &new_crlf, all);
                }
            }
            Err("old_text wasn't found (it must match exactly, whitespace included; read the file again)".into())
        }
        1 => Ok(text.replacen(old, new, 1)),
        n if all => {
            let _ = n;
            Ok(text.replace(old, new))
        }
        n => Err(format!("old_text matches {n} places; include more surrounding text, or set replace_all")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacements_must_be_unique_unless_all() {
        assert_eq!(replace("a b a", "b", "c", false).unwrap(), "a c a");
        assert!(replace("a b a", "a", "c", false).unwrap_err().contains("2 places"));
        assert_eq!(replace("a b a", "a", "c", true).unwrap(), "c b c");
        assert!(replace("abc", "x", "y", false).unwrap_err().contains("wasn't found"));
        assert_eq!(replace("one\r\ntwo\r\n", "one\ntwo", "1\n2", false).unwrap(), "1\r\n2\r\n");
        assert!(replace("abc", "", "y", false).is_err());
    }

    #[test]
    fn prompts_become_parts() {
        let parts = prompt_parts(&[
            json!({ "type": "text", "text": "Fix this" }),
            json!({ "type": "resource", "resource": { "uri": "file:///p/a.rs", "text": "fn main() {}" } }),
            json!({ "type": "resource_link", "uri": "file:///p/b.rs", "name": "b.rs" }),
        ]);
        assert_eq!(parts[0], Part::text("Fix this"));
        assert_eq!(parts[1], Part::text("<context source=\"/p/a.rs\">\nfn main() {}\n</context>"));
        assert_eq!(parts[2], Part::text("(The user refers to /p/b.rs)"));
        assert_eq!(prompt_parts(&[]), vec![Part::text("(empty message)")]);
    }

    #[test]
    fn plan_mode_offers_only_reading_tools() {
        let names = |mode| tool_specs(mode).into_iter().map(|t| t.name).collect::<Vec<_>>();
        assert!(!names(Mode::Plan).contains(&"edit_file".to_string()));
        assert!(names(Mode::Ask).contains(&"run_command".to_string()));
        assert!(system_prompt(Path::new("/p"), Mode::Plan).contains("Plan mode"));
    }

    /// The agent end to end over ACP, against a fake provider: it reads a
    /// file through the client, asks to edit it, writes it, and finishes.
    #[test]
    fn a_turn_reads_asks_and_writes_through_the_client() {
        use crate::acp::{Agent, FromAgent};
        use crate::ai::stream::test_server;

        let sse = |events: Vec<(&str, Value)>| events.into_iter().map(|(n, d)| format!("event: {n}\ndata: {d}\n\n")).collect::<String>();
        let tool_turn = |id: &str, name: &str, input: Value| {
            sse(vec![
                ("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } })),
                ("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": input.to_string() } })),
                ("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 5 } })),
            ])
        };
        let server = test_server::start(vec![
            (200, "application/json", json!({ "data": [{ "id": "claude-opus-5-5" }] }).to_string()),
            (200, "text/event-stream", tool_turn("t1", "read_file", json!({ "path": "a.txt" }))),
            (200, "text/event-stream", tool_turn("t2", "edit_file", json!({ "path": "a.txt", "old_text": "hello", "new_text": "goodbye" }))),
            (200, "text/event-stream", sse(vec![
                ("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
                ("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Changed it." } })),
                ("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } })),
            ])),
        ]);
        // Anthropic's format, sent to the fake server.
        let url = server.url.clone();
        let choose: Chooser = Arc::new(move || {
            let endpoint = ai::Endpoint { provider: ai::providers::info("anthropic").unwrap(), url: url.clone(), key: Some("test-key".into()) };
            Ok(ai::Choice { endpoint, model: "claude-opus-5-5".into(), effort: None })
        });
        let (read, write) = start(choose).unwrap();
        futures::executor::block_on(async {
            let (agent, mut events) = Agent::connect(read, write).await.unwrap();
            let session = agent.new_session(Path::new("/proj"), Vec::new()).await.unwrap();
            let id = session.session_id.to_string();
            let turn = agent.prompt(&id, vec![json!({ "type": "text", "text": "Say goodbye" })]);
            let mut written = None;
            let mut text = String::new();
            let serve = async {
                while let Some(event) = events.next().await {
                    match event {
                        FromAgent::ReadFile(request, responder) => {
                            assert_eq!(request.path, Path::new("/proj/a.txt"));
                            responder.ok(json!({ "content": "hello world\n" }));
                        }
                        FromAgent::Permission(request, responder) => {
                            assert_eq!(request.tool_call.fields.title.as_deref(), Some("Edit `a.txt`"));
                            responder.ok(json!({ "outcome": { "outcome": "selected", "optionId": "allow" } }));
                        }
                        FromAgent::WriteFile(request, responder) => {
                            written = Some(request.content.clone());
                            responder.ok(json!({}));
                        }
                        FromAgent::Update(update) => {
                            if let crate::acp::schema::SessionUpdate::AgentMessageChunk(chunk) = update.update {
                                text.push_str(&crate::agents::transcript::text_of(&chunk.content));
                            }
                        }
                        _ => {}
                    }
                }
            };
            let (turn, serve) = (std::pin::pin!(turn), std::pin::pin!(serve));
            let result = match futures::future::select(turn, serve).await {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right((_, turn)) => turn.await,
            };
            assert_eq!(result.unwrap().stop_reason, crate::acp::schema::StopReason::EndTurn);
            assert_eq!(written.as_deref(), Some("goodbye world\n"));
            assert_eq!(text, "Changed it.");
        });
        // The second model call carried the file back as a tool result.
        let requests = server.requests.lock().unwrap();
        let second: Value = serde_json::from_str(&requests[2].1).unwrap();
        let result = &second["messages"][2]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert!(result["content"].as_str().unwrap().contains("hello world"));
    }
}
