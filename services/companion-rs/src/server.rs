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
use crate::protocol::{ClientMsg, PROTOCOL_VERSION, ServerMsg, WorkspaceOp, parse_client_message};
use crate::storage::{DeviceRecord, PairingRecord, Storage};
use crate::ws::{self, Frame, Handshake};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const CLOSE_UNAUTHORIZED: u16 = 4001;
pub const CLOSE_PROTOCOL_MISMATCH: u16 = 4002;
pub const CLOSE_DEVICE_REVOKED: u16 = 4003;

/// The only Origin a WebSocket upgrade may carry: the phone app's explicit
/// non-web origin (D1). Mirrored by `APP_ORIGIN` in packages/e2e.
pub const APP_ORIGIN: &str = "chernobyl://app";

pub const AUTH_FAILURE_LIMIT: u32 = 5;

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
pub const OUTBOUND_QUEUE_MAX_MESSAGES: usize = 256;
pub const OUTBOUND_QUEUE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Bulk (event fan-out) is admitted only while its own depth and a 3 MiB
/// shared byte budget hold: the last 64 queue slots and the last 1 MiB stay
/// reserved for control frames. Overflowing either bound refuses admission —
/// the connection tears down (never a silent drop; a shed event would be
/// invisible loss because no client consumes `lastSeq` for gap detection).
pub(crate) const BULK_QUEUE_MAX_MESSAGES: usize = 192;
pub(crate) const BULK_QUEUE_MAX_BYTES: usize = 3 * 1024 * 1024;
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

/// Header carrying the companion's control key on the loopback control
/// routes (lower-case: `Handshake` lower-cases header names).
pub const CONTROL_KEY_HEADER: &str = "x-chernobyl-control";

/// Security review 2026-10-25: any local process — any Windows user, any
/// service account — reaches 127.0.0.1, so POST /pairing/offer (mints a PSK)
/// and POST /devices/revoke (cuts a phone off) also require the control key,
/// which only an account that can read the user-private data dir has.
fn has_control_key(h: &Handshake, companion: &Companion) -> bool {
    h.header(CONTROL_KEY_HEADER)
        .is_some_and(|k| ct_eq(k, companion.control_key()))
}

/// Maximum concurrent connections on the main listener (mandate §2, audit P7).
pub const MAIN_CONNECTION_CAP: usize = 1024;
/// Maximum concurrent connections on the loopback diagnostics listener (mandate §2, audit P7).
pub const DIAG_CONNECTION_CAP: usize = 64;

type ConnRegistry = Arc<Mutex<Vec<(TcpStream, thread::JoinHandle<()>)>>>;

pub struct ServerHandle {
    pub port: u16,
    pub diagnostics_port: u16,
    stop: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
    conn_threads: ConnRegistry,
}

impl ServerHandle {
    /// Ordered shutdown (mandate §2, audit P6): stop listeners and scheduler,
    /// shutdown all live connection sockets, then join connection threads with
    /// an overall 5 s deadline. Returns true only if every connection thread
    /// joined within the deadline.
    pub fn stop(mut self) -> bool {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let mut conns = match self.conn_threads.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        for (stream, _) in &conns {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !conns.is_empty() {
            let mut i = 0;
            while i < conns.len() {
                if conns[i].1.is_finished() {
                    let (_, handle) = conns.swap_remove(i);
                    let _ = handle.join();
                } else {
                    i += 1;
                }
            }
            if conns.is_empty() || std::time::Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let all_joined = conns.is_empty();
        if !all_joined {
            eprintln!(
                "[companion] stop: {} connection thread(s) still active after 5 s deadline",
                conns.len()
            );
        }
        all_joined
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
    let remote_posture =
        (bind != "127.0.0.1" && bind != "localhost") || companion.shared.config.server_require_e2e;
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
    start_with_caps(
        bind,
        port,
        diagnostics_port,
        token,
        companion,
        remote_posture,
        MAIN_CONNECTION_CAP,
        DIAG_CONNECTION_CAP,
    )
}

/// Same as [`start_with_posture`] with explicit per-listener connection caps
/// (audit P7), so tests can exercise connection limits with small caps.
#[allow(clippy::too_many_arguments)]
pub fn start_with_caps(
    bind: &str,
    port: u16,
    diagnostics_port: u16,
    token: Option<String>,
    companion: Arc<Companion>,
    remote_posture: bool,
    main_cap: usize,
    diag_cap: usize,
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
    let conn_threads: ConnRegistry = Arc::new(Mutex::new(Vec::new()));

    for (listener, token, diagnostics_only, cap) in [
        (
            crate::ffi::clone_listener(&listener).map_err(|e| e.to_string())?,
            token.clone(),
            false,
            main_cap,
        ),
        (
            crate::ffi::clone_listener(&diag_listener).map_err(|e| e.to_string())?,
            None,
            true,
            diag_cap,
        ),
    ] {
        let stop = stop.clone();
        let companion = companion.clone();
        let conns = conn_threads.clone();
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
                        cap,
                        conns,
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
        conn_threads,
    })
}

#[allow(clippy::too_many_arguments)]
fn accept_loop(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    token: Option<String>,
    companion: Arc<Companion>,
    diagnostics_only: bool,
    remote_posture: bool,
    cap: usize,
    conn_threads: ConnRegistry,
) {
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    let listener_active = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::SeqCst) {
        // Opportunistic reaping of finished connection handles (mandate §2, audit P6)
        if let Ok(mut conns) = conn_threads.lock() {
            let mut i = 0;
            while i < conns.len() {
                if conns[i].1.is_finished() {
                    let (_, handle) = conns.swap_remove(i);
                    let _ = handle.join();
                } else {
                    i += 1;
                }
            }
        }

        match listener.accept() {
            Ok((stream, _)) => {
                // Connection cap check (audit P7)
                let admitted = listener_active
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |curr| {
                        if curr < cap { Some(curr + 1) } else { None }
                    })
                    .is_ok();
                if !admitted {
                    companion
                        .shared
                        .connections_rejected
                        .fetch_add(1, Ordering::SeqCst);
                    drop(stream);
                    continue;
                }
                companion
                    .shared
                    .connections_active
                    .fetch_add(1, Ordering::SeqCst);

                // Windows quirk: sockets returned by accept() on a NONBLOCKING
                // listener inherit nonblocking mode — force blocking immediately
                stream.set_nonblocking(false).ok();

                // never inherited by a child the companion spawns (ffi::no_inherit)
                let stream_clone = match crate::ffi::clone_stream(&stream) {
                    Ok(s) => s,
                    Err(_) => {
                        listener_active.fetch_sub(1, Ordering::SeqCst);
                        companion
                            .shared
                            .connections_active
                            .fetch_sub(1, Ordering::SeqCst);
                        companion
                            .shared
                            .connections_rejected
                            .fetch_add(1, Ordering::SeqCst);
                        continue;
                    }
                };

                let token = token.clone();
                let companion_clone = companion.clone();
                let active_guard_count = listener_active.clone();
                let shared_guard_count = companion.shared.clone();

                struct ActiveGuard {
                    listener_active: Arc<AtomicUsize>,
                    shared: Arc<crate::companion::Shared>,
                }
                impl Drop for ActiveGuard {
                    fn drop(&mut self) {
                        self.listener_active.fetch_sub(1, Ordering::SeqCst);
                        self.shared
                            .connections_active
                            .fetch_sub(1, Ordering::SeqCst);
                    }
                }

                let stop_conn = stop.clone();
                let spawn_res = thread::Builder::new().name("conn".into()).spawn(move || {
                    let _guard = ActiveGuard {
                        listener_active: active_guard_count,
                        shared: shared_guard_count,
                    };
                    let _ = handle_conn(
                        stream,
                        token,
                        companion_clone,
                        diagnostics_only,
                        remote_posture,
                        stop_conn,
                    );
                });

                match spawn_res {
                    Ok(handle) => {
                        if let Ok(mut conns) = conn_threads.lock() {
                            conns.push((stream_clone, handle));
                        }
                    }
                    Err(_) => {
                        listener_active.fetch_sub(1, Ordering::SeqCst);
                        companion
                            .shared
                            .connections_active
                            .fetch_sub(1, Ordering::SeqCst);
                        companion
                            .shared
                            .connections_rejected
                            .fetch_add(1, Ordering::SeqCst);
                    }
                }
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
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    // Windows quirk: sockets returned by accept() on a NONBLOCKING listener
    // inherit nonblocking mode — force blocking or every idle read "fails".
    stream.set_nonblocking(false).ok();
    stream.set_nodelay(true).ok();
    // the request head AND any POST body share one 30 s deadline (slowloris
    // bound; security review 2026-10-25: bodies used to get a fresh 30 s per
    // read), read through the short poll so a shutdown is not stuck behind a
    // half-sent request
    stream.set_read_timeout(Some(IDLE_POLL))?;
    let mut reader = crate::ffi::clone_stream(&stream)?;
    let mut writer = stream;

    let mut request = StopAwareReader {
        inner: &mut reader,
        stop: &stop,
        deadline: Instant::now() + HTTP_REQUEST_DEADLINE,
    };
    let handshake = match ws::read_handshake(&mut request) {
        Ok(h) => h,
        Err(_) => return Ok(()), // hostile/garbage/shutdown: drop the connection
    };

    // Security pass 2026-11-15 (N9): a second Origin header is a smuggling
    // attempt (a proxy and this check could each read a different one).
    if handshake.origin_count() > 1 {
        drain_body(&mut request, &handshake);
        let _ = ws::write_http_error(&mut writer, 403, "Forbidden", "");
        return Ok(());
    }
    // Security pass 2026-11-15 (N1): a request stamped by a reverse proxy or
    // tunnel arrives from 127.0.0.1 but is remote. Treat this connection as
    // remote posture (E2E required, no pre-session snapshot, /health behind
    // the token); with no token to hold it to, refuse it outright. The
    // diagnostics listener has no token, so a proxied request there is
    // refused too. Spoofing these headers only makes the connection stricter.
    let proxied = handshake.is_proxied();
    if proxied && token.is_none() {
        drain_body(&mut request, &handshake);
        let _ = ws::write_http_error(&mut writer, 403, "Forbidden", "");
        return Ok(());
    }
    let remote_posture = remote_posture || proxied;

    // HTTP routes (non-upgrade) -------------------------------------------
    if !handshake.is_websocket_upgrade() {
        // Phase 7: POST hook ingress (Claude Code) — reachable ONLY on the
        // loopback diagnostics listener (PRD §15.2). The main listener never
        // answers a POST: its auth/session surface is untouched. RFC 6455
        // upgrades are GET-only (`validate_upgrade`), so a POST here is always
        // a plain HTTP request.
        if handshake.method == "POST" {
            let path = handshake.target.split('?').next().unwrap_or("/");
            if !diagnostics_only {
                // Drain the (capped) body before refusing so the close is a
                // graceful FIN, not a RST racing the client's read of our 404
                // (Windows resets a socket closed with an unread body).
                drain_body(&mut request, &handshake);
                let _ = ws::write_http_error(&mut writer, 404, "Not Found", "");
                return Ok(());
            }
            match path {
                "/hooks/claude" => {
                    return hook_ingress(&mut request, &mut writer, &handshake, &companion);
                }
                "/hooks/claude/permission" => {
                    return hook_permission_ingress(
                        &mut request,
                        &mut writer,
                        &handshake,
                        &companion,
                        &stop,
                    );
                }
                "/pairing/offer" => {
                    return pairing_offer_ingress(
                        &mut request,
                        &mut writer,
                        &handshake,
                        &companion,
                    );
                }
                "/devices/revoke" => {
                    return revoke_ingress(&mut request, &mut writer, &handshake, &companion);
                }
                _ => {
                    drain_body(&mut request, &handshake);
                    let _ = ws::write_http_error(&mut writer, 404, "Not Found", "");
                    return Ok(());
                }
            }
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
    // A browser always sends its page's real Origin (http/https, or "null")
    // and a page cannot forge the header, so every Origin is refused except
    // APP_ORIGIN: React Native's Android WebSocket adds a default
    // "http://host:port" Origin to every connection, so the phone app sets
    // this non-web value explicitly (found on the first emulator run). This
    // closes cross-site WebSocket hijacking, DNS rebinding included, in the
    // default config (loopback, token-less, plaintext commands).
    if handshake
        .header("origin")
        .is_some_and(|o| o.trim() != APP_ORIGIN)
    {
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
    serve_ws(reader, writer, companion, remote_posture, &stop)
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
/// peer lies about Content-Length, or trickles it past the request deadline,
/// we simply stop reading.
fn drain_body(reader: &mut StopAwareReader<'_>, h: &Handshake) {
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
/// oversized Content-Length. Rejects requests with an Origin header (403).
fn hook_ingress(
    reader: &mut StopAwareReader<'_>,
    writer: &mut TcpStream,
    h: &Handshake,
    companion: &Companion,
) -> io::Result<()> {
    // Security review 2026-10-25: the same browser guard as the sibling
    // routes. A text/plain POST needs no CORS preflight, so without it any
    // web page could forge hook activity into the persisted event pipeline.
    if h.header("origin").is_some() {
        drain_body(reader, h);
        let _ = ws::write_http_error(writer, 403, "Forbidden", "");
        return Ok(());
    }
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

/// ADR-018: how long a `PermissionRequest` hook may wait for the phone
/// (`?wait=<ms>`, clamped); the hook CLI asks for its own `--timeout`.
pub const HOOK_WAIT_DEFAULT_MS: i64 = 120_000;
pub const HOOK_WAIT_MAX_MS: i64 = 600_000;

/// ADR-018 approvals from the phone for Claude Code sessions on the PC:
/// loopback diagnostics only, the control key required, no Origin (like
/// the other control routes). The body is the hook's stdin JSON. The
/// answer is 200 `{"behavior":"allow"}` / `{"behavior":"deny","message"}`
/// once the phone decides, or 204 (nothing to print, the PC asks as
/// usual) when the request is not the phone's, its wait runs out, the
/// companion stops, or the hook goes away first (the PC answered: the
/// request becomes `stale`). Never longer than the wait.
fn hook_permission_ingress(
    reader: &mut StopAwareReader<'_>,
    writer: &mut TcpStream,
    h: &Handshake,
    companion: &Companion,
    stop: &AtomicBool,
) -> io::Result<()> {
    if h.header("origin").is_some() || !has_control_key(h, companion) {
        drain_body(reader, h);
        let _ = ws::write_http_error(writer, 403, "Forbidden", "");
        return Ok(());
    }
    let clen = h
        .header("content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n <= ws::MAX_HOOK_BODY_BYTES);
    let Some(clen) = clen else {
        drain_body(reader, h);
        let _ = ws::write_http_error(writer, 400, "Bad Request", "");
        return Ok(());
    };
    let mut body = vec![0u8; clen];
    if reader.read_exact(&mut body).is_err() {
        let _ = ws::write_http_error(writer, 400, "Bad Request", "");
        return Ok(());
    }
    let no_content = b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    let Ok(payload) = String::from_utf8(body) else {
        return writer.write_all(no_content);
    };
    let wait_ms = h
        .target
        .split_once("?wait=")
        .and_then(|(_, w)| w.parse::<i64>().ok())
        .unwrap_or(HOOK_WAIT_DEFAULT_MS)
        .clamp(1_000, HOOK_WAIT_MAX_MS);
    let Some((id, rx, rt)) = companion.claude_hook_permission(&payload, wait_ms) else {
        return writer.write_all(no_content);
    };
    let deadline = Instant::now() + Duration::from_millis(wait_ms as u64);
    let end = loop {
        match rx.recv_timeout(IDLE_POLL) {
            Ok(verdict) => {
                let body = match verdict {
                    crate::claude_runtime::HookVerdict::Allow => {
                        json::obj(vec![("behavior", json::s("allow"))])
                    }
                    crate::claude_runtime::HookVerdict::Deny(m) => json::obj(vec![
                        ("behavior", json::s("deny")),
                        ("message", json::s(&m)),
                    ]),
                };
                let body = json::stringify(&body);
                return writer.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
            // dropped by the runtime (disposed): nothing to say
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return writer.write_all(no_content);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= deadline || stop.load(Ordering::SeqCst) {
            break crate::claude_runtime::HookEnd::Expired;
        }
        // the hook process is gone (Claude Code took the PC's answer and
        // cancelled it): a closed or reset socket reads 0 or fails
        let mut probe = [0u8; 1];
        match reader.inner.peek(&mut probe) {
            Ok(0) => break crate::claude_runtime::HookEnd::AnsweredOnPc,
            Ok(_) => {}
            Err(e) if is_timeout(&e) => {}
            Err(_) => break crate::claude_runtime::HookEnd::AnsweredOnPc,
        }
    };
    rt.hook_ended(&id, end);
    let _ = writer.write_all(no_content);
    Ok(())
}

/// In-process device revocation ingress (D4): reachable ONLY on the loopback diagnostics
/// listener. Rejects requests with an Origin header (403), requires a valid JSON body
/// with `{"deviceId":"<id>"}`, marks the device revoked in the companion's store,
/// appends an audit record, and terminates every live connection for that device with
/// WebSocket close code 4003 ("device revoked"). A request without the control key is
/// refused like one with an Origin header.
fn revoke_ingress(
    reader: &mut StopAwareReader<'_>,
    writer: &mut TcpStream,
    h: &Handshake,
    companion: &Companion,
) -> io::Result<()> {
    if h.header("origin").is_some() || !has_control_key(h, companion) {
        drain_body(reader, h);
        let _ = ws::write_http_error(writer, 403, "Forbidden", "");
        return Ok(());
    }
    let clen = h
        .header("content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n <= ws::MAX_HOOK_BODY_BYTES);
    let Some(clen) = clen else {
        drain_body(reader, h);
        let _ = ws::write_http_error(writer, 400, "Bad Request", "");
        return Ok(());
    };
    let mut body = vec![0u8; clen];
    if reader.read_exact(&mut body).is_err() {
        let _ = ws::write_http_error(writer, 400, "Bad Request", "");
        return Ok(());
    }
    let Ok(payload) = String::from_utf8(body) else {
        let _ = ws::write_http_error(writer, 400, "Bad Request", "");
        return Ok(());
    };
    let Ok(json_val) = json::parse(&payload) else {
        let _ = ws::write_http_error(writer, 400, "Bad Request", "");
        return Ok(());
    };
    let Some(device_id) = json::get_str(&json_val, "deviceId") else {
        let _ = ws::write_http_error(writer, 400, "Bad Request", "");
        return Ok(());
    };
    if device_id.is_empty() {
        let _ = ws::write_http_error(writer, 400, "Bad Request", "");
        return Ok(());
    }
    match companion.revoke_device(device_id) {
        Ok(crate::companion::RevokeResult::NotFound) => {
            let _ = ws::write_http_error(writer, 404, "Not Found", "");
        }
        Ok(crate::companion::RevokeResult::Revoked { terminated }) => {
            let resp_body = format!("{{\"revoked\":true,\"terminated\":{terminated}}}");
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{resp_body}",
                resp_body.len()
            );
            writer.write_all(resp.as_bytes())?;
        }
        Err(_) => {
            let _ = ws::write_http_error(writer, 500, "Internal Server Error", "");
        }
    }
    Ok(())
}

/// In-process pairing offer ingress: reachable ONLY on the loopback diagnostics
/// listener. Rejects requests with an Origin header (403), requires a valid JSON body with
/// optional `{"endpoints":["ws://..."]}`, creates the pairing offer in the companion's
/// own store, and returns HTTP 200 with the pairing payload JSON string the phone parses.
/// A request without the control key is refused like one with an Origin header.
fn pairing_offer_ingress(
    reader: &mut StopAwareReader<'_>,
    writer: &mut TcpStream,
    h: &Handshake,
    companion: &Companion,
) -> io::Result<()> {
    if h.header("origin").is_some() || !has_control_key(h, companion) {
        drain_body(reader, h);
        let _ = ws::write_http_error(writer, 403, "Forbidden", "");
        return Ok(());
    }
    let clen = h
        .header("content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n <= ws::MAX_HOOK_BODY_BYTES);
    let Some(clen) = clen else {
        drain_body(reader, h);
        let _ = ws::write_http_error(writer, 400, "Bad Request", "");
        return Ok(());
    };
    let mut body = vec![0u8; clen];
    if reader.read_exact(&mut body).is_err() {
        let _ = ws::write_http_error(writer, 400, "Bad Request", "");
        return Ok(());
    }
    let Ok(payload) = String::from_utf8(body) else {
        let _ = ws::write_http_error(writer, 400, "Bad Request", "");
        return Ok(());
    };
    let Ok(json_val) = json::parse(&payload) else {
        let _ = ws::write_http_error(writer, 400, "Bad Request", "");
        return Ok(());
    };
    let json::Json::Obj(_) = json_val else {
        let _ = ws::write_http_error(writer, 400, "Bad Request", "");
        return Ok(());
    };
    let endpoints = match json::get(&json_val, "endpoints") {
        Some(json::Json::Arr(arr)) => {
            let mut eps = Vec::new();
            for item in arr {
                if let Some(s) = json::as_str(item) {
                    eps.push(s.to_string());
                } else {
                    let _ = ws::write_http_error(writer, 400, "Bad Request", "");
                    return Ok(());
                }
            }
            eps
        }
        None => Vec::new(),
        _ => {
            let _ = ws::write_http_error(writer, 400, "Bad Request", "");
            return Ok(());
        }
    };
    match companion.create_pairing_offer(&endpoints) {
        Ok(resp_body) => {
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{resp_body}",
                resp_body.len()
            );
            writer.write_all(resp.as_bytes())?;
        }
        Err(_) => {
            let _ = ws::write_http_error(writer, 500, "Internal Server Error", "");
        }
    }
    Ok(())
}

/// Client side of the loopback control routes (the CLI's `--pair` and
/// `--revoke`, and tests): POST a JSON `body` to `url` carrying the control
/// `key`. `client::http_request` takes no custom header; the companion closes
/// after every answer, so the response is read to EOF. `content_type` is not
/// parsed (left empty).
pub fn control_post(
    url: &str,
    key: &str,
    body: &str,
) -> Result<crate::client::HttpResponse, String> {
    control_post_within(url, key, body, Duration::from_secs(15))
}

/// [`control_post`] that waits up to `read` for the answer (ADR-018: the
/// permission hook waits for the phone). A loopback connect gets 2 s.
pub fn control_post_within(
    url: &str,
    key: &str,
    body: &str,
    read: Duration,
) -> Result<crate::client::HttpResponse, String> {
    let (authority, path) = crate::client::split_url(url)?;
    let addr: std::net::SocketAddr = authority
        .parse()
        .map_err(|e| format!("address {authority}: {e}"))?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|e| format!("connect {authority}: {e}"))?;
    s.set_read_timeout(Some(read)).map_err(|e| e.to_string())?;
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nContent-Type: application/json\r\n{CONTROL_KEY_HEADER}: {key}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| format!("write: {e}"))?;
    let mut raw = String::new();
    s.read_to_string(&mut raw)
        .map_err(|e| format!("read: {e}"))?;
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or("response has no header terminator")?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("bad status line: {head}"))?;
    Ok(crate::client::HttpResponse {
        status,
        content_type: String::new(),
        body: body.to_string(),
    })
}

/// Per-connection handshake state: the device that asked to connect, the
/// server's fresh nonce+salt and the client's fresh nonce its proof must be
/// bound to (ADR-011).
struct HsPending {
    device_id: String,
    nonce: [u8; 16],
    salt: [u8; 16],
    cnonce: [u8; 16],
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

/// Which stored PSKs (protected forms) a handshake for `device_id` may prove:
/// the current one, the rotated device key still awaiting the phone's ack
/// (ADR-011 addendum), and the offer to redeem when this is a first
/// connection. Rejects expired / spent / revoked / unknown devices.
fn handshake_candidate(
    store: &Storage,
    device_id: &str,
    now: i64,
) -> Result<(String, Option<String>, Option<PairingRecord>), String> {
    match (store.pairings.get(device_id), store.devices.get(device_id)) {
        // fresh, unexpired offer: first connection (redeemed by `redeem`)
        (Some(p), _) if !p.consumed && now < p.expires_at => {
            Ok((p.psk_protected.clone(), None, Some(p.clone())))
        }
        // already-paired device, or an offer whose redemption already ran
        (_, Some(d)) if d.revoked => Err("device revoked".to_string()),
        (_, Some(d)) => Ok((d.psk_protected.clone(), d.psk_next_protected.clone(), None)),
        (Some(p), None) if p.consumed => Err("pairing offer already used".to_string()),
        (Some(p), None) if now >= p.expires_at => Err("pairing offer expired".to_string()),
        _ => Err("unknown device".to_string()),
    }
}

/// A fresh device key, DPAPI-protected like every stored PSK.
fn mint_protected_psk() -> Result<String, String> {
    let key = crate::ffi::csprng_bytes(32)?;
    crate::ffi::dpapi_protect(&key)
        .map(|c| base64::encode(&c))
        .map_err(|e| format!("protect device key: {e}"))
}

/// Make a device's rotated key its only key (ADR-011 addendum): from here on
/// the QR's PSK fails the handshake. The caller holds the storage lock. A
/// revoked device, or one with nothing pending, is left as it is.
fn promote_next_psk(store: &mut Storage, device_id: &str) -> Result<(), String> {
    if let Some(mut d) = store.devices.get(device_id).cloned()
        && !d.revoked
        && let Some(next) = d.psk_next_protected.take()
    {
        d.psk_protected = next;
        store
            .put_device(&d)
            .map_err(|e| format!("rotate device key: {e}"))?;
    }
    Ok(())
}

/// Resolve + verify a handshake and, for a fresh pairing, redeem it (single-use
/// consume + device registration). Returns the live session, the server's
/// mutual-auth proof, and the rotated device key this connection must be
/// offered (`hs_rekey`, ADR-011 addendum). Rejects expired / spent / revoked /
/// unknown devices and any proof that is not a valid HMAC under the device's
/// current PSK or its pending rotated key.
///
/// DPAPI (an LSASS round-trip) and the HMAC run with the storage lock released
/// (security review 2026-10-25): an unauthenticated hs_proof must not hold the
/// process-wide mutex that event persistence, commands and revocation need.
/// The PSKs are picked under the lock, verified (and a first connection's
/// device key minted) unlocked, and `redeem` decides again under the lock
/// before anything is committed.
fn establish_session(
    companion: &Companion,
    pending: &HsPending,
    client_proof: &str,
) -> Result<(e2e::Session, String, Option<e2e::Psk>), String> {
    let proof = base64::decode(client_proof).ok_or("bad proof encoding")?;
    let (current, next, offer) = {
        let store = companion.shared.storage.lock().unwrap();
        handshake_candidate(&store, &pending.device_id, crate::protocol::now_ms())?
    };
    let proof_as = |psk: &e2e::Psk, role: &str| {
        e2e::handshake_proof(
            psk,
            role,
            &pending.device_id,
            &pending.nonce,
            &pending.salt,
            &pending.cnonce,
        )
    };
    // the current key first; the rotated one only for a phone that already
    // stored it (its ack never arrived)
    let mut psk = unprotect_psk(&current)?;
    let mut verified = current;
    if !e2e::proof_ok(&proof_as(&psk, "client"), &proof) {
        verified = next.ok_or("proof verification failed")?;
        psk = unprotect_psk(&verified)?;
        if !e2e::proof_ok(&proof_as(&psk, "client"), &proof) {
            return Err("proof verification failed".to_string());
        }
    }
    let minted = offer.map(|_| mint_protected_psk()).transpose()?;
    let rekey = redeem(companion, pending, &verified, minted)?
        .map(|next| unprotect_psk(&next))
        .transpose()?;
    let keys = e2e::derive_keys(&psk, &pending.salt, &pending.cnonce);
    Ok((
        e2e::Session::host(keys),
        base64::encode(&proof_as(&psk, "server")),
        rekey,
    ))
}

/// Commit a verified handshake under the storage lock. The records may have
/// moved while the PSK was verified unlocked (revoked, expired, redeemed by a
/// concurrent handshake, or rotated), so the decision is taken again on the
/// current records and the verified PSK counts only if it is still one they
/// name. A fresh offer is redeemed here, so single use stays atomic: the
/// device is registered under the offer's PSK with `minted` queued as its
/// rotated key (ADR-011 addendum). A proof under the queued key promotes it.
/// Returns the queued key (protected) this connection must be offered: the
/// same one on every connection until the phone acks it, never a new mint.
fn redeem(
    companion: &Companion,
    pending: &HsPending,
    verified: &str,
    minted: Option<String>,
) -> Result<Option<String>, String> {
    let mut store = companion.shared.storage.lock().unwrap();
    let now = crate::protocol::now_ms();
    let (current, next, offer) = handshake_candidate(&store, &pending.device_id, now)?;
    if next.as_deref() == Some(verified) {
        promote_next_psk(&mut store, &pending.device_id)?;
        return Ok(None);
    }
    if current != verified {
        return Err("device changed during handshake".to_string());
    }
    let Some(mut p) = offer else {
        return Ok(next); // paired already: re-offer the pending key, if any
    };
    let next = minted.ok_or("device changed during handshake")?;
    p.consumed = true;
    store
        .put_pairing(&p)
        .map_err(|e| format!("redeem pairing: {e}"))?;
    store
        .put_device(&DeviceRecord {
            device_id: pending.device_id.clone(),
            name: pending.device_id.clone(),
            psk_protected: p.psk_protected.clone(),
            psk_next_protected: Some(next.clone()),
            paired_at: now,
            revoked: false,
        })
        .map_err(|e| format!("register device: {e}"))?;
    Ok(Some(next))
}

/// Install a verified session, then re-check revocation (security review
/// 2026-10-25). `revoke_device` persists `revoked` BEFORE it scans live
/// sessions, so a revocation racing this handshake is caught either by its
/// scan (the session is already installed) or by this re-check (the flag is
/// already set). Returns false when the device was revoked meanwhile: the
/// connection is then closed with 4003.
fn install_session(
    companion: &Companion,
    outbound: &Outbound,
    device_id: String,
    session: e2e::Session,
) -> bool {
    outbound.set_session(device_id.clone(), session);
    let revoked = companion
        .shared
        .storage
        .lock()
        .unwrap()
        .devices
        .get(&device_id)
        .is_some_and(|d| d.revoked);
    if revoked {
        outbound.close(CLOSE_DEVICE_REVOKED, "device revoked");
    }
    !revoked
}

/// The live E2E session bound to a verified device (ADR-011 / D4).
pub struct LiveSession {
    pub device_id: String,
    pub session: e2e::Session,
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
    /// set on saturation/disconnect/revocation: the read loop must tear this connection down
    terminated: AtomicBool,
    /// set on revocation: clean exit without saturation error logging
    revoked: AtomicBool,
    /// An `hs_*` handshake is in flight (hs_hello accepted, session not yet
    /// installed): fan-out is dropped instead of sent plaintext (security
    /// review 2026-10-25), see `enqueue_sealed`.
    handshaking: AtomicBool,
    /// The live E2E session for this connection (ADR-011). Shared with the
    /// read loop (which installs it on `hs_ok`) and with the engine fan-out
    /// (which seals events per connection) so that EVERY outbound frame —
    /// reply, event, or side-channel — is sealed once a session exists, as
    /// the wire contract requires ("from `hs_ok` on every frame is sealed").
    /// Stores the device id alongside the session (D4) so live sessions can
    /// be revoked by device id.
    /// Seal-then-enqueue happens under this lock so the AEAD counter order
    /// always matches the FIFO wire order.
    session: Mutex<Option<LiveSession>>,
    /// Process-wide saturation counters (mandate §4): shared so /health and
    /// /metrics describe overload on any connection, including one already gone.
    saturation: Arc<crate::companion::Saturation>,
    /// Dedicated handle to write control/close frames directly (D4 revocation).
    control: Mutex<Option<TcpStream>>,
    /// Remote posture (D2): until a session is live, engine events are
    /// dropped instead of sent in plaintext (see `enqueue_sealed`).
    require_e2e: bool,
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
    /// Public for the live runtime tests (`tests/opencode_e2e.rs`), which
    /// subscribe a connection without a socket.
    pub fn new(
        saturation: Arc<crate::companion::Saturation>,
        require_e2e: bool,
    ) -> (Arc<Outbound>, Receiver<(MsgClass, Vec<u8>)>) {
        let (tx, rx) = sync_channel(CHANNEL_CAPACITY);
        (
            Arc::new(Outbound {
                tx,
                queued_bytes: Arc::new(AtomicUsize::new(0)),
                depth: Arc::new(Depth::default()),
                terminated: AtomicBool::new(false),
                revoked: AtomicBool::new(false),
                handshaking: AtomicBool::new(false),
                session: Mutex::new(None),
                saturation,
                control: Mutex::new(None),
                require_e2e,
            }),
            rx,
        )
    }

    pub(crate) fn set_control(&self, stream: TcpStream) {
        *self.control.lock().unwrap() = Some(stream);
    }

    pub(crate) fn close(&self, code: u16, reason: &str) {
        self.revoked.store(true, Ordering::SeqCst);
        self.terminated.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.control.lock()
            && let Some(mut stream) = guard.take()
        {
            let close = ws::encode_frame(ws::OP_CLOSE, &close_payload(code, reason), false);
            let _ = write_frame(&mut stream, &close);
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    pub(crate) fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::SeqCst)
    }

    /// Install the live E2E session and device id (called by the read loop on `hs_ok`).
    /// From this point every frame this connection sends is sealed.
    pub(crate) fn set_session(&self, device_id: String, session: e2e::Session) {
        *self.session.lock().unwrap() = Some(LiveSession { device_id, session });
        // only now: a fan-out frame racing the install is dropped (still
        // handshaking) or sealed (session live), never plaintext after hs_ok
        self.set_handshaking(false);
    }

    /// Mark an `hs_*` handshake as in flight (true when hs_hello is accepted)
    /// or over (false once the session is installed or the handshake failed).
    pub(crate) fn set_handshaking(&self, on: bool) {
        self.handshaking.store(on, Ordering::SeqCst);
    }

    /// True once an E2E session is live for this connection.
    pub(crate) fn has_session(&self) -> bool {
        self.session.lock().unwrap().is_some()
    }

    pub(crate) fn device_id(&self) -> Option<String> {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.device_id.clone())
    }

    /// Queue one plaintext for this connection, sealing it first when a
    /// session is live. This is the single outbound sealing point the wire
    /// contract requires: replies, events, and side-channel frames all pass
    /// through here (or `enqueue` for the pre-session plaintext phase).
    /// Seal-then-enqueue is done under the session lock so the AEAD counter
    /// order always matches the FIFO wire order — the client's strict
    /// monotonic `open` would otherwise see a reordering as replay/desync.
    /// With no session the plaintext is enqueued verbatim (loopback parity),
    /// except an engine event at remote posture or while a handshake is in
    /// flight, which is dropped.
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
            Some(live) => {
                // plaintext is always UTF-8 JSON from `to_text()`; seal it.
                let text = String::from_utf8_lossy(&plaintext);
                live.session.seal(&text).into_bytes()
            }
            // Security review 2026-10-25: an engine event for a connection
            // without a session is dropped, never sent in plaintext, (a) at
            // remote posture — the token alone must not buy the live stream
            // (D9) — and (b) mid-handshake, where the phone would read a
            // plaintext event as a broken handshake. Decided under the session
            // lock, so it cannot race `set_session`; the client resyncs from
            // the sealed hello snapshot after hs_ok, so nothing is lost.
            None if class == MsgClass::Bulk
                && (self.require_e2e || self.handshaking.load(Ordering::SeqCst)) =>
            {
                return Ok(());
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

    /// ADR-015 §3.5 backpressure: bulk-class frames queued now.
    pub(crate) fn bulk_depth(&self) -> usize {
        self.depth.get(MsgClass::Bulk)
    }

    /// Bytes the bulk budget is checked against: the whole queue's byte
    /// count (see `MsgClass::max_bytes`).
    pub(crate) fn bulk_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::SeqCst)
    }
}

impl Outbound {
    /// Takes one queued frame with the writer's accounting (byte and depth
    /// release), for tests that drain a queue without a socket (unit tests
    /// and the live runtime tests in `tests/`).
    pub fn test_dequeue(&self, rx: &Receiver<(MsgClass, Vec<u8>)>) -> Option<(MsgClass, Vec<u8>)> {
        let (class, payload) = rx.try_recv().ok()?;
        self.queued_bytes.fetch_sub(payload.len(), Ordering::SeqCst);
        self.depth.release(class);
        Some((class, payload))
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
        // ADR-015: a gone connection keeps no transcript subscription
        let mut hub = match self.shared.hub.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        hub.unsubscribe(self.conn_id, None);
        drop(hub);
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

/// Read-timeout poll interval while a connection is idle (audit P6).
const IDLE_POLL: Duration = Duration::from_millis(250);
/// Total time a peer gets to send its HTTP request head (and a POST body).
const HTTP_REQUEST_DEADLINE: Duration = Duration::from_secs(30);

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

/// A read that must not be split (an HTTP request head or body, the rest of
/// a WS frame) over a socket with the short IDLE_POLL timeout: timeouts are
/// retried — bytes `read_exact` already consumed must not be lost — until
/// shutdown (audit P6) or `deadline`, after which the peer counts as dead.
/// The deadline bounds the WHOLE read: it is checked before every socket
/// read, data or not (security review 2026-10-25 — checked only on a
/// timeout, a peer trickling a byte per poll never hit it: slowloris).
struct StopAwareReader<'a> {
    inner: &'a mut TcpStream,
    stop: &'a AtomicBool,
    deadline: Instant,
}

impl Read for StopAwareReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if Instant::now() >= self.deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "read deadline"));
            }
            match self.inner.read(buf) {
                Err(e) if is_timeout(&e) && !self.stop.load(Ordering::SeqCst) => {}
                r => return r,
            }
        }
    }
}

fn serve_ws(
    mut reader: TcpStream,
    writer: TcpStream,
    companion: Arc<Companion>,
    require_e2e: bool,
    stop: &AtomicBool,
) -> io::Result<()> {
    // Fallible socket setup first: the cleanup below runs only after the
    // read loop, so anything that can fail with `?` must happen before the
    // writer is spawned or the connection is registered — otherwise a
    // failure here would leak the registry entry and the writer thread.
    // dedicated handle for control frames (ping/pong/close)
    let mut control = crate::ffi::clone_stream(&reader)?;
    // Short read timeout so shutdown is noticed promptly (audit P6). It only
    // ever fires while idle between frames (`peek` consumes nothing) or is
    // retried by `StopAwareReader` mid-frame, so a frame is never split. (On
    // Windows, ServerHandle::stop()'s shutdown of a cloned handle does not
    // wake a blocked recv, hence the poll.)
    reader.set_read_timeout(Some(IDLE_POLL))?;
    // the write timeout (D8) bounds every write, so a stalled peer can no
    // longer park the writer forever and teardown's join() is always bounded
    writer.set_write_timeout(Some(WRITE_TIMEOUT))?;

    let (outbound, rx) = Outbound::new(companion.shared.saturation.clone(), require_e2e);
    outbound.set_control(crate::ffi::clone_stream(&reader)?);
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

    // heartbeat: every HEARTBEAT_MS of silence is a miss (ping sent); N
    // misses → dead peer (parity with the Node host's cadence and limit)
    let beat = Duration::from_millis(crate::protocol::HEARTBEAT_MS);
    let mut idle_since = Instant::now();
    let result: io::Result<()> = loop {
        match reader.peek(&mut [0u8; 1]) {
            Ok(_) => {} // data or EOF: read_frame reports either
            Err(e) if is_timeout(&e) => {
                if stop.load(Ordering::SeqCst) {
                    break Ok(());
                }
                if idle_since.elapsed() < beat * (misses + 1) {
                    continue;
                }
                misses += 1;
                if misses > crate::protocol::HEARTBEAT_MISS_LIMIT {
                    break Err(io::Error::other("heartbeat miss limit"));
                }
                if outbound.is_revoked() {
                    break Ok(());
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
            Err(e) => break Err(e),
        }
        // a whole frame must land within the dead-peer window from its
        // first byte: now that the deadline counts trickled bytes too, one
        // beat could cut a legitimate max-size frame on a slow link
        let mut frame_reader = StopAwareReader {
            inner: &mut reader,
            stop,
            deadline: Instant::now() + beat * crate::protocol::HEARTBEAT_MISS_LIMIT,
        };
        let frame = match ws::read_frame(&mut frame_reader, true) {
            Ok(f) => f,
            Err(ws::WsError::Io(e)) => break Err(e),
            // protocol violation from this peer: close the connection only
            Err(_) => break Err(io::Error::other("protocol violation")),
        };
        misses = 0;
        idle_since = Instant::now();
        // D8: a saturated queue means this peer is stalled — tear down even
        // if it keeps driving the heartbeat (checked before handling the
        // frame so no further reply is produced for a terminated peer)
        if outbound.is_revoked() {
            break Ok(());
        }
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
                    guard.as_mut().map(|live| {
                        std::str::from_utf8(&payload)
                            .map_err(|_| "non-utf8 sealed frame".to_string())
                            .and_then(|s| live.session.open(s))
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
                    // the rotation ack is transport-level like hs_* (never a
                    // ClientMsg) and counts only sealed: the phone has stored
                    // the rotated key. A failed write leaves it pending; the
                    // phone's next handshake proves it and `redeem` promotes it.
                    if let Some(device_id) = outbound.device_id()
                        && json::parse(&plaintext)
                            .is_ok_and(|v| json::get_str(&v, "type") == Some("hs_rekey_ack"))
                    {
                        let mut store = companion.shared.storage.lock().unwrap();
                        let _ = promote_next_psk(&mut store, &device_id);
                        continue;
                    }
                    if send_malformed(&outbound, &companion).is_err() {
                        break Err(io::Error::other("outbound queue saturated"));
                    }
                    continue;
                };
                // E2E handshake control (plaintext phase only, before a session)
                if !outbound.has_session() {
                    match &msg {
                        ClientMsg::HsHello { device_id, cnonce } => {
                            // the client's fresh nonce is mandatory (security
                            // review 2026-10-25): without it a replayed server
                            // transcript authenticates an impostor host
                            let Some(cnonce) = cnonce
                                .as_deref()
                                .and_then(base64::decode)
                                .and_then(|b| <[u8; 16]>::try_from(b).ok())
                            else {
                                hs = None;
                                outbound.set_handshaking(false);
                                if hs_err(&outbound, "hs_hello needs a 16-byte base64 cnonce")
                                    .is_err()
                                {
                                    break Err(io::Error::other("outbound queue saturated"));
                                }
                                continue;
                            };
                            let (nonce, salt) = e2e::fresh_nonce_salt();
                            hs = Some(HsPending {
                                device_id: device_id.clone(),
                                nonce,
                                salt,
                                cnonce,
                            });
                            outbound.set_handshaking(true);
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
                                Ok((session, server_proof, rekey)) => {
                                    // HsOk is the LAST plaintext frame: fan-out
                                    // stays dropped (handshaking) until the
                                    // install below, then every frame is sealed.
                                    if outbound
                                        .enqueue(
                                            ServerMsg::HsOk { server_proof }.to_text().into_bytes(),
                                            MsgClass::Control,
                                        )
                                        .is_err()
                                    {
                                        break Err(io::Error::other("outbound queue saturated"));
                                    }
                                    if !install_session(
                                        &companion,
                                        &outbound,
                                        pending.device_id,
                                        session,
                                    ) {
                                        break Ok(()); // revoked meanwhile: closed with 4003
                                    }
                                    // ADR-011 addendum: offer the rotated device
                                    // key, sealed, until the phone acks it (a
                                    // phone that ignores it keeps the QR's key)
                                    if let Some(key) = rekey {
                                        let frame = format!(
                                            r#"{{"type":"hs_rekey","psk":"{}"}}"#,
                                            base64::encode(&key)
                                        );
                                        if outbound
                                            .enqueue_sealed(frame.into_bytes(), MsgClass::Control)
                                            .is_err()
                                        {
                                            break Err(io::Error::other(
                                                "outbound queue saturated",
                                            ));
                                        }
                                    }
                                }
                                Err(reason) => {
                                    outbound.set_handshaking(false);
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
                            Some(live) => live.session.seal(&ack),
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
                    // D9 (option B, 2026-10-25): beyond loopback the state
                    // snapshot is session-only. A pre-session hello still
                    // passes the version gate above but gets no snapshot —
                    // over an untrusted transport (ADR-011) a token-only
                    // hello would hand hosts, prompts and commands to any
                    // passive observer of the token-carrying URL.
                    ClientMsg::Hello { .. } if require_e2e && !outbound.has_session() => Some(
                        ServerMsg::ack_err("hello", "e2e session required (remote posture)"),
                    ),
                    ClientMsg::Hello { .. } => Some(ServerMsg::HelloAck {
                        protocol: PROTOCOL_VERSION,
                        server_time: crate::protocol::now_ms(),
                        snapshot: companion.snapshot(),
                        last_seq: companion.last_seq(),
                        features: if companion.shared.config.terminal_enabled {
                            crate::protocol::FEATURES_WITH_TERMINAL
                        } else {
                            crate::protocol::FEATURES
                        },
                    }),
                    // ADR-015: a subscription belongs to this connection (its
                    // id and Outbound); the D2 gate above already demanded a
                    // live E2E session beyond loopback. Never journaled.
                    ClientMsg::SessionSubscribe {
                        command_id,
                        session_id,
                        epoch,
                        after_seq,
                    } => Some(companion.subscribe(
                        conn_id,
                        &outbound,
                        command_id,
                        session_id,
                        epoch.clone().zip(*after_seq),
                    )),
                    ClientMsg::SessionUnsubscribe {
                        command_id,
                        session_id,
                    } => Some(companion.unsubscribe(conn_id, command_id, session_id)),
                    // ADR-020: the terminal's output goes to this connection
                    // only, and only one with a live E2E session gets a sink
                    // (a paired device, whatever the posture); without it the
                    // companion refuses the run.
                    ClientMsg::Workspace {
                        op: WorkspaceOp::TerminalRun { .. },
                        ..
                    } => {
                        let sink = outbound.has_session().then(|| {
                            let out = outbound.clone();
                            Arc::new(move |m: ServerMsg| {
                                out.enqueue_sealed(m.to_text().into_bytes(), MsgClass::Bulk)
                                    .is_ok()
                            }) as crate::companion::TermSink
                        });
                        companion.handle_client_msg_with(msg, sink)
                    }
                    ClientMsg::Workspace {
                        command_id,
                        workspace_id,
                        op: WorkspaceOp::TerminalStop,
                    } => Some(companion.terminal_stop(command_id, workspace_id)),
                    // ADR-016 reads: the result goes to this connection only,
                    // right before its ack (the D2 gate above applies). Never
                    // journaled; mutations take the command path below.
                    ClientMsg::Workspace {
                        command_id,
                        workspace_id,
                        op,
                    } if !op.is_mutating() => {
                        let (result, ack) = companion.workspace_query(command_id, workspace_id, op);
                        if let Some(result) = result
                            && outbound
                                .enqueue_sealed(result.to_text().into_bytes(), MsgClass::Control)
                                .is_err()
                        {
                            break Err(io::Error::other("outbound queue saturated"));
                        }
                        Some(ack)
                    }
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
        let clean = handle.stop();
        companion.shutdown(clean);
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
        fixture_paired_with(tag, revoked, |_| {})
    }

    fn fixture_paired_with(
        tag: &str,
        revoked: bool,
        tweak: impl FnOnce(&mut crate::config::CompanionConfig),
    ) -> Fixture {
        let dir = temp_dir(tag);
        let mut c = default_config();
        c.data_dir = dir.clone();
        tweak(&mut c);
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
                    psk_next_protected: None,
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

    /// One raw request on a fresh connection; returns the response status line.
    fn raw_status(port: u16, req: &str) -> String {
        use std::io::Read;
        let mut s = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        let mut buf = [0u8; 512];
        while !resp.contains("\r\n") {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => resp.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
        resp.lines().next().unwrap_or("").to_string()
    }

    /// The server's challenge for one handshake + the client nonce it answers.
    struct Challenge {
        nonce: Vec<u8>,
        salt: Vec<u8>,
        cnonce: Vec<u8>,
    }

    /// Handshake step 1: `hs_hello` with a fresh client nonce → `hs_challenge`.
    fn begin(c: &mut WsClient, device_id: &str) -> Challenge {
        let cnonce = crate::ffi::csprng_bytes(16).unwrap();
        c.send_text(&format!(
            "{{\"type\":\"hs_hello\",\"deviceId\":\"{device_id}\",\"cnonce\":\"{}\"}}",
            base64::encode(&cnonce)
        ))
        .unwrap();
        let challenge = c.next_text().unwrap();
        let v = json::parse(&challenge).unwrap();
        Challenge {
            nonce: base64::decode(json::get_str(&v, "nonce").unwrap()).unwrap(),
            salt: base64::decode(json::get_str(&v, "salt").unwrap()).unwrap(),
            cnonce,
        }
    }

    /// Round-trips a sealed ping and returns the key of any `hs_rekey` seen
    /// before its pong. Frames are handled in order, so the pong also proves
    /// that everything this client sent earlier (an `hs_rekey_ack`) was
    /// processed and that the server's `hs_ok`-time frames were all read.
    fn ping_barrier(c: &mut WsClient, sess: &mut e2e::Session) -> Option<[u8; 32]> {
        c.send_text(&sess.seal(r#"{"type":"ping","commandId":"barrier"}"#))
            .unwrap();
        let mut rekey = None;
        loop {
            let frame = c.next_text().unwrap();
            let inner = sess
                .open(&frame)
                .unwrap_or_else(|e| panic!("non-sealed frame after hs_ok: {e} ({frame})"));
            let v = json::parse(&inner).unwrap();
            match json::get_str(&v, "type") {
                Some("pong") => return rekey,
                Some("hs_rekey") => {
                    let key = base64::decode(json::get_str(&v, "psk").unwrap()).unwrap();
                    rekey = Some(key.try_into().expect("hs_rekey carries 32 bytes"));
                }
                _ => {}
            }
        }
    }

    /// Handshake step 2: `hs_proof` with `psk`. Returns the server's final
    /// `hs_ok`/`hs_err` payload and, on success, the client-side session and
    /// the rotated key the server offered on it (ADR-011 addendum), which the
    /// session has already read past (`ping_barrier`).
    fn finish_rekey(
        c: &mut WsClient,
        psk: &[u8; 32],
        device_id: &str,
        ch: &Challenge,
    ) -> (String, Option<e2e::Session>, Option<[u8; 32]>) {
        let proof_as = |role: &str| {
            e2e::handshake_proof(psk, role, device_id, &ch.nonce, &ch.salt, &ch.cnonce)
        };
        c.send_text(&format!(
            "{{\"type\":\"hs_proof\",\"proof\":\"{}\"}}",
            base64::encode(&proof_as("client"))
        ))
        .unwrap();
        let resp = c.next_text().unwrap();
        let v = json::parse(&resp).unwrap();
        if json::get_str(&v, "type") == Some("hs_ok") {
            // mutual auth: the server must prove PSK knowledge too
            let sp = base64::decode(json::get_str(&v, "serverProof").unwrap()).unwrap();
            assert_eq!(sp.as_slice(), proof_as("server"), "server must prove PSK");
            let keys = e2e::derive_keys(psk, &ch.salt, &ch.cnonce);
            let mut sess = e2e::Session::client(keys);
            let rekey = ping_barrier(c, &mut sess);
            (resp, Some(sess), rekey)
        } else {
            (resp, None, None)
        }
    }

    /// `finish_rekey` for callers that ignore the rotation offer (as a phone
    /// without rotation support does).
    fn finish(
        c: &mut WsClient,
        psk: &[u8; 32],
        device_id: &str,
        ch: &Challenge,
    ) -> (String, Option<e2e::Session>) {
        let (resp, sess, _) = finish_rekey(c, psk, device_id, ch);
        (resp, sess)
    }

    /// Drive one full handshake attempt with `psk`; see `finish_rekey`.
    fn attempt_rekey(
        c: &mut WsClient,
        psk: &[u8; 32],
        device_id: &str,
    ) -> (String, Option<e2e::Session>, Option<[u8; 32]>) {
        let ch = begin(c, device_id);
        finish_rekey(c, psk, device_id, &ch)
    }

    /// Drive one full handshake attempt with `psk`. Returns the server's final
    /// `hs_ok`/`hs_err` payload and, on success, the client-side session.
    fn attempt(
        c: &mut WsClient,
        psk: &[u8; 32],
        device_id: &str,
    ) -> (String, Option<e2e::Session>) {
        let ch = begin(c, device_id);
        finish(c, psk, device_id, &ch)
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
        // the phone app's explicit non-web origin upgrades (101); a browser's
        // "null" origin (sandboxed iframe, file://) is still refused
        for (origin, status) in [("chernobyl://app", "101"), ("null", "403")] {
            let mut s =
                std::net::TcpStream::connect(format!("127.0.0.1:{}", f.handle.port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let req = format!(
                "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n\
                 Sec-WebSocket-Version: 13\r\nOrigin: {origin}\r\n\r\n"
            );
            s.write_all(req.as_bytes()).unwrap();
            let mut resp = String::new();
            while !resp.contains("\r\n") {
                let n = s.read(&mut buf).unwrap();
                assert!(n > 0, "connection closed before a status line");
                resp.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            assert!(
                resp.starts_with(&format!("HTTP/1.1 {status}")),
                "{origin}: {resp}"
            );
        }
        // control: the identical upgrade without Origin still connects
        let mut c = connect(&f);
        c.send_text(r#"{"type":"ping","commandId":"d1-ping"}"#)
            .unwrap();
        assert!(c.next_text().unwrap().contains("pong"));
        shutdown(f);
    }

    /// Security pass 2026-11-15 (N1): a request a tunnel or reverse proxy
    /// stamped is refused when there is no token to hold it to.
    #[test]
    fn proxied_request_without_a_token_is_refused_403() {
        let f = fixture("n1-notoken", now_ms() + 60_000, false);
        let port = f.handle.port;
        for h in [
            "X-Forwarded-For: 203.0.113.9",
            "X-Forwarded-Host: abc.ngrok-free.app",
            "X-Forwarded-Proto: https",
            "Forwarded: for=203.0.113.9",
            "ngrok-skip-browser-warning: 1",
        ] {
            let st = raw_status(
                port,
                &format!(
                    "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n{h}\r\nConnection: close\r\n\r\n"
                ),
            );
            assert!(st.contains("403"), "{h}: {st}");
        }
        let err = WsClient::connect_with_headers(
            &format!("127.0.0.1:{port}"),
            "/",
            None,
            &[("X-Forwarded-For", "203.0.113.9")],
        )
        .err()
        .expect("proxied upgrade must be refused");
        assert!(err.contains("403"), "{err}");
        // control: the identical unproxied request is served
        let st = raw_status(
            port,
            "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        );
        assert!(st.contains("200"), "{st}");
        shutdown(f);
    }

    /// Security pass 2026-11-15 (N1): with a token, a proxied request on a
    /// loopback bind gets remote posture for that connection: /health needs the
    /// token and a pre-session hello returns no snapshot, while an unproxied
    /// loopback peer on the same server is unchanged.
    #[test]
    fn proxied_request_gets_remote_posture_on_a_loopback_bind() {
        let dir = temp_dir("n1-token");
        let mut c = default_config();
        c.data_dir = dir.clone();
        let companion = Arc::new(Companion::boot(c).unwrap());
        let handle = start_with_posture(
            "127.0.0.1",
            0,
            0,
            Some(REMOTE_TOKEN.into()),
            companion.clone(),
            false,
        )
        .unwrap();
        let port = handle.port;
        let addr = format!("127.0.0.1:{port}");
        let health = |extra: &str| {
            raw_status(
                port,
                &format!(
                    "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Connection: close\r\n\r\n"
                ),
            )
        };
        assert!(health("").contains("200"), "loopback /health is open");
        assert!(health("X-Forwarded-For: 203.0.113.9\r\n").contains("401"));
        let authed =
            format!("X-Forwarded-For: 203.0.113.9\r\nAuthorization: Bearer {REMOTE_TOKEN}\r\n");
        assert!(health(&authed).contains("200"));
        let hello = format!(r#"{{"type":"hello","clientId":"n1","protocol":{PROTOCOL_VERSION}}}"#);
        let mut proxied = WsClient::connect_with_headers(
            &addr,
            "/",
            Some(REMOTE_TOKEN),
            &[("ngrok-skip-browser-warning", "1")],
        )
        .unwrap();
        proxied.send_text(&hello).unwrap();
        let ack = proxied.next_text().unwrap();
        assert!(ack.contains("e2e session required"), "{ack}");
        assert!(!ack.contains("snapshot"), "{ack}");
        let mut local = WsClient::connect(&addr, "/", Some(REMOTE_TOKEN)).unwrap();
        local.send_text(&hello).unwrap();
        assert!(local.next_text().unwrap().contains("snapshot"));
        drop((proxied, local));
        let clean = handle.stop();
        companion.shutdown(clean);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Security pass 2026-11-15 (N9): two Origin headers are refused.
    #[test]
    fn request_with_two_origin_headers_is_refused_403() {
        let f = fixture("n9-origin", now_ms() + 60_000, false);
        let req = |origins: &str| {
            format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n{origins}Connection: close\r\n\r\n")
        };
        let two = req("Origin: chernobyl://app\r\nOrigin: http://evil.example\r\n");
        assert!(raw_status(f.handle.port, &two).contains("403"));
        assert!(raw_status(f.handle.port, &req("")).contains("200"));
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
                    psk_next_protected: None,
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
        // pre-session: liveness works; hello passes the version gate but
        // returns no snapshot (D9 option B) — state is session-only
        c.send_text(&format!(
            r#"{{"type":"hello","clientId":"d2","protocol":{PROTOCOL_VERSION}}}"#
        ))
        .unwrap();
        let ack = c.next_text().unwrap();
        assert!(ack.contains("e2e session required"), "got: {ack}");
        assert!(!ack.contains("snapshot"), "no pre-session state: {ack}");
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
        // and the sealed hello now returns the full snapshot
        c.send_text(&sess.seal(&format!(
            r#"{{"type":"hello","clientId":"d2","protocol":{PROTOCOL_VERSION}}}"#
        )))
        .unwrap();
        let hello = sess.open(&c.next_text().unwrap()).unwrap();
        assert!(
            hello.contains("hello_ack") && hello.contains("snapshot"),
            "got: {hello}"
        );
        shutdown(f);
    }

    /// Security review 2026-10-25: at remote posture a token-only connection
    /// that never handshakes receives NO engine event — it used to get the
    /// whole live stream (prompts, output, approvals) in plaintext — while a
    /// sessioned connection on the same server still gets every event sealed.
    #[test]
    fn remote_posture_fanout_skips_sessionless_connections() {
        let f = fixture_remote("fanout-remote");
        // the lurker holds only the token; its ping round trip proves it is
        // registered for fan-out (ahead of the phone) before the phone acts
        let mut lurker = connect_remote(&f);
        lurker
            .send_text(r#"{"type":"ping","commandId":"lurk-1"}"#)
            .unwrap();
        assert!(lurker.next_text().unwrap().contains("pong"));
        let mut phone = connect_remote(&f);
        let (resp, sess) = attempt(&mut phone, &f.psk, &f.device_id);
        assert!(resp.contains("hs_ok"), "got: {resp}");
        let mut sess = sess.unwrap();
        let create = r#"{"type":"create_session","commandId":"fan-r1","agentId":"a4","title":"Remote","prompt":"secret prompt"}"#;
        phone.send_text(&sess.seal(create)).unwrap();
        let mut phone_saw_event = false;
        for _ in 0..32 {
            let inner = sess.open(&phone.next_text().unwrap()).unwrap();
            if inner.contains("\"session_created\"") {
                phone_saw_event = true;
                break;
            }
        }
        assert!(phone_saw_event, "the sessioned phone must get the event");
        // one fan-out pass serves every registered connection, lurker first:
        // a leaked event would already sit ahead of this pong in its FIFO
        lurker
            .send_text(r#"{"type":"ping","commandId":"lurk-2"}"#)
            .unwrap();
        let next = lurker.next_text().unwrap();
        assert!(
            next.contains("pong") && next.contains("lurk-2"),
            "a sessionless remote peer got: {next}"
        );
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
            ..Default::default()
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

    /// Security review 2026-10-25: /hooks/claude refuses a browser-originated
    /// POST like its sibling routes. The request is the exact shape any web
    /// page can send with no CORS preflight (text/plain).
    #[test]
    fn claude_hook_ingress_refuses_an_origin_header() {
        use std::io::Read;
        let f = fixture_claude("cc-origin");
        let body =
            r#"{"hook_event_name":"Notification","session_id":"x","message":"forged approve-me"}"#;
        let mut s =
            std::net::TcpStream::connect(format!("127.0.0.1:{}", f.handle.diagnostics_port))
                .unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let req = format!(
            "POST /hooks/claude HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://evil.example\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        let mut buf = [0u8; 512];
        while !resp.contains("\r\n") {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "connection closed before status line");
            resp.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(resp.starts_with("HTTP/1.1 403"), "got: {resp}");
        let forged = |c: &Companion| {
            c.snapshot()
                .activity
                .iter()
                .any(|a| a.text.contains("forged approve-me"))
        };
        assert!(!forged(&f.companion), "a refused hook must not surface");
        // control: the same payload without Origin is accepted and surfaces
        let diag = format!("http://127.0.0.1:{}", f.handle.diagnostics_port);
        let ok = http_request("POST", &format!("{diag}/hooks/claude"), None, Some(body)).unwrap();
        assert_eq!(ok.status, 200);
        assert!(forged(&f.companion));
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
        Outbound::new(Arc::new(crate::companion::Saturation::default()), false)
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

    /// A consumed offer with no device behind it (a redemption whose device
    /// write failed). The realistic replay, the QR scanned again after a
    /// phone paired, is `replayed_qr_after_pairing_is_refused`.
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
        // reconnect using the device registry with the same PSK (no rotation
        // ack sent, so the QR's PSK is still the device key; see below).
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

    /* ---- ADR-011 addendum: device key rotation at first use --------- */

    /// Sends the sealed `hs_rekey_ack` and waits until the server handled it.
    fn ack_rekey(c: &mut WsClient, sess: &mut e2e::Session) {
        c.send_text(&sess.seal(r#"{"type":"hs_rekey_ack"}"#))
            .unwrap();
        assert_eq!(ping_barrier(c, sess), None, "one offer per connection");
    }

    fn device(f: &Fixture) -> DeviceRecord {
        f.companion.shared.storage.lock().unwrap().devices[&f.device_id].clone()
    }

    /// Stop the server and companion, keep the data dir, boot both again.
    fn restart(f: Fixture) -> Fixture {
        let Fixture {
            companion,
            handle,
            psk,
            device_id,
            data_dir,
        } = f;
        companion.shutdown(handle.stop());
        drop(companion);
        let mut c = default_config();
        c.data_dir = data_dir.clone();
        let companion = Arc::new(Companion::boot(c).unwrap());
        let handle = start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
        Fixture {
            companion,
            handle,
            psk,
            device_id,
            data_dir,
        }
    }

    /// Row 2.2 (emulator, 2026-10-25): the QR payload stayed a credential
    /// after pairing, so a wiped or second phone scanning it again connected
    /// as the device. Now the first redemption queues a fresh device key, the
    /// phone stores and acks it, and the QR's PSK is refused from then on.
    #[test]
    fn replayed_qr_after_pairing_is_refused() {
        let f = fixture("rekey-replay", now_ms() + 60_000, false);
        let mut phone = connect(&f);
        let (resp, sess, rekey) = attempt_rekey(&mut phone, &f.psk, &f.device_id);
        assert!(resp.contains("hs_ok"), "first connect must redeem: {resp}");
        let mut sess = sess.unwrap();
        let key = rekey.expect("the first connection must be offered hs_rekey");
        assert_ne!(key, f.psk, "the device key is fresh, not the QR's");
        assert!(
            device(&f).psk_next_protected.is_some(),
            "pending until acked"
        );
        ack_rekey(&mut phone, &mut sess);
        let d = device(&f);
        assert_eq!(d.psk_next_protected, None);
        assert_eq!(unprotect_psk(&d.psk_protected).unwrap(), key, "promoted");
        // the QR replayed later: the existing terminal deny, no session
        let (resp, sess) = attempt(&mut connect(&f), &f.psk, &f.device_id);
        assert!(sess.is_none());
        assert!(resp.contains("proof verification failed"), "got: {resp}");
        // the phone that stored the device key reconnects and is offered nothing
        let (resp, sess, rekey) = attempt_rekey(&mut connect(&f), &key, &f.device_id);
        assert!(resp.contains("hs_ok") && sess.is_some(), "got: {resp}");
        assert_eq!(rekey, None);
        shutdown(f);
    }

    /// The ack is lost (the connection died after `hs_rekey`) but the phone
    /// had stored the key: its next handshake proves the rotated key, which
    /// promotes it. The phone is never locked out, and the QR is dead after.
    #[test]
    fn lost_rekey_ack_promotes_on_next_handshake_with_new_key() {
        let f = fixture("rekey-lost-ack", now_ms() + 60_000, false);
        let (_, _, rekey) = attempt_rekey(&mut connect(&f), &f.psk, &f.device_id);
        let key = rekey.expect("the first connection must be offered hs_rekey");
        let (resp, sess, rekey) = attempt_rekey(&mut connect(&f), &key, &f.device_id);
        assert!(
            resp.contains("hs_ok") && sess.is_some(),
            "stored key: {resp}"
        );
        assert_eq!(rekey, None, "a promoted key is not offered again");
        let d = device(&f);
        assert_eq!(d.psk_next_protected, None);
        assert_eq!(unprotect_psk(&d.psk_protected).unwrap(), key);
        let (resp, sess) = attempt(&mut connect(&f), &f.psk, &f.device_id);
        assert!(sess.is_none());
        assert!(resp.contains("proof verification failed"), "got: {resp}");
        shutdown(f);
    }

    /// A phone without rotation support drops the unknown sealed frame and
    /// never acks: it keeps working on the QR's PSK and is offered the SAME
    /// key every time. A fresh mint per connection would orphan the key a
    /// phone stored before its ack was lost.
    #[test]
    fn client_ignoring_rekey_keeps_working_and_is_offered_the_same_key() {
        let f = fixture("rekey-ignored", now_ms() + 60_000, false);
        let (r1, s1, k1) = attempt_rekey(&mut connect(&f), &f.psk, &f.device_id);
        let queued = device(&f).psk_next_protected;
        let (r2, s2, k2) = attempt_rekey(&mut connect(&f), &f.psk, &f.device_id);
        assert!(r1.contains("hs_ok") && r2.contains("hs_ok"), "{r1} / {r2}");
        // both sessions carried a sealed ping/pong (`ping_barrier`)
        assert!(s1.is_some() && s2.is_some());
        assert!(k1.is_some());
        assert_eq!(k1, k2, "the same device key on every connection");
        assert_eq!(device(&f).psk_next_protected, queued, "nothing re-minted");
        shutdown(f);
    }

    /// The pending rotation and its promotion are journaled, the key only in
    /// its DPAPI-protected form: a restart offers the same key, and after the
    /// ack and another restart only the device key connects.
    #[test]
    fn rekey_state_survives_restart() {
        let f = fixture("rekey-restart", now_ms() + 60_000, false);
        let (_, _, rekey) = attempt_rekey(&mut connect(&f), &f.psk, &f.device_id);
        let key = rekey.expect("the first connection must be offered hs_rekey");
        let f = restart(f);
        let mut phone = connect(&f);
        let (resp, sess, rekey) = attempt_rekey(&mut phone, &f.psk, &f.device_id);
        assert!(resp.contains("hs_ok"), "got: {resp}");
        assert_eq!(rekey, Some(key), "the pending key survives a restart");
        ack_rekey(&mut phone, &mut sess.unwrap());
        drop(phone);
        let f = restart(f);
        let (resp, sess) = attempt(&mut connect(&f), &f.psk, &f.device_id);
        assert!(sess.is_none(), "QR after promotion + restart: {resp}");
        assert!(resp.contains("proof verification failed"), "got: {resp}");
        let (resp, sess) = attempt(&mut connect(&f), &key, &f.device_id);
        assert!(resp.contains("hs_ok") && sess.is_some(), "got: {resp}");
        let journal = std::fs::read(f.data_dir.join("companion.journal")).unwrap();
        assert!(
            !String::from_utf8_lossy(&journal).contains(&base64::encode(&key)),
            "the plaintext device key reached the journal"
        );
        shutdown(f);
    }

    /// Revocation does not care about a pending rotation: the live session
    /// closes with 4003, neither the QR's PSK nor the queued key opens a new
    /// one, and nothing promotes the key of a revoked device.
    #[test]
    fn revoked_device_with_pending_rekey_is_refused() {
        let f = fixture("rekey-revoked", now_ms() + 60_000, false);
        let mut phone = connect(&f);
        let (_, _, rekey) = attempt_rekey(&mut phone, &f.psk, &f.device_id);
        let key = rekey.expect("the first connection must be offered hs_rekey");
        assert!(matches!(
            f.companion.revoke_device(&f.device_id),
            Ok(crate::companion::RevokeResult::Revoked { terminated: 1 })
        ));
        match phone.read_frame().unwrap() {
            Frame::Close(code, _) => assert_eq!(code, Some(CLOSE_DEVICE_REVOKED)),
            other => panic!("expected close 4003, got {other:?}"),
        }
        for psk in [f.psk, key] {
            let (resp, sess) = attempt(&mut connect(&f), &psk, &f.device_id);
            assert!(sess.is_none());
            assert!(resp.contains("device revoked"), "got: {resp}");
        }
        let mut store = f.companion.shared.storage.lock().unwrap();
        promote_next_psk(&mut store, &f.device_id).unwrap();
        assert!(store.devices[&f.device_id].psk_next_protected.is_some());
        drop(store);
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
    fn revocation_route_terminates_live_session_leaves_others_intact_and_blocks_reconnect() {
        // D4: revoking a device via the loopback diagnostics route terminates every live
        // connection belonging to that device with WebSocket close code 4003 ("device revoked"),
        // removes it from the registry, leaves other devices' live sessions untouched, and
        // blocks future handshakes.
        let dir = temp_dir("d4-live");
        let mut cfg = default_config();
        cfg.data_dir = dir.clone();
        let companion = Arc::new(Companion::boot(cfg).unwrap());
        let psk1 = [42u8; 32];
        let dev1 = "dev-revoked-1".to_string();
        let psk2 = [43u8; 32];
        let dev2 = "dev-untouched-2".to_string();
        let prot1 = base64::encode(&crate::ffi::dpapi_protect(&psk1).unwrap());
        let prot2 = base64::encode(&crate::ffi::dpapi_protect(&psk2).unwrap());
        {
            let mut store = companion.shared.storage.lock().unwrap();
            store
                .put_device(&DeviceRecord {
                    device_id: dev1.clone(),
                    name: "phone1".into(),
                    psk_protected: prot1,
                    psk_next_protected: None,
                    paired_at: now_ms(),
                    revoked: false,
                })
                .unwrap();
            store
                .put_device(&DeviceRecord {
                    device_id: dev2.clone(),
                    name: "phone2".into(),
                    psk_protected: prot2,
                    psk_next_protected: None,
                    paired_at: now_ms(),
                    revoked: false,
                })
                .unwrap();
        }
        let handle = start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
        let f = Fixture {
            companion,
            handle,
            psk: psk1,
            device_id: dev1.clone(),
            data_dir: dir,
        };

        // 1. Client 1 (dev1) handshakes into a live E2E session
        let mut c1 = connect(&f);
        let (r1, sess1) = attempt(&mut c1, &psk1, &dev1);
        assert!(r1.contains("hs_ok"), "client 1 must connect: {r1}");
        let mut sess1 = sess1.expect("client 1 session");
        c1.send_text(&sess1.seal(r#"{"type":"ping","commandId":"p1"}"#))
            .unwrap();
        let pong1 = sess1.open(&c1.next_text().unwrap()).unwrap();
        assert!(pong1.contains("pong"));

        // 2. Client 2 (dev2) handshakes into a live E2E session
        let mut c2 = connect(&f);
        let (r2, sess2) = attempt(&mut c2, &psk2, &dev2);
        assert!(r2.contains("hs_ok"), "client 2 must connect: {r2}");
        let mut sess2 = sess2.expect("client 2 session");
        c2.send_text(&sess2.seal(r#"{"type":"ping","commandId":"p2"}"#))
            .unwrap();
        let pong2 = sess2.open(&c2.next_text().unwrap()).unwrap();
        assert!(pong2.contains("pong"));

        // Both clients are in the registry
        assert_eq!(f.companion.shared.clients.lock().unwrap().len(), 2);

        // 3. Revoke dev1 via POST /devices/revoke on diagnostics listener
        let diag_url = format!(
            "http://127.0.0.1:{}/devices/revoke",
            f.handle.diagnostics_port
        );
        let req_body = format!("{{\"deviceId\":\"{dev1}\"}}");
        let resp = control_post(&diag_url, f.companion.control_key(), &req_body).unwrap();
        assert_eq!(resp.status, 200, "revoke response: {resp:?}");
        assert!(
            resp.body.contains("\"revoked\":true"),
            "body: {}",
            resp.body
        );
        assert!(
            resp.body.contains("\"terminated\":1"),
            "body: {}",
            resp.body
        );

        // 4. Client 1 receives Close 4003 with reason "device revoked"
        let frame = c1.read_frame().unwrap();
        match frame {
            Frame::Close(code, reason) => {
                assert_eq!(code, Some(CLOSE_DEVICE_REVOKED));
                assert_eq!(reason, "device revoked");
            }
            other => panic!("expected close 4003, got {other:?}"),
        }

        // 5. Client 1 is removed from registry; Client 2 is untouched
        let clients = f.companion.shared.clients.lock().unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].1.device_id().as_deref(), Some(dev2.as_str()));
        drop(clients);

        // Client 2 session still works (ping/pong)
        c2.send_text(&sess2.seal(r#"{"type":"ping","commandId":"p2-after"}"#))
            .unwrap();
        let pong2_after = sess2.open(&c2.next_text().unwrap()).unwrap();
        assert!(pong2_after.contains("pong"));

        // 6. Device 1 is marked revoked in store with audit record
        {
            let store = f.companion.shared.storage.lock().unwrap();
            assert!(store.devices[&dev1].revoked);
            let has_audit = store
                .audit
                .iter()
                .any(|a| a.kind == "revoke" && a.text.contains(&dev1));
            assert!(has_audit, "durable audit log must record revocation");
        }

        // 7. Client 1 reconnect attempt is refused (hs_err device revoked)
        let mut c1_reconnect = connect(&f);
        let (r1_rec, s1_rec) = attempt(&mut c1_reconnect, &psk1, &dev1);
        assert!(
            s1_rec.is_none(),
            "revoked device must not establish session"
        );
        assert!(r1_rec.contains("device revoked"), "got: {r1_rec}");

        shutdown(f);
    }

    #[test]
    fn revocation_route_rejects_origin_header_with_403() {
        use std::io::Read;
        let f = fixture_paired("d4-origin", false);
        let mut s =
            std::net::TcpStream::connect(format!("127.0.0.1:{}", f.handle.diagnostics_port))
                .unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let body = format!("{{\"deviceId\":\"{}\"}}", f.device_id);
        // a valid control key: the 403 must come from the Origin rule alone
        let req = format!(
            "POST /devices/revoke HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://evil.example\r\n{CONTROL_KEY_HEADER}: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            f.companion.control_key(),
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();

        let mut resp = String::new();
        let mut buf = [0u8; 512];
        while !resp.contains("\r\n") {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "connection closed before status line");
            resp.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(resp.starts_with("HTTP/1.1 403"), "got: {resp}");

        // Device in store was NOT revoked
        let store = f.companion.shared.storage.lock().unwrap();
        assert!(!store.devices[&f.device_id].revoked);
        drop(store);

        shutdown(f);
    }

    #[test]
    fn revocation_route_is_absent_on_main_listener() {
        let f = fixture_paired("d4-main-absent", false);
        let main_url = format!("http://127.0.0.1:{}/devices/revoke", f.handle.port);
        let body = format!("{{\"deviceId\":\"{}\"}}", f.device_id);
        let resp = http_request("POST", &main_url, None, Some(&body)).unwrap();
        assert_eq!(
            resp.status, 404,
            "main listener must not answer /devices/revoke"
        );

        // Device in store was NOT revoked
        let store = f.companion.shared.storage.lock().unwrap();
        assert!(!store.devices[&f.device_id].revoked);
        drop(store);

        shutdown(f);
    }

    #[test]
    fn revocation_route_returns_404_for_unknown_device_and_400_for_malformed_body() {
        let f = fixture_paired("d4-errs", false);
        let diag_url = format!(
            "http://127.0.0.1:{}/devices/revoke",
            f.handle.diagnostics_port
        );
        let key = f.companion.control_key();

        // Unknown device -> 404
        let unknown_body = r#"{"deviceId":"unknown-device-xyz"}"#;
        let resp_404 = control_post(&diag_url, key, unknown_body).unwrap();
        assert_eq!(resp_404.status, 404, "got: {resp_404:?}");

        // Missing deviceId -> 400
        let bad_body = r#"{"wrongKey":"foo"}"#;
        let resp_400a = control_post(&diag_url, key, bad_body).unwrap();
        assert_eq!(resp_400a.status, 400, "got: {resp_400a:?}");

        // Empty deviceId -> 400
        let empty_body = r#"{"deviceId":""}"#;
        let resp_400b = control_post(&diag_url, key, empty_body).unwrap();
        assert_eq!(resp_400b.status, 400, "got: {resp_400b:?}");

        // Malformed JSON -> 400
        let invalid_json = "{not json";
        let resp_400c = control_post(&diag_url, key, invalid_json).unwrap();
        assert_eq!(resp_400c.status, 400, "got: {resp_400c:?}");

        shutdown(f);
    }

    #[test]
    fn pairing_offer_route_creates_offer_redeemable_by_running_server() {
        let dir = temp_dir("pairing-route-redeem");
        let mut c = default_config();
        c.data_dir = dir.clone();
        let companion = Arc::new(Companion::boot(c).unwrap());
        let handle = start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
        let diag_url = format!("http://127.0.0.1:{}/pairing/offer", handle.diagnostics_port);
        let ep = format!("ws://127.0.0.1:{}", handle.port);
        let req_body = format!(r#"{{"endpoints":["{ep}"]}}"#);

        // 1. POST /pairing/offer creates the offer in the companion's store and returns 200
        let resp = control_post(&diag_url, companion.control_key(), &req_body).unwrap();
        assert_eq!(resp.status, 200, "expected 200, got: {resp:?}");

        // 2. Parse the payload JSON string
        let payload = json::parse(&resp.body).expect("payload must be valid JSON");
        let dev = json::get_str(&payload, "dev").expect("dev must be present");
        let psk_b64 = json::get_str(&payload, "psk").expect("psk must be present");
        let psk_bytes = base64::decode(psk_b64).expect("psk must decode");
        let psk: [u8; 32] = psk_bytes.try_into().expect("psk must be 32 bytes");

        // Verify the offer is indeed in the companion's in-memory store
        {
            let store = companion.shared.storage.lock().unwrap();
            assert!(
                store.pairings.contains_key(dev),
                "offer must be in companion store"
            );
        }

        // 3. Connect to the running server and complete a real handshake
        let mut client =
            WsClient::connect(&format!("127.0.0.1:{}", handle.port), "/", None).unwrap();
        let (hs_resp, session) = attempt(&mut client, &psk, dev);
        assert!(hs_resp.contains("hs_ok"), "expected hs_ok, got: {hs_resp}");
        assert!(session.is_some(), "session must be established");

        // 4. Sealed ping round-trip proves live session works
        let mut session = session.unwrap();
        let ping = r#"{"type":"ping","commandId":"p-route-1"}"#;
        client.send_text(&session.seal(ping)).unwrap();
        let sealed_pong = client.next_text().unwrap();
        let pong = session.open(&sealed_pong).expect("pong must open");
        assert!(pong.contains("p-route-1"), "got: {pong}");

        let clean = handle.stop();
        companion.shutdown(clean);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pairing_offer_route_rejects_origin_header_with_403() {
        use std::io::Read;
        let dir = temp_dir("pairing-origin-403");
        let mut c = default_config();
        c.data_dir = dir.clone();
        let companion = Arc::new(Companion::boot(c).unwrap());
        let handle = start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
        let mut s =
            std::net::TcpStream::connect(format!("127.0.0.1:{}", handle.diagnostics_port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let body = r#"{"endpoints":["ws://127.0.0.1:8787"]}"#;
        // a valid control key: the 403 must come from the Origin rule alone
        let req = format!(
            "POST /pairing/offer HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://evil.example\r\n{CONTROL_KEY_HEADER}: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            companion.control_key(),
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();

        let mut resp = String::new();
        let mut buf = [0u8; 512];
        while !resp.contains("\r\n") {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "connection closed before status line");
            resp.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(resp.starts_with("HTTP/1.1 403"), "got: {resp}");

        let clean = handle.stop();
        companion.shutdown(clean);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pairing_offer_route_is_absent_on_main_listener() {
        let dir = temp_dir("pairing-main-404");
        let mut c = default_config();
        c.data_dir = dir.clone();
        let companion = Arc::new(Companion::boot(c).unwrap());
        let handle = start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
        let main_url = format!("http://127.0.0.1:{}/pairing/offer", handle.port);
        let req_body = r#"{"endpoints":["ws://127.0.0.1:8787"]}"#;
        let resp = http_request("POST", &main_url, None, Some(req_body)).unwrap();
        assert_eq!(
            resp.status, 404,
            "main listener must return 404 for /pairing/offer, got: {resp:?}"
        );

        let clean = handle.stop();
        companion.shutdown(clean);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pairing_offer_route_returns_400_for_malformed_body() {
        let dir = temp_dir("pairing-400");
        let mut c = default_config();
        c.data_dir = dir.clone();
        let companion = Arc::new(Companion::boot(c).unwrap());
        let handle = start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
        let diag_url = format!("http://127.0.0.1:{}/pairing/offer", handle.diagnostics_port);
        let key = companion.control_key();

        // Non-JSON
        let resp = control_post(&diag_url, key, "not json").unwrap();
        assert_eq!(resp.status, 400);

        // Non-object
        let resp = control_post(&diag_url, key, "[1, 2, 3]").unwrap();
        assert_eq!(resp.status, 400);

        // Non-string in endpoints array
        let resp = control_post(&diag_url, key, r#"{"endpoints":[123]}"#).unwrap();
        assert_eq!(resp.status, 400);

        let clean = handle.stop();
        companion.shutdown(clean);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Security review 2026-10-25: any local process reaches the loopback
    /// diagnostics listener, so minting a PSK or revoking a phone takes the
    /// control key; a missing or wrong one is refused (403) and changes
    /// nothing, the right one is accepted.
    #[test]
    fn control_routes_refuse_a_missing_or_wrong_control_key() {
        let f = fixture_paired("control-key", false);
        let diag = format!("http://127.0.0.1:{}", f.handle.diagnostics_port);
        let revoke_body = format!(r#"{{"deviceId":"{}"}}"#, f.device_id);
        let wrong = "0".repeat(64);
        for (route, body) in [
            ("/pairing/offer", "{}"),
            ("/devices/revoke", revoke_body.as_str()),
        ] {
            let url = format!("{diag}{route}");
            let missing = http_request("POST", &url, None, Some(body)).unwrap();
            assert_eq!(missing.status, 403, "{route} without the key: {missing:?}");
            let refused = control_post(&url, &wrong, body).unwrap();
            assert_eq!(refused.status, 403, "{route} with a wrong key: {refused:?}");
        }
        {
            let store = f.companion.shared.storage.lock().unwrap();
            assert!(store.pairings.is_empty(), "a refused offer minted a PSK");
            assert!(!store.devices[&f.device_id].revoked);
        }
        let key = f.companion.control_key();
        let offer = control_post(&format!("{diag}/pairing/offer"), key, "{}").unwrap();
        assert_eq!(offer.status, 200, "got: {offer:?}");
        let revoke = control_post(&format!("{diag}/devices/revoke"), key, &revoke_body).unwrap();
        assert_eq!(revoke.status, 200, "got: {revoke:?}");
        shutdown(f);
    }

    /// ADR-018 end to end: the hook CLI's request becomes an engine
    /// approval the phone answers; a hook that goes away (the PC answered)
    /// leaves it `stale` and a late tap refused; a wait that runs out
    /// leaves it `expired`; with no phone connected nothing is held.
    #[test]
    fn permission_hook_round_trip_through_the_companion() {
        use crate::claude::permission_hook;
        use crate::claude_runtime::tests as cl;
        let dir = temp_dir("hook-perm");
        let (work, data) = (dir.join("work"), dir.join("data"));
        std::fs::create_dir_all(&work).unwrap();
        let mut c = default_config();
        c.data_dir = data.clone();
        c.adapters.push(crate::config::AdapterEntryConfig {
            id: "claude-1".into(),
            kind: "claude".into(),
            enabled: true,
            ..Default::default()
        });
        let mut comp = Companion::boot(c).unwrap();
        let ws =
            crate::claude_live::validate_workspace(&work.to_string_lossy(), &data, None).unwrap();
        let rig = cl::rig_with(comp.hub(), cl::fast(), 3, vec![], |cfg| {
            cfg.workspaces = vec![ws.clone()];
        });
        comp.attach_claude_live(rig.rt.clone());
        let comp = Arc::new(comp);
        let handle = start("127.0.0.1", 0, 0, None, comp.clone()).unwrap();
        let (port, key) = (handle.diagnostics_port, comp.control_key().to_string());
        let ask = format!(
            r#"{{"session_id":"5e55e55e-0000-4000-8000-000000000001","transcript_path":"x","cwd":{},"permission_mode":"default","hook_event_name":"PermissionRequest","tool_name":"Write","tool_input":{{"file_path":"a.txt","content":"hi"}}}}"#,
            json::stringify(&json::s(&ws.path))
        );
        let pending = |comp: &Companion| {
            comp.snapshot()
                .permissions
                .into_iter()
                .find(|p| p.from_pc && p.status == "pending")
        };
        let status = |comp: &Companion, id: &str| {
            comp.snapshot()
                .permissions
                .into_iter()
                .find(|p| p.id == id)
                .map(|p| p.status)
        };
        let until = |what: &str, done: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                thread::sleep(Duration::from_millis(10));
            }
        };
        // no phone connected: answered at once with nothing
        let t0 = Instant::now();
        assert_eq!(
            permission_hook(&ask, port, &key, Duration::from_secs(30)),
            None
        );
        assert!(t0.elapsed() < Duration::from_secs(3));
        assert!(pending(&comp).is_none());
        // a wrong key never reaches the runtime
        assert_eq!(
            permission_hook(&ask, port, "nope", Duration::from_secs(30)),
            None
        );

        let mut phone =
            WsClient::connect(&format!("127.0.0.1:{}", handle.port), "/", None).unwrap();
        phone.hello().unwrap();
        // 1. the phone allows
        let (a, k) = (ask.clone(), key.clone());
        let hook = thread::spawn(move || permission_hook(&a, port, &k, Duration::from_secs(30)));
        until("the request", &|| pending(&comp).is_some());
        let p = pending(&comp).unwrap();
        assert!(
            p.expires_at - now_ms() <= 32_000,
            "the hook's wait, not 24 h"
        );
        let decide = |cmd: &str, id: &str| ClientMsg::PermissionDecision {
            command_id: cmd.into(),
            permission_id: id.into(),
            decision: "approve-once".into(),
            revision: 1.0,
            option_id: Some("allow".into()),
            message: None,
            answers: vec![],
        };
        assert!(matches!(
            comp.handle_client_msg(decide("d1", &p.id)),
            Some(ServerMsg::Ack { ok: true, .. })
        ));
        let out = hook.join().unwrap().expect("a decision to print");
        assert_eq!(
            out,
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
        );
        assert_eq!(status(&comp, &p.id).as_deref(), Some("approved"));

        // 2. the hook goes away first (Claude Code took the PC's answer)
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            s,
            "POST /hooks/claude/permission?wait=30000 HTTP/1.1\r\nHost: x\r\n{CONTROL_KEY_HEADER}: {key}\r\nContent-Length: {}\r\n\r\n{ask}",
            ask.len()
        )
        .unwrap();
        until("the second request", &|| pending(&comp).is_some());
        let p = pending(&comp).unwrap();
        drop(s);
        until("stale", &|| {
            status(&comp, &p.id).as_deref() == Some("stale")
        });
        match comp.handle_client_msg(decide("d2", &p.id)) {
            Some(ServerMsg::Ack {
                ok: false,
                error: Some(e),
                ..
            }) => assert_eq!(e, crate::claude_runtime::ANSWERED_ON_PC),
            other => panic!("{other:?}"),
        }

        // 3. nobody answers: the wait runs out, the PC asks
        assert_eq!(
            permission_hook(&ask, port, &key, Duration::from_millis(1_000)),
            None
        );
        let expired = comp
            .snapshot()
            .permissions
            .into_iter()
            .filter(|p| p.from_pc && p.status == "expired")
            .count();
        assert_eq!(expired, 1);

        drop(phone);
        let clean = handle.stop();
        comp.shutdown(clean);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loopback_bind_with_require_e2e_forces_remote_posture() {
        let dir = temp_dir("require-e2e-loopback");
        let mut c = default_config();
        c.data_dir = dir.clone();
        c.server_require_e2e = true;
        let companion = Arc::new(Companion::boot(c).unwrap());
        // start() on loopback, but companion config has server_require_e2e = true
        let handle = start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
        let addr = format!("127.0.0.1:{}", handle.port);

        let mut client = WsClient::connect(&addr, "/", None).unwrap();

        // 1. Pre-session hello gets no snapshot (refused with e2e session required)
        client
            .send_text(r#"{"type":"hello","clientId":"test-e2e","protocol":2}"#)
            .unwrap();
        let hello_ack = client.next_text().unwrap();
        assert!(
            hello_ack.contains("e2e session required"),
            "pre-session hello must be refused with e2e session required, got: {hello_ack}"
        );
        assert!(
            !hello_ack.contains("snapshot"),
            "pre-session hello must NOT carry snapshot: {hello_ack}"
        );

        // 2. Plaintext command is refused with "e2e session required"
        client
            .send_text(r#"{"type":"list_state","commandId":"cmd-e2e-2"}"#)
            .unwrap();
        let cmd_ack = client.next_text().unwrap();
        assert!(
            cmd_ack.contains("e2e session required"),
            "plaintext command must be refused with e2e session required, got: {cmd_ack}"
        );
        assert!(
            cmd_ack.contains("cmd-e2e-2"),
            "commandId must survive: {cmd_ack}"
        );

        let clean = handle.stop();
        companion.shutdown(clean);
        let _ = std::fs::remove_dir_all(&dir);
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

    /* ---- security review 2026-10-25: handshake path ---------------- */

    /// hs_hello must carry the client's fresh 16-byte nonce: a missing or
    /// malformed one is an explicit hs_err (a pre-fix client fails visibly
    /// instead of handshaking without client freshness), it aborts the
    /// handshake in progress, and the connection survives.
    #[test]
    fn hs_hello_without_a_valid_cnonce_is_rejected_but_connection_survives() {
        let f = fixture("hs-cnonce", now_ms() + 60_000, false);
        let mut c = connect(&f);
        let dev = &f.device_id;
        let abandoned = begin(&mut c, dev);
        for bad in [
            format!(r#"{{"type":"hs_hello","deviceId":"{dev}"}}"#),
            format!(r#"{{"type":"hs_hello","deviceId":"{dev}","cnonce":7}}"#),
            format!(r#"{{"type":"hs_hello","deviceId":"{dev}","cnonce":"!!"}}"#),
            format!(
                r#"{{"type":"hs_hello","deviceId":"{dev}","cnonce":"{}"}}"#,
                base64::encode(&[1u8; 15])
            ),
            format!(
                r#"{{"type":"hs_hello","deviceId":"{dev}","cnonce":"{}"}}"#,
                base64::encode(&[1u8; 32])
            ),
        ] {
            c.send_text(&bad).unwrap();
            let reply = c.next_text().unwrap();
            assert!(
                reply.contains("hs_err") && reply.contains("cnonce"),
                "{bad} -> {reply}"
            );
        }
        // the rejected hello aborted the earlier challenge
        let (resp, sess) = finish(&mut c, &f.psk, dev, &abandoned);
        assert!(sess.is_none());
        assert!(resp.contains("no handshake in progress"), "got: {resp}");
        // and the same connection still completes a proper handshake
        let (resp, sess) = attempt(&mut c, &f.psk, dev);
        assert!(resp.contains("hs_ok"), "got: {resp}");
        assert!(sess.is_some());
        shutdown(f);
    }

    /// Fan-out that lands while a handshake is in flight is dropped, never
    /// queued plaintext: `hs_ok` follows `hs_challenge` directly and every
    /// frame after it is sealed (a plaintext event there used to break the
    /// phone's handshake or its first sealed open). A loopback plaintext
    /// client keeps receiving events (dev parity).
    #[test]
    fn fanout_never_reaches_a_handshaking_client_in_plaintext() {
        let f = fixture("hs-fanout", now_ms() + 60_000, false);
        let mut phone = connect(&f);
        let ch = begin(&mut phone, &f.device_id);
        let mut plain = connect(&f);
        let create = |id: &str| {
            format!(
                r#"{{"type":"create_session","commandId":"{id}","agentId":"a4","title":"Fanout","prompt":"p"}}"#
            )
        };
        plain.send_text(&create("hs-fanout-1")).unwrap();
        plain
            .next_text_matching(|t| t.contains("session_created"), Duration::from_secs(5))
            .expect("loopback parity: a plaintext client still gets events");
        // the phone registered first, so the fan-out already offered it that
        // event (mid-handshake); its next frame must still be hs_ok
        let (resp, sess) = finish(&mut phone, &f.psk, &f.device_id, &ch);
        assert!(resp.contains("hs_ok"), "expected hs_ok, got: {resp}");
        let mut sess = sess.unwrap();
        // after hs_ok every frame is sealed, and fan-out still reaches it
        plain.send_text(&create("hs-fanout-2")).unwrap();
        let mut seen = false;
        for _ in 0..32 {
            let frame = phone.next_text().unwrap();
            let inner = sess
                .open(&frame)
                .unwrap_or_else(|e| panic!("non-sealed frame after hs_ok: {e} ({frame})"));
            if inner.contains("session_created") {
                seen = true;
                break;
            }
        }
        assert!(seen, "no sealed session_created reached the phone");
        shutdown(f);
    }

    /// Revocation race: `revoke_device` persisted `revoked` and scanned live
    /// sessions BEFORE this connection installed its session, so the scan
    /// missed it. The re-check after the install must close it (4003).
    #[test]
    fn a_revocation_that_raced_the_handshake_closes_the_new_session() {
        let f = fixture_paired("revoke-race", false);
        let session = || e2e::Session::host(e2e::derive_keys(&f.psk, &[9u8; 16], &[3u8; 16]));
        // control: an unrevoked device keeps its freshly installed session
        let (live, _rx) = test_outbound();
        assert!(install_session(
            &f.companion,
            &live,
            f.device_id.clone(),
            session()
        ));
        assert!(live.has_session() && !live.is_revoked());
        // revoke_device's half: flag persisted, its scan already done
        {
            let mut store = f.companion.shared.storage.lock().unwrap();
            let mut d = store.devices.get(&f.device_id).cloned().unwrap();
            d.revoked = true;
            store.put_device(&d).unwrap();
        }
        let (raced, _rx2) = test_outbound();
        assert!(!install_session(
            &f.companion,
            &raced,
            f.device_id.clone(),
            session()
        ));
        assert!(
            raced.is_revoked() && raced.is_terminated(),
            "must close with 4003"
        );
        shutdown(f);
    }

    /// DPAPI + HMAC now run with the storage lock released, so `redeem` must
    /// decide again on the records as they are when it commits: a concurrent
    /// redemption of the same offer still opens (as the device it registered,
    /// offer consumed once, one device key queued), while a revocation, a
    /// replaced key or a rotation that landed in the meantime is refused.
    #[test]
    fn redeem_decides_again_on_records_that_moved_while_unlocked() {
        let f = fixture("redeem-again", now_ms() + 60_000, false);
        let pending = HsPending {
            device_id: f.device_id.clone(),
            nonce: [1u8; 16],
            salt: [2u8; 16],
            cnonce: [3u8; 16],
        };
        let offer = f.companion.shared.storage.lock().unwrap().pairings[&f.device_id]
            .psk_protected
            .clone();
        // two handshakes verified the same fresh offer while unlocked and each
        // minted a device key: only the first is queued, both are offered it
        let first = redeem(&f.companion, &pending, &offer, Some("minted-1".into())).unwrap();
        let second = redeem(&f.companion, &pending, &offer, Some("minted-2".into())).unwrap();
        assert_eq!(first.as_deref(), Some("minted-1"));
        assert_eq!(
            second, first,
            "a second redemption never queues a second key"
        );
        {
            let store = f.companion.shared.storage.lock().unwrap();
            assert!(store.pairings[&f.device_id].consumed);
            assert_eq!(store.devices[&f.device_id].psk_protected, offer);
        }
        assert_eq!(
            redeem(&f.companion, &pending, "replaced", None).unwrap_err(),
            "device changed during handshake"
        );
        // a proof under the queued key promotes it; the offer's PSK, verified
        // just before that, no longer counts
        assert_eq!(redeem(&f.companion, &pending, "minted-1", None), Ok(None));
        assert_eq!(
            redeem(&f.companion, &pending, &offer, None).unwrap_err(),
            "device changed during handshake"
        );
        {
            let mut store = f.companion.shared.storage.lock().unwrap();
            let mut d = store.devices.get(&f.device_id).cloned().unwrap();
            d.revoked = true;
            store.put_device(&d).unwrap();
        }
        assert_eq!(
            redeem(&f.companion, &pending, "minted-1", None).unwrap_err(),
            "device revoked"
        );
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

        let (outbound, _rx) = Outbound::new(companion.shared.saturation.clone(), false);
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

        companion.shutdown(true);
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
        let keys = e2e::derive_keys(&[7u8; 32], &[9u8; 16], &[3u8; 16]);
        let peer_keys = e2e::derive_keys(&[7u8; 32], &[9u8; 16], &[3u8; 16]);
        outbound.set_session("test-device".into(), e2e::Session::host(keys));
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
            .session
            .seal("post-shed frame");
        assert_eq!(
            peer.open(&payload)
                .expect("shed frame must not consume a counter"),
            "post-shed frame"
        );
    }

    /// Security review 2026-10-25: at remote posture an engine event (Bulk)
    /// for a connection without a session is dropped, not queued in
    /// plaintext and not a saturation teardown; replies (Control) still flow
    /// so the peer can handshake, and once the session is live events are
    /// queued sealed. Loopback posture keeps the plaintext parity path.
    #[test]
    fn remote_posture_drops_sessionless_events_but_not_replies() {
        let event = br#"{"type":"event"}"#;
        let sat = Arc::new(crate::companion::Saturation::default());
        let (remote, rx) = Outbound::new(sat.clone(), true);
        remote
            .enqueue_sealed(event.to_vec(), MsgClass::Bulk)
            .unwrap();
        assert!(rx.try_recv().is_err(), "no event without a session");
        assert!(!remote.is_terminated());
        remote
            .enqueue_sealed(b"pong".to_vec(), MsgClass::Control)
            .unwrap();
        assert_eq!(rx.try_recv().unwrap().1, b"pong".to_vec());
        let keys = e2e::derive_keys(&[7u8; 32], &[9u8; 16], &[3u8; 16]);
        remote.set_session("dev".into(), e2e::Session::host(keys.clone()));
        remote
            .enqueue_sealed(event.to_vec(), MsgClass::Bulk)
            .unwrap();
        let sealed = String::from_utf8(rx.try_recv().unwrap().1).unwrap();
        let mut peer = e2e::Session::client(keys);
        assert_eq!(peer.open(&sealed).unwrap().as_bytes(), event);

        let (local, rx) = Outbound::new(sat, false);
        local
            .enqueue_sealed(event.to_vec(), MsgClass::Bulk)
            .unwrap();
        assert_eq!(rx.try_recv().unwrap().1, event.to_vec());
    }

    #[test]
    fn saturation_counters_surface_in_health_and_metrics() {
        // Regression for P2: production overload must be readable from
        // /health and /metrics, never inferred from dropped clients.
        let dir = temp_dir("sat-obs");
        let mut c = default_config();
        c.data_dir = dir.clone();
        let companion = Companion::boot(c).unwrap();
        let (outbound, _rx) = Outbound::new(companion.shared.saturation.clone(), false);
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

        companion.shutdown(true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The 250 ms idle poll must never split a frame: a frame that trickles
    /// in slower than the poll (here 700 ms between halves) is reassembled,
    /// not desynced into a protocol violation.
    #[test]
    fn a_frame_slower_than_the_idle_poll_is_not_split() {
        let dir = temp_dir("p6-slow-frame");
        let mut cfg = default_config();
        cfg.data_dir = dir.clone();
        let companion = Arc::new(Companion::boot(cfg).unwrap());
        let srv = start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
        let mut c = WsClient::connect(&format!("127.0.0.1:{}", srv.port), "/", None).unwrap();
        c.hello().unwrap();
        let frame = ws::encode_frame(
            ws::OP_TEXT,
            br#"{"type":"ping","commandId":"slow-1"}"#,
            true,
        );
        c.send_raw(&frame[..5]).unwrap();
        thread::sleep(Duration::from_millis(700));
        c.send_raw(&frame[5..]).unwrap();
        let pong = c
            .next_text_matching(|t| t.contains("\"pong\""), Duration::from_secs(5))
            .expect("pong after a slow frame");
        assert!(pong.contains("slow-1"), "{pong}");
        drop(c);
        assert!(srv.stop());
        companion.shutdown(true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Security review 2026-10-25 (slowloris): the deadline bounds the WHOLE
    /// read. A peer trickling a byte every 20 ms never lets a read time out,
    /// which used to be the only moment the deadline was checked, so it held
    /// the slot until the 16 KiB head cap (~55 min) — or, for a POST body
    /// read with per-read timeouts, for days. Head and body are cut at it.
    #[test]
    fn a_trickling_peer_is_cut_at_the_deadline() {
        let (mut server, mut client) = tcp_pair();
        server.set_read_timeout(Some(IDLE_POLL)).unwrap();
        // never completes a head or body; capped so a regression fails in
        // seconds (EOF) instead of hanging
        let trickle = thread::spawn(move || {
            for _ in 0..250 {
                if client.write_all(b"a").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let stop = AtomicBool::new(false);
        let deadline = Duration::from_millis(500);

        let started = Instant::now();
        let head = ws::read_handshake(&mut StopAwareReader {
            inner: &mut server,
            stop: &stop,
            deadline: started + deadline,
        });
        assert!(
            matches!(&head, Err(ws::WsError::Io(e)) if e.kind() == io::ErrorKind::TimedOut),
            "got: {head:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2), "head cut late");

        let post = Handshake {
            method: "POST".into(),
            target: "/hooks/claude".into(),
            headers: vec![("content-length".into(), "65536".into())],
        };
        let started = Instant::now();
        drain_body(
            &mut StopAwareReader {
                inner: &mut server,
                stop: &stop,
                deadline: started + deadline,
            },
            &post,
        );
        assert!(started.elapsed() < Duration::from_secs(2), "body cut late");
        drop(server);
        trickle.join().unwrap();
    }

    /* ---- mandate §2: ordered shutdown (audit P6) -------------------- */

    #[test]
    fn ordered_shutdown_joins_live_ws_and_graceful_line_is_last() {
        // Regression for P6: ServerHandle::stop() must join connection threads
        // and companion.shutdown() must write the graceful line ONLY when all
        // threads joined cleanly; no further appends may occur after it.
        let dir = temp_dir("p6-ordered-shutdown");
        let mut c = default_config();
        c.data_dir = dir.clone();
        let companion = Arc::new(Companion::boot(c).unwrap());
        let srv = start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
        let addr = format!("127.0.0.1:{}", srv.port);

        // Open a live WS connection
        let mut client = WsClient::connect(&addr, "/", None).unwrap();
        client.hello().unwrap();

        // Ensure the connection is established and tracked
        assert!(companion.shared.connections_active.load(Ordering::SeqCst) >= 1);

        // Stop the server: must return true (all threads joined within 5 s)
        let clean = srv.stop();
        assert!(
            clean,
            "ServerHandle::stop() must return true when threads join cleanly"
        );

        // Connection thread is gone
        assert_eq!(
            companion.shared.connections_active.load(Ordering::SeqCst),
            0,
            "connection thread must be gone after stop()"
        );

        // Shut down the companion
        companion.shutdown(clean);

        // Verify the audit log: graceful line must be the LAST record written
        let store = companion.shared.storage.lock().unwrap();
        let last_audit = store.audit.last().expect("must have audit records");
        assert_eq!(last_audit.kind, "shutdown");
        assert_eq!(last_audit.text, "companion shut down gracefully");

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /* ---- ADR-015: transcript subscriptions (step 2) ----------------- */

    /// No real runtime streams yet: tests push ops through the hub API.
    fn push_text(f: &Fixture, session: &str, id: &str, text: &str) {
        use crate::transcript::{Item, ItemKind, Op};
        let item = Item {
            id: id.into(),
            kind: ItemKind::Text,
            text: text.into(),
            ..Default::default()
        };
        f.companion.hub().push(session, Op::Item(item));
    }

    fn subscribe_json(command_id: &str, session: &str) -> String {
        format!(
            r#"{{"type":"session_subscribe","commandId":"{command_id}","sessionId":"{session}"}}"#
        )
    }

    fn transcript_subs(f: &Fixture) -> i64 {
        json::get_i64(&f.companion.health(), "transcriptSubs").unwrap()
    }

    /// Reads sealed frames until `pred` matches one (opened), failing on any
    /// frame that does not open with the session.
    fn next_sealed(
        c: &mut WsClient,
        sess: &mut e2e::Session,
        pred: impl Fn(&json::Json) -> bool,
    ) -> json::Json {
        for _ in 0..32 {
            let frame = c.next_text().unwrap();
            let inner = sess
                .open(&frame)
                .unwrap_or_else(|e| panic!("non-sealed frame: {e} ({frame})"));
            let v = json::parse(&inner).unwrap();
            if pred(&v) {
                return v;
            }
        }
        panic!("no matching sealed frame");
    }

    fn is_type(v: &json::Json, t: &str) -> bool {
        json::get_str(v, "type") == Some(t)
    }

    #[test]
    fn transcript_frames_are_sealed_for_session_clients() {
        let f = fixture_paired("tx-sealed", false);
        push_text(&f, "ses_tx", "i1", "hello from the runtime");
        let mut c = connect(&f);
        let (_r, sess) = attempt(&mut c, &f.psk, &f.device_id);
        let mut sess = sess.unwrap();
        c.send_text(&sess.seal(&subscribe_json("sub-1", "ses_tx")))
            .unwrap();
        // the ack (read loop) and the snapshot (scheduler) may come either way
        let (mut ack, mut snap) = (None, None);
        while ack.is_none() || snap.is_none() {
            let v = next_sealed(&mut c, &mut sess, |_| true);
            if is_type(&v, "ack") {
                ack = Some(v);
            } else if is_type(&v, "transcript_snapshot") {
                snap = Some(v);
            }
        }
        let (ack, snap) = (ack.unwrap(), snap.unwrap());
        assert_eq!(
            json::get(&ack, "ok"),
            Some(&json::Json::Bool(true)),
            "{ack:?}"
        );
        assert_eq!(json::get_str(&snap, "sessionId"), Some("ses_tx"));
        assert!(json::stringify(&snap).contains("hello from the runtime"));
        // live ops follow sealed too, continuing the snapshot's seq
        push_text(&f, "ses_tx", "i2", "and more");
        let live = next_sealed(&mut c, &mut sess, |v| is_type(v, "transcript"));
        let ops = json::as_arr(json::get(&live, "ops").unwrap()).unwrap();
        assert_eq!(json::get_i64(&ops[0], "seq"), Some(2));
        // the snapshot says which sessions stream: none of the demo ones here
        assert!(
            f.companion
                .snapshot()
                .sessions
                .iter()
                .all(|s| !s.transcript)
        );
        // an unknown session is refused
        c.send_text(&sess.seal(&subscribe_json("sub-2", "nope")))
            .unwrap();
        let ack = next_sealed(&mut c, &mut sess, |v| is_type(v, "ack"));
        assert_eq!(json::get_str(&ack, "error"), Some("unknown session"));
        shutdown(f);
    }

    #[test]
    fn sessionless_remote_connection_gets_no_transcript() {
        let f = fixture_remote("tx-lurker");
        push_text(&f, "ses_tx", "i1", "secret transcript text");
        // the lurker holds only the token; the ping proves it is registered
        let mut lurker = connect_remote(&f);
        lurker
            .send_text(r#"{"type":"ping","commandId":"lurk-1"}"#)
            .unwrap();
        assert!(lurker.next_text().unwrap().contains("pong"));
        let (conn, out) = f.companion.shared.clients.lock().unwrap()[0].clone();
        // what a D2 bypass would amount to: its Outbound subscribed directly
        f.companion
            .shared
            .hub
            .lock()
            .unwrap()
            .subscribe(conn, out, "ses_tx", None)
            .unwrap();
        // a sessioned phone subscribes and gets the snapshot, sealed: the
        // pump served the lurker (subscribed first) in that pass or earlier
        let mut phone = connect_remote(&f);
        let (_r, sess) = attempt(&mut phone, &f.psk, &f.device_id);
        let mut sess = sess.unwrap();
        phone
            .send_text(&sess.seal(&subscribe_json("sub-r", "ses_tx")))
            .unwrap();
        next_sealed(&mut phone, &mut sess, |v| is_type(v, "transcript_snapshot"));
        // D9: nothing was queued for the sessionless connection
        lurker
            .send_text(r#"{"type":"ping","commandId":"lurk-2"}"#)
            .unwrap();
        let next = lurker.next_text().unwrap();
        assert!(
            next.contains("pong") && next.contains("lurk-2"),
            "a sessionless remote peer got: {next}"
        );
        shutdown(f);
    }

    #[test]
    fn subscribe_requires_session_at_remote_posture() {
        let f = fixture_remote("tx-d2");
        push_text(&f, "ses_tx", "i1", "text");
        let mut c = connect_remote(&f);
        c.send_text(&subscribe_json("sub-d2", "ses_tx")).unwrap();
        let refusal = c.next_text().unwrap();
        assert!(refusal.contains("e2e session required"), "got: {refusal}");
        assert!(
            refusal.contains("sub-d2"),
            "commandId must survive: {refusal}"
        );
        assert_eq!(transcript_subs(&f), 0);
        // after the handshake the same subscribe is accepted
        let (_r, sess) = attempt(&mut c, &f.psk, &f.device_id);
        let mut sess = sess.unwrap();
        c.send_text(&sess.seal(&subscribe_json("sub-d2b", "ses_tx")))
            .unwrap();
        let ack = next_sealed(&mut c, &mut sess, |v| is_type(v, "ack"));
        assert_eq!(json::get(&ack, "ok"), Some(&json::Json::Bool(true)));
        assert_eq!(transcript_subs(&f), 1);
        shutdown(f);
    }

    /// ADR-016: workspace reads need the E2E session like every live
    /// message; once sealed, an unknown workspace is refused by the ack.
    #[test]
    fn workspace_reads_require_session_at_remote_posture() {
        let f = fixture_remote("ws-d2");
        let mut c = connect_remote(&f);
        let list = r#"{"type":"fs_list","commandId":"fs-d2","workspaceId":"000000000000"}"#;
        c.send_text(list).unwrap();
        let refusal = c.next_text().unwrap();
        assert!(
            refusal.contains("e2e session required") && refusal.contains("fs-d2"),
            "got: {refusal}"
        );
        let (_r, sess) = attempt(&mut c, &f.psk, &f.device_id);
        let mut sess = sess.unwrap();
        c.send_text(&sess.seal(list)).unwrap();
        let ack = next_sealed(&mut c, &mut sess, |v| is_type(v, "ack"));
        assert_eq!(json::get(&ack, "ok"), Some(&json::Json::Bool(false)));
        assert_eq!(json::get_str(&ack, "error"), Some("unknown workspace"));
        shutdown(f);
    }

    /// ADR-020: `terminal.v1` is advertised only when enabled, and even on
    /// loopback a run needs a live E2E session (a paired device).
    #[test]
    fn terminal_is_advertised_when_enabled_and_needs_a_session() {
        let hello = format!(r#"{{"type":"hello","clientId":"c1","protocol":{PROTOCOL_VERSION}}}"#);
        let run = r#"{"type":"terminal_run","commandId":"t-1","workspaceId":"000000000000","command":"echo hi"}"#;
        let off = fixture_paired("term-off", false);
        let mut c = connect(&off);
        c.send_text(&hello).unwrap();
        let ack = c
            .next_text_matching(|t| t.contains("hello_ack"), Duration::from_secs(5))
            .unwrap();
        assert!(!ack.contains("terminal.v1"), "{ack}");
        c.send_text(run).unwrap();
        let refusal = c
            .next_text_matching(|t| t.contains("t-1"), Duration::from_secs(5))
            .unwrap();
        assert!(refusal.contains("terminal is off"), "{refusal}");
        shutdown(off);

        let f = fixture_paired_with("term-on", false, |c| c.terminal_enabled = true);
        let mut c = connect(&f);
        c.send_text(&hello).unwrap();
        let ack = c
            .next_text_matching(|t| t.contains("hello_ack"), Duration::from_secs(5))
            .unwrap();
        assert!(ack.contains("terminal.v1"), "{ack}");
        // loopback, token only, no session: refused
        c.send_text(run).unwrap();
        let refusal = c
            .next_text_matching(|t| t.contains("t-1"), Duration::from_secs(5))
            .unwrap();
        assert!(refusal.contains("paired device"), "{refusal}");
        let (_r, sess) = attempt(&mut c, &f.psk, &f.device_id);
        let mut sess = sess.unwrap();
        c.send_text(&sess.seal(&run.replace("t-1", "t-2"))).unwrap();
        let ack = next_sealed(&mut c, &mut sess, |v| is_type(v, "ack"));
        // past the gate: the workspace check answers
        assert_eq!(json::get_str(&ack, "error"), Some("unknown workspace"));
        c.send_text(
            &sess
                .seal(r#"{"type":"terminal_stop","commandId":"t-3","workspaceId":"000000000000"}"#),
        )
        .unwrap();
        let ack = next_sealed(&mut c, &mut sess, |v| is_type(v, "ack"));
        assert_eq!(
            json::get_str(&ack, "error"),
            Some("nothing is running in this workspace")
        );
        shutdown(f);
    }

    #[test]
    fn teardown_drops_subscriptions() {
        let f = fixture("tx-teardown", now_ms() + 60_000, false);
        push_text(&f, "ses_tx", "i1", "text");
        let mut c = connect(&f);
        c.send_text(&subscribe_json("sub-t", "ses_tx")).unwrap();
        c.next_text_matching(|t| t.contains("\"ack\""), Duration::from_secs(5))
            .unwrap();
        // re-subscribing on the same connection only moves the cursor
        c.send_text(&subscribe_json("sub-t2", "ses_tx")).unwrap();
        c.next_text_matching(|t| t.contains("sub-t2"), Duration::from_secs(5))
            .unwrap();
        assert_eq!(transcript_subs(&f), 1);
        let _ = c.close(1000);
        drop(c);
        let deadline = Instant::now() + Duration::from_secs(5);
        while transcript_subs(&f) != 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            transcript_subs(&f),
            0,
            "ConnGuard teardown kept a subscription"
        );
        let health = f.companion.health();
        assert_eq!(json::get_i64(&health, "transcriptSessions"), Some(1));
        assert_eq!(json::get_i64(&health, "runtimeMalformed"), Some(0));
        shutdown(f);
    }

    #[test]
    fn revoked_device_loses_its_subscriptions() {
        let f = fixture_paired("tx-revoke", false);
        push_text(&f, "ses_tx", "i1", "text");
        let mut c = connect(&f);
        let (_r, sess) = attempt(&mut c, &f.psk, &f.device_id);
        let mut sess = sess.unwrap();
        c.send_text(&sess.seal(&subscribe_json("sub-v", "ses_tx")))
            .unwrap();
        next_sealed(&mut c, &mut sess, |v| is_type(v, "ack"));
        assert_eq!(transcript_subs(&f), 1);
        let url = format!(
            "http://127.0.0.1:{}/devices/revoke",
            f.handle.diagnostics_port
        );
        let body = format!(r#"{{"deviceId":"{}"}}"#, f.device_id);
        let resp = control_post(&url, f.companion.control_key(), &body).unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        // gone before the route answered, not at the next teardown
        assert_eq!(transcript_subs(&f), 0);
        loop {
            match c.read_frame().unwrap() {
                Frame::Close(code, _) => {
                    assert_eq!(code, Some(CLOSE_DEVICE_REVOKED));
                    break;
                }
                // a snapshot queued before the revoke, or a heartbeat
                Frame::Text(_) | Frame::Ping(_) | Frame::Pong(_) => continue,
                other => panic!("expected close 4003, got {other:?}"),
            }
        }
        shutdown(f);
    }

    /* ---- mandate §2: connection caps (audit P7) --------------------- */

    #[test]
    fn connection_cap_rejects_excess_and_accepts_after_disconnect() {
        // Regression for P7: bound on concurrent connections per listener.
        // Cap=2, third concurrent connection is closed immediately and
        // connectionsRejected is 1; after closing one, a new one is accepted.
        let dir = temp_dir("p7-conn-cap");
        let mut c = default_config();
        c.data_dir = dir.clone();
        let companion = Arc::new(Companion::boot(c).unwrap());
        let srv =
            start_with_caps("127.0.0.1", 0, 0, None, companion.clone(), false, 2, 64).unwrap();
        let addr = format!("127.0.0.1:{}", srv.port);

        // Connect 1
        let mut c1 = WsClient::connect(&addr, "/", None).unwrap();
        c1.hello().unwrap();

        // Connect 2
        let mut c2 = WsClient::connect(&addr, "/", None).unwrap();
        c2.hello().unwrap();

        // Both active
        assert_eq!(
            companion.shared.connections_active.load(Ordering::SeqCst),
            2
        );
        assert_eq!(
            companion.shared.connections_rejected.load(Ordering::SeqCst),
            0
        );

        // Third connection exceeds cap=2
        let rejected = match WsClient::connect(&addr, "/", None) {
            Ok(mut c3) => c3.hello().is_err(),
            Err(_) => true,
        };
        assert!(
            rejected,
            "third connection must be closed/rejected due to cap"
        );

        assert_eq!(
            companion.shared.connections_rejected.load(Ordering::SeqCst),
            1
        );

        // Verify /health and /metrics report the counters
        let health = companion.health();
        let get = |k: &str| json::as_i64(json::get(&health, k).expect(k)).expect(k);
        assert_eq!(get("connectionsActive"), 2);
        assert_eq!(get("connectionsRejected"), 1);

        let metrics = companion.metrics_text();
        assert!(metrics.contains("chernobyl_connections_active 2"));
        assert!(metrics.contains("chernobyl_connections_rejected_total 1"));

        // Close connection 1
        let _ = c1.close(1000);
        drop(c1);

        // Wait for connection 1 to finish and active count to decrement
        let start = std::time::Instant::now();
        while companion.shared.connections_active.load(Ordering::SeqCst) > 1 {
            if start.elapsed() > Duration::from_secs(2) {
                panic!("timed out waiting for active connections to drop to 1");
            }
            thread::sleep(Duration::from_millis(20));
        }

        // Now connection 4 can be accepted
        let mut c4 = WsClient::connect(&addr, "/", None).unwrap();
        c4.hello().unwrap();
        assert_eq!(
            companion.shared.connections_active.load(Ordering::SeqCst),
            2
        );

        let clean = srv.stop();
        assert!(clean);
        companion.shutdown(clean);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
