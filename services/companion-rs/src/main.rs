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
//!   --require-e2e            force remote E2E session posture on any bind (e.g. tunnels)
//!   --hook permission-request  ADR-018: Claude Code's PermissionRequest command hook:
//!                            stdin JSON to the running companion, waits for the phone
//!                            (--timeout <s>, default 120), prints the decision or
//!                            nothing; always exits 0
//!   --install-hooks          add that hook to ~/.claude/settings.json (backup first)
//!   --uninstall-hooks        remove it again
//!   --tui                    boot the daemon and open the terminal UI (status,
//!                            sessions, approvals, pairing, keybinds) on loopback
//!   --soak smoke|<seconds>   soak harness (soak.rs): run this exe as the server under
//!                            test and judge 10 PASS/FAIL criteria; smoke = one pass,
//!                            <seconds> = long run; --out <dir>, --inject server-kill
//!   --version                print version and exit

mod soak;

use chernobyl_companion::client::{WsClient, http_get};
use chernobyl_companion::companion::Companion;
use chernobyl_companion::config;
use chernobyl_companion::json;
use chernobyl_companion::{ffi, pairing, qr, server};
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
    let mut claude_hooks = false;
    let mut hook: Option<String> = None;
    let mut hook_timeout: Option<u64> = None;
    let mut install_hooks = false;
    let mut uninstall_hooks = false;
    let mut backup_path: Option<String> = None;
    let mut restore_path: Option<String> = None;
    let mut background = false;
    let mut tui = false;
    let mut require_e2e = false;
    let mut soak_mode: Option<String> = None;
    let mut soak_out: Option<String> = None;
    let mut soak_inject: Option<String> = None;

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
            "--claude-hooks" => claude_hooks = true,
            "--hook" => hook = Some(take(&mut i).unwrap_or_default()),
            "--timeout" => hook_timeout = take(&mut i).and_then(|v| v.parse().ok()),
            "--install-hooks" => install_hooks = true,
            "--uninstall-hooks" => uninstall_hooks = true,
            "--backup" => backup_path = take(&mut i),
            "--restore" => restore_path = take(&mut i),
            "--background" => background = true,
            "--tui" => tui = true,
            "--require-e2e" => require_e2e = true,
            // a bare --soak must not fall through to booting the daemon
            "--soak" => soak_mode = Some(take(&mut i).unwrap_or_default()),
            "--out" => soak_out = take(&mut i),
            "--inject" => soak_inject = take(&mut i),
            "--config" => config_path = take(&mut i),
            "--port" => port_override = take(&mut i).and_then(|v| v.parse().ok()),
            "--bind" => bind_override = take(&mut i),
            "--token" => token_override = take(&mut i),
            "--diagnostics-port" => diag_override = take(&mut i).and_then(|v| v.parse().ok()),
            "--exit-after" => exit_after = take(&mut i).and_then(|v| v.parse().ok()),
            // the hook never fails Claude Code's tool call over a flag
            _ if args.iter().any(|a| a == "--hook") => {}
            other => {
                eprintln!("unknown flag: {other}");
                return 2;
            }
        }
        i += 1;
    }

    // ADR-018: inside Claude Code, before anything that could write a file
    if let Some(mode) = hook {
        return hook_main(&mode, hook_timeout, config_path.as_deref());
    }

    // The soak harness drives child processes of this same exe. Dispatched
    // before the shutdown handler, so Ctrl-C ends the harness at once (as it
    // did the Node tool) while each child shuts itself down.
    if let Some(mode) = soak_mode {
        return soak::run(&mode, soak_out.as_deref(), soak_inject.as_deref());
    }
    if soak_out.is_some() || soak_inject.is_some() {
        eprintln!("--out / --inject only apply to --soak");
        return 2;
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
    if require_e2e {
        cfg.server_require_e2e = true;
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
    // `companion claude-hooks` — print the Claude Code hook-bridge snippet for
    // THIS companion's loopback diagnostics ingress (explicit install: the
    // companion never writes the user's settings.json). Socket-free.
    if claude_hooks {
        return claude_hooks_main(&cfg);
    }
    // ADR-018: written only on this explicit command, after a backup
    if install_hooks || uninstall_hooks {
        return install_hooks_main(&cfg_path, install_hooks, hook_timeout);
    }
    // Phase 8: `companion backup <path>` / `companion restore <path>` —
    // snapshot/restore the durable journal with a SHA-256 sidecar. Socket-free.
    if backup_path.is_some() || restore_path.is_some() {
        return backup_main(&cfg, backup_path.as_deref(), restore_path.as_deref());
    }

    // Secure-by-default: exposing beyond loopback requires a token, and so does
    // remote posture on loopback: --require-e2e is the tunnel recipe, where
    // loopback is reachable from the internet (security review 2026-10-25).
    let public_bind = cfg.server_bind != "127.0.0.1" && cfg.server_bind != "localhost";
    if (public_bind || cfg.server_require_e2e) && token_override.is_none() {
        let what = if public_bind {
            "bind"
        } else {
            "require E2E (--require-e2e / server.requireE2e) on"
        };
        eprintln!(
            "[companion] refusing to {what} {} without a token (set HOST_TOKEN or --token)",
            cfg.server_bind
        );
        return 1;
    }

    // ---- boot --------------------------------------------------------
    if created {
        eprintln!("[companion] wrote default config to {}", cfg_path.display());
    }
    // The journal's single writer for as long as this daemon runs (security
    // review 2026-10-25): CLI --pair/--revoke/--restore then refuse to write
    // offline, and --devices reads without touching the file.
    let _journal_lock = match chernobyl_companion::storage::lock_data_dir(&cfg.data_dir) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("[companion] refusing to boot: {e}");
            return 1;
        }
    };
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

    // Phase 8 windowless host: `--background` (used by the HKCU Run autostart)
    // closes the console window after a successful boot, so the login daemon
    // runs without flashing a console. Diagnostics stay reachable on the
    // loopback /health + /metrics routes, so nothing is lost.
    if background {
        ffi::detach_console();
    }

    // Phase 8 terminal UI (ADR-011 §5): `--tui` boots the daemon, then opens
    // an interactive operator console as a loopback client of this same server.
    // On quit (or Ctrl-C) the server and companion shut down cleanly. Built for
    // the default loopback posture — a non-loopback bind still requires the E2E
    // handshake, which the TUI surfaces verbatim rather than pretending to work.
    if tui {
        let addr = format!("127.0.0.1:{}", srv.port);
        let comp_for_tui = companion.clone();
        let endpoints = endpoint_override.clone();
        let res = chernobyl_companion::tui::run_tui(&addr, token_override.as_deref(), move || {
            comp_for_tui
                .create_pairing_offer(&endpoints)
                .map(pairing_block_from_payload)
                .map(|b| b.render())
        });
        if let Err(e) = res {
            eprintln!("[companion] tui error: {e}");
        }
        eprintln!("[companion] shutting down");
        let clean = srv.stop();
        companion.shutdown(clean);
        eprintln!("[companion] bye");
        return 0;
    }

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
    let clean = srv.stop();
    companion.shutdown(clean);
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
/// login instance boots the same file. `--background` (Phase 8) closes the
/// console window after boot so the login daemon is windowless. Token stays
/// env-only (HOST_TOKEN) — the autostarted daemon serves loopback without one,
/// which is the Node parity mode; token-at-login arrives with the relay phase.
fn autostart_value(exe: &std::path::Path, config_path: Option<&str>) -> String {
    let mut value = format!("\"{}\" --background", exe.display());
    if let Some(p) = config_path {
        value.push_str(&format!(" --config \"{p}\""));
    }
    value
}

/// A pairing offer rendered for humans: the payload line, the terminal QR (or
/// the encode error when the payload outran QR capacity), and the device line.
struct PairingBlock {
    payload: String,
    qr: Result<String, String>,
    device_line: String,
}

impl PairingBlock {
    /// Everything in one block (payload + QR/note + device line) — what the
    /// `--tui` pairing overlay shows; the CLI prints the same pieces separately
    /// so a QR failure still lands the payload on stdout and exits 1.
    fn render(&self) -> String {
        let mut out = format!("payload: {}\n", self.payload);
        match &self.qr {
            Ok(q) => {
                out.push_str(q);
                out.push('\n');
            }
            Err(_) => out.push_str("[payload too large for QR — paste the payload line above]\n"),
        }
        out.push_str(&self.device_line);
        out
    }
}

fn pairing_block_from_payload(payload: String) -> PairingBlock {
    let dev = json::parse(&payload)
        .ok()
        .as_ref()
        .and_then(|v| json::get_str(v, "dev").map(str::to_string))
        .unwrap_or_else(|| "unknown".into());
    let qr = qr::encode(payload.as_bytes())
        .map(|q| qr::render_terminal(&q))
        .map_err(|e| e.to_string());
    let device_line = format!(
        "device {} — valid {}s, single-use (scan with the phone)",
        dev,
        pairing::PAIRING_TTL_MS / 1000
    );
    PairingBlock {
        payload,
        qr,
        device_line,
    }
}

fn pair_offline(
    cfg: &config::CompanionConfig,
    endpoints: &[String],
) -> Result<PairingBlock, String> {
    use chernobyl_companion::{pairing, storage};

    // Offline only while no companion holds the journal (security review
    // 2026-10-25): a running one would never load this offer.
    let _lock = storage::lock_data_dir(&cfg.data_dir).map_err(|e| {
        format!(
            "no answer on 127.0.0.1:{} and not writing offline: {e} — no offer created",
            cfg.diagnostics_port
        )
    })?;
    let mut store = storage::Storage::open(&cfg.data_dir, cfg.events_cap, cfg.commands_cap)
        .map_err(|e| format!("cannot open store: {e}"))?;

    let eps: Vec<String> = if endpoints.is_empty() {
        vec![format!("ws://{}:{}", cfg.server_bind, cfg.server_port)]
    } else {
        endpoints.to_vec()
    };
    let payload = pairing::create_pairing_offer(&mut store, &eps)?;
    Ok(pairing_block_from_payload(payload))
}

fn print_pairing_block(b: &PairingBlock, path_msg: &str) -> i32 {
    println!("payload: {}", b.payload);
    match &b.qr {
        Ok(q) => {
            println!("{q}");
            println!();
            println!("{}", b.device_line);
            println!("{path_msg}");
            0
        }
        Err(e) => {
            eprintln!(
                "[companion] pair: payload too large for QR ({e}) — paste the payload line instead"
            );
            println!("{path_msg}");
            1
        }
    }
}

/// The running companion's loopback control key, read from the data dir it
/// booted on (security review 2026-10-25). Empty when none booted there; the
/// control routes refuse that like any wrong key.
fn control_key(cfg: &config::CompanionConfig) -> String {
    let path = cfg
        .data_dir
        .join(chernobyl_companion::companion::CONTROL_KEY_FILE);
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// `companion pair` — create a pairing offer and print the terminal QR the phone scans.
/// Tries the running companion first (loopback POST /pairing/offer); if unreachable,
/// falls back to offline journal write (restart-scoped).
fn pair_main(cfg: &config::CompanionConfig, endpoints: &[String]) -> i32 {
    // With the journal lock free no companion runs, so whatever listens on
    // the diagnostics port is not ours: never send it the control key
    // (security review 2026-10-25). The probe lock is released at once.
    if chernobyl_companion::storage::lock_data_dir(&cfg.data_dir).is_ok() {
        return match pair_offline(cfg, endpoints) {
            Ok(b) => print_pairing_block(&b, "pairing offer created (offline journal)"),
            Err(e) => {
                eprintln!("[companion] pair: {e}");
                1
            }
        };
    }
    let url = format!("http://127.0.0.1:{}/pairing/offer", cfg.diagnostics_port);
    let body = json::stringify(&json::obj(vec![(
        "endpoints",
        json::Json::Arr(endpoints.iter().map(|e| json::s(e)).collect()),
    )]));

    match server::control_post(&url, &control_key(cfg), &body) {
        Ok(resp) => match resp.status {
            200 => {
                let b = pairing_block_from_payload(resp.body);
                print_pairing_block(&b, "pairing offer created via running companion")
            }
            other => {
                eprintln!(
                    "[companion] pair: server returned HTTP {other}: {}",
                    resp.body
                );
                1
            }
        },
        Err(e) => {
            eprintln!(
                "[companion] pair: a companion holds the journal but 127.0.0.1:{} did not answer ({e}) — no offer created",
                cfg.diagnostics_port
            );
            1
        }
    }
}

/// `companion devices` — list the paired-device registry (id, name, paired-at,
/// state) so an operator knows what to revoke. Read-only (security review
/// 2026-10-25): a full open could truncate, prune and compact the journal
/// under a running companion, orphaning its append handle.
fn devices_main(cfg: &config::CompanionConfig) -> i32 {
    use chernobyl_companion::storage;
    let store = match storage::Storage::open_read_only(&cfg.data_dir) {
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

/// `companion revoke <device-id>` — mark a paired device revoked (D4).
/// First tries the running companion via loopback diagnostics POST /devices/revoke;
/// if unreachable, falls back to offline journal write (restart-scoped, unchanged).
fn revoke_main(cfg: &config::CompanionConfig, device_id: &str) -> i32 {
    // same rule as pair_main: only a companion holding the journal is asked
    if chernobyl_companion::storage::lock_data_dir(&cfg.data_dir).is_ok() {
        return revoke_offline(cfg, device_id);
    }
    let url = format!("http://127.0.0.1:{}/devices/revoke", cfg.diagnostics_port);
    let req_body = json::stringify(&json::obj(vec![("deviceId", json::s(device_id))]));
    match server::control_post(&url, &control_key(cfg), &req_body) {
        Ok(resp) => match resp.status {
            200 => {
                let terminated = json::parse(&resp.body)
                    .ok()
                    .as_ref()
                    .and_then(|v| json::get_i64(v, "terminated"))
                    .unwrap_or(0);
                println!(
                    "revoked device {device_id} via running companion (terminated {terminated} live session(s))"
                );
                0
            }
            404 => {
                eprintln!("[companion] revoke: unknown device {device_id}");
                1
            }
            other => {
                eprintln!(
                    "[companion] revoke: server returned HTTP {other}: {}",
                    resp.body
                );
                1
            }
        },
        Err(e) => {
            eprintln!(
                "[companion] revoke: a companion holds the journal but 127.0.0.1:{} did not answer ({e}) — revocation NOT applied",
                cfg.diagnostics_port
            );
            1
        }
    }
}

fn revoke_offline(cfg: &config::CompanionConfig, device_id: &str) -> i32 {
    use chernobyl_companion::storage;
    // Offline only while no companion holds the journal (security review
    // 2026-10-25): a running one never re-reads the file and its next
    // compaction erases the record, so the revocation would never apply.
    let _lock = match storage::lock_data_dir(&cfg.data_dir) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!(
                "[companion] revoke: no answer on 127.0.0.1:{} and not writing offline: {e} — revocation NOT applied",
                cfg.diagnostics_port
            );
            return 1;
        }
    };
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
        println!("device {device_id} already revoked (offline journal)");
        return 0;
    }
    d.revoked = true;
    if let Err(e) = store.put_device(&d) {
        eprintln!("[companion] revoke: cannot persist: {e}");
        return 1;
    }
    let _ = store.append_audit("revoke", &format!("device {device_id} revoked (offline)"));
    println!("revoked device {device_id} (offline journal)");
    0
}

/// `companion claude-hooks` — print the Claude Code hook-bridge snippet that
/// forwards every hook to THIS companion's loopback diagnostics ingress. The
/// operator pastes it into `~/.claude/settings.json` themselves (explicit
/// install; the companion never writes the user's settings). Socket-free.
fn claude_hooks_main(cfg: &config::CompanionConfig) -> i32 {
    let ingress = format!("http://127.0.0.1:{}/hooks/claude", cfg.diagnostics_port);
    print!("{}", chernobyl_companion::claude::bridge_config(&ingress));
    0
}

/// ADR-018 `--hook permission-request`: Claude Code's `PermissionRequest`
/// command hook. Reads the hook JSON on stdin, asks the running companion
/// (loopback diagnostics, control key) and prints its decision. Prints
/// nothing and exits 0 whenever it cannot: Claude Code then asks on the
/// PC as usual. Never writes a file, never blocks past `--timeout`.
fn hook_main(mode: &str, timeout_s: Option<u64>, config_path: Option<&str>) -> i32 {
    use std::io::Read;
    if mode != "permission-request" {
        return 0;
    }
    let mut input = String::new();
    let cap = chernobyl_companion::claude::ClaudeAdapter::MAX_HOOK_PAYLOAD_BYTES as u64 + 1;
    if std::io::stdin()
        .take(cap)
        .read_to_string(&mut input)
        .is_err()
    {
        return 0;
    }
    let Some(cfg) = config::read_config(config_path.map(std::path::Path::new)) else {
        return 0;
    };
    // the control key goes only to the companion that holds this journal
    if !companion_running(&cfg.data_dir) {
        return 0;
    }
    let wait = Duration::from_secs(
        timeout_s
            .unwrap_or(chernobyl_companion::claude::HOOK_TIMEOUT_S)
            .clamp(1, 600),
    );
    let key = control_key(&cfg);
    if let Some(out) =
        chernobyl_companion::claude::permission_hook(&input, cfg.diagnostics_port, &key, wait)
    {
        println!("{out}");
    }
    0
}

/// A companion holds `companion.lock` exclusively while it runs: opening
/// it fails with a sharing violation. Only a read handle is tried, and
/// dropped at once, so a booting companion is not kept out.
fn companion_running(data_dir: &std::path::Path) -> bool {
    match std::fs::File::open(data_dir.join("companion.lock")) {
        Ok(_) => cfg!(not(windows)),
        Err(e) => e.raw_os_error() == Some(32),
    }
}

/// ADR-018 `--install-hooks` / `--uninstall-hooks`: adds or removes exactly
/// the `PermissionRequest` hook in Claude Code's user `settings.json`
/// (`CLAUDE_CONFIG_DIR` or `%USERPROFILE%\.claude`), backing the file up
/// first; invalid JSON is refused untouched.
fn install_hooks_main(cfg_path: &std::path::Path, install: bool, timeout_s: Option<u64>) -> i32 {
    use chernobyl_companion::claude;
    let Some(settings) = claude::settings_path(
        std::env::var_os("CLAUDE_CONFIG_DIR").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
    ) else {
        eprintln!("[companion] hooks: no USERPROFILE or CLAUDE_CONFIG_DIR");
        return 1;
    };
    let result = if install {
        let timeout = timeout_s.unwrap_or(claude::HOOK_TIMEOUT_S).clamp(1, 600);
        let exe = std::env::current_exe().map_err(|e| format!("this exe's path: {e}"));
        let cfg = std::fs::canonicalize(cfg_path)
            .map(|p| std::path::PathBuf::from(chernobyl_companion::claude_live::plain(&p)))
            .map_err(|e| format!("{}: {e}", cfg_path.display()));
        exe.and_then(|exe| {
            let exe = std::fs::canonicalize(&exe)
                .map(|p| std::path::PathBuf::from(chernobyl_companion::claude_live::plain(&p)))
                .unwrap_or(exe);
            cfg.and_then(|c| claude::hook_command(&exe, &c, timeout))
        })
        .and_then(|cmd| claude::install_hook(&settings, &cmd, timeout))
    } else {
        claude::uninstall_hook(&settings)
    };
    match result {
        Ok(msg) => {
            println!("{msg}");
            if install {
                println!(
                    "approvals reach the phone only for sessions in the workspaces configured for \
                     Claude Code, while a phone is connected; otherwise the PC asks as usual"
                );
            }
            0
        }
        Err(e) => {
            eprintln!("[companion] hooks: {e}");
            1
        }
    }
}

/// Phase 8: `companion backup <path>` / `companion restore <path>`. Copies the
/// durable journal with a SHA-256 sidecar; restore refuses a checksum mismatch
/// or a running companion, and keeps every revocation of the journal it
/// replaces. Socket-free (no server started). Caveat: PSKs are DPAPI-bound to
/// this machine+user, so a restore on another machine needs re-pairing.
fn backup_main(cfg: &config::CompanionConfig, backup: Option<&str>, restore: Option<&str>) -> i32 {
    use chernobyl_companion::backup;
    use std::path::Path;
    if let Some(dest) = backup {
        match backup::backup(&cfg.data_dir, Path::new(dest)) {
            Ok(digest) => {
                println!("backed up {} -> {dest}", backup::JOURNAL_NAME);
                println!("sha256 {digest}");
                0
            }
            Err(e) => {
                eprintln!("[companion] backup failed: {e}");
                1
            }
        }
    } else if let Some(src) = restore {
        match backup::restore(Path::new(src), &cfg.data_dir) {
            Ok((digest, reapplied)) => {
                println!(
                    "restored {src} -> {}",
                    cfg.data_dir.join(backup::JOURNAL_NAME).display()
                );
                println!("sha256 {digest}");
                for id in reapplied {
                    println!("re-applied revocation of device {id} (revoked after this backup)");
                }
                0
            }
            Err(e) => {
                eprintln!("[companion] restore failed: {e}");
                1
            }
        }
    } else {
        2
    }
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
        let clean = srv.stop();
        companion.shutdown(clean);
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
        let clean2 = match srv2 {
            Ok(s) => s.stop(),
            Err(e) => {
                check("second boot serves", false, e, &mut failures);
                false
            }
        };
        companion2.shutdown(clean2);
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
            r#""C:\a b\c.exe" --background"#
        );
        assert_eq!(
            autostart_value(
                std::path::Path::new(r"C:\x.exe"),
                Some(r"C:\cfg dir\companion.json")
            ),
            r#""C:\x.exe" --background --config "C:\cfg dir\companion.json""#
        );
    }

    /// ADR-018: the hook path writes nothing and only talks to a companion
    /// that holds the journal lock.
    #[test]
    fn hook_mode_writes_no_config_and_sees_only_a_running_companion() {
        use chernobyl_companion::{config, storage};
        let dir = std::env::temp_dir().join(format!("chb-hook-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("companion.json");
        assert!(config::read_config(Some(&cfg)).is_none());
        assert!(!cfg.exists(), "no default config written");
        assert!(!super::companion_running(&dir));
        let lock = storage::lock_data_dir(&dir).unwrap();
        assert!(super::companion_running(&dir));
        drop(lock);
        assert!(!super::companion_running(&dir));
        // an unknown hook mode is a silent no-op
        assert_eq!(super::hook_main("pre-tool-use", None, None), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_revoke_offline_falls_back_and_online_routes_to_running_companion() {
        use super::run;
        use chernobyl_companion::config::default_config;
        use chernobyl_companion::protocol::now_ms;
        use chernobyl_companion::storage::{self, DeviceRecord};

        let temp_dir = std::env::temp_dir().join(format!(
            "chernobyl-cli-revoke-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut cfg = default_config();
        cfg.data_dir = temp_dir.clone();
        cfg.diagnostics_port = 0; // won't connect when offline
        let cfg_path = temp_dir.join("companion.json");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let cfg_json = chernobyl_companion::json::stringify(
            &chernobyl_companion::config::config_to_json(&cfg),
        );
        std::fs::write(&cfg_path, cfg_json).unwrap();

        // Seed devices in the storage
        let psk = [7u8; 32];
        let prot = chernobyl_companion::base64::encode(
            &chernobyl_companion::ffi::dpapi_protect(&psk).unwrap(),
        );
        {
            let mut store =
                storage::Storage::open(&temp_dir, cfg.events_cap, cfg.commands_cap).unwrap();
            store
                .put_device(&DeviceRecord {
                    device_id: "dev-offline".into(),
                    name: "offline-phone".into(),
                    psk_protected: prot.clone(),
                    psk_next_protected: None,
                    paired_at: now_ms(),
                    revoked: false,
                })
                .unwrap();
            store
                .put_device(&DeviceRecord {
                    device_id: "dev-online".into(),
                    name: "online-phone".into(),
                    psk_protected: prot,
                    psk_next_protected: None,
                    paired_at: now_ms(),
                    revoked: false,
                })
                .unwrap();
        }

        let cfg_str = cfg_path.to_str().unwrap().to_string();

        // 1. Offline path: unknown device -> exit 1
        let exit_unknown = run(&[
            "--config".into(),
            cfg_str.clone(),
            "--revoke".into(),
            "unknown-offline".into(),
        ]);
        assert_eq!(exit_unknown, 1);

        // 2. Offline path: known device -> exit 0 (written to offline journal)
        let exit_offline = run(&[
            "--config".into(),
            cfg_str.clone(),
            "--revoke".into(),
            "dev-offline".into(),
        ]);
        assert_eq!(exit_offline, 0);
        {
            let store =
                storage::Storage::open(&temp_dir, cfg.events_cap, cfg.commands_cap).unwrap();
            assert!(store.devices["dev-offline"].revoked);
        }

        // 3. Online path: boot a companion + server on this data dir, holding
        // the journal lock like the daemon run path does
        let _daemon_lock = chernobyl_companion::storage::lock_data_dir(&temp_dir).unwrap();
        let companion = std::sync::Arc::new(
            chernobyl_companion::companion::Companion::boot(cfg.clone()).unwrap(),
        );
        let handle =
            chernobyl_companion::server::start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();
        let diag_port_str = handle.diagnostics_port.to_string();

        // Unknown device against running companion -> exit 1
        let exit_online_unknown = run(&[
            "--config".into(),
            cfg_str.clone(),
            "--diagnostics-port".into(),
            diag_port_str.clone(),
            "--revoke".into(),
            "unknown-online".into(),
        ]);
        assert_eq!(exit_online_unknown, 1);

        // Known device against running companion -> exit 0
        let exit_online_known = run(&[
            "--config".into(),
            cfg_str,
            "--diagnostics-port".into(),
            diag_port_str,
            "--revoke".into(),
            "dev-online".into(),
        ]);
        assert_eq!(exit_online_known, 0);
        {
            let store = companion.shared.storage.lock().unwrap();
            assert!(store.devices["dev-online"].revoked);
        }

        let clean = handle.stop();
        companion.shutdown(clean);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn cli_pair_offline_falls_back_and_online_routes_to_running_companion() {
        use super::run;
        use chernobyl_companion::config::default_config;
        use chernobyl_companion::protocol::now_ms;

        let temp_dir = std::env::temp_dir().join(format!(
            "chernobyl-cli-pair-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut cfg = default_config();
        cfg.data_dir = temp_dir.clone();
        cfg.diagnostics_port = 0; // offline initially
        let cfg_path = temp_dir.join("companion.json");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let cfg_json = chernobyl_companion::json::stringify(
            &chernobyl_companion::config::config_to_json(&cfg),
        );
        std::fs::write(&cfg_path, cfg_json).unwrap();
        let cfg_str = cfg_path.to_str().unwrap().to_string();

        // 1. Offline path: companion not running -> writes to journal, exits 0
        let exit_offline = run(&["--config".into(), cfg_str.clone(), "--pair".into()]);
        assert_eq!(exit_offline, 0);

        // Verify offer exists in offline journal
        {
            let store = chernobyl_companion::storage::Storage::open(
                &temp_dir,
                cfg.events_cap,
                cfg.commands_cap,
            )
            .unwrap();
            assert_eq!(
                store.pairings.len(),
                1,
                "offline pairing must persist offer"
            );
        }

        // 2. Online path: boot companion and server, holding the journal lock
        // like the daemon run path does
        let _daemon_lock = chernobyl_companion::storage::lock_data_dir(&temp_dir).unwrap();
        let companion = std::sync::Arc::new(
            chernobyl_companion::companion::Companion::boot(cfg.clone()).unwrap(),
        );
        let srv =
            chernobyl_companion::server::start("127.0.0.1", 0, 0, None, companion.clone()).unwrap();

        let diag_port_str = srv.diagnostics_port.to_string();
        let exit_online = run(&[
            "--config".into(),
            cfg_str.clone(),
            "--diagnostics-port".into(),
            diag_port_str,
            "--pair".into(),
        ]);
        assert_eq!(exit_online, 0);

        // Verify companion's in-memory store has the offer
        {
            let store = companion.shared.storage.lock().unwrap();
            // It had 1 from boot (the offline one), now it has 2!
            assert_eq!(
                store.pairings.len(),
                2,
                "running companion must have both offers"
            );
        }

        let clean = srv.stop();
        companion.shutdown(clean);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn cli_require_e2e_flag_forces_remote_posture() {
        use super::run;
        use chernobyl_companion::config::default_config;
        use chernobyl_companion::protocol::now_ms;

        let temp_dir = std::env::temp_dir().join(format!(
            "chernobyl-cli-e2e-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut cfg = default_config();
        cfg.data_dir = temp_dir.clone();
        cfg.server_port = 0;
        cfg.diagnostics_port = 0;
        let cfg_path = temp_dir.join("companion.json");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let cfg_json = chernobyl_companion::json::stringify(
            &chernobyl_companion::config::config_to_json(&cfg),
        );
        std::fs::write(&cfg_path, cfg_json).unwrap();
        let cfg_str = cfg_path.to_str().unwrap().to_string();

        // Run with --require-e2e and --exit-after 50ms smoke. The token is
        // required since the security review 2026-10-25 (see
        // cli_require_e2e_without_token_is_refused).
        let exit = run(&[
            "--config".into(),
            cfg_str,
            "--require-e2e".into(),
            "--token".into(),
            "e2e-secret".into(),
            "--exit-after".into(),
            "50".into(),
        ]);
        assert_eq!(exit, 0);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// A fresh data dir holding a companion.json that points at it (ephemeral
    /// ports); returns the dir and the config path.
    fn temp_config(
        tag: &str,
        edit: impl FnOnce(&mut chernobyl_companion::config::CompanionConfig),
    ) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!(
            "chernobyl-cli-{tag}-{}-{}",
            std::process::id(),
            chernobyl_companion::protocol::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = chernobyl_companion::config::default_config();
        cfg.data_dir = dir.clone();
        cfg.server_port = 0;
        cfg.diagnostics_port = 0; // the online attempt fails at once
        edit(&mut cfg);
        let path = dir.join("companion.json");
        let text = chernobyl_companion::json::stringify(
            &chernobyl_companion::config::config_to_json(&cfg),
        );
        std::fs::write(&path, text).unwrap();
        (dir, path.to_str().unwrap().to_string())
    }

    fn cli(cfg_path: &str, args: &[&str]) -> i32 {
        let mut argv = vec!["--config".to_string(), cfg_path.to_string()];
        argv.extend(args.iter().map(|a| a.to_string()));
        super::run(&argv)
    }

    /// Security review 2026-10-25: remote posture on loopback is the tunnel
    /// recipe, so it needs a token exactly like a non-loopback bind.
    #[test]
    fn cli_require_e2e_without_token_is_refused() {
        let (dir, cfg_path) = temp_config("e2e-no-token", |_| {});
        assert_eq!(cli(&cfg_path, &["--require-e2e", "--exit-after", "50"]), 1);
        let (dir2, cfg_path2) = temp_config("e2e-no-token-cfg", |c| c.server_require_e2e = true);
        assert_eq!(cli(&cfg_path2, &["--exit-after", "50"]), 1);
        // refused before boot: no journal was ever opened
        assert!(!dir.join("companion.journal").exists());
        assert!(!dir2.join("companion.journal").exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// Security review 2026-10-25: the daemon holds the data-dir lock from
    /// before boot until after shutdown, and refuses to boot on a held one.
    #[cfg(windows)]
    #[test]
    fn cli_daemon_holds_the_journal_lock_while_it_runs() {
        use chernobyl_companion::storage::lock_data_dir;
        let (dir, cfg_path) = temp_config("daemon-lock", |_| {});
        let daemon = ["--token", "t", "--exit-after", "1500"];

        let other = lock_data_dir(&dir).unwrap();
        assert_eq!(cli(&cfg_path, &daemon), 1, "booted on a held journal");
        drop(other);

        let path = cfg_path.clone();
        let running = std::thread::spawn(move || cli(&path, &daemon));
        // the journal only appears at boot, which follows taking the lock
        let t0 = std::time::Instant::now();
        while !dir.join("companion.journal").exists() {
            assert!(t0.elapsed().as_secs() < 10, "daemon never booted");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let err = lock_data_dir(&dir).unwrap_err();
        assert!(err.contains("already running"), "got: {err}");
        assert_eq!(running.join().unwrap(), 0);
        drop(lock_data_dir(&dir).expect("released once the daemon exits"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Security review 2026-10-25: while a companion holds the journal the CLI
    /// never writes it. Offline --revoke / --pair / --restore refuse, and
    /// --devices reads without changing a byte of a journal that a full open
    /// would prune, compact and truncate.
    #[cfg(windows)]
    #[test]
    fn cli_never_writes_a_journal_a_companion_holds() {
        use chernobyl_companion::storage::{DeviceRecord, Storage, lock_data_dir};
        use std::io::Write;
        let (dir, cfg_path) = temp_config("held-journal", |c| c.events_cap = 100);
        {
            let mut store = Storage::open(&dir, 1_000, 1_000).unwrap();
            store
                .put_device(&DeviceRecord {
                    device_id: "dev-1".into(),
                    name: "phone".into(),
                    psk_protected: "P".into(),
                    psk_next_protected: None,
                    paired_at: 1,
                    revoked: false,
                })
                .unwrap();
            for seq in 1..=101 {
                store.append_event("e", "{}", seq).unwrap(); // over retention.events
            }
        }
        let bak = dir.join("snap.journal");
        chernobyl_companion::backup::backup(&dir, &bak).unwrap();
        let journal = dir.join("companion.journal");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .unwrap()
            .write_all(b"{\"kind\":\"ev") // torn tail
            .unwrap();
        let before = std::fs::read(&journal).unwrap();

        let companion = lock_data_dir(&dir).unwrap();
        assert_eq!(cli(&cfg_path, &["--revoke", "dev-1"]), 1, "offline revoke");
        assert_eq!(cli(&cfg_path, &["--pair"]), 1, "offline pair");
        assert_eq!(
            cli(&cfg_path, &["--restore", bak.to_str().unwrap()]),
            1,
            "restore"
        );
        assert_eq!(cli(&cfg_path, &["--devices"]), 0, "--devices still lists");
        assert_eq!(
            std::fs::read(&journal).unwrap(),
            before,
            "a held journal changed"
        );

        // control: once the companion is gone the offline revoke applies
        drop(companion);
        assert_eq!(cli(&cfg_path, &["--revoke", "dev-1"]), 0);
        assert!(Storage::open_read_only(&dir).unwrap().devices["dev-1"].revoked);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
