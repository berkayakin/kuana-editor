//! The bit of HTTP/1.1 Gemini CLI's MCP client needs (Node's fetch through
//! undici): requests with a `Content-Length` or chunked body on connections
//! kept alive, plain responses, and an event stream sent in chunks.

use std::io::{self, BufRead, Read as _, Write};

/// Headers past this are refused (Node sends well under 1 KB).
const MAX_HEAD: usize = 64 * 1024;
/// A body past this is refused: openDiff carries a whole file.
const MAX_BODY: usize = 64 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub struct Request {
    pub method: String,
    /// The target without its query.
    pub path: String,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// A header's value (names compare ignoring case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Whether the client asked to close the connection after this.
    pub fn closes(&self) -> bool {
        self.header("connection").is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")))
    }

    /// Whether `Accept` lists this media type.
    pub fn accepts(&self, media: &str) -> bool {
        self.header("accept").is_some_and(|v| v.split(',').any(|t| t.split(';').next().is_some_and(|m| m.trim().eq_ignore_ascii_case(media))))
    }
}

fn bad(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

/// Reads one request; None when the client closed the connection between
/// requests.
pub fn read_request(reader: &mut impl BufRead) -> io::Result<Option<Request>> {
    let mut head = Vec::new();
    let mut lines = Vec::new();
    loop {
        let start = head.len();
        if reader.by_ref().take((MAX_HEAD - start) as u64 + 1).read_until(b'\n', &mut head)? == 0 {
            return if head.is_empty() { Ok(None) } else { Err(bad("connection closed in the headers")) };
        }
        if head.len() > MAX_HEAD {
            return Err(bad("headers too long"));
        }
        let line = String::from_utf8_lossy(&head[start..]).trim_end_matches(['\r', '\n']).to_string();
        // Blank lines before a request are allowed.
        if line.is_empty() && lines.is_empty() {
            continue;
        }
        if line.is_empty() {
            break;
        }
        lines.push(line);
    }
    let mut request_line = lines[0].split(' ');
    let (Some(method), Some(target), Some(version)) = (request_line.next(), request_line.next(), request_line.next()) else {
        return Err(bad("bad request line"));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(bad("not HTTP/1"));
    }
    let mut headers = Vec::new();
    for line in &lines[1..] {
        let (name, value) = line.split_once(':').ok_or_else(|| bad("bad header"))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    let mut request = Request {
        method: method.to_string(),
        path: target.split('?').next().unwrap_or_default().to_string(),
        headers,
        body: Vec::new(),
    };
    if request.header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        request.body = read_chunked(reader)?;
    } else if let Some(length) = request.header("content-length") {
        let length: usize = length.parse().map_err(|_| bad("bad content length"))?;
        if length > MAX_BODY {
            return Err(bad("body too large"));
        }
        request.body = vec![0; length];
        reader.read_exact(&mut request.body)?;
    }
    Ok(Some(request))
}

fn read_chunked(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        reader.by_ref().take(1024).read_line(&mut line)?;
        let size = line.trim().split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size, 16).map_err(|_| bad("bad chunk size"))?;
        if size == 0 {
            // Trailers, up to the blank line.
            loop {
                line.clear();
                if reader.by_ref().take(MAX_HEAD as u64).read_line(&mut line)? == 0 || line.trim().is_empty() {
                    return Ok(body);
                }
            }
        }
        if body.len() + size > MAX_BODY {
            return Err(bad("body too large"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        let mut end = [0; 2];
        reader.read_exact(&mut end)?;
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    }
}

/// Writes a whole response.
pub fn respond(out: &mut impl Write, status: u16, headers: &[(&str, &str)], body: &[u8], close: bool) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\nContent-Length: {}\r\n", reason(status), body.len());
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(if close { "Connection: close\r\n\r\n" } else { "Connection: keep-alive\r\n\r\n" });
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    out.write_all(&bytes)?;
    out.flush()
}

/// Starts an event stream: its events follow as chunks ([`event`]).
pub fn start_events(out: &mut impl Write, session: &str) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\nmcp-session-id: {session}\r\n\r\n"
    );
    out.write_all(head.as_bytes())?;
    out.flush()
}

/// One event-stream message as a chunk (`None`: a comment that only keeps
/// the stream from looking idle).
pub fn event(out: &mut impl Write, data: Option<&str>) -> io::Result<()> {
    let text = match data {
        // JSON written by serde has no line breaks to split on.
        Some(data) => format!("event: message\ndata: {data}\n\n"),
        None => ": keepalive\n\n".to_string(),
    };
    out.write_all(format!("{:x}\r\n{text}\r\n", text.len()).as_bytes())?;
    out.flush()
}

/// Ends an event stream.
pub fn end_events(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"0\r\n\r\n")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn read_all(bytes: &[u8]) -> Vec<Request> {
        let mut reader = BufReader::new(bytes);
        std::iter::from_fn(|| read_request(&mut reader).unwrap()).collect()
    }

    #[test]
    fn reads_requests_kept_alive() {
        let bytes = b"POST /mcp?x=1 HTTP/1.1\r\nHost: 127.0.0.1:9\r\nContent-Length: 2\r\nAccept: application/json, text/event-stream\r\n\r\n{}\
GET /mcp HTTP/1.1\r\nconnection: close\r\n\r\n";
        let requests = read_all(bytes);
        assert_eq!(requests.len(), 2);
        assert_eq!((requests[0].method.as_str(), requests[0].path.as_str(), &requests[0].body[..]), ("POST", "/mcp", &b"{}"[..]));
        assert_eq!(requests[0].header("host"), Some("127.0.0.1:9"));
        assert!(requests[0].accepts("text/event-stream") && requests[0].accepts("application/json"));
        assert!(!requests[0].closes());
        assert!(requests[1].closes() && requests[1].body.is_empty());
    }

    #[test]
    fn reads_chunked_bodies() {
        let bytes = b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n4;ext=1\r\n\":1}\r\n0\r\nTrailer: x\r\n\r\n";
        assert_eq!(read_all(bytes)[0].body, b"{\"a\":1}");
    }

    #[test]
    fn refuses_what_isnt_http() {
        let mut reader = BufReader::new(&b"hello\r\n\r\n"[..]);
        assert!(read_request(&mut reader).is_err());
        let mut reader = BufReader::new(&b"POST / HTTP/1.1\r\nContent-Length: 99999999999\r\n\r\n"[..]);
        assert!(read_request(&mut reader).is_err());
        let mut reader = BufReader::new(&b"POST / HTTP/1.1\r\nHost"[..]);
        assert!(read_request(&mut reader).is_err());
        let mut reader = BufReader::new(&b""[..]);
        assert!(read_request(&mut reader).unwrap().is_none());
    }

    #[test]
    fn writes_responses_and_events() {
        let mut out = Vec::new();
        respond(&mut out, 202, &[("mcp-session-id", "s")], b"", false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n"));
        assert!(text.contains("mcp-session-id: s\r\n") && text.ends_with("Connection: keep-alive\r\n\r\n"));
        let mut out = Vec::new();
        event(&mut out, Some("{}")).unwrap();
        end_events(&mut out).unwrap();
        assert_eq!(out, b"19\r\nevent: message\ndata: {}\n\n\r\n0\r\n\r\n");
    }
}
