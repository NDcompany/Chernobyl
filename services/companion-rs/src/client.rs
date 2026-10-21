//! Minimal blocking WebSocket client — used by integration tests and the
//! `--self-test` probe. Client frames are masked per RFC 6455.

use crate::base64;
use crate::sha1::sha1;
use crate::ws::{self, Frame};
use std::io::{BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub struct WsClient {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
}

impl WsClient {
    /// Connects, performs the WS handshake, and validates the accept key.
    pub fn connect(addr: &str, path: &str, token: Option<&str>) -> Result<WsClient, String> {
        let stream = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        let mut key_src = [0u8; 16];
        // deterministic nonce is fine for a client; servers accept any 16 bytes
        for (i, b) in key_src.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(41);
        }
        let key = base64::encode(&key_src);
        let auth = match token {
            Some(t) => format!("Authorization: Bearer {t}\r\n"),
            None => String::new(),
        };
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n{auth}\r\n"
        );
        let mut stream2 = stream.try_clone().map_err(|e| e.to_string())?;
        stream2
            .write_all(req.as_bytes())
            .map_err(|e| format!("write handshake: {e}"))?;

        // read the response head
        let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if head.len() > ws::MAX_HANDSHAKE_BYTES {
                return Err("handshake response too large".into());
            }
            reader.read_exact(&mut byte).map_err(|e| e.to_string())?;
            head.push(byte[0]);
            if head.len() >= 4 && &head[head.len() - 4..] == b"\r\n\r\n" {
                break;
            }
        }
        let text = String::from_utf8(head).map_err(|_| "bad handshake response")?;
        let status_line = text.lines().next().unwrap_or("");
        if !status_line.contains("101") {
            return Err(format!("handshake rejected: {status_line}"));
        }
        let mut expected = key.into_bytes();
        expected.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        let expected = base64::encode(&sha1(&expected));
        if !text.contains(&expected) {
            return Err("accept key mismatch".into());
        }
        let _ = stream2;
        Ok(WsClient { stream, reader })
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), String> {
        let frame = ws::encode_frame(ws::OP_TEXT, text.as_bytes(), true);
        self.stream
            .write_all(&frame)
            .map_err(|e| format!("send: {e}"))
    }

    pub fn send_raw(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream
            .write_all(bytes)
            .map_err(|e| format!("send: {e}"))
    }

    /// Sends a masked close frame.
    pub fn close(&mut self, code: u16) -> Result<(), String> {
        let mut payload = code.to_be_bytes().to_vec();
        payload.extend_from_slice(b"bye");
        let frame = ws::encode_frame(ws::OP_CLOSE, &payload, true);
        self.stream.write_all(&frame).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn read_frame(&mut self) -> Result<Frame, String> {
        ws::read_frame(&mut self.reader, false).map_err(|e| format!("read frame: {e:?}"))
    }

    /// Reads frames until a Text frame arrives; answers pings; returns the text.
    pub fn next_text(&mut self) -> Result<String, String> {
        loop {
            match self.read_frame()? {
                Frame::Text(p) => {
                    return String::from_utf8(p).map_err(|_| "text frame was not utf-8".into());
                }
                Frame::Ping(p) => {
                    let pong = ws::encode_frame(ws::OP_PONG, &p, true);
                    self.stream.write_all(&pong).map_err(|e| e.to_string())?;
                }
                Frame::Close(code, reason) => {
                    return Err(format!("closed by server: {code:?} {reason}"));
                }
                _ => {}
            }
        }
    }

    /// Reads until a frame satisfying `pred`, skipping/buffering the rest.
    pub fn next_text_matching(
        &mut self,
        pred: impl Fn(&str) -> bool,
        timeout: Duration,
    ) -> Result<String, String> {
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| e.to_string())?;
        let out = loop {
            let text = self.next_text()?;
            if pred(&text) {
                break text;
            }
        };
        self.stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// Sends `hello` and asserts a hello_ack arrives; returns lastSeq.
    pub fn hello(&mut self) -> Result<i64, String> {
        self.send_text(r#"{"type":"hello","clientId":"test-client","protocol":2}"#)?;
        let ack = self.next_text()?;
        let v = crate::json::parse(&ack).map_err(|e| format!("hello_ack parse: {e}"))?;
        if crate::json::get_str(&v, "type") != Some("hello_ack") {
            return Err(format!("expected hello_ack, got: {ack}"));
        }
        Ok(crate::json::get_i64(&v, "lastSeq").unwrap_or(-1))
    }
}

/// Sends a raw HTTP request and returns the raw response head+body (for
/// probing /health and non-upgrade routes).
pub fn http_get(addr: &str, path: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut reader = BufReader::new(stream);
    let raw = read_http_response(&mut reader)?;
    String::from_utf8(raw).map_err(|_| "response was not utf-8".into())
}

/* ---- Generic blocking HTTP (adapter transport, PRD §7.4) ---------------- */

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    /// Lowercased `Content-Type` header ("" when absent).
    pub content_type: String,
    pub body: String,
}

/// Minimal HTTP/1.1 request/response for loopback integrations (no TLS by
/// design: PRD §7.4 keeps native surfaces local+authenticated). Bodies are
/// framed per RFC 7230 §3.3.3 (Content-Length / chunked / no-body status):
/// a response is complete when its framing is complete — NOT when the peer
/// closes the connection (opencode 1.17.7 answers and can keep the socket
/// open; the old read-to-end client reported that as a false timeout).
/// Splits `http://host[:port]/path` into `(host[:port], /path…)` — shared by
/// the one-shot request and the SSE stream (loopback HTTP only, no TLS).
pub(crate) fn split_url(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// endpoints are supported (got {url})"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    Ok((authority.to_string(), path.to_string()))
}

pub fn http_request(
    method: &str,
    url: &str,
    auth: Option<&str>,
    body: Option<&str>,
) -> Result<HttpResponse, String> {
    let (authority, path) = split_url(url)?;
    let authority = authority.as_str();
    let path = path.as_str();
    let mut addrs = authority
        .to_socket_addrs()
        .map_err(|e| format!("resolve {authority}: {e}"))?;
    let addr = addrs
        .next()
        .ok_or_else(|| format!("no addresses for {authority}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);

    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
    if let Some(a) = auth {
        req.push_str(&format!(
            "Authorization: Basic {}\r\n",
            crate::base64::encode(a.as_bytes())
        ));
    }
    match body {
        Some(b) => {
            req.push_str("Content-Type: application/json\r\n");
            req.push_str(&format!("Content-Length: {}\r\n\r\n", b.len()));
            req.push_str(b);
        }
        None => req.push_str("\r\n"),
    }
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut reader = BufReader::new(stream);
    let raw = read_http_response(&mut reader)?;
    parse_response(&raw)
}

/// Read errors keep the bytes received so far (G4.1 honesty: a "timeout"
/// that actually carried a complete answer stays visible in the error).
fn read_err(e: impl std::fmt::Display, raw: &[u8], started: std::time::Instant) -> String {
    if raw.is_empty() {
        format!("read: {e}")
    } else {
        format!(
            "read: {e} ({} bytes after {:?}: {:?})",
            raw.len(),
            started.elapsed(),
            String::from_utf8_lossy(&raw[..raw.len().min(160)])
        )
    }
}

/// Reads one HTTP/1.1 response: headers until `\r\n\r\n`, then the body per
/// the framing rules (1xx/204/304 have none, otherwise Content-Length,
/// chunked, or read-to-close). A self-delimiting body completes WITHOUT
/// waiting for the peer to close — the Phase-3 live run proved opencode
/// delivers a full `200 true` abort answer and keeps the socket open.
fn read_http_response(reader: &mut BufReader<TcpStream>) -> Result<Vec<u8>, String> {
    const MAX_HEAD: usize = 64 * 1024;
    let started = std::time::Instant::now();
    let mut raw: Vec<u8> = Vec::with_capacity(4096);
    let mut buf = [0u8; 8192];

    // 1) headers
    let head_end = loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if raw.len() > MAX_HEAD {
            return Err(format!("response head exceeds {MAX_HEAD} bytes"));
        }
        match reader.read(&mut buf) {
            Ok(0) => return Err(read_err("connection closed before headers", &raw, started)),
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) => return Err(read_err(e, &raw, started)),
        }
    };

    // 2) parse just enough of the head to size the body
    let head = std::str::from_utf8(&raw[..head_end]).map_err(|_| "response head was not ascii")?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = v.trim().parse().ok(),
                "transfer-encoding" => chunked = v.trim().to_ascii_lowercase().contains("chunked"),
                _ => {}
            }
        }
    }

    // 3) body
    if (100..200).contains(&status) || status == 204 || status == 304 {
        raw.truncate(head_end);
        return Ok(raw);
    }
    if chunked {
        // shortcut: a 0-chunk followed by trailer headers (RFC-legal, never
        // seen on loopback) is not detected and falls back to read-to-close
        loop {
            if raw.ends_with(b"0\r\n\r\n") || raw.windows(7).any(|w| w == b"\r\n0\r\n\r\n") {
                return Ok(raw);
            }
            match reader.read(&mut buf) {
                Ok(0) => {
                    return Err(read_err(
                        "connection closed mid chunked body",
                        &raw,
                        started,
                    ));
                }
                Ok(n) => raw.extend_from_slice(&buf[..n]),
                Err(e) => return Err(read_err(e, &raw, started)),
            }
        }
    }
    if let Some(len) = content_length {
        loop {
            if raw.len() >= head_end + len {
                raw.truncate(head_end + len);
                return Ok(raw);
            }
            match reader.read(&mut buf) {
                Ok(0) => {
                    return Err(read_err(
                        format!(
                            "connection closed after {} of {len} body bytes",
                            raw.len() - head_end
                        ),
                        &raw,
                        started,
                    ));
                }
                Ok(n) => raw.extend_from_slice(&buf[..n]),
                Err(e) => return Err(read_err(e, &raw, started)),
            }
        }
    }
    // HTTP/1.0-style: body delimited by close
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(raw),
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) => return Err(read_err(e, &raw, started)),
        }
    }
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("response has no header terminator")?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| "response head was not ascii")?;
    let mut body = raw[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line: {status_line}"))?;
    let mut content_type = String::new();
    let mut chunked = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            if k == "content-type" {
                content_type = v.to_ascii_lowercase();
            } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }
    if chunked {
        body = decode_chunked(&body)?;
    }
    let body = match String::from_utf8(body) {
        Ok(b) => b,
        Err(_) => return Err("response body was not utf-8".into()),
    };
    Ok(HttpResponse {
        status,
        content_type,
        body,
    })
}

/// Decodes an HTTP/1.1 chunked body (hex size lines, `0` terminator,
/// trailers ignored). Used for one-shot responses; the SSE stream decodes
/// incrementally in the adapter.
pub fn decode_chunked(raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut rest = raw;
    loop {
        let eol = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("chunked body: missing size line terminator")?;
        let size_line =
            std::str::from_utf8(&rest[..eol]).map_err(|_| "chunk size line was not ascii")?;
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| format!("bad chunk size: {size_hex}"))?;
        rest = &rest[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if rest.len() < size + 2 {
            return Err("chunked body: truncated chunk".into());
        }
        out.extend_from_slice(&rest[..size]);
        if &rest[size..size + 2] != b"\r\n" {
            return Err("chunked body: missing chunk terminator".into());
        }
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;

    /// Serves `resp` once and deliberately never closes the socket (no FIN),
    /// replicating the live opencode 1.17.7 behavior the Phase-3 run hit.
    fn serve_once_without_fin(resp: &[u8]) -> String {
        serve_once(resp, false)
    }

    /// `close=true` drops the socket after writing (sends FIN).
    fn serve_once(resp: &[u8], close: bool) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let resp = resp.to_vec();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut req = [0u8; 4096];
            let _ = s.read(&mut req);
            let _ = s.write_all(&resp);
            if close {
                drop(s);
            } else {
                std::mem::forget(s);
            }
        });
        format!("http://{addr}/probe")
    }

    #[test]
    fn http_request_returns_on_framing_not_fin() {
        // live failure mode: complete Content-Length answer, no close
        let url = serve_once_without_fin(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4\r\n\r\ntrue",
        );
        let r = http_request("POST", &url, None, None).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "true");

        // prompt_async shape: 204 with no body framing at all
        let url = serve_once_without_fin(b"HTTP/1.1 204 No Content\r\n\r\n");
        let r = http_request("POST", &url, None, Some("{}")).unwrap();
        assert_eq!(r.status, 204);
        assert_eq!(r.body, "");

        // chunked body completes on its 0-chunk, also without a FIN
        let url = serve_once_without_fin(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"a\":1}\r\n0\r\n\r\n",
        );
        let r = http_request("GET", &url, None, None).unwrap();
        assert_eq!(r.body, "{\"a\":1}");
    }

    #[test]
    fn http_request_reports_truncation_honestly() {
        // Content-Length promises more than the server delivers, then closes:
        // the shortfall must be named, with the partial bytes still visible
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 40\r\n\r\ntrue",
            true,
        );
        let err = http_request("POST", &url, None, None).unwrap_err();
        assert!(
            err.contains("connection closed after 4 of 40 body bytes"),
            "got: {err}"
        );
        assert!(
            err.contains("true"),
            "partial bytes must stay visible: {err}"
        );
    }

    #[test]
    fn parse_plain_and_chunked_responses() {
        let plain =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\n\r\n{\"a\":1}";
        let r = parse_response(plain).unwrap();
        assert_eq!(r.status, 200);
        assert!(r.content_type.starts_with("application/json"));
        assert_eq!(r.body, "{\"a\":1}");

        // two chunks + terminator
        let body = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n"
                .to_vec();
        raw.extend_from_slice(body);
        let r = parse_response(&raw).unwrap();
        assert_eq!(r.body, "hello world");
        assert_eq!(decode_chunked(b"0\r\n\r\n").unwrap(), b"" as &[u8]);
        assert!(decode_chunked(b"zz\r\nx\r\n").is_err());
        assert!(decode_chunked(b"10\r\nshort").is_err());
    }
}
