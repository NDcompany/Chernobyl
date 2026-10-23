//! RFC 6455 WebSocket: HTTP handshake + frame codec. Zero dependencies —
//! SHA-1/base64 live in their own modules with published test vectors.
//! Every length, opcode and mask path is strict; garbage closes, never panics.

use crate::base64;
use crate::sha1::sha1;
use std::io::{self, Read, Write};

pub const MAX_PAYLOAD_BYTES: usize = 256_000;
pub const MAX_HANDSHAKE_BYTES: usize = 16_384;
const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub const OP_CONT: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xa;

#[derive(Debug)]
pub enum WsError {
    Io(io::Error),
    HandshakeTooLarge,
    BadHandshake,
    NotAWebSocket,
    BadVersion,
    BadKey,
    FrameTooLarge,
    ControlFrameTooLarge,
    FragmentedControl,
    UnmaskedClientFrame,
    BadOpcode,
    NonZeroRsv,
    LengthOverflow,
    BadUtf8,
    BadClosePayload,
}

/// Io errors compare by kind (io::Error is not PartialEq); all other
/// variants compare by discriminant.
impl PartialEq for WsError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (WsError::Io(a), WsError::Io(b)) => a.kind() == b.kind(),
            (WsError::HandshakeTooLarge, WsError::HandshakeTooLarge)
            | (WsError::BadHandshake, WsError::BadHandshake)
            | (WsError::NotAWebSocket, WsError::NotAWebSocket)
            | (WsError::BadVersion, WsError::BadVersion)
            | (WsError::BadKey, WsError::BadKey)
            | (WsError::FrameTooLarge, WsError::FrameTooLarge)
            | (WsError::ControlFrameTooLarge, WsError::ControlFrameTooLarge)
            | (WsError::FragmentedControl, WsError::FragmentedControl)
            | (WsError::UnmaskedClientFrame, WsError::UnmaskedClientFrame)
            | (WsError::BadOpcode, WsError::BadOpcode)
            | (WsError::NonZeroRsv, WsError::NonZeroRsv)
            | (WsError::LengthOverflow, WsError::LengthOverflow)
            | (WsError::BadUtf8, WsError::BadUtf8)
            | (WsError::BadClosePayload, WsError::BadClosePayload) => true,
            _ => false,
        }
    }
}

impl From<io::Error> for WsError {
    fn from(e: io::Error) -> Self {
        WsError::Io(e)
    }
}

#[derive(Debug, PartialEq)]
pub enum Frame {
    Text(Vec<u8>),
    Binary(Vec<u8>),
    Continuation(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<u16>, String),
}

/* ------------------------------------------------------------------ */
/* Handshake                                                           */
/* ------------------------------------------------------------------ */

#[derive(Debug, PartialEq)]
pub struct Handshake {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>, // lower-cased names
}

impl Handshake {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn is_websocket_upgrade(&self) -> bool {
        let upgrade = self.header("upgrade").map(str::to_ascii_lowercase);
        let connection = self.header("connection").unwrap_or("");
        upgrade.as_deref() == Some("websocket")
            && connection
                .split(',')
                .map(|t| t.trim().to_ascii_lowercase())
                .any(|t| t == "upgrade")
            && self.header("sec-websocket-key").is_some()
    }
}

/// Reads the HTTP request head (up to `\r\n\r\n`), enforcing the size cap.
pub fn read_handshake<R: Read>(r: &mut R) -> Result<Handshake, WsError> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        if buf.len() > MAX_HANDSHAKE_BYTES {
            return Err(WsError::HandshakeTooLarge);
        }
        r.read_exact(&mut byte)?;
        buf.push(byte[0]);
        if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
            break;
        }
    }
    let text = String::from_utf8(buf).map_err(|_| WsError::BadHandshake)?;
    parse_handshake(&text)
}

/// Maximum accepted POST body for the loopback hook ingress. A Claude Code
/// hook payload is small (sub-1 KB); this cap bounds the ingress memory.
pub const MAX_HOOK_BODY_BYTES: usize = 64 * 1024;

fn parse_handshake(text: &str) -> Result<Handshake, WsError> {
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or(WsError::BadHandshake)?;
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let version = parts.next().unwrap_or("");
    // GET for the WS upgrade + HTTP routes; POST only for the loopback hook
    // ingress (Phase 7). `validate_upgrade` additionally requires GET, so a
    // POST can never reach the upgrade path regardless of its headers.
    if (method != "GET" && method != "POST") || !version.starts_with("HTTP/1.") {
        return Err(WsError::BadHandshake);
    }
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        } else {
            return Err(WsError::BadHandshake);
        }
        if headers.len() > 64 {
            return Err(WsError::BadHandshake);
        }
    }
    Ok(Handshake {
        method,
        target,
        headers,
    })
}

/// Validates the upgrade headers and returns the accept value.
pub fn validate_upgrade(h: &Handshake) -> Result<String, WsError> {
    // RFC 6455 upgrades are GET-only; a POST (the loopback hook ingress) can
    // never reach the upgrade path regardless of the headers it carries.
    if h.method != "GET" {
        return Err(WsError::BadHandshake);
    }
    if !h.is_websocket_upgrade() {
        return Err(WsError::NotAWebSocket);
    }
    let version = h.header("sec-websocket-version").unwrap_or("");
    if version != "13" {
        return Err(WsError::BadVersion);
    }
    let key = h.header("sec-websocket-key").ok_or(WsError::BadKey)?;
    let raw = base64::decode(key).ok_or(WsError::BadKey)?;
    if raw.len() != 16 {
        return Err(WsError::BadKey);
    }
    let mut src = key.as_bytes().to_vec();
    src.extend_from_slice(WS_GUID);
    Ok(base64::encode(&sha1(&src)))
}

pub fn write_handshake_response<W: Write>(w: &mut W, accept: &str) -> io::Result<()> {
    write!(
        w,
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    )
}

pub fn write_http_error<W: Write>(
    w: &mut W,
    code: u16,
    reason: &str,
    extra: &str,
) -> io::Result<()> {
    let body = format!("{{\"error\":\"{reason}\"}}");
    write!(
        w,
        "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra}\r\n{body}",
        body.len()
    )
}

/* ------------------------------------------------------------------ */
/* Frames                                                              */
/* ------------------------------------------------------------------ */

/// Encodes a single frame. `mask` is true for client→server direction.
pub fn encode_frame(opcode: u8, payload: &[u8], mask: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    let mut b0 = opcode & 0x0f;
    b0 |= 0x80; // FIN: we never fragment outbound
    out.push(b0);
    let mask_bit: u8 = if mask { 0x80 } else { 0 };
    if payload.len() < 126 {
        out.push(mask_bit | payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        out.push(mask_bit | 126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        out.push(mask_bit | 127);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    if mask {
        // deterministic key from a counter-free source: derive from payload
        // length + first bytes is unnecessary — any 4 bytes work; use a fixed
        // rotating pattern seeded by a time-free value for testability.
        let key = mask_key(payload.len());
        out.extend_from_slice(&key);
        for (i, b) in payload.iter().enumerate() {
            out.push(b ^ key[i % 4]);
        }
    } else {
        out.extend_from_slice(payload);
    }
    out
}

fn mask_key(len: usize) -> [u8; 4] {
    // Any key satisfies RFC; derive deterministically so tests can craft and
    // verify frames. (Clients in the wild pick random keys — we accept any.)
    let x = (len as u32).wrapping_mul(0x9E37_79B9);
    x.to_be_bytes()
}

/// Reads exactly one frame; `mask_required` enforces the client-must-mask rule.
pub fn read_frame<R: Read>(r: &mut R, mask_required: bool) -> Result<Frame, WsError> {
    let mut head = [0u8; 2];
    r.read_exact(&mut head)?;
    let b0 = head[0];
    let b1 = head[1];
    if b0 & 0x70 != 0 {
        return Err(WsError::NonZeroRsv);
    }
    let fin = b0 & 0x80 != 0;
    let opcode = b0 & 0x0f;
    let masked = b1 & 0x80 != 0;
    let mut len = (b1 & 0x7f) as u64;
    if len == 126 {
        let mut ext = [0u8; 2];
        r.read_exact(&mut ext)?;
        len = u16::from_be_bytes(ext) as u64;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        r.read_exact(&mut ext)?;
        if ext[0] & 0x80 != 0 {
            return Err(WsError::LengthOverflow);
        }
        len = u64::from_be_bytes(ext);
    }
    if len > MAX_PAYLOAD_BYTES as u64 {
        return Err(WsError::FrameTooLarge);
    }
    let is_control = opcode & 0x08 != 0;
    if is_control {
        if !fin {
            return Err(WsError::FragmentedControl);
        }
        if len > 125 {
            return Err(WsError::ControlFrameTooLarge);
        }
    }
    if mask_required && !masked {
        return Err(WsError::UnmaskedClientFrame);
    }
    let mut key = [0u8; 4];
    if masked {
        r.read_exact(&mut key)?;
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= key[i % 4];
        }
    }
    match opcode {
        OP_TEXT => {
            std::str::from_utf8(&payload).map_err(|_| WsError::BadUtf8)?;
            Ok(Frame::Text(payload))
        }
        OP_BINARY => Ok(Frame::Binary(payload)),
        OP_CONT => Ok(Frame::Continuation(payload)),
        OP_PING => Ok(Frame::Ping(payload)),
        OP_PONG => Ok(Frame::Pong(payload)),
        OP_CLOSE => {
            if payload.is_empty() {
                Ok(Frame::Close(None, String::new()))
            } else if payload.len() == 1 {
                Err(WsError::BadClosePayload)
            } else {
                let code = u16::from_be_bytes([payload[0], payload[1]]);
                let reason =
                    String::from_utf8(payload[2..].to_vec()).map_err(|_| WsError::BadUtf8)?;
                Ok(Frame::Close(Some(code), reason))
            }
        }
        _ => Err(WsError::BadOpcode),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn handshake_parses_and_validates() {
        let raw = "GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
        let h = read_handshake(&mut Cursor::new(raw.as_bytes())).unwrap();
        assert_eq!(h.method, "GET");
        let accept = validate_upgrade(&h).unwrap();
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn handshake_rejects_bad_input() {
        // not a websocket upgrade
        let raw = "GET /health HTTP/1.1\r\nHost: x\r\n\r\n";
        let h = read_handshake(&mut Cursor::new(raw.as_bytes())).unwrap();
        assert_eq!(validate_upgrade(&h), Err(WsError::NotAWebSocket));
        // bad version
        let raw = "GET / HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 8\r\n\r\n";
        let h = read_handshake(&mut Cursor::new(raw.as_bytes())).unwrap();
        assert_eq!(validate_upgrade(&h), Err(WsError::BadVersion));
        // key not 16 bytes
        let raw = "GET / HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: YQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
        let h = read_handshake(&mut Cursor::new(raw.as_bytes())).unwrap();
        assert_eq!(validate_upgrade(&h), Err(WsError::BadKey));
        // unsupported method (neither GET nor POST) is still rejected at parse
        let raw = "PUT / HTTP/1.1\r\n\r\n";
        assert_eq!(
            read_handshake(&mut Cursor::new(raw.as_bytes())),
            Err(WsError::BadHandshake)
        );
        // POST parses (loopback hook ingress), but a POST — even with full
        // upgrade headers — can NEVER reach the WS upgrade path.
        let raw = "POST /hooks/claude HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
        let h = read_handshake(&mut Cursor::new(raw.as_bytes())).unwrap();
        assert_eq!(h.method, "POST");
        assert_eq!(validate_upgrade(&h), Err(WsError::BadHandshake));
    }

    #[test]
    fn handshake_size_cap() {
        let mut raw = b"GET / HTTP/1.1\r\n".to_vec();
        raw.extend(std::iter::repeat_n(b'a', MAX_HANDSHAKE_BYTES + 100));
        assert_eq!(
            read_handshake(&mut Cursor::new(&raw)),
            Err(WsError::HandshakeTooLarge)
        );
    }

    fn round_trip(opcode: u8, payload: &[u8]) {
        let bytes = encode_frame(opcode, payload, true);
        let mut cur = Cursor::new(bytes);
        let frame = read_frame(&mut cur, true).unwrap();
        match frame {
            Frame::Text(p) | Frame::Binary(p) | Frame::Ping(p) | Frame::Pong(p) => {
                assert_eq!(p, payload)
            }
            _ => panic!("unexpected frame kind"),
        }
    }

    #[test]
    fn frame_round_trips_all_lengths() {
        round_trip(OP_TEXT, b"hi");
        round_trip(OP_TEXT, &[b'x'; 125]);
        round_trip(OP_TEXT, &[b'x'; 126]);
        round_trip(OP_TEXT, &[b'x'; 70_000]); // extended 16-bit length
        round_trip(OP_PING, b"");
        round_trip(OP_BINARY, &[0, 159, 146, 159]);
    }

    #[test]
    fn close_frames_carry_code_and_reason() {
        let mut payload = 1000u16.to_be_bytes().to_vec();
        payload.extend_from_slice(b"bye");
        let bytes = encode_frame(OP_CLOSE, &payload, true);
        let frame = read_frame(&mut Cursor::new(bytes), true).unwrap();
        assert_eq!(frame, Frame::Close(Some(1000), "bye".into()));
    }

    #[test]
    fn hostile_frames_error_not_panic() {
        // unmasked client frame → protocol error
        let bytes = encode_frame(OP_TEXT, b"hi", false);
        assert_eq!(
            read_frame(&mut Cursor::new(bytes), true),
            Err(WsError::UnmaskedClientFrame)
        );
        // reserved bits set
        let mut bytes = encode_frame(OP_TEXT, b"hi", true);
        bytes[0] |= 0x40;
        assert_eq!(
            read_frame(&mut Cursor::new(bytes), true),
            Err(WsError::NonZeroRsv)
        );
        // 64-bit length with the top bit set → overflow, not a huge alloc
        let bytes = vec![0x81, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0];
        assert_eq!(
            read_frame(&mut Cursor::new(bytes), true),
            Err(WsError::LengthOverflow)
        );
        // valid 64-bit length over the cap → too large (300_000 > 256_000)
        let bytes = vec![0x81, 0x7f, 0, 0, 0, 0, 0, 0x04, 0x9f, 0x20];
        assert_eq!(
            read_frame(&mut Cursor::new(bytes), true),
            Err(WsError::FrameTooLarge)
        );
        // control frame with len > 125 (ping opcode, masked, 126-byte claim)
        let bytes = encode_frame(OP_PING, &[0u8; 126], true);
        // encode_frame writes 126 as extended length — valid encoding, but
        // control frames may not exceed 125
        assert_eq!(
            read_frame(&mut Cursor::new(bytes.as_slice()), true),
            Err(WsError::ControlFrameTooLarge)
        );
        // unknown opcode
        let bytes = encode_frame(0x3, b"", true);
        assert_eq!(
            read_frame(&mut Cursor::new(bytes), true),
            Err(WsError::BadOpcode)
        );
        // fragmented control frame
        let mut raw = encode_frame(OP_PING, b"x", true);
        raw[0] &= !0x80; // clear FIN
        assert_eq!(
            read_frame(&mut Cursor::new(raw), true),
            Err(WsError::FragmentedControl)
        );
        // bad utf8 in text frame
        let mut raw = encode_frame(OP_TEXT, &[0xff, 0xfe], true);
        raw[0] = 0x01; // text without FIN is fine at codec level
        assert_eq!(
            read_frame(&mut Cursor::new(raw), true),
            Err(WsError::BadUtf8)
        );
        // truncated frame → io error, no panic
        let bytes = encode_frame(OP_TEXT, b"hello", true);
        assert!(matches!(
            read_frame(&mut Cursor::new(&bytes[..3]), true),
            Err(WsError::Io(_))
        ));
    }

    #[test]
    fn server_frames_are_unmasked() {
        let bytes = encode_frame(OP_TEXT, b"hello", false);
        assert_eq!(bytes[1] & 0x80, 0);
    }

    #[test]
    fn truncation_sweep_never_panics_and_fresh_reader_still_parses() {
        // Cutting a valid frame at any byte yields an IO error (incomplete),
        // never a panic; a fresh reader with the full bytes still decodes —
        // the decoder holds no cross-call state to corrupt.
        let bytes = encode_frame(OP_TEXT, b"hello world", true);
        for cut in 0..bytes.len() {
            let r = read_frame(&mut Cursor::new(&bytes[..cut]), true);
            assert!(matches!(&r, Err(WsError::Io(_))), "cut {cut}: {r:?}");
        }
        assert!(read_frame(&mut Cursor::new(&bytes), true).is_ok());
        // also sweep a 16-bit-length frame (extended header + mask + payload)
        let long = encode_frame(OP_TEXT, &vec![b'x'; 300], true);
        for cut in 0..long.len() {
            let r = read_frame(&mut Cursor::new(&long[..cut]), true);
            assert!(matches!(&r, Err(WsError::Io(_))), "cut {cut}: {r:?}");
        }
        assert!(read_frame(&mut Cursor::new(&long), true).is_ok());
    }

    #[test]
    fn payload_size_boundaries_at_cap_and_length_marker_edges() {
        // exactly at the cap decodes; one past it is rejected before any
        // payload allocation (header-only input proves the check runs first)
        let at_cap = encode_frame(OP_TEXT, &vec![b'x'; MAX_PAYLOAD_BYTES], false);
        assert!(read_frame(&mut Cursor::new(&at_cap), false).is_ok());
        let mut over = vec![0x81u8, 0x7f]; // FIN+text, 64-bit length marker
        over.extend_from_slice(&((MAX_PAYLOAD_BYTES as u64) + 1).to_be_bytes());
        assert_eq!(
            read_frame(&mut Cursor::new(&over), false),
            Err(WsError::FrameTooLarge)
        );
        // 16-bit length marker boundary: 65535 ok, 65536 moves to the 64-bit path
        let at_16 = encode_frame(OP_TEXT, &vec![b'y'; 65_535], false);
        assert!(read_frame(&mut Cursor::new(&at_16), false).is_ok());
        let past_16 = encode_frame(OP_TEXT, &vec![b'y'; 65_536], false);
        assert!(read_frame(&mut Cursor::new(&past_16), false).is_ok());
    }

    #[test]
    fn close_frame_payload_boundaries() {
        let decode = |payload: &[u8]| {
            read_frame(
                &mut Cursor::new(encode_frame(OP_CLOSE, payload, true)),
                true,
            )
        };
        assert_eq!(decode(b""), Ok(Frame::Close(None, String::new())));
        assert_eq!(decode(&[0x03]), Err(WsError::BadClosePayload));
        assert_eq!(
            decode(&[0x03, 0xe8]),
            Ok(Frame::Close(Some(1000), String::new()))
        );
        assert_eq!(decode(&[0x03, 0xe8, 0xff]), Err(WsError::BadUtf8));
    }

    #[test]
    fn unmasked_frames_decode_when_mask_not_required() {
        // client-side reading (client.rs uses mask_required=false): server
        // frames are unmasked; masked frames still decode and unmask correctly
        let unmasked = encode_frame(OP_TEXT, b"from-server", false);
        assert_eq!(
            read_frame(&mut Cursor::new(unmasked), false),
            Ok(Frame::Text(b"from-server".to_vec()))
        );
        let masked = encode_frame(OP_TEXT, b"from-server", true);
        assert_eq!(
            read_frame(&mut Cursor::new(masked), false),
            Ok(Frame::Text(b"from-server".to_vec()))
        );
    }

    #[test]
    fn sequential_frames_decode_from_one_reader() {
        // the decoder consumes exactly one frame's bytes per call: two frames
        // back-to-back decode in order, then EOF errors
        let mut buf = encode_frame(OP_TEXT, b"first", true);
        buf.extend(encode_frame(OP_PING, b"p", true));
        buf.extend(encode_frame(OP_TEXT, b"second", true));
        let mut r = Cursor::new(buf);
        assert_eq!(
            read_frame(&mut r, true).unwrap(),
            Frame::Text(b"first".to_vec())
        );
        assert_eq!(
            read_frame(&mut r, true).unwrap(),
            Frame::Ping(b"p".to_vec())
        );
        assert_eq!(
            read_frame(&mut r, true).unwrap(),
            Frame::Text(b"second".to_vec())
        );
        assert!(matches!(read_frame(&mut r, true), Err(WsError::Io(_))));
    }

    #[test]
    fn handshake_truncation_and_header_boundaries() {
        let full = b"GET /chat HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\r\n";
        // truncated before the final CRLFCRLF is an IO error, never a panic
        for cut in 0..full.len() {
            let r = read_handshake(&mut Cursor::new(&full[..cut]));
            assert!(matches!(&r, Err(WsError::Io(_))), "cut {cut}: {r:?}");
        }
        assert!(read_handshake(&mut Cursor::new(&full[..])).is_ok());
        // a header line without a colon is rejected
        let bad = b"GET / HTTP/1.1\r\noops\r\n\r\n";
        assert!(matches!(
            read_handshake(&mut Cursor::new(&bad[..])),
            Err(WsError::BadHandshake)
        ));
        // header-count boundary: 64 accepted, 65 rejected
        let mut many = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..64 {
            many.extend_from_slice(format!("x-{i}: v\r\n").as_bytes());
        }
        many.extend_from_slice(b"\r\n");
        assert!(read_handshake(&mut Cursor::new(&many)).is_ok());
        many = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..65 {
            many.extend_from_slice(format!("x-{i}: v\r\n").as_bytes());
        }
        many.extend_from_slice(b"\r\n");
        assert!(matches!(
            read_handshake(&mut Cursor::new(&many)),
            Err(WsError::BadHandshake)
        ));
        // non-UTF8 head bytes are rejected
        let mut raw = b"GET / HTTP/1.1\r\nX: ".to_vec();
        raw.push(0xff);
        raw.extend_from_slice(b"\r\n\r\n");
        assert!(matches!(
            read_handshake(&mut Cursor::new(&raw)),
            Err(WsError::BadHandshake)
        ));
    }
}
