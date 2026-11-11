//! ADR-018 live check (ignored; costs a Haiku turn, under $0.03): a real
//! `claude -p` in a temp workspace asks to Write, its `PermissionRequest`
//! command hook is this crate's binary (`--hook permission-request`), the
//! companion turns it into an approval, a "phone" allows it, and the file
//! is written.
//!
//! The hook comes from a temp settings file passed with `--settings` (made
//! by `install_hook`), not from a temp `CLAUDE_CONFIG_DIR`: the CLI's
//! sign-in lives in the real config dir, and this test never copies it.
//!
//! Run: `cargo test --test live_hook -- --ignored --nocapture`.

use chernobyl_companion::claude;
use chernobyl_companion::claude_runtime::{ClaudeRuntime, LiveConfig};
use chernobyl_companion::client::WsClient;
use chernobyl_companion::companion::Companion;
use chernobyl_companion::config::{AdapterEntryConfig, config_to_json, default_config};
use chernobyl_companion::json;
use chernobyl_companion::protocol::{ClientMsg, ServerMsg, now_ms};
use chernobyl_companion::{server, storage};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[test]
#[ignore = "spawns the real Claude Code CLI (Haiku, < $0.03)"]
fn live_write_is_approved_from_the_phone_through_the_hook() {
    let root = std::env::temp_dir().join(format!("chb-live-hook-{}", now_ms()));
    let (work, data) = (root.join("work"), root.join("data"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    // the companion, holding its journal lock as the daemon does
    let _lock = storage::lock_data_dir(&data).unwrap();
    let mut c = default_config();
    c.data_dir = data.clone();
    let mut entry = AdapterEntryConfig {
        id: "claude-1".into(),
        kind: "claude".into(),
        enabled: true,
        ..Default::default()
    };
    c.adapters.push(entry.clone());
    let mut comp = Companion::boot(c.clone()).unwrap();
    entry.workspaces = vec![work.to_string_lossy().into_owned()];
    let cfg = LiveConfig::from_entry(&entry, &data).unwrap().unwrap();
    let binary = cfg.binary.clone();
    let canonical_work = std::path::PathBuf::from(&cfg.workspaces[0].path);
    comp.attach_claude_live(Arc::new(ClaudeRuntime::new("claude-1", cfg, comp.hub())));
    let comp = Arc::new(comp);
    let handle = server::start("127.0.0.1", 0, 0, None, comp.clone()).unwrap();
    // the config the hook reads: this data dir and diagnostics port
    let mut hook_cfg = c.clone();
    hook_cfg.diagnostics_port = handle.diagnostics_port;
    let cfg_path = root.join("companion.json");
    std::fs::write(&cfg_path, json::stringify(&config_to_json(&hook_cfg))).unwrap();
    let settings = root.join("settings.json");
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_chernobyl-companion"));
    let command = claude::hook_command(exe, &cfg_path, 60).unwrap();
    claude::install_hook(&settings, &command, 60).unwrap();
    // the phone: connected, and it allows the first request from the PC
    let mut phone = WsClient::connect(&format!("127.0.0.1:{}", handle.port), "/", None).unwrap();
    phone.hello().unwrap();
    let asked = Arc::new(AtomicBool::new(false));
    let (c2, seen, done) = (
        comp.clone(),
        asked.clone(),
        Arc::new(AtomicBool::new(false)),
    );
    let stop = done.clone();
    let answerer = std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let p = c2
                .snapshot()
                .permissions
                .into_iter()
                .find(|p| p.from_pc && p.status == "pending");
            if let Some(p) = p {
                eprintln!("live: phone sees {:?} {:?}", p.title, p.command);
                let reply = c2.handle_client_msg(ClientMsg::PermissionDecision {
                    command_id: format!("d-{}", now_ms()),
                    permission_id: p.id.clone(),
                    decision: "approve-once".into(),
                    revision: p.revision as f64,
                    option_id: Some("allow".into()),
                    message: None,
                    answers: vec![],
                });
                assert!(
                    matches!(reply, Some(ServerMsg::Ack { ok: true, .. })),
                    "{reply:?}"
                );
                seen.store(true, Ordering::SeqCst);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let started = Instant::now();
    let out = std::process::Command::new(&binary)
        .args([
            "-p",
            "Use the Write tool to create hello.txt in the current directory containing exactly: hi",
            "--model",
            "haiku",
            "--output-format",
            "json",
            "--max-turns",
            "4",
            "--setting-sources",
            "project,local",
            "--settings",
        ])
        .arg(&settings)
        .current_dir(&canonical_work)
        .env("ANTHROPIC_MODEL", "haiku")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    done.store(true, Ordering::SeqCst);
    answerer.join().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let cost = json::parse(stdout.trim())
        .ok()
        .and_then(|v| json::get(&v, "total_cost_usd").and_then(json::as_f64));
    eprintln!(
        "live: exit {:?} in {:?}, cost {cost:?}; stderr: {}",
        out.status.code(),
        started.elapsed(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        asked.load(Ordering::SeqCst),
        "the hook reached the phone: {stdout}"
    );
    let written = std::fs::read_to_string(canonical_work.join("hello.txt")).unwrap_or_default();
    assert_eq!(written.trim(), "hi", "{stdout}");
    drop(phone);
    let clean = handle.stop();
    comp.shutdown(clean);
    drop(_lock);
    let _ = std::fs::remove_dir_all(&root);
}
