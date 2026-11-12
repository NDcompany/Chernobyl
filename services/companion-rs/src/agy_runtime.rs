//! ADR-022: Google Antigravity (`agy acp`) as a [`LiveRuntime`]. One `agy
//! acp` child per adapter, spawned on the first command that needs it and
//! again after it exited; each phone session is an agy session inside it. A
//! prompt to a session the child does not have open (it exited, was closed
//! for idling, or the session is from history) respawns agy if needed and
//! `session/resume`s it first.
//!
//! [`Agy`] (the protocol core) turns agy's output into [`Event`]s; this file
//! routes them: transcript ops to the hub, approvals and withdrawals to the
//! engine (G2), command progress and results to the engine's emit path (G1).
//! One lock (`Inner::st`) holds the core and the bookkeeping, and events are
//! dispatched under it, so the hub sees ops in agy's order. Lock order: `st`
//! → hub / engine, never back.
//!
//! History: `session/list` per workspace while agy runs (once at boot, then
//! at most every [`LIST_EVERY_MS`] on the probe cadence). The first
//! subscriber of a listed session gets it replayed by `session/load`.

use crate::adapters::{AdapterError, Capability, LiveRuntime};
use crate::agy::{self, Agy, Event, LaunchConfig, Listed, Prompt};
use crate::claude_runtime::Timing;
use crate::codex_runtime::Sink;
use crate::protocol::{
    Agent, ClientMsg, EventKind, PermissionRequest, PromptImage, ServerMsg, Session, now_ms,
};
use crate::runtime_io::{ChildEvent, ChildIo, child_env};
use crate::transcript::{HubHandle, Op};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Starts an agy child in a folder (`ChildIo::spawn` in production).
type Spawn = Box<dyn Fn(&Path) -> io::Result<(ChildIo, Receiver<ChildEvent>)> + Send + Sync>;

/// `create_session` waits this long for `session/new` before refusing.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// History is listed at most this often while agy runs.
pub const LIST_EVERY_MS: i64 = 60_000;
/// agy with nothing in flight and nobody watching closes after this long.
const IDLE_CLOSE_MS: i64 = 10 * 60 * 1000;
const CREATE_EVERY_MS: i64 = 5_000;

/// T1: a stolen phone cannot fan out agents (rate limit, then the cap).
fn admit(last: Option<i64>, now: i64, every: i64, open: usize, max: usize) -> Result<(), String> {
    if last.is_some_and(|t| now - t < every) {
        return Err("wait a few seconds before starting another session".into());
    }
    if open >= max {
        return Err(format!(
            "too many open Antigravity sessions ({max}): end one first"
        ));
    }
    Ok(())
}

pub struct AgyRuntime {
    inner: Arc<Inner>,
}

struct Inner {
    id: String,
    cfg: LaunchConfig,
    hub: HubHandle,
    sink: Sink,
    spawn: Spawn,
    /// Production health = the binary resolves; injected spawners are healthy.
    probe_binary: bool,
    timing: Timing,
    /// Idle close; tests shrink it.
    idle_close_ms: i64,
    healthy: AtomicBool,
    st: Mutex<State>,
}

#[derive(Default)]
struct State {
    agy: Option<Agy>,
    generation: u64,
    /// Started or adopted here.
    sessions: Vec<Session>,
    /// From agy's `session/list`, not adopted (idle, resumable).
    found: Vec<Session>,
    creating: HashMap<String, Creating>,
    /// Command id → its session, for G1 events.
    tags: HashMap<String, String>,
    /// Commands whose ack refused them: a late outcome is not published.
    dropped: HashSet<String>,
    perms: HashMap<String, PermissionRequest>,
    last_create: Option<i64>,
    /// When the phone or agy last did something (idle close).
    last_active: i64,
    listed_at: Option<i64>,
    /// The models agy offered last, kept across processes.
    models: Vec<(String, String)>,
}

struct Creating {
    reply: Option<Sender<Result<String, String>>>,
    title: String,
    path: String,
}

impl AgyRuntime {
    /// The production runtime: spawns `cfg.binary acp` (no shell, scrubbed env).
    pub fn new(id: &str, cfg: LaunchConfig, hub: HubHandle, sink: Sink) -> Self {
        let (binary, argv) = (cfg.binary.clone(), cfg.argv());
        let spawn: Spawn = Box::new(move |cwd| {
            let env = child_env(std::env::vars_os(), &[]);
            ChildIo::spawn(&binary, &argv, cwd, &env)
        });
        let timing = Timing {
            create_every_ms: CREATE_EVERY_MS,
            ..Timing::default()
        };
        Self::build(id, cfg, hub, sink, spawn, true, timing, IDLE_CLOSE_MS)
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        id: &str,
        cfg: LaunchConfig,
        hub: HubHandle,
        sink: Sink,
        spawn: Spawn,
        probe_binary: bool,
        timing: Timing,
        idle_close_ms: i64,
    ) -> Self {
        let inner = Arc::new(Inner {
            id: id.into(),
            cfg,
            hub,
            sink,
            spawn,
            probe_binary,
            timing,
            idle_close_ms,
            healthy: AtomicBool::new(false),
            st: Mutex::new(State::default()),
        });
        let _ = inner.check_health();
        AgyRuntime { inner }
    }
}

impl LiveRuntime for AgyRuntime {
    fn id(&self) -> &str {
        &self.inner.id
    }

    fn is_healthy(&self) -> bool {
        self.inner.healthy.load(Ordering::SeqCst)
    }

    fn agents(&self) -> Vec<Agent> {
        let i = &self.inner;
        let st = i.st.lock().unwrap();
        let busy = st
            .sessions
            .iter()
            .any(|s| matches!(s.status.as_str(), "running" | "waiting"));
        let healthy = self.is_healthy();
        let state = match (healthy, busy) {
            (false, _) => "offline",
            (true, true) => "busy",
            (true, false) => "online",
        };
        vec![Agent {
            id: i.id.clone(),
            host_id: i.id.clone(),
            kind: "antigravity".into(),
            name: "Antigravity".into(),
            state: state.into(),
            capabilities: vec!["live".into()],
            accepts_new_session: healthy,
            simulated: false,
            live: Some(i.cfg.agent_live(&st.models)),
        }]
    }

    fn sessions(&self) -> Vec<Session> {
        let st = self.inner.st.lock().unwrap();
        st.sessions.iter().chain(&st.found).cloned().collect()
    }

    fn owns_session(&self, session_id: &str) -> bool {
        let st = self.inner.st.lock().unwrap();
        st.sessions
            .iter()
            .chain(&st.found)
            .any(|s| s.id == session_id)
    }

    fn knows_permission(&self, permission_id: &str) -> bool {
        let st = self.inner.st.lock().unwrap();
        st.agy
            .as_ref()
            .is_some_and(|a| a.knows_permission(permission_id))
    }

    fn send(&self, msg: &ClientMsg) -> Result<ServerMsg, AdapterError> {
        self.inner.send(msg)
    }

    /// The first subscriber of a session from history: agy replays it.
    fn on_subscribe(&self, session_id: &str) {
        let i = &self.inner;
        if i.hub.has_items(session_id) {
            return;
        }
        let mut st = i.st.lock().unwrap();
        let Some(cwd) = st
            .found
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| s.project.clone())
        else {
            return;
        };
        st.last_active = now_ms();
        let loaded = i
            .running(&mut st, Path::new(&cwd))
            .map_err(|e| e.to_string())
            .and_then(|a| a.load_session(session_id, &cwd).map_err(str::to_string));
        if let Err(e) = loaded {
            eprintln!("[agy] {}: could not load a session from history: {e}", i.id);
        }
    }

    /// Health, then the history (agy's `session/list`) on the probe cadence.
    fn refresh(&self) -> Result<(), AdapterError> {
        let res = self.inner.check_health();
        if res.is_ok() {
            self.inner.list_history();
        }
        res
    }

    fn capabilities(&self) -> Vec<Capability> {
        agy::capabilities()
    }

    fn dispose(&self) {
        if let Some(mut a) = self.inner.st.lock().unwrap().agy.take() {
            a.shutdown(true);
        }
    }
}

impl Inner {
    fn check_health(&self) -> Result<(), AdapterError> {
        let running = self.st.lock().unwrap().agy.is_some();
        let res = if running || !self.probe_binary {
            Ok(())
        } else {
            probe(&self.cfg.binary)
        };
        self.healthy.store(res.is_ok(), Ordering::SeqCst);
        res
    }

    fn send(self: &Arc<Self>, msg: &ClientMsg) -> Result<ServerMsg, AdapterError> {
        self.st.lock().unwrap().last_active = now_ms();
        match msg {
            ClientMsg::SubmitPrompt { fork: true, .. } => Err(AdapterError::Unsupported(
                "Antigravity sessions cannot be forked from the phone".into(),
            )),
            ClientMsg::CreateSession {
                command_id,
                title,
                prompt,
                project,
                images,
                model,
                ..
            } => self.create(
                command_id,
                title,
                prompt,
                images,
                model.as_deref(),
                project.as_deref(),
            ),
            ClientMsg::SubmitPrompt {
                command_id,
                session_id,
                text,
                images,
                ..
            } => self.prompt(command_id, session_id, text, images),
            ClientMsg::Interrupt {
                command_id,
                session_id,
            } => self.command(command_id, session_id, |a| {
                a.interrupt(session_id, command_id)
            }),
            ClientMsg::SetModel {
                command_id,
                session_id,
                model,
            } => {
                if let Some(e) = self.unknown_model(model) {
                    return Ok(ServerMsg::ack_err(command_id, &e));
                }
                self.command(command_id, session_id, |a| {
                    a.set_model(session_id, model, command_id)
                        .map(|()| Vec::new())
                })
            }
            ClientMsg::SetPermissionMode {
                command_id,
                session_id,
                mode,
            } => self.command(command_id, session_id, |a| {
                a.set_mode(session_id, mode, command_id)
                    .map(|()| Vec::new())
            }),
            ClientMsg::RenameSession {
                command_id,
                session_id,
                title,
            } => Ok(self.rename(command_id, session_id, title)),
            ClientMsg::SessionEnd {
                command_id,
                session_id,
            } => self.end(command_id, session_id),
            ClientMsg::PermissionDecision {
                command_id,
                permission_id,
                decision,
                option_id,
                ..
            } => {
                let mut st = self.st.lock().unwrap();
                let Some(a) = st.agy.as_mut() else {
                    return Ok(ServerMsg::ack_err(
                        command_id,
                        "request is no longer pending",
                    ));
                };
                match a.decide(permission_id, decision, option_id.as_deref()) {
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
            // MCP: agy acp reports no servers, so the phone shows none
            _ => Err(AdapterError::Unsupported(
                "Antigravity does not support this command".into(),
            )),
        }
    }

    /// A model agy did not offer (once it offered some) is refused here.
    fn unknown_model(&self, model: &str) -> Option<String> {
        let st = self.st.lock().unwrap();
        (!st.models.is_empty() && !st.models.iter().any(|(id, _)| id == model))
            .then(|| format!("Antigravity does not offer the model {model}"))
    }

    fn create(
        self: &Arc<Self>,
        command_id: &str,
        title: &str,
        text: &str,
        images: &[PromptImage],
        model: Option<&str>,
        project: Option<&str>,
    ) -> Result<ServerMsg, AdapterError> {
        // the phone names an allow-listed workspace by id, never a folder
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
        let model = match model.filter(|m| *m != "default") {
            Some(m) => {
                if let Some(e) = self.unknown_model(m) {
                    return Ok(ServerMsg::ack_err(command_id, &e));
                }
                Some(m.to_string())
            }
            None => self.cfg.model.clone(),
        };
        let prompt = Prompt {
            text: text.into(),
            images: images.to_vec(),
        };
        let (reply, started) = mpsc::channel();
        {
            let mut st = self.st.lock().unwrap();
            let now = now_ms();
            let open = st
                .sessions
                .iter()
                .filter(|s| !matches!(s.status.as_str(), "completed" | "idle"))
                .count()
                + st.creating.len();
            let every = self.timing.create_every_ms;
            if let Err(e) = admit(st.last_create, now, every, open, self.cfg.max_sessions) {
                return Ok(ServerMsg::ack_err(command_id, &e));
            }
            st.last_create = Some(now);
            self.running(&mut st, Path::new(&ws.path))?
                .start_session(&ws.path, prompt, model, command_id)
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
                    Some(c) => {
                        c.reply = None;
                        st.dropped.insert(command_id.into());
                        Err("Antigravity did not start the session in time".to_string())
                    }
                    None => started
                        .try_recv()
                        .unwrap_or_else(|_| Err("Antigravity exited".into())),
                }
            }
        };
        Ok(match res {
            Ok(session) => ServerMsg::ack_ok(command_id, Some(&session)),
            Err(e) => ServerMsg::ack_err(command_id, &e),
        })
    }

    /// `submit_prompt`: on the open session, or a respawn and `session/
    /// resume` first. A session from history is adopted. Not a create: no
    /// rate limit or cap slot.
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
        let mut st = self.st.lock().unwrap();
        if let Some(at) = st.found.iter().position(|s| s.id == session_id) {
            let s = st.found.remove(at);
            st.sessions.push(s);
        }
        let live = st.agy.as_ref().is_some_and(|a| a.owns_session(session_id));
        if !live {
            let Some(cwd) = st
                .sessions
                .iter()
                .find(|s| s.id == session_id && s.status != "completed")
                .map(|s| s.project.clone())
            else {
                return Ok(ServerMsg::ack_err(
                    command_id,
                    "this Antigravity session has ended; start a new session",
                ));
            };
            let a = self.running(&mut st, Path::new(&cwd))?;
            return match a.resume_session(session_id, &cwd, prompt, command_id) {
                Ok(events) => {
                    st.tags.insert(command_id.into(), session_id.into());
                    self.dispatch(&mut st, events);
                    Ok(ServerMsg::ack_ok(command_id, Some(session_id)))
                }
                Err(e) => Ok(ServerMsg::ack_err(command_id, e)),
            };
        }
        drop(st);
        self.command(command_id, session_id, |a| {
            a.prompt(session_id, &prompt, command_id)
        })
    }

    /// A session command on an open session: acked when written (G1).
    fn command(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
        f: impl FnOnce(&mut Agy) -> Result<Vec<Event>, &'static str>,
    ) -> Result<ServerMsg, AdapterError> {
        let mut st = self.st.lock().unwrap();
        let Some(a) = st.agy.as_mut().filter(|a| a.owns_session(session_id)) else {
            let ended = st
                .sessions
                .iter()
                .any(|s| s.id == session_id && s.status == "completed");
            let why = if ended {
                "this Antigravity session has ended; start a new session"
            } else {
                "Antigravity is not running for this session; send a prompt to resume it"
            };
            return Ok(ServerMsg::ack_err(command_id, why));
        };
        match f(a) {
            Ok(events) => {
                st.tags.insert(command_id.into(), session_id.into());
                self.dispatch(&mut st, events);
                Ok(ServerMsg::ack_ok(command_id, Some(session_id)))
            }
            Err(e) => Ok(ServerMsg::ack_err(command_id, e)),
        }
    }

    /// `rename_session`: the title is the companion's (agy keeps its own).
    fn rename(&self, command_id: &str, session_id: &str, title: &str) -> ServerMsg {
        let mut guard = self.st.lock().unwrap();
        let st = &mut *guard;
        let Some(s) = st
            .sessions
            .iter_mut()
            .chain(st.found.iter_mut())
            .find(|s| s.id == session_id)
        else {
            return ServerMsg::ack_err(command_id, "unknown session");
        };
        s.title = title.into();
        let s = s.clone();
        (self.sink)(EventKind::SessionUpdated(s));
        (self.sink)(EventKind::CommandResult {
            session_id: Some(session_id.into()),
            command_id: command_id.into(),
            status: "completed".into(),
            error: None,
            permission_id: None,
        });
        ServerMsg::ack_ok(command_id, Some(session_id))
    }

    /// `session_end`: `session/close` when agy has it open; ended here either way.
    fn end(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
    ) -> Result<ServerMsg, AdapterError> {
        let mut st = self.st.lock().unwrap();
        if st.agy.as_ref().is_some_and(|a| a.owns_session(session_id)) {
            drop(st);
            return self.command(command_id, session_id, |a| {
                a.close(session_id, command_id).map(|()| Vec::new())
            });
        }
        if let Some(at) = st.found.iter().position(|s| s.id == session_id) {
            let s = st.found.remove(at);
            st.sessions.push(s);
        }
        let ended = crate::transcript::LiveState {
            status: "ended",
            ..Default::default()
        };
        self.op(&mut st, session_id, Op::State(ended));
        (self.sink)(EventKind::CommandResult {
            session_id: Some(session_id.into()),
            command_id: command_id.into(),
            status: "completed".into(),
            error: None,
            permission_id: None,
        });
        Ok(ServerMsg::ack_ok(command_id, Some(session_id)))
    }

    /// The running core, spawning agy (and its reader) when there is none.
    fn running<'a>(
        self: &Arc<Self>,
        st: &'a mut State,
        cwd: &Path,
    ) -> Result<&'a mut Agy, AdapterError> {
        if st.agy.is_none() {
            let (io, rx) = (self.spawn)(cwd).map_err(|e| {
                AdapterError::RuntimeUnavailable(format!(
                    "could not start {}: {e}",
                    self.cfg.binary.display()
                ))
            })?;
            st.generation += 1;
            let generation = st.generation;
            let me = Arc::clone(self);
            let mut core = Agy::new(io, &self.id, generation, env!("CARGO_PKG_VERSION"));
            core.interrupt_ms = self.timing.interrupt_ms;
            thread::Builder::new()
                .name("agy-events".into())
                .spawn(move || me.read(generation, rx))
                .map_err(|e| AdapterError::RuntimeUnavailable(format!("agy reader: {e}")))?;
            st.agy = Some(core);
            st.last_active = now_ms();
            self.healthy.store(true, Ordering::SeqCst);
        }
        Ok(st.agy.as_mut().expect("spawned above"))
    }

    /// History: once at boot (spawning agy), then only while agy runs.
    fn list_history(self: &Arc<Self>) {
        let mut st = self.st.lock().unwrap();
        let now = now_ms();
        let due = st.listed_at.is_none_or(|t| now - t >= LIST_EVERY_MS);
        if !due || (st.agy.is_none() && st.listed_at.is_some()) {
            return;
        }
        st.listed_at = Some(now);
        let first = Path::new(&self.cfg.workspaces[0].path).to_path_buf();
        let Ok(a) = self.running(&mut st, &first) else {
            return;
        };
        for ws in &self.cfg.workspaces {
            let _ = a.list(&ws.path);
        }
    }

    /// The child's output, in order, until it exits; timers and the idle
    /// close run every `timing.tick` in between.
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
            let now = now_ms();
            let idle = now - st.last_active >= self.idle_close_ms;
            let Some(core) = st.agy.as_mut() else {
                return; // disposed
            };
            let Some(event) = event else {
                let events = core.tick(now);
                let watched = core.session_ids().iter().any(|s| self.hub.is_subscribed(s));
                if idle && core.is_quiet() && !watched {
                    // the next prompt respawns it and resumes its sessions
                    core.shutdown(true);
                }
                self.dispatch(&mut st, events);
                continue;
            };
            st.last_active = now;
            let core = st.agy.as_mut().expect("checked above");
            let before = core.malformed();
            let events = core.on_event(event);
            for _ in before..core.malformed() {
                self.hub.malformed();
            }
            if exit {
                eprintln!("[agy] {} exited: {}", self.id, core.stderr_tail().trim());
                st.agy = None;
            }
            self.dispatch(&mut st, events);
            if exit {
                return;
            }
        }
    }

    fn dispatch(&self, st: &mut State, events: Vec<Event>) {
        if let Some(models) = st.agy.as_ref().map(|a| a.models().to_vec())
            && !models.is_empty()
        {
            st.models = models;
        }
        for event in events {
            match event {
                Event::Op(session, op) => self.op(st, &session, op),
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
                Event::SessionStarted { tag, session } => {
                    let Some(c) = st.creating.remove(&tag) else {
                        continue;
                    };
                    let now = now_ms();
                    let s = Session {
                        id: session.clone(),
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
                    st.found.retain(|f| f.id != session);
                    st.sessions.push(s.clone());
                    st.tags.insert(tag, session.clone());
                    (self.sink)(EventKind::SessionCreated(s));
                    if let Some(reply) = c.reply {
                        let _ = reply.send(Ok(session));
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
                    if let Some(c) = st.creating.remove(&tag)
                        && let Some(reply) = c.reply
                    {
                        let why = error.unwrap_or_else(|| "Antigravity refused the session".into());
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
                Event::Listed { sessions, .. } => self.found(st, sessions),
            }
        }
    }

    /// Sessions agy has in the workspaces that are not ours yet.
    fn found(&self, st: &mut State, listed: Vec<Listed>) {
        for l in listed {
            if st.sessions.iter().any(|s| s.id == l.id) {
                continue;
            }
            let Some(cwd) =
                crate::claude_history::inside(agy::native_path(&l.cwd), &self.cfg.workspaces)
            else {
                continue;
            };
            let s = Session {
                id: l.id.clone(),
                agent_id: self.id.clone(),
                host_id: self.id.clone(),
                title: l.title,
                project: cwd,
                status: "idle".into(),
                progress: 0,
                created_at: l.updated_at,
                last_activity_at: l.updated_at,
                prompt: None,
                timeline: vec![],
                output: vec![],
                files: vec![],
                transcript: true,
                live_pc: false,
                forked_from: None,
            };
            match st.found.iter_mut().find(|f| f.id == l.id) {
                Some(f) if f.last_activity_at == s.last_activity_at => {}
                Some(f) => {
                    let title = std::mem::take(&mut f.title);
                    *f = Session { title, ..s };
                    (self.sink)(EventKind::SessionUpdated(f.clone()));
                }
                None => {
                    st.found.push(s.clone());
                    (self.sink)(EventKind::SessionUpdated(s));
                }
            }
        }
        st.found
            .sort_by_key(|s| std::cmp::Reverse(s.last_activity_at));
    }

    /// One transcript op; state changes also move `Session.status` (journaled).
    fn op(&self, st: &mut State, session: &str, op: Op) {
        let state = match op {
            Op::State(state) => state,
            Op::Append { id, field, text } => return self.hub.append(session, &id, field, &text),
            other => return self.hub.push(session, other),
        };
        let ended = state.status == "ended";
        let status = match state.status {
            "starting" | "running" => "running",
            "waiting" => "waiting",
            "ended" => "completed",
            _ => "idle",
        };
        self.hub.push(session, Op::State(state));
        if ended {
            self.hub.end_session(session);
        }
        if let Some(s) = st.sessions.iter_mut().find(|s| s.id == session)
            && s.status != status
        {
            s.status = status.into();
            s.last_activity_at = now_ms();
            (self.sink)(EventKind::SessionUpdated(s.clone()));
        }
    }
}

/// Health without spawning: the binary exists (a path) or is on PATH.
fn probe(binary: &Path) -> Result<(), AdapterError> {
    let name = binary.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".cmd") || name.ends_with(".bat") {
        return Err(AdapterError::RuntimeUnavailable(format!(
            "{} is a shell script; set \"binary\" to agy.exe",
            binary.display()
        )));
    }
    let found = if binary.components().count() > 1 {
        binary.is_file()
    } else {
        let mut names = vec![PathBuf::from(binary)];
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
            "Antigravity binary {} not found (install agy or set \"binary\" in companion.json)",
            binary.display()
        )))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::codex_runtime::tests::{WAIT, fast, recorder, wait_until};
    use crate::json;
    use crate::protocol::LiveWorkspace;
    use std::io::{BufRead, BufReader, Write};

    #[test]
    fn session_cap_and_create_rate_limit_refuse() {
        assert!(admit(None, 10_000, CREATE_EVERY_MS, 0, 3).is_ok());
        assert!(admit(Some(8_000), 10_000, CREATE_EVERY_MS, 0, 3).is_err());
        assert!(admit(Some(5_000), 10_000, CREATE_EVERY_MS, 0, 3).is_ok());
        assert!(admit(None, 10_000, CREATE_EVERY_MS, 2, 3).is_ok());
        let full = admit(None, 10_000, CREATE_EVERY_MS, 3, 3).unwrap_err();
        assert!(full.contains("too many"), "{full}");
    }

    pub(crate) const SID: &str = "6b27e1ab-ee11-459e-912b-7b2dfb3a440f";

    pub(crate) fn launch(ws: &str) -> LaunchConfig {
        LaunchConfig {
            binary: PathBuf::from("agy.exe"),
            args: Vec::new(),
            workspaces: vec![LiveWorkspace {
                id: "ws0123456789".into(),
                label: "work".into(),
                path: ws.into(),
            }],
            model: None,
            max_sessions: 3,
        }
    }

    /// One scripted agy per spawn, over `std::io::pipe()` ends.
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

        pub(crate) fn say(&mut self, line: &str) {
            writeln!(self.to_core, "{line}").unwrap();
        }

        /// Answers the request `frame` with `result`.
        pub(crate) fn answer(&mut self, frame: &str, result: &str) {
            let id = json::get_i64(&json::parse(frame).unwrap(), "id").unwrap();
            self.say(&format!(
                r#"{{"jsonrpc":"2.0","result":{result},"id":{id}}}"#
            ));
        }

        pub(crate) fn update(&mut self, session: &str, update: &str) {
            self.say(&format!(
                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{session}","update":{update}}}}}"#
            ));
        }

        /// The next frame, which must be a request for `method`.
        pub(crate) fn expect(&self, method: &str) -> String {
            let f = self.recv();
            assert!(
                f.contains(&format!(r#""method":"{method}""#)),
                "wanted {method}: {f}"
            );
            f
        }
    }

    pub(crate) fn piped(
        cfg: LaunchConfig,
        hub: HubHandle,
        sink: Sink,
    ) -> (AgyRuntime, Receiver<Peer>) {
        piped_with(cfg, hub, sink, fast(), IDLE_CLOSE_MS)
    }

    fn piped_with(
        cfg: LaunchConfig,
        hub: HubHandle,
        sink: Sink,
        timing: Timing,
        idle_close_ms: i64,
    ) -> (AgyRuntime, Receiver<Peer>) {
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
        let rt = AgyRuntime::build("agy-1", cfg, hub, sink, spawn, false, timing, idle_close_ms);
        (rt, peers)
    }

    pub(crate) const NEW_OK: &str = r#"{"sessionId":"6b27e1ab-ee11-459e-912b-7b2dfb3a440f","models":{"availableModels":[{"modelId":"gemini-3.8-flash-high","name":"Gemini 3.8 Flash (High)"},{"modelId":"gemini-3.8-flash-low","name":"Gemini 3.8 Flash (Low)"}],"currentModelId":"gemini-3.8-flash-high"},"modes":{"currentModeId":"default"}}"#;

    /// The next spawn, past `initialize`.
    pub(crate) fn initialized(peers: &Receiver<Peer>) -> Peer {
        let mut peer = peers.recv_timeout(WAIT).expect("a spawn");
        let init = peer.expect("initialize");
        peer.answer(&init, r#"{"protocolVersion":1}"#);
        peer
    }

    pub(crate) fn create(cid: &str, model: Option<&str>) -> ClientMsg {
        ClientMsg::CreateSession {
            command_id: cid.into(),
            agent_id: "agy-1".into(),
            title: "Fix tests".into(),
            prompt: "hello".into(),
            project: Some("ws0123456789".into()),
            images: vec![],
            model: model.map(str::to_string),
        }
    }

    pub(crate) fn prompt(cid: &str, text: &str) -> ClientMsg {
        ClientMsg::SubmitPrompt {
            command_id: cid.into(),
            session_id: SID.into(),
            text: text.into(),
            images: vec![],
            fork: false,
        }
    }

    /// `create` waits on another thread while the peer answers
    /// `session/new`; returns the peer at the first prompt (its frame).
    pub(crate) fn started(
        peers: &Receiver<Peer>,
        create: impl FnOnce() -> Option<ServerMsg> + Send,
    ) -> (Peer, String) {
        thread::scope(|scope| {
            let ack = scope.spawn(create);
            let mut peer = initialized(peers);
            let new = peer.expect("session/new");
            peer.answer(&new, NEW_OK);
            let reply = ack.join().unwrap().expect("an ack");
            assert!(
                matches!(&reply, ServerMsg::Ack { ok: true, session_id: Some(s), .. } if s == SID),
                "{reply:?}"
            );
            let first = peer.expect("session/prompt");
            (peer, first)
        })
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

    fn ack_ok(r: Result<ServerMsg, AdapterError>) -> bool {
        matches!(r, Ok(ServerMsg::Ack { ok: true, .. }))
    }

    const ASK: &str = r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"6b27e1ab-ee11-459e-912b-7b2dfb3a440f","toolCall":{"toolCallId":"call_9","name":"run_command","status":"pending","kind":"execute","title":"hostname","rawInput":{"CommandLine":"hostname"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"1"}"#;

    #[test]
    fn create_streams_approves_steers_and_ends() {
        let hub = HubHandle::default();
        let (sink, seen) = recorder();
        let (rt, peers) = piped(launch(r"C:\work"), hub.clone(), sink);
        assert!(rt.is_healthy());
        let agent = &rt.agents()[0];
        assert_eq!(
            (agent.kind.as_str(), agent.name.as_str()),
            ("antigravity", "Antigravity")
        );
        let live = agent.live.as_ref().unwrap();
        assert_eq!(live.prompt_while_busy, "merge");
        assert_eq!(live.workspaces[0].id, "ws0123456789");
        // no models known yet: no model picker
        assert!(!live.composer.contains(&"model"));
        // the phone never picks a folder
        let mut bad = create("c0", None);
        if let ClientMsg::CreateSession { project, .. } = &mut bad {
            *project = Some(r"C:\Windows".into());
        }
        assert!(matches!(
            rt.send(&bad),
            Ok(ServerMsg::Ack { ok: false, .. })
        ));

        let (mut peer, first) = started(&peers, || rt.send(&create("c1", None)).ok());
        assert!(first.contains(r#""text":"hello""#), "{first}");
        // agy's models now drive the picker
        let live = rt.agents()[0].live.clone().unwrap();
        assert_eq!(live.composer[0], "model");
        assert_eq!(live.models[1].0, "gemini-3.8-flash-low");
        peer.update(SID, r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Running it"}}"#);
        peer.update(SID, r#"{"sessionUpdate":"tool_call","toolCallId":"call_9","name":"run_command","status":"in_progress","kind":"execute","title":"hostname","rawInput":{"CommandLine":"hostname"}}"#);
        peer.say(ASK);
        wait_until("the permission", || !requested(&seen).is_empty());
        let pr = requested(&seen).remove(0);
        assert_eq!(pr.id, "agy-1:1-1");
        assert_eq!(pr.command, "hostname");
        assert!(rt.knows_permission("agy-1:1-1"));
        wait_until("waiting", || rt.sessions()[0].status == "waiting");
        let decide = |cid: &str, decision: &str, option: &str| ClientMsg::PermissionDecision {
            command_id: cid.into(),
            permission_id: "agy-1:1-1".into(),
            decision: decision.into(),
            revision: 1.0,
            option_id: Some(option.into()),
            message: None,
            answers: vec![],
        };
        assert!(ack_ok(rt.send(&decide("d1", "approve-once", "allow"))));
        assert_eq!(
            peer.recv(),
            r#"{"jsonrpc":"2.0","id":"1","result":{"outcome":{"outcome":"selected","optionId":"allow"}}}"#
        );
        assert!(!ack_ok(rt.send(&decide("d2", "approve-once", "allow"))));
        peer.update(SID, r#"{"sessionUpdate":"tool_call_update","toolCallId":"call_9","status":"completed","kind":"execute","rawOutput":{"output":"PC-1"}}"#);
        // a follow-up while busy steers: written at once
        assert!(ack_ok(rt.send(&prompt("p1", "and whoami"))));
        let steer = peer.expect("session/prompt");
        assert!(steer.contains("and whoami"), "{steer}");
        peer.answer(&first, r#"{"stopReason":"end_turn"}"#);
        peer.answer(
            &steer,
            r#"{"stopReason":"end_turn","usage":{"inputTokens":5,"outputTokens":2}}"#,
        );
        wait_until("both prompts", || {
            let r = results(&seen);
            r.contains(&("c1".into(), "completed".into()))
                && r.contains(&("p1".into(), "completed".into()))
        });
        wait_until("idle", || rt.sessions()[0].status == "idle");
        let ops = hub.ops(SID).join("\n");
        for want in [
            r#""kind":"user""#,
            r#""text":"Running it""#,
            r#""status":"awaiting-approval""#,
            r#""output":"PC-1""#,
            r#""text":"and whoami""#,
            r#""inputTokens":5"#,
        ] {
            assert!(ops.contains(want), "{want} missing from {ops}");
        }
        // model and mode: only agy's models, only the three phone modes
        let set = |m: &str| ClientMsg::SetModel {
            command_id: "m1".into(),
            session_id: SID.into(),
            model: m.into(),
        };
        assert!(!ack_ok(rt.send(&set("gemini-9-ultra"))));
        assert!(ack_ok(rt.send(&set("gemini-3.8-flash-low"))));
        let f = peer.expect("session/set_model");
        peer.answer(&f, "{}");
        wait_until("the model", || {
            results(&seen).contains(&("m1".into(), "completed".into()))
        });
        let mode = ClientMsg::SetPermissionMode {
            command_id: "s1".into(),
            session_id: SID.into(),
            mode: "acceptEdits".into(),
        };
        assert!(ack_ok(rt.send(&mode)));
        assert!(
            peer.expect("session/set_mode")
                .contains(r#""modeId":"accept-edits""#)
        );
        // rename is the companion's
        let rename = ClientMsg::RenameSession {
            command_id: "r1".into(),
            session_id: SID.into(),
            title: "Hostnames".into(),
        };
        assert!(ack_ok(rt.send(&rename)));
        assert_eq!(rt.sessions()[0].title, "Hostnames");
        // MCP and forks are not Antigravity's
        let mcp = ClientMsg::McpReconnect {
            command_id: "x".into(),
            session_id: SID.into(),
            server: "s".into(),
        };
        assert!(matches!(rt.send(&mcp), Err(AdapterError::Unsupported(_))));
        let mut fork = prompt("f1", "x");
        if let ClientMsg::SubmitPrompt { fork: f, .. } = &mut fork {
            *f = true;
        }
        assert!(matches!(rt.send(&fork), Err(AdapterError::Unsupported(_))));
        // end: session/close, then the session is over
        let end = ClientMsg::SessionEnd {
            command_id: "e1".into(),
            session_id: SID.into(),
        };
        assert!(ack_ok(rt.send(&end)));
        let f = peer.expect("session/close");
        peer.answer(&f, "{}");
        wait_until("the end", || rt.sessions()[0].status == "completed");
        assert!(!ack_ok(rt.send(&prompt("p9", "more"))));
    }

    #[test]
    fn deny_interrupt_exit_and_resume() {
        let hub = HubHandle::default();
        let (sink, seen) = recorder();
        let (rt, peers) = piped(launch(r"C:\work"), hub.clone(), sink);
        let (mut peer, first) = started(&peers, || rt.send(&create("c1", None)).ok());
        peer.say(ASK);
        wait_until("the permission", || requested(&seen).len() == 1);
        let deny = ClientMsg::PermissionDecision {
            command_id: "d1".into(),
            permission_id: "agy-1:1-1".into(),
            decision: "reject".into(),
            revision: 1.0,
            option_id: Some("deny".into()),
            message: None,
            answers: vec![],
        };
        assert!(ack_ok(rt.send(&deny)));
        assert!(peer.recv().contains(r#""optionId":"deny""#));
        // a second ask, then the interrupt: answered cancelled, withdrawn
        peer.say(&ASK.replace(r#""id":"1""#, r#""id":"2""#));
        wait_until("the second permission", || requested(&seen).len() == 2);
        let stop = ClientMsg::Interrupt {
            command_id: "i1".into(),
            session_id: SID.into(),
        };
        assert!(ack_ok(rt.send(&stop)));
        assert!(peer.expect("session/cancel").contains(SID));
        assert!(peer.recv().contains(r#""outcome":"cancelled""#));
        wait_until("the withdrawal", || {
            seen.lock().unwrap().iter().any(|k| {
                matches!(k, EventKind::PermissionResolved(p) if p.id == "agy-1:1-2" && p.status == "stale")
            })
        });
        peer.answer(&first, r#"{"stopReason":"cancelled"}"#);
        wait_until("the cancelled prompt", || {
            let r = results(&seen);
            r.contains(&("c1".into(), "cancelled".into()))
                && r.contains(&("i1".into(), "completed".into()))
        });
        // agy goes away: the session idles and the next prompt resumes it
        drop(peer);
        wait_until("the exit", || {
            hub.ops(SID)
                .iter()
                .any(|o| o.contains("the next prompt resumes"))
        });
        assert_eq!(rt.sessions()[0].status, "idle");
        assert!(ack_ok(rt.send(&prompt("p2", "go on"))));
        let mut peer = initialized(&peers);
        let resume = peer.expect("session/resume");
        assert!(
            resume.contains(&format!(r#""sessionId":"{SID}","cwd":"C:\\work""#)),
            "{resume}"
        );
        // one resume at a time
        assert!(!ack_ok(rt.send(&prompt("p3", "again"))));
        peer.answer(&resume, NEW_OK);
        let p = peer.expect("session/prompt");
        assert!(p.contains("go on"));
        peer.answer(&p, r#"{"stopReason":"end_turn"}"#);
        wait_until("the resumed prompt", || {
            results(&seen).contains(&("p2".into(), "completed".into()))
        });
        assert_eq!(rt.sessions().len(), 1, "a resume is not a create");
        // the respawned process's items never reuse the first one's ids
        let ops = hub.ops(SID);
        assert!(ops.iter().any(|o| o.contains(r#""id":"user:1.1""#)));
        assert!(
            ops.iter().any(|o| o.contains(r#""id":"user:2."#)),
            "{ops:?}"
        );
    }

    #[test]
    fn history_is_listed_loaded_and_adopted() {
        let hub = HubHandle::default();
        let (sink, seen) = recorder();
        let ws = std::env::temp_dir();
        let ws_path = crate::claude_live::plain(&std::fs::canonicalize(&ws).unwrap());
        let (rt, peers) = piped(launch(&ws_path), hub.clone(), sink);
        // the boot list spawns agy once
        rt.refresh().unwrap();
        let mut peer = initialized(&peers);
        let list = peer.expect("session/list");
        let listed_cwd = format!("/{}", ws_path.replace('\\', "/"));
        peer.answer(
            &list,
            &format!(
                r#"{{"sessions":[{{"sessionId":"{SID}","cwd":{},"title":"Ping And Summarize","updatedAt":"2026-11-11T23:29:14Z"}},{{"sessionId":"elsewhere","cwd":"/C:/Windows","title":"x"}}]}}"#,
                json::stringify(&json::s(&listed_cwd))
            ),
        );
        wait_until("the found session", || rt.sessions().len() == 1);
        let s = rt.sessions().remove(0);
        assert_eq!(
            (s.id.as_str(), s.status.as_str(), s.title.as_str()),
            (SID, "idle", "Ping And Summarize")
        );
        assert!(rt.owns_session(SID));
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .any(|k| matches!(k, EventKind::SessionUpdated(s) if s.id == SID))
        );
        // listed again within a minute: nothing is sent
        rt.refresh().unwrap();
        // the first subscriber gets it replayed
        rt.on_subscribe(SID);
        let load = peer.expect("session/load");
        peer.update(
            SID,
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Ping it"}}"#,
        );
        peer.update(
            SID,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Pinged"}}"#,
        );
        peer.answer(&load, NEW_OK);
        wait_until("the replay", || {
            let ops = hub.ops(SID).join("\n");
            ops.contains(r#""text":"Ping it""#) && ops.contains(r#""status":"idle""#)
        });
        // a prompt adopts it: no create, no resume needed (it is open)
        assert!(ack_ok(rt.send(&prompt("p1", "again"))));
        let p = peer.expect("session/prompt");
        peer.answer(&p, r#"{"stopReason":"end_turn"}"#);
        wait_until("the prompt", || {
            results(&seen).contains(&("p1".into(), "completed".into()))
        });
        assert_eq!(rt.sessions().len(), 1);
    }

    #[test]
    fn idle_agy_closes_and_caps_hold() {
        let hub = HubHandle::default();
        let (sink, seen) = recorder();
        let mut cfg = launch(r"C:\work");
        cfg.max_sessions = 1;
        let (rt, peers) = piped_with(cfg, hub.clone(), sink, fast(), 200);
        let (mut peer, first) = started(&peers, || rt.send(&create("c1", None)).ok());
        // the cap: one running session
        assert!(!ack_ok(rt.send(&create("c2", None))));
        peer.answer(&first, r#"{"stopReason":"end_turn"}"#);
        wait_until("idle", || rt.sessions()[0].status == "idle");
        // quiet and unwatched: stdin closes; no error notice for an idle close
        let closed = peer.frames.recv_timeout(WAIT);
        assert!(closed.is_err(), "nothing more is written: {closed:?}");
        drop(peer);
        wait_until("the close", || rt.inner.st.lock().unwrap().agy.is_none());
        assert!(
            !hub.ops(SID)
                .iter()
                .any(|o| o.contains("the next prompt resumes"))
        );
        assert!(results(&seen).contains(&("c1".into(), "completed".into())));
    }

    /// `node` + the committed fake `agy acp` (the real spawn path), or
    /// `None` when node is not installed.
    pub(crate) fn fake_agy(ws: &Path, extra: &[&str]) -> Option<LaunchConfig> {
        if probe(Path::new("node")).is_err() {
            eprintln!("skipped: node is not on PATH");
            return None;
        }
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tools")
            .join("fake-agy-acp.mjs");
        let ws = crate::claude_live::plain(&std::fs::canonicalize(ws).ok()?);
        let mut cfg = launch(&ws);
        cfg.binary = PathBuf::from("node");
        cfg.args = vec![script.to_string_lossy().into_owned()];
        cfg.args.extend(extra.iter().map(|a| a.to_string()));
        Some(cfg)
    }

    /// The real spawn path against the fake: a denied command is approved
    /// and runs, an interrupt stops a slow one, and after agy exits the next
    /// prompt resumes the conversation it saved.
    #[test]
    fn fake_agy_runs_approves_interrupts_and_resumes() {
        let state =
            std::env::temp_dir().join(format!("chernobyl-agy-state-{}", std::process::id()));
        let state_arg = state.to_string_lossy().into_owned();
        let Some(cfg) = fake_agy(&std::env::temp_dir(), &["--state-dir", &state_arg]) else {
            return;
        };
        let hub = HubHandle::default();
        let (sink, seen) = recorder();
        let rt = AgyRuntime::new("agy-1", cfg, hub.clone(), sink);
        assert!(rt.is_healthy(), "node resolves on PATH");
        let session = match rt.send(&create("c1", Some("fake-model-low"))).unwrap() {
            ServerMsg::Ack {
                ok: true,
                session_id: Some(s),
                ..
            } => s,
            other => panic!("{other:?}"),
        };
        let done = |cid: &str, status: &str| results(&seen).contains(&(cid.into(), status.into()));
        wait_until("the first turn", || done("c1", "completed"));
        let say = |cid: &str, text: &str| ClientMsg::SubmitPrompt {
            command_id: cid.into(),
            session_id: session.clone(),
            text: text.into(),
            images: vec![],
            fork: false,
        };
        // a command the fake asks about: approve it, it runs
        assert!(ack_ok(rt.send(&say("p1", "run hostname"))));
        wait_until("the approval", || !requested(&seen).is_empty());
        let pr = requested(&seen).remove(0);
        let allow = ClientMsg::PermissionDecision {
            command_id: "d1".into(),
            permission_id: pr.id.clone(),
            decision: "approve-once".into(),
            revision: 1.0,
            option_id: Some("allow".into()),
            message: None,
            answers: vec![],
        };
        assert!(ack_ok(rt.send(&allow)));
        wait_until("the command ran", || done("p1", "completed"));
        assert!(hub.ops(&session).join("\n").contains("FAKE-HOST"));
        // a slow one, interrupted
        assert!(ack_ok(rt.send(&say("p2", "slow"))));
        wait_until("the slow tool", || {
            hub.ops(&session).join("\n").contains("sleep 60")
        });
        let stop = ClientMsg::Interrupt {
            command_id: "i1".into(),
            session_id: session.clone(),
        };
        assert!(ack_ok(rt.send(&stop)));
        wait_until("the interrupt", || {
            done("p2", "cancelled") && done("i1", "completed")
        });
        // agy goes away; the conversation resumes in a new process
        rt.dispose();
        wait_until("the exit", || rt.inner.st.lock().unwrap().agy.is_none());
        assert!(ack_ok(rt.send(&say("p3", "what did I ask first?"))));
        wait_until("the resumed turn", || done("p3", "completed"));
        assert!(
            hub.ops(&session)
                .join("\n")
                .contains("You first said: hello")
        );
        assert_eq!(rt.sessions().len(), 1);
        rt.dispose();
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn probe_refuses_scripts_and_missing_binaries() {
        assert!(probe(Path::new(r"C:\agy\agy.cmd")).is_err());
        assert!(probe(Path::new("surely-not-an-agy-binary-here")).is_err());
        assert!(probe(Path::new(r"C:\nope\agy.exe")).is_err());
        assert!(probe(&std::env::current_exe().unwrap()).is_ok());
    }

    /// Live, against the owner's agy login (run once, by hand):
    /// `cargo test agy_live -- --ignored --nocapture`. Cheap model, temp
    /// workspace; never `--dangerously-skip-permissions`. Every approval is
    /// answered "allow once" from here, as a phone would.
    #[test]
    #[ignore]
    fn agy_live_create_approve_follow_up_interrupt_resume() {
        let ws = std::env::temp_dir().join(format!("chernobyl-agy-live-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let ws = crate::claude_live::plain(&std::fs::canonicalize(&ws).unwrap());
        let mut cfg = launch(&ws);
        cfg.binary = agy::launch_config(
            &json::parse(&format!(
                r#"{{"workspaces":[{}]}}"#,
                json::stringify(&json::s(&ws))
            ))
            .unwrap(),
            Path::new(r"C:\nonexistent-chernobyl-data"),
        )
        .unwrap()
        .binary;
        let hub = HubHandle::default();
        let (sink, seen) = recorder();
        let rt = AgyRuntime::new("agy-1", cfg, hub.clone(), sink);
        assert!(rt.is_healthy(), "agy is installed");
        let session = Mutex::new(String::new());
        let decided = Mutex::new(HashSet::<String>::new());
        let approve = || {
            for pr in requested(&seen) {
                if !decided.lock().unwrap().insert(pr.id.clone()) {
                    continue;
                }
                eprintln!("approving: {} `{}`", pr.title, pr.command);
                let allow = pr.options.iter().find(|o| o.decision == "approve-once");
                let msg = ClientMsg::PermissionDecision {
                    command_id: format!("d-{}", pr.id),
                    permission_id: pr.id.clone(),
                    decision: "approve-once".into(),
                    revision: 1.0,
                    option_id: allow.map(|o| o.id.clone()),
                    message: None,
                    answers: vec![],
                };
                let _ = rt.send(&msg);
            }
        };
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(180);
            while !f() {
                approve();
                if std::time::Instant::now() >= deadline {
                    let ops = hub.ops(&session.lock().unwrap());
                    let tail = ops.iter().rev().take(25).rev().cloned().collect::<Vec<_>>();
                    panic!(
                        "timed out waiting for {what}; last ops:\n{}",
                        tail.join("\n")
                    );
                }
                thread::sleep(Duration::from_millis(100));
            }
        };
        let done = |cid: &str| results(&seen).iter().any(|(c, _)| c == cid);
        let status = |cid: &str| {
            results(&seen)
                .into_iter()
                .find(|(c, _)| c == cid)
                .map(|(_, s)| s)
                .unwrap_or_default()
        };
        let msg = ClientMsg::CreateSession {
            command_id: "c1".into(),
            agent_id: "agy-1".into(),
            title: "live".into(),
            prompt: "Reply with just: ready".into(),
            project: Some("ws0123456789".into()),
            images: vec![],
            model: Some("gemini-3.8-flash-low".into()),
        };
        let started = std::time::Instant::now();
        match rt.send(&msg).unwrap() {
            ServerMsg::Ack {
                ok: true,
                session_id: Some(s),
                ..
            } => *session.lock().unwrap() = s,
            other => panic!("{other:?}"),
        }
        let sid = session.lock().unwrap().clone();
        let say = |cid: &str, text: &str| ClientMsg::SubmitPrompt {
            command_id: cid.into(),
            session_id: sid.clone(),
            text: text.into(),
            images: vec![],
            fork: false,
        };
        wait("the first turn", &|| done("c1"));
        eprintln!("c1: {} after {:?}", status("c1"), started.elapsed());
        rt.send(&say(
            "p1",
            "Run the shell command `hostname` with your command tool and tell me its output.",
        ))
        .unwrap();
        wait("the command turn", &|| done("p1"));
        eprintln!("p1: {}, asked {}", status("p1"), requested(&seen).len());
        rt.send(&say("p2", "Now reply with just: second")).unwrap();
        wait("the follow-up", &|| done("p2"));
        eprintln!("p2: {}", status("p2"));
        rt.send(&say(
            "p3",
            "Run `ping -n 30 127.0.0.1` with your command tool and wait for it.",
        ))
        .unwrap();
        wait("the slow command", &|| {
            hub.ops(&sid)
                .iter()
                .any(|o| o.contains("ping -n 30") && o.contains(r#""status":"running""#))
        });
        thread::sleep(Duration::from_secs(2));
        let stop = ClientMsg::Interrupt {
            command_id: "i1".into(),
            session_id: sid.clone(),
        };
        let stopped = std::time::Instant::now();
        rt.send(&stop).unwrap();
        wait("the interrupt", &|| done("p3") && done("i1"));
        eprintln!(
            "p3: {}, i1: {} after {:?}",
            status("p3"),
            status("i1"),
            stopped.elapsed()
        );
        rt.dispose();
        wait("the exit", &|| rt.inner.st.lock().unwrap().agy.is_none());
        rt.send(&say("p4", "What word did you reply with first? One word."))
            .unwrap();
        wait("the resumed turn", &|| done("p4"));
        eprintln!("p4: {}", status("p4"));
        let ops = hub.ops(&sid).join("\n");
        let usage: i64 = ops
            .lines()
            .filter_map(|l| json::parse(l).ok())
            .filter_map(|o| {
                json::get(&o, "turn")
                    .and_then(|t| json::get(t, "usage"))
                    .cloned()
            })
            .map(|u| {
                json::get_i64(&u, "inputTokens").unwrap_or(0)
                    + json::get_i64(&u, "outputTokens").unwrap_or(0)
            })
            .sum();
        eprintln!("tokens (input + output, all turns): {usage}");
        eprintln!("--- transcript ---\n{ops}");
        rt.dispose();
        assert_eq!(status("c1"), "completed");
        assert_eq!(status("p1"), "completed");
        assert!(!requested(&seen).is_empty(), "hostname needs an approval");
        assert_eq!(status("p3"), "cancelled");
        assert_eq!(status("i1"), "completed");
        assert_eq!(status("p4"), "completed");
        assert!(
            ops.to_lowercase().contains("ready"),
            "the resumed session remembers"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }
}
