//! ADR-015 §4.3 step 7: the Codex app-server as a [`LiveRuntime`]. One
//! `codex app-server` child per adapter, spawned on the first
//! `create_session` and again on the first command after it exited; each
//! phone session is a codex thread inside it. A prompt to a session whose
//! child exited respawns it and `thread/resume`s the thread (§10).
//!
//! [`Codex`] (the protocol core) turns codex output into [`Event`]s; this
//! file routes them: transcript ops to the hub, approvals and withdrawals to
//! the engine (G2), command progress and results to the engine's emit path
//! (G1), sign-in URLs to the desktop opener. One lock (`Inner::st`) holds
//! the core and the bookkeeping, and events are dispatched under it, so the
//! hub sees ops in codex order. Lock order: `st` → hub / engine, never back:
//! neither the hub nor the engine calls a runtime.

use crate::adapters::{AdapterError, Capability, LiveRuntime};
use crate::claude_runtime::Timing;
use crate::codex::{self, Codex, Event, LaunchConfig};
use crate::protocol::{Agent, ClientMsg, EventKind, PermissionRequest, ServerMsg, Session, now_ms};
use crate::runtime_io::{ChildEvent, ChildIo, child_env};
use crate::transcript::{HubHandle, Op};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Where engine-side output goes: `Engine::ingest_permission` for
/// `PermissionRequested` (it sets the runtime TTL), `publish_external` for
/// the rest (a `PermissionResolved` becomes `resolve_native`).
pub type Sink = Arc<dyn Fn(EventKind) + Send + Sync>;
/// Opens a runtime-produced sign-in URL on the desktop
/// (`runtime_io::open_url_on_desktop` in production).
pub type Opener = Arc<dyn Fn(&str) -> Result<(), &'static str> + Send + Sync>;
/// Starts a codex child in a folder (`ChildIo::spawn` in production).
type Spawn = Box<dyn Fn(&Path) -> io::Result<(ChildIo, Receiver<ChildEvent>)> + Send + Sync>;

/// `create_session` waits this long for `thread/start` before refusing.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// T1 bounds (ADR-015 §6), the Claude runtime's defaults.
const CREATE_EVERY_MS: i64 = 5_000;
const MAX_OPEN_SESSIONS: usize = 8;

/// Whether a new session may start: the rate limit, then the open-session cap.
fn admit(
    last_create: Option<i64>,
    now: i64,
    every_ms: i64,
    open: usize,
) -> Result<(), &'static str> {
    if last_create.is_some_and(|t| now - t < every_ms) {
        return Err("wait a few seconds before starting another session");
    }
    if open >= MAX_OPEN_SESSIONS {
        return Err("too many open Codex sessions: end one first");
    }
    Ok(())
}

pub struct CodexRuntime {
    inner: Arc<Inner>,
}

struct Inner {
    id: String,
    cfg: LaunchConfig,
    hub: HubHandle,
    sink: Sink,
    opener: Opener,
    spawn: Spawn,
    /// Production health = the binary resolves; injected spawners are healthy.
    probe_binary: bool,
    /// T1 create rate limit, the §4.6 interrupt and sign-in timers, and the
    /// reader's timer tick (the Claude runtime's struct; tests shrink it).
    timing: Timing,
    healthy: AtomicBool,
    st: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The running app-server; `None` before the first session and after it exited.
    codex: Option<Codex>,
    /// Bumped per spawn, so a reader never feeds a newer child's core.
    generation: u64,
    sessions: Vec<Session>,
    /// `create_session` commands until codex names their thread.
    creating: HashMap<String, Creating>,
    /// Command id → its session, for G1 events.
    tags: HashMap<String, String>,
    /// Commands whose ack refused them: a late outcome is not published.
    dropped: HashSet<String>,
    /// Permissions ingested and not resolved yet, for a withdrawal's record.
    perms: HashMap<String, PermissionRequest>,
    /// When the last `create_session` was accepted (rate limit, T1).
    last_create: Option<i64>,
}

struct Creating {
    /// `send` waits on it; `None` once it gave up.
    reply: Option<Sender<Result<String, String>>>,
    title: String,
    path: String,
}

impl CodexRuntime {
    /// The production runtime: spawns `cfg.binary` (no shell, scrubbed env).
    pub fn new(id: &str, cfg: LaunchConfig, hub: HubHandle, sink: Sink, opener: Opener) -> Self {
        let (binary, argv) = (cfg.binary.clone(), cfg.argv());
        let spawn: Spawn = Box::new(move |cwd| {
            let env = child_env(std::env::vars_os(), &[]);
            ChildIo::spawn(&binary, &argv, cwd, &env)
        });
        let timing = Timing {
            create_every_ms: CREATE_EVERY_MS,
            ..Timing::default()
        };
        Self::build(id, cfg, hub, sink, opener, spawn, true, timing)
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        id: &str,
        cfg: LaunchConfig,
        hub: HubHandle,
        sink: Sink,
        opener: Opener,
        spawn: Spawn,
        probe_binary: bool,
        timing: Timing,
    ) -> Self {
        let inner = Arc::new(Inner {
            id: id.into(),
            cfg,
            hub,
            sink,
            opener,
            spawn,
            probe_binary,
            timing,
            healthy: AtomicBool::new(false),
            st: Mutex::new(State::default()),
        });
        let rt = CodexRuntime { inner };
        let _ = rt.refresh();
        rt
    }
}

impl LiveRuntime for CodexRuntime {
    fn id(&self) -> &str {
        &self.inner.id
    }

    fn is_healthy(&self) -> bool {
        self.inner.healthy.load(Ordering::SeqCst)
    }

    fn agents(&self) -> Vec<Agent> {
        let i = &self.inner;
        vec![Agent {
            id: i.id.clone(),
            host_id: i.id.clone(),
            kind: "codex".into(),
            name: "Codex".into(),
            state: "online".into(),
            capabilities: codex::capabilities().into_iter().map(|c| c.id).collect(),
            accepts_new_session: true,
            simulated: false,
            live: Some(i.cfg.agent_live()),
        }]
    }

    fn sessions(&self) -> Vec<Session> {
        self.inner.st.lock().unwrap().sessions.clone()
    }

    /// Sessions outlive their process: a follow-up to one whose codex exited
    /// stays here (it resumes the thread), it is not handed to the engine.
    fn owns_session(&self, session_id: &str) -> bool {
        let st = self.inner.st.lock().unwrap();
        st.sessions.iter().any(|s| s.id == session_id)
    }

    fn knows_permission(&self, permission_id: &str) -> bool {
        let st = self.inner.st.lock().unwrap();
        st.codex
            .as_ref()
            .is_some_and(|c| c.knows_permission(permission_id))
    }

    fn send(&self, msg: &ClientMsg) -> Result<ServerMsg, AdapterError> {
        self.inner.send(msg)
    }

    fn refresh(&self) -> Result<(), AdapterError> {
        let i = &self.inner;
        let running = i.st.lock().unwrap().codex.is_some();
        let res = if running || !i.probe_binary {
            Ok(())
        } else {
            probe(&i.cfg.binary)
        };
        i.healthy.store(res.is_ok(), Ordering::SeqCst);
        res
    }

    fn capabilities(&self) -> Vec<Capability> {
        codex::capabilities()
    }

    fn dispose(&self) {
        // dropping the core closes codex's stdin and kills it after a grace
        if let Some(c) = self.inner.st.lock().unwrap().codex.take() {
            c.shutdown();
        }
    }
}

impl Inner {
    fn send(self: &Arc<Self>, msg: &ClientMsg) -> Result<ServerMsg, AdapterError> {
        match msg {
            // ADR-018: forking is Claude's
            ClientMsg::SubmitPrompt { fork: true, .. } => Err(AdapterError::Unsupported(
                "Codex sessions cannot be forked from the phone".into(),
            )),
            // ADR-017: the composer extras are Claude's
            ClientMsg::CreateSession { images, .. } | ClientMsg::SubmitPrompt { images, .. }
                if !images.is_empty() =>
            {
                Err(AdapterError::Unsupported(
                    "Codex does not take images".into(),
                ))
            }
            ClientMsg::CreateSession { model: Some(_), .. } => Err(AdapterError::Unsupported(
                "Codex sessions use the model Codex is configured with".into(),
            )),
            ClientMsg::CreateSession {
                command_id,
                title,
                prompt,
                project,
                ..
            } => self.create(command_id, title, prompt, project.as_deref()),
            ClientMsg::SubmitPrompt {
                command_id,
                session_id,
                text,
                ..
            } => self.prompt(command_id, session_id, text),
            ClientMsg::Interrupt {
                command_id,
                session_id,
            } => self.command(command_id, session_id, |c| {
                c.interrupt(session_id, command_id)
            }),
            ClientMsg::McpAuth {
                command_id,
                session_id,
                server,
                flow,
            } => {
                if flow != "desktop" {
                    return Err(AdapterError::Unsupported(
                        "Codex signs MCP servers in on the desktop only".into(),
                    ));
                }
                self.command(command_id, session_id, |c| {
                    c.mcp_login(server, command_id).map(|()| Vec::new())
                })
            }
            ClientMsg::McpReconnect {
                command_id,
                session_id,
                server,
            } => self.command(command_id, session_id, |c| {
                c.mcp_reload(server, command_id).map(|()| Vec::new())
            }),
            ClientMsg::PermissionDecision {
                command_id,
                permission_id,
                decision,
                option_id,
                answers,
                ..
            } => {
                let mut st = self.st.lock().unwrap();
                let Some(codex) = st.codex.as_mut() else {
                    return Ok(ServerMsg::ack_err(
                        command_id,
                        "request is no longer pending",
                    ));
                };
                match codex.decide(permission_id, decision, option_id.as_deref(), answers) {
                    Ok(events) => {
                        let session = st.perms.remove(permission_id).map(|p| p.session_id);
                        self.dispatch(&mut st, events);
                        Ok(ServerMsg::Ack {
                            command_id: command_id.clone(),
                            ok: true,
                            error: None,
                            duplicate: false,
                            session_id: session,
                            permission_id: Some(permission_id.clone()),
                        })
                    }
                    Err(e) => Ok(ServerMsg::ack_err(command_id, e)),
                }
            }
            // session_end, set_permission_mode, the phone sign-in callback
            _ => Err(AdapterError::Unsupported(
                "Codex does not support this command".into(),
            )),
        }
    }

    /// `create_session`: spawn codex if needed, `thread/start` in the chosen
    /// workspace, and ack once codex names the thread (the session id).
    fn create(
        self: &Arc<Self>,
        command_id: &str,
        title: &str,
        prompt: &str,
        project: Option<&str>,
    ) -> Result<ServerMsg, AdapterError> {
        let Some(ws) = self
            .cfg
            .workspaces
            .iter()
            .find(|w| Some(w.id.as_str()) == project)
        else {
            return Ok(ServerMsg::ack_err(
                command_id,
                "project must be one of this agent's workspaces",
            ));
        };
        let (reply, started) = mpsc::channel();
        {
            let mut st = self.st.lock().unwrap();
            // T1: a stolen phone cannot fan out agents — a session cap and a
            // create rate limit, like the Claude runtime
            let now = now_ms();
            let open = st
                .sessions
                .iter()
                .filter(|s| !matches!(s.status.as_str(), "completed" | "failed" | "interrupted"))
                .count()
                + st.creating.len();
            if let Err(e) = admit(st.last_create, now, self.timing.create_every_ms, open) {
                return Ok(ServerMsg::ack_err(command_id, e));
            }
            st.last_create = Some(now);
            self.running(&mut st, Path::new(&ws.path))?
                .start_session(&ws.path, prompt, command_id)
                .map_err(|e| AdapterError::RuntimeUnavailable(e.into()))?;
            let creating = Creating {
                reply: Some(reply),
                title: title.into(),
                path: ws.path.clone(),
            };
            st.creating.insert(command_id.into(), creating);
        }
        let res = match started.recv_timeout(START_TIMEOUT) {
            Ok(res) => res,
            Err(_) => {
                let mut st = self.st.lock().unwrap();
                match st.creating.get_mut(command_id) {
                    // too late: the session may still appear, its outcome is not published
                    Some(c) => {
                        c.reply = None;
                        st.dropped.insert(command_id.into());
                        Err("Codex did not start the session in time".to_string())
                    }
                    // answered while the timeout fired
                    None => started
                        .try_recv()
                        .unwrap_or_else(|_| Err("Codex exited".into())),
                }
            }
        };
        Ok(match res {
            Ok(thread) => ServerMsg::ack_ok(command_id, Some(&thread)),
            Err(e) => ServerMsg::ack_err(command_id, &e),
        })
    }

    /// `submit_prompt`: on the live thread, or, when the session's app-server
    /// exited, a respawn and `thread/resume` first (ADR-015 §10). A resume
    /// is not a create: no T1 rate limit or session-cap slot is taken.
    fn prompt(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
        text: &str,
    ) -> Result<ServerMsg, AdapterError> {
        {
            let mut st = self.st.lock().unwrap();
            let live = st
                .codex
                .as_ref()
                .is_some_and(|c| c.owns_session(session_id));
            if !live {
                let Some(cwd) = st
                    .sessions
                    .iter()
                    .find(|s| s.id == session_id && s.status != "completed")
                    .map(|s| s.project.clone())
                else {
                    return Ok(ServerMsg::ack_err(
                        command_id,
                        "this Codex session has ended; start a new session",
                    ));
                };
                let codex = self.running(&mut st, Path::new(&cwd))?;
                return match codex.resume_session(session_id, &cwd, text, command_id) {
                    Ok(events) => {
                        st.tags.insert(command_id.into(), session_id.into());
                        self.dispatch(&mut st, events);
                        Ok(ServerMsg::ack_ok(command_id, Some(session_id)))
                    }
                    Err(e) => Ok(ServerMsg::ack_err(command_id, e)),
                };
            }
        }
        self.command(command_id, session_id, |c| {
            c.prompt(session_id, text, command_id)
        })
    }

    /// A session command on a live thread: acked when written (G1).
    fn command(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
        f: impl FnOnce(&mut Codex) -> Result<Vec<Event>, &'static str>,
    ) -> Result<ServerMsg, AdapterError> {
        let mut st = self.st.lock().unwrap();
        let Some(codex) = st.codex.as_mut().filter(|c| c.owns_session(session_id)) else {
            let ended = st
                .sessions
                .iter()
                .any(|s| s.id == session_id && s.status == "completed");
            let why = if ended {
                "this Codex session has ended; start a new session"
            } else {
                "Codex is not running for this session; send a prompt to resume it"
            };
            return Ok(ServerMsg::ack_err(command_id, why));
        };
        match f(codex) {
            Ok(events) => {
                st.tags.insert(command_id.into(), session_id.into());
                self.dispatch(&mut st, events);
                Ok(ServerMsg::ack_ok(command_id, Some(session_id)))
            }
            Err(e) => Ok(ServerMsg::ack_err(command_id, e)),
        }
    }

    /// The running core, spawning codex (and its reader) when there is none.
    fn running<'a>(
        self: &Arc<Self>,
        st: &'a mut State,
        cwd: &Path,
    ) -> Result<&'a mut Codex, AdapterError> {
        if st.codex.is_none() {
            let (io, rx) = (self.spawn)(cwd).map_err(|e| {
                AdapterError::RuntimeUnavailable(format!(
                    "could not start {}: {e}",
                    self.cfg.binary.display()
                ))
            })?;
            st.generation += 1;
            let generation = st.generation;
            let me = Arc::clone(self);
            let mut core = Codex::new(io, &self.id, env!("CARGO_PKG_VERSION"));
            core.interrupt_ms = self.timing.interrupt_ms;
            core.auth_ttl_ms = self.timing.auth_ttl_ms;
            thread::Builder::new()
                .name("codex-events".into())
                .spawn(move || me.read(generation, rx))
                .map_err(|e| AdapterError::RuntimeUnavailable(format!("codex reader: {e}")))?;
            st.codex = Some(core);
            self.healthy.store(true, Ordering::SeqCst);
        }
        Ok(st.codex.as_mut().expect("spawned above"))
    }

    /// The child's output, in order, until it exits; the §4.6 timers run
    /// every `timing.tick` in between.
    fn read(self: Arc<Self>, generation: u64, rx: Receiver<ChildEvent>) {
        loop {
            let event = match rx.recv_timeout(self.timing.tick) {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            let exit = matches!(event, Some(ChildEvent::Exit(_)));
            let mut st = self.st.lock().unwrap();
            if st.generation != generation {
                return;
            }
            let Some(core) = st.codex.as_mut() else {
                return; // disposed
            };
            let Some(event) = event else {
                let events = core.tick(now_ms());
                self.dispatch(&mut st, events);
                continue;
            };
            let before = core.malformed();
            let events = core.on_event(event);
            for _ in before..core.malformed() {
                self.hub.malformed();
            }
            if exit {
                eprintln!("[codex] {} exited: {}", self.id, core.stderr_tail().trim());
                // the next create_session or prompt respawns it
                st.codex = None;
            }
            self.dispatch(&mut st, events);
            if exit {
                return;
            }
        }
    }

    fn dispatch(&self, st: &mut State, events: Vec<Event>) {
        for event in events {
            match event {
                Event::Op(thread, op) => self.op(st, &thread, op),
                Event::Permission(pr) => {
                    st.perms.insert(pr.id.clone(), (*pr).clone());
                    (self.sink)(EventKind::PermissionRequested(*pr));
                }
                Event::Withdrawn(id) => {
                    if let Some(mut pr) = st.perms.remove(&id) {
                        pr.status = "stale".into();
                        (self.sink)(EventKind::PermissionResolved(pr));
                    }
                }
                Event::SessionStarted { tag, thread } => {
                    let Some(c) = st.creating.remove(&tag) else {
                        continue;
                    };
                    let now = now_ms();
                    let session = Session {
                        id: thread.clone(),
                        agent_id: self.id.clone(),
                        host_id: self.id.clone(),
                        title: c.title,
                        project: c.path,
                        status: "running".into(),
                        progress: 0,
                        created_at: now,
                        last_activity_at: now,
                        prompt: None,
                        timeline: vec![],
                        output: vec![],
                        files: vec![],
                        transcript: true,
                        live_pc: false,
                        forked_from: None,
                    };
                    st.sessions.push(session.clone());
                    st.tags.insert(tag, thread.clone());
                    (self.sink)(EventKind::SessionCreated(session));
                    if let Some(reply) = c.reply {
                        let _ = reply.send(Ok(thread));
                    }
                }
                Event::Progress { tag, status } => {
                    if !st.dropped.contains(&tag) {
                        (self.sink)(EventKind::CommandProgress {
                            session_id: st.tags.get(&tag).cloned(),
                            command_id: tag,
                            state: status.into(),
                        });
                    }
                }
                Event::Done { tag, status, error } => {
                    // failed before codex named the thread: the ack says so
                    if let Some(c) = st.creating.remove(&tag)
                        && let Some(reply) = c.reply
                    {
                        let why = error.unwrap_or_else(|| "Codex refused the session".into());
                        let _ = reply.send(Err(why));
                        continue;
                    }
                    if st.dropped.remove(&tag) {
                        continue;
                    }
                    (self.sink)(EventKind::CommandResult {
                        session_id: st.tags.remove(&tag),
                        command_id: tag,
                        status: status.into(),
                        error,
                        permission_id: None,
                    });
                }
                Event::OpenUrl { tag, url } => {
                    // only a URL codex produced ever reaches the opener (T5)
                    if let Err(e) = (self.opener)(&url) {
                        st.dropped.insert(tag.clone());
                        (self.sink)(EventKind::CommandResult {
                            session_id: st.tags.remove(&tag),
                            command_id: tag,
                            status: "failed".into(),
                            error: Some(format!("could not open the sign-in page: {e}")),
                            permission_id: None,
                        });
                    }
                }
            }
        }
    }

    /// One transcript op; state changes also move `Session.status` (journaled).
    fn op(&self, st: &mut State, thread: &str, op: Op) {
        let state = match op {
            Op::State(state) => state,
            Op::Append { id, field, text } => return self.hub.append(thread, &id, field, &text),
            other => return self.hub.push(thread, other),
        };
        let ended = state.status == "ended";
        let status = match state.status {
            "starting" | "running" => "running",
            "waiting" => "waiting",
            "ended" => "completed",
            _ => "idle",
        };
        self.hub.push(thread, Op::State(state));
        if ended {
            self.hub.end_session(thread);
        }
        if let Some(s) = st.sessions.iter_mut().find(|s| s.id == thread)
            && s.status != status
        {
            s.status = status.into();
            s.last_activity_at = now_ms();
            (self.sink)(EventKind::SessionUpdated(s.clone()));
        }
    }
}

/// Health without spawning: the binary exists (a path) or is on PATH.
/// npm's `codex.cmd` shim cannot be spawned without a shell: `binary` must
/// name the real `codex.exe`.
fn probe(binary: &Path) -> Result<(), AdapterError> {
    let name = binary.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".cmd") || name.ends_with(".bat") {
        return Err(AdapterError::RuntimeUnavailable(format!(
            "{} is a cmd shim; set \"binary\" to the real codex.exe",
            binary.display()
        )));
    }
    let found = if binary.components().count() > 1 {
        binary.is_file()
    } else {
        let mut names = vec![binary.to_path_buf()];
        if binary.extension().is_none() {
            names.insert(0, binary.with_extension("exe"));
        }
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| names.iter().any(|n| dir.join(n).is_file()))
        })
    };
    if found {
        Ok(())
    } else {
        Err(AdapterError::RuntimeUnavailable(format!(
            "Codex binary {} not found (set \"binary\" in companion.json)",
            binary.display()
        )))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::LiveWorkspace;
    use std::io::{BufRead, BufReader, Write};
    use std::path::PathBuf;
    use std::time::Instant;

    #[test]
    fn session_cap_and_create_rate_limit_refuse() {
        assert!(admit(None, 10_000, CREATE_EVERY_MS, 0).is_ok());
        assert!(admit(Some(8_000), 10_000, CREATE_EVERY_MS, 0).is_err());
        assert!(admit(Some(5_000), 10_000, CREATE_EVERY_MS, 0).is_ok());
        assert!(admit(None, 10_000, CREATE_EVERY_MS, MAX_OPEN_SESSIONS - 1).is_ok());
        assert!(admit(None, 10_000, CREATE_EVERY_MS, MAX_OPEN_SESSIONS).is_err());
    }

    pub(crate) const WAIT: Duration = Duration::from_secs(10);

    pub(crate) fn launch(ws: &str) -> LaunchConfig {
        LaunchConfig {
            binary: PathBuf::from("codex.exe"),
            args: Vec::new(),
            workspaces: vec![LiveWorkspace {
                id: "ws0123456789".into(),
                label: "work".into(),
                path: ws.into(),
            }],
        }
    }

    /// One scripted codex per spawn, over `std::io::pipe()` ends.
    pub(crate) struct Peer {
        frames: Receiver<String>,
        to_core: io::PipeWriter,
    }

    impl Peer {
        pub(crate) fn recv(&self) -> String {
            self.frames
                .recv_timeout(WAIT)
                .expect("the runtime wrote a frame")
        }

        pub(crate) fn line(&mut self, line: &str) {
            writeln!(self.to_core, "{line}").unwrap();
        }

        pub(crate) fn note(&mut self, method: &str, params: &str) {
            self.line(&format!(r#"{{"method":"{method}","params":{params}}}"#));
        }
    }

    /// A runtime whose spawns are pipe peers, handed to the test on `peers`.
    pub(crate) fn piped(
        cfg: LaunchConfig,
        hub: HubHandle,
        sink: Sink,
        opener: Opener,
    ) -> (CodexRuntime, Receiver<Peer>) {
        piped_with(cfg, hub, sink, opener, fast())
    }

    /// Test timing: no create rate limit, a 10 ms timer tick.
    pub(crate) fn fast() -> Timing {
        Timing {
            create_every_ms: 0,
            tick: Duration::from_millis(10),
            ..Timing::default()
        }
    }

    pub(crate) fn piped_with(
        cfg: LaunchConfig,
        hub: HubHandle,
        sink: Sink,
        opener: Opener,
        timing: Timing,
    ) -> (CodexRuntime, Receiver<Peer>) {
        let (tx, peers) = mpsc::channel();
        let tx = Mutex::new(tx);
        let spawn: Spawn = Box::new(move |_cwd| {
            let (core_stdout, to_core) = io::pipe()?;
            let (from_core, core_stdin) = io::pipe()?;
            let (frames_tx, frames) = mpsc::channel();
            thread::spawn(move || {
                for line in BufReader::new(from_core).lines().map_while(Result::ok) {
                    if frames_tx.send(line).is_err() {
                        return;
                    }
                }
            });
            let _ = tx.lock().unwrap().send(Peer { frames, to_core });
            ChildIo::from_pipes(core_stdout, core_stdin)
        });
        let rt = CodexRuntime::build("codex-1", cfg, hub, sink, opener, spawn, false, timing);
        (rt, peers)
    }

    /// Engine-side output, recorded.
    pub(crate) fn recorder() -> (Sink, Arc<Mutex<Vec<EventKind>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        (Arc::new(move |k| log.lock().unwrap().push(k)), seen)
    }

    pub(crate) fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn no_opener() -> Opener {
        Arc::new(|_| panic!("nothing opens a URL here"))
    }

    pub(crate) fn create(cid: &str) -> ClientMsg {
        ClientMsg::CreateSession {
            command_id: cid.into(),
            agent_id: "codex-1".into(),
            title: "Fix tests".into(),
            prompt: "hello".into(),
            project: Some("ws0123456789".into()),
            images: vec![],
            model: None,
        }
    }

    fn results(seen: &Mutex<Vec<EventKind>>) -> Vec<(String, String)> {
        seen.lock()
            .unwrap()
            .iter()
            .filter_map(|k| match k {
                EventKind::CommandResult {
                    command_id, status, ..
                } => Some((command_id.clone(), status.clone())),
                _ => None,
            })
            .collect()
    }

    fn requested(seen: &Mutex<Vec<EventKind>>) -> Vec<PermissionRequest> {
        seen.lock()
            .unwrap()
            .iter()
            .filter_map(|k| match k {
                EventKind::PermissionRequested(p) => Some(p.clone()),
                _ => None,
            })
            .collect()
    }

    fn stale(seen: &Mutex<Vec<EventKind>>) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .filter_map(|k| match k {
                EventKind::PermissionResolved(p) if p.status == "stale" => Some(p.id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Handshake, `thread/start` → thr_1, `turn/start` → turn_1, while
    /// `create` (a create_session through the runtime or the router) waits
    /// on another thread for its session.
    pub(crate) fn started(
        peers: &Receiver<Peer>,
        create: impl FnOnce() -> Option<ServerMsg> + Send,
    ) -> Peer {
        thread::scope(|scope| {
            let ack = scope.spawn(create);
            let peer = handshake(peers);
            let reply = ack.join().unwrap().expect("an ack");
            assert!(
                matches!(&reply, ServerMsg::Ack { ok: true, session_id: Some(s), .. } if s == "thr_1"),
                "{reply:?}"
            );
            first_turn(peer)
        })
    }

    /// The next spawn, past `initialize` / `initialized`.
    fn initialized(peers: &Receiver<Peer>) -> Peer {
        let mut peer = peers.recv_timeout(WAIT).expect("a spawn");
        assert!(peer.recv().contains(r#""method":"initialize""#));
        peer.line(r#"{"id":1,"result":{"userAgent":"fake"}}"#);
        assert_eq!(peer.recv(), r#"{"method":"initialized"}"#);
        peer
    }

    fn rpc_id(frame: &str) -> crate::json::Json {
        crate::json::get(&crate::json::parse(frame).unwrap(), "id")
            .cloned()
            .unwrap()
    }

    /// Answers the request `frame` with `result`.
    fn reply(peer: &mut Peer, frame: &str, result: &str) {
        let id = crate::json::stringify(&rpc_id(frame));
        peer.line(&format!(r#"{{"id":{id},"result":{result}}}"#));
    }

    fn handshake(peers: &Receiver<Peer>) -> Peer {
        let mut peer = initialized(peers);
        let start = peer.recv();
        assert!(start.contains(r#""method":"thread/start""#), "{start}");
        let id = crate::json::get(&crate::json::parse(&start).unwrap(), "id")
            .cloned()
            .unwrap();
        peer.line(&format!(
            r#"{{"id":{},"result":{{"thread":{{"id":"thr_1"}},"model":"gpt-test"}}}}"#,
            crate::json::stringify(&id)
        ));
        peer
    }

    fn first_turn(mut peer: Peer) -> Peer {
        let turn = peer.recv();
        assert!(turn.contains(r#""method":"turn/start""#), "{turn}");
        let id = crate::json::get(&crate::json::parse(&turn).unwrap(), "id")
            .cloned()
            .unwrap();
        peer.line(&format!(
            r#"{{"id":{},"result":{{"turn":{{"id":"turn_1","status":"inProgress"}}}}}}"#,
            crate::json::stringify(&id)
        ));
        peer.note(
            "turn/started",
            r#"{"threadId":"thr_1","turn":{"id":"turn_1","status":"inProgress"}}"#,
        );
        peer
    }

    pub(crate) const ASK: &str = r#"{"id":0,"method":"item/commandExecution/requestApproval","params":{"threadId":"thr_1","turnId":"turn_1","itemId":"call_0","command":"npm test","cwd":"C:\\work","reason":"Run tests","proposedExecpolicyAmendment":["npm","test"]}}"#;

    #[test]
    fn create_streams_transcript_ops_and_closes_the_command() {
        let hub = HubHandle::default();
        let (sink, seen) = recorder();
        let (rt, peers) = piped(launch(r"C:\work"), hub.clone(), sink, no_opener());
        assert!(rt.is_healthy());
        let agent = &rt.agents()[0];
        assert_eq!(
            (agent.id.as_str(), agent.host_id.as_str()),
            ("codex-1", "codex-1")
        );
        assert_eq!(agent.live.as_ref().unwrap().prompt_while_busy, "merge");
        // only a configured workspace can be spawned in
        let mut bad = create("c0");
        if let ClientMsg::CreateSession { project, .. } = &mut bad {
            *project = Some(r"C:\Windows".into());
        }
        assert!(matches!(
            rt.send(&bad),
            Ok(ServerMsg::Ack { ok: false, .. })
        ));

        let mut peer = started(&peers, || rt.send(&create("c1")).ok());
        peer.note(
            "item/started",
            r#"{"threadId":"thr_1","turnId":"turn_1","item":{"type":"agentMessage","id":"msg_1","text":""}}"#,
        );
        peer.note(
            "item/agentMessage/delta",
            r#"{"threadId":"thr_1","turnId":"turn_1","itemId":"msg_1","delta":"Hi there"}"#,
        );
        peer.note(
            "turn/completed",
            r#"{"threadId":"thr_1","turn":{"id":"turn_1","status":"completed","error":null}}"#,
        );
        wait_until("the create command result", || {
            results(&seen).contains(&("c1".into(), "completed".into()))
        });
        let ops = hub.ops("thr_1").join("\n");
        for want in [
            r#""kind":"user""#,
            r#""text":"hello""#,
            r#""id":"msg_1""#,
            r#""text":"Hi there""#,
            r#""status":"completed""#,
        ] {
            assert!(ops.contains(want), "{want} missing from {ops}");
        }
        let session = rt.sessions().pop().unwrap();
        assert_eq!(
            (
                session.id.as_str(),
                session.project.as_str(),
                session.transcript
            ),
            ("thr_1", r"C:\work", true)
        );
        let seen = seen.lock().unwrap();
        assert!(matches!(&seen[0], EventKind::SessionCreated(s) if s.id == "thr_1"));
        assert!(seen.iter().any(|k| matches!(k,
            EventKind::CommandProgress { command_id, state, session_id: Some(s) }
                if command_id == "c1" && state == "running" && s == "thr_1")));
    }

    #[test]
    fn approvals_decisions_withdrawals_and_exit() {
        let hub = HubHandle::default();
        let (sink, seen) = recorder();
        let (rt, peers) = piped(launch(r"C:\work"), hub.clone(), sink, no_opener());
        let mut peer = started(&peers, || rt.send(&create("c1")).ok());

        // an approval: an engine permission with codex's options, then its card
        peer.line(ASK);
        wait_until("the permission", || !requested(&seen).is_empty());
        let pr = requested(&seen).remove(0);
        assert_eq!(pr.id, "codex-1:0");
        assert_eq!(pr.tool_item_id.as_deref(), Some("call_0"));
        let ids: Vec<&str> = pr.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "accept",
                "acceptForSession",
                "acceptWithExecpolicyAmendment",
                "decline",
                "cancel"
            ]
        );
        assert!(rt.knows_permission("codex-1:0"));
        let decide = |cid: &str, pid: &str, option: &str| ClientMsg::PermissionDecision {
            command_id: cid.into(),
            permission_id: pid.into(),
            decision: "approve-session".into(),
            revision: 1.0,
            option_id: Some(option.into()),
            message: None,
            answers: vec![],
        };
        let ack = rt
            .send(&decide("d1", "codex-1:0", "acceptWithExecpolicyAmendment"))
            .unwrap();
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }), "{ack:?}");
        assert_eq!(
            peer.recv(),
            r#"{"id":0,"result":{"decision":{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["npm","test"]}}}}"#
        );
        assert!(!rt.knows_permission("codex-1:0"));
        // answered once: a second decision is refused, codex's resolved note is a no-op
        let again = rt.send(&decide("d2", "codex-1:0", "accept")).unwrap();
        assert!(
            matches!(again, ServerMsg::Ack { ok: false, .. }),
            "{again:?}"
        );
        peer.note(
            "serverRequest/resolved",
            r#"{"threadId":"thr_1","requestId":0}"#,
        );

        // answered elsewhere: the engine record goes stale
        peer.line(
            &ASK.replace(r#""id":0"#, r#""id":1"#)
                .replace("call_0", "call_1"),
        );
        wait_until("the second permission", || requested(&seen).len() == 2);
        peer.note(
            "serverRequest/resolved",
            r#"{"threadId":"thr_1","requestId":1}"#,
        );
        wait_until("the withdrawal", || stale(&seen) == ["codex-1:1"]);

        // a follow-up steers the running turn
        let prompt = ClientMsg::SubmitPrompt {
            command_id: "p1".into(),
            session_id: "thr_1".into(),
            text: "also lint".into(),
            images: vec![],
            fork: false,
        };
        assert!(matches!(
            rt.send(&prompt),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        assert!(peer.recv().contains(r#""method":"turn/steer""#));

        // exit: the open approval goes stale, commands end, the session is over
        peer.line(
            &ASK.replace(r#""id":0"#, r#""id":2"#)
                .replace("call_0", "call_2"),
        );
        wait_until("the third permission", || requested(&seen).len() == 3);
        let interrupt = ClientMsg::Interrupt {
            command_id: "i1".into(),
            session_id: "thr_1".into(),
        };
        assert!(matches!(
            rt.send(&interrupt),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        assert!(peer.recv().contains(r#""method":"turn/interrupt""#));
        drop(peer);
        wait_until("the exit", || stale(&seen).len() == 2);
        wait_until("the command results", || results(&seen).len() >= 3);
        let mut got = results(&seen);
        got.sort();
        let want = [("c1", "failed"), ("i1", "unknown"), ("p1", "failed")];
        assert_eq!(got, want.map(|(a, b)| (a.to_string(), b.to_string())));
        // codex keeps the thread: the session idles instead of ending
        let ops = hub.ops("thr_1").join("\n");
        assert!(!ops.contains(r#""status":"ended""#), "{ops}");
        assert!(
            ops.contains("the next prompt resumes this session"),
            "{ops}"
        );
        assert_eq!(rt.sessions()[0].status, "idle");

        // a prompt respawns codex and resumes the thread before its turn
        let again = |cid: &str| ClientMsg::SubmitPrompt {
            command_id: cid.into(),
            session_id: "thr_1".into(),
            text: "go on".into(),
            images: vec![],
            fork: false,
        };
        let ack = rt.send(&again("p2")).unwrap();
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }), "{ack:?}");
        let mut peer = initialized(&peers);
        let resume = peer.recv();
        assert!(
            resume.contains(
                r#""method":"thread/resume","params":{"threadId":"thr_1","cwd":"C:\\work""#
            ),
            "{resume}"
        );
        // one resume at a time
        let busy = rt.send(&again("p3")).unwrap();
        assert!(matches!(busy, ServerMsg::Ack { ok: false, .. }), "{busy:?}");
        reply(
            &mut peer,
            &resume,
            r#"{"thread":{"id":"thr_1"},"model":"gpt-test"}"#,
        );
        let turn = peer.recv();
        assert!(turn.contains(r#""method":"turn/start""#), "{turn}");
        reply(
            &mut peer,
            &turn,
            r#"{"turn":{"id":"turn_2","status":"inProgress"}}"#,
        );
        peer.note(
            "turn/completed",
            r#"{"threadId":"thr_1","turn":{"id":"turn_2","status":"completed","error":null}}"#,
        );
        wait_until("the resumed prompt", || {
            results(&seen).contains(&("p2".into(), "completed".into()))
        });
        assert_eq!(rt.sessions().len(), 1, "a resume is not a create");

        // a thread codex no longer has: the prompt fails, the session ends
        drop(peer);
        wait_until("the second exit", || {
            hub.ops("thr_1")
                .iter()
                .filter(|o| o.contains("the next prompt resumes"))
                .count()
                == 2
        });
        assert!(matches!(
            rt.send(&again("p4")),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        let mut peer = initialized(&peers);
        let resume = peer.recv();
        peer.line(&format!(
            r#"{{"id":{},"error":{{"code":-32600,"message":"no rollout found for thread id thr_1"}}}}"#,
            crate::json::stringify(&rpc_id(&resume))
        ));
        wait_until("the failed resume", || {
            results(&seen).contains(&("p4".into(), "failed".into()))
        });
        assert_eq!(rt.sessions()[0].status, "completed");
        let ops = hub.ops("thr_1").join("\n");
        assert!(ops.contains("could not resume this session"), "{ops}");
        let late = rt.send(&again("p5")).unwrap();
        assert!(
            matches!(&late, ServerMsg::Ack { ok: false, error: Some(e), .. } if e.contains("ended")),
            "{late:?}"
        );
        drop(peer);
        // the next session respawns codex
        let mut peer = started(&peers, || rt.send(&create("c2")).ok());
        peer.note(
            "turn/completed",
            r#"{"threadId":"thr_1","turn":{"id":"turn_1","status":"completed","error":null}}"#,
        );
        wait_until("the respawned session's result", || {
            results(&seen).contains(&("c2".into(), "completed".into()))
        });
    }

    #[test]
    fn interrupt_without_a_turn_end_resolves_unknown() {
        let (sink, seen) = recorder();
        let timing = Timing {
            interrupt_ms: 50,
            ..fast()
        };
        let (rt, peers) = piped_with(
            launch(r"C:\work"),
            HubHandle::default(),
            sink,
            no_opener(),
            timing,
        );
        let mut peer = started(&peers, || rt.send(&create("c1")).ok());
        wait_until("the running turn", || {
            seen.lock().unwrap().iter().any(|k| {
                matches!(k, EventKind::CommandProgress { command_id, state, .. }
                    if command_id == "c1" && state == "running")
            })
        });
        let interrupt = ClientMsg::Interrupt {
            command_id: "i1".into(),
            session_id: "thr_1".into(),
        };
        assert!(matches!(
            rt.send(&interrupt),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        let frame = peer.recv();
        assert!(frame.contains(r#""method":"turn/interrupt""#), "{frame}");
        wait_until("the unknown interrupt", || {
            results(&seen).contains(&("i1".into(), "unknown".into()))
        });
        // codex stops the turn after all: the prompt ends, i1 is not closed twice
        reply(&mut peer, &frame, "{}");
        peer.note(
            "turn/completed",
            r#"{"threadId":"thr_1","turn":{"id":"turn_1","status":"interrupted","error":null}}"#,
        );
        wait_until("the cancelled prompt", || {
            results(&seen).contains(&("c1".into(), "cancelled".into()))
        });
        let i1 = results(&seen).iter().filter(|(c, _)| c == "i1").count();
        assert_eq!(i1, 1);
    }

    /// `node` + the committed fake app-server as a codex entry (the real
    /// spawn path), or `None` when node is not installed.
    pub(crate) fn fake_codex(ws: &Path, extra: &[&str]) -> Option<LaunchConfig> {
        if probe(Path::new("node")).is_err() {
            eprintln!("skipped: node is not on PATH");
            return None;
        }
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tools")
            .join("fake-codex-app-server.mjs");
        let mut cfg = launch(&ws.to_string_lossy());
        cfg.binary = PathBuf::from("node");
        cfg.args = vec![script.to_string_lossy().into_owned()];
        cfg.args.extend(extra.iter().map(|a| a.to_string()));
        Some(cfg)
    }

    #[test]
    fn mcp_desktop_flow_opens_only_the_runtime_url() {
        let Some(cfg) = fake_codex(&std::env::temp_dir(), &["--oauth-delay-ms", "50"]) else {
            return;
        };
        let hub = HubHandle::default();
        let (sink, seen) = recorder();
        let urls = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = urls.clone();
        let opener: Opener = Arc::new(move |url| {
            log.lock().unwrap().push(url.into());
            Ok(())
        });
        let rt = CodexRuntime::new("codex-1", cfg, hub.clone(), sink, opener);
        assert!(rt.is_healthy(), "node resolves on PATH");
        let thread = match rt.send(&create("c1")).unwrap() {
            ServerMsg::Ack {
                ok: true,
                session_id: Some(s),
                ..
            } => s,
            other => panic!("{other:?}"),
        };
        let done = |cid: &str, status: &str| results(&seen).contains(&(cid.into(), status.into()));
        wait_until("the first turn", || done("c1", "completed"));
        let auth = |cid: &str, server: &str, flow: &str| ClientMsg::McpAuth {
            command_id: cid.into(),
            session_id: thread.clone(),
            server: server.into(),
            flow: flow.into(),
        };
        // codex has no phone-callback flow; the phone never supplies a URL
        assert!(matches!(
            rt.send(&auth("m0", "fake-linear", "phone-callback")),
            Err(AdapterError::Unsupported(_))
        ));
        assert!(matches!(
            rt.send(&auth("m1", "fake-linear", "desktop")),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        wait_until("the sign-in", || done("m1", "completed"));
        let opened = urls.lock().unwrap().clone();
        assert_eq!(opened.len(), 1, "{opened:?}");
        assert!(
            opened[0].starts_with("https://auth.fake-linear.example/oauth/authorize?"),
            "{opened:?}"
        );
        wait_until("fake-linear connected", || {
            let ops = hub.ops(&thread);
            let last = ops.iter().rev().find(|o| o.contains(r#""op":"mcp""#));
            last.is_some_and(|o| o.contains(r#"{"name":"fake-linear","status":"connected""#))
        });
        // a stdio server has no sign-in: codex refuses, nothing opens
        assert!(matches!(
            rt.send(&auth("m2", "fake-docs", "desktop")),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        wait_until("the refused sign-in", || done("m2", "failed"));
        assert_eq!(urls.lock().unwrap().len(), 1);
        // reconnect = codex's config reload, done once the server is back
        let reconnect = ClientMsg::McpReconnect {
            command_id: "r1".into(),
            session_id: thread.clone(),
            server: "fake-linear".into(),
        };
        assert!(matches!(
            rt.send(&reconnect),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        wait_until("the reconnect", || done("r1", "completed"));
        rt.dispose();
    }

    /// The real spawn path: the app-server goes away, the next prompt
    /// respawns it and resumes the thread it saved.
    #[test]
    fn fake_app_server_resumes_a_thread_after_exit() {
        let state =
            std::env::temp_dir().join(format!("chernobyl-codex-state-{}", std::process::id()));
        let state_arg = state.to_string_lossy().into_owned();
        let Some(cfg) = fake_codex(&std::env::temp_dir(), &["--state-dir", &state_arg]) else {
            return;
        };
        let (sink, seen) = recorder();
        let rt = CodexRuntime::new("codex-1", cfg, HubHandle::default(), sink, no_opener());
        let thread = match rt.send(&create("c1")).unwrap() {
            ServerMsg::Ack {
                ok: true,
                session_id: Some(s),
                ..
            } => s,
            other => panic!("{other:?}"),
        };
        let done = |cid: &str, status: &str| results(&seen).contains(&(cid.into(), status.into()));
        wait_until("the first turn", || done("c1", "completed"));
        rt.dispose(); // as after a crash or an idle shutdown
        let prompt = ClientMsg::SubmitPrompt {
            command_id: "p1".into(),
            session_id: thread.clone(),
            text: "hello again".into(),
            images: vec![],
            fork: false,
        };
        let ack = rt.send(&prompt).unwrap();
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }), "{ack:?}");
        wait_until("the resumed turn", || done("p1", "completed"));
        assert_eq!(rt.sessions().len(), 1);
        assert_eq!(rt.sessions()[0].id, thread);
        rt.dispose();
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn probe_refuses_cmd_shims_and_missing_binaries() {
        let err = probe(Path::new(r"C:\npm\codex.cmd")).unwrap_err();
        assert!(err.to_string().contains("real codex.exe"), "{err}");
        assert!(probe(Path::new("surely-not-a-codex-binary-here")).is_err());
        assert!(probe(Path::new(r"C:\nope\codex.exe")).is_err());
        let me = std::env::current_exe().unwrap();
        assert!(probe(&me).is_ok());
        let rt = CodexRuntime::new(
            "codex-1",
            launch(r"C:\work"),
            HubHandle::default(),
            recorder().0,
            no_opener(),
        );
        // `codex.exe` is not installed here (ADR §7: fake processes only)
        if probe(Path::new("codex.exe")).is_err() {
            assert!(!rt.is_healthy());
        }
    }
}
