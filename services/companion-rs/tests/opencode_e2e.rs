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
use chernobyl_companion::protocol::{ClientMsg, PermissionRequest, ServerMsg, WorkspaceOp, now_ms};
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

/// `opencode serve` with every user directory moved under `base`: data,
/// config, cache, state AND home (`opencode-live-events.md` §1.1: without
/// `HOME`/`USERPROFILE` 1.19.4 still loads the user's skills).
fn spawn_server(exe: &Path, port: u16, base: &Path, work: &Path, log: &Path) -> Child {
    let log = std::fs::File::create(log).expect("serve log");
    let log2 = log.try_clone().expect("serve log clone");
    let dir = |name: &str| {
        let d = base.join(name);
        std::fs::create_dir_all(&d).expect("isolated dir");
        d
    };
    // a git root stops OpenCode's walk up from the cwd: without it, a temp
    // cwd under the profile finds the user's ~/.claude and ~/.agents skills
    let _ = Command::new("git")
        .args(["init", "-q"])
        .current_dir(work)
        .output();
    Command::new(exe)
        .args([
            "serve",
            "--port",
            &port.to_string(),
            "--hostname",
            "127.0.0.1",
        ])
        .env("HOME", dir("home"))
        .env("USERPROFILE", dir("home"))
        .env("XDG_DATA_HOME", dir("data"))
        .env("XDG_CONFIG_HOME", dir("config"))
        .env("XDG_CACHE_HOME", dir("cache"))
        .env("XDG_STATE_HOME", dir("state"))
        .env("PWD", work)
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
    let data = base.join("xdg"); // isolated HOME + XDG_* roots
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
        ..Default::default()
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
        images: vec![],
        model: None,
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
        images: vec![],
        fork: false,
    });
    assert!(matches!(reply, Some(ServerMsg::Ack { ok: true, .. })));
    eprintln!("E2E evidence: prompt accepted (204) — outcome follows on SSE");

    // 5) ADR-015: conversation text is transcript-only (memory), never
    //    journaled activity — the old echo of our own prompt is gone

    // 6) turn terminal on the real runtime — EITHER a healthy turn (session
    //    idle/completed) OR the correlated failure channel (G4.1; the
    //    provider may reject 1.17.7 with 426, probe §8). Never mock either.
    // The prompt command's own outcome (ADR-015 step 5 closes it on idle),
    // not the session status: a freshly idle session already reads
    // "completed" before the prompt has run.
    let terminal = wait_until(Duration::from_secs(60), || {
        let store = comp.shared.storage.lock().unwrap();
        store
            .commands
            .get("e2e-turn-1")
            .map(|r| matches!(r.status.as_str(), "completed" | "failed" | "cancelled"))
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
        !events_contain(&comp, "PHASE3OK"),
        "conversation text reached the journal"
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

/* ------------------------------------------------------------------ */
/* ADR-015 step 5: rich parts against a live server                    */
/* ------------------------------------------------------------------ */

/// The temporary Ollama alias: `qwen3.5:4b` with a 24k context (Ollama's
/// OpenAI-compatible route cannot set `num_ctx`, and the default cuts
/// OpenCode's prompt and its tool list, `opencode-live-events.md` §7.3).
const LIVE_ALIAS: &str = "chernobyl-qwen35-24k";
const OLLAMA: &str = "http://127.0.0.1:11434";

fn ollama(method: &str, path: &str, body: &str) -> Result<String, String> {
    chernobyl_companion::client::http_request_waiting(
        method,
        &format!("{OLLAMA}{path}"),
        None,
        Some(body),
        Duration::from_secs(300),
    )
    .map(|r| format!("{} {}", r.status, r.body))
}

/// Kills the server and drops the alias whatever the test's outcome.
struct LiveGuard {
    server: Option<Child>,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        if let Some(child) = self.server.take() {
            kill_server(child);
        }
        let _ = ollama(
            "DELETE",
            "/api/delete",
            &format!(r#"{{"model":"{LIVE_ALIAS}"}}"#),
        );
    }
}

type Frames = std::sync::mpsc::Receiver<(chernobyl_companion::server::MsgClass, Vec<u8>)>;

/// A phone's view without a socket: one subscribed connection whose frames
/// the test drains while it drives the companion's scheduler.
struct Phone {
    out: std::sync::Arc<chernobyl_companion::server::Outbound>,
    rx: Frames,
    seen: String,
}

impl Phone {
    fn drain(&mut self, comp: &Companion) {
        comp.pump();
        while let Some((_, frame)) = self.out.test_dequeue(&self.rx) {
            self.seen.push_str(&String::from_utf8_lossy(&frame));
            self.seen.push('\n');
        }
    }
    fn count(&self, needle: &str) -> usize {
        self.seen.matches(needle).count()
    }
}

fn command_status(comp: &Companion, id: &str) -> Option<String> {
    let store = comp.shared.storage.lock().unwrap();
    store.commands.get(id).map(|r| r.status.clone())
}

/// Waits (pumping) until `f` holds; minutes, because a CPU-offloaded 4B
/// model reads OpenCode's ~8k-token prompt slowly on a cold cache.
fn live_wait(
    comp: &Companion,
    phone: &mut Phone,
    what: &str,
    mut f: impl FnMut(&Companion, &Phone) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        phone.drain(comp);
        if f(comp, phone) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn prompt(comp: &Companion, sid: &str, cmd: &str, text: &str) {
    let reply = comp.handle_client_msg(ClientMsg::SubmitPrompt {
        command_id: cmd.into(),
        session_id: sid.into(),
        text: text.into(),
        images: vec![],
        fork: false,
    });
    assert!(
        matches!(reply, Some(ServerMsg::Ack { ok: true, .. })),
        "{reply:?}"
    );
}

/// The session's pending request, once the model asked.
fn pending(comp: &Companion, sid: &str) -> Option<PermissionRequest> {
    comp.snapshot()
        .permissions
        .into_iter()
        .find(|p| p.session_id == sid && p.status == "pending")
}

fn decide(comp: &Companion, p: &PermissionRequest, cmd: &str, option: &str, message: Option<&str>) {
    let reply = comp.handle_client_msg(ClientMsg::PermissionDecision {
        command_id: cmd.into(),
        permission_id: p.id.clone(),
        decision: if option == "reject" {
            "reject"
        } else {
            "approve-once"
        }
        .into(),
        revision: p.revision as f64,
        option_id: Some(option.into()),
        message: message.map(str::to_string),
        answers: vec![],
    });
    assert!(
        matches!(reply, Some(ServerMsg::Ack { ok: true, .. })),
        "{reply:?}"
    );
}

/// ADR-015 step 5 live run. Opt-in twice: `#[ignore]` and
/// `CHERNOBYL_LIVE_OPENCODE=1`. Needs `opencode` 1.19.x and Ollama with
/// `qwen3.5:4b` at 127.0.0.1:11434. Everything OpenCode reads or writes
/// lives under `$CHERNOBYL_LIVE_DIR` (default: the temp dir): HOME,
/// USERPROFILE, every XDG_* root and the cwd. The user's own OpenCode data
/// is never touched; the harness owns the server, the companion only
/// attaches (G4.3).
///
/// ```text
/// CHERNOBYL_LIVE_OPENCODE=1 cargo test --test opencode_e2e live_opencode_rich_parts -- --ignored --nocapture
/// ```
#[test]
#[ignore = "live: set CHERNOBYL_LIVE_OPENCODE=1 (opencode CLI + local Ollama)"]
fn live_opencode_rich_parts() {
    if std::env::var("CHERNOBYL_LIVE_OPENCODE").as_deref() != Ok("1") {
        eprintln!("skipped: set CHERNOBYL_LIVE_OPENCODE=1");
        return;
    }
    let exe = opencode_exe().expect("opencode CLI not found (set OPENCODE_BIN)");
    let root = std::env::var_os("CHERNOBYL_LIVE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let base = root.join(format!("chernobyl-opencode-live-{}", now_ms()));
    let (iso, work, comp_dir) = (base.join("iso"), base.join("work"), base.join("companion"));
    let config = iso.join("config").join("opencode");
    for d in [&work, &comp_dir, &config] {
        std::fs::create_dir_all(d).expect("temp dirs");
    }
    let mut guard = LiveGuard { server: None };
    let created = ollama(
        "POST",
        "/api/create",
        &format!(
            r#"{{"model":"{LIVE_ALIAS}","from":"qwen3.5:4b","parameters":{{"num_ctx":24576}},"stream":false}}"#
        ),
    )
    .expect("Ollama at 127.0.0.1:11434");
    assert!(created.contains("success"), "alias: {created}");
    // `ask` for bash and edit; one remote MCP server that needs OAuth
    let opencode_json = format!(
        r#"{{"$schema":"https://opencode.ai/config.json","autoupdate":false,"share":"disabled",
"model":"ollama/{LIVE_ALIAS}",
"provider":{{"ollama":{{"npm":"@ai-sdk/openai-compatible","name":"Ollama",
"options":{{"baseURL":"{OLLAMA}/v1"}},
"models":{{"{LIVE_ALIAS}":{{"name":"qwen3.5 4b 24k","limit":{{"context":24576,"output":4096}}}}}}}}}},
"permission":{{"bash":"ask","edit":"ask"}},
"mcp":{{"sentry":{{"type":"remote","url":"https://mcp.sentry.dev/mcp"}}}}}}"#
    );
    std::fs::write(config.join("opencode.json"), opencode_json).unwrap();
    let port = free_port();
    let addr = format!("http://127.0.0.1:{port}");
    guard.server = Some(spawn_server(
        &exe,
        port,
        &iso,
        &work,
        &base.join("serve.log"),
    ));
    // Hard budget for the whole live run: one run once hung for hours on an
    // unbounded call. Past it, kill the isolated server tree and fail.
    let pid = guard.server.as_ref().map(|s| s.id());
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(900));
        eprintln!("live opencode test: 15 min budget exceeded");
        if let Some(pid) = pid {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output();
        }
        std::process::exit(1);
    });
    let probe = OpenCodeAdapter::new("live-probe", &addr, None);
    assert!(
        wait_until(Duration::from_secs(60), || probe.refresh().is_ok()),
        "opencode serve never became healthy"
    );

    let mut c: CompanionConfig = default_config();
    c.data_dir = comp_dir.clone();
    c.adapters.push(AdapterEntryConfig {
        id: "oc-live".into(),
        kind: "opencode".into(),
        enabled: true,
        endpoint: Some(addr.clone()),
        basic_auth: None,
        ..Default::default()
    });
    let comp = Companion::boot(c).unwrap();
    let sid = match comp.handle_client_msg(ClientMsg::CreateSession {
        command_id: "live-create".into(),
        agent_id: "build".into(),
        title: "ADR-015 step 5 live".into(),
        prompt: String::new(),
        project: None,
        images: vec![],
        model: None,
    }) {
        Some(ServerMsg::Ack {
            ok: true,
            session_id: Some(id),
            ..
        }) => id,
        other => panic!("create refused: {other:?}"),
    };
    assert!(
        comp.snapshot()
            .sessions
            .iter()
            .any(|s| s.id == sid && s.transcript)
    );
    let (out, rx) = chernobyl_companion::server::Outbound::new(
        std::sync::Arc::new(chernobyl_companion::companion::Saturation::default()),
        false,
    );
    let ack = comp.subscribe(1, &out, "live-sub", &sid, None);
    assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }), "{ack:?}");
    let mut phone = Phone {
        out,
        rx,
        seen: String::new(),
    };

    // GET /mcp, polled on the first subscribe
    live_wait(&comp, &mut phone, "the mcp op", |_, p| {
        p.count(r#""op":"mcp""#) > 0
    });
    let mcp: String = phone
        .seen
        .split(r#""op":"mcp""#)
        .nth(1)
        .unwrap_or_default()
        .chars()
        .take(160)
        .collect();
    eprintln!("LIVE: mcp → {mcp}");

    // 1) streaming deltas + a bash permission approved inline
    prompt(
        &comp,
        &sid,
        "live-1",
        "Use the bash tool to run exactly this command: echo hi > note.txt. Then reply with the single word DONE.",
    );
    live_wait(&comp, &mut phone, "the bash permission", |c, _| {
        pending(c, &sid).is_some()
    });
    let p = pending(&comp, &sid).unwrap();
    let call = p.tool_item_id.clone().expect("toolItemId = callID");
    let linked = format!(r#""permissionId":"{}""#, p.id);
    live_wait(&comp, &mut phone, "the card awaiting approval", |_, ph| {
        ph.seen.contains(&linked) && ph.seen.contains("awaiting-approval")
    });
    let labels: Vec<&str> = p.options.iter().map(|o| o.label.as_str()).collect();
    eprintln!(
        "LIVE: {} asks {:?} on {call}; options {labels:?}",
        p.id, p.command
    );
    decide(&comp, &p, "live-1-allow", "once", None);
    live_wait(&comp, &mut phone, "turn 1 completed", |c, _| {
        command_status(c, "live-1").as_deref() == Some("completed")
    });
    let appends = phone.count(r#""op":"append""#);
    assert!(appends > 0, "no streaming deltas reached the phone");
    assert!(phone.seen.contains(&format!(r#""id":"{call}""#)));
    eprintln!("LIVE: turn 1 completed; {appends} append ops so far");
    // isolation held: OpenCode loaded none of the user's skills
    let logs = iso.join("data").join("opencode").join("log");
    for entry in std::fs::read_dir(&logs)
        .expect("isolated log dir")
        .flatten()
    {
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        assert!(
            !text.contains("duplicate skill name"),
            "user skills leaked in"
        );
    }

    // 2) reject with a message: the card is denied, the turn goes on
    prompt(
        &comp,
        &sid,
        "live-2",
        "Use the bash tool to run exactly this command: echo bye",
    );
    live_wait(&comp, &mut phone, "the second permission", |c, _| {
        pending(c, &sid).is_some()
    });
    let p = pending(&comp, &sid).unwrap();
    decide(
        &comp,
        &p,
        "live-2-deny",
        "reject",
        Some("Not now - phone user declined."),
    );
    live_wait(&comp, &mut phone, "turn 2 completed", |c, ph| {
        command_status(c, "live-2").as_deref() == Some("completed")
            && ph.seen.contains(r#""status":"denied""#)
    });
    eprintln!("LIVE: reject with message → tool denied, turn completed");

    // 3) abort mid-stream: interrupted, not failed
    let before = phone.count(r#""op":"append""#);
    prompt(
        &comp,
        &sid,
        "live-3",
        "Write a 400-word story about a lighthouse keeper. Do not use any tools.",
    );
    live_wait(&comp, &mut phone, "the story streaming", |_, ph| {
        ph.count(r#""op":"append""#) >= before + 10
    });
    let reply = comp.handle_client_msg(ClientMsg::Interrupt {
        command_id: "live-stop".into(),
        session_id: sid.clone(),
    });
    assert!(
        matches!(reply, Some(ServerMsg::Ack { ok: true, .. })),
        "{reply:?}"
    );
    live_wait(&comp, &mut phone, "the abort outcome", |c, ph| {
        command_status(c, "live-3").as_deref() == Some("cancelled")
            && command_status(c, "live-stop").as_deref() == Some("completed")
            && ph.seen.contains(r#""status":"interrupted""#)
    });
    eprintln!("LIVE: abort → prompt cancelled, interrupt completed, turn interrupted");

    // conversation text never reached the journal
    {
        let store = comp.shared.storage.lock().unwrap();
        assert!(
            !store
                .events
                .iter()
                .any(|e| e.payload.contains("single word DONE"))
        );
    }
    comp.shutdown(true);
    drop(guard);
    let _ = std::fs::remove_dir_all(&base);
}

/* ------------------------------------------------------------------ */
/* ADR-021: OpenCode parity with Claude Code against a live server     */
/* ------------------------------------------------------------------ */

fn git(work: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=chernobyl",
            "-c",
            "user.email=t@example.invalid",
        ])
        .args(args)
        .current_dir(work)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// The ack of a create/prompt-like message, `Ok(session id)` or the error.
fn acked(reply: Option<ServerMsg>) -> Result<Option<String>, String> {
    match reply {
        Some(ServerMsg::Ack {
            ok: true,
            session_id,
            ..
        }) => Ok(session_id),
        Some(ServerMsg::Ack { error, .. }) => Err(error.unwrap_or_default()),
        other => Err(format!("{other:?}")),
    }
}

fn done(comp: &Companion, id: &str) -> bool {
    matches!(
        command_status(comp, id).as_deref(),
        Some("completed" | "failed" | "cancelled")
    )
}

/// ADR-021 live run: the parity features against `opencode serve` 1.19.4
/// and Ollama `qwen3.5:4b` (24k-context alias), everything isolated as in
/// `live_opencode_rich_parts`. The serve runs in a temp git repository,
/// which becomes the agent's workspace.
///
/// ```text
/// CHERNOBYL_LIVE_OPENCODE=1 cargo test --test opencode_e2e live_opencode_parity -- --ignored --nocapture
/// ```
#[test]
#[ignore = "live: set CHERNOBYL_LIVE_OPENCODE=1 (opencode CLI + local Ollama)"]
fn live_opencode_parity() {
    if std::env::var("CHERNOBYL_LIVE_OPENCODE").as_deref() != Ok("1") {
        eprintln!("skipped: set CHERNOBYL_LIVE_OPENCODE=1");
        return;
    }
    let exe = opencode_exe().expect("opencode CLI not found (set OPENCODE_BIN)");
    let root = std::env::var_os("CHERNOBYL_LIVE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let base = root.join(format!("chernobyl-opencode-parity-{}", now_ms()));
    let (iso, work, comp_dir) = (base.join("iso"), base.join("work"), base.join("companion"));
    let config = iso.join("config").join("opencode");
    for d in [&work, &comp_dir, &config] {
        std::fs::create_dir_all(d).expect("temp dirs");
    }
    // a repository with one commit: Changes has something to compare to
    git(&work, &["init", "-q"]);
    std::fs::write(work.join("README.md"), "parity\n").unwrap();
    git(&work, &["add", "README.md"]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let mut guard = LiveGuard { server: None };
    let created = ollama(
        "POST",
        "/api/create",
        &format!(
            r#"{{"model":"{LIVE_ALIAS}","from":"qwen3.5:4b","parameters":{{"num_ctx":24576}},"stream":false}}"#
        ),
    )
    .expect("Ollama at 127.0.0.1:11434");
    assert!(created.contains("success"), "alias: {created}");
    // the configured default is the plain 8k model, so a session that runs
    // on the 24k alias proves the phone's model choice reached OpenCode; a
    // config-defined slash command keeps /commands cheap
    let opencode_json = format!(
        r#"{{"$schema":"https://opencode.ai/config.json","autoupdate":false,"share":"disabled",
"model":"ollama/qwen3.5:4b","small_model":"ollama/{LIVE_ALIAS}",
"provider":{{"ollama":{{"npm":"@ai-sdk/openai-compatible","name":"Ollama",
"options":{{"baseURL":"{OLLAMA}/v1"}},
"models":{{"{LIVE_ALIAS}":{{"name":"qwen3.5 4b 24k","limit":{{"context":24576,"output":4096}}}},
"qwen3.5:4b":{{"name":"qwen3.5 4b","limit":{{"context":8192,"output":2048}}}}}}}}}},
"command":{{"hello":{{"description":"say hello","template":"Reply with exactly the word SLASHOK and then: $ARGUMENTS"}}}},
"permission":{{"bash":"ask","edit":"ask"}}}}"#
    );
    std::fs::write(config.join("opencode.json"), opencode_json).unwrap();
    let port = free_port();
    let addr = format!("http://127.0.0.1:{port}");
    guard.server = Some(spawn_server(
        &exe,
        port,
        &iso,
        &work,
        &base.join("serve.log"),
    ));
    let pid = guard.server.as_ref().map(|s| s.id());
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2700));
        eprintln!("live opencode parity: 45 min budget exceeded");
        if let Some(pid) = pid {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output();
        }
        std::process::exit(1);
    });
    let probe = OpenCodeAdapter::new("live-probe", &addr, None);
    assert!(
        wait_until(Duration::from_secs(60), || probe.refresh().is_ok()),
        "opencode serve never became healthy"
    );

    let mut c: CompanionConfig = default_config();
    c.data_dir = comp_dir.clone();
    c.probe_interval_ms = 500;
    c.terminal_enabled = true;
    c.adapters.push(AdapterEntryConfig {
        id: "oc-live".into(),
        kind: "opencode".into(),
        enabled: true,
        endpoint: Some(addr.clone()),
        basic_auth: None,
        ..Default::default()
    });
    let comp = Companion::boot(c).unwrap();

    // 1) the serve's folder is the workspace; the composer is advertised
    let build = comp
        .snapshot()
        .agents
        .into_iter()
        .find(|a| a.id == "build")
        .expect("build agent");
    let live = build.live.expect("live");
    assert_eq!(live.workspaces.len(), 1, "{:?}", live.workspaces);
    let ws = live.workspaces[0].clone();
    let canon = std::fs::canonicalize(&work).unwrap();
    assert!(
        canon.to_string_lossy().ends_with(&ws.path),
        "{} vs {}",
        canon.display(),
        ws.path
    );
    assert_eq!(
        live.composer,
        ["model", "images", "commands", "context", "rename"]
    );
    let alias = format!("ollama/{LIVE_ALIAS}");
    assert!(
        live.models.iter().any(|m| m.0 == alias),
        "{:?}",
        live.models
    );
    eprintln!(
        "LIVE: workspace {} ({}), {} models, composer {:?}",
        ws.id,
        ws.path,
        live.models.len(),
        live.composer
    );
    // a folder the phone picks is still refused
    let refused = acked(comp.handle_client_msg(ClientMsg::CreateSession {
        command_id: "p-refused".into(),
        agent_id: "build".into(),
        title: "x".into(),
        prompt: String::new(),
        project: Some("0123456789ab".into()),
        images: vec![],
        model: None,
    }));
    assert!(refused.is_err(), "{refused:?}");

    // 2) a session made outside the companion (`opencode run`, same data)
    let run = Command::new(&exe)
        .args([
            "run",
            "--dir",
            &work.to_string_lossy(),
            "--model",
            &alias,
            "Reply with exactly the word RUNOK and nothing else.",
        ])
        .env("HOME", iso.join("home"))
        .env("USERPROFILE", iso.join("home"))
        .env("XDG_DATA_HOME", iso.join("data"))
        .env("XDG_CONFIG_HOME", iso.join("config"))
        .env("XDG_CACHE_HOME", iso.join("cache"))
        .env("XDG_STATE_HOME", iso.join("state"))
        // `run` takes its folder from PWD (Git Bash exports the caller's)
        .env("PWD", &work)
        .current_dir(&work)
        .stdin(Stdio::null())
        .output()
        .expect("opencode run");
    eprintln!(
        "LIVE: opencode run exit {:?}, stdout {:?}",
        run.status.code(),
        String::from_utf8_lossy(&run.stdout).trim()
    );
    let mut ran = None;
    assert!(
        wait_until(Duration::from_secs(30), || {
            comp.pump();
            ran = comp
                .snapshot()
                .sessions
                .into_iter()
                .find(|s| s.host_id == "oc-live");
            ran.is_some()
        }),
        "the opencode run session never listed"
    );
    let ran = ran.unwrap();
    assert_eq!(ran.project, ws.path, "placed in the workspace");
    let (out, rx) = chernobyl_companion::server::Outbound::new(
        std::sync::Arc::new(chernobyl_companion::companion::Saturation::default()),
        false,
    );
    let mut phone = Phone {
        out,
        rx,
        seen: String::new(),
    };
    let ack = comp.subscribe(1, &phone.out, "sub-run", &ran.id, None);
    assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }), "{ack:?}");
    live_wait(&comp, &mut phone, "the run session's history", |_, p| {
        p.seen.contains("RUNOK and nothing else") && p.count(r#""kind":"text""#) > 0
    });
    eprintln!(
        "LIVE: `opencode run` session {} listed and hydrated",
        ran.id
    );

    // 3) create with a chosen model in the workspace; text streams
    let sid = acked(comp.handle_client_msg(ClientMsg::CreateSession {
        command_id: "p-create".into(),
        agent_id: "build".into(),
        title: "ADR-021 parity".into(),
        prompt: "Reply with exactly the word HELLO and nothing else.".into(),
        project: Some(ws.id.clone()),
        images: vec![],
        model: Some(alias.clone()),
    }))
    .expect("create")
    .expect("session id");
    let ack = comp.subscribe(1, &phone.out, "sub-1", &sid, None);
    assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }), "{ack:?}");
    live_wait(&comp, &mut phone, "the first turn", |c, _| {
        done(c, "p-create")
    });
    assert_eq!(
        command_status(&comp, "p-create").as_deref(),
        Some("completed")
    );
    let appends = phone.count(r#""op":"append""#);
    assert!(appends > 0, "no streaming deltas");
    // the answer ran on the chosen model, and the meter filled
    live_wait(&comp, &mut phone, "the context meter", |_, p| {
        p.seen.contains(r#""maxTokens":24576"#)
    });
    assert!(phone.seen.contains(&format!(r#""model":"{alias}""#)));
    let oc = comp.shared.opencode.lock().unwrap().clone().unwrap();
    let msgs = chernobyl_companion::json::stringify(&oc.messages(&sid).unwrap());
    assert!(
        msgs.contains(&format!(r#""modelID":"{LIVE_ALIAS}""#)),
        "the answer did not run on the chosen model"
    );
    eprintln!("LIVE: created {sid} on {alias}; {appends} appends; context reported");

    // 4) bash asks; allow once; the tool completes
    prompt(
        &comp,
        &sid,
        "p-bash",
        "Use the bash tool to run exactly this command: echo hi > note.txt. Then reply with the single word DONE.",
    );
    live_wait(&comp, &mut phone, "the bash permission", |c, _| {
        pending(c, &sid).is_some()
    });
    let p = pending(&comp, &sid).unwrap();
    let labels: Vec<&str> = p.options.iter().map(|o| o.label.as_str()).collect();
    eprintln!("LIVE: {} asks {:?}; options {labels:?}", p.id, p.command);
    decide(&comp, &p, "p-bash-allow", "once", None);
    live_wait(&comp, &mut phone, "the bash turn", |c, _| done(c, "p-bash"));
    assert!(phone.seen.contains(r#""status":"completed""#));
    assert!(work.join("note.txt").exists(), "the allowed command ran");

    // 5) a follow-up sent mid-turn is queued and runs after
    prompt(
        &comp,
        &sid,
        "p-q1",
        "Count from 1 to 20, separated by spaces.",
    );
    prompt(&comp, &sid, "p-q2", "Reply with exactly the word AFTER.");
    live_wait(&comp, &mut phone, "both queued turns", |c, _| {
        done(c, "p-q1") && done(c, "p-q2")
    });
    eprintln!(
        "LIVE: queued follow-up → {:?} / {:?}",
        command_status(&comp, "p-q1"),
        command_status(&comp, "p-q2")
    );
    assert_eq!(command_status(&comp, "p-q2").as_deref(), Some("completed"));

    // 6) Stop: interrupted, not failed
    let before = phone.count(r#""op":"append""#);
    prompt(
        &comp,
        &sid,
        "p-story",
        "Write a 400-word story about a lighthouse keeper. Do not use any tools.",
    );
    live_wait(&comp, &mut phone, "the story streaming", |_, p| {
        p.count(r#""op":"append""#) >= before + 10
    });
    let reply = comp.handle_client_msg(ClientMsg::Interrupt {
        command_id: "p-stop".into(),
        session_id: sid.clone(),
    });
    assert!(
        matches!(reply, Some(ServerMsg::Ack { ok: true, .. })),
        "{reply:?}"
    );
    live_wait(&comp, &mut phone, "the stop", |c, p| {
        command_status(c, "p-story").as_deref() == Some("cancelled")
            && p.seen.contains(r#""status":"interrupted""#)
    });
    eprintln!("LIVE: stop → prompt cancelled, turn interrupted");

    // 7) rename
    let renamed = acked(comp.handle_client_msg(ClientMsg::RenameSession {
        command_id: "p-rename".into(),
        session_id: sid.clone(),
        title: "Parity renamed".into(),
    }));
    assert!(renamed.is_ok(), "{renamed:?}");
    assert!(
        wait_until(Duration::from_secs(10), || {
            comp.pump();
            comp.snapshot()
                .sessions
                .iter()
                .any(|s| s.id == sid && s.title == "Parity renamed")
        }),
        "rename never showed"
    );
    assert_eq!(
        command_status(&comp, "p-rename").as_deref(),
        Some("completed")
    );

    // 8) set_model, then a slash command (config-defined /hello)
    let set = acked(comp.handle_client_msg(ClientMsg::SetModel {
        command_id: "p-model".into(),
        session_id: sid.clone(),
        model: alias.clone(),
    }));
    assert!(set.is_ok(), "{set:?}");
    let commands: String = phone
        .seen
        .split(r#""slashCommands":"#)
        .last()
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect();
    eprintln!("LIVE: slash commands {commands}");
    assert!(phone.seen.contains(r#""hello""#));
    prompt(&comp, &sid, "p-slash", "/hello from the phone");
    live_wait(&comp, &mut phone, "the slash command", |c, _| {
        done(c, "p-slash")
    });
    eprintln!(
        "LIVE: /hello → {:?}; SLASHOK seen: {}",
        command_status(&comp, "p-slash"),
        phone.seen.contains("SLASHOK")
    );
    assert_eq!(
        command_status(&comp, "p-slash").as_deref(),
        Some("completed")
    );

    // 9) a photo rides along (the 4B model cannot see it; OpenCode says so)
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEUlEQVR4nGP4z8DwnwGMgRQAH+4D/dJQfRoAAAAASUVORK5CYII=";
    let reply = comp.handle_client_msg(ClientMsg::SubmitPrompt {
        command_id: "p-photo".into(),
        session_id: sid.clone(),
        text: "What color is this image? One word.".into(),
        images: vec![chernobyl_companion::protocol::PromptImage {
            media_type: "image/png".into(),
            data: png.into(),
        }],
        fork: false,
    });
    assert!(
        matches!(reply, Some(ServerMsg::Ack { ok: true, .. })),
        "{reply:?}"
    );
    live_wait(&comp, &mut phone, "the photo turn", |c, p| {
        done(c, "p-photo") && p.seen.contains(r#""images":1"#)
    });
    eprintln!(
        "LIVE: photo prompt → {:?}",
        command_status(&comp, "p-photo")
    );

    // 10) fork: a new session with the history, the original untouched
    let fork = acked(comp.handle_client_msg(ClientMsg::SubmitPrompt {
        command_id: "p-fork".into(),
        session_id: sid.clone(),
        text: "Reply with exactly the word FORKOK.".into(),
        images: vec![],
        fork: true,
    }))
    .expect("fork")
    .expect("fork id");
    assert_ne!(fork, sid);
    let forked = comp
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.id == fork)
        .expect("fork listed");
    assert_eq!(forked.forked_from.as_deref(), Some("Parity renamed"));
    live_wait(&comp, &mut phone, "the fork's turn", |c, _| {
        done(c, "p-fork")
    });
    eprintln!(
        "LIVE: forked {sid} → {fork}: {:?}",
        command_status(&comp, "p-fork")
    );

    // 11) Files and Changes in the serve's folder
    let query = |cmd: &str, op: WorkspaceOp| {
        let (result, ack) = comp.workspace_query(cmd, &ws.id, &op);
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }), "{ack:?}");
        format!("{result:?}")
    };
    let list = query(
        "w-list",
        WorkspaceOp::FsList {
            path: String::new(),
        },
    );
    assert!(
        list.contains("note.txt") && list.contains("README.md"),
        "{list}"
    );
    let read = query(
        "w-read",
        WorkspaceOp::FsRead {
            path: "README.md".into(),
        },
    );
    // (note.txt may be UTF-16: OpenCode's shell is PowerShell 5.1 when no
    // SHELL is set, and its `>` writes UTF-16; fs_read calls that binary)
    assert!(read.contains("parity"), "{read}");
    let status = query("w-git", WorkspaceOp::GitStatus);
    assert!(status.contains("note.txt"), "{status}");
    eprintln!(
        "LIVE: fs_list, fs_read and git_status answer in {}",
        ws.path
    );

    // 12) the terminal runs in the same folder
    let seen: std::sync::Arc<std::sync::Mutex<Vec<ServerMsg>>> = Default::default();
    let sink = seen.clone();
    let reply = comp.handle_client_msg_with(
        ClientMsg::Workspace {
            command_id: "t-echo".into(),
            workspace_id: ws.id.clone(),
            op: WorkspaceOp::TerminalRun {
                command: "echo parity-terminal && dir /b".into(),
            },
        },
        Some(std::sync::Arc::new(move |m: ServerMsg| {
            sink.lock().unwrap().push(m);
            true
        })),
    );
    assert!(
        matches!(reply, Some(ServerMsg::Ack { ok: true, .. })),
        "{reply:?}"
    );
    assert!(
        wait_until(Duration::from_secs(30), || done(&comp, "t-echo")),
        "terminal never finished"
    );
    let text: String = seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| match m {
            ServerMsg::TerminalOutput { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("parity-terminal") && text.contains("note.txt"),
        "{text}"
    );
    eprintln!("LIVE: terminal echo ran in the workspace");

    // isolation held, nothing of the conversation reached the journal
    let logs = iso.join("data").join("opencode").join("log");
    for entry in std::fs::read_dir(&logs)
        .expect("isolated log dir")
        .flatten()
    {
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        assert!(
            !text.contains("duplicate skill name"),
            "user skills leaked in"
        );
    }
    {
        let store = comp.shared.storage.lock().unwrap();
        assert!(
            !store
                .events
                .iter()
                .any(|e| e.payload.contains("lighthouse"))
        );
    }
    comp.shutdown(true);
    drop(guard);
    let _ = std::fs::remove_dir_all(&base);
}
