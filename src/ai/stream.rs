//! Running a request: a thread posts it, reads the stream and sends Kuana's
//! [`StreamEvent`]s back over a channel. Dropping the [`Stream`] cancels it
//! (the thread stops at the next line and closes the connection).

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures::channel::mpsc;

use super::{
    anthropic, http,
    message::{Request, StreamEvent},
    openai,
    providers::{Endpoint, Wire},
    sse,
};

/// Attempts before a retryable failure (overload, rate limit) is reported.
const ATTEMPTS: u32 = 3;

pub struct Stream {
    pub events: mpsc::UnboundedReceiver<StreamEvent>,
    cancelled: Arc<AtomicBool>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
impl Stream {
    /// Reads the whole reply, `each` seeing every event on the way.
    pub async fn reply(mut self, mut each: impl FnMut(&StreamEvent)) -> Result<super::message::Reply, String> {
        use futures::StreamExt as _;
        let mut acc = super::message::Accumulator::default();
        while let Some(event) = self.events.next().await {
            each(&event);
            acc.push(&event);
        }
        acc.finish()
    }

    /// Just the reply's text.
    pub async fn text(self) -> Result<String, String> {
        self.reply(|_| {}).await.map(|r| r.text())
    }
}

/// Starts `request` on `endpoint`.
pub fn start(endpoint: Endpoint, request: Request) -> Stream {
    let (tx, rx) = mpsc::unbounded();
    let cancelled = Arc::new(AtomicBool::new(false));
    let stop = cancelled.clone();
    let spawned = std::thread::Builder::new().name("ai request".into()).spawn(move || {
        let result = run(&endpoint, &request, &stop, &mut |event| tx.unbounded_send(event).is_ok() && !stop.load(Ordering::Relaxed));
        if let Err(error) = result {
            let _ = tx.unbounded_send(StreamEvent::Error(error));
        }
    });
    if let Err(err) = spawned {
        let (tx, rx) = mpsc::unbounded();
        let _ = tx.unbounded_send(StreamEvent::Error(format!("Couldn't start the request: {err}")));
        return Stream { events: rx, cancelled };
    }
    Stream { events: rx, cancelled }
}

/// Posts and decodes, retrying failures worth retrying while nothing has
/// streamed yet; `send` returns false to stop.
fn run(endpoint: &Endpoint, request: &Request, stop: &AtomicBool, send: &mut dyn FnMut(StreamEvent) -> bool) -> Result<(), String> {
    let (url, body, mut headers) = match endpoint.provider.wire {
        Wire::Anthropic => (format!("{}/messages", endpoint.url), anthropic::body(request), anthropic::beta_headers(request)),
        Wire::OpenAi(flavor) => (format!("{}/chat/completions", endpoint.url), openai::body(request, flavor), Vec::new()),
    };
    headers.extend(endpoint.headers());
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut streamed = false;
        let mut parser = sse::Parser::default();
        let mut anthropic = anthropic::Decoder::default();
        let mut openai = openai::Decoder::default();
        let mut keep_going = true;
        let mut decode = |event: sse::Event, streamed: &mut bool| -> bool {
            let events = match endpoint.provider.wire {
                Wire::Anthropic => anthropic.event(&event),
                Wire::OpenAi(_) => openai.event(&event),
            };
            for event in events {
                *streamed = true;
                if !send(event) {
                    return false;
                }
            }
            true
        };
        let result = http::post_lines(&url, &headers, &body, |line| {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            if let Some(event) = parser.line(line) {
                keep_going = decode(event, &mut streamed);
            }
            keep_going
        });
        match result {
            Ok(()) => {
                if keep_going && let Some(event) = parser.finish() {
                    decode(event, &mut streamed);
                }
                return Ok(());
            }
            Err(err) if err.retryable() && !streamed && attempt < ATTEMPTS && !stop.load(Ordering::Relaxed) => {
                let wait = err.retry_after.unwrap_or(Duration::from_secs(u64::from(attempt) * 2)).min(Duration::from_secs(30));
                // In slices, so a cancel meanwhile doesn't send it again.
                let until = std::time::Instant::now() + wait;
                while std::time::Instant::now() < until {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(50).min(until.saturating_duration_since(std::time::Instant::now())));
                }
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

#[cfg(test)]
pub(crate) mod test_server {
    //! A local HTTP server that answers like a provider, so tests never call
    //! a paid API: each request takes the next queued response and is kept
    //! for the test to read.

    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
    };

    pub struct Server {
        pub url: String,
        pub requests: Arc<Mutex<Vec<(String, String)>>>,
    }

    /// Serves `responses` in order: (status, content type, body).
    pub fn start(responses: Vec<(u16, &'static str, String)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for (stream, (status, kind, body)) in listener.incoming().zip(responses) {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    head.push_str(&line);
                }
                let mut body_in = vec![0; length];
                reader.read_exact(&mut body_in).unwrap();
                seen.lock().unwrap().push((head, String::from_utf8_lossy(&body_in).into_owned()));
                let reply = format!("HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        Server { url, requests }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{message::StopReason, providers};
    use serde_json::json;

    fn endpoint(provider: &str, url: &str) -> Endpoint {
        Endpoint { provider: providers::info(provider).unwrap(), url: url.into(), key: Some("test-key".into()) }
    }

    fn sse(events: &[(&str, serde_json::Value)]) -> String {
        events.iter().map(|(name, data)| format!("event: {name}\ndata: {data}\n\n")).collect()
    }

    #[test]
    fn anthropic_stream_end_to_end_with_a_retry() {
        let body = sse(&[
            ("message_start", json!({ "type": "message_start", "message": { "usage": { "input_tokens": 9 } } })),
            ("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
            ("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hello" } })),
            ("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 2 } })),
        ]);
        let server = test_server::start(vec![
            (529, "application/json", json!({ "type": "error", "error": { "message": "Overloaded" } }).to_string()),
            (200, "text/event-stream", body),
        ]);
        let stream = start(endpoint("anthropic", &server.url), Request::simple("claude-opus-5-5", "sys", "hi"));
        let reply = futures::executor::block_on(stream.reply(|_| {})).unwrap();
        assert_eq!(reply.text(), "Hello");
        assert_eq!(reply.stop, StopReason::EndTurn);
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let (head, body) = &requests[1];
        assert!(head.starts_with("POST /messages"), "{head}");
        assert!(head.contains("x-api-key: test-key") && head.contains("anthropic-version: 2026-10-21"));
        assert_eq!(serde_json::from_str::<serde_json::Value>(body).unwrap()["system"], "sys");
    }

    #[test]
    fn openai_stream_and_a_refused_key() {
        let chunk = |v: serde_json::Value| format!("data: {v}\n\n");
        let body = [
            chunk(json!({ "choices": [{ "delta": { "content": "Hi" } }] })),
            chunk(json!({ "choices": [{ "delta": { "content": " there" }, "finish_reason": "stop" }] })),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        let server = test_server::start(vec![(200, "text/event-stream", body)]);
        let text = futures::executor::block_on(start(endpoint("openai", &server.url), Request::simple("m", "", "hi")).text()).unwrap();
        assert_eq!(text, "Hi there");
        let head = server.requests.lock().unwrap()[0].0.clone();
        assert!(head.starts_with("POST /chat/completions") && head.contains("authorization: Bearer test-key"), "{head}");

        let server = test_server::start(vec![(401, "application/json", json!({ "error": { "message": "Incorrect API key" } }).to_string())]);
        let err = futures::executor::block_on(start(endpoint("openai", &server.url), Request::simple("m", "", "hi")).text()).unwrap_err();
        assert!(err.contains("Incorrect API key") && err.contains("refused the key"), "{err}");
        assert_eq!(server.requests.lock().unwrap().len(), 1, "a refused key isn't retried");
    }

    #[test]
    fn models_are_listed() {
        let server = test_server::start(vec![(200, "application/json", json!({ "data": [{ "id": "llama3" }] }).to_string())]);
        let models = endpoint("ollama", &server.url).models().unwrap();
        assert_eq!(models, vec![("llama3".to_string(), "llama3".to_string())]);
    }
}
