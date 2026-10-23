//! CHERNOBYL companion (Rust) — standalone local backend binary.
//!
//! One process: protocol v2 WS server (what the mobile app already speaks)
//! plus a durable companion foundation and loopback diagnostics. Zero
//! external runtime modules; a single self-contained .exe.
//!
//! Flags (env overrides in parentheses):
//!   --config <path>          config file (CHERNOBYL_HOME picks the directory)
//!   --port <n>               WS/HTTP port (PORT), 0 = ephemeral, default 8787
//!   --bind <addr>            bind address, default 127.0.0.1
//!   --token <t>              shared secret (HOST_TOKEN)
//!   --diagnostics-port <n>   loopback diagnostics port, default 9487
//!   --exit-after <ms>        smoke mode: boot, serve, exit 0 after N ms
//!   --self-test              boot in a temp dir, exercise the stack, exit
//!   --install-autostart      start-on-login: write the HKCU Run entry and exit
//!   --uninstall-autostart    remove that entry (idempotent) and exit
//!   --pair                   create a pairing offer, print its terminal QR, exit
//!   --endpoint <ws-url>      endpoint embedded in the pairing QR (repeatable, kept in
//!                             CLI order; default ws://bind:port when none given)
//!   --devices                list the paired-device registry, exit
//!   --revoke <device-id>     revoke a paired device (blocks future E2E handshakes), exit
//!   --version                print version and exit

use chernobyl_companion::client::{WsClient, http_get};
use chernobyl_companion::companion::Companion;
use chernobyl_companion::config;
use chernobyl_companion::json;
use chernobyl_companion::{ffi, server};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exit = run(&args);
    std::process::exit(exit);
}

fn run(args: &[String]) -> i32 {
    let mut config_path: Option<String> = None;
    let mut port_override: Option<i64> = None;
    let mut bind_override: Option<String> = None;
    let mut token_override: Option<String> = None;
    let mut diag_override: Option<i64> = None;
    let mut exit_after: Option<u64> = None;
    let mut self_test = false;
    let mut install_autostart = false;
    let mut uninstall_autostart = false;
    let mut pair = false;
    let mut endpoint_override: Vec<String> = Vec::new();
    let mut list_devices = false;
    let mut revoke: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let take = |i: &mut usize| -> Option<String> {
            *i += 1;
            args.get(*i).cloned()
        };
        match flag {
            "--version" => {
                println!(
                    "chernobyl-companion {VERSION} (protocol v{})",
                    chernobyl_companion::protocol::PROTOCOL_VERSION
                );
                return 0;
            }
            "--help" | "-h" => {
                println!("chernobyl-companion {VERSION} — see source header for flags");
                return 0;
            }
            "--self-test" => self_test = true,
            "--install-autostart" => install_autostart = true,
            "--uninstall-autostart" => uninstall_autostart = true,
            "--pair" => pair = true,
            "--endpoint" => endpoint_override.extend(take(&mut i)),
            "--devices" => list_devices = true,
            "--revoke" => revoke = take(&mut i),
            "--config" => config_path = take(&mut i),
            "--port" => port_override = take(&mut i).and_then(|v| v.parse().ok()),
            "--bind" => bind_override = take(&mut i),
            "--token" => token_override = take(&mut i),
            "--diagnostics-port" => diag_override = take(&mut i).and_then(|v| v.parse().ok()),
            "--exit-after" => exit_after = take(&mut i).and_then(|v| v.parse().ok()),
            other => {
                eprintln!("unknown flag: {other}");
                return 2;
            }
        }
        i += 1;
    }

    ffi::install_shutdown_handler();

    if self_test {
        return self_test_main();
    }

    // start-on-login (PRD §6.3) — before config load: installing an entry
    // must work even with no config on disk yet
    if install_autostart || uninstall_autostart {
        return autostart_main(install_autostart, config_path);
    }

    // ---- config ------------------------------------------------------
    let (mut cfg, cfg_path, created) =
        match config::load_config(config_path.as_deref().map(std::path::Path::new)) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[companion] config error: {e}");
                eprintln!("[companion] refusing to boot with an unreadable config (fail-closed)");
                return 1;
            }
        };
    // env + flag overrides (env wins over file, flags win over env — the
    // operator is present when flags are used)
    if let Ok(v) = std::env::var("PORT")
        && let Ok(n) = v.parse()
    {
        cfg.server_port = n;
    }
    if let Ok(v) = std::env::var("HOST_TOKEN")
        && !v.is_empty()
    {
        token_override.get_or_insert(v);
    }
    if let Some(n) = port_override {
        cfg.server_port = n as u16;
    }
    if let Some(b) = bind_override {
        cfg.server_bind = b;
    }
    if let Some(n) = diag_override {
        cfg.diagnostics_port = n as u16;
    }

    // `companion pair` — create + persist a pairing offer and print its QR.
    // Runs before the token check: it never binds a socket (the QR carries the
    // endpoint the phone dials instead).
    if pair {
        return pair_main(&cfg, &endpoint_override);
    }
    // Device-registry maintenance (also socket-free).
    if list_devices {
        return devices_main(&cfg);
    }
    if let Some(device_id) = revoke.as_deref() {
        return revoke_main(&cfg, device_id);
    }

    // Secure-by-default: exposing beyond loopback requires a token.
    let public_bind = cfg.server_bind != "127.0.0.1" && cfg.server_bind != "localhost";
    if public_bind && token_override.is_none() {
        eprintln!(
            "[companion] refusing to bind {} without a token (set HOST_TOKEN or --token)",
            cfg.server_bind
        );
        return 1;
    }

    // ---- boot --------------------------------------------------------
    if created {
        eprintln!("[companion] wrote default config to {}", cfg_path.display());
    }
    let companion = match Companion::boot(cfg.clone()) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("[companion] boot failed: {e}");
            return 1;
        }
    };
    let srv = match server::start(
        &cfg.server_bind,
        cfg.server_port,
        cfg.diagnostics_port,
        token_override.clone(),
        companion.clone(),
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[companion] server start failed: {e}");
            return 1;
        }
    };
    eprintln!(
        "[companion] listening on {}:{} (protocol v{}, token auth {})",
        cfg.server_bind,
        srv.port,
        chernobyl_companion::protocol::PROTOCOL_VERSION,
        if token_override.is_some() {
            "on"
        } else {
            "OFF"
        }
    );
    eprintln!(
        "[companion] diagnostics on 127.0.0.1:{}",
        srv.diagnostics_port
    );

    // ---- run ---------------------------------------------------------
    let start = std::time::Instant::now();
    loop {
        if ffi::shutdown_requested() {
            break;
        }
        if let Some(ms) = exit_after
            && start.elapsed() >= Duration::from_millis(ms)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("[companion] shutting down");
    srv.stop();
    companion.shutdown();
    eprintln!("[companion] bye");
    0
}

/// Boots everything in a temp dir, exercises the full stack over a real
/// socket, and reports honestly. Exit 0 = all checks passed.
// Events are broadcast on the same queue before the ack — skip to the ack.
fn next_ack(c: &mut WsClient) -> Result<json::Json, String> {
    for _ in 0..32 {
        let text = c.next_text()?;
        let v = json::parse(&text).map_err(|e| e.to_string())?;
        if json::get(&v, "ok").is_some() {
            return Ok(v);
        }
    }
    Err("no ack within 32 frames".into())
}

/// The Run-key command line: quoted exe path so spaces survive, plus
/// `--config` when the operator installed from a non-default config so the
/// login instance boots the same file. Token stays env-only (HOST_TOKEN) —
/// the autostarted daemon serves loopback without one, which is the Node
/// parity mode; token-at-login arrives with the relay phase.
fn autostart_value(exe: &std::path::Path, config_path: Option<&str>) -> String {
    let mut value = format!("\"{}\"", exe.display());
    if let Some(p) = config_path {
        value.push_str(&format!(" --config \"{p}\""));
    }
    value
}

/// `companion pair` — create a pairing offer, persist it to the shared journal
/// (PSK DPAPI-protected so the on-disk record never holds it plaintext), and
/// print the terminal QR the phone scans. The running daemon redeems the offer
/// when the phone proves knowledge of the PSK during the E2E handshake.
fn pair_main(cfg: &config::CompanionConfig, endpoints: &[String]) -> i32 {
    use chernobyl_companion::{base64, pairing, protocol, qr, storage};

    // Open the same journal a running daemon uses, so the offer is visible.
    let mut store = match storage::Storage::open(&cfg.data_dir, cfg.events_cap, cfg.commands_cap) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[companion] pair: cannot open store: {e}");
            return 1;
        }
    };

    // Stable host id, created on first pairing.
    let host_id = match store.kv.get("hostId") {
        Some(h) => h.clone(),
        None => {
            let id = match ffi::csprng_bytes(8) {
                Ok(b) => b.iter().map(|x| format!("{x:02x}")).collect::<String>(),
                Err(e) => {
                    eprintln!("[companion] pair: RNG failed: {e}");
                    return 1;
                }
            };
            if let Err(e) = store.put_kv("hostId", &id) {
                eprintln!("[companion] pair: cannot persist hostId: {e}");
                return 1;
            }
            id
        }
    };

    // Fresh single-use offer.
    let offer = match pairing::PairingOffer::new(protocol::now_ms()) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[companion] pair: offer failed: {e}");
            return 1;
        }
    };

    // Protect the PSK at rest before it touches the journal.
    let psk_protected = match ffi::dpapi_protect(&offer.psk).map(|c| base64::encode(&c)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[companion] pair: cannot protect PSK: {e}");
            return 1;
        }
    };
    let rec = storage::PairingRecord {
        device_id: offer.device_id.clone(),
        psk_protected,
        challenge: base64::encode(&offer.challenge),
        created_at: offer.created_at,
        expires_at: offer.expires_at,
        consumed: false,
    };
    if let Err(e) = store.put_pairing(&rec) {
        eprintln!("[companion] pair: cannot persist offer: {e}");
        return 1;
    }

    // Endpoint the phone should dial (override for LAN reachability: a loopback
    // Endpoint list for the QR (ADR-011 §2 / S10): every --endpoint flag is
    // advertised in CLI order; with none given, the bind address stands alone
    // (bind is not dialable from a phone — pass `--endpoint ws://<lan-ip>:<port>`,
    // repeatable for LAN + tailnet + tunnel in one payload).
    let eps: Vec<String> = if endpoints.is_empty() {
        vec![format!("ws://{}:{}", cfg.server_bind, cfg.server_port)]
    } else {
        endpoints.to_vec()
    };
    let payload = offer.payload_json(&host_id, &eps);

    // The payload prints BEFORE the QR: a long endpoint list that outruns QR
    // capacity still yields a pasteable credential (QR failure = warning + exit 1).
    println!("payload: {payload}");
    match qr::encode(payload.as_bytes()) {
        Ok(q) => {
            println!("{}", qr::render_terminal(&q));
            println!();
        }
        Err(e) => {
            eprintln!(
                "[companion] pair: payload too large for QR ({e}) — paste the payload line instead"
            );
            return 1;
        }
    }
    println!(
        "device {} — valid {}s, single-use (scan with the phone)",
        offer.device_id,
        pairing::PAIRING_TTL_MS / 1000
    );
    0
}

/// `companion devices` — list the paired-device registry (id, name, paired-at,
/// state) so an operator knows what to revoke.
fn devices_main(cfg: &config::CompanionConfig) -> i32 {
    use chernobyl_companion::storage;
    let store = match storage::Storage::open(&cfg.data_dir, cfg.events_cap, cfg.commands_cap) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[companion] devices: cannot open store: {e}");
            return 1;
        }
    };
    if store.devices.is_empty() {
        println!("no paired devices");
        return 0;
    }
    for d in store.devices.values() {
        let state = if d.revoked { "revoked" } else { "active" };
        println!(
            "{}  {}  paired_at={}  {state}",
            d.device_id, d.name, d.paired_at
        );
    }
    0
}

/// `companion revoke <device-id>` — mark a paired device revoked. The E2E
/// handshake refuses revoked devices, so this cuts a lost/stolen phone off.
fn revoke_main(cfg: &config::CompanionConfig, device_id: &str) -> i32 {
    use chernobyl_companion::storage;
    let mut store = match storage::Storage::open(&cfg.data_dir, cfg.events_cap, cfg.commands_cap) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[companion] revoke: cannot open store: {e}");
            return 1;
        }
    };
    let Some(mut d) = store.devices.get(device_id).cloned() else {
        eprintln!("[companion] revoke: unknown device {device_id}");
        return 1;
    };
    if d.revoked {
        println!("device {device_id} already revoked");
        return 0;
    }
    d.revoked = true;
    if let Err(e) = store.put_device(&d) {
        eprintln!("[companion] revoke: cannot persist: {e}");
        return 1;
    }
    println!("revoked device {device_id}");
    0
}

/// Start-on-login via the HKCU Run key (PRD §6.3): user-level, no elevation,
/// reversible, and `reg.exe` instead of advapi32 FFI keeps it std-only.
/// Known limitation: the console daemon appears as a console window at login;
/// a windowless host is packaging work (Phase 8 / ADR-007).
fn autostart_main(install: bool, config_path: Option<String>) -> i32 {
    const VALUE_NAME: &str = "CHERNOBYL companion";
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    if install {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[companion] cannot resolve own exe path: {e}");
                return 1;
            }
        };
        let value = autostart_value(&exe, config_path.as_deref());
        let status = std::process::Command::new("reg")
            .args(["add", KEY, "/v", VALUE_NAME, "/d", &value, "/f"])
            .status();
        match status {
            Ok(s) if s.success() => {
                println!("[companion] start-on-login installed: {VALUE_NAME} = {value}");
                0
            }
            Ok(s) => {
                eprintln!("[companion] reg add exited with {s}");
                1
            }
            Err(e) => {
                eprintln!("[companion] reg add failed: {e}");
                1
            }
        }
    } else {
        // idempotent uninstall: absent entry is success, not an error
        let present = std::process::Command::new("reg")
            .args(["query", KEY, "/v", VALUE_NAME])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !present {
            println!("[companion] start-on-login not installed — nothing to do");
            return 0;
        }
        let status = std::process::Command::new("reg")
            .args(["delete", KEY, "/v", VALUE_NAME, "/f"])
            .status();
        match status {
            Ok(s) if s.success() => {
                println!("[companion] start-on-login removed");
                0
            }
            Ok(s) => {
                eprintln!("[companion] reg delete exited with {s}");
                1
            }
            Err(e) => {
                eprintln!("[companion] reg delete failed: {e}");
                1
            }
        }
    }
}

fn self_test_main() -> i32 {
    let mut failures: Vec<String> = Vec::new();
    let check = |name: &str, ok: bool, detail: String, failures: &mut Vec<String>| {
        if ok {
            eprintln!("  ok   {name}");
        } else {
            eprintln!("  FAIL {name}: {detail}");
            failures.push(name.to_string());
        }
    };

    let dir = std::env::temp_dir().join(format!(
        "chernobyl-selftest-{}-{}",
        std::process::id(),
        chernobyl_companion::protocol::now_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = config::default_config();
    cfg.data_dir = dir.clone();
    cfg.server_port = 0;
    cfg.diagnostics_port = 0;

    let companion = match Companion::boot(cfg.clone()) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("FAIL boot: {e}");
            return 1;
        }
    };
    let srv = match server::start(
        "127.0.0.1",
        0,
        0,
        Some("st-secret".into()),
        companion.clone(),
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("FAIL server: {e}");
            return 1;
        }
    };
    let addr = format!("127.0.0.1:{}", srv.port);

    // 1. hello round-trip
    match WsClient::connect(&addr, "/", Some("st-secret")) {
        Ok(mut c) => match c.hello() {
            Ok(last_seq) => check(
                "hello + hello_ack",
                last_seq >= 0,
                format!("lastSeq={last_seq}"),
                &mut failures,
            ),
            Err(e) => check("hello + hello_ack", false, e, &mut failures),
        },
        Err(e) => check("connect", false, e, &mut failures),
    }

    // 2. wrong token closes 4001
    {
        let result = (|| -> Result<(), String> {
            let mut c = WsClient::connect(&addr, "/", Some("wrong"))?;
            match c.read_frame() {
                Ok(chernobyl_companion::ws::Frame::Close(code, _)) => {
                    if code == Some(server::CLOSE_UNAUTHORIZED) {
                        Ok(())
                    } else {
                        Err(format!("close code {code:?}"))
                    }
                }
                other => Err(format!("expected close frame, got {other:?}")),
            }
        })();
        check(
            "missing/wrong token closes 4001",
            result.is_ok(),
            result.as_ref().err().cloned().unwrap_or_default(),
            &mut failures,
        );
    }

    // 3. version gate closes 4002
    {
        let result = (|| -> Result<(), String> {
            let mut c = WsClient::connect(&addr, "/", Some("st-secret"))?;
            c.send_text(r#"{"type":"hello","clientId":"v1","protocol":1}"#)?;
            let mut saw_mismatch_ack = false;
            loop {
                match c.read_frame() {
                    Ok(chernobyl_companion::ws::Frame::Text(p)) => {
                        let t = String::from_utf8_lossy(&p).to_string();
                        if t.contains("protocol mismatch") {
                            saw_mismatch_ack = true;
                        }
                    }
                    Ok(chernobyl_companion::ws::Frame::Close(code, _)) => {
                        if code == Some(server::CLOSE_PROTOCOL_MISMATCH) && saw_mismatch_ack {
                            return Ok(());
                        }
                        return Err(format!("close {code:?}, ack seen: {saw_mismatch_ack}"));
                    }
                    Ok(_) => {}
                    Err(e) => return Err(e),
                }
            }
        })();
        check(
            "protocol mismatch closes 4002",
            result.is_ok(),
            result.as_ref().err().cloned().unwrap_or_default(),
            &mut failures,
        );
    }

    // 4. create_session over the wire, dedupe on replay
    {
        let result = (|| -> Result<(), String> {
            let mut c = WsClient::connect(&addr, "/", Some("st-secret"))?;
            c.hello()?;
            c.send_text(r#"{"type":"create_session","commandId":"st-1","agentId":"a4","title":"Self test","prompt":"p"}"#)?;
            let v = next_ack(&mut c)?;
            if json::get(&v, "ok").and_then(json::as_bool) != Some(true) {
                return Err(format!("ack not ok: {}", json::stringify(&v)));
            }
            // replay: duplicate ack, same session
            c.send_text(r#"{"type":"create_session","commandId":"st-1","agentId":"a4","title":"Self test","prompt":"p"}"#)?;
            let v2 = next_ack(&mut c)?;
            if json::get(&v2, "duplicate").and_then(json::as_bool) != Some(true) {
                return Err(format!("expected duplicate ack: {}", json::stringify(&v2)));
            }
            let sid1 = json::get(&v, "sessionId").and_then(json::as_str);
            let sid2 = json::get(&v2, "sessionId").and_then(json::as_str);
            if sid1 != sid2 {
                return Err("session ids differ between original and duplicate".into());
            }
            Ok(())
        })();
        check(
            "create_session + idempotent replay",
            result.is_ok(),
            result.as_ref().err().cloned().unwrap_or_default(),
            &mut failures,
        );
    }

    // 5. /health endpoint
    {
        let body = http_get(&addr, "/health").unwrap_or_default();
        check(
            "/health",
            body.contains("\"ok\":true") && body.contains("\"protocol\":2"),
            format!("body: {}", &body[..body.len().min(200)]),
            &mut failures,
        );
    }

    // 6. malformed frame does not kill the server
    {
        let result = (|| -> Result<(), String> {
            let mut c = WsClient::connect(&addr, "/", Some("st-secret"))?;
            c.hello()?;
            c.send_text("not json at all")?;
            let reply = c.next_text()?;
            if !reply.contains("malformed message") {
                return Err(format!("expected malformed ack, got {reply}"));
            }
            // server still alive: new connection works
            let mut c2 = WsClient::connect(&addr, "/", Some("st-secret"))?;
            c2.hello()?;
            Ok(())
        })();
        check(
            "garbage frame → malformed ack, server survives",
            result.is_ok(),
            result.as_ref().err().cloned().unwrap_or_default(),
            &mut failures,
        );
    }

    // 7. permission decision over the wire: stale revision refused (G2
    // fail-closed), fresh revision resolves and broadcasts
    {
        let result = (|| -> Result<(), String> {
            let mut c = WsClient::connect(&addr, "/", Some("st-secret"))
                .map_err(|e| format!("connect: {e}"))?;
            c.hello().map_err(|e| format!("hello: {e}"))?;
            // seeded p1 is pending at revision 1 — a stale revision must be refused
            c.send_text(r#"{"type":"permission_decision","commandId":"st-perm-1","permissionId":"p1","decision":"approve-once","revision":2}"#)?;
            let refused = next_ack(&mut c).map_err(|e| format!("stale-decision ack: {e}"))?;
            if json::get(&refused, "ok").and_then(json::as_bool) != Some(false) {
                return Err(format!(
                    "stale revision not refused: {}",
                    json::stringify(&refused)
                ));
            }
            // fresh revision resolves; events precede the ack on the same
            // queue, so watch every frame while waiting for the ack
            c.send_text(r#"{"type":"permission_decision","commandId":"st-perm-2","permissionId":"p1","decision":"approve-once","revision":1}"#)?;
            let mut seen_resolved = false;
            let mut accepted = None;
            for _ in 0..64 {
                let text = c.next_text().map_err(|e| format!("fresh-decision: {e}"))?;
                let v = json::parse(&text).map_err(|e| e.to_string())?;
                if json::get(&v, "type").and_then(json::as_str) == Some("permission_resolved")
                    && json::get(&v, "permission")
                        .and_then(|p| json::get(p, "id"))
                        .and_then(json::as_str)
                        == Some("p1")
                {
                    seen_resolved = true;
                }
                if json::get(&v, "ok").is_some() {
                    accepted = Some(v);
                    break;
                }
            }
            let Some(accepted) = accepted else {
                return Err("no ack for fresh decision".into());
            };
            if json::get(&accepted, "ok").and_then(json::as_bool) != Some(true) {
                return Err(format!(
                    "fresh revision not accepted: {}",
                    json::stringify(&accepted)
                ));
            }
            if !seen_resolved {
                return Err("no permission_resolved event for p1".into());
            }
            Ok(())
        })();
        check(
            "permission decision: stale refused, fresh resolves",
            result.is_ok(),
            result.as_ref().err().cloned().unwrap_or_default(),
            &mut failures,
        );
    }

    // 8. events fan out to every connected client (the phone sees activity
    // it did not cause)
    {
        let result = (|| -> Result<(), String> {
            let mut watcher = WsClient::connect(&addr, "/", Some("st-secret"))?;
            watcher.hello()?;
            let mut actor = WsClient::connect(&addr, "/", Some("st-secret"))?;
            actor.hello()?;
            actor.send_text(r#"{"type":"create_session","commandId":"st-fan-1","agentId":"a4","title":"Fanout","prompt":"p"}"#)?;
            // actor finds its ack (events precede it on the same queue)
            let mut sid: Option<String> = None;
            for _ in 0..32 {
                let text = actor.next_text()?;
                let v = json::parse(&text).map_err(|e| e.to_string())?;
                if json::get(&v, "ok").is_some() {
                    sid = json::get(&v, "sessionId")
                        .and_then(json::as_str)
                        .map(str::to_string);
                    break;
                }
            }
            let Some(sid) = sid else {
                return Err("actor got no create ack".into());
            };
            // watcher sees the session_created event it did not cause
            for _ in 0..64 {
                let text = watcher.next_text()?;
                let v = json::parse(&text).map_err(|e| e.to_string())?;
                if json::get(&v, "type").and_then(json::as_str) == Some("session_created") {
                    let id = json::get(&v, "session")
                        .and_then(|s| json::get(s, "id"))
                        .and_then(json::as_str);
                    if id == Some(sid.as_str()) {
                        return Ok(());
                    }
                }
            }
            Err(format!("watcher never saw session {sid}"))
        })();
        check(
            "events fan out to other connected clients",
            result.is_ok(),
            result.as_ref().err().cloned().unwrap_or_default(),
            &mut failures,
        );
    }

    // 9. durable seq across restart
    {
        let seq_before = companion.last_seq();
        srv.stop();
        companion.shutdown();
        let companion2 = match Companion::boot(cfg.clone()) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                return {
                    eprintln!("FAIL restart boot: {e}");
                    1
                };
            }
        };
        let ok = companion2.last_seq() >= seq_before;
        check(
            "event seq durable across restart",
            ok,
            format!("before={seq_before} after={}", companion2.last_seq()),
            &mut failures,
        );
        let srv2 = server::start("127.0.0.1", 0, 0, None, companion2.clone());
        match srv2 {
            Ok(s) => s.stop(),
            Err(e) => {
                check("second boot serves", false, e, &mut failures);
            }
        }
        companion2.shutdown();
    }

    let _ = std::fs::remove_dir_all(&dir);
    if failures.is_empty() {
        eprintln!("self-test: ALL CHECKS PASSED");
        0
    } else {
        eprintln!(
            "self-test: {} CHECK(S) FAILED: {failures:?}",
            failures.len()
        );
        1
    }
}

// silence unused warnings for helpers only used by some paths
#[allow(dead_code)]
fn _unused(f: impl Fn() -> bool) -> bool {
    let _ = Ordering::SeqCst;
    f()
}

#[cfg(test)]
mod tests {
    use super::autostart_value;

    #[test]
    fn autostart_value_quotes_paths_and_appends_config() {
        assert_eq!(
            autostart_value(std::path::Path::new(r"C:\a b\c.exe"), None),
            r#""C:\a b\c.exe""#
        );
        assert_eq!(
            autostart_value(
                std::path::Path::new(r"C:\x.exe"),
                Some(r"C:\cfg dir\companion.json")
            ),
            r#""C:\x.exe" --config "C:\cfg dir\companion.json""#
        );
    }
}
