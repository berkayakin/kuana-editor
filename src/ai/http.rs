//! HTTP for model requests: `ureq`, blocking, run on a thread of its own by
//! the caller (no async runtime beside GPUI's executors). Certificates come
//! from the system, so a company's TLS-inspecting proxy works, and
//! `HTTPS_PROXY` / `ALL_PROXY` are honored.

use std::{io::BufRead, sync::OnceLock, time::Duration};

use serde_json::Value;

/// How long a stream may stay silent before it counts as stalled.
const STALL: Duration = Duration::from_secs(300);

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        use ureq::tls::{RootCerts, TlsConfig};
        ureq::Agent::config_builder()
            .tls_config(TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build())
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_per_read(Some(STALL))
            .user_agent(format!("Kuana/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .into()
    })
}

/// A failed request: its status (none when it never got one) and what the
/// provider said, or the network error.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpError {
    pub status: Option<u16>,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl HttpError {
    /// Whether trying again may work: rate limits, overload, server errors,
    /// connection failures.
    pub fn retryable(&self) -> bool {
        match self.status {
            None => true,
            Some(status) => status == 408 || status == 409 || status == 429 || status >= 500,
        }
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(401) | Some(403) => write!(f, "The provider refused the key ({}): {}", self.status.unwrap_or_default(), self.message),
            Some(status) => write!(f, "{} ({status})", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

type Headers<'a> = &'a [(String, String)];

/// POSTs JSON and hands each line of a successful response to `line` until
/// it returns false or the body ends.
pub fn post_lines(url: &str, headers: Headers, body: &Value, mut line: impl FnMut(&str) -> bool) -> Result<(), HttpError> {
    let mut request = agent().post(url).header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send(body.to_string()).map_err(network)?;
    let response = check(response)?;
    let mut reader = std::io::BufReader::new(response.into_body().into_reader());
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                if !line(buf.trim_end_matches(['\n', '\r'])) {
                    return Ok(());
                }
            }
            Err(err) => return Err(HttpError { status: None, message: format!("The connection broke: {err}"), retry_after: None }),
        }
    }
}

/// GETs JSON.
pub fn get_json(url: &str, headers: Headers) -> Result<Value, HttpError> {
    let mut request = agent().get(url);
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.call().map_err(network)?;
    let response = check(response)?;
    let text = response.into_body().with_config().limit(16 * 1024 * 1024).read_to_string().map_err(network)?;
    serde_json::from_str(&text).map_err(|e| HttpError { status: None, message: format!("Not JSON: {e}"), retry_after: None })
}

fn network(err: ureq::Error) -> HttpError {
    let message = match &err {
        ureq::Error::Io(io) => format!("Couldn't reach the provider: {io}"),
        ureq::Error::Timeout(_) => "The provider didn't answer in time".to_string(),
        ureq::Error::HostNotFound => "Couldn't find the provider's server (offline?)".to_string(),
        other => format!("Couldn't reach the provider: {other}"),
    };
    HttpError { status: None, message, retry_after: None }
}

/// A 2xx response, or its status and the provider's message.
fn check(mut response: ureq::http::Response<ureq::Body>) -> Result<ureq::http::Response<ureq::Body>, HttpError> {
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(response);
    }
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite())
        .map(|s| Duration::from_secs_f64(s.clamp(0., 120.)));
    let text = response.body_mut().with_config().limit(1024 * 1024).read_to_string().unwrap_or_default();
    Err(HttpError { status: Some(status), message: error_message(&text, status), retry_after })
}

/// The message in a provider's error body: Anthropic's
/// `{"error": {"message"}}`, OpenAI's, Ollama's `{"error": "…"}`.
pub fn error_message(body: &str, status: u16) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let from_json = parsed.as_ref().and_then(|v| {
        let error = v.get("error")?;
        error.get("message").and_then(Value::as_str).or_else(|| error.as_str()).map(str::to_string)
    });
    from_json
        .or_else(|| parsed.as_ref().and_then(|v| v.get("message").and_then(Value::as_str)).map(str::to_string))
        .unwrap_or_else(|| {
            let body = body.trim();
            if body.is_empty() || body.len() > 300 { format!("HTTP {status}") } else { body.to_string() }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_error_bodies() {
        assert_eq!(error_message(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#, 529), "Overloaded");
        assert_eq!(error_message(r#"{"error":"model 'x' not found"}"#, 404), "model 'x' not found");
        assert_eq!(error_message(r#"{"message":"Unauthorized"}"#, 401), "Unauthorized");
        assert_eq!(error_message("", 502), "HTTP 502");
        assert_eq!(error_message("Bad Gateway", 502), "Bad Gateway");
    }

    #[test]
    fn which_errors_are_worth_retrying() {
        let at = |status| HttpError { status, message: String::new(), retry_after: None };
        assert!(at(Some(429)).retryable() && at(Some(529)).retryable() && at(None).retryable());
        assert!(!at(Some(400)).retryable() && !at(Some(401)).retryable() && !at(Some(404)).retryable());
        assert!(at(Some(401)).to_string().starts_with("The provider refused the key"));
    }
}
