//! Protocol v2 server — WS + HTTP on one port, exact behavioral parity with
//! `services/host/src/host.ts`, plus the companion's loopback diagnostics.
//!
//! Hardening beyond parity: constant-time token compare, auth-failure
//! cooldown, per-connection read timeouts (slowloris resistance), payload
//! caps in the frame codec, no panics on hostile input (garbage closes the
//! one connection; the server survives).

use crate::base64;
use crate::companion::Companion;
use crate::e2e;
use crate::json;
use crate::protocol::{ClientMsg, PROTOCOL_VERSION, ServerMsg, parse_client_message};
use crate::storage::DeviceRecord;
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

/// Per-connection handshake state: the device that asked to connect and the
/// server's fresh nonce+salt its proof must be bound to (ADR-011).
struct HsPending {
    device_id: String,
    nonce: [u8; 16],
    salt: [u8; 16],
}

fn hs_err(tx: &Sender<Vec<u8>>, reason: &str) {
    let _ = tx.send(
        ServerMsg::HsErr {
            reason: reason.to_string(),
        }
        .to_text()
        .into_bytes(),
    );
}

/// Recover a stored PSK from its DPAPI-protected base64 form. Fails closed.
fn unprotect_psk(b64: &str) -> Result<[u8; 32], String> {
    let cipher = base64::decode(b64).ok_or("bad psk encoding")?;
    crate::ffi::dpapi_unprotect(&cipher)
        .map_err(|e| format!("psk decrypt: {e}"))?
        .try_into()
        .map_err(|_| "psk must be 32 bytes".to_string())
}

/// Resolve + verify a handshake and, for a fresh pairing, redeem it (single-use
/// consume + device registration). Returns the live session and the server's
/// mutual-auth proof. Rejects expired / spent / revoked / unknown devices and
/// any proof that is not a valid HMAC under the device's PSK.
fn establish_session(
    companion: &Companion,
    pending: &HsPending,
    client_proof: &str,
) -> Result<(e2e::Session, String), String> {
    let proof = base64::decode(client_proof).ok_or("bad proof encoding")?;
    let mut store = companion.shared.storage.lock().unwrap();
    let now = crate::protocol::now_ms();
    let pairing = store.pairings.get(&pending.device_id).cloned();
    let device = store.devices.get(&pending.device_id).cloned();

    let verify = |psk: &[u8; 32]| -> Result<(), String> {
        let expected = e2e::handshake_proof(
            psk,
            "client",
            &pending.device_id,
            &pending.nonce,
            &pending.salt,
        );
        if e2e::proof_ok(&expected, &proof) {
            Ok(())
        } else {
            Err("proof verification failed".to_string())
        }
    };

    let (psk, redeem) = match (pairing, device) {
        // fresh, unexpired offer: first connection (redeemed below)
        (Some(p), _) if !p.consumed && now < p.expires_at => {
            let psk = unprotect_psk(&p.psk_protected)?;
            verify(&psk)?;
            (psk, Some(p))
        }
        // already-paired device, or an offer whose redemption already ran
        (_, Some(d)) => {
            if d.revoked {
                return Err("device revoked".to_string());
            }
            let psk = unprotect_psk(&d.psk_protected)?;
            verify(&psk)?;
            (psk, None)
        }
        (Some(p), None) if p.consumed => return Err("pairing offer already used".to_string()),
        (Some(p), None) if now >= p.expires_at => return Err("pairing offer expired".to_string()),
        _ => return Err("unknown device".to_string()),
    };

    if let Some(mut p) = redeem {
        p.consumed = true;
        store
            .put_pairing(&p)
            .map_err(|e| format!("redeem pairing: {e}"))?;
        store
            .put_device(&DeviceRecord {
                device_id: pending.device_id.clone(),
                name: pending.device_id.clone(),
                psk_protected: p.psk_protected.clone(),
                paired_at: now,
                revoked: false,
            })
            .map_err(|e| format!("register device: {e}"))?;
    }

    let keys = e2e::derive_keys(&psk, &pending.salt);
    let server_proof = e2e::handshake_proof(
        &psk,
        "server",
        &pending.device_id,
        &pending.nonce,
        &pending.salt,
    );
    Ok((e2e::Session::host(keys), base64::encode(&server_proof)))
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
    // E2E session (ADR-011): `e2e` is live once a handshake succeeds; `hs`
    // holds the in-flight handshake between hs_hello and hs_proof.
    let mut e2e: Option<e2e::Session> = None;
    let mut hs: Option<HsPending> = None;

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
                // When an E2E session is live the frame is sealed: open it first
                // and tear the connection down on replay / desync / tamper.
                let plaintext: String = if let Some(session) = e2e.as_mut() {
                    let p = match std::str::from_utf8(&payload) {
                        Ok(s) => s.to_string(),
                        Err(_) => break Err(io::Error::other("e2e: non-utf8 sealed frame")),
                    };
                    match session.open(&p) {
                        Ok(t) => t,
                        Err(reason) => break Err(io::Error::other(format!("e2e: {reason}"))),
                    }
                } else {
                    match String::from_utf8(payload) {
                        Ok(s) => s,
                        Err(_) => {
                            send_malformed(&tx, &companion);
                            continue;
                        }
                    }
                };
                let Some(msg) = parse_client_message(&plaintext) else {
                    send_malformed(&tx, &companion);
                    continue;
                };
                // E2E handshake control (plaintext phase only, before a session)
                if e2e.is_none() {
                    match &msg {
                        ClientMsg::HsHello { device_id } => {
                            let (nonce, salt) = e2e::fresh_nonce_salt();
                            hs = Some(HsPending {
                                device_id: device_id.clone(),
                                nonce,
                                salt,
                            });
                            let reply = ServerMsg::HsChallenge {
                                nonce: base64::encode(&nonce),
                                salt: base64::encode(&salt),
                            };
                            let _ = tx.send(reply.to_text().into_bytes());
                            continue;
                        }
                        ClientMsg::HsProof { proof } => {
                            let Some(pending) = hs.take() else {
                                hs_err(&tx, "no handshake in progress");
                                continue;
                            };
                            match establish_session(&companion, &pending, proof) {
                                Ok((session, server_proof)) => {
                                    // HsOk is plaintext; both sides switch to
                                    // sealed frames from the next message on.
                                    let _ = tx.send(
                                        ServerMsg::HsOk { server_proof }.to_text().into_bytes(),
                                    );
                                    e2e = Some(session);
                                }
                                Err(reason) => hs_err(&tx, &reason),
                            }
                            continue;
                        }
                        _ => {}
                    }
                }
                // strict integer equality, matching host.ts `!==`: the `as u32`
                // cast truncates (2.5 → 2), so the fractional check must run first
                if let ClientMsg::Hello { protocol, .. } = &msg
                    && (protocol.fract() != 0.0 || *protocol as u32 != PROTOCOL_VERSION)
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
                    let text = reply.to_text();
                    let out = match e2e.as_mut() {
                        Some(session) => session.seal(&text).into_bytes(),
                        None => text.into_bytes(),
                    };
                    let _ = tx.send(out);
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

/* ------------------------------------------------------------------ */
/* E2E security matrix (ADR-011) — server-level integration tests.    */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::client::WsClient;
    use crate::companion::Companion;
    use crate::config::default_config;
    use crate::protocol::now_ms;
    use crate::storage::{DeviceRecord, PairingRecord};

    struct Fixture {
        companion: Arc<Companion>,
        handle: ServerHandle,
        psk: [u8; 32],
        device_id: String,
        data_dir: std::path::PathBuf,
    }

    fn shutdown(f: Fixture) {
        let Fixture {
            companion,
            handle,
            data_dir,
            ..
        } = f;
        handle.stop();
        companion.shutdown();
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chernobyl-e2e-{}-{}-{}",
            tag,
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Boot a companion + server (loopback, no token) and seed a pending
    /// pairing offer with a known PSK.
    fn fixture(tag: &str, expires_at: i64, consumed: bool) -> Fixture {
        let dir = temp_dir(tag);
        let mut c = default_config();
        c.data_dir = dir.clone();
        let companion = Arc::new(Companion::boot(c).unwrap());
        let psk = [42u8; 32];
        let device_id = "dev-e2e".to_string();
        let protected = base64::encode(&crate::ffi::dpapi_protect(&psk).unwrap());
        {
            let mut store = companion.shared.storage.lock().unwrap();
            store
                .put_pairing(&PairingRecord {
                    device_id: device_id.clone(),
                    psk_protected: protected,
                    challenge: base64::encode(b"CHALLENGE"),
                    created_at: now_ms(),
                    expires_at,
                    consumed,
                })
                .unwrap();
        }
        let handle = start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
        Fixture {
            companion,
            handle,
            psk,
            device_id,
            data_dir: dir,
        }
    }

    /// Boot + seed an already-paired device registry entry (no pending offer).
    fn fixture_paired(tag: &str, revoked: bool) -> Fixture {
        let dir = temp_dir(tag);
        let mut c = default_config();
        c.data_dir = dir.clone();
        let companion = Arc::new(Companion::boot(c).unwrap());
        let psk = [42u8; 32];
        let device_id = "dev-paired".to_string();
        let protected = base64::encode(&crate::ffi::dpapi_protect(&psk).unwrap());
        {
            let mut store = companion.shared.storage.lock().unwrap();
            store
                .put_device(&DeviceRecord {
                    device_id: device_id.clone(),
                    name: "phone".into(),
                    psk_protected: protected,
                    paired_at: now_ms(),
                    revoked,
                })
                .unwrap();
        }
        let handle = start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
        Fixture {
            companion,
            handle,
            psk,
            device_id,
            data_dir: dir,
        }
    }

    fn connect(f: &Fixture) -> WsClient {
        WsClient::connect(&format!("127.0.0.1:{}", f.handle.port), "/", None).unwrap()
    }

    /// Drive one full handshake attempt with `psk`. Returns the server's final
    /// `hs_ok`/`hs_err` payload and, on success, the client-side session.
    fn attempt(
        c: &mut WsClient,
        psk: &[u8; 32],
        device_id: &str,
    ) -> (String, Option<e2e::Session>) {
        c.send_text(&format!(
            "{{\"type\":\"hs_hello\",\"deviceId\":\"{device_id}\"}}"
        ))
        .unwrap();
        let challenge = c.next_text().unwrap();
        let v = json::parse(&challenge).unwrap();
        let nonce = base64::decode(json::get_str(&v, "nonce").unwrap()).unwrap();
        let salt = base64::decode(json::get_str(&v, "salt").unwrap()).unwrap();
        let proof = e2e::handshake_proof(psk, "client", device_id, &nonce, &salt);
        c.send_text(&format!(
            "{{\"type\":\"hs_proof\",\"proof\":\"{}\"}}",
            base64::encode(&proof)
        ))
        .unwrap();
        let resp = c.next_text().unwrap();
        let v = json::parse(&resp).unwrap();
        if json::get_str(&v, "type") == Some("hs_ok") {
            // mutual auth: the server must prove PSK knowledge too
            let sp = base64::decode(json::get_str(&v, "serverProof").unwrap()).unwrap();
            let expected = e2e::handshake_proof(psk, "server", device_id, &nonce, &salt);
            assert_eq!(sp.as_slice(), expected.as_slice(), "server must prove PSK");
            let keys = e2e::derive_keys(psk, &salt);
            (resp, Some(e2e::Session::client(keys)))
        } else {
            (resp, None)
        }
    }

    #[test]
    fn happy_path_seals_and_unseals_frames() {
        let f = fixture("happy", now_ms() + 60_000, false);
        let mut c = connect(&f);
        let (resp, sess) = attempt(&mut c, &f.psk, &f.device_id);
        assert!(resp.contains("hs_ok"), "got: {resp}");
        let mut sess = sess.unwrap();
        // sealed ping → sealed pong
        let ping = "{\"type\":\"ping\",\"commandId\":\"p1\"}";
        c.send_text(&sess.seal(ping)).unwrap();
        let pong = sess.open(&c.next_text().unwrap()).unwrap();
        assert!(pong.contains("pong"), "got: {pong}");
        // sealed hello → sealed hello_ack (snapshot round-trips)
        let hello = format!(
            "{{\"type\":\"hello\",\"clientId\":\"c1\",\"protocol\":{}}}",
            PROTOCOL_VERSION
        );
        c.send_text(&sess.seal(&hello)).unwrap();
        let ack = sess.open(&c.next_text().unwrap()).unwrap();
        assert!(ack.contains("protocol"), "got: {ack}");
        shutdown(f);
    }

    #[test]
    fn replayed_frame_tears_down_connection() {
        let f = fixture("replay", now_ms() + 60_000, false);
        let mut c = connect(&f);
        let (_resp, sess) = attempt(&mut c, &f.psk, &f.device_id);
        let mut sess = sess.unwrap();
        let ping = "{\"type\":\"ping\",\"commandId\":\"p1\"}";
        let frame = sess.seal(ping); // counter 0
        c.send_text(&frame).unwrap();
        let _ = sess.open(&c.next_text().unwrap()).unwrap(); // pong
        // resend the identical frame: server sees counter 0 again (already at 1)
        c.send_text(&frame).unwrap();
        assert!(
            c.next_text().is_err(),
            "server must close the connection on frame replay"
        );
        shutdown(f);
    }

    #[test]
    fn tampered_frame_tears_down_connection() {
        let f = fixture("tamper", now_ms() + 60_000, false);
        let mut c = connect(&f);
        let (_resp, sess) = attempt(&mut c, &f.psk, &f.device_id);
        let mut sess = sess.unwrap();
        let frame = sess.seal("{\"type\":\"ping\",\"commandId\":\"p1\"}");
        // flip one body byte (counter stays 0) ⇒ AEAD tag fails server-side
        let mut bad = frame.clone();
        let idx = bad.rfind('\"').unwrap() - 1;
        let b = bad.as_bytes()[idx];
        bad.replace_range(idx..idx + 1, if b == b'A' { "B" } else { "A" });
        c.send_text(&bad).unwrap();
        assert!(
            c.next_text().is_err(),
            "server must close the connection on a tampered frame"
        );
        shutdown(f);
    }

    /* D7: protocol-version gate — strict integer equality, no f64 truncation. */

    #[test]
    fn hello_gate_rejects_fractional_and_wrong_versions_with_4002() {
        let f = fixture("d7-gate", now_ms() + 60_000, false);
        // 2.5 used to pass via `as u32` truncation; 1 = older, 3 = newer
        for bad in ["2.5", "1", "3"] {
            let mut c = connect(&f);
            c.send_text(&format!(
                r#"{{"type":"hello","clientId":"d7","protocol":{bad}}}"#
            ))
            .unwrap();
            let ack = c.next_text().unwrap();
            assert!(
                ack.contains("protocol mismatch"),
                "v{bad} must be rejected, got: {ack}"
            );
            match c.read_frame().unwrap() {
                Frame::Close(Some(code), reason) => {
                    assert_eq!(code, CLOSE_PROTOCOL_MISMATCH, "v{bad}");
                    assert!(reason.contains("protocol mismatch"), "v{bad}: {reason}");
                }
                other => panic!("v{bad}: expected close 4002, got {other:?}"),
            }
        }
        shutdown(f);
    }

    #[test]
    fn hello_gate_accepts_the_exact_supported_version() {
        let f = fixture("d7-accept", now_ms() + 60_000, false);
        let mut c = connect(&f);
        c.send_text(&format!(
            r#"{{"type":"hello","clientId":"d7","protocol":{}}}"#,
            PROTOCOL_VERSION
        ))
        .unwrap();
        let ack = c.next_text().unwrap();
        assert!(ack.contains("hello_ack"), "got: {ack}");
        // the connection must stay open for the session
        c.send_text(r#"{"type":"ping","commandId":"d7-ping"}"#)
            .unwrap();
        assert!(c.next_text().unwrap().contains("pong"));
        shutdown(f);
    }

    #[test]
    fn bad_proof_is_rejected() {
        let f = fixture("badproof", now_ms() + 60_000, false);
        let mut c = connect(&f);
        let wrong_psk = [99u8; 32];
        let (resp, sess) = attempt(&mut c, &wrong_psk, &f.device_id);
        assert!(sess.is_none());
        assert!(resp.contains("proof verification failed"), "got: {resp}");
        shutdown(f);
    }

    #[test]
    fn unknown_device_is_rejected() {
        let f = fixture("unknown", now_ms() + 60_000, false);
        let mut c = connect(&f);
        let (resp, sess) = attempt(&mut c, &f.psk, "no-such-device");
        assert!(sess.is_none());
        assert!(resp.contains("unknown device"), "got: {resp}");
        shutdown(f);
    }

    #[test]
    fn expired_pairing_is_rejected() {
        let f = fixture("expired", now_ms() - 1_000, false); // already expired
        let mut c = connect(&f);
        let (resp, sess) = attempt(&mut c, &f.psk, &f.device_id);
        assert!(sess.is_none());
        assert!(resp.contains("expired"), "got: {resp}");
        shutdown(f);
    }

    #[test]
    fn spent_pairing_cannot_be_reused() {
        let f = fixture("spent", now_ms() + 60_000, true); // already redeemed
        let mut c = connect(&f);
        let (resp, sess) = attempt(&mut c, &f.psk, &f.device_id);
        assert!(sess.is_none());
        assert!(resp.contains("already used"), "got: {resp}");
        shutdown(f);
    }

    #[test]
    fn revoked_device_is_rejected() {
        let f = fixture_paired("revoked", true);
        let mut c = connect(&f);
        let (resp, sess) = attempt(&mut c, &f.psk, &f.device_id);
        assert!(sess.is_none());
        assert!(resp.contains("revoked"), "got: {resp}");
        shutdown(f);
    }

    #[test]
    fn paired_device_can_reconnect_after_redemption() {
        // full lifecycle: redeem a fresh offer (registers the device), then
        // reconnect using the device registry with the same PSK.
        let f = fixture("lifecycle", now_ms() + 60_000, false);
        let mut c1 = connect(&f);
        let (r1, _s1) = attempt(&mut c1, &f.psk, &f.device_id);
        assert!(r1.contains("hs_ok"), "first connect must redeem: {r1}");
        // the offer is now spent AND a device record exists → reconnect works
        let mut c2 = connect(&f);
        let (r2, s2) = attempt(&mut c2, &f.psk, &f.device_id);
        assert!(r2.contains("hs_ok"), "paired device must reconnect: {r2}");
        assert!(s2.is_some());
        shutdown(f);
    }

    #[test]
    fn revoking_a_live_device_blocks_reconnect() {
        // a device works, then revocation (the same put_device persistence the
        // `companion revoke` CLI uses) cuts it off at the next handshake.
        let f = fixture_paired("revoke-live", false);
        let mut c1 = connect(&f);
        let (r1, _s1) = attempt(&mut c1, &f.psk, &f.device_id);
        assert!(r1.contains("hs_ok"), "unrevoked device must connect: {r1}");
        // revoke it
        {
            let mut store = f.companion.shared.storage.lock().unwrap();
            let mut d = store.devices.get(&f.device_id).cloned().unwrap();
            d.revoked = true;
            store.put_device(&d).unwrap();
        }
        let mut c2 = connect(&f);
        let (r2, s2) = attempt(&mut c2, &f.psk, &f.device_id);
        assert!(s2.is_none(), "revoked device must not reconnect");
        assert!(r2.contains("revoked"), "got: {r2}");
        shutdown(f);
    }

    #[test]
    fn malformed_message_is_acked_and_connection_survives() {
        // message-level garbage is an ack + continue (send_malformed); only
        // frame-level violations tear the connection down
        let f = fixture("malformed-msg", now_ms() + 60_000, false);
        let mut c = connect(&f);
        c.send_text(r#"{"bogus":1}"#).unwrap();
        let reply = c.next_text().unwrap();
        assert!(reply.contains("malformed message"), "got: {reply}");
        // a binary frame is also message-level: acked, not fatal
        c.send_raw(&ws::encode_frame(ws::OP_BINARY, b"\x00\x01", true))
            .unwrap();
        let reply = c.next_text().unwrap();
        assert!(reply.contains("malformed message"), "got: {reply}");
        // the same connection still completes a full handshake
        let (resp, sess) = attempt(&mut c, &f.psk, &f.device_id);
        assert!(
            resp.contains("hs_ok"),
            "connection must survive malformed input: {resp}"
        );
        assert!(sess.is_some());
        shutdown(f);
    }

    #[test]
    fn hs_proof_before_hello_is_rejected_but_connection_survives() {
        // out-of-order handshake is an hs_err + continue, not a teardown
        let f = fixture("hs-order", now_ms() + 60_000, false);
        let mut c = connect(&f);
        c.send_text(r#"{"type":"hs_proof","proof":"AAAA"}"#)
            .unwrap();
        let reply = c.next_text().unwrap();
        assert!(reply.contains("no handshake in progress"), "got: {reply}");
        // a proper handshake on the same connection still succeeds
        let (resp, sess) = attempt(&mut c, &f.psk, &f.device_id);
        assert!(resp.contains("hs_ok"), "got: {resp}");
        assert!(sess.is_some());
        shutdown(f);
    }
}
