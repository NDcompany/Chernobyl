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
//!
//! Frames carry tool input and OAuth codes: nothing here logs them.

use crate::adapters::{AdapterError, Capability, LiveRuntime};
use crate::claude_live::{
    self, Callback, Decision, Driver, Event, PERMISSION_MODES, PromptState, new_uuid, spawn_args,
    spawn_env,
};
use crate::codex_runtime::Sink;
use crate::config::AdapterEntryConfig;
use crate::protocol::{
    Agent, AgentLive, ClientMsg, DecisionError, EventKind, FileChange, LiveWorkspace,
    PermissionRequest, ServerMsg, Session, now_ms,
};
use crate::runtime_io::{
    AUTH_TTL_MS, ChildEvent, ChildIo, KILL_GRACE, PendingAuth, check_callback_url,
    open_url_on_desktop,
};
use crate::transcript::{HubHandle, McpAuthState, McpServer, Op, ToolStatus};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Ended sessions kept for routing ("this session has ended") and the list.
const ENDED_KEPT: usize = 32;
/// Every this many desktop sign-in polls, an `mcp_reconnect` too: research
/// run 4 needed one after the callback (unverified for the CLI's own
/// listener, §4.7).
const RECONNECT_EVERY: u64 = 5;

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
}

#[derive(Default)]
struct State {
    sessions: Vec<Live>,
    last_create: Option<i64>,
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
            }),
        }
    }

    /// Where engine-bound events go (permissions are ingested, the rest
    /// published); the companion sets it before routing anything here.
    pub fn set_sink(&self, sink: Sink) {
        *self.inner.sink.lock().unwrap() = Some(sink);
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
            }),
        }]
    }

    fn sessions(&self) -> Vec<Session> {
        let st = self.inner.state.lock().unwrap();
        st.sessions.iter().map(|l| l.session.clone()).collect()
    }

    fn owns_session(&self, session_id: &str) -> bool {
        let st = self.inner.state.lock().unwrap();
        st.sessions.iter().any(|l| l.session.id == session_id)
    }

    fn knows_permission(&self, permission_id: &str) -> bool {
        let st = self.inner.state.lock().unwrap();
        st.sessions
            .iter()
            .any(|l| l.driver.knows_permission(permission_id))
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

    /// The first subscriber: MCP status at the next tick.
    fn on_subscribe(&self, session_id: &str) {
        let mut st = self.inner.state.lock().unwrap();
        if let Some(live) = st.sessions.iter_mut().find(|l| l.session.id == session_id) {
            live.last_mcp_poll = 0;
        }
    }

    fn refresh(&self) -> Result<(), AdapterError> {
        let result = (self.inner.probe)();
        self.inner.healthy.store(result.is_ok(), Ordering::SeqCst);
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
                "unavailable",
                Some("only the live transcript of companion-started sessions is streamed"),
            ),
            Capability::new(
                "session.list",
                "limited",
                Some(
                    "sessions started from the phone; desktop sessions arrive through the \
                     hook ingress",
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
            ..
        } = msg
        {
            return self.create(st, command_id, title, prompt, project.as_deref(), now, out);
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
            } => (command_id, session_id),
            _ => {
                return Err(AdapterError::Unsupported(
                    "session-neutral messages stay with the engine".into(),
                ));
            }
        };
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
                    ClientMsg::SubmitPrompt { text, .. } => live
                        .driver
                        .user_prompt(command_id, text, now, |f| tx(&live.io, f))
                        .map(|op| self.hub.push(session_id, op)),
                    ClientMsg::SetPermissionMode { mode, .. } => {
                        live.driver
                            .set_permission_mode(command_id, mode, |f| tx(&live.io, f))
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

    #[allow(clippy::too_many_arguments)]
    fn create(
        self: &Arc<Self>,
        st: &mut State,
        command_id: &str,
        title: &str,
        prompt: &str,
        project: Option<&str>,
        now: i64,
        out: &mut Vec<EventKind>,
    ) -> Result<ServerMsg, AdapterError> {
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
        let mut live = Live {
            session: Session {
                id: id.clone(),
                agent_id: self.id.clone(),
                host_id: self.id.clone(),
                title,
                project: ws.path.clone(),
                status: "running".into(),
                progress: 0,
                created_at: now,
                last_activity_at: now,
                prompt: (!prompt.is_empty()).then(|| prompt.to_string()),
                timeline: Vec::new(),
                output: Vec::new(),
                files: Vec::new(),
                transcript: true,
            },
            driver: Driver::new(&self.id, &id, &ws.path),
            io: None,
            generation: 0,
            ended: false,
            reaping: false,
            last_active: now,
            last_mcp_poll: now,
            auth_polls: 0,
            create: Some((command_id.to_string(), prompt.trim().is_empty())),
            interrupts: Vec::new(),
            end: None,
            progress: Vec::new(),
            mcp: Vec::new(),
            waits: Vec::new(),
            callbacks: PendingAuth::default(),
            asked: Vec::new(),
        };
        let mode = self.cfg.permission_mode.clone();
        if let Err(e) = self.start(&mut live, false, mode.as_deref(), now) {
            return refuse(command_id, e);
        }
        st.last_create = Some(now);
        if !prompt.trim().is_empty() {
            match live
                .driver
                .user_prompt(command_id, prompt, now, |f| tx(&live.io, f))
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

    fn full(&self) -> String {
        format!(
            "at most {} Claude Code sessions can run at once; end one first",
            self.cfg.max_sessions
        )
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
        self.start(live, true, mode.as_deref(), now)
    }

    /// Spawn → `state starting` → `initialize`; the reader owns the events.
    fn start(
        self: &Arc<Self>,
        live: &mut Live,
        resume: bool,
        mode: Option<&str>,
        now: i64,
    ) -> Result<(), String> {
        let sid = live.session.id.clone();
        let mut args = spawn_args(&sid, resume, mode)?;
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
            "set_permission_mode" => {
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
        let peers = Arc::new(Mutex::new(VecDeque::from(peers)));
        let spawns = Arc::new(Mutex::new(Vec::new()));
        let stdin = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(Mutex::new(Vec::new()));
        let (log, read) = (spawns.clone(), stdin.clone());
        let spawn: Box<Spawn> = Box::new(move |args: &[String], _cwd: &Path| {
            log.lock().unwrap().push(args.to_vec());
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
        let cfg = LiveConfig {
            workspaces: vec![LiveWorkspace {
                id: WS.into(),
                label: "work".into(),
                path: "<tmp>\\work".into(),
            }],
            binary: PathBuf::from("claude.exe"),
            max_sessions,
            permission_mode: Some("default".into()),
            extra_args: Vec::new(),
        };
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
        }
    }

    fn prompt(command_id: &str, sid: &str, text: &str) -> ClientMsg {
        ClientMsg::SubmitPrompt {
            command_id: command_id.into(),
            session_id: sid.into(),
            text: text.into(),
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

    /* ---------------------------- live ---------------------------- */

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
