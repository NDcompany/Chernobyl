//! ADR-015 §4.3 step 7: the Codex app-server as a [`LiveRuntime`]. One
//! `codex app-server` child per adapter, spawned on the first command (or the
//! first session-list probe) and again on the first command after it exited;
//! each phone session is a codex thread inside it. A prompt to a session
//! whose child exited respawns it and `thread/resume`s the thread (§10).
//!
//! Codex keeps every thread on disk, the Codex desktop app's included, so
//! the probe lists the ones in the allow-listed workspaces (`thread/list`)
//! as idle sessions; the first subscriber replays one (`thread/items/list`),
//! a prompt resumes it, and one the Codex app is writing (updated in the
//! last 2 minutes) is "Live on PC": it is re-read at the probe cadence and
//! continues on the phone only as a fork (ADR-018, `thread/fork`).
//!
//! [`Codex`] (the protocol core) turns codex output into [`Event`]s; this
//! file routes them: transcript ops to the hub, approvals and withdrawals to
//! the engine (G2), command progress and results to the engine's emit path
//! (G1), sign-in URLs to the desktop opener. One lock (`Inner::st`) holds
//! the core and the bookkeeping, and events are dispatched under it, so the
//! hub sees ops in codex order. Lock order: `st` → hub / engine, never back:
//! neither the hub nor the engine calls a runtime.

use crate::adapters::{AdapterError, Capability, LiveRuntime};
use crate::claude_history::IN_USE_MS;
use crate::claude_runtime::Timing;
use crate::codex::{self, Codex, Event, FoundThread, LaunchConfig, Prompt};
use crate::protocol::{
    Agent, AgentLive, ClientMsg, EventKind, PermissionRequest, PromptImage, ServerMsg, Session,
    now_ms,
};
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
    /// Production: health = the binary resolves, and the probe lists codex's
    /// threads (which starts the app-server). Injected spawners are healthy
    /// and list only when a test asks.
    production: bool,
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
    /// Threads on disk in the workspaces that no phone session drives.
    found: Vec<Found>,
    /// The first `thread/list` came back: later changes are published.
    scanned: bool,
    /// `create_session` and fork commands until codex names their thread.
    creating: HashMap<String, Creating>,
    /// Command id → its session, for G1 events.
    tags: HashMap<String, String>,
    /// Commands whose ack refused them: a late outcome is not published.
    dropped: HashSet<String>,
    /// Permissions ingested and not resolved yet, for a withdrawal's record.
    perms: HashMap<String, PermissionRequest>,
    /// ADR-017 `set_model` per session, kept across a respawn.
    models: HashMap<String, String>,
    /// When the last `create_session` was accepted (rate limit, T1).
    last_create: Option<i64>,
}

struct Found {
    thread: FoundThread,
    /// ADR-018: written in the last [`IN_USE_MS`] (by the Codex app).
    live: bool,
    /// `updated_at` of the last history replay.
    hydrated: Option<i64>,
}

struct Creating {
    /// `send` waits on it; `None` once it gave up.
    reply: Option<Sender<Result<String, String>>>,
    title: String,
    path: String,
    /// ADR-018: the title of the thread this one forks.
    forked_from: Option<String>,
}

impl CodexRuntime {
    /// The production runtime: spawns codex (no shell, scrubbed env),
    /// resolving the binary at every spawn ([`LaunchConfig::resolve_binary`]).
    pub fn new(id: &str, cfg: LaunchConfig, hub: HubHandle, sink: Sink, opener: Opener) -> Self {
        let launch = cfg.clone();
        let spawn: Spawn = Box::new(move |cwd| {
            let binary = launch.resolve_binary().map_err(io::Error::other)?;
            let env = child_env(std::env::vars_os(), &[]);
            ChildIo::spawn(&binary, &launch.argv(), cwd, &env)
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
        production: bool,
        timing: Timing,
    ) -> Self {
        let inner = Arc::new(Inner {
            id: id.into(),
            cfg,
            hub,
            sink,
            opener,
            spawn,
            production,
            timing,
            healthy: AtomicBool::new(false),
            st: Mutex::new(State::default()),
        });
        let rt = CodexRuntime { inner };
        let healthy = !production || rt.inner.health().is_ok();
        rt.inner.healthy.store(healthy, Ordering::SeqCst);
        rt
    }

    /// The session-list probe (`thread/list`), spawning codex if needed.
    fn discover(&self) -> Result<(), AdapterError> {
        let i = &self.inner;
        let mut st = i.st.lock().unwrap();
        let first = Path::new(&i.cfg.workspaces[0].path);
        let cwds: Vec<String> = i.cfg.workspaces.iter().map(|w| w.path.clone()).collect();
        i.running(&mut st, first)?
            .list_threads(&cwds)
            .map_err(|e| AdapterError::RuntimeUnavailable(e.into()))
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
        let healthy = self.is_healthy();
        let busy = {
            let st = i.st.lock().unwrap();
            st.sessions
                .iter()
                .any(|s| matches!(s.status.as_str(), "running" | "waiting"))
        };
        let state = match (healthy, busy) {
            (false, _) => "offline",
            (true, true) => "busy",
            (true, false) => "online",
        };
        let live = AgentLive {
            // ADR-017: models from `model/list`, images as data URLs, the
            // context meter from token usage, rename = `thread/name/set`
            composer: vec!["model", "images", "context", "rename"],
            ..i.cfg.agent_live()
        };
        vec![Agent {
            id: i.id.clone(),
            host_id: i.id.clone(),
            kind: "codex".into(),
            name: "Codex".into(),
            state: state.into(),
            capabilities: codex::capabilities().into_iter().map(|c| c.id).collect(),
            accepts_new_session: healthy,
            simulated: false,
            live: Some(live),
        }]
    }

    /// Started or adopted here, then the threads found on disk (newest first).
    fn sessions(&self) -> Vec<Session> {
        let i = &self.inner;
        let st = i.st.lock().unwrap();
        let found = st.found.iter().map(|f| i.found_session(f));
        st.sessions.iter().cloned().chain(found).collect()
    }

    /// Sessions outlive their process: a follow-up to one whose codex exited
    /// stays here (it resumes the thread), it is not handed to the engine.
    fn owns_session(&self, session_id: &str) -> bool {
        let st = self.inner.st.lock().unwrap();
        st.sessions.iter().any(|s| s.id == session_id)
            || st.found.iter().any(|f| f.thread.id == session_id)
    }

    fn drives_session(&self, session_id: &str) -> bool {
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

    /// The first subscriber of a thread found on disk: its history.
    fn on_subscribe(&self, session_id: &str) {
        let i = &self.inner;
        let mut st = i.st.lock().unwrap();
        let Some(f) = st.found.iter().find(|f| f.thread.id == session_id) else {
            return;
        };
        if f.hydrated == Some(f.thread.updated_at) && i.hub.has_items(session_id) {
            return;
        }
        let cwd = f.thread.cwd.clone();
        i.hydrate(&mut st, session_id, &cwd);
    }

    /// Health (the binary resolves), then, in production, codex's threads.
    fn refresh(&self) -> Result<(), AdapterError> {
        let i = &self.inner;
        if !i.production {
            return Ok(());
        }
        let res = i.health();
        i.healthy.store(res.is_ok(), Ordering::SeqCst);
        res?;
        self.discover()
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
    /// Without spawning: codex runs, or its binary resolves.
    fn health(&self) -> Result<(), AdapterError> {
        if self.st.lock().unwrap().codex.is_some() {
            return Ok(());
        }
        let binary = self
            .cfg
            .resolve_binary()
            .map_err(AdapterError::RuntimeUnavailable)?;
        probe(&binary)
    }

    fn send(self: &Arc<Self>, msg: &ClientMsg) -> Result<ServerMsg, AdapterError> {
        match msg {
            ClientMsg::CreateSession { model: Some(_), .. } => Err(AdapterError::Unsupported(
                "Codex sessions start on the model Codex is configured with; switch it in the \
                 session"
                    .into(),
            )),
            ClientMsg::CreateSession {
                command_id,
                title,
                prompt,
                project,
                images,
                ..
            } => {
                let Some(ws) = self
                    .cfg
                    .workspaces
                    .iter()
                    .find(|w| Some(w.id.as_str()) == project.as_deref())
                else {
                    return Ok(ServerMsg::ack_err(
                        command_id,
                        "project must be one of this agent's workspaces",
                    ));
                };
                let prompt = Prompt {
                    text: prompt.clone(),
                    images: images.clone(),
                };
                self.start(command_id, title, &ws.path, None, |c| {
                    c.start_session(&ws.path, prompt, command_id)
                })
            }
            ClientMsg::SubmitPrompt {
                command_id,
                session_id,
                text,
                images,
                fork,
            } => {
                if *fork {
                    self.fork(command_id, session_id, text, images)
                } else {
                    self.prompt(command_id, session_id, text, images)
                }
            }
            ClientMsg::Interrupt {
                command_id,
                session_id,
            } => {
                if self.found(session_id).is_some() {
                    // nothing of ours runs in it: stopping it is a no-op
                    return Ok(self.noop(command_id, session_id));
                }
                self.command(command_id, session_id, |c| {
                    c.interrupt(session_id, command_id)
                })
            }
            ClientMsg::SetModel {
                command_id,
                session_id,
                model,
            } => {
                let done = Event::Done {
                    tag: command_id.clone(),
                    status: "completed",
                    error: None,
                };
                let ack = self.command(command_id, session_id, |c| {
                    c.set_model(session_id, model).map(|mut events| {
                        events.push(done);
                        events
                    })
                })?;
                if matches!(ack, ServerMsg::Ack { ok: true, .. }) {
                    let mut st = self.st.lock().unwrap();
                    st.models.insert(session_id.clone(), model.clone());
                }
                Ok(ack)
            }
            ClientMsg::RenameSession {
                command_id,
                session_id,
                title,
            } => {
                let mut st = self.st.lock().unwrap();
                let Some(cwd) = self.cwd_of(&st, session_id) else {
                    return Err(AdapterError::Unsupported("unknown session".into()));
                };
                // codex names threads on disk: a loaded one is not needed
                match self
                    .running(&mut st, Path::new(&cwd))?
                    .rename(session_id, title, command_id)
                {
                    Ok(()) => {
                        st.tags.insert(command_id.clone(), session_id.clone());
                        Ok(ServerMsg::ack_ok(command_id, Some(session_id)))
                    }
                    Err(e) => Ok(ServerMsg::ack_err(command_id, e)),
                }
            }
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

    /// A new session (`create_session` or a fork): T1 admission, spawn codex
    /// if needed, `begin` the request, and ack once codex names the thread
    /// (the session id).
    fn start(
        self: &Arc<Self>,
        command_id: &str,
        title: &str,
        path: &str,
        forked_from: Option<String>,
        begin: impl FnOnce(&mut Codex) -> Result<(), &'static str>,
    ) -> Result<ServerMsg, AdapterError> {
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
            begin(self.running(&mut st, Path::new(path))?)
                .map_err(|e| AdapterError::RuntimeUnavailable(e.into()))?;
            let creating = Creating {
                reply: Some(reply),
                title: title.into(),
                path: path.into(),
                forked_from,
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

    /// ADR-018: continue a thread found on disk as a NEW session
    /// (`thread/fork`); the ack names it. The original is never written.
    fn fork(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
        text: &str,
        images: &[PromptImage],
    ) -> Result<ServerMsg, AdapterError> {
        let source = {
            let st = self.st.lock().unwrap();
            if st.sessions.iter().any(|s| s.id == session_id) {
                return Ok(ServerMsg::ack_err(
                    command_id,
                    "this session runs here; send a normal prompt",
                ));
            }
            st.found
                .iter()
                .find(|f| f.thread.id == session_id)
                .map(|f| (f.thread.title.clone(), f.thread.cwd.clone()))
        };
        let Some((title, cwd)) = source else {
            return Err(AdapterError::Unsupported("unknown session".into()));
        };
        let prompt = Prompt {
            text: text.into(),
            images: images.to_vec(),
        };
        self.start(command_id, &title, &cwd, Some(title.clone()), |c| {
            c.fork(session_id, &cwd, prompt, command_id)
        })
    }

    /// `submit_prompt`: on the live thread, or, when the session's app-server
    /// exited (or it was found on disk), a respawn and `thread/resume` first
    /// (ADR-015 §10). A resume is not a create: no T1 rate limit or
    /// session-cap slot is taken.
    fn prompt(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
        text: &str,
        images: &[PromptImage],
    ) -> Result<ServerMsg, AdapterError> {
        let prompt = Prompt {
            text: text.into(),
            images: images.to_vec(),
        };
        {
            let mut st = self.st.lock().unwrap();
            let live = st
                .codex
                .as_ref()
                .is_some_and(|c| c.owns_session(session_id));
            if !live {
                if let Some(at) = st.found.iter().position(|f| f.thread.id == session_id) {
                    // T10: one writer per thread; the Codex app's is untouched
                    if st.found[at].live {
                        return Ok(ServerMsg::ack_err(
                            command_id,
                            "this Codex thread is running in the Codex app on the PC; continue \
                             it there, or fork it to continue on the phone",
                        ));
                    }
                    let f = st.found.remove(at);
                    st.sessions.push(self.found_session(&f));
                }
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
                let model = st.models.get(session_id).cloned();
                let codex = self.running(&mut st, Path::new(&cwd))?;
                return match codex.resume_session(
                    session_id,
                    &cwd,
                    model.as_deref(),
                    prompt,
                    command_id,
                ) {
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
            c.prompt(session_id, prompt, command_id)
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

    /// Done at once: the command has nothing to act on.
    fn noop(&self, command_id: &str, session_id: &str) -> ServerMsg {
        (self.sink)(EventKind::CommandResult {
            command_id: command_id.into(),
            status: "completed".into(),
            error: None,
            session_id: Some(session_id.into()),
            permission_id: None,
        });
        ServerMsg::ack_ok(command_id, Some(session_id))
    }

    fn found(&self, session_id: &str) -> Option<String> {
        let st = self.st.lock().unwrap();
        let f = st.found.iter().find(|f| f.thread.id == session_id)?;
        Some(f.thread.cwd.clone())
    }

    /// The folder of a session, driven or found.
    fn cwd_of(&self, st: &State, session_id: &str) -> Option<String> {
        let driven = st.sessions.iter().find(|s| s.id == session_id);
        driven.map(|s| s.project.clone()).or_else(|| {
            let f = st.found.iter().find(|f| f.thread.id == session_id)?;
            Some(f.thread.cwd.clone())
        })
    }

    /// Replays a found thread's history into the hub (`thread/items/list`).
    fn hydrate(self: &Arc<Self>, st: &mut State, session_id: &str, cwd: &str) {
        let Ok(codex) = self.running(st, Path::new(cwd)) else {
            return;
        };
        if codex.history(session_id, cwd).is_ok()
            && let Some(f) = st.found.iter_mut().find(|f| f.thread.id == session_id)
        {
            f.hydrated = Some(f.thread.updated_at);
        }
    }

    fn found_session(&self, f: &Found) -> Session {
        let t = &f.thread;
        Session {
            id: t.id.clone(),
            agent_id: self.id.clone(),
            host_id: self.id.clone(),
            title: t.title.clone(),
            project: t.cwd.clone(),
            status: "idle".into(),
            progress: 0,
            created_at: t.created_at,
            last_activity_at: t.updated_at,
            prompt: None,
            timeline: vec![],
            output: vec![],
            files: vec![],
            transcript: true,
            live_pc: f.live,
            forked_from: None,
        }
    }

    /// A `thread/list` result: the threads in the workspaces that no phone
    /// session drives. After the first list, new and changed ones are
    /// published; a watched one the Codex app wrote since its replay is
    /// replayed again (the probe-cadence mirror).
    fn threads(self: &Arc<Self>, st: &mut State, list: Vec<FoundThread>) {
        let now = now_ms();
        let mut found = Vec::new();
        let mut out = Vec::new();
        let mut stale = Vec::new();
        for mut t in list {
            // T11: the filter is codex's; the allow-list is checked here again,
            // on the canonical path (a `..` or a junction must not escape it),
            // and the canonical cwd is what the session keeps
            let Some(canon) = crate::claude_history::inside(&t.cwd, &self.cfg.workspaces) else {
                continue;
            };
            t.cwd = canon;
            if st.sessions.iter().any(|s| s.id == t.id) {
                continue;
            }
            let old = st.found.iter().find(|f| f.thread.id == t.id);
            let f = Found {
                live: now - t.updated_at < IN_USE_MS,
                hydrated: old.and_then(|o| o.hydrated),
                thread: t,
            };
            if st.scanned
                && old.is_none_or(|o| o.thread.title != f.thread.title || o.live != f.live)
            {
                out.push(EventKind::SessionUpdated(self.found_session(&f)));
            }
            if f.hydrated.is_some_and(|h| h != f.thread.updated_at)
                && self.hub.is_subscribed(&f.thread.id)
            {
                stale.push((f.thread.id.clone(), f.thread.cwd.clone()));
            }
            found.push(f);
        }
        st.found = found;
        st.scanned = true;
        for k in out {
            (self.sink)(k);
        }
        for (id, cwd) in stale {
            self.hydrate(st, &id, &cwd);
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
                self.healthy.store(false, Ordering::SeqCst);
                AdapterError::RuntimeUnavailable(format!("could not start Codex: {e}"))
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
                // the next command or probe respawns it
                st.codex = None;
            }
            self.dispatch(&mut st, events);
            if exit {
                return;
            }
        }
    }

    fn dispatch(self: &Arc<Self>, st: &mut State, events: Vec<Event>) {
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
                        forked_from: c.forked_from,
                    };
                    st.sessions.push(session.clone());
                    st.tags.insert(tag, thread.clone());
                    (self.sink)(EventKind::SessionCreated(session));
                    if let Some(reply) = c.reply {
                        let _ = reply.send(Ok(thread));
                    }
                }
                Event::Renamed { thread, title } => {
                    if let Some(s) = st.sessions.iter_mut().find(|s| s.id == thread) {
                        if s.title != title {
                            s.title = title;
                            (self.sink)(EventKind::SessionUpdated(s.clone()));
                        }
                    } else if let Some(f) = st.found.iter_mut().find(|f| f.thread.id == thread) {
                        f.thread.title = title;
                        let s = self.found_session(f);
                        (self.sink)(EventKind::SessionUpdated(s));
                    }
                }
                Event::Threads(list) => self.threads(st, list),
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
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    /// Security pass 2026-11-15 (E3): a listed thread counts only when its
    /// CANONICAL cwd is inside a workspace: a `..` path or a junction that
    /// leaves the folder is dropped (the old check was lexical), and the
    /// canonical cwd is what the session keeps.
    #[test]
    fn listed_threads_are_checked_on_the_canonical_cwd() {
        let root = std::env::temp_dir().join(format!(
            "chb-codex-canon-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let (ws, outside) = (root.join("ws"), root.join("outside"));
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // a junction inside the workspace pointing outside it
        let link = ws.join("link");
        let junction = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
        let ws_canon = std::fs::canonicalize(&ws).unwrap();
        let plain = |p: &Path| p.to_string_lossy().trim_start_matches(r"\\?\").to_string();
        let cfg = launch(&plain(&ws_canon));
        let (sink, _seen) = recorder();
        let rt = CodexRuntime::new("codex-1", cfg, HubHandle::default(), sink, no_opener());
        let thread = |id: &str, cwd: String| FoundThread {
            id: id.into(),
            title: id.into(),
            cwd,
            created_at: 1,
            updated_at: 1,
        };
        let mut list = vec![
            thread("inside", plain(&ws_canon.join("sub"))),
            thread(
                "dotdot",
                plain(&ws_canon.join("sub").join("..").join("..").join("outside")),
            ),
            thread("missing", plain(&ws_canon.join("gone"))),
        ];
        if junction {
            list.push(thread("junction", plain(&link)));
        }
        {
            let mut st = rt.inner.st.lock().unwrap();
            rt.inner.threads(&mut st, list);
            let ids: Vec<&str> = st.found.iter().map(|f| f.thread.id.as_str()).collect();
            assert_eq!(ids, ["inside"], "only the thread really inside is kept");
            assert_eq!(st.found[0].thread.cwd, plain(&ws_canon.join("sub")));
        }
        let _ = std::process::Command::new("cmd")
            .args(["/C", "rmdir"])
            .arg(&link)
            .output();
        let _ = std::fs::remove_dir_all(&root);
    }

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
            binary: Some(PathBuf::from("codex.exe")),
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
                // the core's own model/list (a background id) is answered by no one
                let lines = BufReader::new(from_core).lines().map_while(Result::ok);
                for line in lines.filter(|l| !l.contains(r#""method":"model/list""#)) {
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
        cfg.binary = Some(PathBuf::from("node"));
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

    /// ADR-015 §10 / ADR-018 for Codex over the real spawn path: threads on
    /// disk are listed, replayed on subscribe, resumed by a prompt (idle) or
    /// forked (written by the Codex app in the last 2 minutes), renamed, and
    /// switched to another listed model.
    #[test]
    fn fake_app_server_lists_replays_resumes_forks_and_renames() {
        let root = std::env::temp_dir().join(format!("chb-codex-found-{}", std::process::id()));
        let (ws, state) = (root.join("ws"), root.join("state"));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let ws_path = ws.to_string_lossy().into_owned();
        let saved = |id: &str, title: &str, updated: i64| {
            let entry = |kind: &str, text: &str| {
                let item = if kind == "user" {
                    crate::json::obj(vec![
                        ("type", crate::json::s("userMessage")),
                        ("id", crate::json::s(&format!("{id}-u"))),
                        ("clientId", crate::json::Json::Null),
                        (
                            "content",
                            crate::json::Json::Arr(vec![crate::json::obj(vec![
                                ("type", crate::json::s("text")),
                                ("text", crate::json::s(text)),
                            ])]),
                        ),
                    ])
                } else {
                    crate::json::obj(vec![
                        ("type", crate::json::s("agentMessage")),
                        ("id", crate::json::s(&format!("{id}-m"))),
                        ("text", crate::json::s(text)),
                    ])
                };
                crate::json::obj(vec![("turnId", crate::json::s("t1")), ("item", item)])
            };
            let body = crate::json::obj(vec![
                ("id", crate::json::s(id)),
                ("cwd", crate::json::s(&ws_path)),
                ("model", crate::json::s("gpt-6-luna")),
                ("name", crate::json::Json::Null),
                (
                    "items",
                    crate::json::Json::Arr(vec![
                        entry("user", title),
                        entry("agent", &format!("{title}: done")),
                    ]),
                ),
                ("createdAt", crate::json::Json::Int(updated - 60)),
                ("updatedAt", crate::json::Json::Int(updated)),
            ]);
            std::fs::write(
                state.join(format!("{id}.json")),
                crate::json::stringify(&body),
            )
            .unwrap();
        };
        saved("old-1", "Old work", 1_700_000_000);
        saved("live-1", "Live work", now_ms() / 1000);
        let state_arg = state.to_string_lossy().into_owned();
        let Some(cfg) = fake_codex(&ws, &["--state-dir", &state_arg]) else {
            return;
        };
        let hub = HubHandle::default();
        let (sink, seen) = recorder();
        let rt = CodexRuntime::new("codex-1", cfg, hub.clone(), sink, no_opener());
        assert_eq!(
            rt.agents()[0].live.as_ref().unwrap().composer,
            ["model", "images", "context", "rename"]
        );
        rt.refresh().unwrap();
        wait_until("the listed threads", || rt.sessions().len() == 2);
        let listed = |id: &str| rt.sessions().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(
            (listed("old-1").title.as_str(), listed("old-1").live_pc),
            ("Old work", false)
        );
        assert_eq!(
            (listed("live-1").title.as_str(), listed("live-1").live_pc),
            ("Live work", true)
        );
        assert!(rt.owns_session("old-1") && !rt.drives_session("old-1"));
        // the first subscriber gets the history
        rt.on_subscribe("old-1");
        wait_until("the replay", || {
            hub.ops("old-1").join("\n").contains("Old work: done")
        });
        let prompt = |cid: &str, sid: &str, fork: bool| ClientMsg::SubmitPrompt {
            command_id: cid.into(),
            session_id: sid.into(),
            text: "go on".into(),
            images: vec![],
            fork,
        };
        let done = |cid: &str| results(&seen).contains(&(cid.into(), "completed".into()));
        // T10: one the Codex app writes is not resumed, it is forked
        let refused = rt.send(&prompt("p0", "live-1", false)).unwrap();
        assert!(
            matches!(&refused, ServerMsg::Ack { ok: false, error: Some(e), .. } if e.contains("Codex app")),
            "{refused:?}"
        );
        let forked = match rt.send(&prompt("f1", "live-1", true)).unwrap() {
            ServerMsg::Ack {
                ok: true,
                session_id: Some(s),
                ..
            } => s,
            other => panic!("{other:?}"),
        };
        assert_ne!(forked, "live-1");
        wait_until("the forked turn", || done("f1"));
        assert_eq!(listed(&forked).forked_from.as_deref(), Some("Live work"));
        let ops = hub.ops(&forked).join("\n");
        assert!(
            ops.contains("Live work: done") && ops.contains("Forked from"),
            "{ops}"
        );
        // an idle one is resumed by a prompt and driven from then on
        let ack = rt.send(&prompt("p1", "old-1", false)).unwrap();
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }), "{ack:?}");
        wait_until("the resumed turn", || done("p1"));
        assert!(rt.drives_session("old-1"));
        // rename and model switch
        let rename = ClientMsg::RenameSession {
            command_id: "r1".into(),
            session_id: "old-1".into(),
            title: "Renamed".into(),
        };
        assert!(matches!(
            rt.send(&rename),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        wait_until("the rename", || {
            done("r1") && listed("old-1").title == "Renamed"
        });
        let model = |cid: &str, m: &str| ClientMsg::SetModel {
            command_id: cid.into(),
            session_id: "old-1".into(),
            model: m.into(),
        };
        assert!(matches!(
            rt.send(&model("m1", "gpt-5.6-luna")),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        assert!(matches!(
            rt.send(&model("m2", "gpt-9")),
            Ok(ServerMsg::Ack { ok: false, .. })
        ));
        let ops = hub.ops("old-1").join("\n");
        assert!(
            ops.contains(r#""model":"gpt-5.6-luna""#) && ops.contains("GPT-5.6-Terra"),
            "{ops}"
        );
        rt.dispose();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A temp git workspace for the live runs (`CHERNOBYL_LIVE_CODEX_DIR` is
    /// its parent), and its codex adapter entry with no `binary`: the real
    /// codex is found the way production finds it.
    fn live_workspace(tag: &str) -> Option<(PathBuf, LaunchConfig)> {
        std::env::var_os("CHERNOBYL_LIVE_CODEX")?;
        let parent = std::env::var_os("CHERNOBYL_LIVE_CODEX_DIR")
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let root = parent.join(format!("chb-live-codex-{tag}-{}", now_ms()));
        let (work, data) = (root.join("work"), root.join("data"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(work.join("README.md"), "# live\n").unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "."],
            &[
                "-c",
                "user.email=live@example.invalid",
                "-c",
                "user.name=live",
                "commit",
                "-qm",
                "init",
            ],
        ] {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(&work)
                .status();
            assert!(ok.is_ok_and(|s| s.success()), "git {args:?}");
        }
        let entry = crate::json::obj(vec![
            ("type", crate::json::s("codex")),
            (
                "workspaces",
                crate::json::Json::Arr(vec![crate::json::s(&work.to_string_lossy())]),
            ),
        ]);
        let cfg = codex::launch_config(&entry, &data).unwrap();
        assert_eq!(cfg.binary, None);
        eprintln!("live: codex = {}", cfg.resolve_binary().unwrap().display());
        Some((PathBuf::from(&cfg.workspaces[0].path), cfg))
    }

    /// The text of an ops log's agent messages (items and appends).
    fn said(ops: &[String]) -> String {
        ops.iter()
            .filter(|o| o.contains(r#""kind":"text""#) || o.contains(r#""op":"append""#))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Approves every request as it comes (option `accept`), like a phone.
    fn approver(rt: Arc<CodexRuntime>, seen: Arc<Mutex<Vec<EventKind>>>) -> Arc<AtomicBool> {
        let stop = Arc::new(AtomicBool::new(false));
        let halt = stop.clone();
        thread::spawn(move || {
            let mut done = HashSet::new();
            while !halt.load(Ordering::SeqCst) {
                for p in requested(&seen) {
                    if done.insert(p.id.clone()) {
                        let ids: Vec<&str> = p.options.iter().map(|o| o.id.as_str()).collect();
                        eprintln!(
                            "live: approval {:?} {:?} options {ids:?}",
                            p.title, p.command
                        );
                        let reply = rt.send(&ClientMsg::PermissionDecision {
                            command_id: format!("d-{}", p.id),
                            permission_id: p.id.clone(),
                            decision: "approve-once".into(),
                            revision: 1.0,
                            option_id: Some("accept".into()),
                            message: None,
                            answers: vec![],
                        });
                        assert!(
                            matches!(reply, Ok(ServerMsg::Ack { ok: true, .. })),
                            "{reply:?}"
                        );
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        stop
    }

    /// LIVE (ignored; 6 short turns on the owner's ChatGPT plan, the default
    /// model): `CHERNOBYL_LIVE_CODEX=1 cargo test --lib live_codex_runtime --
    /// --ignored --nocapture`. The real `codex app-server` (found, not
    /// configured) in a temp git workspace: stream → approvals (file change
    /// and command) → model switch, context → image → interrupt → resume
    /// after the server exits → rename → the thread listed by a second
    /// runtime, replayed, and forked. Prints counts and replies only.
    #[test]
    #[ignore = "spawns the real Codex app-server (ChatGPT plan usage)"]
    fn live_codex_runtime_round_trip() {
        let Some((work, cfg)) = live_workspace("rt") else {
            return;
        };
        let hub = HubHandle::default();
        let (sink, seen) = recorder();
        let rt = Arc::new(CodexRuntime::new(
            "codex-1",
            cfg.clone(),
            hub.clone(),
            sink,
            no_opener(),
        ));
        assert!(rt.is_healthy());
        rt.refresh().unwrap();
        let stop = approver(rt.clone(), seen.clone());
        let wait = |what: &str, cid: &str| {
            let deadline = Instant::now() + Duration::from_secs(240);
            loop {
                if let Some((_, s)) = results(&seen).into_iter().find(|(c, _)| c == cid) {
                    eprintln!("live: {what}: {cid} {s}");
                    return s;
                }
                assert!(Instant::now() < deadline, "timed out: {what}");
                thread::sleep(Duration::from_millis(100));
            }
        };
        let prompt =
            |cid: &str, sid: &str, text: &str, images: Vec<PromptImage>| ClientMsg::SubmitPrompt {
                command_id: cid.into(),
                session_id: sid.into(),
                text: text.into(),
                images,
                fork: false,
            };
        // 1: create, streamed
        let mut create = create("c1");
        if let ClientMsg::CreateSession {
            project, prompt, ..
        } = &mut create
        {
            *project = Some(cfg.workspaces[0].id.clone());
            *prompt = "Reply with exactly: pong".into();
        }
        let sid = match rt.send(&create).unwrap() {
            ServerMsg::Ack {
                ok: true,
                session_id: Some(s),
                ..
            } => s,
            other => panic!("{other:?}"),
        };
        assert_eq!(wait("turn 1", "c1"), "completed");
        let ops = hub.ops(&sid);
        let appends = ops
            .iter()
            .filter(|o| o.contains(r#""op":"append""#))
            .count();
        eprintln!(
            "live: turn 1 appends={appends} said={:?}",
            said(&ops).contains("pong")
        );
        assert!(said(&ops).to_lowercase().contains("pong"));
        let state = ops
            .iter()
            .rev()
            .find(|o| o.contains(r#""op":"state""#))
            .unwrap()
            .clone();
        assert!(state.contains(r#""models":["#), "{state}");
        // 2: model switch, then a file change and a command, approved
        let model = ClientMsg::SetModel {
            command_id: "m1".into(),
            session_id: sid.clone(),
            model: "gpt-5.6-luna".into(),
        };
        assert!(matches!(
            rt.send(&model),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        let text = "Create a file named probe.txt containing the single word ok using your file \
                    edit tool. Then run the shell command: git add probe.txt";
        assert!(matches!(
            rt.send(&prompt("p2", &sid, text, vec![])),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        assert_eq!(wait("turn 2", "p2"), "completed");
        let asked = requested(&seen).len();
        eprintln!("live: turn 2 approvals={asked}");
        assert!(asked >= 2, "a file change and a command");
        assert_eq!(
            std::fs::read_to_string(work.join("probe.txt"))
                .unwrap()
                .trim(),
            "ok"
        );
        let status = std::process::Command::new("git")
            .args(["status", "--short"])
            .current_dir(&work)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&status.stdout).contains("A  probe.txt"));
        let ops = hub.ops(&sid);
        assert!(
            ops.iter().any(|o| o.contains(r#""path":"probe.txt""#)),
            "relative diff path"
        );
        let state = ops
            .iter()
            .rev()
            .find(|o| o.contains(r#""op":"state""#))
            .unwrap()
            .clone();
        assert!(
            state.contains(r#""model":"gpt-5.6-luna""#) && state.contains(r#""context":{"#),
            "{state}"
        );
        // 3: an image
        let red = PromptImage {
            media_type: "image/png".into(),
            data: "iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAIAAACQkWg2AAAAFklEQVR4nGO4IyJCEmIY1TCqYfhqAAACcQQQFwFJQgAAAABJRU5ErkJggg==".into(),
        };
        let ask = "What color is this image? Answer with one word.";
        assert!(matches!(
            rt.send(&prompt("p3", &sid, ask, vec![red])),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        assert_eq!(wait("turn 3", "p3"), "completed");
        let reply = said(&hub.ops(&sid)).to_lowercase();
        eprintln!(
            "live: turn 3 image reply mentions red: {}",
            reply.contains("red")
        );
        assert!(reply.contains("red"));
        // 4: interrupted mid-turn
        let long = "Write the numbers from 1 to 400 as words, one per line, no tools.";
        let before = hub.ops(&sid).len();
        assert!(matches!(
            rt.send(&prompt("p4", &sid, long, vec![])),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        wait_until_long("streaming", || hub.ops(&sid).len() > before + 3);
        let stop_turn = ClientMsg::Interrupt {
            command_id: "i4".into(),
            session_id: sid.clone(),
        };
        assert!(matches!(
            rt.send(&stop_turn),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        assert_eq!(wait("turn 4", "p4"), "cancelled");
        assert_eq!(wait("interrupt", "i4"), "completed");
        // 5: the server exits; the next prompt resumes the thread
        rt.dispose();
        let text = "Reply with exactly: resumed";
        assert!(matches!(
            rt.send(&prompt("p5", &sid, text, vec![])),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        assert_eq!(wait("turn 5 (resumed)", "p5"), "completed");
        assert!(said(&hub.ops(&sid)).to_lowercase().contains("resumed"));
        let rename = ClientMsg::RenameSession {
            command_id: "r1".into(),
            session_id: sid.clone(),
            title: "Live codex".into(),
        };
        assert!(matches!(
            rt.send(&rename),
            Ok(ServerMsg::Ack { ok: true, .. })
        ));
        assert_eq!(wait("rename", "r1"), "completed");
        stop.store(true, Ordering::SeqCst);
        rt.dispose();
        // a second companion: the thread is listed (named), replayed and forked
        let hub2 = HubHandle::default();
        let (sink2, seen2) = recorder();
        let rt2 = CodexRuntime::new("codex-1", cfg, hub2.clone(), sink2, no_opener());
        rt2.refresh().unwrap();
        wait_until_long("the listed thread", || {
            rt2.sessions().iter().any(|s| s.id == sid)
        });
        let found = rt2.sessions().into_iter().find(|s| s.id == sid).unwrap();
        eprintln!("live: listed {:?} livePc={}", found.title, found.live_pc);
        assert_eq!(found.title, "Live codex");
        rt2.on_subscribe(&sid);
        wait_until_long("the replay", || said(&hub2.ops(&sid)).contains("pong"));
        let fork = ClientMsg::SubmitPrompt {
            command_id: "f6".into(),
            session_id: sid.clone(),
            text: "Reply with exactly: forked".into(),
            images: vec![],
            fork: true,
        };
        let forked = match rt2.send(&fork).unwrap() {
            ServerMsg::Ack {
                ok: true,
                session_id: Some(s),
                ..
            } => s,
            other => panic!("{other:?}"),
        };
        let deadline = Instant::now() + Duration::from_secs(240);
        while !results(&seen2).contains(&("f6".into(), "completed".into())) {
            assert!(Instant::now() < deadline, "the forked turn");
            thread::sleep(Duration::from_millis(100));
        }
        let ops = hub2.ops(&forked);
        assert!(said(&ops).contains("pong") && said(&ops).to_lowercase().contains("forked"));
        eprintln!("live: forked into {forked}; 6 turns");
        rt2.dispose();
    }

    /// LIVE through the companion (ignored; 1 short turn): the real server
    /// in-process with a codex adapter from config, a WS client as the phone.
    /// create → subscribe → streamed text → a file-change approval decided
    /// by the phone → the item and the command complete.
    /// `CHERNOBYL_LIVE_CODEX=1 cargo test --lib live_codex_companion --
    /// --ignored --nocapture`.
    #[test]
    #[ignore = "spawns the real Codex app-server (ChatGPT plan usage)"]
    fn live_codex_companion_phone_round_trip() {
        use crate::client::WsClient;
        use crate::companion::Companion;
        use crate::config::{AdapterEntryConfig, default_config};
        use crate::json::{self, Json};
        let Some((work, cfg)) = live_workspace("phone") else {
            return;
        };
        let data = work.parent().unwrap().join("data");
        let mut c = default_config();
        c.data_dir = data;
        c.adapters.push(AdapterEntryConfig {
            id: "codex-1".into(),
            kind: "codex".into(),
            enabled: true,
            codex: Some(cfg.clone()),
            ..Default::default()
        });
        let comp = Arc::new(Companion::boot(c).unwrap());
        let server = crate::server::start("127.0.0.1", 0, 0, None, comp.clone()).unwrap();
        let mut phone =
            WsClient::connect(&format!("127.0.0.1:{}", server.port), "/", None).unwrap();
        phone.hello().unwrap();
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
                ("agentId", "codex-1"),
                ("title", "live codex"),
                (
                    "prompt",
                    "Create a file named hello.txt containing the single word hi using your file edit tool, then reply with: done",
                ),
                ("project", &cfg.workspaces[0].id),
            ]),
        );
        let (mut appends, mut perms, mut tool_done) = (0, 0, false);
        let mut result = None;
        let deadline = Instant::now() + Duration::from_secs(240);
        while Instant::now() < deadline && result.is_none() {
            let Some(text) = phone.next_text_timeoutable().unwrap() else {
                continue;
            };
            let v = json::parse(&text).unwrap();
            let get = |k: &str| json::get_str(&v, k).unwrap_or("").to_string();
            match get("type").as_str() {
                "ack" if get("commandId") == "c1" => {
                    assert_eq!(json::get_bool(&v, "ok"), Some(true), "{text}");
                    let sid = get("sessionId");
                    send(
                        &mut phone,
                        msg(vec![
                            ("type", "session_subscribe"),
                            ("commandId", "s1"),
                            ("sessionId", &sid),
                        ]),
                    );
                }
                "transcript" => {
                    for op in json::get(&v, "ops").and_then(json::as_arr).unwrap_or(&[]) {
                        if json::get_str(op, "op") == Some("append") {
                            appends += 1;
                        }
                        let tool = json::get(op, "item").and_then(|i| json::get(i, "tool"));
                        tool_done |= tool.is_some_and(|t| {
                            json::get_str(t, "name") == Some("apply_patch")
                                && json::get_str(t, "status") == Some("completed")
                        });
                    }
                }
                "permission_requested" => {
                    let p = json::get(&v, "permission").unwrap();
                    perms += 1;
                    eprintln!(
                        "live: phone sees {:?} {:?}",
                        json::get_str(p, "title"),
                        json::get_str(p, "command")
                    );
                    let mut d = msg(vec![
                        ("type", "permission_decision"),
                        ("commandId", &format!("d{perms}")),
                        ("permissionId", json::get_str(p, "id").unwrap()),
                        ("decision", "approve-once"),
                        ("optionId", "accept"),
                    ]);
                    if let Json::Obj(f) = &mut d {
                        f.push((
                            "revision".into(),
                            Json::Int(json::get_i64(p, "revision").unwrap_or(1)),
                        ));
                    }
                    send(&mut phone, d);
                }
                "command_result" if get("commandId") == "c1" => result = Some(get("status")),
                _ => {}
            }
        }
        eprintln!(
            "live: phone appends={appends} approvals={perms} tool_done={tool_done} result={result:?}; 1 turn"
        );
        assert_eq!(result.as_deref(), Some("completed"));
        assert!(perms >= 1 && tool_done && appends >= 1);
        assert_eq!(
            std::fs::read_to_string(work.join("hello.txt"))
                .unwrap()
                .trim(),
            "hi"
        );
        comp.shutdown(true);
    }

    fn wait_until_long(what: &str, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(50));
        }
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
