//! Claude Code live runtime (ADR-015 §4.2, step 6 part 2, and the Claude half
//! of step 8): `claude.exe` processes the phone starts in operator-allow-listed
//! workspaces, behind the [`LiveRuntime`] router. The protocol core is
//! `claude_live::Driver`; this module owns the processes, the per-session
//! timers, the transcript hub pushes and the G1 command lifecycles (§4.6).
//!
//! - One [`Driver`] per session for its whole life: the CLI restores its
//!   cumulative cost on `--resume`, so a respawn keeps the driver.
//! - One reader thread per process. It decodes stdout under the runtime lock
//!   and pushes transcript ops while still holding it (the hub is a leaf
//!   lock), so a decision's ops never overtake the frames that follow it.
//!   Engine events (permissions, sessions, command progress and results) go
//!   to the sink after the lock is released: the engine lock is never taken
//!   under ours.
//! - The same thread runs the session's timers on every `tick`: interrupt
//!   timeouts, MCP polls, sign-in expiry and the idle reaper.
//! - A process idle for 30 min with no subscriber and nothing pending gets
//!   its stdin closed; the next command respawns it with `--resume`.
//! - Every session on disk in a workspace (`claude_history`, scanned on the
//!   probe cadence) is listed `idle`; the first subscriber gets its history,
//!   and a prompt adopts it (`--resume` in its recorded cwd) unless another
//!   process wrote it in the last two minutes.
//!
//! Frames carry tool input and OAuth codes: nothing here logs them.

use crate::adapters::{AdapterError, Capability, LiveRuntime};
use crate::claude_history::{self, Found, IN_USE, IN_USE_MS, Scanner, Tail};
use crate::claude_live::{
    self, Callback, Decision, Driver, Event, PERMISSION_MODES, PromptState, is_uuid, new_uuid,
    spawn_args, spawn_env,
};
use crate::codex_runtime::Sink;
use crate::config::AdapterEntryConfig;
use crate::json::{self, Json};
use crate::protocol::{
    Agent, AgentLive, ClientMsg, DecisionError, EventKind, FileChange, LiveWorkspace,
    PermissionRequest, PromptImage, ServerMsg, Session, is_model, now_ms,
};
use crate::runtime_io::{
    AUTH_TTL_MS, ChildEvent, ChildIo, KILL_GRACE, PendingAuth, check_callback_url,
    open_url_on_desktop,
};
use crate::transcript::{HubHandle, Item, ItemKind, McpAuthState, McpServer, Op, ToolStatus};
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Ended sessions kept for routing ("this session has ended") and the list.
const ENDED_KEPT: usize = 32;
/// Every this many desktop sign-in polls, an `mcp_reconnect` too: research
/// run 4 needed one after the callback (unverified for the CLI's own
/// listener, §4.7).
const RECONNECT_EVERY: u64 = 5;
/// ADR-018: hook requests waiting for the phone at once; more get no
/// answer from here (the PC asks as usual).
pub const HOOKS_MAX: usize = 16;
/// Hook requests that ended without the phone, kept to refuse a late tap
/// honestly.
const GONE_KEPT: usize = 32;
/// A hook request outlives its wait by this much in the engine.
const HOOK_GRACE_MS: i64 = 2_000;
pub const ANSWERED_ON_PC: &str = "This request was already answered on the PC";
pub const HOOK_EXPIRED: &str = "This request timed out; the PC asked again there";

/* ------------------------------------------------------------------ */
/* Configuration                                                       */
/* ------------------------------------------------------------------ */

/// A `claude` adapter entry with `workspaces`, checked (§4.2.3).
#[derive(Debug, Clone)]
pub struct LiveConfig {
    pub workspaces: Vec<LiveWorkspace>,
    pub binary: PathBuf,
    pub max_sessions: usize,
    pub permission_mode: Option<String>,
    /// Flags after the SDK set. Never from the config file or the phone (no
    /// bypass mode is reachable, T1); the live test passes `--model haiku`.
    pub extra_args: Vec<String>,
    /// Claude's `projects` folder, where every session's transcript lives
    /// (`claude_history`); None lists only the sessions started here.
    pub projects: Option<PathBuf>,
}

impl LiveConfig {
    /// `Ok(None)`: no `workspaces`, the entry stays a hook ingress. `Err`
    /// names the workspace or binary that failed; the caller logs it and the
    /// entry stays a hook ingress.
    pub fn from_entry(
        entry: &AdapterEntryConfig,
        data_dir: &Path,
    ) -> Result<Option<LiveConfig>, String> {
        if entry.workspaces.is_empty() {
            return Ok(None);
        }
        let env = |k: &str| std::env::var_os(k);
        let profile = env("USERPROFILE").map(PathBuf::from);
        let workspaces = entry
            .workspaces
            .iter()
            .map(|w| claude_live::validate_workspace(w, data_dir, profile.as_deref()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("adapter {}: {e}; live sessions are off", entry.id))?;
        let binary = claude_live::resolve_binary(
            entry.binary.as_deref().map(Path::new),
            env("USERPROFILE").as_deref(),
            env("PATH").as_deref(),
        )
        .map_err(|e| format!("adapter {}: {e}; live sessions are off", entry.id))?;
        Ok(Some(LiveConfig {
            workspaces,
            binary,
            max_sessions: entry.max_sessions.unwrap_or(3) as usize,
            permission_mode: entry.permission_mode.clone(),
            extra_args: Vec::new(),
            projects: claude_history::projects_dir(
                env("CLAUDE_CONFIG_DIR").as_deref(),
                profile.as_deref().map(Path::as_os_str),
            ),
        }))
    }
}

/// Every duration the runtime waits on; tests shrink them.
#[derive(Debug, Clone)]
pub struct Timing {
    /// `create_session` rate limit per adapter.
    pub create_every_ms: i64,
    pub idle_reap_ms: i64,
    /// An interrupt with no turn end by then is `unknown`.
    pub interrupt_ms: i64,
    /// `mcp_status` while a session is subscribed.
    pub mcp_poll_ms: i64,
    /// `mcp_status` while a sign-in or reconnect is in flight.
    pub auth_poll_ms: i64,
    pub auth_ttl_ms: i64,
    /// Reader wakeup for the timers.
    pub tick: Duration,
    /// ADR-018: the live mirror's poll of a transcript the PC writes.
    pub mirror_every: Duration,
    /// ADR-018: a mirrored transcript quiet this long is no longer live.
    pub mirror_idle_ms: i64,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            create_every_ms: 5_000,
            idle_reap_ms: 30 * 60 * 1000,
            interrupt_ms: 30_000,
            mcp_poll_ms: 30_000,
            auth_poll_ms: 2_000,
            auth_ttl_ms: AUTH_TTL_MS,
            tick: Duration::from_millis(250),
            mirror_every: Duration::from_millis(750),
            mirror_idle_ms: IN_USE_MS,
        }
    }
}

/// Starts one process: (argv after the binary, cwd).
pub type Spawn =
    dyn Fn(&[String], &Path) -> io::Result<(ChildIo, Receiver<ChildEvent>)> + Send + Sync;
/// Opens a runtime-produced sign-in URL on the desktop.
pub type Open = dyn Fn(&str) -> Result<(), &'static str> + Send + Sync;
/// The health probe (`claude --version`).
pub type Probe = dyn Fn() -> Result<(), String> + Send + Sync;

/* ------------------------------------------------------------------ */
/* State                                                               */
/* ------------------------------------------------------------------ */

/// An MCP command waiting for a server status.
struct Wait {
    command_id: String,
    server: String,
    deadline: i64,
    /// `mcp_auth`: completes when the server is connected, expires otherwise.
    auth: Option<Auth>,
    /// `mcp_reconnect` / `mcp_auth_callback`: the reconnect was answered, so
    /// the next status decides.
    replied: bool,
    /// `mcp_auth_callback`: failing it fails the sign-in too (single use).
    callback: bool,
}

struct Auth {
    phone: bool,
    /// The runtime answered `mcp_authenticate`: (url for the phone, prefix).
    started: Option<(Option<String>, Option<String>)>,
}

struct Live {
    session: Session,
    driver: Driver,
    /// None: never started, reaped or exited.
    io: Option<ChildIo>,
    /// Process generation: a reader of an older process stops.
    generation: u64,
    ended: bool,
    reaping: bool,
    last_active: i64,
    last_mcp_poll: i64,
    auth_polls: u64,
    /// `create_session`: (command, completes on initialize because no prompt).
    create: Option<(String, bool)>,
    interrupts: Vec<(String, i64)>,
    end: Option<String>,
    /// Highest `command_progress` sent per command (accepted 1, running 2).
    progress: Vec<(String, u8)>,
    mcp: Vec<McpServer>,
    waits: Vec<Wait>,
    callbacks: PendingAuth<Callback>,
    /// Copies of the pending requests, to resolve them `stale` by id.
    asked: Vec<PermissionRequest>,
    /// ADR-018: the session this one forks (`--resume <id> --fork-session`
    /// on its first start only).
    fork_of: Option<String>,
}

impl Live {
    /// No process yet: `start` spawns one.
    fn new(session: Session, driver: Driver, now: i64) -> Live {
        Live {
            session,
            driver,
            io: None,
            generation: 0,
            ended: false,
            reaping: false,
            last_active: now,
            last_mcp_poll: now,
            auth_polls: 0,
            create: None,
            interrupts: Vec::new(),
            end: None,
            progress: Vec::new(),
            mcp: Vec::new(),
            waits: Vec::new(),
            callbacks: PendingAuth::default(),
            asked: Vec::new(),
            fork_of: None,
        }
    }
}

#[derive(Default)]
struct State {
    sessions: Vec<Live>,
    last_create: Option<i64>,
    /// Sessions on disk in a workspace that are not in `sessions`: idle and
    /// resumable (`claude_history`). A command adopts one into `sessions`.
    found: Vec<Found>,
    scanned: bool,
    /// ADR-018: found sessions whose transcript a mirror thread follows.
    mirrors: Vec<String>,
    /// ADR-018: `PermissionRequest` hooks waiting for the phone.
    hooks: Vec<HookAsk>,
    /// Hook requests that ended without the phone: (id, why).
    gone: VecDeque<(String, &'static str)>,
}

/// ADR-018: one `PermissionRequest` hook from a Claude Code session on the
/// PC; its HTTP request waits on the other end of `reply`.
struct HookAsk {
    request: PermissionRequest,
    reply: mpsc::Sender<HookVerdict>,
}

/// The phone's answer to a hook request.
#[derive(Debug, Clone, PartialEq)]
pub enum HookVerdict {
    Allow,
    Deny(String),
}

/// Why a hook request ended without the phone's answer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HookEnd {
    /// The wait ran out: the hook prints nothing, the PC asks.
    Expired,
    /// The hook went away first: the PC's own prompt was answered (or the
    /// session moved on).
    AnsweredOnPc,
}

struct Inner {
    id: String,
    cfg: LiveConfig,
    hub: HubHandle,
    timing: Timing,
    spawn: Box<Spawn>,
    open: Box<Open>,
    probe: Box<Probe>,
    sink: Mutex<Option<Sink>>,
    healthy: AtomicBool,
    stop: AtomicBool,
    state: Mutex<State>,
    scanner: Mutex<Scanner>,
}

pub struct ClaudeRuntime {
    inner: Arc<Inner>,
}

fn tx(io: &Option<ChildIo>, frame: String) -> Result<(), &'static str> {
    io.as_ref()
        .ok_or("Claude Code is not running")?
        .try_send(frame)
}

fn ack(command_id: &str, ok: bool, error: Option<String>, session_id: Option<&str>) -> ServerMsg {
    ServerMsg::Ack {
        command_id: command_id.to_string(),
        ok,
        error,
        duplicate: false,
        session_id: session_id.map(str::to_string),
        permission_id: None,
    }
}

fn refuse(command_id: &str, error: impl Into<String>) -> Result<ServerMsg, AdapterError> {
    Ok(ack(command_id, false, Some(error.into()), None))
}

/// What a `create_session` asks for.
struct New<'a> {
    title: &'a str,
    prompt: &'a str,
    project: Option<&'a str>,
    images: &'a [PromptImage],
    model: Option<&'a str>,
}

/* ------------------------------------------------------------------ */
/* Construction + LiveRuntime                                          */
/* ------------------------------------------------------------------ */

impl ClaudeRuntime {
    /// The production runtime: real processes, the desktop browser opener
    /// and `claude --version` as the health probe.
    pub fn new(id: &str, cfg: LiveConfig, hub: HubHandle) -> ClaudeRuntime {
        let (bin, bin2) = (cfg.binary.clone(), cfg.binary.clone());
        let spawn: Box<Spawn> = Box::new(move |args: &[String], cwd: &Path| {
            ChildIo::spawn(&bin, args, cwd, &spawn_env(std::env::vars_os()))
        });
        let probe: Box<Probe> = Box::new(move || version(&bin2));
        Self::with_parts(
            id,
            cfg,
            hub,
            Timing::default(),
            spawn,
            Box::new(open_url_on_desktop),
            probe,
        )
    }

    pub fn with_parts(
        id: &str,
        cfg: LiveConfig,
        hub: HubHandle,
        timing: Timing,
        spawn: Box<Spawn>,
        open: Box<Open>,
        probe: Box<Probe>,
    ) -> ClaudeRuntime {
        ClaudeRuntime {
            inner: Arc::new(Inner {
                id: id.to_string(),
                cfg,
                hub,
                timing,
                spawn,
                open,
                probe,
                sink: Mutex::new(None),
                healthy: AtomicBool::new(false),
                stop: AtomicBool::new(false),
                state: Mutex::new(State::default()),
                scanner: Mutex::new(Scanner::default()),
            }),
        }
    }

    /// Where engine-bound events go (permissions are ingested, the rest
    /// published); the companion sets it before routing anything here.
    pub fn set_sink(&self, sink: Sink) {
        *self.inner.sink.lock().unwrap() = Some(sink);
    }

    /// ADR-018: one `PermissionRequest` hook payload from a Claude Code
    /// session on the PC becomes an engine permission request (allow once
    /// or deny; no rules) that expires after `wait_ms`. The caller waits on
    /// the receiver. `None`: not the phone's to answer (a cwd outside the
    /// workspaces, a session this companion drives, a question or a plan,
    /// which need the PC's own dialog, or too many waiting); the hook then
    /// prints nothing and the PC asks as usual.
    pub fn hook_permission(
        &self,
        payload: &Json,
        wait_ms: i64,
    ) -> Option<(String, Receiver<HookVerdict>)> {
        let inner = &self.inner;
        let now = now_ms();
        let tool = json::get_str(payload, "tool_name").filter(|t| !t.is_empty())?;
        if matches!(tool, "AskUserQuestion" | "ExitPlanMode") {
            return None;
        }
        let input = json::get(payload, "tool_input").filter(|i| matches!(i, Json::Obj(_)))?;
        let sid = json::get_str(payload, "session_id").filter(|s| is_uuid(s))?;
        let cwd = claude_history::inside(json::get_str(payload, "cwd")?, &inner.cfg.workspaces)?;
        let known = {
            let st = inner.state.lock().unwrap();
            if st.sessions.iter().any(|l| l.session.id == sid) || st.hooks.len() >= HOOKS_MAX {
                return None;
            }
            st.found.iter().any(|f| f.id == sid)
        };
        if !known {
            inner.discover(); // a session started since the last scan
        }
        let rid = format!("hook-{}", new_uuid().ok()?);
        let mut fields = vec![
            ("tool_name", json::s(tool)),
            ("input", input.clone()),
            // the hook path never writes a rule (no persistent grants)
            ("suppress_always_allow_rule", Json::Bool(true)),
        ];
        if let Some(d) = json::get_str(input, "description") {
            fields.push(("description", json::s(d)));
        }
        let req = json::obj(fields);
        let raw = json::stringify(&req);
        let asked =
            claude_live::permission_request(&inner.id, sid, &cwd, &rid, &req, &raw, now).ok()?;
        let mut pr = asked.request;
        pr.from_pc = true;
        // a little after the hook gives up, so the runtime ends it first
        pr.expires_at = now + wait_ms + HOOK_GRACE_MS;
        if pr.context.is_empty() {
            pr.context = "Asked by Claude Code on the PC".into();
        }
        let (tx, rx) = mpsc::channel();
        let id = pr.id.clone();
        {
            let mut st = inner.state.lock().unwrap();
            if st.hooks.len() >= HOOKS_MAX {
                return None;
            }
            st.hooks.push(HookAsk {
                request: pr.clone(),
                reply: tx,
            });
        }
        inner.publish(vec![EventKind::PermissionRequested(pr)]);
        Some((id, rx))
    }

    /// ADR-018: why a hook request ended without the phone, if it did.
    pub fn hook_gone(&self, permission_id: &str) -> Option<&'static str> {
        let st = self.inner.state.lock().unwrap();
        st.gone
            .iter()
            .find(|(id, _)| id == permission_id)
            .map(|(_, why)| *why)
    }

    /// ADR-018: a hook request ended without the phone's answer; the phone
    /// sees it `expired` or `stale`, and a late tap is refused with why.
    pub fn hook_ended(&self, permission_id: &str, why: HookEnd) {
        let ended = {
            let mut st = self.inner.state.lock().unwrap();
            let Some(at) = st.hooks.iter().position(|h| h.request.id == permission_id) else {
                return;
            };
            let h = st.hooks.remove(at);
            let text = match why {
                HookEnd::Expired => HOOK_EXPIRED,
                HookEnd::AnsweredOnPc => ANSWERED_ON_PC,
            };
            st.gone.push_back((h.request.id.clone(), text));
            if st.gone.len() > GONE_KEPT {
                st.gone.pop_front();
            }
            h.request
        };
        let mut p = ended;
        p.status = match why {
            HookEnd::Expired => "expired",
            HookEnd::AnsweredOnPc => "stale",
        }
        .into();
        self.inner.publish(vec![EventKind::PermissionResolved(p)]);
    }
}

/// `claude --version` through the same scrubbed, windowless spawn path.
fn version(bin: &Path) -> Result<(), String> {
    let env = spawn_env(std::env::vars_os());
    let (io, rx) = ChildIo::spawn(bin, &["--version"], &std::env::temp_dir(), &env)
        .map_err(|e| format!("runtime-unavailable: claude --version: {e}"))?;
    loop {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(ChildEvent::Exit(Some(0))) => return Ok(()),
            Ok(ChildEvent::Exit(code)) => {
                return Err(format!(
                    "runtime-unavailable: claude --version exited with {code:?}: {}",
                    io.stderr_tail()
                ));
            }
            Ok(_) => {}
            Err(_) => return Err("runtime-unavailable: claude --version did not answer".into()),
        }
    }
}

impl LiveRuntime for ClaudeRuntime {
    fn id(&self) -> &str {
        &self.inner.id
    }

    fn is_healthy(&self) -> bool {
        self.inner.healthy.load(Ordering::SeqCst)
    }

    fn agents(&self) -> Vec<Agent> {
        let healthy = self.is_healthy();
        let busy = {
            let st = self.inner.state.lock().unwrap();
            st.sessions
                .iter()
                .any(|l| matches!(l.session.status.as_str(), "running" | "waiting"))
        };
        let state = match (healthy, busy) {
            (false, _) => "offline",
            (true, true) => "busy",
            (true, false) => "online",
        };
        vec![Agent {
            id: self.inner.id.clone(),
            host_id: self.inner.id.clone(),
            kind: "claude-code".into(),
            name: "Claude Code".into(),
            state: state.into(),
            capabilities: vec!["live".into()],
            accepts_new_session: healthy,
            simulated: false,
            live: Some(AgentLive {
                prompt_while_busy: "queue", // the CLI usually runs a mid-turn follow-up as its own turn (emulator row 9.5)
                interrupt: true,
                end: true,
                permission_modes: PERMISSION_MODES.iter().map(|m| m.to_string()).collect(),
                workspaces: self.inner.cfg.workspaces.clone(),
                composer: vec!["model", "images", "commands", "context", "rename"],
            }),
        }]
    }

    /// Started or adopted here, then the ones found on disk (newest first).
    fn sessions(&self) -> Vec<Session> {
        let st = self.inner.state.lock().unwrap();
        let live = st.sessions.iter().map(|l| l.session.clone());
        live.chain(st.found.iter().map(|f| self.inner.found_session(f)))
            .collect()
    }

    fn owns_session(&self, session_id: &str) -> bool {
        let st = self.inner.state.lock().unwrap();
        st.sessions.iter().any(|l| l.session.id == session_id)
            || st.found.iter().any(|f| f.id == session_id)
    }

    fn drives_session(&self, session_id: &str) -> bool {
        let st = self.inner.state.lock().unwrap();
        st.sessions.iter().any(|l| l.session.id == session_id)
    }

    fn knows_permission(&self, permission_id: &str) -> bool {
        let st = self.inner.state.lock().unwrap();
        st.sessions
            .iter()
            .any(|l| l.driver.knows_permission(permission_id))
            || st.hooks.iter().any(|h| h.request.id == permission_id)
            || st.gone.iter().any(|(id, _)| id == permission_id)
    }

    fn send(&self, msg: &ClientMsg) -> Result<ServerMsg, AdapterError> {
        let mut out = Vec::new();
        let reply = {
            let mut st = self.inner.state.lock().unwrap();
            self.inner.command(&mut st, msg, now_ms(), &mut out)
        };
        self.inner.publish(out);
        reply
    }

    /// The first subscriber: MCP status at the next tick, or the history of
    /// a session found on disk (and, while the PC writes it, the mirror).
    fn on_subscribe(&self, session_id: &str) {
        let mut st = self.inner.state.lock().unwrap();
        if let Some(live) = st.sessions.iter_mut().find(|l| l.session.id == session_id) {
            live.last_mcp_poll = 0;
            return;
        }
        let Some(f) = st.found.iter().find(|f| f.id == session_id).cloned() else {
            return;
        };
        drop(st);
        self.inner.hydrate(&f);
    }

    /// The probe, then the sessions on disk (the probe cadence, not every
    /// snapshot).
    fn refresh(&self) -> Result<(), AdapterError> {
        let result = (self.inner.probe)();
        self.inner.healthy.store(result.is_ok(), Ordering::SeqCst);
        self.inner.discover();
        result.map_err(AdapterError::RuntimeUnavailable)
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![
            Capability::new("session.create", "available", None),
            Capability::new("session.prompt", "available", None),
            Capability::new("session.interrupt", "available", None),
            Capability::new("permission.review", "available", None),
            Capability::new("session.activity.stream", "available", None),
            Capability::new(
                "session.history.read",
                "limited",
                Some(
                    "read from Claude Code's transcripts on the PC: the last 4 MiB of a \
                     session, at most 300 items, redacted",
                ),
            ),
            Capability::new(
                "session.list",
                "limited",
                Some(
                    "the 50 most recent sessions in the allow-listed workspaces, from the \
                     phone, a terminal or VS Code; one in use on the PC is not resumed",
                ),
            ),
            Capability::new(
                "server.spawn",
                "limited",
                Some("claude.exe is started only in workspaces the operator allow-listed"),
            ),
            Capability::new(
                "mcp.auth",
                "limited",
                Some(
                    "phone-callback sign-in verified against a mock OAuth server; whether the \
                     CLI's own desktop listener reconnects by itself is unverified, so the \
                     companion polls and reconnects",
                ),
            ),
        ]
    }

    fn dispose(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        let mut st = self.inner.state.lock().unwrap();
        for live in &mut st.sessions {
            live.io = None; // closes stdin, kills after the grace
        }
        st.hooks.clear(); // every waiting hook answers nothing: the PC asks
    }
}

/* ------------------------------------------------------------------ */
/* Commands                                                            */
/* ------------------------------------------------------------------ */

impl Inner {
    fn publish(&self, out: Vec<EventKind>) {
        if out.is_empty() {
            return;
        }
        let sink = self.sink.lock().unwrap().clone();
        if let Some(sink) = sink {
            for kind in out {
                sink(kind);
            }
        }
    }

    fn command(
        self: &Arc<Self>,
        st: &mut State,
        msg: &ClientMsg,
        now: i64,
        out: &mut Vec<EventKind>,
    ) -> Result<ServerMsg, AdapterError> {
        if let ClientMsg::CreateSession {
            command_id,
            title,
            prompt,
            project,
            images,
            model,
            ..
        } = msg
        {
            let new = New {
                title,
                prompt,
                project: project.as_deref(),
                images,
                model: model.as_deref(),
            };
            return self.create(st, command_id, new, now, out);
        }
        if let ClientMsg::PermissionDecision {
            command_id,
            permission_id,
            decision,
            option_id,
            message,
            answers,
            ..
        } = msg
        {
            // ADR-018: a hook request from a session on the PC
            if let Some(at) = st.hooks.iter().position(|h| h.request.id == *permission_id) {
                let h = st.hooks.remove(at);
                let verdict = if decision == "reject" {
                    HookVerdict::Deny(
                        message
                            .as_deref()
                            .map(str::trim)
                            .filter(|m| !m.is_empty())
                            .unwrap_or("Denied from the phone")
                            .to_string(),
                    )
                } else {
                    HookVerdict::Allow
                };
                if h.reply.send(verdict).is_err() {
                    // the hook went away just now: the PC answered first
                    st.gone.push_back((h.request.id.clone(), ANSWERED_ON_PC));
                    let mut p = h.request;
                    p.status = "stale".into();
                    out.push(EventKind::PermissionResolved(p));
                    return refuse(command_id, ANSWERED_ON_PC);
                }
                let mut reply = ack(command_id, true, None, Some(&h.request.session_id));
                if let ServerMsg::Ack {
                    permission_id: p, ..
                } = &mut reply
                {
                    *p = Some(permission_id.clone());
                }
                return Ok(reply);
            }
            if let Some((_, why)) = st.gone.iter().find(|(id, _)| id == permission_id) {
                return refuse(command_id, *why);
            }
            let Some(live) = st
                .sessions
                .iter_mut()
                .find(|l| l.driver.knows_permission(permission_id))
            else {
                return refuse(command_id, DecisionError::NotPending.message());
            };
            let d = Decision {
                permission_id,
                decision,
                option_id: option_id.as_deref(),
                message: message.as_deref(),
                answers,
            };
            let events = match live.driver.decide(&d, |f| tx(&live.io, f)) {
                Ok(events) => events,
                Err(e) => return refuse(command_id, e),
            };
            live.asked.retain(|p| p.id != *permission_id);
            live.last_active = now;
            self.apply(live, events, now, out);
            let mut reply = ack(command_id, true, None, Some(&live.session.id));
            if let ServerMsg::Ack {
                permission_id: p, ..
            } = &mut reply
            {
                *p = Some(permission_id.clone());
            }
            return Ok(reply);
        }
        let (command_id, session_id) = match msg {
            ClientMsg::SubmitPrompt {
                command_id,
                session_id,
                ..
            }
            | ClientMsg::Interrupt {
                command_id,
                session_id,
            }
            | ClientMsg::SessionEnd {
                command_id,
                session_id,
            }
            | ClientMsg::SetPermissionMode {
                command_id,
                session_id,
                ..
            }
            | ClientMsg::McpAuth {
                command_id,
                session_id,
                ..
            }
            | ClientMsg::McpAuthCallback {
                command_id,
                session_id,
                ..
            }
            | ClientMsg::McpReconnect {
                command_id,
                session_id,
                ..
            }
            | ClientMsg::SetModel {
                command_id,
                session_id,
                ..
            }
            | ClientMsg::RenameSession {
                command_id,
                session_id,
                ..
            } => (command_id, session_id),
            _ => {
                return Err(AdapterError::Unsupported(
                    "session-neutral messages stay with the engine".into(),
                ));
            }
        };
        if let ClientMsg::SubmitPrompt {
            text,
            images,
            fork: true,
            ..
        } = msg
        {
            return self.fork(st, command_id, session_id, text, images, now, out);
        }
        if !st.sessions.iter().any(|l| l.session.id == *session_id)
            && let Some(at) = st.found.iter().position(|f| f.id == *session_id)
        {
            // nothing of ours runs in it: stopping or ending it is a no-op
            if matches!(
                msg,
                ClientMsg::Interrupt { .. } | ClientMsg::SessionEnd { .. }
            ) {
                out.push(EventKind::CommandResult {
                    command_id: command_id.clone(),
                    status: "completed".into(),
                    error: None,
                    session_id: Some(session_id.clone()),
                    permission_id: None,
                });
                return Ok(ack(command_id, true, None, Some(session_id)));
            }
            if let Err(e) = self.adopt(st, at, now) {
                return refuse(command_id, e);
            }
        }
        let running = st.sessions.iter().filter(|l| l.io.is_some()).count();
        let Some(live) = st.sessions.iter_mut().find(|l| l.session.id == *session_id) else {
            return Err(AdapterError::Unsupported("unknown session".into()));
        };
        live.last_active = now;
        if live.ended {
            if let ClientMsg::SessionEnd { .. } = msg {
                self.result(live, out, command_id, "completed", None);
                return Ok(ack(command_id, true, None, Some(session_id)));
            }
            return refuse(command_id, "this Claude Code session has ended");
        }
        match msg {
            ClientMsg::Interrupt { .. } => {
                if live.io.is_none() || !live.driver.turn_running() {
                    self.result(live, out, command_id, "completed", None);
                } else {
                    if let Err(e) = live.driver.interrupt(command_id, |f| tx(&live.io, f)) {
                        return refuse(command_id, e);
                    }
                    let deadline = now + self.timing.interrupt_ms;
                    live.interrupts.push((command_id.clone(), deadline));
                }
            }
            ClientMsg::SessionEnd { .. } => match &live.io {
                None => {
                    self.finish(live, "completed", now, out);
                    self.result(live, out, command_id, "completed", None);
                }
                Some(io) => {
                    if live.driver.turn_running() {
                        // the reply is ignored: this command is not an interrupt
                        let _ = live.driver.interrupt(command_id, |f| io.try_send(f));
                    }
                    io.close_stdin();
                    io.kill_after(KILL_GRACE);
                    live.end = Some(command_id.clone());
                    self.progress(live, out, command_id, "running");
                }
            },
            ClientMsg::McpAuthCallback {
                server,
                callback_url,
                ..
            } => {
                let Some(cb) = live.callbacks.take(session_id, server, now) else {
                    return refuse(command_id, "no phone sign-in is waiting for this server");
                };
                let checked = check_callback_url(callback_url, cb.port, &cb.path, &cb.state);
                let params = match checked {
                    Ok(p) => p,
                    Err(e) => {
                        let why = format!("the sign-in callback was refused: {e}");
                        self.fail_auth(live, server, &why, out);
                        return refuse(command_id, e);
                    }
                };
                let sent = live
                    .driver
                    .mcp_oauth_callback_url(command_id, server, &params.url, |f| tx(&live.io, f))
                    .and_then(|()| {
                        live.driver
                            .mcp_reconnect(Some(command_id), server, |f| tx(&live.io, f))
                    });
                if let Err(e) = sent {
                    return refuse(command_id, e);
                }
                live.waits.push(Wait {
                    command_id: command_id.clone(),
                    server: server.clone(),
                    deadline: now + self.timing.auth_ttl_ms,
                    auth: None,
                    replied: false,
                    callback: true,
                });
            }
            _ => {
                // these need the process: a reaped one comes back (`--resume`)
                if let Err(e) = self.ensure(live, running, now) {
                    return refuse(command_id, e);
                }
                let sent = match msg {
                    ClientMsg::SubmitPrompt { text, images, .. } => live
                        .driver
                        .user_prompt(command_id, text, images, now, |f| tx(&live.io, f))
                        .map(|op| self.hub.push(session_id, op)),
                    ClientMsg::SetPermissionMode { mode, .. } => {
                        live.driver
                            .set_permission_mode(command_id, mode, |f| tx(&live.io, f))
                    }
                    ClientMsg::SetModel { model, .. } => {
                        live.driver
                            .set_model(command_id, model, |f| tx(&live.io, f))
                    }
                    ClientMsg::RenameSession { title, .. } => {
                        live.driver.rename(command_id, title, |f| tx(&live.io, f))
                    }
                    ClientMsg::McpAuth { server, flow, .. } => {
                        return self.mcp_auth(live, command_id, server, flow, now, out);
                    }
                    ClientMsg::McpReconnect { server, .. } => live
                        .driver
                        .mcp_reconnect(Some(command_id), server, |f| tx(&live.io, f))
                        .map(|()| {
                            live.waits.push(Wait {
                                command_id: command_id.clone(),
                                server: server.clone(),
                                deadline: now + self.timing.auth_ttl_ms,
                                auth: None,
                                replied: false,
                                callback: false,
                            })
                        }),
                    _ => unreachable!("matched above"),
                };
                if let Err(e) = sent {
                    return refuse(command_id, e);
                }
            }
        }
        Ok(ack(command_id, true, None, Some(session_id)))
    }

    fn create(
        self: &Arc<Self>,
        st: &mut State,
        command_id: &str,
        new: New,
        now: i64,
        out: &mut Vec<EventKind>,
    ) -> Result<ServerMsg, AdapterError> {
        let New {
            title,
            prompt,
            project,
            images,
            model,
        } = new;
        let Some(ws) = self
            .cfg
            .workspaces
            .iter()
            .find(|w| Some(w.id.as_str()) == project)
        else {
            return refuse(
                command_id,
                "choose one of the workspaces configured for Claude Code",
            );
        };
        if st
            .last_create
            .is_some_and(|t| now - t < self.timing.create_every_ms)
        {
            return refuse(
                command_id,
                "a Claude Code session was just started; try again in a few seconds",
            );
        }
        let running = st.sessions.iter().filter(|l| l.io.is_some()).count();
        if running >= self.cfg.max_sessions {
            return refuse(command_id, self.full());
        }
        let id = new_uuid().map_err(|e| AdapterError::Unknown(e.into()))?;
        let title = match (title.trim(), prompt.trim()) {
            ("", "") => "Claude Code".to_string(),
            ("", p) => p.chars().take(60).collect(),
            (t, _) => t.to_string(),
        };
        let mut session = self.session(&id, title, &ws.path, "running", now, now);
        session.prompt = (!prompt.is_empty()).then(|| prompt.to_string());
        let mut live = Live::new(session, Driver::new(&self.id, &id, &ws.path), now);
        live.create = Some((command_id.to_string(), prompt.trim().is_empty()));
        let mode = self.cfg.permission_mode.clone();
        if let Err(e) = self.start(&mut live, false, mode.as_deref(), model, now) {
            return refuse(command_id, e);
        }
        st.last_create = Some(now);
        if !prompt.trim().is_empty() {
            match live
                .driver
                .user_prompt(command_id, prompt, images, now, |f| tx(&live.io, f))
            {
                Ok(op) => self.hub.push(&id, op),
                Err(e) => {
                    live.io = None;
                    return refuse(command_id, e);
                }
            }
        }
        out.push(EventKind::SessionCreated(live.session.clone()));
        st.sessions.push(live);
        while st.sessions.iter().filter(|l| l.ended).count() > ENDED_KEPT {
            let oldest = st.sessions.iter().position(|l| l.ended).expect("counted");
            st.sessions.remove(oldest);
        }
        Ok(ack(command_id, true, None, Some(&id)))
    }

    /// ADR-018: a session on disk (in use on the PC or not) continues as a
    /// NEW session: `--resume <id> --fork-session --session-id <new>` in its
    /// recorded cwd, so the original is never touched. The new session gets
    /// the original's history first, then the prompt; the ack names it.
    #[allow(clippy::too_many_arguments)]
    fn fork(
        self: &Arc<Self>,
        st: &mut State,
        command_id: &str,
        session_id: &str,
        text: &str,
        images: &[PromptImage],
        now: i64,
        out: &mut Vec<EventKind>,
    ) -> Result<ServerMsg, AdapterError> {
        let Some(f) = st.found.iter().find(|f| f.id == session_id).cloned() else {
            return refuse(
                command_id,
                "only a session from the PC can be forked; send a normal prompt",
            );
        };
        let Some(cwd) = claude_history::inside(&f.cwd, &self.cfg.workspaces) else {
            return refuse(
                command_id,
                "this session's folder is no longer in a workspace configured for Claude Code",
            );
        };
        if st
            .last_create
            .is_some_and(|t| now - t < self.timing.create_every_ms)
        {
            return refuse(
                command_id,
                "a Claude Code session was just started; try again in a few seconds",
            );
        }
        if st.sessions.iter().filter(|l| l.io.is_some()).count() >= self.cfg.max_sessions {
            return refuse(command_id, self.full());
        }
        let id = new_uuid().map_err(|e| AdapterError::Unknown(e.into()))?;
        let mut session = self.session(&id, f.title.clone(), &cwd, "running", now, now);
        session.prompt = (!text.is_empty()).then(|| text.to_string());
        session.forked_from = Some(f.title.clone());
        let mut live = Live::new(session, Driver::new(&self.id, &id, &cwd), now);
        live.fork_of = Some(f.id.clone());
        let mode = self.cfg.permission_mode.clone();
        if let Err(e) = self.start(&mut live, false, mode.as_deref(), None, now) {
            return refuse(command_id, e);
        }
        // the conversation so far, as the fork's own history
        if let Ok((ops, _)) = claude_history::replay(&f.path, &self.id, &id, &cwd, false) {
            for op in ops {
                self.hub.push(&id, op);
            }
        }
        self.hub.push(
            &id,
            Op::Item(Item {
                id: "forked-from".into(),
                kind: ItemKind::Notice,
                text: format!(
                    "Forked from \u{201c}{}\u{201d}; the PC session is unchanged",
                    f.title
                ),
                level: Some("info"),
                done: true,
                ts: now,
                ..Default::default()
            }),
        );
        match live
            .driver
            .user_prompt(command_id, text, images, now, |fr| tx(&live.io, fr))
        {
            Ok(op) => self.hub.push(&id, op),
            Err(e) => {
                live.io = None;
                return refuse(command_id, e);
            }
        }
        st.last_create = Some(now);
        out.push(EventKind::SessionCreated(live.session.clone()));
        st.sessions.push(live);
        Ok(ack(command_id, true, None, Some(&id)))
    }

    fn full(&self) -> String {
        format!(
            "at most {} Claude Code sessions can run at once; end one first",
            self.cfg.max_sessions
        )
    }

    fn session(
        &self,
        id: &str,
        title: String,
        project: &str,
        status: &str,
        created_at: i64,
        now: i64,
    ) -> Session {
        Session {
            id: id.to_string(),
            agent_id: self.id.clone(),
            host_id: self.id.clone(),
            title,
            project: project.to_string(),
            status: status.into(),
            progress: 0,
            created_at,
            last_activity_at: now,
            prompt: None,
            timeline: Vec::new(),
            output: Vec::new(),
            files: Vec::new(),
            transcript: true,
            live_pc: false,
            forked_from: None,
        }
    }

    fn found_session(&self, f: &Found) -> Session {
        let title = f.title.clone();
        let mut s = self.session(&f.id, title, &f.cwd, "idle", f.created_at, f.updated_at);
        s.live_pc = f.live;
        s
    }

    /* ----------------------- sessions on disk ---------------------- */

    /// Lists the sessions on disk; new or renamed ones are published once
    /// the first list is in place (the snapshot carries that one).
    fn discover(self: &Arc<Self>) {
        let Some(dir) = &self.cfg.projects else {
            return;
        };
        let mut found = self.scanner.lock().unwrap().scan(dir, &self.cfg.workspaces);
        let now = now_ms();
        let mut out = Vec::new();
        // ADR-018: watched sessions the PC started writing again
        let mut wake = Vec::new();
        {
            let mut st = self.state.lock().unwrap();
            found.retain(|f| !st.sessions.iter().any(|l| l.session.id == f.id));
            for f in &mut found {
                let old = st.found.iter().find(|o| o.id == f.id);
                f.hydrated = old.and_then(|o| o.hydrated);
                // ADR-018: a mirror decides while it runs
                f.live = if st.mirrors.contains(&f.id) {
                    old.is_some_and(|o| o.live)
                } else {
                    now - f.updated_at < IN_USE_MS
                };
                if st.scanned && old.is_none_or(|o| o.title != f.title || o.live != f.live) {
                    out.push(EventKind::SessionUpdated(self.found_session(f)));
                }
                if f.live && !st.mirrors.contains(&f.id) && self.hub.is_subscribed(&f.id) {
                    wake.push(f.clone());
                }
            }
            st.found = found;
            st.scanned = true;
        }
        self.publish(out);
        for f in wake {
            self.hydrate(&f);
        }
    }

    /// Replays a found session's transcript into the hub, unless the hub
    /// already holds it as of the file's current mtime. ADR-018: one the PC
    /// is writing keeps its last turn open and gets a mirror thread.
    fn hydrate(self: &Arc<Self>, f: &Found) {
        let mtime = claude_history::mtime_ms(&f.path);
        let live = claude_history::in_use(&f.path, now_ms());
        if !live && f.hydrated.is_some() && f.hydrated == mtime && self.hub.has_items(&f.id) {
            return;
        }
        if live && self.state.lock().unwrap().mirrors.contains(&f.id) {
            return;
        }
        match claude_history::replay(&f.path, &self.id, &f.id, &f.cwd, live) {
            Ok((ops, tail)) => {
                for op in ops {
                    self.hub.push(&f.id, op);
                }
                let mut st = self.state.lock().unwrap();
                if let Some(g) = st.found.iter_mut().find(|g| g.id == f.id) {
                    g.hydrated = mtime;
                }
                if live && !st.mirrors.contains(&f.id) {
                    st.mirrors.push(f.id.clone());
                    drop(st);
                    let (inner, sid) = (Arc::clone(self), f.id.clone());
                    let since = mtime.unwrap_or_else(now_ms);
                    let spawned = std::thread::Builder::new()
                        .name("claude-mirror".into())
                        .spawn(move || inner.mirror(&sid, tail, since));
                    if spawned.is_err() {
                        self.state.lock().unwrap().mirrors.retain(|m| *m != f.id);
                    }
                }
            }
            Err(_) => self.hub.push(
                &f.id,
                Op::Item(crate::transcript::Item {
                    id: "history-unreadable".into(),
                    kind: crate::transcript::ItemKind::Notice,
                    text: "The history of this session could not be read on the PC".into(),
                    level: Some("warning"),
                    done: true,
                    ..Default::default()
                }),
            ),
        }
    }

    /// ADR-018 live mirror: polls a transcript the PC writes and pushes what
    /// it adds, until nobody watches, the session leaves the list, or the
    /// file has been quiet for `mirror_idle_ms` (then its turn ends and the
    /// session is no longer "Live on PC").
    fn mirror(self: Arc<Self>, sid: &str, mut tail: Tail, mut last_write: i64) {
        loop {
            std::thread::sleep(self.timing.mirror_every);
            let now = now_ms();
            let idle = now - last_write >= self.timing.mirror_idle_ms;
            let mut out = Vec::new();
            {
                // decided under the lock `hydrate` checks, so a subscriber
                // arriving now either sees this mirror or starts its own
                let mut st = self.state.lock().unwrap();
                let listed = st.found.iter().any(|f| f.id == sid);
                if self.stop.load(Ordering::SeqCst)
                    || !listed
                    || idle
                    || !self.hub.is_subscribed(sid)
                {
                    st.mirrors.retain(|m| m != sid);
                    if idle && let Some(f) = st.found.iter_mut().find(|f| f.id == sid) {
                        for op in tail.close() {
                            self.hub.push(sid, op);
                        }
                        f.live = false;
                        f.hydrated = None; // the next subscriber replays it whole
                        let f = f.clone();
                        out.push(EventKind::SessionUpdated(self.found_session(&f)));
                    }
                    drop(st);
                    self.publish(out);
                    return;
                }
            }
            let Ok((ops, grew)) = tail.poll() else {
                self.state.lock().unwrap().mirrors.retain(|m| m != sid);
                return;
            };
            for op in ops {
                self.hub.push(sid, op);
            }
            if grew {
                last_write = now;
                let mut st = self.state.lock().unwrap();
                if let Some(f) = st.found.iter_mut().find(|f| f.id == sid && !f.live) {
                    f.live = true;
                    let f = f.clone();
                    out.push(EventKind::SessionUpdated(self.found_session(&f)));
                }
            }
            self.publish(out);
        }
    }

    /// A command for a session found on disk: refused while another process
    /// writes it (T10), and only in a workspace still allow-listed (T11);
    /// else it becomes a companion session whose process starts with
    /// `--resume` in its recorded cwd, like a reaped one.
    fn adopt(&self, st: &mut State, at: usize, now: i64) -> Result<(), String> {
        let f = &st.found[at];
        if claude_history::in_use(&f.path, now) {
            return Err(IN_USE.into());
        }
        let cwd = claude_history::inside(&f.cwd, &self.cfg.workspaces).ok_or(
            "this session's folder is no longer in a workspace configured for Claude Code",
        )?;
        let f = st.found.remove(at);
        let mut session = self.found_session(&f);
        session.project = cwd.clone();
        let mut driver = Driver::new(&self.id, &f.id, &cwd);
        driver.restore_cost(f.cost);
        st.sessions.push(Live::new(session, driver, now));
        Ok(())
    }

    /// A reaped session's process comes back with `--resume` (T10: this
    /// companion created it and holds no live process for it).
    fn ensure(self: &Arc<Self>, live: &mut Live, running: usize, now: i64) -> Result<(), String> {
        if live.reaping {
            return Err(
                "Claude Code is closing this idle session; try again in a few seconds".into(),
            );
        }
        if live.io.is_some() {
            return Ok(());
        }
        if running >= self.cfg.max_sessions {
            return Err(self.full());
        }
        let mode = live
            .driver
            .permission_mode()
            .filter(|m| PERMISSION_MODES.contains(m))
            .map(str::to_string)
            .or_else(|| self.cfg.permission_mode.clone());
        let model = live
            .driver
            .model()
            .filter(|m| is_model(m))
            .map(str::to_string);
        self.start(live, true, mode.as_deref(), model.as_deref(), now)
    }

    /// Spawn → `state starting` → `initialize`; the reader owns the events.
    fn start(
        self: &Arc<Self>,
        live: &mut Live,
        resume: bool,
        mode: Option<&str>,
        model: Option<&str>,
        now: i64,
    ) -> Result<(), String> {
        let sid = live.session.id.clone();
        let mut args = spawn_args(&sid, resume, mode, model)?;
        // ADR-018: a fork's first process; a respawn resumes the fork itself
        if let Some(parent) = live.fork_of.take() {
            args.extend(["--resume".into(), parent, "--fork-session".into()]);
        }
        args.extend(self.cfg.extra_args.iter().cloned());
        let (io, rx) = (self.spawn)(&args, Path::new(&live.session.project))
            .map_err(|e| format!("could not start Claude Code: {e}"))?;
        live.generation += 1;
        let (inner, generation) = (Arc::clone(self), live.generation);
        let thread_sid = sid.clone();
        std::thread::Builder::new()
            .name("claude-live".into())
            .spawn(move || inner.read(&thread_sid, generation, rx))
            .map_err(|e| format!("could not start Claude Code: {e}"))?;
        live.io = Some(io);
        live.reaping = false;
        live.last_active = now;
        self.hub.push(&sid, live.driver.starting());
        live.driver
            .initialize(|f| tx(&live.io, f))
            .map_err(String::from)
    }

    fn mcp_auth(
        &self,
        live: &mut Live,
        command_id: &str,
        server: &str,
        flow: &str,
        now: i64,
        out: &mut Vec<EventKind>,
    ) -> Result<ServerMsg, AdapterError> {
        let Some(entry) = live.mcp.iter().find(|s| s.name == server) else {
            return refuse(command_id, "unknown MCP server");
        };
        if !entry.auth_flows.contains(&flow) {
            return refuse(
                command_id,
                if entry.auth_flows.contains(&"claude-ai") {
                    "claude.ai connectors are signed in on claude.ai"
                } else {
                    "this MCP server cannot sign in that way"
                },
            );
        }
        if let Err(e) = live
            .driver
            .mcp_authenticate(command_id, server, |f| tx(&live.io, f))
        {
            return refuse(command_id, e);
        }
        self.fail_auth(live, server, "a newer sign-in replaced this one", out);
        live.waits.push(Wait {
            command_id: command_id.to_string(),
            server: server.to_string(),
            deadline: now + self.timing.auth_ttl_ms,
            auth: Some(Auth {
                phone: flow == "phone-callback",
                started: None,
            }),
            replied: false,
            callback: false,
        });
        self.progress(live, out, command_id, "accepted");
        Ok(ack(command_id, true, None, Some(&live.session.id)))
    }

    /* ---------------------------- events --------------------------- */

    /// One process's stdout and timers, until it exits or a newer process
    /// replaces it.
    fn read(self: Arc<Self>, sid: &str, generation: u64, rx: Receiver<ChildEvent>) {
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            let event = rx.recv_timeout(self.timing.tick);
            let now = now_ms();
            let mut out = Vec::new();
            let done = {
                let mut st = self.state.lock().unwrap();
                let Some(live) = st
                    .sessions
                    .iter_mut()
                    .find(|l| l.session.id == sid && l.generation == generation)
                else {
                    return;
                };
                match event {
                    Ok(ChildEvent::Line(line)) => {
                        live.last_active = now;
                        let before = live.driver.malformed();
                        let events = live.driver.on_line(&line, now);
                        if live.driver.malformed() > before {
                            self.hub.malformed();
                        }
                        // `system/init` lists servers without their status
                        // detail: ask for it (§4.7)
                        let init = line.contains(r#""subtype":"init""#)
                            && events.iter().any(|e| matches!(e, Event::Op(Op::Mcp(_))));
                        self.apply(live, events, now, &mut out);
                        if init {
                            self.poll(live, now);
                        }
                        false
                    }
                    Ok(ChildEvent::Skipped(n)) => {
                        let events = live.driver.on_skipped(n, now);
                        self.apply(live, events, now, &mut out);
                        false
                    }
                    Ok(ChildEvent::Exit(code)) => {
                        self.exited(live, code, now, &mut out);
                        true
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        self.exited(live, None, now, &mut out);
                        true
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        self.tick(live, now, &mut out);
                        false
                    }
                }
            };
            self.publish(out);
            if done {
                return;
            }
        }
    }

    fn apply(&self, live: &mut Live, events: Vec<Event>, now: i64, out: &mut Vec<EventKind>) {
        let sid = live.session.id.clone();
        for event in events {
            match event {
                Event::Op(Op::Append { id, field, text }) => {
                    self.hub.append(&sid, &id, field, &text)
                }
                Event::Op(Op::Mcp(list)) => self.mcp(live, list, out),
                Event::Op(Op::State(s)) if live.reaping && s.status == "ended" => {}
                Event::Op(op) => {
                    if let Op::Turn(t) = &op
                        && t.ended_at.is_some()
                    {
                        for (cmd, _) in std::mem::take(&mut live.interrupts) {
                            self.result(live, out, &cmd, "completed", None);
                        }
                        // ADR-017 context meter: cheap (no API call); a dead
                        // process just has none
                        let _ = live.driver.context_usage(|f| tx(&live.io, f));
                    }
                    if let Op::Item(item) = &op
                        && let Some(tool) = &item.tool
                        && tool.status == ToolStatus::Completed
                        && !tool.diffs.is_empty()
                    {
                        for d in &tool.diffs {
                            let files = &mut live.session.files;
                            let status = match d.status {
                                "added" => "added",
                                "deleted" => "removed",
                                _ => "modified",
                            };
                            match files.iter_mut().find(|f| f.path == d.path) {
                                Some(f) => {
                                    f.additions += d.additions;
                                    f.deletions += d.deletions;
                                }
                                None => files.push(FileChange {
                                    path: d.path.clone(),
                                    additions: d.additions,
                                    deletions: d.deletions,
                                    status: status.into(),
                                }),
                            }
                        }
                        out.push(EventKind::SessionUpdated(live.session.clone()));
                    }
                    self.hub.push(&sid, op);
                }
                Event::Permission(pr) => {
                    live.asked.push((*pr).clone());
                    out.push(EventKind::PermissionRequested(*pr));
                }
                Event::Withdrawn(pid) => {
                    if let Some(at) = live.asked.iter().position(|p| p.id == pid) {
                        let mut p = live.asked.remove(at);
                        p.status = "stale".into();
                        out.push(EventKind::PermissionResolved(p));
                    }
                }
                Event::Write(frame) => {
                    let _ = tx(&live.io, frame); // a dead child fails the request anyway
                }
                Event::Prompt { command_id, state } => match state {
                    PromptState::Queued => self.progress(live, out, &command_id, "accepted"),
                    PromptState::Started => self.progress(live, out, &command_id, "running"),
                    PromptState::Done { status, error } => {
                        self.result(live, out, &command_id, status, error)
                    }
                },
                Event::Reply {
                    command_id,
                    subtype,
                    result,
                } => self.reply(live, command_id, subtype, result, now, out),
                Event::AuthStarted {
                    command_id,
                    server,
                    result,
                } => self.auth_started(live, command_id, &server, result, now, out),
                Event::Status(s) => {
                    live.session.status = s.into();
                    live.session.last_activity_at = now;
                    out.push(EventKind::SessionUpdated(live.session.clone()));
                }
                Event::Title(t) => {
                    live.session.title = t;
                    out.push(EventKind::SessionUpdated(live.session.clone()));
                }
            }
        }
    }

    fn reply(
        &self,
        live: &mut Live,
        command_id: Option<String>,
        subtype: &str,
        result: Result<(), String>,
        now: i64,
        out: &mut Vec<EventKind>,
    ) {
        match subtype {
            "initialize" => match result {
                Ok(()) => {
                    if let Some((cmd, no_prompt)) = live.create.take() {
                        self.progress(live, out, &cmd, "accepted");
                        if no_prompt {
                            self.result(live, out, &cmd, "completed", None);
                        }
                    }
                    self.poll(live, now);
                    let _ = live.driver.context_usage(|f| tx(&live.io, f));
                }
                // the exit fails the first prompt (and with it the create)
                Err(_) => {
                    if let Some(io) = &live.io {
                        io.close_stdin();
                    }
                }
            },
            "interrupt" => {
                let Some(cmd) = command_id else { return };
                let Some(at) = live.interrupts.iter().position(|(c, _)| *c == cmd) else {
                    return; // session_end's interrupt
                };
                match result {
                    Ok(()) => self.progress(live, out, &cmd, "accepted"),
                    Err(e) => {
                        live.interrupts.remove(at);
                        self.result(live, out, &cmd, "failed", Some(e));
                    }
                }
            }
            "set_permission_mode" | "set_model" | "rename_session" => {
                if let Some(cmd) = command_id {
                    match result {
                        Ok(()) => self.result(live, out, &cmd, "completed", None),
                        Err(e) => self.result(live, out, &cmd, "failed", Some(e)),
                    }
                }
            }
            "mcp_oauth_callback_url" | "mcp_reconnect" => {
                let Some(at) =
                    command_id.and_then(|cmd| live.waits.iter().position(|w| w.command_id == cmd))
                else {
                    return; // the desktop sign-in's own reconnects
                };
                match result {
                    Err(e) => {
                        let w = live.waits.remove(at);
                        self.result(live, out, &w.command_id, "failed", Some(e.clone()));
                        if w.callback {
                            self.fail_auth(live, &w.server, &e, out);
                        }
                    }
                    Ok(()) if subtype == "mcp_reconnect" => {
                        live.waits[at].replied = true;
                        self.poll(live, now);
                    }
                    Ok(()) => {}
                }
            }
            _ => {}
        }
    }

    fn auth_started(
        &self,
        live: &mut Live,
        command_id: Option<String>,
        server: &str,
        result: Result<claude_live::AuthStart, String>,
        now: i64,
        out: &mut Vec<EventKind>,
    ) {
        let Some(at) = command_id.and_then(|cmd| {
            live.waits
                .iter()
                .position(|w| w.command_id == cmd && w.auth.is_some())
        }) else {
            return;
        };
        let phone = live.waits[at].auth.as_ref().is_some_and(|a| a.phone);
        let started = match result {
            Err(e) => Err(e),
            // T5: the phone only ever gets the URL the runtime produced, and
            // the desktop only ever opens it
            Ok(start) if phone => match start.callback {
                Some(cb) => {
                    let prefix = cb.prefix.clone();
                    live.callbacks.insert(&live.session.id, server, cb, now);
                    Ok((Some(start.auth_url), Some(prefix)))
                }
                None => Err("this MCP server can only sign in on the desktop".to_string()),
            },
            Ok(start) => (self.open)(&start.auth_url)
                .map(|()| (None, None))
                .map_err(String::from),
        };
        match started {
            Ok(urls) => {
                let w = &mut live.waits[at];
                if let Some(a) = w.auth.as_mut() {
                    a.started = Some(urls);
                }
                let cmd = w.command_id.clone();
                self.progress(live, out, &cmd, "running");
                live.last_mcp_poll = now; // the op below; the next poll judges
                let list = live.mcp.clone();
                self.mcp(live, list, out);
            }
            Err(e) => {
                let w = live.waits.remove(at);
                self.result(live, out, &w.command_id, "failed", Some(e));
            }
        }
    }

    /// A server list from the runtime: sign-ins in progress are shown on it,
    /// and waits are judged by it.
    fn mcp(&self, live: &mut Live, mut list: Vec<McpServer>, out: &mut Vec<EventKind>) {
        for w in &live.waits {
            let Some((url, prefix)) = w.auth.as_ref().and_then(|a| a.started.clone()) else {
                continue;
            };
            if let Some(s) = list
                .iter_mut()
                .find(|s| s.name == w.server && s.status != "connected")
            {
                s.auth = Some(McpAuthState {
                    state: if prefix.is_some() {
                        "awaiting-callback"
                    } else {
                        "awaiting-browser"
                    },
                    url,
                    callback_prefix: prefix,
                    error: None,
                });
            }
        }
        let mut failed_callbacks = Vec::new();
        for w in std::mem::take(&mut live.waits) {
            let status = list
                .iter()
                .find(|s| s.name == w.server)
                .map_or("missing", |s| s.status);
            if status == "connected" {
                self.result(live, out, &w.command_id, "completed", None);
            } else if w.auth.is_none() && w.replied {
                let why = format!("MCP server {} is {status}", w.server);
                self.result(live, out, &w.command_id, "failed", Some(why.clone()));
                if w.callback {
                    failed_callbacks.push((w.server, why));
                }
            } else {
                live.waits.push(w);
            }
        }
        for (server, why) in failed_callbacks {
            self.fail_auth(live, &server, &why, out);
        }
        live.mcp = list.clone();
        self.hub.push(&live.session.id, Op::Mcp(list));
    }

    /// Ends every sign-in for `server` (its pending callback goes too).
    fn fail_auth(&self, live: &mut Live, server: &str, why: &str, out: &mut Vec<EventKind>) {
        let now = now_ms();
        let _ = live.callbacks.take(&live.session.id, server, now);
        for w in std::mem::take(&mut live.waits) {
            if w.server == server && w.auth.is_some() {
                self.result(live, out, &w.command_id, "failed", Some(why.to_string()));
            } else {
                live.waits.push(w);
            }
        }
    }

    fn poll(&self, live: &mut Live, now: i64) {
        live.last_mcp_poll = now;
        let _ = live.driver.mcp_status(|f| tx(&live.io, f));
    }

    /// Timers of a live process.
    fn tick(&self, live: &mut Live, now: i64, out: &mut Vec<EventKind>) {
        let (late, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut live.interrupts)
            .into_iter()
            .partition(|(_, deadline)| now >= *deadline);
        live.interrupts = kept;
        for (cmd, _) in late {
            let why = "no turn end from Claude Code within 30 s".to_string();
            self.result(live, out, &cmd, "unknown", Some(why));
        }
        for w in std::mem::take(&mut live.waits) {
            if now < w.deadline {
                live.waits.push(w);
            } else if w.auth.is_some() {
                let _ = live.callbacks.take(&live.session.id, &w.server, now);
                self.result(live, out, &w.command_id, "expired", None);
            } else {
                let why = "no answer from Claude Code".to_string();
                self.result(live, out, &w.command_id, "failed", Some(why));
            }
        }
        let subscribed = self.hub.is_subscribed(&live.session.id);
        let every = if live.waits.is_empty() {
            self.timing.mcp_poll_ms
        } else {
            self.timing.auth_poll_ms
        };
        if (subscribed || !live.waits.is_empty()) && now - live.last_mcp_poll >= every {
            let desktop: Vec<String> = live
                .waits
                .iter()
                .filter(|w| {
                    w.auth
                        .as_ref()
                        .is_some_and(|a| !a.phone && a.started.is_some())
                })
                .map(|w| w.server.clone())
                .collect();
            if !desktop.is_empty() {
                live.auth_polls += 1;
                if live.auth_polls.is_multiple_of(RECONNECT_EVERY) {
                    for server in desktop {
                        let _ = live
                            .driver
                            .mcp_reconnect(None, &server, |f| tx(&live.io, f));
                    }
                }
            }
            self.poll(live, now);
        }
        // the reaper (§4.2.3): stdin closes, the CLI exits, a prompt resumes
        let quiet = live.driver.is_quiet()
            && live.asked.is_empty()
            && live.waits.is_empty()
            && live.interrupts.is_empty()
            && live.end.is_none()
            && live.create.is_none();
        if !live.reaping
            && quiet
            && !subscribed
            && now - live.last_active >= self.timing.idle_reap_ms
        {
            live.reaping = true;
            if let Some(io) = &live.io {
                io.close_stdin();
            }
        }
    }

    /// The process is gone (§4.2.4). Reaped: quietly, a prompt resumes it.
    /// Otherwise the session ends: prompts fail, requests go stale, a
    /// pending interrupt is `unknown`, a `session_end` completes.
    fn exited(&self, live: &mut Live, code: Option<i32>, now: i64, out: &mut Vec<EventKind>) {
        live.io = None;
        let reaped = live.reaping && live.end.is_none();
        for (cmd, _) in std::mem::take(&mut live.interrupts) {
            let why = "Claude Code exited before the turn ended".to_string();
            self.result(live, out, &cmd, "unknown", Some(why));
        }
        let why = match code {
            Some(c) => format!("Claude Code exited with code {c}"),
            None => "Claude Code exited".to_string(),
        };
        for w in std::mem::take(&mut live.waits) {
            self.result(live, out, &w.command_id, "failed", Some(why.clone()));
        }
        live.callbacks = PendingAuth::default();
        live.reaping = reaped; // filters the driver's `state ended`
        let events = live.driver.on_exit(code, now);
        self.apply(live, events, now, out);
        live.reaping = false;
        if reaped {
            return;
        }
        let status = if live.end.is_some() || code == Some(0) {
            "completed"
        } else {
            "interrupted"
        };
        self.finish(live, status, now, out);
        if let Some(cmd) = live.end.take() {
            self.result(live, out, &cmd, "completed", None);
        }
    }

    /// The session is over: `state ended`, collected after an hour.
    fn finish(&self, live: &mut Live, status: &str, now: i64, out: &mut Vec<EventKind>) {
        live.ended = true;
        live.session.status = status.into();
        live.session.last_activity_at = now;
        out.push(EventKind::SessionUpdated(live.session.clone()));
        self.hub.end_session(&live.session.id);
    }

    /* ------------------------ G1 lifecycles ------------------------ */

    fn progress(&self, live: &mut Live, out: &mut Vec<EventKind>, cmd: &str, state: &str) {
        let rank = if state == "running" { 2 } else { 1 };
        match live.progress.iter_mut().find(|(c, _)| c == cmd) {
            Some((_, r)) if *r >= rank => return,
            Some((_, r)) => *r = rank,
            None => {
                if live.progress.len() >= 64 {
                    live.progress.remove(0);
                }
                live.progress.push((cmd.to_string(), rank));
            }
        }
        out.push(EventKind::CommandProgress {
            command_id: cmd.to_string(),
            state: state.to_string(),
            session_id: Some(live.session.id.clone()),
        });
    }

    fn result(
        &self,
        live: &mut Live,
        out: &mut Vec<EventKind>,
        cmd: &str,
        status: &str,
        error: Option<String>,
    ) {
        live.progress.retain(|(c, _)| c != cmd);
        if live.create.as_ref().is_some_and(|(c, _)| c == cmd) {
            live.create = None;
        }
        out.push(EventKind::CommandResult {
            command_id: cmd.to_string(),
            status: status.to_string(),
            error,
            session_id: Some(live.session.id.clone()),
            permission_id: None,
        });
    }
}

/* ------------------------------------------------------------------ */
/* Tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::claude_live::raw_member;
    use crate::json::{self, Json};
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Write};
    use std::time::Instant;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!("../tests/fixtures/claude/", $name))
        };
    }

    /// A fake `claude.exe`: gets each stdin frame, answers stdout lines, and
    /// `true` exits at once (a crash); stdin EOF exits it too, like the CLI.
    pub(crate) type React = Box<dyn FnMut(&str) -> (Vec<String>, bool) + Send>;

    pub(crate) const WS: &str = "ws-0123456789";

    pub(crate) fn fast() -> Timing {
        Timing {
            create_every_ms: 0,
            idle_reap_ms: 60 * 60 * 1000,
            interrupt_ms: 30_000,
            mcp_poll_ms: 60 * 60 * 1000,
            auth_poll_ms: 20,
            auth_ttl_ms: AUTH_TTL_MS,
            tick: Duration::from_millis(10),
            mirror_every: Duration::from_millis(10),
            mirror_idle_ms: IN_USE_MS,
        }
    }

    fn type_of(frame: &str) -> String {
        let v = json::parse(frame).unwrap_or(Json::Null);
        let sub = json::get(&v, "request")
            .and_then(|r| json::get_str(r, "subtype"))
            .unwrap_or("");
        format!("{}:{sub}", json::get_str(&v, "type").unwrap_or(""))
    }

    /// Replays a research capture as the CLI: each `in` frame waits for our
    /// frame of the same type (others wait in a buffer: our post-initialize
    /// `mcp_status` is not in every capture), then the `out` frames up to
    /// the next `in` are written with the capture's request ids and prompt
    /// uuids mapped to ours.
    pub(crate) fn replay(capture: &'static str) -> React {
        let frames: Vec<(String, String)> = capture
            .lines()
            .map(|l| {
                let v = json::parse(l).unwrap();
                let dir = json::get_str(&v, "dir").unwrap_or("").to_string();
                (dir, raw_member(l, "msg").unwrap_or("null").to_string())
            })
            .collect();
        let mut pos = 0;
        let mut buf: Vec<String> = Vec::new();
        let mut ids: Vec<(String, String)> = Vec::new();
        Box::new(move |frame| {
            buf.push(frame.to_string());
            let mut out = Vec::new();
            loop {
                while pos < frames.len() && frames[pos].0 != "in" {
                    if frames[pos].0 == "out" {
                        let mut line = frames[pos].1.clone();
                        for (theirs, ours) in &ids {
                            line = line.replace(&format!("\"{theirs}\""), &format!("\"{ours}\""));
                        }
                        out.push(line);
                    }
                    pos += 1;
                }
                if pos >= frames.len() {
                    break;
                }
                let want = type_of(&frames[pos].1);
                let Some(at) = buf.iter().position(|f| type_of(f) == want) else {
                    break;
                };
                let ours = json::parse(&buf.remove(at)).unwrap();
                let theirs = json::parse(&frames[pos].1).unwrap();
                for key in ["request_id", "uuid"] {
                    if let (Some(t), Some(o)) =
                        (json::get_str(&theirs, key), json::get_str(&ours, key))
                        && t != o
                    {
                        ids.push((t.to_string(), o.to_string()));
                    }
                }
                pos += 1;
            }
            (out, false)
        })
    }

    pub(crate) fn idle() -> React {
        Box::new(|_| (Vec::new(), false))
    }

    pub(crate) struct Rig {
        pub rt: Arc<ClaudeRuntime>,
        pub hub: HubHandle,
        pub events: Arc<Mutex<Vec<EventKind>>>,
        pub spawns: Arc<Mutex<Vec<Vec<String>>>>,
        /// The cwd of each spawn.
        pub cwds: Arc<Mutex<Vec<PathBuf>>>,
        pub opened: Arc<Mutex<Vec<String>>>,
        /// Every frame the fake processes read, in order.
        pub stdin: Arc<Mutex<Vec<String>>>,
    }

    pub(crate) fn rig(timing: Timing, max_sessions: usize, peers: Vec<React>) -> Rig {
        rig_with_hub(HubHandle::default(), timing, max_sessions, peers)
    }

    /// A runtime whose processes are `peers` (in spawn order) on pipes. Its
    /// sink records engine-bound events; the companion's replaces it.
    pub(crate) fn rig_with_hub(
        hub: HubHandle,
        timing: Timing,
        max_sessions: usize,
        peers: Vec<React>,
    ) -> Rig {
        rig_with(hub, timing, max_sessions, peers, |_| {})
    }

    /// [`rig_with_hub`] whose config `edit` adjusts before the first probe.
    pub(crate) fn rig_with(
        hub: HubHandle,
        timing: Timing,
        max_sessions: usize,
        peers: Vec<React>,
        edit: impl FnOnce(&mut LiveConfig),
    ) -> Rig {
        let peers = Arc::new(Mutex::new(VecDeque::from(peers)));
        let spawns = Arc::new(Mutex::new(Vec::new()));
        let cwds = Arc::new(Mutex::new(Vec::new()));
        let stdin = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(Mutex::new(Vec::new()));
        let (log, read, dirs) = (spawns.clone(), stdin.clone(), cwds.clone());
        let spawn: Box<Spawn> = Box::new(move |args: &[String], cwd: &Path| {
            log.lock().unwrap().push(args.to_vec());
            dirs.lock().unwrap().push(cwd.to_path_buf());
            let mut react = peers
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| io::Error::other("no fake process left"))?;
            let (out_r, mut out_w) = io::pipe()?;
            let (in_r, in_w) = io::pipe()?;
            let read = read.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(in_r).lines() {
                    let Ok(line) = line else { return };
                    read.lock().unwrap().push(line.clone());
                    let (lines, exit) = react(&line);
                    for l in lines {
                        if writeln!(out_w, "{l}").is_err() {
                            return;
                        }
                    }
                    if exit {
                        return;
                    }
                }
            });
            ChildIo::from_pipes(out_r, in_w)
        });
        let urls = opened.clone();
        let open: Box<Open> = Box::new(move |url: &str| {
            urls.lock().unwrap().push(url.to_string());
            Ok(())
        });
        let mut cfg = LiveConfig {
            workspaces: vec![LiveWorkspace {
                id: WS.into(),
                label: "work".into(),
                path: "<tmp>\\work".into(),
            }],
            binary: PathBuf::from("claude.exe"),
            max_sessions,
            permission_mode: Some("default".into()),
            extra_args: Vec::new(),
            projects: None,
        };
        edit(&mut cfg);
        let rt = ClaudeRuntime::with_parts(
            "claude-1",
            cfg,
            hub.clone(),
            timing,
            spawn,
            open,
            Box::new(|| Ok(())),
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        rt.set_sink(Arc::new(move |k| sink.lock().unwrap().push(k)));
        rt.refresh().unwrap();
        Rig {
            rt: Arc::new(rt),
            hub,
            events,
            spawns,
            cwds,
            opened,
            stdin,
        }
    }

    impl Rig {
        pub(crate) fn send(&self, msg: ClientMsg) -> ServerMsg {
            self.rt.send(&msg).unwrap()
        }

        pub(crate) fn create(&self, command_id: &str, prompt: &str) -> String {
            match self.send(create_msg(command_id, prompt)) {
                ServerMsg::Ack {
                    ok: true,
                    session_id: Some(s),
                    ..
                } => s,
                other => panic!("{other:?}"),
            }
        }

        pub(crate) fn wait(&self, what: &str, done: impl Fn(&Rig) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done(self) {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        pub(crate) fn result_of(&self, cmd: &str) -> Option<(String, Option<String>)> {
            let events = self.events.lock().unwrap();
            events.iter().find_map(|e| match e {
                EventKind::CommandResult {
                    command_id,
                    status,
                    error,
                    ..
                } if command_id == cmd => Some((status.clone(), error.clone())),
                _ => None,
            })
        }

        pub(crate) fn wait_result(&self, cmd: &str) -> (String, Option<String>) {
            self.wait(cmd, |r| r.result_of(cmd).is_some());
            self.result_of(cmd).unwrap()
        }

        pub(crate) fn progress_of(&self, cmd: &str) -> Vec<String> {
            let events = self.events.lock().unwrap();
            events
                .iter()
                .filter_map(|e| match e {
                    EventKind::CommandProgress {
                        command_id, state, ..
                    } if command_id == cmd => Some(state.clone()),
                    _ => None,
                })
                .collect()
        }

        /// The `n`th permission request (0-based), once it arrives.
        pub(crate) fn permission(&self, n: usize) -> PermissionRequest {
            let find = |r: &Rig| {
                let events = r.events.lock().unwrap();
                events
                    .iter()
                    .filter_map(|e| match e {
                        EventKind::PermissionRequested(p) => Some(p.clone()),
                        _ => None,
                    })
                    .nth(n)
            };
            self.wait("a permission", |r| find(r).is_some());
            find(self).unwrap()
        }

        /// Approves `p` once by option id, as the router forwards it.
        pub(crate) fn approve(&self, command_id: &str, p: &PermissionRequest) -> ServerMsg {
            let option = p
                .options
                .iter()
                .find(|o| o.decision == "approve-once")
                .expect("an allow option");
            self.send(ClientMsg::PermissionDecision {
                command_id: command_id.into(),
                permission_id: p.id.clone(),
                decision: "approve-once".into(),
                revision: 1.0,
                option_id: Some(option.id.clone()),
                message: None,
                answers: vec![],
            })
        }

        pub(crate) fn ops(&self, sid: &str) -> Vec<Json> {
            let ops = self.hub.ops(sid);
            ops.iter().map(|o| json::parse(o).unwrap()).collect()
        }
    }

    pub(crate) fn create_msg(command_id: &str, prompt: &str) -> ClientMsg {
        ClientMsg::CreateSession {
            command_id: command_id.into(),
            agent_id: "claude-1".into(),
            title: String::new(),
            prompt: prompt.into(),
            project: Some(WS.into()),
            images: vec![],
            model: None,
        }
    }

    fn prompt(command_id: &str, sid: &str, text: &str) -> ClientMsg {
        ClientMsg::SubmitPrompt {
            command_id: command_id.into(),
            session_id: sid.into(),
            text: text.into(),
            images: vec![],
            fork: false,
        }
    }

    fn refusal(reply: ServerMsg) -> String {
        match reply {
            ServerMsg::Ack {
                ok: false,
                error: Some(e),
                ..
            } => e,
            other => panic!("expected a refusal: {other:?}"),
        }
    }

    fn ok(reply: ServerMsg) {
        assert!(
            matches!(reply, ServerMsg::Ack { ok: true, .. }),
            "{reply:?}"
        );
    }

    fn op_of<'a>(ops: &'a [Json], op: &str) -> Vec<&'a Json> {
        ops.iter()
            .filter(|o| json::get_str(o, "op") == Some(op))
            .collect()
    }

    fn state_statuses(ops: &[Json]) -> Vec<String> {
        op_of(ops, "state")
            .iter()
            .filter_map(|o| json::get(o, "state").and_then(|s| json::get_str(s, "status")))
            .map(String::from)
            .collect()
    }

    fn stale(r: &Rig, id: &str) -> bool {
        let events = r.events.lock().unwrap();
        events.iter().any(
            |e| matches!(e, EventKind::PermissionResolved(x) if x.id == id && x.status == "stale"),
        )
    }

    /* --------------------------- lifecycle -------------------------- */

    #[test]
    fn interrupt_progress_then_cancelled_turn() {
        // run 5: a Bash request is pending when the phone stops the turn
        let r = rig(fast(), 3, vec![replay(fixture!("sdk-run5-cancel.ndjson"))]);
        let sid = r.create("cmd-create", "go");
        let p = r.permission(0);
        ok(r.send(ClientMsg::Interrupt {
            command_id: "cmd-int".into(),
            session_id: sid.clone(),
        }));
        assert_eq!(r.wait_result("cmd-int"), ("completed".into(), None));
        assert_eq!(r.progress_of("cmd-int"), ["accepted"]);
        // the prompt ends cancelled, never failed; the request went stale
        assert_eq!(r.wait_result("cmd-create").0, "cancelled");
        assert!(stale(&r, &p.id), "the withdrawn request is resolved stale");
        assert!(!r.rt.knows_permission(&p.id));
        let ops = r.ops(&sid);
        let turns: Vec<&str> = op_of(&ops, "turn")
            .iter()
            .filter_map(|o| json::get(o, "turn").and_then(|t| json::get_str(t, "status")))
            .collect();
        assert!(turns.contains(&"interrupted"), "{turns:?}");
        // the process is still there: the next prompt runs a turn
        ok(r.send(prompt("cmd-2", &sid, "again")));
        assert_eq!(r.wait_result("cmd-2"), ("completed".into(), None));
        assert_eq!(r.spawns.lock().unwrap().len(), 1);
        // an interrupt while idle completes at once
        ok(r.send(ClientMsg::Interrupt {
            command_id: "cmd-int-2".into(),
            session_id: sid,
        }));
        assert_eq!(r.result_of("cmd-int-2"), Some(("completed".into(), None)));
    }

    #[test]
    fn mid_turn_prompt_lifecycle_queued_then_started() {
        // run 2: a follow-up sent mid-turn is folded into the running turn
        let r = rig(fast(), 3, vec![replay(fixture!("sdk-run2.ndjson"))]);
        let sid = r.create("cmd-1", "mkdir");
        ok(r.approve("dec-1", &r.permission(0)));
        assert_eq!(r.wait_result("cmd-1").0, "completed");
        ok(r.send(prompt("cmd-2", &sid, "edit")));
        r.wait("cmd-2 running", |r| {
            r.progress_of("cmd-2").contains(&"running".to_string())
        });
        ok(r.send(prompt("cmd-3", &sid, "after that")));
        ok(r.approve("dec-2", &r.permission(1)));
        assert_eq!(r.wait_result("cmd-3"), ("completed".into(), None));
        assert_eq!(r.result_of("cmd-2"), Some(("completed".into(), None)));
        assert_eq!(r.progress_of("cmd-3"), ["accepted", "running"]);
        // its user item says "joins the current turn" until it starts
        let ops = r.ops(&sid);
        let queued: Vec<bool> = op_of(&ops, "item")
            .iter()
            .filter_map(|o| json::get(o, "item"))
            .filter(|i| json::get_str(i, "text") == Some("after that"))
            .map(|i| json::get_bool(i, "queued") == Some(true))
            .collect();
        assert_eq!(queued, [false, true, false]);
    }

    #[test]
    fn process_exit_fails_prompt_and_stales_permissions() {
        // run 5 again, but the process dies when the interrupt arrives
        let mut run = replay(fixture!("sdk-run5-cancel.ndjson"));
        let crash: React = Box::new(move |f| {
            if f.contains(r#""subtype":"interrupt""#) {
                (Vec::new(), true)
            } else {
                run(f)
            }
        });
        let r = rig(fast(), 3, vec![crash]);
        let sid = r.create("cmd-create", "go");
        let p = r.permission(0);
        ok(r.send(ClientMsg::Interrupt {
            command_id: "cmd-int".into(),
            session_id: sid.clone(),
        }));
        let (status, error) = r.wait_result("cmd-create");
        assert_eq!(status, "failed");
        assert!(error.unwrap().starts_with("Claude Code exited"));
        assert_eq!(r.wait_result("cmd-int").0, "unknown");
        assert!(stale(&r, &p.id));
        assert!(!r.rt.knows_permission(&p.id));
        assert_eq!(r.rt.sessions()[0].status, "interrupted");
        assert_eq!(state_statuses(&r.ops(&sid)).last().unwrap(), "ended");
        // an ended session refuses prompts; ending it again is a no-op
        let e = refusal(r.send(prompt("cmd-2", &sid, "more")));
        assert_eq!(e, "this Claude Code session has ended");
        ok(r.send(ClientMsg::SessionEnd {
            command_id: "cmd-end".into(),
            session_id: sid,
        }));
        assert_eq!(r.result_of("cmd-end"), Some(("completed".into(), None)));
    }

    #[test]
    fn idle_process_is_reaped_and_resumed() {
        let mut t = fast();
        t.idle_reap_ms = 150;
        let peers = vec![
            replay(fixture!("capture-3.ndjson")),
            replay(fixture!("capture-3.ndjson")),
        ];
        let r = rig(t, 3, peers);
        let sid = r.create("cmd-1", "run node");
        ok(r.approve("dec-1", &r.permission(0)));
        assert_eq!(r.wait_result("cmd-1").0, "completed");
        // idle, unwatched, nothing pending: stdin closes and the CLI exits
        r.wait("the reaper", |r| {
            r.rt.inner.state.lock().unwrap().sessions[0].io.is_none()
        });
        let ops = r.ops(&sid);
        assert_ne!(
            state_statuses(&ops).last().unwrap(),
            "ended",
            "reaped, not ended"
        );
        assert!(!r.rt.inner.state.lock().unwrap().sessions[0].ended);
        // the next prompt resumes the same session with the same driver
        ok(r.send(prompt("cmd-2", &sid, "again")));
        {
            let spawns = r.spawns.lock().unwrap();
            assert_eq!(spawns.len(), 2);
            let flag = |args: &[String], f: &str| args.windows(2).any(|w| w[0] == f && w[1] == sid);
            assert!(flag(&spawns[0], "--session-id"), "{:?}", spawns[0]);
            assert!(flag(&spawns[1], "--resume"), "{:?}", spawns[1]);
            assert!(!spawns[1].contains(&"--session-id".to_string()));
        }
        ok(r.approve("dec-2", &r.permission(1)));
        assert_eq!(r.wait_result("cmd-2").0, "completed");
        let ops = r.ops(&sid);
        assert_eq!(
            state_statuses(&ops)
                .iter()
                .filter(|s| *s == "starting")
                .count(),
            2
        );
        // `--resume` restored the cumulative cost: the kept driver sees +0
        let costs: Vec<f64> = op_of(&ops, "turn")
            .iter()
            .filter_map(|o| json::get(o, "turn").and_then(|t| json::get(t, "costUsd")))
            .filter_map(json::as_f64)
            .collect();
        assert_eq!(costs.len(), 2, "{costs:?}");
        assert!(costs[0] > 0.0 && costs[1] == 0.0, "{costs:?}");
    }

    #[test]
    fn max_sessions_and_rate_limit_refuse() {
        let mut t = fast();
        t.create_every_ms = 60_000;
        let r = rig(t, 2, vec![idle(), idle()]);
        r.create("cmd-a", "a");
        let e = refusal(r.send(create_msg("cmd-b", "b")));
        assert!(e.contains("just started"), "{e}");
        let r = rig(fast(), 2, vec![idle(), idle(), idle()]);
        r.create("cmd-a", "a");
        r.create("cmd-b", "b");
        let e = refusal(r.send(create_msg("cmd-c", "c")));
        assert_eq!(
            e,
            "at most 2 Claude Code sessions can run at once; end one first"
        );
        assert_eq!(
            r.spawns.lock().unwrap().len(),
            2,
            "nothing spawned when refused"
        );
        // the workspace must be one of the allow-listed ids
        let mut msg = create_msg("cmd-d", "d");
        if let ClientMsg::CreateSession { project, .. } = &mut msg {
            *project = Some("C:\\elsewhere".into());
        }
        let e = refusal(r.send(msg));
        assert!(e.contains("workspaces configured"), "{e}");
        // ending one frees a slot
        let sid = r.rt.sessions()[0].id.clone();
        ok(r.send(ClientMsg::SessionEnd {
            command_id: "cmd-end".into(),
            session_id: sid,
        }));
        assert_eq!(r.wait_result("cmd-end"), ("completed".into(), None));
        assert_eq!(r.rt.sessions()[0].status, "completed");
        r.create("cmd-e", "e");
    }

    /* ------------------------ sessions on disk ---------------------- */

    /// A temp workspace and a fake Claude `projects` folder.
    struct Disk {
        root: PathBuf,
        projects: PathBuf,
        ws: LiveWorkspace,
    }

    impl Disk {
        fn new() -> Disk {
            let root = std::env::temp_dir().join(format!("chb-claude-rt-{}", new_uuid().unwrap()));
            std::fs::create_dir_all(root.join("work").join("app")).unwrap();
            let work = root.join("work").to_string_lossy().into_owned();
            let ws = claude_live::validate_workspace(&work, &root.join("data"), None).unwrap();
            let projects = root.join("projects");
            Disk { root, projects, ws }
        }

        /// One prompt and one answer in `cwd`, last written `age_ms` ago.
        fn session(&self, cwd: &Path, id: &str, title: &str, age_ms: u64) {
            let cwd_json = json::stringify(&json::s(&cwd.to_string_lossy()));
            let lines = [
                format!(
                    r#"{{"type":"user","uuid":"u-{id}","cwd":{cwd_json},"timestamp":"2026-11-12T09:00:00.000Z","message":{{"role":"user","content":"{title}"}}}}"#
                ),
                format!(
                    r#"{{"type":"assistant","uuid":"a-{id}","cwd":{cwd_json},"timestamp":"2026-11-12T09:00:03.000Z","message":{{"id":"msg-{id}","role":"assistant","content":[{{"type":"text","text":"Hello from history"}}]}}}}"#
                ),
                r#"{"type":"cost-state","totalCostUSD":0.4}"#.to_string(),
            ];
            let dir = self
                .projects
                .join(claude_history::encode(&cwd.to_string_lossy()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{id}.jsonl"));
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            let when = std::time::SystemTime::now() - Duration::from_millis(age_ms);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(when)
                .unwrap();
        }

        fn rig(&self, max_sessions: usize, peers: Vec<React>) -> Rig {
            let (ws, projects) = (self.ws.clone(), self.projects.clone());
            rig_with(HubHandle::default(), fast(), max_sessions, peers, |c| {
                c.workspaces = vec![LiveWorkspace {
                    id: WS.into(),
                    ..ws
                }];
                c.projects = Some(projects);
            })
        }
    }

    impl Drop for Disk {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const OLD: &str = "0a0a0a0a-0000-4000-8000-000000000001";
    const BUSY: &str = "0b0b0b0b-0000-4000-8000-000000000002";

    #[test]
    fn disk_sessions_are_listed_replayed_and_resumed_in_their_cwd() {
        let d = Disk::new();
        let app = PathBuf::from(&d.ws.path).join("app");
        d.session(&app, OLD, "fix the parser", 10 * 60 * 1000);
        d.session(Path::new(&d.ws.path), BUSY, "still typing", 5_000);
        // the resumed CLI reports its cumulative cost, history included
        let resumed: React = Box::new(|f| {
            let done =
                r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.5}"#;
            let lines = if f.starts_with(r#"{"type":"user""#) {
                vec![done.to_string()]
            } else {
                Vec::new()
            };
            (lines, false)
        });
        let r = d.rig(1, vec![idle(), resumed]);
        // listed idle, newest first, with the recorded cwd; not driven
        let list = r.rt.sessions();
        let shape: Vec<(&str, &str, &str)> = list
            .iter()
            .map(|s| (s.id.as_str(), s.status.as_str(), s.title.as_str()))
            .collect();
        assert_eq!(
            shape,
            [
                (BUSY, "idle", "still typing"),
                (OLD, "idle", "fix the parser")
            ]
        );
        assert_eq!(list[1].project, app.to_string_lossy());
        assert!(list.iter().all(|s| s.transcript));
        assert!(r.rt.owns_session(OLD) && !r.rt.drives_session(OLD));
        // the first subscriber gets the history, once
        r.rt.on_subscribe(OLD);
        let texts = |r: &Rig| -> Vec<String> {
            op_of(&r.ops(OLD), "item")
                .iter()
                .filter_map(|o| json::get(o, "item").and_then(|i| json::get_str(i, "text")))
                .map(String::from)
                .collect()
        };
        assert_eq!(texts(&r), ["fix the parser", "Hello from history"]);
        r.rt.on_subscribe(OLD);
        assert_eq!(
            r.ops(OLD).len(),
            3,
            "two items and a turn, not replayed again"
        );
        // written 5 s ago by another process: refused, nothing spawned
        let e = refusal(r.send(prompt("cmd-busy", BUSY, "hi")));
        assert_eq!(e, IN_USE);
        // ending one the companion never ran is a no-op; it stays listed
        ok(r.send(ClientMsg::SessionEnd {
            command_id: "cmd-end-busy".into(),
            session_id: BUSY.into(),
        }));
        assert_eq!(
            r.result_of("cmd-end-busy"),
            Some(("completed".into(), None))
        );
        assert!(r.rt.owns_session(BUSY) && !r.rt.drives_session(BUSY));
        // the one slot is taken: the resume waits like any session
        let sid = r.create("cmd-create", "");
        let e = refusal(r.send(prompt("cmd-full", OLD, "continue")));
        assert!(e.starts_with("at most 1 Claude Code sessions"), "{e}");
        ok(r.send(ClientMsg::SessionEnd {
            command_id: "cmd-end".into(),
            session_id: sid,
        }));
        assert_eq!(r.wait_result("cmd-end").0, "completed");
        // now it resumes with `--resume <id>` in the recorded cwd
        ok(r.send(prompt("cmd-go", OLD, "continue")));
        {
            let spawns = r.spawns.lock().unwrap();
            assert_eq!(spawns.len(), 2);
            assert!(
                spawns[1]
                    .windows(2)
                    .any(|w| w[0] == "--resume" && w[1] == OLD),
                "{:?}",
                spawns[1]
            );
            assert_eq!(r.cwds.lock().unwrap()[1], app);
        }
        assert!(r.rt.drives_session(OLD));
        assert_eq!(r.wait_result("cmd-go"), ("completed".into(), None));
        let costs: Vec<f64> = op_of(&r.ops(OLD), "turn")
            .iter()
            .filter_map(|o| json::get(o, "turn").and_then(|t| json::get(t, "costUsd")))
            .filter_map(json::as_f64)
            .collect();
        assert_eq!(costs.len(), 1, "{costs:?}");
        assert!((costs[0] - 0.1).abs() < 1e-9, "this turn only: {costs:?}");
        // a rescan does not list it twice
        r.rt.refresh().unwrap();
        let ids: Vec<String> = r.rt.sessions().into_iter().map(|s| s.id).collect();
        assert_eq!(ids.iter().filter(|i| *i == OLD).count(), 1, "{ids:?}");
        assert_eq!(ids.len(), 3);
    }

    /* ------------------------- ADR-018 ---------------------------- */

    /// A rig over `d` whose hub tests can subscribe to, with a short mirror
    /// idle time.
    fn pc_rig(
        d: &Disk,
        idle_ms: i64,
        peers: Vec<React>,
    ) -> (Rig, Arc<Mutex<crate::transcript::Hub>>) {
        let hub = Arc::new(Mutex::new(crate::transcript::Hub::default()));
        let (ws, projects) = (d.ws.clone(), d.projects.clone());
        let mut timing = fast();
        timing.mirror_idle_ms = idle_ms;
        let r = rig_with(HubHandle::new(hub.clone()), timing, 3, peers, |c| {
            c.workspaces = vec![LiveWorkspace {
                id: WS.into(),
                ..ws
            }];
            c.projects = Some(projects);
        });
        (r, hub)
    }

    fn watch(hub: &Arc<Mutex<crate::transcript::Hub>>, sid: &str) {
        let (out, rx) =
            crate::server::Outbound::new(Arc::new(crate::companion::Saturation::default()), false);
        std::mem::forget(rx); // nobody reads it; the subscription is what counts
        let mut h = hub.lock().unwrap();
        h.ensure(sid, now_ms());
        h.subscribe(1, out, sid, None).unwrap();
    }

    fn transcript_of(d: &Disk, id: &str) -> PathBuf {
        d.projects
            .join(claude_history::encode(&d.ws.path))
            .join(format!("{id}.jsonl"))
    }

    fn append_line(path: &Path, line: &str) {
        use std::io::Write;
        let mut f = std::fs::File::options().append(true).open(path).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    fn item_ids(r: &Rig, sid: &str) -> Vec<String> {
        op_of(&r.ops(sid), "item")
            .iter()
            .filter_map(|o| json::get(o, "item").and_then(|i| json::get_str(i, "id")))
            .map(String::from)
            .collect()
    }

    /// The live mirror: a session another process writes is listed "Live on
    /// PC", its new lines stream to a subscriber as they land, and it stops
    /// being live (its turn closed) once the file goes quiet.
    #[test]
    fn pc_session_is_mirrored_live_then_goes_quiet() {
        let d = Disk::new();
        d.session(Path::new(&d.ws.path), BUSY, "still typing", 1_000);
        let (r, hub) = pc_rig(&d, 1_500, vec![]);
        let busy = r.rt.sessions().into_iter().find(|s| s.id == BUSY).unwrap();
        assert!(busy.live_pc, "written a second ago");
        watch(&hub, BUSY);
        r.rt.on_subscribe(BUSY);
        assert_eq!(
            item_ids(&r, BUSY),
            [format!("u-{BUSY}"), format!("msg-{BUSY}#0")]
        );
        assert_eq!(r.rt.inner.state.lock().unwrap().mirrors, [BUSY]);
        // the PC runs a tool, then answers and ends the turn
        let path = transcript_of(&d, BUSY);
        append_line(
            &path,
            r#"{"type":"user","uuid":"u2","timestamp":"2026-11-12T09:01:00.000Z","message":{"role":"user","content":"list files"}}"#,
        );
        append_line(
            &path,
            r#"{"type":"assistant","uuid":"a2","timestamp":"2026-11-12T09:01:01.000Z","message":{"id":"msg-2","role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_9","name":"Bash","input":{"command":"ls"}}]}}"#,
        );
        r.wait("the tool call", |r| {
            item_ids(r, BUSY).contains(&"toolu_9".into())
        });
        append_line(
            &path,
            r#"{"type":"user","uuid":"u3","timestamp":"2026-11-12T09:01:02.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_9","content":"a.txt"}]},"toolUseResult":{"stdout":"a.txt","stderr":""}}"#,
        );
        append_line(
            &path,
            r#"{"type":"assistant","uuid":"a3","timestamp":"2026-11-12T09:01:03.000Z","message":{"id":"msg-3","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"One file."}]}}"#,
        );
        r.wait("the answer", |r| {
            item_ids(r, BUSY).contains(&"msg-3#0".into())
        });
        let tool = op_of(&r.ops(BUSY), "item")
            .into_iter()
            .filter_map(|o| json::get(o, "item"))
            .rfind(|i| json::get_str(i, "id") == Some("toolu_9"))
            .cloned()
            .unwrap();
        assert_eq!(
            json::get(&tool, "tool").and_then(|t| json::get_str(t, "output")),
            Some("a.txt")
        );
        // quiet for 1.5 s: no longer live, said once to the phone
        r.wait("quiet", |r| {
            r.rt.inner.state.lock().unwrap().mirrors.is_empty()
        });
        let busy = r.rt.sessions().into_iter().find(|s| s.id == BUSY).unwrap();
        assert!(!busy.live_pc);
        assert!(
            r.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, EventKind::SessionUpdated(s) if s.id == BUSY && !s.live_pc))
        );
        // written again while still watched: the next scan starts a new
        // mirror; unwatched: it stops
        append_line(&path, r#"{"type":"last-prompt","lastPrompt":"x"}"#);
        r.rt.refresh().unwrap();
        assert_eq!(r.rt.inner.state.lock().unwrap().mirrors, [BUSY]);
        hub.lock().unwrap().unsubscribe(1, Some(BUSY));
        r.wait("unwatched", |r| {
            r.rt.inner.state.lock().unwrap().mirrors.is_empty()
        });
    }

    /// Fork: a session in use on the PC continues as a new session through
    /// `--resume <id> --fork-session --session-id <new>`; the file of the
    /// original is never written, and a plain prompt is still refused.
    #[test]
    fn fork_spawns_a_new_session_and_leaves_the_pc_one_alone() {
        let d = Disk::new();
        d.session(Path::new(&d.ws.path), BUSY, "still typing", 1_000);
        let before = std::fs::read(transcript_of(&d, BUSY)).unwrap();
        let (r, _hub) = pc_rig(&d, IN_USE_MS, vec![idle()]);
        assert_eq!(refusal(r.send(prompt("p0", BUSY, "hi"))), IN_USE);
        let fork = |cmd: &str, sid: &str| ClientMsg::SubmitPrompt {
            command_id: cmd.into(),
            session_id: sid.into(),
            text: "carry on".into(),
            images: vec![],
            fork: true,
        };
        let new = match r.send(fork("f1", BUSY)) {
            ServerMsg::Ack {
                ok: true,
                session_id: Some(s),
                ..
            } => s,
            other => panic!("{other:?}"),
        };
        assert!(new != BUSY && claude_live::is_uuid(&new));
        let args = r.spawns.lock().unwrap()[0].clone();
        let pair = |a: &str, b: &str| args.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(pair("--session-id", &new), "{args:?}");
        assert!(pair("--resume", BUSY), "{args:?}");
        assert!(args.contains(&"--fork-session".to_string()), "{args:?}");
        assert_eq!(r.cwds.lock().unwrap()[0], PathBuf::from(&d.ws.path));
        let s = r.rt.sessions().into_iter().find(|s| s.id == new).unwrap();
        assert_eq!(s.forked_from.as_deref(), Some("still typing"));
        assert!(r.rt.drives_session(&new) && !r.rt.drives_session(BUSY));
        // its view: the history, a notice, then the prompt
        let ids = item_ids(&r, &new);
        assert_eq!(
            ids[..3],
            [
                format!("u-{BUSY}"),
                format!("msg-{BUSY}#0"),
                "forked-from".into()
            ]
        );
        assert_eq!(ids.len(), 4, "{ids:?}");
        assert_eq!(std::fs::read(transcript_of(&d, BUSY)).unwrap(), before);
        // a session the companion drives is not forked
        let e = refusal(r.send(fork("f2", &new)));
        assert!(e.starts_with("only a session from the PC"), "{e}");
    }

    fn hook_payload(d: &Disk, sid: &str, tool: &str) -> Json {
        json::parse(&format!(
            r#"{{"session_id":"{sid}","transcript_path":"x","cwd":{},"permission_mode":"default","hook_event_name":"PermissionRequest","tool_name":"{tool}","tool_input":{{"command":"rm -rf build","description":"Clean the build"}}}}"#,
            json::stringify(&json::s(&d.ws.path))
        ))
        .unwrap()
    }

    fn hook_decision(cmd: &str, pid: &str, decision: &str, message: Option<&str>) -> ClientMsg {
        ClientMsg::PermissionDecision {
            command_id: cmd.into(),
            permission_id: pid.into(),
            decision: decision.into(),
            revision: 1.0,
            option_id: None,
            message: message.map(String::from),
            answers: vec![],
        }
    }

    /// Approvals for PC sessions: allow once or deny with a reason, nothing
    /// persistent; a request whose hook went away (the PC answered) or ran
    /// out is refused honestly afterwards.
    #[test]
    fn hook_requests_are_answered_by_the_phone_or_end_on_the_pc() {
        let d = Disk::new();
        d.session(Path::new(&d.ws.path), BUSY, "still typing", 1_000);
        let (r, _hub) = pc_rig(&d, IN_USE_MS, vec![]);
        let now = now_ms();
        let (pid, rx) =
            r.rt.hook_permission(&hook_payload(&d, BUSY, "Bash"), 60_000)
                .unwrap();
        let asked = r
            .events
            .lock()
            .unwrap()
            .iter()
            .find_map(|e| match e {
                EventKind::PermissionRequested(p) if p.id == pid => Some(p.clone()),
                _ => None,
            })
            .unwrap();
        assert!(asked.from_pc && asked.session_id == BUSY);
        assert_eq!(asked.command, "rm -rf build");
        let opts: Vec<(&str, &str, &str)> = asked
            .options
            .iter()
            .map(|o| (o.id.as_str(), o.decision.as_str(), o.scope))
            .collect();
        assert_eq!(
            opts,
            [
                ("allow", "approve-once", "once"),
                ("deny", "reject", "once")
            ]
        );
        assert!((asked.expires_at - now - 60_000).abs() < 5_000);
        assert!(r.rt.knows_permission(&pid));
        ok(r.send(hook_decision("d1", &pid, "approve-once", None)));
        assert_eq!(rx.recv().unwrap(), HookVerdict::Allow);
        assert!(!refusal(r.send(hook_decision("d1b", &pid, "approve-once", None))).is_empty());

        let (pid, rx) =
            r.rt.hook_permission(&hook_payload(&d, BUSY, "Bash"), 60_000)
                .unwrap();
        ok(r.send(hook_decision("d2", &pid, "reject", Some("not on main"))));
        assert_eq!(rx.recv().unwrap(), HookVerdict::Deny("not on main".into()));

        // the PC answered first: the hook is gone
        let (pid, _rx) =
            r.rt.hook_permission(&hook_payload(&d, BUSY, "Bash"), 60_000)
                .unwrap();
        r.rt.hook_ended(&pid, HookEnd::AnsweredOnPc);
        assert!(r.events.lock().unwrap().iter().any(
            |e| matches!(e, EventKind::PermissionResolved(p) if p.id == pid && p.status == "stale")
        ));
        assert_eq!(
            refusal(r.send(hook_decision("d3", &pid, "approve-once", None))),
            ANSWERED_ON_PC
        );
        // its waiter vanished before the tap
        let (pid, rx) =
            r.rt.hook_permission(&hook_payload(&d, BUSY, "Bash"), 60_000)
                .unwrap();
        drop(rx);
        assert_eq!(
            refusal(r.send(hook_decision("d4", &pid, "approve-once", None))),
            ANSWERED_ON_PC
        );
        // the wait ran out
        let (pid, _rx) =
            r.rt.hook_permission(&hook_payload(&d, BUSY, "Bash"), 60_000)
                .unwrap();
        r.rt.hook_ended(&pid, HookEnd::Expired);
        assert!(r.events.lock().unwrap().iter().any(
            |e| matches!(e, EventKind::PermissionResolved(p) if p.id == pid && p.status == "expired")
        ));
        assert_eq!(
            refusal(r.send(hook_decision("d5", &pid, "approve-once", None))),
            HOOK_EXPIRED
        );

        // not the phone's: a question, a cwd outside, a bad id, too many
        assert!(
            r.rt.hook_permission(&hook_payload(&d, BUSY, "AskUserQuestion"), 1_000)
                .is_none()
        );
        let mut outside = hook_payload(&d, BUSY, "Bash");
        if let Json::Obj(f) = &mut outside {
            f.retain(|(k, _)| k != "cwd");
            f.push(("cwd".into(), json::s(&d.root.to_string_lossy())));
        }
        assert!(r.rt.hook_permission(&outside, 1_000).is_none());
        assert!(
            r.rt.hook_permission(&hook_payload(&d, "not-a-uuid", "Bash"), 1_000)
                .is_none()
        );
        let held: Vec<_> = (0..HOOKS_MAX)
            .map(|_| {
                r.rt.hook_permission(&hook_payload(&d, BUSY, "Bash"), 1_000)
                    .unwrap()
            })
            .collect();
        assert!(
            r.rt.hook_permission(&hook_payload(&d, BUSY, "Bash"), 1_000)
                .is_none()
        );
        drop(held);
    }

    /// A session the companion drives answers through its own stdio
    /// permission channel: its hook gets nothing from here.
    #[test]
    fn hooks_from_a_driven_session_are_not_doubled() {
        let d = Disk::new();
        let (r, _hub) = pc_rig(&d, IN_USE_MS, vec![idle()]);
        let sid = r.create("c1", "");
        assert!(
            r.rt.hook_permission(&hook_payload(&d, &sid, "Bash"), 1_000)
                .is_none()
        );
    }

    /* ------------------------------ MCP ---------------------------- */

    /// A CLI with two remote servers that need a sign-in and a claude.ai
    /// connector. `remote` connects two status polls after its sign-in
    /// starts (the desktop browser finished it); `remote2` once the phone's
    /// callback URL is relayed.
    fn mcp_peer() -> React {
        let mut remote_in: Option<u32> = None;
        let mut remote2 = false;
        Box::new(move |f| {
            let v = json::parse(f).unwrap();
            let id = json::get_str(&v, "request_id").unwrap_or("").to_string();
            let req = json::get(&v, "request").cloned().unwrap_or(Json::Null);
            let server = json::get_str(&req, "serverName").unwrap_or("").to_string();
            let body = match json::get_str(&req, "subtype").unwrap_or("") {
                "initialize" | "mcp_reconnect" => "{}".to_string(),
                "mcp_status" => {
                    let remote = match remote_in.as_mut() {
                        Some(0) => "connected",
                        Some(n) => {
                            *n -= 1;
                            "needs-auth"
                        }
                        None => "needs-auth",
                    };
                    let remote2 = if remote2 { "connected" } else { "needs-auth" };
                    let entry = |name: &str, status: &str, kind: &str| {
                        format!(
                            r#"{{"name":"{name}","status":"{status}","config":{{"type":"{kind}","url":"https://mcp.example/x"}}}}"#
                        )
                    };
                    format!(
                        r#"{{"mcpServers":[{},{},{}]}}"#,
                        entry("remote", remote, "http"),
                        entry("remote2", remote2, "http"),
                        entry("claude.ai Gmail", "needs-auth", "claudeai-proxy")
                    )
                }
                "mcp_authenticate" => {
                    let port = if server == "remote" { 40960 } else { 40961 };
                    if server == "remote" {
                        remote_in = Some(2);
                    }
                    format!(
                        r#"{{"authUrl":"https://auth.example/authorize?client_id=c&redirect_uri=http%3A%2F%2Flocalhost%3A{port}%2Fcallback&state=st-{port}","requiresUserAction":true,"callbackExpected":true,"redirectScheme":"localhost","state":"st-{port}","callbackPort":{port}}}"#
                    )
                }
                "mcp_oauth_callback_url" => {
                    remote2 =
                        json::get_str(&req, "callbackUrl").is_some_and(|u| u.contains("code="));
                    "{}".to_string()
                }
                _ => return (Vec::new(), false),
            };
            let reply = format!(
                r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{id}","response":{body}}}}}"#
            );
            (vec![reply], false)
        })
    }

    #[test]
    fn mcp_desktop_flow_opens_runtime_url_only() {
        let r = rig(fast(), 3, vec![mcp_peer()]);
        let sid = r.create("cmd-create", "");
        // no prompt: the create completes on initialize, then MCP is polled
        assert_eq!(r.wait_result("cmd-create"), ("completed".into(), None));
        r.wait("mcp status", |r| !op_of(&r.ops(&sid), "mcp").is_empty());
        let auth = |cmd: &str, server: &str, flow: &str| ClientMsg::McpAuth {
            command_id: cmd.into(),
            session_id: sid.clone(),
            server: server.into(),
            flow: flow.into(),
        };
        // claude.ai connectors and unknown servers never reach the opener
        let e = refusal(r.send(auth("cmd-ai", "claude.ai Gmail", "desktop")));
        assert_eq!(e, "claude.ai connectors are signed in on claude.ai");
        let e = refusal(r.send(auth("cmd-x", "nope", "desktop")));
        assert_eq!(e, "unknown MCP server");
        // desktop: the runtime's own URL opens on the PC, polls see it connect
        ok(r.send(auth("cmd-desk", "remote", "desktop")));
        assert_eq!(r.wait_result("cmd-desk"), ("completed".into(), None));
        assert_eq!(r.progress_of("cmd-desk"), ["accepted", "running"]);
        let url = "https://auth.example/authorize?client_id=c&redirect_uri=http%3A%2F%2Flocalhost%3A40960%2Fcallback&state=st-40960";
        assert_eq!(*r.opened.lock().unwrap(), [url]);
        // phone-callback: the URL and callback prefix go to the phone only
        ok(r.send(auth("cmd-phone", "remote2", "phone-callback")));
        r.wait("phone sign-in", |r| r.progress_of("cmd-phone").len() == 2);
        let ops = r.ops(&sid);
        let last = json::stringify(op_of(&ops, "mcp").last().unwrap());
        assert!(last.contains(r#""state":"awaiting-callback""#), "{last}");
        let prefix = r#""callbackPrefix":"http://localhost:40961/callback""#;
        assert!(last.contains(prefix), "{last}");
        assert_eq!(
            r.opened.lock().unwrap().len(),
            1,
            "nothing opened on the PC"
        );
        let callback = |cmd: &str, state: &str| ClientMsg::McpAuthCallback {
            command_id: cmd.into(),
            session_id: sid.clone(),
            server: "remote2".into(),
            callback_url: format!("http://localhost:40961/callback?code=c0de&state={state}"),
        };
        // a forged state is refused, and the sign-in is spent (single use)
        let e = refusal(r.send(callback("cmd-cb-bad", "forged")));
        assert_eq!(e, "callback state does not match the sign-in");
        assert_eq!(r.wait_result("cmd-phone").0, "failed");
        let e = refusal(r.send(callback("cmd-cb-again", "st-40961")));
        assert_eq!(e, "no phone sign-in is waiting for this server");
        // a fresh sign-in and the right callback: relayed, reconnected, connected
        ok(r.send(auth("cmd-phone-2", "remote2", "phone-callback")));
        r.wait("phone sign-in", |r| r.progress_of("cmd-phone-2").len() == 2);
        ok(r.send(callback("cmd-cb", "st-40961")));
        assert_eq!(r.wait_result("cmd-cb"), ("completed".into(), None));
        assert_eq!(r.wait_result("cmd-phone-2"), ("completed".into(), None));
        let relayed = r.stdin.lock().unwrap().iter().any(|f| {
            f.contains(r#""subtype":"mcp_oauth_callback_url","serverName":"remote2","callbackUrl":"http://localhost:40961/callback?code=c0de&state=st-40961""#)
        });
        assert!(relayed);
        // a reconnect completes on the next connected status
        ok(r.send(ClientMsg::McpReconnect {
            command_id: "cmd-rc".into(),
            session_id: sid,
            server: "remote".into(),
        }));
        assert_eq!(r.wait_result("cmd-rc"), ("completed".into(), None));
    }

    #[test]
    fn set_permission_mode_is_written_and_whitelisted() {
        let r = rig(fast(), 3, vec![mcp_peer()]);
        let sid = r.create("cmd-create", "");
        assert_eq!(r.wait_result("cmd-create").0, "completed");
        ok(r.send(ClientMsg::SetPermissionMode {
            command_id: "cmd-mode".into(),
            session_id: sid.clone(),
            mode: "plan".into(),
        }));
        r.wait("the mode frame", |r| {
            r.stdin
                .lock()
                .unwrap()
                .iter()
                .any(|f| f.contains(r#""request":{"subtype":"set_permission_mode","mode":"plan"}"#))
        });
        // mcp_peer never answers it: no outcome is claimed
        assert_eq!(r.result_of("cmd-mode"), None);
        let e = refusal(r.send(ClientMsg::SetPermissionMode {
            command_id: "cmd-bad".into(),
            session_id: sid,
            mode: "bypassPermissions".into(),
        }));
        assert_eq!(e, "permission mode must be default, acceptEdits or plan");
    }

    /// A CLI that answers the ADR-017 requests the way 2.1.322 does.
    fn composer_peer() -> React {
        Box::new(move |f| {
            let v = json::parse(f).unwrap();
            if json::get_str(&v, "type") == Some("user") {
                let init = r#"{"type":"system","subtype":"init","model":"claude-haiku-5-5","permissionMode":"default","slash_commands":["compact","clear","context","__remote-workflow","review"]}"#;
                let done = r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.001}"#;
                return (vec![init.into(), done.into()], false);
            }
            let id = json::get_str(&v, "request_id").unwrap_or("").to_string();
            let req = json::get(&v, "request").cloned().unwrap_or(Json::Null);
            let ok = |body: &str| {
                format!(
                    r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{id}","response":{body}}}}}"#
                )
            };
            let lines = match json::get_str(&req, "subtype").unwrap_or("") {
                "initialize" => vec![ok(
                    r#"{"models":[{"value":"default","displayName":"Default (recommended)"},{"value":"haiku","displayName":"Haiku 5.5"},{"value":"sonnet[1m]","displayName":"Sonnet (1M)"}],"commands":[{"name":"compact","description":"…"},{"name":"clear"},{"name":"heapdump"},{"name":"review"}],"account":{"email":"never@example.com"}}"#,
                )],
                "get_context_usage" => vec![ok(
                    r#"{"totalTokens":22900,"maxTokens":200000,"percentage":11}"#,
                )],
                "set_model" => match json::get_str(&req, "model") {
                    Some("opus") => vec![ok(r#"{"model":"claude-opus-5-5"}"#)],
                    _ => vec![format!(
                        r#"{{"type":"control_response","response":{{"subtype":"error","request_id":"{id}","error":"Model 'claude-nope-1' not found"}}}}"#
                    )],
                },
                "rename_session" => {
                    let title = json::get_str(&req, "title").unwrap_or("");
                    vec![
                        format!(
                            r#"{{"type":"system","subtype":"session_title_changed","title":{}}}"#,
                            json::stringify(&json::s(title))
                        ),
                        ok("{}"),
                    ]
                }
                "mcp_status" => vec![ok(r#"{"mcpServers":[]}"#)],
                _ => Vec::new(),
            };
            (lines, false)
        })
    }

    #[test]
    fn composer_model_images_commands_context_and_rename() {
        let r = rig(fast(), 3, vec![composer_peer()]);
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEUlEQVR4nGP4z8DwnwGMgRQAH+4D/dJQfRoAAAAASUVORK5CYII=";
        let image = PromptImage {
            media_type: "image/png".into(),
            data: png.into(),
        };
        let mut create = create_msg("cmd-create", "What color is this?");
        if let ClientMsg::CreateSession { images, model, .. } = &mut create {
            *images = vec![image];
            *model = Some("haiku".into());
        }
        let sid = match r.send(create) {
            ServerMsg::Ack {
                ok: true,
                session_id: Some(s),
                ..
            } => s,
            other => panic!("{other:?}"),
        };
        assert_eq!(r.wait_result("cmd-create").0, "completed");
        let args = r.spawns.lock().unwrap()[0].join(" ");
        assert!(args.contains("--model haiku"), "{args}");
        // the image goes to the CLI first in the user frame…
        let user = r
            .stdin
            .lock()
            .unwrap()
            .iter()
            .find(|f| f.starts_with(r#"{"type":"user""#))
            .cloned();
        let user = user.expect("a user frame");
        let content = format!(
            r#""content":[{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{png}"}}}},{{"type":"text","text":"What color is this?"}}]"#
        );
        assert!(user.contains(&content), "{user}");
        // …and never into the hub: the user item holds a count
        r.wait("context", |r| {
            r.ops(&sid)
                .iter()
                .any(|o| json::stringify(o).contains("usedTokens"))
        });
        let ops = r.ops(&sid);
        let all: String = ops.iter().map(json::stringify).collect();
        assert!(!all.contains(&png[..20]), "image bytes reached the hub");
        let item = op_of(&ops, "item")
            .into_iter()
            .filter_map(|o| json::get(o, "item"))
            .find(|i| json::get_str(i, "kind") == Some("user"))
            .unwrap();
        assert_eq!(json::get_i64(item, "images"), Some(1));
        // state: models the phone may pick, commands minus the hidden ones, context
        let state = op_of(&ops, "state")
            .last()
            .and_then(|o| json::get(o, "state"))
            .unwrap();
        assert_eq!(
            json::stringify(json::get(state, "slashCommands").unwrap()),
            r#"["compact","context","review"]"#
        );
        assert_eq!(
            json::stringify(json::get(state, "models").unwrap()),
            r#"[{"value":"default","label":"Default (recommended)"},{"value":"haiku","label":"Haiku 5.5"}]"#
        );
        assert_eq!(
            json::stringify(json::get(state, "context").unwrap()),
            r#"{"usedTokens":22900,"maxTokens":200000}"#
        );
        assert!(!all.contains("never@example.com"));
        // set_model: the resolved id lands on state; an unknown model fails
        ok(r.send(ClientMsg::SetModel {
            command_id: "cmd-model".into(),
            session_id: sid.clone(),
            model: "opus".into(),
        }));
        assert_eq!(r.wait_result("cmd-model"), ("completed".into(), None));
        let model = |r: &Rig| {
            op_of(&r.ops(&sid), "state")
                .last()
                .and_then(|o| json::get(o, "state"))
                .and_then(|s| json::get_str(s, "model"))
                .map(String::from)
        };
        assert_eq!(model(&r).as_deref(), Some("claude-opus-5-5"));
        ok(r.send(ClientMsg::SetModel {
            command_id: "cmd-model-bad".into(),
            session_id: sid.clone(),
            model: "claude-nope-1".into(),
        }));
        let (status, error) = r.wait_result("cmd-model-bad");
        assert_eq!(status, "failed");
        assert!(error.unwrap().contains("not found"));
        assert_eq!(model(&r).as_deref(), Some("claude-opus-5-5"));
        let e = refusal(r.send(ClientMsg::SetModel {
            command_id: "cmd-model-flag".into(),
            session_id: sid.clone(),
            model: "--dangerously-skip-permissions".into(),
        }));
        assert!(e.starts_with("model must be"), "{e}");
        // rename: the CLI's own title event renames the session
        ok(r.send(ClientMsg::RenameSession {
            command_id: "cmd-name".into(),
            session_id: sid.clone(),
            title: "Colour check".into(),
        }));
        assert_eq!(r.wait_result("cmd-name"), ("completed".into(), None));
        r.wait("title", |r| r.rt.sessions()[0].title == "Colour check");
        let frame = r.stdin.lock().unwrap().iter().any(|f| {
            f.contains(&format!(
                r#""request":{{"subtype":"rename_session","title":"Colour check","source":"host","session_id":"{sid}"}}"#
            ))
        });
        assert!(frame);
    }

    /* ---------------------------- live ---------------------------- */

    /// ADR-017 on the real CLI (Haiku, about $0.01): set_model, a 2x2 red
    /// PNG with "what color", rename, context usage and the slash command
    /// list: `CHERNOBYL_LIVE_CLAUDE=1 ANTHROPIC_MODEL=haiku cargo test --lib
    /// live_claude_haiku_composer -- --ignored --nocapture`. Prints counts,
    /// the answer word and the cost only.
    #[test]
    #[ignore = "spawns the real Claude Code CLI"]
    fn live_claude_haiku_composer() {
        if std::env::var_os("CHERNOBYL_LIVE_CLAUDE").is_none() {
            return;
        }
        let parent = std::env::var_os("CHERNOBYL_LIVE_CLAUDE_DIR")
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let root = parent.join(format!("chb-live-composer-{}", now_ms()));
        let (work, data) = (root.join("work"), root.join("data"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let entry = AdapterEntryConfig {
            id: "claude-1".into(),
            kind: "claude".into(),
            enabled: true,
            workspaces: vec![work.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let mut cfg = LiveConfig::from_entry(&entry, &data).unwrap().unwrap();
        cfg.projects = None;
        let ws = cfg.workspaces[0].id.clone();
        let hub = HubHandle::default();
        let rt = ClaudeRuntime::new("claude-1", cfg, hub.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        rt.set_sink(Arc::new(move |k| sink.lock().unwrap().push(k)));
        rt.refresh().unwrap();
        let result_of = |cmd: &str| {
            let deadline = Instant::now() + Duration::from_secs(180);
            loop {
                let found = events.lock().unwrap().iter().find_map(|e| match e {
                    EventKind::CommandResult {
                        command_id,
                        status,
                        error,
                        ..
                    } if command_id == cmd => Some((status.clone(), error.clone())),
                    _ => None,
                });
                if let Some(r) = found {
                    return r;
                }
                assert!(Instant::now() < deadline, "no result for {cmd}");
                std::thread::sleep(Duration::from_millis(100));
            }
        };
        let sid = match rt
            .send(&ClientMsg::CreateSession {
                command_id: "c1".into(),
                agent_id: "claude-1".into(),
                title: "live composer".into(),
                prompt: String::new(),
                project: Some(ws),
                images: vec![],
                model: None,
            })
            .unwrap()
        {
            ServerMsg::Ack {
                ok: true,
                session_id: Some(s),
                ..
            } => s,
            other => panic!("{other:?}"),
        };
        assert_eq!(result_of("c1").0, "completed");
        ok(rt
            .send(&ClientMsg::SetModel {
                command_id: "m1".into(),
                session_id: sid.clone(),
                model: "haiku".into(),
            })
            .unwrap());
        assert_eq!(result_of("m1"), ("completed".into(), None));
        // a 2x2 red PNG (the bytes of the research probe)
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEUlEQVR4nGP4z8DwnwGMgRQAH+4D/dJQfRoAAAAASUVORK5CYII=";
        ok(rt
            .send(&ClientMsg::SubmitPrompt {
                command_id: "p1".into(),
                session_id: sid.clone(),
                text: "What color is this image? Answer with one word, no tools.".into(),
                images: vec![PromptImage {
                    media_type: "image/png".into(),
                    data: png.into(),
                }],
                fork: false,
            })
            .unwrap());
        assert_eq!(result_of("p1").0, "completed");
        ok(rt
            .send(&ClientMsg::RenameSession {
                command_id: "r1".into(),
                session_id: sid.clone(),
                title: "Live composer check".into(),
            })
            .unwrap());
        assert_eq!(result_of("r1"), ("completed".into(), None));
        let deadline = Instant::now() + Duration::from_secs(30);
        let ops = loop {
            let ops: Vec<Json> = hub
                .ops(&sid)
                .iter()
                .map(|o| json::parse(o).unwrap())
                .collect();
            let titled = rt
                .sessions()
                .iter()
                .any(|s| s.title == "Live composer check");
            let context = ops
                .iter()
                .any(|o| json::stringify(o).contains("usedTokens"));
            if titled && context {
                break ops;
            }
            assert!(
                Instant::now() < deadline,
                "title {titled} context {context}"
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        let all: String = ops.iter().map(json::stringify).collect();
        assert!(!all.contains(&png[..20]), "image bytes reached the hub");
        let answer: String = op_of(&ops, "item")
            .iter()
            .filter_map(|o| json::get(o, "item"))
            .filter(|i| json::get_str(i, "kind") == Some("text"))
            .filter_map(|i| json::get_str(i, "text"))
            .collect();
        let state = op_of(&ops, "state")
            .last()
            .and_then(|o| json::get(o, "state"))
            .cloned()
            .unwrap();
        let commands = json::get(&state, "slashCommands")
            .and_then(json::as_arr)
            .map_or(0, |c| c.len());
        let models = json::get(&state, "models")
            .and_then(json::as_arr)
            .map_or(0, |c| c.len());
        let context = json::get(&state, "context")
            .map(json::stringify)
            .unwrap_or_default();
        let cost: f64 = op_of(&ops, "turn")
            .iter()
            .filter_map(|o| json::get(o, "turn").and_then(|t| json::get(t, "costUsd")))
            .filter_map(json::as_f64)
            .sum();
        println!(
            "live: model={:?} answer={:?} commands={commands} models={models} context={context} cost_usd={cost:.4}",
            json::get_str(&state, "model"),
            answer.trim()
        );
        rt.dispose();
        assert!(answer.to_ascii_lowercase().contains("red"), "{answer}");
        assert!(json::get_str(&state, "model").is_some_and(|m| m.contains("haiku")));
        assert!(commands > 0 && models > 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A real transcript from `claude -p` on Haiku in a temp workspace is
    /// found, refused while fresh, replayed, then continued with one prompt
    /// (about $0.01): `CHERNOBYL_LIVE_CLAUDE=1 cargo test --lib
    /// live_claude_haiku_disk_session -- --ignored --nocapture`. Prints
    /// counts and the cost only, never the transcript.
    #[test]
    #[ignore = "spawns the real Claude Code CLI"]
    fn live_claude_haiku_disk_session_is_found_replayed_and_resumed() {
        if std::env::var_os("CHERNOBYL_LIVE_CLAUDE").is_none() {
            return;
        }
        let parent = std::env::var_os("CHERNOBYL_LIVE_CLAUDE_DIR")
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let root = parent.join(format!("chb-live-disk-{}", now_ms()));
        let (work, data) = (root.join("work"), root.join("data"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let entry = AdapterEntryConfig {
            id: "claude-1".into(),
            kind: "claude".into(),
            enabled: true,
            workspaces: vec![work.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let mut cfg = LiveConfig::from_entry(&entry, &data).unwrap().unwrap();
        cfg.extra_args = vec!["--model".into(), "haiku".into()];
        let mut env = spawn_env(std::env::vars_os());
        env.push(("ANTHROPIC_MODEL".into(), "haiku".into()));
        let first = std::process::Command::new(&cfg.binary)
            .args([
                "-p",
                "say hi",
                "--model",
                "haiku",
                "--output-format",
                "json",
            ])
            .current_dir(&cfg.workspaces[0].path)
            .env_clear()
            .envs(env)
            .output()
            .unwrap();
        assert!(
            first.status.success(),
            "claude -p exited {:?}",
            first.status
        );
        let out = json::parse(String::from_utf8_lossy(&first.stdout).trim()).unwrap();
        let sid = json::get_str(&out, "session_id").unwrap().to_string();
        let cost1 = json::get(&out, "total_cost_usd")
            .and_then(json::as_f64)
            .unwrap_or(0.0);
        let hub = HubHandle::default();
        let rt = ClaudeRuntime::new("claude-1", cfg, hub.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        rt.set_sink(Arc::new(move |k| sink.lock().unwrap().push(k)));
        let started = Instant::now();
        let _ = rt.refresh();
        let scan_ms = started.elapsed().as_millis();
        let listed = rt.sessions().into_iter().find(|s| s.id == sid);
        let listed = listed.expect("the new session is listed");
        assert_eq!(listed.status, "idle");
        // just written: another process may still own it
        let send = |cmd: &str| {
            rt.send(&ClientMsg::SubmitPrompt {
                command_id: cmd.into(),
                session_id: sid.clone(),
                text: "Reply with the single word: ok".into(),
                images: vec![],
                fork: false,
            })
            .unwrap()
        };
        assert_eq!(refusal(send("p0")), IN_USE);
        rt.on_subscribe(&sid);
        let ops: Vec<Json> = hub
            .ops(&sid)
            .iter()
            .map(|o| json::parse(o).unwrap())
            .collect();
        let kinds: Vec<&str> = op_of(&ops, "item")
            .iter()
            .filter_map(|o| json::get(o, "item").and_then(|i| json::get_str(i, "kind")))
            .collect();
        assert!(
            kinds.contains(&"user") && kinds.contains(&"text"),
            "{kinds:?}"
        );
        // an hour later as far as the in-use rule goes
        let path = {
            let st = rt.inner.state.lock().unwrap();
            st.found.iter().find(|f| f.id == sid).unwrap().path.clone()
        };
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        ok(send("p1"));
        let deadline = Instant::now() + Duration::from_secs(180);
        let result = loop {
            let found = events.lock().unwrap().iter().find_map(|e| match e {
                EventKind::CommandResult {
                    command_id, status, ..
                } if command_id == "p1" => Some(status.clone()),
                _ => None,
            });
            if let Some(s) = found {
                break s;
            }
            assert!(
                Instant::now() < deadline,
                "no result for the resumed prompt"
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        let ops: Vec<Json> = hub
            .ops(&sid)
            .iter()
            .map(|o| json::parse(o).unwrap())
            .collect();
        let cost2: f64 = op_of(&ops, "turn")
            .iter()
            .filter_map(|o| json::get(o, "turn").and_then(|t| json::get(t, "costUsd")))
            .filter_map(json::as_f64)
            .sum();
        println!(
            "live: scan_ms={scan_ms} history_items={} result={result} first_cost_usd={cost1:.4} \
             resumed_turn_cost_usd={cost2:.4}",
            kinds.len()
        );
        rt.dispose();
        assert_eq!(result, "completed");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The real companion server in-process, a WS client acting as the phone
    /// and the real CLI on Haiku (about $0.02): `CHERNOBYL_LIVE_CLAUDE=1
    /// cargo test --lib live_claude_haiku_inline -- --ignored --nocapture`.
    /// `CHERNOBYL_LIVE_CLAUDE_DIR` is the parent of the temp data dir and
    /// workspace. Prints counts and the cost only: never the account.
    #[test]
    #[ignore = "spawns the real Claude Code CLI"]
    fn live_claude_haiku_inline_permission_round_trip() {
        use crate::client::WsClient;
        use crate::companion::Companion;
        use crate::config::default_config;
        if std::env::var_os("CHERNOBYL_LIVE_CLAUDE").is_none() {
            return;
        }
        let parent = std::env::var_os("CHERNOBYL_LIVE_CLAUDE_DIR")
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let root = parent.join(format!("chb-live-rt-{}", now_ms()));
        let (work, data) = (root.join("work"), root.join("data"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let mut c = default_config();
        c.data_dir = data.clone();
        let mut entry = AdapterEntryConfig {
            id: "claude-1".into(),
            kind: "claude".into(),
            enabled: true,
            ..Default::default()
        };
        c.adapters.push(entry.clone()); // the ingress; the runtime below
        let mut comp = Companion::boot(c).unwrap();
        entry.workspaces = vec![work.to_string_lossy().into_owned()];
        let mut cfg = LiveConfig::from_entry(&entry, &data).unwrap().unwrap();
        let ws_id = cfg.workspaces[0].id.clone();
        let canonical = cfg.workspaces[0].path.to_ascii_lowercase();
        cfg.extra_args = vec!["--model".into(), "haiku".into()];
        let rt = Arc::new(ClaudeRuntime::new("claude-1", cfg, comp.hub()));
        comp.attach_claude_live(rt.clone());
        assert!(rt.is_healthy(), "claude --version");
        eprintln!("live: runtime healthy");
        let comp = Arc::new(comp);
        let server = crate::server::start("127.0.0.1", 0, 0, None, comp.clone()).unwrap();
        let mut phone =
            WsClient::connect(&format!("127.0.0.1:{}", server.port), "/", None).unwrap();
        phone.hello().unwrap();
        eprintln!("live: hello");
        phone.set_read_timeout_ms(500).unwrap();
        let send = |phone: &mut WsClient, v: Json| phone.send_text(&json::stringify(&v)).unwrap();
        let msg = |pairs: Vec<(&str, &str)>| {
            json::obj(pairs.into_iter().map(|(k, v)| (k, json::s(v))).collect())
        };
        send(
            &mut phone,
            msg(vec![
                ("type", "create_session"),
                ("commandId", "c1"),
                ("agentId", "claude-1"),
                ("title", "live"),
                (
                    "prompt",
                    "Create a.txt containing hi, then edit it to say hello",
                ),
                ("project", &ws_id),
            ]),
        );
        let (mut sid, mut perms, mut appends, mut costs) = (String::new(), 0, 0, Vec::<f64>::new());
        let (mut edit_diff, mut snapshot, mut ended) = (false, false, false);
        let mut results: Vec<(String, String)> = Vec::new();
        let mut progress: Vec<(String, String)> = Vec::new();
        let mut acks: Vec<(String, bool)> = Vec::new();
        let result = |results: &[(String, String)], cmd: &str| {
            results
                .iter()
                .find(|(c, _)| c == cmd)
                .map(|(_, s)| s.clone())
        };
        let deadline = Instant::now() + Duration::from_secs(300);
        while Instant::now() < deadline && !(ended && result(&results, "e1").is_some()) {
            let Some(text) = phone.next_text_timeoutable().unwrap() else {
                continue;
            };
            let v = json::parse(&text).unwrap();
            if std::env::var_os("CHERNOBYL_LIVE_TRACE").is_some() {
                eprintln!("live: {}", text.chars().take(160).collect::<String>());
            }
            let get = |k: &str| json::get_str(&v, k).unwrap_or("").to_string();
            match get("type").as_str() {
                "ack" => {
                    let ok = json::get_bool(&v, "ok") == Some(true);
                    acks.push((get("commandId"), ok));
                    if get("commandId") == "c1" {
                        assert!(ok, "{text}");
                        sid = get("sessionId");
                        send(
                            &mut phone,
                            msg(vec![
                                ("type", "session_subscribe"),
                                ("commandId", "s1"),
                                ("sessionId", &sid),
                            ]),
                        );
                    }
                }
                "transcript_snapshot" => snapshot = true,
                "transcript" => {
                    for op in json::get(&v, "ops").and_then(json::as_arr).unwrap_or(&[]) {
                        match json::get_str(op, "op") {
                            Some("append") => appends += 1,
                            Some("turn") => {
                                let turn = json::get(op, "turn").unwrap();
                                if let Some(c) = json::get(turn, "costUsd").and_then(json::as_f64) {
                                    costs.push(c);
                                }
                            }
                            Some("item") => {
                                let tool = json::get(op, "item").and_then(|i| json::get(i, "tool"));
                                // the edit's diff (Haiku may rewrite with Write)
                                let edit = tool.is_some_and(|t| {
                                    matches!(json::get_str(t, "name"), Some("Edit" | "Write"))
                                        && json::get(t, "diffs")
                                            .is_some_and(|d| json::stringify(d).contains("+hello"))
                                });
                                if edit && !edit_diff {
                                    edit_diff = true;
                                    // the file is edited: stop whatever runs now
                                    send(
                                        &mut phone,
                                        msg(vec![
                                            ("type", "interrupt"),
                                            ("commandId", "i1"),
                                            ("sessionId", &sid),
                                        ]),
                                    );
                                }
                            }
                            Some("state") => {
                                let s =
                                    json::get(op, "state").and_then(|s| json::get_str(s, "status"));
                                ended |= s == Some("ended");
                            }
                            _ => {}
                        }
                    }
                }
                "permission_requested" => {
                    let p = json::get(&v, "permission").unwrap();
                    let pid = json::get_str(p, "id").unwrap().to_string();
                    let inside = json::get_str(p, "command")
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .starts_with(&canonical);
                    let revision = json::get_i64(p, "revision").unwrap_or(1);
                    let options = json::get(p, "options")
                        .and_then(json::as_arr)
                        .unwrap_or(&[]);
                    let pick = |d: &str| {
                        options
                            .iter()
                            .find(|o| json::get_str(o, "decision") == Some(d))
                            .and_then(|o| json::get_str(o, "id"))
                            .unwrap()
                            .to_string()
                    };
                    let decision = if inside { "approve-once" } else { "reject" };
                    perms += 1;
                    let mut d = msg(vec![
                        ("type", "permission_decision"),
                        ("commandId", &format!("d{perms}")),
                        ("permissionId", &pid),
                        ("decision", decision),
                        ("optionId", &pick(decision)),
                    ]);
                    if let Json::Obj(f) = &mut d {
                        f.push(("revision".into(), Json::Int(revision)));
                    }
                    send(&mut phone, d);
                    if perms == 1 {
                        // mid-turn: Claude still has the edit to make
                        send(
                            &mut phone,
                            msg(vec![
                                ("type", "submit_prompt"),
                                ("commandId", "f1"),
                                ("sessionId", &sid),
                                (
                                    "text",
                                    "When you are done, reply with the single word: done",
                                ),
                            ]),
                        );
                    }
                }
                "command_progress" => progress.push((get("commandId"), get("state"))),
                "command_result" => {
                    let cmd = get("commandId");
                    results.push((cmd.clone(), get("status")));
                    if cmd == "i1" {
                        // the process outlives the interrupt: one more turn
                        send(
                            &mut phone,
                            msg(vec![
                                ("type", "submit_prompt"),
                                ("commandId", "p2"),
                                ("sessionId", &sid),
                                ("text", "Reply with the single word: ok"),
                            ]),
                        );
                    }
                    if cmd == "p2" {
                        send(
                            &mut phone,
                            msg(vec![
                                ("type", "session_end"),
                                ("commandId", "e1"),
                                ("sessionId", &sid),
                            ]),
                        );
                    }
                }
                _ => {}
            }
        }
        let file = std::fs::read_to_string(work.join("a.txt")).unwrap_or_default();
        let f1: Vec<&str> = progress
            .iter()
            .filter(|(c, _)| c == "f1")
            .map(|(_, s)| s.as_str())
            .collect();
        println!(
            "live: snapshot={snapshot} appends={appends} permissions={perms} edit_diff={edit_diff} \
             f1_progress={f1:?} results={results:?} turns={} cost_usd={:.4} file={:?}",
            costs.len(),
            costs.iter().sum::<f64>(),
            file.trim()
        );
        server.stop();
        comp.shutdown(true);
        assert!(snapshot, "subscribe got a snapshot");
        assert!(appends > 0, "streaming deltas");
        assert!(
            perms >= 1 && acks.iter().any(|(c, ok)| c == "d1" && *ok),
            "{acks:?}"
        );
        assert!(edit_diff, "the Edit card carries its diff");
        assert!(!f1.is_empty(), "the follow-up joined the turn");
        assert!(result(&results, "f1").is_some());
        assert_eq!(result(&results, "i1").as_deref(), Some("completed"));
        assert_eq!(
            result(&results, "p2").as_deref(),
            Some("completed"),
            "alive after the interrupt"
        );
        assert_eq!(result(&results, "e1").as_deref(), Some("completed"));
        assert!(ended, "state ended");
        assert_eq!(file.trim(), "hello");
        let _ = std::fs::remove_dir_all(&root);
    }
}
