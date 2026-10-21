//! Protocol v2 server — WS + HTTP on one port, exact behavioral parity with
//! `services/host/src/host.ts`, plus the companion's loopback diagnostics.
//!
//! Hardening beyond parity: constant-time token compare, auth-failure
//! cooldown, per-connection read timeouts (slowloris resistance), payload
//! caps in the frame codec, no panics on hostile input (garbage closes the
//! one connection; the server survives).

use crate::companion::Companion;
use crate::json;
use crate::protocol::{ClientMsg, PROTOCOL_VERSION, ServerMsg, parse_client_message};
use crate::ws::{self, Frame, Handshake};
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::Duration;

pub const CLOSE_UNAUTHORIZED: u16 = 4001;
pub const CLOSE_PROTOCOL_MISMATCH: u16 = 4002;

const AUTH_FAILURE_LIMIT: u32 = 5;

/// Constant-time secret comparison (content comparison leaks nothing).
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

fn authorized(h: &Handshake, token: &str) -> bool {
    // `?token=` query (the mobile app's ws URL) or Authorization: Bearer
    if let Some(query) = h.target.split('?').nth(1) {
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("token=")
                && ct_eq(v, token)
            {
                return true;
            }
        }
    }
    if let Some(auth) = h.header("authorization")
        && let Some(v) = auth.strip_prefix("Bearer ")
    {
        return ct_eq(v, token);
    }
    false
}

pub struct ServerHandle {
    pub port: u16,
    pub diagnostics_port: u16,
    stop: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl ServerHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Binds and serves until `stop()` is called. Returns once both listeners
/// are bound (so callers know the real ports).
pub fn start(
    bind: &str,
    port: u16,
    diagnostics_port: u16,
    token: Option<String>,
    companion: Arc<Companion>,
) -> Result<ServerHandle, String> {
    let listener = match crate::ffi::bind_listener_backlog(bind, port) {
        Ok(l) => l,
        Err(e) => {
            // IPv6 binds (and any exotic config) keep std's ~128-handshake backlog
            eprintln!("[companion] large-backlog listener unavailable ({e}); using std bind");
            TcpListener::bind((bind, port)).map_err(|e2| format!("bind {bind}:{port}: {e2}"))?
        }
    };
    let actual_port = listener.local_addr().map_err(|e| e.to_string())?.port();
    // diagnostics is loopback-only by contract (PRD §15.2)
    let diag_listener = TcpListener::bind(("127.0.0.1", diagnostics_port))
        .map_err(|e| format!("bind diagnostics: {e}"))?;
    let actual_diag_port = diag_listener
        .local_addr()
        .map_err(|e| e.to_string())?
        .port();

    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();

    for (listener, token, diagnostics_only) in [
        (
            listener.try_clone().map_err(|e| e.to_string())?,
            token.clone(),
            false,
        ),
        (
            diag_listener.try_clone().map_err(|e| e.to_string())?,
            None,
            true,
        ),
    ] {
        let stop = stop.clone();
        let companion = companion.clone();
        let name = if diagnostics_only {
            "accept-diag"
        } else {
            "accept-main"
        };
        threads.push(
            thread::Builder::new()
                .name(name.into())
                .spawn(move || accept_loop(listener, stop, token, companion, diagnostics_only))
                .map_err(|e| e.to_string())?,
        );
    }
    // engine scheduler: advances mock runs + sweeps permission expiry
    {
        let stop = stop.clone();
        let companion = companion.clone();
        threads.push(
            thread::Builder::new()
                .name("scheduler".into())
                .spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        companion.pump();
                        thread::sleep(Duration::from_millis(50));
                    }
                })
                .map_err(|e| e.to_string())?,
        );
    }

    Ok(ServerHandle {
        port: actual_port,
        diagnostics_port: actual_diag_port,
        stop,
        threads,
    })
}

fn accept_loop(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    token: Option<String>,
    companion: Arc<Companion>,
    diagnostics_only: bool,
) {
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let token = token.clone();
                let companion = companion.clone();
                let _ = thread::Builder::new().name("conn".into()).spawn(move || {
                    let _ = handle_conn(stream, token, companion, diagnostics_only);
                });
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn handle_conn(
    stream: TcpStream,
    token: Option<String>,
    companion: Arc<Companion>,
    diagnostics_only: bool,
) -> io::Result<()> {
    // Windows quirk: sockets returned by accept() on a NONBLOCKING listener
    // inherit nonblocking mode — force blocking or every idle read "fails".
    stream.set_nonblocking(false).ok();
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = stream.try_clone()?;
    let mut writer = stream;

    let handshake = match ws::read_handshake(&mut reader) {
        Ok(h) => h,
        Err(_) => return Ok(()), // hostile/garbage: drop the connection
    };

    // HTTP routes (non-upgrade) -------------------------------------------
    if !handshake.is_websocket_upgrade() {
        return http_route(&mut writer, &handshake, &companion, diagnostics_only);
    }

    // WS upgrade ----------------------------------------------------------
    let Ok(accept) = ws::validate_upgrade(&handshake) else {
        let _ = ws::write_http_error(&mut writer, 400, "Bad Request", "");
        return Ok(());
    };

    // Auth after a valid upgrade request. Parity with the Node host: the
    // socket upgrades, then closes with 4001 when the token is wrong.
    if let Some(expected) = &token {
        let failures = *companion.shared.auth_failures.lock().unwrap();
        let ok_now = authorized(&handshake, expected);
        if !ok_now && failures >= AUTH_FAILURE_LIMIT {
            // brute-force cooldown: refuse fast, do not even upgrade
            let _ = ws::write_http_error(&mut writer, 429, "Too Many Requests", "");
            return Ok(());
        }
        if !ok_now {
            *companion.shared.auth_failures.lock().unwrap() += 1;
            let _ = ws::write_handshake_response(&mut writer, &accept);
            let _ = write_frame(
                &mut writer,
                &ws::encode_frame(
                    ws::OP_CLOSE,
                    &close_payload(CLOSE_UNAUTHORIZED, "unauthorized"),
                    false,
                ),
            );
            return Ok(());
        }
        *companion.shared.auth_failures.lock().unwrap() = 0;
    }
    ws::write_handshake_response(&mut writer, &accept)?;
    serve_ws(reader, writer, companion)
}

fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    let mut p = code.to_be_bytes().to_vec();
    p.extend_from_slice(reason.as_bytes());
    p
}

fn write_frame(w: &mut TcpStream, frame: &[u8]) -> io::Result<()> {
    w.write_all(frame)
}

fn http_route(
    writer: &mut TcpStream,
    h: &Handshake,
    companion: &Companion,
    diagnostics_only: bool,
) -> io::Result<()> {
    let path = h.target.split('?').next().unwrap_or("/");
    let ok_body = |body: String| -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    match (diagnostics_only, path) {
        (_, "/health") => {
            let body = json::stringify(&companion.health());
            writer.write_all(ok_body(body).as_bytes())
        }
        (true, "/version") => {
            let body = json::stringify(&json::obj(vec![
                ("protocol", json::Json::Int(PROTOCOL_VERSION as i64)),
                ("impl", json::s("chernobyl-companion-rs")),
                (
                    "uptimeMs",
                    json::Json::Int(crate::protocol::now_ms() - companion.shared.started_at),
                ),
            ]));
            writer.write_all(ok_body(body).as_bytes())
        }
        (true, "/adapters") => {
            let body = json::stringify(&companion.adapters_json());
            writer.write_all(ok_body(body).as_bytes())
        }
        _ => writer
            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"),
    }
}

fn serve_ws(mut reader: TcpStream, writer: TcpStream, companion: Arc<Companion>) -> io::Result<()> {
    let (tx, rx) = channel::<Vec<u8>>();
    let conn_id = companion
        .shared
        .next_conn_id
        .fetch_add(1, Ordering::Relaxed);
    companion
        .shared
        .clients
        .lock()
        .unwrap()
        .push((conn_id, tx.clone()));

    // writer thread: outbound events/replies
    let writer_stop = Arc::new(AtomicBool::new(false));
    let wstop = writer_stop.clone();
    let mut writer = writer;
    let writer_thread = thread::Builder::new()
        .name("ws-writer".into())
        .spawn(move || {
            while !wstop.load(Ordering::SeqCst) {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(payload) => {
                        let frame = ws::encode_frame(ws::OP_TEXT, &payload, false);
                        if write_frame(&mut writer, &frame).is_err() {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .map_err(|_| io::Error::other("writer spawn"))?;

    // a dedicated handle for control frames (ping/pong/close)
    let mut control = reader.try_clone()?;
    // heartbeat: read timeout = one beat; N misses → dead peer (parity with
    // the Node host's HEARTBEAT_MS cadence and miss limit)
    reader.set_read_timeout(Some(Duration::from_millis(crate::protocol::HEARTBEAT_MS)))?;
    let mut misses: u32 = 0;

    let result: io::Result<()> = loop {
        let frame = match ws::read_frame(&mut reader, true) {
            Ok(f) => f,
            Err(ws::WsError::Io(e))
                if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock =>
            {
                misses += 1;
                if misses > crate::protocol::HEARTBEAT_MISS_LIMIT {
                    break Err(io::Error::other("heartbeat miss limit"));
                }
                let ping = ws::encode_frame(ws::OP_PING, b"", false);
                if write_frame(&mut control, &ping).is_err() {
                    break Err(io::Error::other("ping write failed"));
                }
                continue;
            }
            Err(ws::WsError::Io(e)) => break Err(e),
            // protocol violation from this peer: close the connection only
            Err(_) => break Err(io::Error::other("protocol violation")),
        };
        misses = 0;
        match frame {
            Frame::Ping(p) => {
                let pong = ws::encode_frame(ws::OP_PONG, &p, false);
                if write_frame(&mut control, &pong).is_err() {
                    break Err(io::Error::other("pong write failed"));
                }
            }
            Frame::Pong(_) => {}
            Frame::Close(_, _) => break Ok(()),
            Frame::Text(payload) => {
                let raw = match std::str::from_utf8(&payload) {
                    Ok(r) => r,
                    Err(_) => {
                        send_malformed(&tx, &companion);
                        continue;
                    }
                };
                let Some(msg) = parse_client_message(raw) else {
                    send_malformed(&tx, &companion);
                    continue;
                };
                if let ClientMsg::Hello { protocol, .. } = &msg
                    && *protocol as u32 != PROTOCOL_VERSION
                {
                    // write the mismatch ack synchronously, THEN close —
                    // ordering must be guaranteed before the socket dies
                    let ack = ServerMsg::ack_err(
                        "hello",
                        &format!(
                            "protocol mismatch: client v{protocol}, server v{PROTOCOL_VERSION}"
                        ),
                    )
                    .to_text();
                    let _ = write_frame(
                        &mut control,
                        &ws::encode_frame(ws::OP_TEXT, ack.as_bytes(), false),
                    );
                    let close = ws::encode_frame(
                        ws::OP_CLOSE,
                        &close_payload(CLOSE_PROTOCOL_MISMATCH, "protocol mismatch"),
                        false,
                    );
                    let _ = write_frame(&mut control, &close);
                    break Ok(());
                }
                let reply: Option<ServerMsg> = match &msg {
                    ClientMsg::Hello { .. } => Some(ServerMsg::HelloAck {
                        protocol: PROTOCOL_VERSION,
                        server_time: crate::protocol::now_ms(),
                        snapshot: companion.snapshot(),
                        last_seq: companion.last_seq(),
                    }),
                    _ => companion.handle_client_msg(msg),
                };
                if let Some(reply) = reply {
                    let _ = tx.send(reply.to_text().into_bytes());
                }
            }
            Frame::Binary(_) | Frame::Continuation(_) => {
                // not part of the protocol: refuse politely, stay open
                send_malformed(&tx, &companion);
            }
        }
    };

    // teardown: unregister, stop the writer, drop the socket
    companion
        .shared
        .clients
        .lock()
        .unwrap()
        .retain(|(id, _)| *id != conn_id);
    writer_stop.store(true, Ordering::SeqCst);
    let _ = writer_thread.join();
    // normal client disconnects (FIN/RST) are not worth logging; anything
    // else is a real anomaly worth a line on stderr
    if let Err(e) = &result {
        let normal_disconnect = matches!(
            e.kind(),
            io::ErrorKind::UnexpectedEof
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
        );
        if !normal_disconnect {
            eprintln!("[ws] connection {conn_id} ended: {e}");
        }
    }
    let _ = result;
    Ok(())
}

fn send_malformed(tx: &Sender<Vec<u8>>, companion: &Companion) {
    companion.malformed_total.fetch_add(1, Ordering::Relaxed);
    let _ = tx.send(
        ServerMsg::ack_err("invalid", "malformed message")
            .to_text()
            .into_bytes(),
    );
}
