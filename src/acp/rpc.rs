//! JSON-RPC 2.0 as ACP frames it: one JSON object per line, both ways, over
//! any byte streams (an agent process's stdio, or an in-process pipe for
//! Kuana's own agent and the tests). A reader thread hands responses to
//! whoever waits for them and everything else (requests, notifications) to
//! the receiver; a writer thread sends in order.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
        mpsc as std_mpsc,
    },
};

use futures::channel::{mpsc, oneshot};
use serde_json::{Value, json};

/// An error answer: JSON-RPC's code and message.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    /// ACP: the agent needs the user to sign in first.
    pub const AUTH_REQUIRED: i64 = -32000;
    /// ACP: no such file, session or terminal.
    pub const NOT_FOUND: i64 = -32002;
    /// The connection ended before an answer came (Kuana's own code).
    pub const CLOSED: i64 = -32099;

    pub fn new(code: i64, message: impl Into<String>) -> RpcError {
        RpcError { code, message: message.into(), data: None }
    }

    pub fn internal(message: impl Into<String>) -> RpcError {
        RpcError::new(RpcError::INTERNAL, message)
    }

    fn to_json(&self) -> Value {
        let mut error = json!({ "code": self.code, "message": self.message });
        if let Some(data) = &self.data {
            error["data"] = data.clone();
        }
        error
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// What the other side sent that isn't an answer to us.
#[derive(Debug)]
pub enum Incoming {
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

#[derive(Clone)]
pub struct Connection {
    out: std_mpsc::Sender<String>,
    pending: Pending,
    next_id: Arc<AtomicI64>,
    closed: Arc<AtomicBool>,
}

impl Connection {
    /// Starts reading `input` and writing `output`.
    pub fn start(input: impl Read + Send + 'static, output: impl Write + Send + 'static) -> (Connection, mpsc::UnboundedReceiver<Incoming>) {
        let (out, to_write) = std_mpsc::channel::<String>();
        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let (incoming, received) = mpsc::unbounded();
        let _ = std::thread::Builder::new().name("acp writer".into()).spawn(move || {
            let mut output = output;
            for line in to_write {
                if output.write_all(line.as_bytes()).and_then(|()| output.write_all(b"\n")).and_then(|()| output.flush()).is_err() {
                    break;
                }
            }
        });
        let (waiting, done) = (pending.clone(), closed.clone());
        let _ = std::thread::Builder::new().name("acp reader".into()).spawn(move || {
            let mut reader = BufReader::new(input);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(message) = serde_json::from_str::<Value>(line.trim()) else { continue };
                let method = message.get("method").and_then(Value::as_str).map(str::to_string);
                let id = message.get("id").cloned().filter(|id| !id.is_null());
                match (method, id) {
                    (Some(method), Some(id)) => {
                        let params = message.get("params").cloned().unwrap_or(Value::Null);
                        let _ = incoming.unbounded_send(Incoming::Request { id, method, params });
                    }
                    (Some(method), None) => {
                        let params = message.get("params").cloned().unwrap_or(Value::Null);
                        let _ = incoming.unbounded_send(Incoming::Notification { method, params });
                    }
                    (None, Some(id)) => {
                        let Some(id) = id.as_i64() else { continue };
                        let Some(answer) = waiting.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) else { continue };
                        let result = match message.get("error") {
                            Some(error) => Err(RpcError {
                                code: error.get("code").and_then(Value::as_i64).unwrap_or(RpcError::INTERNAL),
                                message: error.get("message").and_then(Value::as_str).unwrap_or("Error").to_string(),
                                data: error.get("data").cloned(),
                            }),
                            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = answer.send(result);
                    }
                    (None, None) => {}
                }
            }
            // Gone: whoever waits hears so; the receiver ends.
            done.store(true, Ordering::Relaxed);
            for (_, answer) in waiting.lock().unwrap_or_else(|e| e.into_inner()).drain() {
                let _ = answer.send(Err(RpcError::new(RpcError::CLOSED, "The agent stopped")));
            }
        });
        (Connection { out, pending, next_id: Arc::new(AtomicI64::new(0)), closed }, received)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Sends a request; the future gives its answer.
    pub fn request(&self, method: &str, params: Value) -> impl Future<Output = Result<Value, RpcError>> + use<> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        // Closed is checked under the lock the reader drains with, after
        // setting it: a request is either drained (answered "stopped") or refused.
        let waiting = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let open = !self.is_closed();
            if open {
                pending.insert(id, answer);
            }
            open
        };
        let sent = waiting && self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        if !sent {
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        }
        async move {
            if !sent {
                return Err(RpcError::new(RpcError::CLOSED, "The agent stopped"));
            }
            answered.await.unwrap_or_else(|_| Err(RpcError::new(RpcError::CLOSED, "The agent stopped")))
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub fn respond(&self, id: Value, result: Result<Value, RpcError>) {
        let message = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error.to_json() }),
        };
        self.send(message);
    }

    fn send(&self, message: Value) -> bool {
        self.out.send(message.to_string()).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    /// Two connections talking over in-process pipes.
    fn pair() -> ((Connection, mpsc::UnboundedReceiver<Incoming>), (Connection, mpsc::UnboundedReceiver<Incoming>)) {
        let (a_read, b_write) = std::io::pipe().unwrap();
        let (b_read, a_write) = std::io::pipe().unwrap();
        (Connection::start(a_read, a_write), Connection::start(b_read, b_write))
    }

    #[test]
    fn requests_notifications_and_errors_round_trip() {
        let ((client, _client_in), (agent, mut agent_in)) = pair();
        let answering = std::thread::spawn(move || {
            futures::executor::block_on(async {
                while let Some(incoming) = agent_in.next().await {
                    match incoming {
                        Incoming::Request { id, method, params } if method == "echo" => agent.respond(id, Ok(params)),
                        Incoming::Request { id, .. } => agent.respond(id, Err(RpcError::new(RpcError::METHOD_NOT_FOUND, "no"))),
                        Incoming::Notification { method, .. } => {
                            assert_eq!(method, "bye");
                            return;
                        }
                    }
                }
            })
        });
        let echoed = futures::executor::block_on(client.request("echo", json!({ "x": 1 }))).unwrap();
        assert_eq!(echoed, json!({ "x": 1 }));
        let error = futures::executor::block_on(client.request("nope", json!({}))).unwrap_err();
        assert_eq!(error.code, RpcError::METHOD_NOT_FOUND);
        client.notify("bye", json!({}));
        answering.join().unwrap();
    }

    #[test]
    fn a_closed_connection_fails_what_waits() {
        let (a_read, b_write) = std::io::pipe().unwrap();
        let (_b_read, a_write) = std::io::pipe().unwrap();
        let (client, mut incoming) = Connection::start(a_read, a_write);
        let waiting = client.request("slow", json!({}));
        drop(b_write);
        let error = futures::executor::block_on(waiting).unwrap_err();
        assert_eq!(error.code, RpcError::CLOSED);
        assert!(futures::executor::block_on(incoming.next()).is_none());
        assert!(client.is_closed());
        assert_eq!(futures::executor::block_on(client.request("after", json!({}))).unwrap_err().code, RpcError::CLOSED);
    }
}
