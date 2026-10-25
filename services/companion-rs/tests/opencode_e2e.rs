//! Live Phase-3 end-to-end test against a REAL `opencode serve`
//! (PRD §25 exit criteria: real session controlled end to end, no mock
//! success responses, reconnection + restart recovery).
//!
//! Opt-in — ignored by default so gates run without a server:
//!
//! ```text
//! cargo test --test opencode_e2e -- --ignored --nocapture
//! ```
//!
//! Isolation rules (`docs/reverse-engineering/opencode.md` §9):
//!   * its own `XDG_DATA_HOME` — never the user's shared
//!     `~/.local/share/opencode` (two CLI generations sharing one DB kill
//!     turns on FK skew);
//!   * attach-only (G4.3): the companion never spawns the server — this
//!     harness owns the server lifecycle, the adapter only attaches;
//!   * Windows-only harness (`taskkill /T` for tree kill).

use chernobyl_companion::companion::Companion;
use chernobyl_companion::config::{AdapterEntryConfig, CompanionConfig, default_config};
use chernobyl_companion::opencode::OpenCodeAdapter;
use chernobyl_companion::protocol::{ClientMsg, ServerMsg, now_ms};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Real server binary: `$OPENCODE_BIN`, PATH `opencode.exe`, or the npm
/// shim's target (`node_modules\opencode-ai\bin\opencode.exe`).
fn opencode_exe() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("OPENCODE_BIN") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let path = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path) {
        let exe = dir.join("opencode.exe");
        if exe.exists() {
            return Some(exe);
        }
        if dir.join("opencode.cmd").exists() {
            let target = dir
                .join("node_modules")
                .join("opencode-ai")
                .join("bin")
                .join("opencode.exe");
            if target.exists() {
                return Some(target);
            }
        }
    }
    None
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn_server(exe: &Path, port: u16, data: &Path, work: &Path, log: &Path) -> Child {
    let log = std::fs::File::create(log).expect("serve log");
    let log2 = log.try_clone().expect("serve log clone");
    Command::new(exe)
        .args([
            "serve",
            "--port",
            &port.to_string(),
            "--hostname",
            "127.0.0.1",
        ])
        .env("XDG_DATA_HOME", data)
        .current_dir(work)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
        .expect("spawn opencode serve")
}

fn kill_server(mut child: Child) {
    // kill the tree — the npm launcher may own a node child (Windows)
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &child.id().to_string()])
        .output();
    let _ = child.kill();
    let _ = child.wait();
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn events_contain(comp: &Companion, needle: &str) -> bool {
    let store = comp.shared.storage.lock().unwrap();
    store
        .events
        .iter()
        .any(|e| e.payload.contains(needle) || e.kind.contains(needle))
}

/// Raw socket POST with the exact bytes `client::http_request` sends, with
/// per-chunk arrival timing so "server never answered" and "answered but
/// never closed the connection" stay distinguishable (diagnostics only).
fn raw_abort(addr: &str, sid: &str, cap: Duration) -> String {
    use std::io::{Read, Write};
    let authority = addr.trim_start_matches("http://");
    let mut stream = match std::net::TcpStream::connect(authority) {
        Ok(s) => s,
        Err(e) => return format!("connect failed: {e}"),
    };
    let _ = stream.set_read_timeout(Some(cap));
    let req = format!(
        "POST /session/{sid}/abort HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    );
    if let Err(e) = stream.write_all(req.as_bytes()) {
        return format!("write failed: {e}");
    }
    let t = Instant::now();
    let mut buf = [0u8; 4096];
    let mut total = 0usize;
    let mut out = String::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => {
                out.push_str(&format!("EOF at {:?} ({total} bytes)", t.elapsed()));
                break;
            }
            Ok(n) => {
                total += n;
                out.push_str(&format!("+{n} bytes at {:?}; ", t.elapsed()));
            }
            Err(e) => {
                out.push_str(&format!(
                    "read err at {:?}: {e} ({total} bytes)",
                    t.elapsed()
                ));
                break;
            }
        }
    }
    out
}

#[test]
#[ignore = "needs a real `opencode` CLI on PATH — run manually for Phase-3 evidence"]
fn live_opencode_end_to_end() {
    let Some(exe) = opencode_exe() else {
        panic!("opencode CLI not found (set OPENCODE_BIN) — Phase-3 e2e needs the real runtime");
    };

    let base = std::env::temp_dir().join(format!("chernobyl-opencode-e2e-{}", now_ms()));
    let data = base.join("xdg"); // isolated data dir root (XDG_DATA_HOME)
    let work = base.join("work");
    let comp_dir = base.join("companion");
    for d in [&data, &work, &comp_dir] {
        std::fs::create_dir_all(d).expect("temp dirs");
    }
    let port = free_port();
    let addr = format!("http://127.0.0.1:{port}");

    let mut server = spawn_server(&exe, port, &data, &work, &base.join("serve1.log"));

    // 1) attach readiness — the adapter's own probe, never a spawn
    let probe = OpenCodeAdapter::new("e2e-probe", &addr, None);
    assert!(
        wait_until(Duration::from_secs(30), || probe.refresh().is_ok()),
        "opencode serve never became healthy (see serve1.log: {})",
        base.join("serve1.log").display()
    );
    eprintln!(
        "E2E evidence: attached to real opencode {} at {addr} (isolated XDG_DATA_HOME)",
        probe.version()
    );

    // 2) companion boots already attached (probe-interval 300 ms so the
    //    restart-recovery step can be driven by pumping)
    let mut c: CompanionConfig = default_config();
    c.data_dir = comp_dir.clone();
    c.probe_interval_ms = 300;
    c.adapters.push(AdapterEntryConfig {
        id: "oc-live".into(),
        kind: "opencode".into(),
        enabled: true,
        endpoint: Some(addr.clone()),
        basic_auth: None,
    });
    let comp = Companion::boot(c.clone()).unwrap();
    let oc = comp
        .shared
        .opencode
        .lock()
        .unwrap()
        .clone()
        .expect("attached at boot");
    assert!(oc.is_healthy());
    let snap = comp.snapshot();
    eprintln!(
        "E2E evidence: agents={:?} sessions={} version={}",
        snap.agents
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        snap.sessions.len(),
        oc.version()
    );
    assert!(!snap.agents.is_empty(), "real agent list must map");

    // 3) create a session on the REAL runtime
    let sid = match comp.handle_client_msg(ClientMsg::CreateSession {
        command_id: "e2e-create".into(),
        agent_id: "build".into(),
        title: "Phase 3 e2e".into(),
        prompt: String::new(),
        project: None,
    }) {
        Some(ServerMsg::Ack {
            ok: true,
            session_id: Some(id),
            ..
        }) => id,
        other => panic!("create refused: {other:?}"),
    };
    assert!(
        sid.starts_with("ses_"),
        "native session id shape, got {sid}"
    );
    eprintln!("E2E evidence: created real session {sid}");
    assert!(
        wait_until(Duration::from_secs(5), || comp
            .snapshot()
            .sessions
            .iter()
            .any(|s| s.id == sid)),
        "created session never reached the merged snapshot"
    );

    // 4) prompt → acceptance only (real 204); outcome follows on the stream
    let reply = comp.handle_client_msg(ClientMsg::SubmitPrompt {
        command_id: "e2e-turn-1".into(),
        session_id: sid.clone(),
        text: "Reply with exactly: PHASE3OK".into(),
    });
    assert!(matches!(reply, Some(ServerMsg::Ack { ok: true, .. })));
    eprintln!("E2E evidence: prompt accepted (204) — outcome follows on SSE");

    // 5) our user-message text must normalize out of the LIVE stream
    //    (independent of the model provider's answer)
    assert!(
        wait_until(Duration::from_secs(20), || events_contain(
            &comp, "PHASE3OK"
        )),
        "no message.part activity reached storage from the live SSE stream"
    );
    eprintln!("E2E evidence: message.part.updated normalized into storage events");

    // 6) turn terminal on the real runtime — EITHER a healthy turn (session
    //    idle/completed) OR the correlated failure channel (G4.1; the
    //    provider may reject 1.17.7 with 426, probe §8). Never mock either.
    let terminal = wait_until(Duration::from_secs(60), || {
        let failed = {
            let store = comp.shared.storage.lock().unwrap();
            store
                .commands
                .get("e2e-turn-1")
                .map(|r| r.status.as_str() == "failed")
                .unwrap_or(false)
        };
        if failed {
            return true;
        }
        comp.snapshot()
            .sessions
            .iter()
            .find(|s| s.id == sid)
            .map(|s| s.status == "completed" || s.status == "interrupted")
            .unwrap_or(false)
    });
    let msgs = oc.messages(&sid).expect("real messages endpoint");
    let msg_count = chernobyl_companion::json::as_arr(&msgs)
        .map(|a| a.len())
        .unwrap_or(0);
    {
        let store = comp.shared.storage.lock().unwrap();
        let rec = store.commands.get("e2e-turn-1");
        eprintln!(
            "E2E evidence: terminal={terminal} turn_status={:?} turn_error={:?} messages={msg_count}",
            rec.map(|r| r.status.as_str()),
            rec.and_then(|r| r.error.clone())
        );
    }
    assert!(
        terminal,
        "turn never reached a terminal on the event stream"
    );
    assert!(
        msg_count > 0,
        "user message must persist on the real runtime"
    );

    // 7) interrupt → native abort (Phase 0 §5 measured 51 ms idle and
    //    mid-turn). A slow or failed answer is surfaced honestly — never
    //    retried into a fake ack — and diagnosed with the same bytes.
    eprintln!("E2E diag: interrupt dispatched at unix_ms={}", now_ms());
    let t0 = Instant::now();
    let reply = comp.handle_client_msg(ClientMsg::Interrupt {
        command_id: "e2e-int".into(),
        session_id: sid.clone(),
    });
    let elapsed = t0.elapsed();
    if matches!(reply, Some(ServerMsg::Ack { ok: true, .. })) {
        eprintln!("E2E evidence: abort → ack ok in {elapsed:?}");
    } else {
        let t1 = Instant::now();
        let direct = chernobyl_companion::client::http_request(
            "POST",
            &format!("{addr}/session/{sid}/abort"),
            None,
            None,
        );
        eprintln!(
            "E2E diag: companion interrupt after {elapsed:?} → {reply:?}; direct http_request → {:?} in {:?}",
            direct.as_ref().map(|r| format!("{} {}", r.status, r.body)),
            t1.elapsed()
        );
        let t2 = Instant::now();
        eprintln!(
            "E2E diag: raw socket (identical bytes) → {} in {:?}",
            raw_abort(&addr, &sid, Duration::from_secs(20)),
            t2.elapsed()
        );
        let t3 = Instant::now();
        let refresh = probe.refresh().map(|_| "ok");
        eprintln!("E2E diag: GET refresh → {refresh:?} in {:?}", t3.elapsed());
        let events = comp.shared.storage.lock().unwrap().events.len();
        let st = comp
            .snapshot()
            .sessions
            .iter()
            .find(|s| s.id == sid)
            .map(|s| s.status.clone());
        eprintln!(
            "E2E diag: healthy={:?} events={events} session_status={st:?}",
            oc.is_healthy()
        );
    }
    assert!(
        matches!(reply, Some(ServerMsg::Ack { ok: true, .. })),
        "interrupt after {elapsed:?}: {reply:?}"
    );

    // 8) diff endpoint answers real validated JSON (empty diff is honest)
    let d = oc.diff(&sid).expect("diff endpoint");
    eprintln!(
        "E2E evidence: diff = {}",
        chernobyl_companion::json::stringify(&d)
    );

    // 9) restart recovery: kill → probe loop latches unhealthy → restart
    //    (same data dir) → adapter reconnects and re-lists the session
    kill_server(server);
    assert!(
        wait_until(Duration::from_secs(5), || {
            comp.pump(); // production host drives ticks; probing lives in pump()
            !oc.is_healthy()
        }),
        "probe loop never reported the dead runtime"
    );
    eprintln!(
        "E2E evidence: dead runtime latched unhealthy (probe detail: {:?})",
        oc.detail()
    );

    server = spawn_server(&exe, port, &data, &work, &base.join("serve2.log"));
    assert!(
        wait_until(Duration::from_secs(45), || {
            comp.pump(); // drive the probe loop (engine tick + adapter refresh)
            oc.is_healthy() && comp.snapshot().sessions.iter().any(|s| s.id == sid)
        }),
        "adapter never recovered after server restart (see serve2.log)"
    );
    eprintln!(
        "E2E evidence: reconnected after restart; session {sid} re-listed from persisted state"
    );

    // teardown: adapter first (stops the SSE thread), then the server
    comp.shutdown(true);
    kill_server(server);
    let _ = std::fs::remove_dir_all(&base);
}
