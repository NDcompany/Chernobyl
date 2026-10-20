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
//!   --version                print version and exit

use chernobyl_companion::companion::Companion;
use chernobyl_companion::config;
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
fn self_test_main() -> i32 {
    use chernobyl_companion::client::{WsClient, http_get};
    use chernobyl_companion::json;

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
        // events are broadcast on the same queue before the ack — skip to the ack
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

    // 7. durable seq across restart
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
