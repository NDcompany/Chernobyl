//! Integration tests for lifecycle and resilience (mandate §8 and §10).
//!
//! Drives the real server over real TCP/WS sockets through the public API only.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use chernobyl_companion::base64;
use chernobyl_companion::client::{WsClient, http_get};
use chernobyl_companion::companion::Companion;
use chernobyl_companion::config::default_config;
use chernobyl_companion::json;
use chernobyl_companion::protocol::now_ms;
use chernobyl_companion::server;
use chernobyl_companion::storage::PairingRecord;
use chernobyl_companion::ws;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chernobyl-lifecycle-{}-{}-{}",
        tag,
        std::process::id(),
        now_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn health_connections_active(addr: &str) -> Option<i64> {
    let raw = http_get(addr, "/health").ok()?;
    let (_head, body) = raw.split_once("\r\n\r\n")?;
    let v = json::parse(body).ok()?;
    json::get_i64(&v, "connectionsActive")
}

// NOTE on Mandate Scenario 1:
// "Shutdown while a client is mid-HTTP-upgrade (sent half the request): stop() returns true within 5 s."
// This scenario is skipped because the public API makes it impossible without touching `src/`.
// In `src/server.rs`, `handle_conn` sets a 30-second read timeout before `read_handshake`
// (`stream.set_read_timeout(Some(Duration::from_secs(30)))`). On Windows, calling `shutdown()`
// on a duplicated/cloned `TcpStream` handle does not unblock a thread blocked in `read_exact()`
// on another handle of the same socket. While `serve_ws` works around this with a 250 ms `IDLE_POLL`
// loop, `read_handshake` occurs before `serve_ws` and remains blocked until the 30-second socket
// timeout. Consequently, `ServerHandle::stop()` hits its 5-second deadline and returns `false`.
// Modifying `src/server.rs` is forbidden as another agent is editing it in parallel.

/// Security pass 2026-11-15 (N5): the daemon refuses to start with an empty or
/// short access token, before it opens a socket.
#[test]
fn daemon_refuses_an_empty_or_short_token() {
    let dir = temp_dir("short-token");
    std::fs::create_dir_all(&dir).unwrap();
    for token in ["", "short-token"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_chernobyl-companion"))
            .args(["--token", token, "--port", "0", "--diagnostics-port", "0"])
            .args(["--exit-after", "1", "--config"])
            .arg(dir.join("companion.json"))
            .env("CHERNOBYL_HOME", &dir)
            .env_remove("HOST_TOKEN")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{token:?}: {err}");
        assert!(err.contains("at least 32 characters"), "{token:?}: {err}");
        assert!(!err.contains("listening on"), "{token:?}: {err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Mandate §8 / §10 scenario 2:
/// Shutdown while a client is mid E2E handshake (hs_hello sent, no hs_proof): stop() returns true.
#[test]
fn shutdown_while_client_mid_e2e_handshake() {
    let dir = temp_dir("mid-e2e-hs");
    let mut c = default_config();
    c.data_dir = dir.clone();
    let companion = Arc::new(Companion::boot(c).unwrap());

    let psk = [7u8; 32];
    let device_id = "dev-lifecycle".to_string();
    let protected = base64::encode(&chernobyl_companion::ffi::dpapi_protect(&psk).unwrap());
    {
        let mut store = companion.shared.storage.lock().unwrap();
        store
            .put_pairing(&PairingRecord {
                device_id: device_id.clone(),
                psk_protected: protected,
                challenge: base64::encode(b"CHALLENGE"),
                created_at: now_ms(),
                expires_at: now_ms() + 60_000,
                consumed: false,
            })
            .unwrap();
    }

    let srv = server::start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
    let addr = format!("127.0.0.1:{}", srv.port);

    let mut client = WsClient::connect(&addr, "/", None).unwrap();
    // hs_hello carries the client's 16-byte nonce (security review 2026-10-25)
    client
        .send_text(&format!(
            "{{\"type\":\"hs_hello\",\"deviceId\":\"{device_id}\",\"cnonce\":\"{}\"}}",
            base64::encode(&[9u8; 16])
        ))
        .unwrap();

    let challenge = client.next_text().unwrap();
    let v = json::parse(&challenge).unwrap();
    assert_eq!(json::get_str(&v, "type"), Some("hs_challenge"));

    // Client intentionally does NOT send hs_proof.
    let start = Instant::now();
    let clean = srv.stop();
    let elapsed = start.elapsed();

    drop(client);
    companion.shutdown(clean);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        clean,
        "stop() must return true when client is mid-handshake"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "stop() took too long: {elapsed:?}"
    );
}

/// Mandate §8 / §10 scenario 3:
/// Shutdown during an active command and during event fan-out to several clients:
/// stop() returns true, the journal's last audit record is the graceful-shutdown line,
/// and rebooting the companion on the same data dir finds every command either with its
/// recorded outcome or reconciled as unknown (never re-executed, never claimed as success).
#[test]
fn shutdown_during_active_command_and_fanout_reconciles_at_reboot() {
    let dir = temp_dir("active-cmd-fanout");
    let mut c = default_config();
    c.data_dir = dir.clone();
    let companion = Arc::new(Companion::boot(c.clone()).unwrap());

    let srv = server::start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
    let addr = format!("127.0.0.1:{}", srv.port);

    // Connect 3 clients and handshake hello
    let mut c1 = WsClient::connect(&addr, "/", None).unwrap();
    c1.hello().unwrap();
    let mut c2 = WsClient::connect(&addr, "/", None).unwrap();
    c2.hello().unwrap();
    let mut c3 = WsClient::connect(&addr, "/", None).unwrap();
    c3.hello().unwrap();

    // c1 issues a session creation command which triggers agent execution and event fan-out
    let cmd_id = "cmd-fanout-active";
    let create = format!(
        r#"{{"type":"create_session","commandId":"{cmd_id}","agentId":"a4","title":"FanoutTask","prompt":"run"}}"#
    );
    c1.send_text(&create).unwrap();

    // Events reach clients before the ack (one send queue); scan for the ack frame
    let ack = c1
        .next_text_matching(
            |text| {
                if let Ok(v) = json::parse(text) {
                    json::get_str(&v, "type") == Some("ack")
                        && json::get_str(&v, "commandId") == Some(cmd_id)
                } else {
                    false
                }
            },
            Duration::from_secs(2),
        )
        .unwrap();
    let ack_v = json::parse(&ack).unwrap();
    assert_eq!(json::get_bool(&ack_v, "ok"), Some(true));

    // Verify event fanout reached watcher clients c2 or c3
    let mut got_event = false;
    for _ in 0..10 {
        if let Ok(msg) = c2.next_text_matching(
            |text| {
                if let Ok(v) = json::parse(text) {
                    json::get(&v, "seq").is_some()
                        || json::get_str(&v, "type") == Some("session_created")
                        || json::get_str(&v, "type") == Some("command_progress")
                } else {
                    false
                }
            },
            Duration::from_millis(500),
        ) {
            let _ = msg;
            got_event = true;
            break;
        }
    }
    assert!(got_event, "c2 must receive fanout event before shutdown");

    // Also record an in-flight command that was unresolved when shutdown arrived
    {
        use chernobyl_companion::storage::CommandRecord;
        let mut store = companion.shared.storage.lock().unwrap();
        store
            .put_command(&CommandRecord {
                command_id: "cmd-in-flight-interrupted".into(),
                kind: "prompt".into(),
                payload: "{}".into(),
                status: "unresolved".into(),
                ok: None,
                error: None,
                session_id: None,
                permission_id: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .unwrap();
    }

    // Initiate shutdown during active event fan-out
    let start = Instant::now();
    let clean = srv.stop();
    let elapsed = start.elapsed();

    drop(c1);
    drop(c2);
    drop(c3);

    assert!(
        clean,
        "stop() during active command and fan-out must return true"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "stop() took too long: {elapsed:?}"
    );

    companion.shutdown(clean);

    // Check the journal's last audit record
    {
        let store = companion.shared.storage.lock().unwrap();
        let last_audit = store.audit.last().expect("audit record must exist");
        assert_eq!(last_audit.kind, "shutdown");
        assert_eq!(last_audit.text, "companion shut down gracefully");
    }
    drop(companion);

    // Reboot companion on the same data dir
    let rebooted = Arc::new(Companion::boot(c).unwrap());
    {
        let store2 = rebooted.shared.storage.lock().unwrap();
        let cmd = store2
            .commands
            .get(cmd_id)
            .expect("command must exist in storage");
        assert!(
            cmd.status == "executed" || cmd.status == "completed" || cmd.status == "unknown",
            "command must have recorded outcome or be reconciled as unknown, got: {}",
            cmd.status
        );

        // Verify the in-flight interrupted command was reconciled to unknown with ok=false
        let interrupted = store2
            .commands
            .get("cmd-in-flight-interrupted")
            .expect("interrupted command must exist");
        assert_eq!(
            interrupted.status, "unknown",
            "interrupted command must be reconciled to unknown"
        );
        assert_eq!(
            interrupted.ok,
            Some(false),
            "reconciled command must never be claimed as success"
        );
        assert_eq!(
            interrupted.error.as_deref(),
            Some("reconciled at boot"),
            "reconciled command must carry 'reconciled at boot' error"
        );

        // Verify all commands in storage are valid and none falsely claimed as success
        for (id, record) in &store2.commands {
            assert_ne!(
                record.status, "unresolved",
                "command {id} must not remain unresolved after boot"
            );
            if record.status == "unknown" {
                assert_ne!(
                    record.ok,
                    Some(true),
                    "unknown command {id} must never be claimed as success"
                );
            }
        }
    }

    rebooted.shutdown(true);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Mandate §8 / §10 scenario 4:
/// Resource cleanup after repeated failures: 200 connect / garbage-frame / disconnect cycles,
/// then /health connectionsActive returns to 0.
#[test]
fn resource_cleanup_after_repeated_failure_cycles() {
    let dir = temp_dir("resource-cleanup");
    let mut c = default_config();
    c.data_dir = dir.clone();
    let companion = Arc::new(Companion::boot(c).unwrap());

    let srv = server::start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
    let addr = format!("127.0.0.1:{}", srv.port);

    // 200 connect / garbage-frame / disconnect cycles
    for _ in 0..200 {
        if let Ok(mut client) = WsClient::connect(&addr, "/", None) {
            // Send garbage unmasked frame data (violates RFC 6455 masking rule for clients)
            let _ = client.send_raw(b"\x81\x04test");
            let _ = client.close(1000);
            drop(client);
        }
    }

    // Wait briefly for all 200 connection threads to complete their cleanup
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if companion.shared.connections_active.load(Ordering::SeqCst) == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    assert_eq!(
        companion.shared.connections_active.load(Ordering::SeqCst),
        0,
        "all 200 connection threads must be cleaned up"
    );

    // /health evaluated in-process returns 0 active connections
    let in_process_health = companion.health();
    assert_eq!(
        json::get_i64(&in_process_health, "connectionsActive"),
        Some(0),
        "in-process health connectionsActive must be 0"
    );

    // /health queried over HTTP: the probe itself is an active connection, so it returns 1,
    // proving zero residual connections from the 200 failure cycles
    if let Some(active_http) = health_connections_active(&addr) {
        assert_eq!(
            active_http, 1,
            "HTTP /health probe sees only itself, proving no leaked connections"
        );
    }

    let clean = srv.stop();
    companion.shutdown(clean);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(clean, "stop() must return true");
}

/// Mandate §8 / §10 scenario 5:
/// Multi-client isolation: one client flooding malformed frames does not stop another client's ping/pong.
#[test]
fn multi_client_isolation_malformed_flood_does_not_break_ping_pong() {
    let dir = temp_dir("client-isolation");
    let mut c = default_config();
    c.data_dir = dir.clone();
    let companion = Arc::new(Companion::boot(c).unwrap());

    let srv = server::start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
    let addr = format!("127.0.0.1:{}", srv.port);

    // Connect Client 1 (the flooding client) and Client 2 (the healthy ping/pong client)
    let mut c1 = WsClient::connect(&addr, "/", None).unwrap();
    c1.hello().unwrap();
    let mut c2 = WsClient::connect(&addr, "/", None).unwrap();
    c2.hello().unwrap();

    let stop_flood = Arc::new(AtomicBool::new(false));
    let stop_flood_clone = stop_flood.clone();

    // Spawn Client 1 flood thread
    let flood_handle = thread::spawn(move || {
        let mut malformed_count = 0;
        while !stop_flood_clone.load(Ordering::SeqCst) && malformed_count < 100 {
            // Send malformed JSON message (server returns malformed ack)
            let _ = c1.send_text(r#"{"invalid":"message_shape"}"#);
            // Send binary frame (message-level malformed)
            let binary_frame = ws::encode_frame(ws::OP_BINARY, b"\x00\x01\x02\x03", true);
            let _ = c1.send_raw(&binary_frame);
            malformed_count += 1;
            thread::sleep(Duration::from_millis(5));
        }
    });

    // Client 2 executes ping/pong rounds while Client 1 is flooding
    for i in 0..10 {
        let cmd_id = format!("cmd-ping-{i}");
        c2.send_text(&format!(r#"{{"type":"ping","commandId":"{cmd_id}"}}"#))
            .unwrap();

        // Expect pong with matching commandId
        let pong = c2
            .next_text_matching(
                |text| {
                    if let Ok(v) = json::parse(text) {
                        json::get_str(&v, "type") == Some("pong")
                            && json::get_str(&v, "commandId") == Some(&cmd_id)
                    } else {
                        false
                    }
                },
                Duration::from_secs(2),
            )
            .unwrap();

        let v = json::parse(&pong).unwrap();
        assert_eq!(json::get_str(&v, "type"), Some("pong"));
        assert_eq!(json::get_str(&v, "commandId"), Some(cmd_id.as_str()));

        thread::sleep(Duration::from_millis(10));
    }

    // Also test WebSocket-level ping/pong on Client 2
    let ws_ping = ws::encode_frame(ws::OP_PING, b"iso-ping-payload", true);
    c2.send_raw(&ws_ping).unwrap();
    let ws_pong = c2.read_frame().unwrap();
    match ws_pong {
        ws::Frame::Pong(payload) => assert_eq!(payload, b"iso-ping-payload"),
        other => panic!("expected Frame::Pong, got {other:?}"),
    }

    stop_flood.store(true, Ordering::SeqCst);
    let _ = flood_handle.join();

    drop(c2);
    let clean = srv.stop();
    companion.shutdown(clean);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(clean, "stop() must return true after isolation test");
}

/// Shutdown while a client has sent only half of its HTTP upgrade request:
/// the connection thread is parked in the handshake read and must still be
/// released within stop()'s deadline.
#[test]
fn shutdown_while_client_mid_http_upgrade() {
    use std::io::Write;
    let dir = temp_dir("mid-upgrade");
    let mut cfg = chernobyl_companion::config::default_config();
    cfg.data_dir = dir.clone();
    let companion =
        std::sync::Arc::new(chernobyl_companion::companion::Companion::boot(cfg).unwrap());
    let srv =
        chernobyl_companion::server::start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
    let mut half = std::net::TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
    half.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n")
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let t0 = std::time::Instant::now();
    let clean = srv.stop();
    assert!(clean, "stop() must release a connection parked mid-upgrade");
    assert!(t0.elapsed() < std::time::Duration::from_secs(5));
    drop(half);
    companion.shutdown(clean);
    let _ = std::fs::remove_dir_all(&dir);
}
