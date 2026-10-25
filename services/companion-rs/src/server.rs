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
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const CLOSE_UNAUTHORIZED: u16 = 4001;
pub const CLOSE_PROTOCOL_MISMATCH: u16 = 4002;

const AUTH_FAILURE_LIMIT: u32 = 5;

// Per-message-class outbound bounds (D8 + mandate §4): ONE FIFO, TWO admission
// policies, so a bulk event burst can never consume the room a control frame
// (reply, ack, refusal, close) needs. Typical event/ack frames are a few KB
// (SessionOutput emits one line per event; hello snapshots are a few KB), while
// a single frame may legally reach the 256 000-B codec cap (ws.rs). The count
// bound caps per-message bookkeeping for tiny frames; the byte bound caps
// payload memory at 16x one max-size frame — a writer that cannot drain 4 MiB
// while the peer still drives the heartbeat is stalled. The write timeout
// bounds every write_frame (and every control-frame write: `control` shares
// the socket's options), so the writer thread and the teardown join() can
// never block forever; 10 s is generous for loopback/LAN and far inside the
// 3-beat (45 s) dead-peer window.
const OUTBOUND_QUEUE_MAX_MESSAGES: usize = 256;
const OUTBOUND_QUEUE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Bulk (event fan-out) is admitted only while its own depth and a 3 MiB
/// shared byte budget hold: the last 64 queue slots and the last 1 MiB stay
/// reserved for control frames. Overflowing either bound refuses admission —
/// the connection tears down (never a silent drop; a shed event would be
/// invisible loss because no client consumes `lastSeq` for gap detection).
const BULK_QUEUE_MAX_MESSAGES: usize = 192;
const BULK_QUEUE_MAX_BYTES: usize = 3 * 1024 * 1024;
/// Channel capacity = both class caps summed: control stays admissible up to
/// its own 256 even when bulk already holds its full 192 (reserved headroom).
const CHANNEL_CAPACITY: usize = OUTBOUND_QUEUE_MAX_MESSAGES + BULK_QUEUE_MAX_MESSAGES;
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Which admission policy a frame falls under (mandate §4: bounds and
/// overflow policy are per message class, not one global limit).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MsgClass {
    /// Replies, acks, refusals, handshake and side-channel frames — the
    /// frames that keep a session honest; never shed because of bulk depth.
    Control,
    /// Engine event fan-out (session output, activity) — the flood source.
    Bulk,
}

impl MsgClass {
    fn max_messages(self) -> usize {
        match self {
            MsgClass::Control => OUTBOUND_QUEUE_MAX_MESSAGES,
            MsgClass::Bulk => BULK_QUEUE_MAX_MESSAGES,
        }
    }
    /// Byte budget checked against the shared `queued_bytes` counter: bulk
    /// sees the whole queue, so its 3 MiB cap also reserves 1 MiB for control.
    fn max_bytes(self) -> usize {
        match self {
            MsgClass::Control => OUTBOUND_QUEUE_MAX_BYTES,
            MsgClass::Bulk => BULK_QUEUE_MAX_BYTES,
        }
    }
}

/// In-queue depth per class for one connection, shared with the writer
/// thread, which releases a slot as each payload leaves the queue.
#[derive(Default)]
pub struct Depth {
    bulk: AtomicUsize,
    ctrl: AtomicUsize,
}

impl Depth {
    fn get(&self, class: MsgClass) -> usize {
        match class {
            MsgClass::Control => self.ctrl.load(Ordering::SeqCst),
            MsgClass::Bulk => self.bulk.load(Ordering::SeqCst),
        }
    }
    fn add(&self, class: MsgClass) {
        match class {
            MsgClass::Control => self.ctrl.fetch_add(1, Ordering::SeqCst),
            MsgClass::Bulk => self.bulk.fetch_add(1, Ordering::SeqCst),
        };
    }
    fn release(&self, class: MsgClass) {
        match class {
            MsgClass::Control => self.ctrl.fetch_sub(1, Ordering::SeqCst),
            MsgClass::Bulk => self.bulk.fetch_sub(1, Ordering::SeqCst),
        };
    }
}

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
    // Remote posture (D2/D6): binding beyond loopback hardens the server —
    // same predicate as main.rs's "non-loopback requires a token" check.
    let remote_posture = bind != "127.0.0.1" && bind != "localhost";
    start_with_posture(
        bind,
        port,
        diagnostics_port,
        token,
        companion,
        remote_posture,
    )
}

/// Same as [`start`] with the remote-posture flag set explicitly, so tests
/// can exercise the non-loopback policies while still binding loopback.
pub fn start_with_posture(
    bind: &str,
    port: u16,
    diagnostics_port: u16,
    token: Option<String>,
    companion: Arc<Companion>,
    remote_posture: bool,
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
                .spawn(move || {
                    accept_loop(
                        listener,
                        stop,
                        token,
                        companion,
                        diagnostics_only,
                        remote_posture,
                    )
                })
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
    remote_posture: bool,
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
                    let _ = handle_conn(stream, token, companion, diagnostics_only, remote_posture);
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
    remote_posture: bool,
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
        // Phase 7: POST hook ingress (Claude Code) — reachable ONLY on the
        // loopback diagnostics listener (PRD §15.2). The main listener never
        // answers a POST: its auth/session surface is untouched. RFC 6455
        // upgrades are GET-only (`validate_upgrade`), so a POST here is always
        // a plain HTTP request.
        if handshake.method == "POST" {
            let path = handshake.target.split('?').next().unwrap_or("/");
            if !diagnostics_only || path != "/hooks/claude" {
                // Drain the (capped) body before refusing so the close is a
                // graceful FIN, not a RST racing the client's read of our 404
                // (Windows resets a socket closed with an unread body).
                drain_body(&mut reader, &handshake);
                let _ = ws::write_http_error(&mut writer, 404, "Not Found", "");
                return Ok(());
            }
            return hook_ingress(&mut reader, &mut writer, &handshake, &companion);
        }
        // D6: on a non-loopback main listener /health joins the token gate.
        let require_health_auth = !diagnostics_only && remote_posture && token.is_some();
        return http_route(
            &mut writer,
            &handshake,
            &companion,
            diagnostics_only,
            token.as_deref(),
            require_health_auth,
        );
    }

    // WS upgrade ----------------------------------------------------------
    // D1: browsers are unsupported until the web app ships (ADR-011 §5).
    // An Origin header marks a browser-initiated upgrade, which is refused
    // outright — this closes cross-site WebSocket hijacking in the default
    // config (loopback, token-less, plaintext commands). Non-browser clients
    // (phone app, CLI, test tooling) send no Origin, so nothing else is
    // affected. When the web app ships this becomes an Origin allow-list.
    if handshake.header("origin").is_some() {
        let _ = ws::write_http_error(&mut writer, 403, "Forbidden", "");
        return Ok(());
    }

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
    serve_ws(reader, writer, companion, remote_posture)
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
    token: Option<&str>,
    require_health_auth: bool,
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
            // D6: beyond loopback the counters are operator-only — same
            // posture as the WS token gate (loopback needs no token).
            // The diagnostics listener (loopback-bound by contract) stays open.
            if require_health_auth && !token.is_some_and(|t| authorized(h, t)) {
                return writer.write_all(
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                );
            }
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
        // Phase 8: local metrics — loopback diagnostics only, same posture as
        // /version and /adapters (never exposed on the main listener).
        (true, "/metrics") => {
            let body = companion.metrics_text();
            writer.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain; version=0.0.4; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
        }
        _ => writer
            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"),
    }
}

/// Best-effort drain of a POST body we are about to refuse, so the connection
/// closes with a graceful FIN rather than a RST (Windows resets a socket that
/// is closed while a request body is still unread in the receive buffer, which
/// can race the client's read of our refusal). Bounded and non-fatal: if the
/// peer lies about Content-Length we simply stop reading.
fn drain_body(reader: &mut TcpStream, h: &Handshake) {
    let n = h
        .header("content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
        .min(ws::MAX_HOOK_BODY_BYTES);
    if n == 0 {
        return;
    }
    let mut sink = vec![0u8; n];
    let _ = reader.read_exact(&mut sink);
}

/// Phase 7: read one Claude Code hook POST body (Content-Length framed, capped
/// at `MAX_HOOK_BODY_BYTES`) and route it into the engine pipeline. Returns the
/// adapter's verdict as an HTTP status: 200 accepted, 400 malformed, 507 when
/// no Claude adapter is mounted. The body read fails closed on a missing or
/// oversized Content-Length.
fn hook_ingress(
    reader: &mut TcpStream,
    writer: &mut TcpStream,
    h: &Handshake,
    companion: &Companion,
) -> io::Result<()> {
    let clen = h
        .header("content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n <= ws::MAX_HOOK_BODY_BYTES);
    let Some(clen) = clen else {
        // no body, non-numeric, or oversize: refuse without reading a byte
        return writer.write_all(
            b"HTTP/1.1 411 Length Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
    };
    let mut body = vec![0u8; clen];
    if reader.read_exact(&mut body).is_err() {
        return writer.write_all(
            b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
    }
    let payload = String::from_utf8(body)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "hook body was not utf-8"))?;
    let status = companion.ingest_claude_hook(&payload);
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Service Unavailable", // 404: no Claude adapter mounted
    };
    let resp =
        format!("HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    writer.write_all(resp.as_bytes())
}

/// Per-connection handshake state: the device that asked to connect and the
/// server's fresh nonce+salt its proof must be bound to (ADR-011).
struct HsPending {
    device_id: String,
    nonce: [u8; 16],
    salt: [u8; 16],
}

fn hs_err(outbound: &Outbound, reason: &str) -> Result<(), EnqueueError> {
    outbound.enqueue(
        ServerMsg::HsErr {
            reason: reason.to_string(),
        }
        .to_text()
        .into_bytes(),
        MsgClass::Control,
    )
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

/// Per-connection outbound queue (D8): bounded by message count AND total
/// payload bytes — payload sizes vary by orders of magnitude, so a
/// count-only bound still allows unbounded memory. Saturation is never a
/// silent drop: `enqueue` fails and marks the connection terminated, and the
/// read loop tears the socket down; the client reconnects and resyncs via a
/// fresh hello snapshot. Every producer uses non-blocking `enqueue`, so a
/// stalled peer can never park the engine, the registry lock, or the read
/// loop — no deadlock is possible between producers and the writer.
pub struct Outbound {
    tx: SyncSender<(MsgClass, Vec<u8>)>,
    /// bytes reserved by queued payloads (not yet dequeued by the writer);
    /// shared with the writer thread so it can release on dequeue
    queued_bytes: Arc<AtomicUsize>,
    /// per-class in-queue depth; released by the writer on dequeue
    depth: Arc<Depth>,
    /// set on saturation/disconnect: the read loop must tear this connection down
    terminated: AtomicBool,
    /// The live E2E session for this connection (ADR-011). Shared with the
    /// read loop (which installs it on `hs_ok`) and with the engine fan-out
    /// (which seals events per connection) so that EVERY outbound frame —
    /// reply, event, or side-channel — is sealed once a session exists, as
    /// the wire contract requires ("from `hs_ok` on every frame is sealed").
    /// Seal-then-enqueue happens under this lock so the AEAD counter order
    /// always matches the FIFO wire order.
    session: Mutex<Option<e2e::Session>>,
    /// Process-wide saturation counters (mandate §4): shared so /health and
    /// /metrics describe overload on any connection, including one already gone.
    saturation: Arc<crate::companion::Saturation>,
}

/// Why an outbound message could not be queued. Both variants end the
/// connection — nothing is ever silently discarded.
#[derive(Debug)]
pub enum EnqueueError {
    /// count/byte bound hit, or the connection is already being torn down
    Full,
    /// writer thread is gone (socket write failed or timed out)
    Disconnected,
}

impl Outbound {
    fn new(
        saturation: Arc<crate::companion::Saturation>,
    ) -> (Arc<Outbound>, Receiver<(MsgClass, Vec<u8>)>) {
        let (tx, rx) = sync_channel(CHANNEL_CAPACITY);
        (
            Arc::new(Outbound {
                tx,
                queued_bytes: Arc::new(AtomicUsize::new(0)),
                depth: Arc::new(Depth::default()),
                terminated: AtomicBool::new(false),
                session: Mutex::new(None),
                saturation,
            }),
            rx,
        )
    }

    /// Install the live E2E session (called by the read loop on `hs_ok`).
    /// From this point every frame this connection sends is sealed.
    pub(crate) fn set_session(&self, session: e2e::Session) {
        *self.session.lock().unwrap() = Some(session);
    }

    /// True once an E2E session is live for this connection.
    pub(crate) fn has_session(&self) -> bool {
        self.session.lock().unwrap().is_some()
    }

    /// Queue one plaintext for this connection, sealing it first when a
    /// session is live. This is the single outbound sealing point the wire
    /// contract requires: replies, events, and side-channel frames all pass
    /// through here (or `enqueue` for the pre-session plaintext phase).
    /// Seal-then-enqueue is done under the session lock so the AEAD counter
    /// order always matches the FIFO wire order — the client's strict
    /// monotonic `open` would otherwise see a reordering as replay/desync.
    /// With no session the plaintext is enqueued verbatim (loopback parity).
    pub(crate) fn enqueue_sealed(
        &self,
        plaintext: Vec<u8>,
        class: MsgClass,
    ) -> Result<(), EnqueueError> {
        // Admission BEFORE sealing (mandate §4): a frame that cannot be
        // queued must never consume an AEAD counter, so the terminated latch
        // and the class depth bound are checked while nothing has been spent.
        // The byte bound is checked in `enqueue` under this same lock; if it
        // fails there the connection terminates, so a spent counter can never
        // be followed by further frames on a live session (no desync).
        if self.terminated.load(Ordering::SeqCst) || self.depth.get(class) >= class.max_messages() {
            return Err(self.refuse(class));
        }
        let mut guard = self.session.lock().unwrap();
        let payload = match guard.as_mut() {
            Some(session) => {
                // plaintext is always UTF-8 JSON from `to_text()`; seal it.
                let text = String::from_utf8_lossy(&plaintext);
                session.seal(&text).into_bytes()
            }
            None => plaintext,
        };
        // enqueue under the same lock so no other producer can interleave a
        // frame between this seal and its position on the wire
        self.enqueue(payload, class)
    }

    /// Refuse a frame: count it against its class and, on the first refusal
    /// on this connection, latch termination + the saturation-teardown
    /// counter. Refusal always means teardown — events are never dropped
    /// silently (a shed event would be invisible loss: no client consumes
    /// `lastSeq` for gap detection, so reconnect+resync is the recovery).
    fn refuse(&self, class: MsgClass) -> EnqueueError {
        if !self.terminated.swap(true, Ordering::SeqCst) {
            match class {
                MsgClass::Control => {
                    self.saturation.shed_ctrl.fetch_add(1, Ordering::SeqCst);
                }
                MsgClass::Bulk => {
                    self.saturation.shed_bulk.fetch_add(1, Ordering::SeqCst);
                }
            }
            self.saturation.teardowns.fetch_add(1, Ordering::SeqCst);
        }
        EnqueueError::Full
    }

    /// Reserve payload bytes against the class byte bound; fails without side
    /// effects when the reservation cannot fit.
    fn reserve(&self, len: usize, limit: usize) -> bool {
        self.queued_bytes
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |queued| {
                queued.checked_add(len).filter(|total| *total <= limit)
            })
            .is_ok()
    }

    /// Queue one payload for the writer thread (FIFO). On a bound being hit
    /// the connection is marked terminated so the read loop starts teardown;
    /// every class has its own bound (mandate §4), so a bulk flood is
    /// refused while control still has reserved room in the same queue.
    pub(crate) fn enqueue(&self, payload: Vec<u8>, class: MsgClass) -> Result<(), EnqueueError> {
        if self.terminated.load(Ordering::SeqCst) {
            return Err(EnqueueError::Full);
        }
        let len = payload.len();
        if self.depth.get(class) >= class.max_messages() || !self.reserve(len, class.max_bytes()) {
            return Err(self.refuse(class));
        }
        match self.tx.try_send((class, payload)) {
            Ok(()) => {
                self.depth.add(class);
                Ok(())
            }
            Err(TrySendError::Full(p)) => {
                self.queued_bytes.fetch_sub(p.1.len(), Ordering::SeqCst);
                Err(self.refuse(class))
            }
            Err(TrySendError::Disconnected(_)) => {
                // the channel — and every payload it still held — is gone:
                // every reservation against it is void. Writer death (write
                // timeout, reset peer) is a liveness failure, not a class
                // bound being hit, so it terminates without touching the
                // saturation counters.
                self.queued_bytes.store(0, Ordering::SeqCst);
                self.terminated.store(true, Ordering::SeqCst);
                Err(EnqueueError::Disconnected)
            }
        }
    }

    pub(crate) fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::SeqCst)
    }
}

/// Writer thread body: dequeue queued payloads and write them as text
/// frames. Exits on the stop flag, a socket write failure (including the
/// write timeout against a stalled peer) or when every sender is gone.
/// Exiting drops `rx`, which makes every future `enqueue` fail with
/// `Disconnected` — that is how writer death propagates to producers.
/// `queued_bytes` is released as soon as a payload leaves the queue for
/// the socket; only the kernel's bounded send buffer remains beyond it.
fn run_writer(
    mut stream: TcpStream,
    rx: Receiver<(MsgClass, Vec<u8>)>,
    stop: Arc<AtomicBool>,
    queued_bytes: Arc<AtomicUsize>,
    depth: Arc<Depth>,
) {
    while !stop.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok((class, payload)) => {
                queued_bytes.fetch_sub(payload.len(), Ordering::SeqCst);
                depth.release(class);
                let frame = ws::encode_frame(ws::OP_TEXT, &payload, false);
                if write_frame(&mut stream, &frame).is_err() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// RAII teardown for one live connection (mandate §2): unregister, stop and
/// join the writer on *every* exit path — normal EOF, protocol violation,
/// saturation, `?` failure, or an unwind from a poisoned lock — so a panic
/// can never leak a registry entry, an orphaned `ws-writer` thread, or a
/// half-open socket. Teardown is idempotent: explicit `teardown()` runs it,
/// `Drop` runs it again as the unwind safety net and finds nothing to do.
struct ConnGuard {
    shared: Arc<crate::companion::Shared>,
    conn_id: u64,
    writer_stop: Arc<AtomicBool>,
    writer: Option<thread::JoinHandle<()>>,
}

impl ConnGuard {
    fn teardown(&mut self) {
        // cleanup must never panic: a poisoned registry lock still has to
        // yield its contents to the retain below
        let mut list = match self.shared.clients.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        list.retain(|(id, _)| *id != self.conn_id);
        drop(list);
        self.writer_stop.store(true, Ordering::SeqCst);
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.teardown();
    }
}

fn serve_ws(
    mut reader: TcpStream,
    writer: TcpStream,
    companion: Arc<Companion>,
    require_e2e: bool,
) -> io::Result<()> {
    // Fallible socket setup first: the cleanup below runs only after the
    // read loop, so anything that can fail with `?` must happen before the
    // writer is spawned or the connection is registered — otherwise a
    // failure here would leak the registry entry and the writer thread.
    // a dedicated handle for control frames (ping/pong/close)
    let mut control = reader.try_clone()?;
    // heartbeat: read timeout = one beat; N misses → dead peer (parity with
    // the Node host's HEARTBEAT_MS cadence and miss limit)
    reader.set_read_timeout(Some(Duration::from_millis(crate::protocol::HEARTBEAT_MS)))?;
    // the write timeout (D8) bounds every write, so a stalled peer can no
    // longer park the writer forever and teardown's join() is always bounded
    writer.set_write_timeout(Some(WRITE_TIMEOUT))?;

    let (outbound, rx) = Outbound::new(companion.shared.saturation.clone());
    let conn_id = companion
        .shared
        .next_conn_id
        .fetch_add(1, Ordering::Relaxed);

    // writer thread: outbound events/replies
    let writer_stop = Arc::new(AtomicBool::new(false));
    let wstop = writer_stop.clone();
    let wbytes = outbound.queued_bytes.clone();
    let wdepth = outbound.depth.clone();
    let writer_thread = thread::Builder::new()
        .name("ws-writer".into())
        .spawn(move || run_writer(writer, rx, wstop, wbytes, wdepth))
        .map_err(|_| io::Error::other("writer spawn"))?;
    // register only now: a spawn failure above leaves nothing behind, and
    // any event missed between spawn and registration is recovered by the
    // hello snapshot the client requests next
    companion
        .shared
        .clients
        .lock()
        .unwrap()
        .push((conn_id, outbound.clone()));
    // from here on teardown is owned by the guard: it runs on return, on `?`,
    // and on unwind (see ConnGuard) — the inline cleanup can no longer be
    // skipped by a panic above
    let mut guard = ConnGuard {
        shared: companion.shared.clone(),
        conn_id,
        writer_stop: writer_stop.clone(),
        writer: Some(writer_thread),
    };

    let mut misses: u32 = 0;
    // The live E2E session now lives in `outbound.session` (shared with the
    // fan-out path so events are sealed too). `hs` holds the in-flight
    // handshake between hs_hello and hs_proof.
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
                if outbound.is_terminated() {
                    break Err(io::Error::other("outbound queue saturated (slow consumer)"));
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
        // D8: a saturated queue means this peer is stalled — tear down even
        // if it keeps driving the heartbeat (checked before handling the
        // frame so no further reply is produced for a terminated peer)
        if outbound.is_terminated() {
            break Err(io::Error::other("outbound queue saturated (slow consumer)"));
        }
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
                // and tear the connection down on replay / desync / tamper. The
                // session lock is released before any path that re-enters it
                // (send_malformed → enqueue_sealed).
                let opened: Option<Result<String, String>> = {
                    let mut guard = outbound.session.lock().unwrap();
                    guard.as_mut().map(|session| {
                        std::str::from_utf8(&payload)
                            .map_err(|_| "non-utf8 sealed frame".to_string())
                            .and_then(|s| session.open(s))
                    })
                };
                let plaintext: String = match opened {
                    Some(Ok(t)) => t,
                    Some(Err(reason)) => break Err(io::Error::other(format!("e2e: {reason}"))),
                    None => match String::from_utf8(payload) {
                        Ok(s) => s,
                        Err(_) => {
                            if send_malformed(&outbound, &companion).is_err() {
                                break Err(io::Error::other("outbound queue saturated"));
                            }
                            continue;
                        }
                    },
                };
                let Some(msg) = parse_client_message(&plaintext) else {
                    if send_malformed(&outbound, &companion).is_err() {
                        break Err(io::Error::other("outbound queue saturated"));
                    }
                    continue;
                };
                // E2E handshake control (plaintext phase only, before a session)
                if !outbound.has_session() {
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
                            if outbound
                                .enqueue(reply.to_text().into_bytes(), MsgClass::Control)
                                .is_err()
                            {
                                break Err(io::Error::other("outbound queue saturated"));
                            }
                            continue;
                        }
                        ClientMsg::HsProof { proof } => {
                            let Some(pending) = hs.take() else {
                                if hs_err(&outbound, "no handshake in progress").is_err() {
                                    break Err(io::Error::other("outbound queue saturated"));
                                }
                                continue;
                            };
                            match establish_session(&companion, &pending, proof) {
                                Ok((session, server_proof)) => {
                                    // HsOk is plaintext; both sides switch to
                                    // sealed frames from the next message on.
                                    if outbound
                                        .enqueue(
                                            ServerMsg::HsOk { server_proof }.to_text().into_bytes(),
                                            MsgClass::Control,
                                        )
                                        .is_err()
                                    {
                                        break Err(io::Error::other("outbound queue saturated"));
                                    }
                                    outbound.set_session(session);
                                }
                                Err(reason) => {
                                    if hs_err(&outbound, &reason).is_err() {
                                        break Err(io::Error::other("outbound queue saturated"));
                                    }
                                }
                            }
                            continue;
                        }
                        _ => {}
                    }
                }
                // D2: remote posture — beyond loopback a stolen token alone
                // must not buy command execution (ADR-011: the credential
                // travels only inside AEAD frames, so a live E2E session
                // gates every command). Hello (version gate) and Ping
                // (liveness) plus the hs_* handshake above are the only
                // pre-session messages a compliant client needs; everything
                // else gets an explicit refusal it can recover from by
                // completing the handshake, never silent execution.
                if require_e2e
                    && !outbound.has_session()
                    && !matches!(msg, ClientMsg::Hello { .. } | ClientMsg::Ping { .. })
                {
                    let cid = msg.command_id().unwrap_or("unknown").to_string();
                    let refusal =
                        ServerMsg::ack_err(&cid, "e2e session required (remote posture)").to_text();
                    if outbound
                        .enqueue(refusal.into_bytes(), MsgClass::Control)
                        .is_err()
                    {
                        break Err(io::Error::other("outbound queue saturated"));
                    }
                    continue;
                }
                // strict integer equality, matching host.ts `!==`: the `as u32`
                // cast truncates (2.5 → 2), so the fractional check must run first
                if let ClientMsg::Hello { protocol, .. } = &msg
                    && (protocol.fract() != 0.0 || *protocol as u32 != PROTOCOL_VERSION)
                {
                    // write the mismatch ack synchronously, THEN close —
                    // ordering must be guaranteed before the socket dies.
                    // Sealed when a session is live (wire contract), written
                    // directly to `control` to keep the ack-before-close order.
                    let ack = ServerMsg::ack_err(
                        "hello",
                        &format!(
                            "protocol mismatch: client v{protocol}, server v{PROTOCOL_VERSION}"
                        ),
                    )
                    .to_text();
                    let ack_payload = {
                        let mut guard = outbound.session.lock().unwrap();
                        match guard.as_mut() {
                            Some(session) => session.seal(&ack),
                            None => ack.clone(),
                        }
                    };
                    let _ = write_frame(
                        &mut control,
                        &ws::encode_frame(ws::OP_TEXT, ack_payload.as_bytes(), false),
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
                    if outbound
                        .enqueue_sealed(text.into_bytes(), MsgClass::Control)
                        .is_err()
                    {
                        break Err(io::Error::other("outbound queue saturated"));
                    }
                }
            }
            Frame::Binary(_) | Frame::Continuation(_) => {
                // not part of the protocol: refuse politely, stay open
                if send_malformed(&outbound, &companion).is_err() {
                    break Err(io::Error::other("outbound queue saturated"));
                }
            }
        }
    };

    // teardown: unregister, stop the writer, drop the socket — routed
    // through the guard so the same three steps also run on unwind
    guard.teardown();
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

fn send_malformed(outbound: &Outbound, companion: &Companion) -> Result<(), EnqueueError> {
    companion.malformed_total.fetch_add(1, Ordering::Relaxed);
    // side-channel frame: sealed when a session is live (wire contract)
    outbound.enqueue_sealed(
        ServerMsg::ack_err("invalid", "malformed message")
            .to_text()
            .into_bytes(),
        MsgClass::Control,
    )
}

/* ------------------------------------------------------------------ */
/* E2E security matrix (ADR-011) — server-level integration tests.    */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::client::{WsClient, http_get, http_request};
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

    /// Post-integration threat-model fix: once a session is live, EVERY
    /// outbound frame — not just replies but also fan-out events — must be
    /// sealed. A plaintext event reaching a paired phone makes `handleSealed`
    /// fail to open it and tear the socket down (the mobile client has no
    /// plaintext fallback). This pins that an event fanned out to a
    /// session-bearing connection arrives sealed and openable.
    #[test]
    fn fanout_event_to_session_client_is_sealed() {
        let f = fixture("sealed-event", now_ms() + 60_000, false);
        let mut actor = connect(&f);
        let (_r, sess) = attempt(&mut actor, &f.psk, &f.device_id);
        let mut sess = sess.unwrap();
        // A second (watcher) connection with its OWN session: the event must
        // reach it sealed too, proving the fan-out seals per connection.
        let mut watcher = connect(&f);
        let (_r2, wsess) = attempt(&mut watcher, &f.psk, &f.device_id);
        let mut wsess = wsess.unwrap();

        // actor creates a session over its sealed channel → server fans out a
        // session_created event to BOTH connections.
        let create = r#"{"type":"create_session","commandId":"fan-1","agentId":"a4","title":"Fanout","prompt":"p"}"#;
        actor.send_text(&sess.seal(create)).unwrap();
        // The watcher must receive the event SEALED: it opens cleanly with the
        // watcher session and parses to a session_created event. A plaintext
        // event would fail wsess.open (AEAD/counter) — the exact mobile bug.
        let mut got_sealed_event = false;
        for _ in 0..32 {
            let frame = watcher.next_text().unwrap();
            let inner = match wsess.open(&frame) {
                Ok(t) => t,
                Err(e) => panic!("watcher received a non-sealed frame: {e} ({frame})"),
            };
            let v = json::parse(&inner).unwrap();
            if json::get(&v, "type").and_then(json::as_str) == Some("session_created") {
                got_sealed_event = true;
                break;
            }
        }
        assert!(
            got_sealed_event,
            "watcher never saw a sealed session_created event"
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

    /// Post-integration threat-model completeness (§3.10): the protocol-mismatch
    /// ack is a side-channel frame. It is written directly to `control` (to
    /// guarantee ack-before-close ordering), but it must STILL be sealed when a
    /// session is live — the mobile app sends its `hello` *after* the handshake,
    /// so a version-drift hits this path with a live session. A plaintext ack
    /// here would break `handleSealed` (no plaintext fallback) right before the
    /// close. This pins that the mismatch ack arrives sealed and openable.
    #[test]
    fn hello_gate_mismatch_ack_is_sealed_for_a_session_client() {
        let f = fixture("d7-sealed-gate", now_ms() + 60_000, false);
        let mut c = connect(&f);
        let (_r, sess) = attempt(&mut c, &f.psk, &f.device_id);
        let mut sess = sess.unwrap();
        // a version-drifted hello, sent SEALED over the live session
        let hello = r#"{"type":"hello","clientId":"d7s","protocol":999}"#;
        c.send_text(&sess.seal(hello)).unwrap();
        // the mismatch ack must come back SEALED: it opens cleanly with the
        // session and reports the protocol mismatch. A plaintext ack would
        // fail sess.open (AEAD/counter) — the exact mobile breakage.
        let frame = c.next_text().unwrap();
        let ack = sess
            .open(&frame)
            .unwrap_or_else(|e| panic!("mismatch ack was not sealed: {e} ({frame})"));
        assert!(ack.contains("protocol mismatch"), "got: {ack}");
        // and the server still closes with 4002 after the sealed ack
        match c.read_frame().unwrap() {
            Frame::Close(Some(code), reason) => {
                assert_eq!(code, CLOSE_PROTOCOL_MISMATCH);
                assert!(reason.contains("protocol mismatch"), "got: {reason}");
            }
            other => panic!("expected close 4002, got {other:?}"),
        }
        shutdown(f);
    }

    /* D1: browser origins are refused before the WS handshake. */

    #[test]
    fn ws_upgrade_with_origin_header_is_refused_403() {
        use std::io::Read;
        let f = fixture("d1-origin", now_ms() + 60_000, false);
        let mut s = std::net::TcpStream::connect(format!("127.0.0.1:{}", f.handle.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let key = base64::encode(&[7u8; 16]);
        let req = format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\nOrigin: http://evil.example\r\n\r\n"
        );
        s.write_all(req.as_bytes()).unwrap();
        // read until the status line is complete (one read may be partial)
        let mut resp = String::new();
        let mut buf = [0u8; 512];
        while !resp.contains("\r\n") {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "connection closed before a status line");
            resp.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(resp.starts_with("HTTP/1.1 403"), "got: {resp}");
        drop(s);
        // control: the identical upgrade without Origin still connects
        let mut c = connect(&f);
        c.send_text(r#"{"type":"ping","commandId":"d1-ping"}"#)
            .unwrap();
        assert!(c.next_text().unwrap().contains("pong"));
        shutdown(f);
    }

    /* D2 + D6: remote posture — beyond loopback a live E2E session gates
    every command and /health joins the token gate. The server still binds
    loopback inside the test; the posture flag makes it behave as if bound
    beyond loopback (production posture main.rs enforces with a token). */

    const REMOTE_TOKEN: &str = "remote-secret";

    /// Paired device + token + remote posture in one fixture.
    fn fixture_remote(tag: &str) -> Fixture {
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
                    revoked: false,
                })
                .unwrap();
        }
        let handle = start_with_posture(
            "127.0.0.1",
            0,
            0,
            Some(REMOTE_TOKEN.into()),
            companion.clone(),
            true,
        )
        .unwrap();
        Fixture {
            companion,
            handle,
            psk,
            device_id,
            data_dir: dir,
        }
    }

    fn connect_remote(f: &Fixture) -> WsClient {
        WsClient::connect(
            &format!("127.0.0.1:{}", f.handle.port),
            "/",
            Some(REMOTE_TOKEN),
        )
        .unwrap()
    }

    #[test]
    fn remote_posture_gates_commands_until_the_session_is_handshaken() {
        let f = fixture_remote("d2-enforce");
        let mut c = connect_remote(&f);
        // pre-session: version gate and liveness still work
        c.send_text(&format!(
            r#"{{"type":"hello","clientId":"d2","protocol":{PROTOCOL_VERSION}}}"#
        ))
        .unwrap();
        let ack = c.next_text().unwrap();
        assert!(ack.contains("hello_ack"), "got: {ack}");
        c.send_text(r#"{"type":"ping","commandId":"d2-ping"}"#)
            .unwrap();
        assert!(c.next_text().unwrap().contains("pong"));
        // pre-session: a command is refused, not executed — the commandId
        // survives so the client sees a correlated failure, and the
        // connection stays open so it can recover by handshaking
        c.send_text(r#"{"type":"list_state","commandId":"d2-1"}"#)
            .unwrap();
        let err = c.next_text().unwrap();
        assert!(err.contains("e2e session required"), "got: {err}");
        assert!(err.contains("d2-1"), "commandId must survive: {err}");
        // recovery: complete the handshake, the same command now runs
        let (resp, sess) = attempt(&mut c, &f.psk, &f.device_id);
        assert!(resp.contains("hs_ok"), "got: {resp}");
        let mut sess = sess.unwrap();
        c.send_text(&sess.seal(r#"{"type":"list_state","commandId":"d2-2"}"#))
            .unwrap();
        let reply = sess.open(&c.next_text().unwrap()).unwrap();
        assert!(reply.contains("\"state\""), "got: {reply}");
        shutdown(f);
    }

    #[test]
    fn loopback_posture_still_accepts_plaintext_commands() {
        // parity control: the flag is what changes — plain loopback keeps
        // the documented dev parity path (ADR-010 deviation note)
        let f = fixture("d2-parity", now_ms() + 60_000, false);
        let mut c = connect(&f);
        c.send_text(r#"{"type":"list_state","commandId":"d2-p"}"#)
            .unwrap();
        let reply = c.next_text().unwrap();
        assert!(reply.contains("\"state\""), "got: {reply}");
        shutdown(f);
    }

    #[test]
    fn health_gates_on_the_token_beyond_loopback_but_stays_open_on_loopback() {
        let f = fixture_remote("d6-health");
        let addr = format!("127.0.0.1:{}", f.handle.port);
        let denied = http_get(&addr, "/health").unwrap();
        assert!(denied.starts_with("HTTP/1.1 401"), "got: {denied}");
        let allowed = http_get(&addr, "/health?token=remote-secret").unwrap();
        assert!(allowed.starts_with("HTTP/1.1 200"), "got: {allowed}");
        // the diagnostics listener is loopback-only by contract: stays open
        let diag = format!("127.0.0.1:{}", f.handle.diagnostics_port);
        let d = http_get(&diag, "/health").unwrap();
        assert!(d.starts_with("HTTP/1.1 200"), "got: {d}");
        shutdown(f);
        // loopback posture: /health stays open without a token (dev parity)
        let g = fixture("d6-loopback", now_ms() + 60_000, false);
        let plain = http_get(&format!("127.0.0.1:{}", g.handle.port), "/health").unwrap();
        assert!(plain.starts_with("HTTP/1.1 200"), "got: {plain}");
        shutdown(g);
    }

    #[test]
    fn metrics_served_on_diagnostics_and_absent_on_main() {
        let f = fixture("p8-metrics", now_ms() + 60_000, false);
        // loopback diagnostics listener: Prometheus text exposition
        let diag = format!("127.0.0.1:{}", f.handle.diagnostics_port);
        let d = http_get(&diag, "/metrics").unwrap();
        assert!(d.starts_with("HTTP/1.1 200"), "got: {d}");
        assert!(
            d.contains("chernobyl_events_total") && d.contains("chernobyl_up 1"),
            "metrics must expose counters + up gauge: {d}"
        );
        // the main listener never serves /metrics (operator counters stay local)
        let main = format!("127.0.0.1:{}", f.handle.port);
        let m = http_get(&main, "/metrics").unwrap();
        assert!(
            m.starts_with("HTTP/1.1 404"),
            "main must not serve /metrics: {m}"
        );
        shutdown(f);
    }

    /* D8: bounded outbound queue — count + byte caps, explicit overload
    teardown, writer lifecycle. All deterministic: no sleeps in the
    assertions, real sockets only where the writer's wire behavior is
    the invariant under test. */

    /// Boot a companion with a Claude hook ingress mounted and start the
    /// server (loopback, no token). Returns the fixture + the claude adapter.
    fn fixture_claude(tag: &str) -> Fixture {
        let dir = temp_dir(tag);
        let mut c = default_config();
        c.data_dir = dir.clone();
        c.adapters.push(crate::config::AdapterEntryConfig {
            id: "cc-1".into(),
            kind: "claude".into(),
            enabled: true,
            endpoint: None,
            basic_auth: None,
        });
        let companion = Arc::new(Companion::boot(c).unwrap());
        let handle = start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
        Fixture {
            companion,
            handle,
            psk: [0u8; 32],
            device_id: String::new(),
            data_dir: dir,
        }
    }

    #[test]
    fn claude_hook_ingress_accepts_on_diagnostics_and_rejects_on_main() {
        let f = fixture_claude("cc-ingress");
        // the REAL captured SessionStart payload (Claude Code 2.1.309)
        let payload = r#"{"session_id":"24e5d08e-906e-47bf-b902-4fc7d50acdec","transcript_path":"C:\\t.jsonl","cwd":"G:\\Chernobyl","hook_event_name":"SessionStart","source":"startup"}"#;

        // diagnostics listener (loopback): accepted, event enters the pipeline
        let diag = format!("http://127.0.0.1:{}", f.handle.diagnostics_port);
        let r = http_request("POST", &format!("{diag}/hooks/claude"), None, Some(payload)).unwrap();
        assert!(r.status == 200, "diag POST must accept, got {}", r.status);
        let snap = f.companion.snapshot();
        assert!(
            snap.activity
                .iter()
                .any(|a| a.text.contains("session started")),
            "hook must surface as activity: {:?}",
            snap.activity
        );

        // the MAIN listener never answers a POST: its auth/session surface is
        // untouched — a hook there is a 404.
        let main = format!("http://127.0.0.1:{}", f.handle.port);
        let m = http_request("POST", &format!("{main}/hooks/claude"), None, Some(payload)).unwrap();
        assert!(
            m.status == 404,
            "main listener must refuse POST, got {}",
            m.status
        );

        // malformed payload on the ingress: 400, never a fabricated event
        let bad =
            http_request("POST", &format!("{diag}/hooks/claude"), None, Some("{nope")).unwrap();
        assert!(
            bad.status == 400,
            "malformed hook must 400, got {}",
            bad.status
        );

        shutdown(f);
    }

    /* D8: bounded outbound queue — count + byte caps, explicit overload
    teardown, writer lifecycle. All deterministic: no sleeps in the
    assertions, real sockets only where the writer's wire behavior is
    the invariant under test. */

    fn tcp_pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (server, client)
    }

    /// Fresh Outbound with its own saturation counters (class-policy tests
    /// that read the counters back build theirs via `Outbound::new` directly).
    fn test_outbound() -> (Arc<Outbound>, Receiver<(MsgClass, Vec<u8>)>) {
        Outbound::new(Arc::new(crate::companion::Saturation::default()))
    }

    #[test]
    fn outbound_queue_bounds_count_releases_on_dequeue_and_preserves_fifo() {
        let (outbound, rx) = test_outbound();
        for i in 0..OUTBOUND_QUEUE_MAX_MESSAGES {
            outbound.enqueue(vec![i as u8], MsgClass::Control).unwrap();
        }
        assert_eq!(
            outbound.queued_bytes.load(Ordering::SeqCst),
            OUTBOUND_QUEUE_MAX_MESSAGES
        );
        // count bound hit: explicit failure, connection marked terminated
        assert!(matches!(
            outbound.enqueue(vec![0], MsgClass::Control),
            Err(EnqueueError::Full)
        ));
        assert!(outbound.is_terminated());
        // queue drains in FIFO order (byte release on dequeue is the writer's
        // job — asserted in run_writer_writes_queued_frames_in_order)
        for i in 0..OUTBOUND_QUEUE_MAX_MESSAGES {
            assert_eq!(rx.try_recv().unwrap().1, vec![i as u8]);
        }
    }

    #[test]
    fn outbound_queue_bounds_total_bytes() {
        let (outbound, rx) = test_outbound();
        let msg = vec![b'x'; 2_000_000];
        outbound.enqueue(msg.clone(), MsgClass::Control).unwrap();
        outbound.enqueue(msg, MsgClass::Control).unwrap(); // 4 MB of 4.19 MB budget
        assert!(matches!(
            outbound.enqueue(vec![b'x'; 2_000_000], MsgClass::Control),
            Err(EnqueueError::Full)
        ));
        assert!(outbound.is_terminated());
        // reservations are released by the writer on dequeue; without one,
        // draining the channel by hand leaves them accounted (see the
        // run_writer test for the release path)
        assert_eq!(outbound.queued_bytes.load(Ordering::SeqCst), 4_000_000);
        rx.try_recv().unwrap();
    }

    #[test]
    fn outbound_byte_bound_is_inclusive_at_the_cap() {
        let (outbound, _rx) = test_outbound();
        // a payload exactly at the cap fits; anything more does not
        outbound
            .enqueue(vec![b'x'; OUTBOUND_QUEUE_MAX_BYTES], MsgClass::Control)
            .unwrap();
        assert!(matches!(
            outbound.enqueue(vec![0], MsgClass::Control),
            Err(EnqueueError::Full)
        ));
    }

    #[test]
    fn outbound_queue_reports_disconnected_and_resets_bytes_after_receiver_drops() {
        let (outbound, rx) = test_outbound();
        outbound.enqueue(vec![1, 2, 3], MsgClass::Control).unwrap();
        drop(rx);
        assert!(matches!(
            outbound.enqueue(vec![9], MsgClass::Control),
            Err(EnqueueError::Disconnected)
        ));
        assert!(outbound.is_terminated());
        // the dropped channel took its queued payloads with it: no reservation survives
        assert_eq!(outbound.queued_bytes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn run_writer_writes_queued_frames_in_order() {
        let (server_sock, mut peer) = tcp_pair();
        let (outbound, rx) = test_outbound();
        let wbytes = outbound.queued_bytes.clone();
        let wdepth = outbound.depth.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let wstop = stop.clone();
        let writer = thread::spawn(move || run_writer(server_sock, rx, wstop, wbytes, wdepth));
        for i in 0..3u8 {
            outbound.enqueue(vec![b'0' + i], MsgClass::Control).unwrap();
        }
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        for i in 0..3u8 {
            match ws::read_frame(&mut peer, false).unwrap() {
                Frame::Text(p) => assert_eq!(p, vec![b'0' + i]),
                other => panic!("expected text frame, got {other:?}"),
            }
        }
        // every dequeued payload released its byte reservation on the way out
        assert_eq!(outbound.queued_bytes.load(Ordering::SeqCst), 0);
        stop.store(true, Ordering::SeqCst);
        writer.join().unwrap();
    }

    #[test]
    fn run_writer_exits_when_all_senders_drop() {
        let (server_sock, _peer) = tcp_pair();
        let (outbound, rx) = test_outbound();
        let wbytes = outbound.queued_bytes.clone();
        let wdepth = outbound.depth.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let writer = thread::spawn(move || run_writer(server_sock, rx, stop, wbytes, wdepth));
        // last sender gone → the writer must wake and exit (no stop flag, no leak)
        drop(outbound);
        writer.join().unwrap();
    }

    #[test]
    fn run_writer_terminates_on_reset_peer() {
        let (server_sock, peer) = tcp_pair();
        // standalone channel: this test exercises the writer's socket-error
        // path, not the Outbound accounting
        let (tx, rx) = sync_channel(64);
        let stop = Arc::new(AtomicBool::new(false));
        let writer = thread::spawn(move || {
            run_writer(
                server_sock,
                rx,
                stop,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(Depth::default()),
            )
        });
        drop(peer); // peer gone: once the reset lands, every write fails
        // keep the writer fed until it dies on the write error; the socket
        // buffers cannot absorb 32 MiB, so a live writer would never let
        // this loop finish — the bounded spin is the failure detector
        let mut spins = 0;
        while !writer.is_finished() {
            match tx.try_send((MsgClass::Control, vec![b'x'; 65_536])) {
                Ok(()) | Err(TrySendError::Full(_)) => std::thread::yield_now(),
                Err(TrySendError::Disconnected(_)) => break,
            }
            spins += 1;
            assert!(spins < 50_000, "writer never terminated after peer reset");
        }
        writer.join().unwrap();
    }

    #[test]
    fn saturated_outbound_queue_tears_the_connection_down() {
        let f = fixture("d8-sat", now_ms() + 60_000, false);
        let mut c = connect(&f);
        // registration happens right after the upgrade response reaches the
        // client; settle for it without asserting on timing
        let outbound = {
            let mut found = None;
            for _ in 0..200 {
                let clients = f.companion.shared.clients.lock().unwrap();
                if let Some((_, q)) = clients.last() {
                    found = Some(q.clone());
                    break;
                }
                drop(clients);
                std::thread::sleep(Duration::from_millis(5));
            }
            found.expect("connection must register with the client registry")
        };
        // one oversized payload cannot be reserved: explicit Full + terminated
        assert!(
            outbound
                .enqueue(vec![b'x'; OUTBOUND_QUEUE_MAX_BYTES + 1], MsgClass::Control)
                .is_err()
        );
        assert!(outbound.is_terminated());
        // the next inbound frame triggers teardown: no reply, socket closes
        c.send_text(r#"{"type":"ping","commandId":"d8"}"#).unwrap();
        if let Ok(other) = c.read_frame() {
            // EOF/reset (Err) is the expected teardown outcome
            panic!("expected the connection to close, got {other:?}");
        }
        // the registry entry is gone too (teardown ran)
        assert!(f.companion.shared.clients.lock().unwrap().is_empty());
        shutdown(f);
    }

    #[test]
    fn sequential_command_replies_stay_in_order() {
        let f = fixture("d8-fifo", now_ms() + 60_000, false);
        let mut c = connect(&f);
        for i in 0..20 {
            c.send_text(&format!(r#"{{"type":"ping","commandId":"fifo-{i}"}}"#))
                .unwrap();
        }
        for i in 0..20 {
            let pong = c.next_text().unwrap();
            let v = json::parse(&pong).unwrap();
            assert_eq!(
                json::get_str(&v, "commandId"),
                Some(format!("fifo-{i}").as_str()),
                "replies must keep enqueue order"
            );
        }
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

    /* ---- mandate §2: unwind-safe teardown (ConnGuard) -------------- */

    #[test]
    fn conn_guard_teardown_runs_on_unwind_and_is_idempotent() {
        // Regression for P1: a panic on the connection thread (e.g. a
        // poisoned registry/session lock) used to skip the inline teardown,
        // leaking the registry entry and the ws-writer thread forever.
        let dir = temp_dir("guard");
        let mut c = default_config();
        c.data_dir = dir.clone();
        let companion = Companion::boot(c).unwrap();

        let (outbound, _rx) = Outbound::new(companion.shared.saturation.clone());
        companion.shared.clients.lock().unwrap().push((7, outbound));

        let stop = Arc::new(AtomicBool::new(false));
        let wstop = stop.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let writer = thread::spawn(move || {
            while !wstop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            let _ = tx.send(());
        });

        let shared = companion.shared.clone();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut guard = ConnGuard {
                shared,
                conn_id: 7,
                writer_stop: stop,
                writer: Some(writer),
            };
            let _ = &mut guard;
            panic!("simulated handler panic");
        }));
        assert!(panic.is_err(), "the test panic must propagate");

        // the unwind path must have completed BOTH teardown steps
        assert_eq!(
            companion.shared.clients.lock().unwrap().len(),
            0,
            "registry entry leaked after a panic"
        );
        rx.recv_timeout(Duration::from_secs(1))
            .expect("writer thread must be stopped and joined after an unwind");

        // a second teardown (explicit + Drop) must be a harmless no-op
        let mut again = ConnGuard {
            shared: companion.shared.clone(),
            conn_id: 7,
            writer_stop: Arc::new(AtomicBool::new(false)),
            writer: None,
        };
        again.teardown();
        drop(again);
        assert_eq!(companion.shared.clients.lock().unwrap().len(), 0);

        companion.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /* ---- mandate §4: per-class bounds + saturation visibility ------- */

    #[test]
    fn control_keeps_reserved_headroom_when_bulk_fills_the_queue() {
        // Regression for P2: bulk (event fan-out) at its own cap must not
        // consume the room control frames need — the old single 256-message
        // bound let one class starve the other.
        let (outbound, rx) = test_outbound();
        for _ in 0..BULK_QUEUE_MAX_MESSAGES {
            outbound.enqueue(vec![b'b'; 8], MsgClass::Bulk).unwrap();
        }
        // bulk is at its cap, yet control is still admitted to ITS cap
        for _ in 0..OUTBOUND_QUEUE_MAX_MESSAGES {
            outbound.enqueue(vec![b'c'; 8], MsgClass::Control).unwrap();
        }
        assert!(!outbound.is_terminated());
        assert_eq!(
            outbound.depth.get(MsgClass::Bulk) + outbound.depth.get(MsgClass::Control),
            CHANNEL_CAPACITY,
            "both class caps must fit the shared channel"
        );
        // first refusal is the class cap itself (257th control frame)
        assert!(matches!(
            outbound.enqueue(vec![b'c'; 8], MsgClass::Control),
            Err(EnqueueError::Full)
        ));
        assert!(outbound.is_terminated());
        // one FIFO: both classes drain in arrival order
        let (mut bulk, mut ctrl) = (0usize, 0usize);
        while let Ok((class, _)) = rx.try_recv() {
            match class {
                MsgClass::Bulk => bulk += 1,
                MsgClass::Control => ctrl += 1,
            }
        }
        assert_eq!(
            (bulk, ctrl),
            (BULK_QUEUE_MAX_MESSAGES, OUTBOUND_QUEUE_MAX_MESSAGES)
        );
    }

    #[test]
    fn shed_seal_consumes_no_aead_counter() {
        // Admission is checked BEFORE sealing: a refused frame must never
        // spend an AEAD counter, or counter order would diverge from wire
        // order on a session that survives the refusal.
        let (outbound, _rx) = test_outbound();
        let keys = e2e::derive_keys(&[7u8; 32], &[9u8; 16]);
        let peer_keys = e2e::derive_keys(&[7u8; 32], &[9u8; 16]);
        *outbound.session.lock().unwrap() = Some(e2e::Session::host(keys));
        let mut peer = e2e::Session::client(peer_keys);
        // fill the bulk class to its cap (depth admission only, no sealing)
        for _ in 0..BULK_QUEUE_MAX_MESSAGES {
            outbound.enqueue(vec![b'b'; 8], MsgClass::Bulk).unwrap();
        }
        assert!(matches!(
            outbound.enqueue_sealed(br#"{"type":"event"}"#.to_vec(), MsgClass::Bulk),
            Err(EnqueueError::Full)
        ));
        // the shed frame spent no counter: the next seal still starts at 0,
        // so a peer at counter 0 opens it (a consumed counter would desync)
        let payload = outbound
            .session
            .lock()
            .unwrap()
            .as_mut()
            .expect("session installed above")
            .seal("post-shed frame");
        assert_eq!(
            peer.open(&payload)
                .expect("shed frame must not consume a counter"),
            "post-shed frame"
        );
    }

    #[test]
    fn saturation_counters_surface_in_health_and_metrics() {
        // Regression for P2: production overload must be readable from
        // /health and /metrics, never inferred from dropped clients.
        let dir = temp_dir("sat-obs");
        let mut c = default_config();
        c.data_dir = dir.clone();
        let companion = Companion::boot(c).unwrap();
        let (outbound, _rx) = Outbound::new(companion.shared.saturation.clone());
        for _ in 0..BULK_QUEUE_MAX_MESSAGES {
            outbound.enqueue(vec![b'b'; 8], MsgClass::Bulk).unwrap();
        }
        assert!(outbound.enqueue(vec![b'b'; 8], MsgClass::Bulk).is_err());

        let health = companion.health();
        let get = |key: &str| json::as_i64(json::get(&health, key).expect(key)).expect(key);
        assert_eq!(get("outboundShedBulk"), 1);
        assert_eq!(get("outboundShedCtrl"), 0);
        assert_eq!(get("outboundSatTeardowns"), 1);

        let metrics = companion.metrics_text();
        assert!(metrics.contains("chernobyl_outbound_shed_bulk_total"));
        assert!(metrics.contains("chernobyl_outbound_shed_ctrl_total"));
        assert!(metrics.contains("chernobyl_outbound_saturation_teardowns_total"));

        companion.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
