//! The connection between Kuana and `kuana-server`: JSON-RPC 2.0 framed as
//! LSP frames it (`Content-Length`), over any byte stream (`ssh`'s stdin and
//! stdout, a pipe in tests). Both sides use it: requests block until their
//! answer (the callers are `std::fs`-like, off the UI thread), notifications
//! carry streams (a program's output, file changes). Errors keep their
//! `io::ErrorKind`, which callers act on (`NotFound`, `AlreadyExists`).

use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, Read, Write},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

use serde_json::{Value, json};

/// How long a request waits for its answer before failing.
pub const TIMEOUT: Duration = Duration::from_secs(120);

pub type Answer = Result<Value, io::Error>;

/// One side of the connection.
pub struct Connection {
    writer: Mutex<Box<dyn Write + Send>>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<Answer>>>,
    open: AtomicBool,
}

/// What the other side sent that isn't an answer.
pub enum Incoming {
    Request { id: u64, method: String, params: Value },
    Notification { method: String, params: Value },
}

impl Connection {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Connection { writer: Mutex::new(writer), next_id: AtomicU64::new(1), pending: Mutex::new(HashMap::new()), open: AtomicBool::new(true) }
    }

    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    fn send(&self, message: &Value) -> io::Result<()> {
        let body = serde_json::to_vec(message)?;
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let result = (|| {
            write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
            writer.write_all(&body)?;
            writer.flush()
        })();
        if result.is_err() {
            self.close();
        }
        result
    }

    /// Sends a request and waits for its answer.
    pub fn request(&self, method: &str, params: Value) -> Answer {
        if !self.is_open() {
            return Err(not_connected());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        if let Err(err) = self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })) {
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            return Err(err);
        }
        match rx.recv_timeout(TIMEOUT) {
            Ok(answer) => answer,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                Err(io::Error::new(io::ErrorKind::TimedOut, format!("the remote host didn't answer {method}")))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(not_connected()),
        }
    }

    pub fn notify(&self, method: &str, params: Value) -> io::Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Answers a request.
    pub fn respond(&self, id: u64, answer: Answer) -> io::Result<()> {
        match answer {
            Ok(result) => self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result })),
            Err(err) => self.send(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": err.to_string(), "data": { "kind": kind_name(err.kind()) } } })),
        }
    }

    /// Reads what comes until the stream ends: answers go to their requests,
    /// the rest to `incoming` (called on this thread, in order). At the end
    /// every waiting request fails.
    pub fn read_loop(&self, reader: impl Read, mut incoming: impl FnMut(Incoming)) {
        let mut reader = BufReader::new(reader);
        while let Some(message) = read_message(&mut reader) {
            let id = message.get("id").and_then(Value::as_u64);
            match (id, message.get("method").and_then(Value::as_str)) {
                (Some(id), None) => {
                    let answer = match message.get("error") {
                        Some(error) => Err(error_of(error)),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    if let Some(tx) = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
                        let _ = tx.send(answer);
                    }
                }
                (Some(id), Some(method)) => incoming(Incoming::Request { id, method: method.to_string(), params: message.get("params").cloned().unwrap_or(Value::Null) }),
                (None, Some(method)) => incoming(Incoming::Notification { method: method.to_string(), params: message.get("params").cloned().unwrap_or(Value::Null) }),
                (None, None) => {}
            }
        }
        self.close();
    }

    /// Ends the connection from this side: the other sees its input end.
    pub fn shutdown(&self) {
        *self.writer.lock().unwrap_or_else(|e| e.into_inner()) = Box::new(io::sink());
        self.close();
    }

    /// The other side went: every waiting request fails.
    pub fn close(&self) {
        self.open.store(false, Ordering::Release);
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// One framed message, or None at the end of the stream.
pub fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(n) = line.strip_prefix("Content-Length:") {
            length = n.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

pub fn not_connected() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "the remote host isn't connected")
}

/// The `io::ErrorKind`s callers tell apart, by name over the wire.
const KINDS: &[(io::ErrorKind, &str)] = &[
    (io::ErrorKind::NotFound, "NotFound"),
    (io::ErrorKind::PermissionDenied, "PermissionDenied"),
    (io::ErrorKind::AlreadyExists, "AlreadyExists"),
    (io::ErrorKind::InvalidInput, "InvalidInput"),
    (io::ErrorKind::InvalidData, "InvalidData"),
    (io::ErrorKind::TimedOut, "TimedOut"),
    (io::ErrorKind::Unsupported, "Unsupported"),
    (io::ErrorKind::NotConnected, "NotConnected"),
    (io::ErrorKind::DirectoryNotEmpty, "DirectoryNotEmpty"),
    (io::ErrorKind::IsADirectory, "IsADirectory"),
    (io::ErrorKind::NotADirectory, "NotADirectory"),
    (io::ErrorKind::CrossesDevices, "CrossesDevices"),
];

fn kind_name(kind: io::ErrorKind) -> &'static str {
    KINDS.iter().find(|(k, _)| *k == kind).map_or("Other", |(_, n)| n)
}

fn error_of(error: &Value) -> io::Error {
    let message = error["message"].as_str().unwrap_or("the remote host failed").to_string();
    let kind = error["data"]["kind"].as_str().and_then(|n| KINDS.iter().find(|(_, name)| *name == n)).map_or(io::ErrorKind::Other, |(k, _)| *k);
    io::Error::new(kind, message)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn requests_get_their_answers_and_errors_keep_their_kind() {
        let (server_in, client_out) = io::pipe().unwrap();
        let (client_in, server_out) = io::pipe().unwrap();
        let server = Arc::new(Connection::new(Box::new(server_out)));
        let answering = server.clone();
        std::thread::spawn(move || {
            server.read_loop(server_in, |incoming| {
                if let Incoming::Request { id, method, params } = incoming {
                    let answer = match method.as_str() {
                        "echo" => Ok(params),
                        _ => Err(io::Error::new(io::ErrorKind::NotFound, "no such thing")),
                    };
                    answering.respond(id, answer).unwrap();
                }
            });
        });
        let client = Arc::new(Connection::new(Box::new(client_out)));
        let reading = client.clone();
        std::thread::spawn(move || reading.read_loop(client_in, |_| {}));
        assert_eq!(client.request("echo", json!({ "a": [1, 2] })).unwrap(), json!({ "a": [1, 2] }));
        let err = client.request("missing", Value::Null).unwrap_err();
        assert_eq!((err.kind(), err.to_string()), (io::ErrorKind::NotFound, "no such thing".to_string()));
        // Several at once, from several threads.
        let answers: Vec<Value> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8).map(|i| s.spawn({ let c = client.clone(); move || c.request("echo", json!(i)).unwrap() })).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(answers, (0..8).map(|i| json!(i)).collect::<Vec<_>>());
    }
}
