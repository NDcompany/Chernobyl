//! Minimal blocking WebSocket client — used by integration tests and the
//! `--self-test` probe. Client frames are masked per RFC 6455.

use crate::base64;
use crate::sha1::sha1;
use crate::ws::{self, Frame};
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
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
    let mut out = Vec::new();
    reader
        .read_to_end(&mut out)
        .map_err(|e| format!("read: {e}"))?;
    String::from_utf8(out).map_err(|_| "response was not utf-8".into())
}
