//! JSON-RPC over a language server's stdin/stdout. A reader thread parses
//! `Content-Length` frames: responses complete the matching request's
//! future, everything else goes to the `incoming` channel. A writer thread
//! sends, so a full pipe never blocks the UI, and fails requests that go
//! unanswered for too long.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    pin::Pin,
    process::{Child, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
        mpsc,
    },
    task::Poll,
    time::{Duration, Instant},
};

use futures::channel::{mpsc as async_mpsc, oneshot};
use serde_json::{Value, json};

/// A message the server sent on its own.
#[derive(Debug)]
pub enum Incoming {
    Notification { method: String, params: Value },
    /// Needs an answer through [`Client::respond`].
    Request { id: Value, method: String, params: Value },
    /// The server exited or its output became unreadable.
    Closed { stderr_tail: String },
}

#[derive(Debug, Clone)]
pub struct RpcError {
    /// The JSON-RPC error code; None when no answer came (`STOPPED`,
    /// `TIMED_OUT`).
    pub code: Option<i64>,
    pub message: String,
}

impl RpcError {
    fn new(message: &str) -> Self {
        RpcError { code: None, message: message.into() }
    }

    /// The server dropped the request itself: the text changed meanwhile
    /// (`ContentModified`) or it cancelled it (`RequestCancelled`,
    /// `ServerCancelled`). Nothing to tell the user about.
    pub fn is_cancelled(&self) -> bool {
        matches!(self.code, Some(-32802..=-32800))
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// JSON-RPC's error code for a request the receiver doesn't handle.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// The error for requests still waiting when the server exits.
pub const STOPPED: &str = "The language server stopped";
/// The error for a request still unanswered after `REQUEST_TIMEOUT`.
pub const TIMED_OUT: &str = "The language server didn't answer in time";

/// How long a request waits for its answer; then it
/// fails, and the server is told to stop working on it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

type Pending = Arc<Mutex<Waiting>>;

/// The requests waiting for their answers, by id.
#[derive(Default)]
struct Waiting {
    requests: HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>,
    /// No answer can come anymore (the server's output ended, or its input
    /// broke): a request made from now on fails at once.
    closed: bool,
}

/// Fails the requests waiting, and those made from now on, with `STOPPED`.
fn close(pending: &Pending) {
    let mut waiting = pending.lock().unwrap();
    waiting.closed = true;
    for (_, tx) in waiting.requests.drain() {
        let _ = tx.send(Err(RpcError::new(STOPPED)));
    }
}

/// What the writer thread does: send a frame, or note when a request it
/// sent times out.
enum Outgoing {
    /// A request or notification: it waits while a request is held.
    Frame(Vec<u8>),
    /// An answer to one of the server's requests: never held, the server
    /// may be waiting for it.
    Answer(Vec<u8>),
    Deadline(Instant, i64),
    /// What's sent after request `id` waits for its answer, until the
    /// instant given (`Client::request_first`).
    Hold(i64, Instant),
    /// The held request `id` was answered (from the reader thread).
    Answered(i64),
}

pub struct Client {
    child: Mutex<Child>,
    writer: mpsc::Sender<Outgoing>,
    next_id: AtomicI64,
    pending: Pending,
    /// The request what's sent after it waits for (`request_first`); 0: none.
    held: Arc<AtomicI64>,
}

impl Client {
    /// Starts `command` in `cwd`. The returned receiver yields what the
    /// server sends besides responses.
    pub fn spawn(
        program: &Path,
        args: &[String],
        cwd: &Path,
        path_env: &std::ffi::OsStr,
        env: &[(String, String)],
    ) -> std::io::Result<(Arc<Client>, async_mpsc::UnboundedReceiver<Incoming>)> {
        let mut command = crate::process::command(program);
        command
            .args(args)
            .current_dir(cwd)
            // Node-based servers start with `#!/usr/bin/env node`.
            .env("PATH", path_env)
            // What an extension's code set (its PATH too).
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| std::io::Error::other("no stderr"))?;

        let pending = Pending::default();
        let held = Arc::new(AtomicI64::new(0));
        let (writer, to_write) = mpsc::channel::<Outgoing>();
        let (timeouts, writer_held) = (pending.clone(), held.clone());
        std::thread::Builder::new().name("lsp-writer".into()).spawn(move || {
            write_loop(stdin, to_write, timeouts.clone(), writer_held);
            // The server can't hear requests now, and their deadlines are gone.
            close(&timeouts);
        })?;

        // Keep the end of stderr to explain a crash.
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let tail = stderr_tail.clone();
        std::thread::Builder::new().name("lsp-stderr".into()).spawn(move || {
            for_each_line(stderr, |line| {
                let mut tail = tail.lock().unwrap();
                tail.push_str(&line);
                tail.push('\n');
                if tail.len() > 4000 {
                    let cut = tail.len() - 2000;
                    let cut = tail.ceil_char_boundary(cut);
                    tail.drain(..cut);
                }
            })
        })?;

        let (incoming_tx, incoming) = async_mpsc::unbounded();
        let reader = Answers { pending: pending.clone(), held: held.clone(), writer: writer.clone() };
        std::thread::Builder::new()
            .name("lsp-reader".into())
            .spawn(move || read_loop(stdout, reader, incoming_tx, stderr_tail))?;

        let client = Arc::new(Client { child: Mutex::new(child), writer, next_id: AtomicI64::new(1), pending, held });
        Ok((client, incoming))
    }

    /// Sends a request; the future resolves with the result. Dropping it
    /// before the answer comes tells the server to stop working on it, and
    /// one unanswered after `REQUEST_TIMEOUT` fails with `TIMED_OUT`.
    pub fn request(&self, method: &str, params: Value) -> Request {
        self.request_held(method, params, None)
    }

    /// Sends a request the server answers before anything sent after it
    /// reaches it: requests and notifications sent meanwhile wait (answers to
    /// the server's own requests don't). Unanswered after `wait`, it fails
    /// with `TIMED_OUT` and they go. For `completionItem/resolve` after an
    /// edit: rust-analyzer resolves an item only in the text it made it for,
    /// and drops the work when a change comes in. One request at a time is
    /// held; another sent meanwhile is an ordinary one.
    pub fn request_first(&self, method: &str, params: Value, wait: Duration) -> Request {
        self.request_held(method, params, Some(wait))
    }

    fn request_held(&self, method: &str, params: Value, wait: Option<Duration>) -> Request {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut waiting = self.pending.lock().unwrap();
            // The server stopped: nothing would ever answer it, nor time it out.
            if waiting.closed {
                let _ = tx.send(Err(RpcError::new(STOPPED)));
                return Request { id, rx, pending: self.pending.clone(), writer: self.writer.clone() };
            }
            waiting.requests.insert(id, tx);
        }
        // Before it's sent, so its answer can't come first.
        let held = wait.filter(|_| self.held.compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire).is_ok());
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        if let Some(wait) = held {
            self.writer.send(Outgoing::Hold(id, Instant::now() + wait)).ok();
        }
        self.writer.send(Outgoing::Deadline(Instant::now() + REQUEST_TIMEOUT, id)).ok();
        Request { id, rx, pending: self.pending.clone(), writer: self.writer.clone() }
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Answers a request from the server.
    pub fn respond(&self, id: Value, result: Value) {
        self.writer.send(Outgoing::Answer(frame(&json!({ "jsonrpc": "2.0", "id": id, "result": result })))).ok();
    }

    /// Answers a request Kuana doesn't handle with JSON-RPC's MethodNotFound,
    /// so the server knows it went unanswered (a `null` result reads as done).
    pub fn respond_not_found(&self, id: Value, method: &str) {
        let error = json!({ "code": METHOD_NOT_FOUND, "message": format!("Unhandled method {method}") });
        self.writer.send(Outgoing::Answer(frame(&json!({ "jsonrpc": "2.0", "id": id, "error": error })))).ok();
    }

    fn send(&self, message: Value) {
        send(&self.writer, message);
    }

    /// Asks the server to exit, then makes sure it does. Blocks for up to a
    /// second, so call it off the main thread.
    pub fn shutdown(&self) {
        self.stop(Duration::from_secs(1));
    }

    /// Asks the server to exit and kills it if it's still running after
    /// `wait`: half of it for the answer to `shutdown` (`exit` comes after
    /// it, as the protocol asks: a server may still be saving state), half
    /// to exit. Blocks, so call it off the main thread.
    pub fn stop(&self, wait: Duration) {
        let shutdown = self.request("shutdown", Value::Null);
        let (answered, answer) = mpsc::channel();
        std::thread::spawn(move || answered.send(futures::executor::block_on(shutdown)).ok());
        answer.recv_timeout(wait / 2).ok();
        self.notify("exit", Value::Null);
        let deadline = Instant::now() + wait / 2;
        while Instant::now() < deadline {
            if self.child.lock().ok().and_then(|mut c| c.try_wait().ok().flatten()).is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Ok(mut child) = self.child.lock() {
            crate::process::kill_tree(&mut child);
        }
    }
}

/// A request waiting for its answer (see [`Client::request`]).
pub struct Request {
    id: i64,
    rx: oneshot::Receiver<Result<Value, RpcError>>,
    pending: Pending,
    writer: mpsc::Sender<Outgoing>,
}

impl Future for Request {
    type Output = Result<Value, RpcError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.rx).poll(cx).map(|answer| answer.unwrap_or_else(|_| Err(RpcError::new(STOPPED))))
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        // Still pending: no answer came, so the server may still be at it.
        if self.pending.lock().unwrap().requests.remove(&self.id).is_some() {
            send(&self.writer, cancel(self.id));
        }
    }
}

fn cancel(id: i64) -> Value {
    json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": id } })
}

fn frame(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(body.as_bytes());
    frame
}

fn send(writer: &mpsc::Sender<Outgoing>, message: Value) {
    // Fails only once the writer thread is gone (server exited).
    writer.send(Outgoing::Frame(frame(&message))).ok();
}

/// Calls `f` with each line of `stream` (without its line break) until it
/// ends. Read as bytes: a line that isn't UTF-8 (a file name, binary noise)
/// mustn't stop the reading, or the program would die writing to a pipe
/// nobody reads.
pub(crate) fn for_each_line(stream: impl Read, mut f: impl FnMut(String)) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let text = String::from_utf8_lossy(&line);
                f(text.trim_end_matches(['\n', '\r']).to_string());
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            crate::process::kill_tree(&mut child);
        }
    }
}

/// Writes frames to the server, holding what follows a held request until
/// it's answered, and fails requests unanswered by their deadline (telling
/// the server to stop working on them too).
fn write_loop(mut stdin: impl Write, outgoing: mpsc::Receiver<Outgoing>, pending: Pending, held: Arc<AtomicI64>) {
    let mut write = |frame: &[u8]| stdin.write_all(frame).and_then(|_| stdin.flush()).is_ok();
    // Requests by deadline: they all get the same timeout, so in order.
    let mut deadlines: VecDeque<(Instant, i64)> = VecDeque::new();
    // The held request, until when, and the frames waiting for its answer.
    let mut hold: Option<(i64, Instant, Vec<Vec<u8>>)> = None;
    // A frame goes now, or waits for the held request's answer.
    let send = |frame: Vec<u8>, hold: &mut Option<(i64, Instant, Vec<Vec<u8>>)>, write: &mut dyn FnMut(&[u8]) -> bool| match hold {
        Some((.., waiting)) => {
            waiting.push(frame);
            true
        }
        None => write(&frame),
    };
    loop {
        let wake = deadlines.front().map(|&(at, _)| at).into_iter().chain(hold.as_ref().map(|&(_, until, _)| until)).min();
        let next = match wake {
            Some(at) => outgoing.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => outgoing.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        let mut release = false;
        match next {
            Ok(Outgoing::Frame(frame)) => {
                if !send(frame, &mut hold, &mut write) {
                    return;
                }
            }
            Ok(Outgoing::Answer(frame)) => {
                if !write(&frame) {
                    return;
                }
            }
            Ok(Outgoing::Hold(id, until)) => hold = Some((id, until, Vec::new())),
            Ok(Outgoing::Answered(id)) => release = hold.as_ref().is_some_and(|&(held, ..)| held == id),
            Ok(Outgoing::Deadline(at, id)) => deadlines.push_back((at, id)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                // The held request took too long: it fails (its answer would
                // be for whatever the server had by then), and the rest goes.
                if let Some(&(id, until, _)) = hold.as_ref()
                    && until <= now
                {
                    if let Some(tx) = pending.lock().unwrap().requests.remove(&id) {
                        let _ = tx.send(Err(RpcError::new(TIMED_OUT)));
                        if !write(&frame(&cancel(id))) {
                            return;
                        }
                    }
                    release = true;
                }
                while let Some(&(_, id)) = deadlines.front().filter(|(at, _)| *at <= now) {
                    deadlines.pop_front();
                    // Answered (or dropped) meanwhile: nothing to do.
                    let Some(tx) = pending.lock().unwrap().requests.remove(&id) else { continue };
                    let written = send(frame(&cancel(id)), &mut hold, &mut write);
                    let _ = tx.send(Err(RpcError::new(TIMED_OUT)));
                    if !written {
                        return;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if release && let Some((_, _, waiting)) = hold.take() {
            held.store(0, Ordering::Release);
            for frame in waiting {
                if !write(&frame) {
                    return;
                }
            }
        }
    }
}

/// What the reader thread needs to deliver answers: the requests waiting
/// for theirs, and to tell the writer when the held one's came.
struct Answers {
    pending: Pending,
    held: Arc<AtomicI64>,
    writer: mpsc::Sender<Outgoing>,
}

fn read_loop(stdout: impl Read, answers: Answers, incoming: async_mpsc::UnboundedSender<Incoming>, stderr_tail: Arc<Mutex<String>>) {
    let Answers { pending, held, writer } = answers;
    let mut reader = BufReader::new(stdout);
    while let Some(message) = read_message(&mut reader) {
        let answer = message.get("method").is_none().then(|| message.get("id").and_then(Value::as_i64)).flatten();
        dispatch(message, &pending, &incoming);
        if let Some(id) = answer.filter(|&id| held.load(Ordering::Acquire) == id) {
            writer.send(Outgoing::Answered(id)).ok();
        }
    }
    // Whatever is still waiting, or asked from now on, will never get an answer.
    close(&pending);
    // Let stderr catch up so the tail explains the exit.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let stderr_tail = stderr_tail.lock().map(|t| t.clone()).unwrap_or_default();
    let _ = incoming.unbounded_send(Incoming::Closed { stderr_tail });
}

/// The longest message read. A longer `Content-Length` means the stream (or
/// the server) is broken, not a message to make room for.
const MAX_MESSAGE: usize = 64 << 20;

/// Reads one `Content-Length`-framed message; None at end of stream, or
/// when it's broken (a length that isn't one, or past `MAX_MESSAGE`).
pub(crate) fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    loop {
        let mut length = None;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let header = line.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = Some(value.trim().parse::<usize>().ok().filter(|&n| n <= MAX_MESSAGE)?);
            }
        }
        let Some(length) = length else { continue };
        let mut body = vec![0; length];
        reader.read_exact(&mut body).ok()?;
        if let Ok(value) = serde_json::from_slice(&body) {
            return Some(value);
        }
    }
}

fn dispatch(mut message: Value, pending: &Pending, incoming: &async_mpsc::UnboundedSender<Incoming>) {
    let method = message.get("method").and_then(Value::as_str).map(str::to_string);
    let id = message.get_mut("id").map(Value::take);
    match (method, id) {
        (Some(method), Some(id)) => {
            let params = message.get_mut("params").map(Value::take).unwrap_or(Value::Null);
            let _ = incoming.unbounded_send(Incoming::Request { id, method, params });
        }
        (Some(method), None) => {
            let params = message.get_mut("params").map(Value::take).unwrap_or(Value::Null);
            let _ = incoming.unbounded_send(Incoming::Notification { method, params });
        }
        (None, Some(id)) => {
            let Some(id) = id.as_i64() else { return };
            let Some(tx) = pending.lock().unwrap().requests.remove(&id) else { return };
            let result = match message.get_mut("error") {
                Some(error) => Err(RpcError {
                    code: error.get("code").and_then(Value::as_i64),
                    message: error.get("message").and_then(Value::as_str).unwrap_or("Request failed").to_string(),
                }),
                None => Ok(message.get_mut("result").map(Value::take).unwrap_or(Value::Null)),
            };
            let _ = tx.send(result);
        }
        (None, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_an_unanswered_request_cancels_it() {
        let pending: Pending = Pending::default();
        let (writer, written) = mpsc::channel();
        let request = |id: i64| {
            let (tx, rx) = oneshot::channel();
            pending.lock().unwrap().requests.insert(id, tx);
            Request { id, rx, pending: pending.clone(), writer: writer.clone() }
        };
        let (incoming, _) = async_mpsc::unbounded();

        // Answered, then dropped: nothing to cancel.
        let answered = request(1);
        dispatch(json!({ "jsonrpc": "2.0", "id": 1, "result": 42 }), &pending, &incoming);
        assert_eq!(futures::executor::block_on(answered).unwrap(), 42);
        assert!(written.try_recv().is_err());

        // Dropped while waiting: the server hears $/cancelRequest, and a
        // late answer goes nowhere.
        drop(request(2));
        let Ok(Outgoing::Frame(frame)) = written.try_recv() else { panic!("a frame") };
        let frame = String::from_utf8(frame).unwrap();
        assert!(frame.ends_with(r#"{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":2}}"#), "{frame}");
        assert!(pending.lock().unwrap().requests.is_empty());
        dispatch(json!({ "jsonrpc": "2.0", "id": 2, "result": null }), &pending, &incoming);
    }

    /// What the writer thread wrote, for tests.
    #[derive(Clone, Default)]
    struct Written(Arc<Mutex<Vec<u8>>>);

    impl Write for Written {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn unanswered_requests_time_out() {
        let pending: Pending = Pending::default();
        let (writer, outgoing) = mpsc::channel();
        let written = Written::default();
        let (out, timeouts) = (written.clone(), pending.clone());
        let thread = std::thread::spawn(move || write_loop(out, outgoing, timeouts, Arc::new(AtomicI64::new(0))));
        let (incoming, _) = async_mpsc::unbounded();
        let request = |id: i64| {
            let (tx, rx) = oneshot::channel();
            pending.lock().unwrap().requests.insert(id, tx);
            writer.send(Outgoing::Deadline(Instant::now() + Duration::from_millis(30), id)).unwrap();
            Request { id, rx, pending: pending.clone(), writer: writer.clone() }
        };
        // One answered in time, one never: only the second fails, and the
        // server is told to stop working on it.
        let (answered, silent) = (request(1), request(2));
        dispatch(json!({ "jsonrpc": "2.0", "id": 1, "result": 7 }), &pending, &incoming);
        assert_eq!(futures::executor::block_on(answered).unwrap(), 7);
        let err = futures::executor::block_on(silent).unwrap_err();
        assert_eq!((err.message.as_str(), err.code), (TIMED_OUT, None));
        let text = String::from_utf8(written.0.lock().unwrap().clone()).unwrap();
        assert!(text.ends_with(r#"{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":2}}"#), "{text}");
        assert!(!text.contains(r#""id":1}"#), "{text}");
        drop(writer);
        thread.join().unwrap();
    }

    /// What's sent after a held request waits for its answer (answers to
    /// the server's own requests don't); unanswered in time, it fails, the
    /// server is told to stop working on it, and the rest goes.
    #[test]
    fn what_follows_a_held_request_waits_for_its_answer() {
        let pending: Pending = Pending::default();
        let held = Arc::new(AtomicI64::new(0));
        let (writer, outgoing) = mpsc::channel();
        let written = Written::default();
        let (out, timeouts, writer_held) = (written.clone(), pending.clone(), held.clone());
        let thread = std::thread::spawn(move || write_loop(out, outgoing, timeouts, writer_held));
        let text = || String::from_utf8(written.0.lock().unwrap().clone()).unwrap();
        let wait_until = |what: &str, done: &dyn Fn(&str) -> bool| {
            let started = Instant::now();
            while !done(&text()) {
                assert!(started.elapsed() < Duration::from_secs(5), "{what} wasn't sent: {}", text());
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        // As `Client::request_first` sends one.
        let held_request = |id: i64, wait: Duration| {
            let (tx, rx) = oneshot::channel();
            pending.lock().unwrap().requests.insert(id, tx);
            held.store(id, Ordering::Release);
            send(&writer, json!({ "jsonrpc": "2.0", "id": id, "method": "completionItem/resolve", "params": {} }));
            writer.send(Outgoing::Hold(id, Instant::now() + wait)).unwrap();
            Request { id, rx, pending: pending.clone(), writer: writer.clone() }
        };

        // Answered: what was sent meanwhile goes after its answer.
        let first = held_request(1, Duration::from_secs(10));
        send(&writer, json!({ "jsonrpc": "2.0", "method": "textDocument/didChange", "params": {} }));
        writer.send(Outgoing::Answer(frame(&json!({ "jsonrpc": "2.0", "id": "cfg", "result": null })))).unwrap();
        wait_until("the answer to the server", &|t| t.contains(r#""id":"cfg""#));
        assert!(!text().contains("didChange"), "{}", text());
        let (incoming, _) = async_mpsc::unbounded();
        dispatch(json!({ "jsonrpc": "2.0", "id": 1, "result": 1 }), &pending, &incoming);
        writer.send(Outgoing::Answered(1)).unwrap();
        wait_until("the change", &|t| t.contains("didChange"));
        assert_eq!(futures::executor::block_on(first).unwrap(), 1);
        assert_eq!(held.load(Ordering::Acquire), 0);

        // Not answered in time.
        let second = held_request(2, Duration::from_millis(30));
        send(&writer, json!({ "jsonrpc": "2.0", "method": "textDocument/didSave", "params": {} }));
        assert_eq!(futures::executor::block_on(second).unwrap_err().message, TIMED_OUT);
        wait_until("the save", &|t| t.contains("didSave"));
        let text = text();
        let cancel = text.find(r#""method":"$/cancelRequest","params":{"id":2}"#).expect("a cancel");
        assert!(cancel < text.find("didSave").unwrap(), "{text}");
        drop(writer);
        thread.join().unwrap();
    }

    /// A request made once the server stopped fails at once: nothing would
    /// answer it, nor time it out (the writer thread may be gone too).
    #[cfg(unix)]
    #[test]
    fn requests_to_a_stopped_server_fail() {
        use futures::StreamExt as _;
        let path = std::env::var_os("PATH").unwrap_or_default();
        let (client, mut incoming) = Client::spawn(Path::new("sh"), &["-c".into(), "exit 0".into()], &std::env::temp_dir(), &path, &[]).unwrap();
        assert!(matches!(futures::executor::block_on(incoming.next()), Some(Incoming::Closed { .. })));
        for _ in 0..2 {
            // The first one's frame breaks the pipe, ending the writer thread.
            let request = client.request("textDocument/hover", Value::Null);
            let (answered, answer) = mpsc::channel();
            std::thread::spawn(move || answered.send(futures::executor::block_on(request)).ok());
            let err = answer.recv_timeout(Duration::from_secs(5)).expect("an answer").unwrap_err();
            assert_eq!(err.message, STOPPED);
        }
    }

    /// The writer thread ending (the server's input broke) fails what waits.
    #[test]
    fn closing_fails_what_waits_and_what_comes() {
        let pending = Pending::default();
        let (tx, mut rx) = oneshot::channel();
        pending.lock().unwrap().requests.insert(1, tx);
        close(&pending);
        assert_eq!(rx.try_recv().unwrap().unwrap().unwrap_err().message, STOPPED);
        assert!(pending.lock().unwrap().closed);
    }

    #[test]
    fn reads_lines_that_are_not_utf8() {
        let stream: &[u8] = b"first\n\xff\xfe broken\r\nlast";
        let mut lines = Vec::new();
        for_each_line(stream, |line| lines.push(line));
        assert_eq!(lines, ["first", "\u{fffd}\u{fffd} broken", "last"]);
    }

    #[test]
    fn reads_frames() {
        let a = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let b = r#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"message":"é"}}"#;
        let stream = format!(
            "Content-Length: {}\r\n\r\n{a}Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n{b}",
            a.len(),
            b.len()
        );
        let mut reader = BufReader::new(stream.as_bytes());
        assert_eq!(read_message(&mut reader).unwrap()["result"]["ok"], true);
        assert_eq!(read_message(&mut reader).unwrap()["params"]["message"], "é");
        assert!(read_message(&mut reader).is_none());
        // A length too big to be a message ends the stream; nothing is allocated for it.
        for length in [format!("{}", MAX_MESSAGE + 1), "18446744073709551616".into(), "lots".into()] {
            let stream = format!("Content-Length: {length}\r\n\r\n{a}");
            assert!(read_message(&mut BufReader::new(stream.as_bytes())).is_none(), "{length}");
        }
    }

    #[test]
    fn routes_responses_and_requests() {
        let pending: Pending = Pending::default();
        let (tx, mut rx) = oneshot::channel();
        pending.lock().unwrap().requests.insert(7, tx);
        let (incoming_tx, mut incoming) = async_mpsc::unbounded();
        dispatch(json!({"id": 7, "error": {"code": -1, "message": "nope"}}), &pending, &incoming_tx);
        let err = rx.try_recv().unwrap().unwrap().unwrap_err();
        assert_eq!((err.message.as_str(), err.code, err.is_cancelled()), ("nope", Some(-1), false));
        // `ContentModified`: the text changed under the request.
        let (tx, mut rx) = oneshot::channel();
        pending.lock().unwrap().requests.insert(8, tx);
        dispatch(json!({"id": 8, "error": {"code": -32801, "message": "content modified"}}), &pending, &incoming_tx);
        assert!(rx.try_recv().unwrap().unwrap().unwrap_err().is_cancelled());
        dispatch(json!({"id": "a", "method": "workspace/configuration", "params": {}}), &pending, &incoming_tx);
        assert!(matches!(incoming.try_recv(), Ok(Incoming::Request { method, .. }) if method == "workspace/configuration"));
    }
}
