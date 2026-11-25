//! Kuana's side of an ACP connection: starting the agent, the handshake,
//! sessions and prompts, and what the agent asks of Kuana, typed, each
//! request with a [`Responder`] that answers it (dropped unanswered, it
//! answers with an error, so an agent never waits forever).

use std::{
    collections::VecDeque,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{Arc, Mutex},
};

use agent_client_protocol_schema::{ProtocolVersion, v1 as schema};
use futures::{StreamExt as _, stream::BoxStream};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::rpc::{Connection, Incoming, RpcError};

/// The ACP major version Kuana speaks.
pub const PROTOCOL_VERSION: u16 = 1;
/// Lines of an agent's stderr kept, to explain why it stopped.
const STDERR_LINES: usize = 80;

/// How to start an agent's process.
#[derive(Clone, Debug, PartialEq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// What the agent asks of Kuana.
pub enum FromAgent {
    Update(schema::SessionNotification),
    Permission(schema::RequestPermissionRequest, Responder),
    ReadFile(schema::ReadTextFileRequest, Responder),
    WriteFile(schema::WriteTextFileRequest, Responder),
    CreateTerminal(schema::CreateTerminalRequest, Responder),
    TerminalOutput(schema::TerminalOutputRequest, Responder),
    WaitForExit(schema::WaitForTerminalExitRequest, Responder),
    KillTerminal(schema::KillTerminalRequest, Responder),
    ReleaseTerminal(schema::ReleaseTerminalRequest, Responder),
    /// An extension request (`_kuana/…`), for Kuana's own agent.
    Ext { method: String, params: Value, responder: Responder },
}

/// Answers one request from the agent, once.
pub struct Responder {
    id: Value,
    rpc: Connection,
    answered: bool,
}

impl Responder {
    pub fn ok(mut self, result: impl Serialize) {
        self.answered = true;
        let value = serde_json::to_value(result).unwrap_or(Value::Null);
        self.rpc.respond(self.id.clone(), Ok(value));
    }

    pub fn err(mut self, error: RpcError) {
        self.answered = true;
        self.rpc.respond(self.id.clone(), Err(error));
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        if !self.answered {
            self.rpc.respond(self.id.clone(), Err(RpcError::internal("Kuana couldn't answer")));
        }
    }
}

pub type Events = BoxStream<'static, FromAgent>;

/// A connected, initialized agent.
pub struct Agent {
    rpc: Connection,
    child: Mutex<Option<Child>>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    pub init: schema::InitializeResponse,
}

impl Agent {
    /// Starts the agent's process and initializes it.
    pub async fn spawn(launch: &Launch) -> Result<(Agent, Events), String> {
        let mut command = crate::process::command(&launch.program);
        command.args(&launch.args).current_dir(&launch.cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for (name, value) in &launch.env {
            command.env(name, value);
        }
        let mut child = command.spawn().map_err(|e| format!("Couldn't start {}: {e}", launch.program.display()))?;
        let (Some(stdin), Some(stdout), stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
            return Err("The agent's pipes didn't open".into());
        };
        let tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
        if let Some(stderr) = stderr {
            let tail = tail.clone();
            let _ = std::thread::Builder::new().name("agent stderr".into()).spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
                    if tail.len() == STDERR_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            });
        }
        let (rpc, incoming) = Connection::start(stdout, stdin);
        let agent = Agent::initialize(rpc, incoming, Some(child), tail).await;
        agent.map_err(|(error, tail)| match tail {
            Some(tail) if !tail.is_empty() => format!("{error}\n\n{tail}"),
            _ => error,
        })
    }

    /// An agent on the other end of `input` / `output` (Kuana's own agent,
    /// tests), initialized.
    pub async fn connect(input: impl std::io::Read + Send + 'static, output: impl std::io::Write + Send + 'static) -> Result<(Agent, Events), String> {
        let (rpc, incoming) = Connection::start(input, output);
        Agent::initialize(rpc, incoming, None, Arc::default()).await.map_err(|(e, _)| e)
    }

    async fn initialize(
        rpc: Connection,
        incoming: futures::channel::mpsc::UnboundedReceiver<Incoming>,
        child: Option<Child>,
        stderr: Arc<Mutex<VecDeque<String>>>,
    ) -> Result<(Agent, Events), (String, Option<String>)> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": { "readTextFile": true, "writeTextFile": true },
                "terminal": true,
                "auth": { "terminal": true },
            },
            "clientInfo": { "name": "kuana", "title": "Kuana", "version": env!("CARGO_PKG_VERSION") },
        });
        let mut agent = Agent { rpc: rpc.clone(), child: Mutex::new(child), stderr, init: empty_init() };
        let answer = rpc.request("initialize", params).await;
        let init: schema::InitializeResponse = match answer.map_err(|e| e.message).and_then(|v| parse(v).map_err(|e| e.message)) {
            Ok(init) => init,
            Err(error) => return Err((format!("The agent didn't start: {error}"), Some(agent.stderr_tail()))),
        };
        if init.protocol_version.as_u16() != PROTOCOL_VERSION {
            return Err((format!("The agent speaks ACP version {}, Kuana speaks {PROTOCOL_VERSION}", init.protocol_version.as_u16()), None));
        }
        agent.init = init;
        let events = incoming.filter_map(move |incoming| futures::future::ready(from_agent(&rpc, incoming))).boxed();
        Ok((agent, events))
    }

    pub fn is_closed(&self) -> bool {
        self.rpc.is_closed()
    }

    /// The last lines the agent wrote to stderr (why it failed, often).
    pub fn stderr_tail(&self) -> String {
        self.stderr.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect::<Vec<_>>().join("\n")
    }

    pub fn authenticate(&self, method: &str) -> impl Future<Output = Result<(), RpcError>> + use<> {
        let answer = self.rpc.request("authenticate", json!({ "methodId": method }));
        async move { answer.await.map(|_| ()) }
    }

    pub fn new_session(&self, cwd: &Path, mcp_servers: Vec<Value>) -> impl Future<Output = Result<schema::NewSessionResponse, RpcError>> + use<> {
        typed(self.rpc.request("session/new", json!({ "cwd": cwd, "mcpServers": mcp_servers })))
    }

    pub fn load_session(&self, id: &str, cwd: &Path, mcp_servers: Vec<Value>) -> impl Future<Output = Result<schema::LoadSessionResponse, RpcError>> + use<> {
        typed(self.rpc.request("session/load", json!({ "sessionId": id, "cwd": cwd, "mcpServers": mcp_servers })))
    }

    /// Sends a prompt; the future ends with the turn.
    pub fn prompt(&self, session: &str, prompt: Vec<Value>) -> impl Future<Output = Result<schema::PromptResponse, RpcError>> + use<> {
        typed(self.rpc.request("session/prompt", json!({ "sessionId": session, "prompt": prompt })))
    }

    pub fn cancel(&self, session: &str) {
        self.rpc.notify("session/cancel", json!({ "sessionId": session }));
    }

    pub fn set_mode(&self, session: &str, mode: &str) -> impl Future<Output = Result<(), RpcError>> + use<> {
        let answer = self.rpc.request("session/set_mode", json!({ "sessionId": session, "modeId": mode }));
        async move { answer.await.map(|_| ()) }
    }

    /// Sets a session option; the answer is the whole new set.
    pub fn set_config_option(&self, session: &str, config: &str, value: Value) -> impl Future<Output = Result<schema::SetSessionConfigOptionResponse, RpcError>> + use<> {
        let mut params = json!({ "sessionId": session, "configId": config, "value": value });
        if value.is_boolean() {
            params["type"] = json!("boolean");
        }
        typed(self.rpc.request("session/set_config_option", params))
    }

    /// Ends the agent's process (and what it started, on Windows).
    pub fn stop(&self) {
        if let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            crate::process::kill_tree(&mut child);
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.stop();
    }
}

fn empty_init() -> schema::InitializeResponse {
    schema::InitializeResponse::new(ProtocolVersion::V1)
}

fn parse<T: DeserializeOwned>(value: Value) -> Result<T, RpcError> {
    serde_json::from_value(value).map_err(|e| RpcError::new(RpcError::INVALID_PARAMS, format!("Unexpected answer from the agent: {e}")))
}

async fn typed<T: DeserializeOwned>(answer: impl Future<Output = Result<Value, RpcError>>) -> Result<T, RpcError> {
    answer.await.and_then(parse)
}

/// Types what the agent sent; requests Kuana doesn't serve are refused on
/// the spot.
fn from_agent(rpc: &Connection, incoming: Incoming) -> Option<FromAgent> {
    match incoming {
        Incoming::Notification { method, params } => match method.as_str() {
            "session/update" => serde_json::from_value(params).ok().map(FromAgent::Update),
            _ => None,
        },
        Incoming::Request { id, method, params } => {
            let responder = Responder { id: id.clone(), rpc: rpc.clone(), answered: false };
            macro_rules! typed {
                ($variant:ident) => {
                    match serde_json::from_value(params) {
                        Ok(request) => Some(FromAgent::$variant(request, responder)),
                        Err(e) => {
                            responder.err(RpcError::new(RpcError::INVALID_PARAMS, e.to_string()));
                            None
                        }
                    }
                };
            }
            match method.as_str() {
                "session/request_permission" => typed!(Permission),
                "fs/read_text_file" => typed!(ReadFile),
                "fs/write_text_file" => typed!(WriteFile),
                "terminal/create" => typed!(CreateTerminal),
                "terminal/output" => typed!(TerminalOutput),
                "terminal/wait_for_exit" => typed!(WaitForExit),
                "terminal/kill" => typed!(KillTerminal),
                "terminal/release" => typed!(ReleaseTerminal),
                _ if method.starts_with("_kuana/") => Some(FromAgent::Ext { method, params, responder }),
                _ => {
                    responder.err(RpcError::new(RpcError::METHOD_NOT_FOUND, format!("Kuana doesn't serve {method}")));
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::testing::FakeAgent;

    #[test]
    fn handshake_session_prompt_and_requests_to_kuana() {
        let fake = FakeAgent::start(|agent, method, params| match method {
            "session/new" => {
                assert!(params["cwd"].as_str().unwrap().ends_with("proj"));
                Ok(json!({ "sessionId": "s1", "modes": { "currentModeId": "ask", "availableModes": [{ "id": "ask", "name": "Ask" }, { "id": "code", "name": "Code" }] } }))
            }
            "session/prompt" => {
                agent.notify("session/update", json!({ "sessionId": "s1", "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "On it." } } }));
                let read = agent.request("fs/read_text_file", json!({ "sessionId": "s1", "path": "/proj/a.rs" }));
                let content = futures::executor::block_on(read).unwrap();
                assert_eq!(content["content"], "fn main() {}");
                let bogus = futures::executor::block_on(agent.request("elicitation/create", json!({})));
                assert_eq!(bogus.unwrap_err().code, RpcError::METHOD_NOT_FOUND);
                Ok(json!({ "stopReason": "end_turn" }))
            }
            _ => Err(RpcError::new(RpcError::METHOD_NOT_FOUND, "no")),
        });
        futures::executor::block_on(async {
            let (agent, mut events) = fake.connect().await;
            assert_eq!(agent.init.agent_info.as_ref().map(|i| i.name.as_str()), Some("fake"));
            let session = agent.new_session(Path::new("/proj"), Vec::new()).await.unwrap();
            assert_eq!(session.session_id.to_string(), "s1");
            assert_eq!(session.modes.unwrap().available_modes.len(), 2);
            let turn = agent.prompt("s1", vec![json!({ "type": "text", "text": "hi" })]);
            // Kuana answers the agent while the turn runs, and keeps reading:
            // requests Kuana refuses are refused as they're read.
            let serve = async {
                while let Some(event) = events.next().await {
                    match event {
                        FromAgent::Update(update) => assert!(matches!(update.update, schema::SessionUpdate::AgentMessageChunk(_))),
                        FromAgent::ReadFile(request, responder) => {
                            assert_eq!(request.path, Path::new("/proj/a.rs"));
                            responder.ok(json!({ "content": "fn main() {}" }));
                        }
                        _ => panic!("unexpected request"),
                    }
                }
            };
            let result = until(turn, serve).await;
            assert_eq!(result.unwrap().stop_reason, schema::StopReason::EndTurn);
        });
    }

    #[test]
    fn a_dropped_responder_answers_with_an_error() {
        let fake = FakeAgent::start(|agent, method, _| match method {
            "session/prompt" => {
                let asked = agent.request("session/request_permission", json!({
                    "sessionId": "s1",
                    "toolCall": { "toolCallId": "t1", "title": "Write a.rs" },
                    "options": [{ "optionId": "allow", "name": "Allow", "kind": "allow_once" }],
                }));
                let answer = futures::executor::block_on(asked);
                Ok(json!({ "stopReason": if answer.is_err() { "refusal" } else { "end_turn" } }))
            }
            _ => Ok(json!({})),
        });
        futures::executor::block_on(async {
            let (agent, mut events) = fake.connect().await;
            let turn = agent.prompt("s1", vec![]);
            let serve = async {
                if let Some(FromAgent::Permission(request, responder)) = events.next().await {
                    assert_eq!(request.options[0].option_id.to_string(), "allow");
                    drop(responder);
                }
            };
            let result = until(turn, serve).await;
            assert_eq!(result.unwrap().stop_reason, schema::StopReason::Refusal);
        });
    }

    /// `turn`'s result, `serve` running beside it until then.
    async fn until<T>(turn: impl Future<Output = T>, serve: impl Future<Output = ()>) -> T {
        let (turn, serve) = (std::pin::pin!(turn), std::pin::pin!(serve));
        match futures::future::select(turn, serve).await {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right(((), turn)) => turn.await,
        }
    }

    #[test]
    fn a_missing_program_says_so() {
        let launch = Launch { program: "/no/such/agent".into(), args: vec![], env: vec![], cwd: std::env::temp_dir() };
        let error = futures::executor::block_on(Agent::spawn(&launch)).err().unwrap();
        assert!(error.contains("Couldn't start /no/such/agent"), "{error}");
    }
}
