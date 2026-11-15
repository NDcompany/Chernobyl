//! Google Antigravity adapter core (ADR-022): the Agent Client Protocol
//! client for one `agy acp` child, the `session/update` decoder and the
//! inline approval mapping. Protocol code only; the companion wiring
//! (LiveRuntime, transcript hub, engine) is `agy_runtime.rs`.
//!
//! Wire (verified live against agy 1.5.6, `docs/reverse-engineering/
//! codex-and-antigravity.md` B9): JSON-RPC 2.0, one object per line. We
//! number our requests from 1; agy numbers its own (`session/request_
//! permission`) with string ids, so a message routes on `method` (method +
//! id = agy's request, method alone = notification, id alone = a reply to
//! us). Every agy request is answered exactly once.
//!
//! One `session/prompt` is one turn. A prompt sent while another runs steers
//! it: agy ends the first with `end_turn` and goes on under the second, so a
//! session can have several turns in flight and new items belong to the
//! newest one.

use crate::adapters::Capability;
use crate::claude_live::{added_patch, replace_patch};
use crate::codex::{
    clip, detail_edit, detail_shell, file_diff, first_line, head_tail, new_item, new_tool,
    paths_title,
};
use crate::json::{self, Json};
use crate::protocol::{
    AgentLive, LiveWorkspace, PermissionOption, PermissionRequest, PromptImage, now_ms,
};
use crate::runtime_io::{ChildEvent, ChildIo, redact};
use crate::transcript::{
    Field, FileDiff, Item, ItemKind, LiveState, Op, PlanStep, ToolCall, ToolStatus, TurnState,
    Usage,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// §4.6: an interrupt with no turn end by then is `unknown`.
pub const INTERRUPT_MS: i64 = 30_000;
const OUTPUT_CAP: usize = 32 << 10;
const INPUT_CAP: usize = 16 << 10;
const TEXT_CAP: usize = 256 << 10;
const THINKING_CAP: usize = 64 << 10;
/// Permission records are journaled: the legacy command stays small.
const COMMAND_CAP: usize = 2 << 10;
/// `session/list` entries kept per workspace.
const LISTED_MAX: usize = 50;
/// The phone's permission-mode names and agy's (`session/set_mode`). No
/// other mode is reachable from the phone: never `auto`.
pub const MODES: [(&str, &str); 3] = [
    ("default", "default"),
    ("acceptEdits", "accept-edits"),
    ("plan", "plan"),
];
const HONESTY: &str = "agy acp (Agent Client Protocol) verified live against agy 1.5.6";

/// What the companion does with agy's output (`agy_runtime` wires each one).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A transcript op for one session (agy's session id).
    Op(String, Op),
    /// A new approval for the engine (G2); its tool card op follows it.
    Permission(Box<PermissionRequest>),
    /// Interrupted, ended with its turn, or agy exited: the engine resolves
    /// it `stale`. It is never answered afterwards.
    Withdrawn(String),
    /// `session/new` succeeded for command `tag`: the session exists.
    SessionStarted { tag: String, session: String },
    /// G1 progress for command `tag`: `running` or `accepted` (interrupt).
    Progress { tag: String, status: &'static str },
    /// G1 result for command `tag`: completed | cancelled | failed | unknown.
    Done {
        tag: String,
        status: &'static str,
        error: Option<String>,
    },
    /// `session/list` for the workspace `cwd`.
    Listed { cwd: String, sessions: Vec<Listed> },
}

/// One past conversation agy has in a workspace.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub id: String,
    /// As agy wrote it (`/C:/x` on Windows); the runtime canonicalizes it.
    pub cwd: String,
    pub title: String,
    pub updated_at: i64,
}

/// A prompt as it goes to agy: text plus images (ACP `image` blocks).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Prompt {
    pub text: String,
    pub images: Vec<PromptImage>,
}

/// The capability declaration (PRD §7.3).
pub fn capabilities() -> Vec<Capability> {
    let mut caps: Vec<Capability> = [
        "session.create",
        "session.prompt",
        "session.interrupt",
        "session.activity.stream",
        "permission.review",
        "diff.read",
    ]
    .into_iter()
    .map(|id| Capability::new(id, "available", Some(HONESTY)))
    .collect();
    caps.push(Capability::new(
        "session.list",
        "limited",
        Some("agy's own session/list for the allow-listed workspaces, read while agy runs"),
    ));
    caps.push(Capability::new(
        "mcp.status",
        "unavailable",
        Some("agy acp reports no MCP server status"),
    ));
    caps
}

/// A request we sent, by id, with what its reply means.
enum Pending {
    Init,
    New {
        tag: String,
        cwd: String,
        prompt: Prompt,
        model: Option<String>,
    },
    /// A prompt to a session agy has on disk but not in this process.
    Resume {
        tag: String,
        session: String,
        cwd: String,
        prompt: Prompt,
    },
    /// History for the phone: agy replays the conversation as updates.
    Load {
        session: String,
    },
    SetModel {
        tag: String,
        session: String,
        model: String,
        /// `create_session` with a model: the first prompt waits for it.
        then: Option<Prompt>,
    },
    SetMode {
        tag: String,
        session: String,
        mode: String,
    },
    Prompt {
        tag: String,
        session: String,
    },
    Close {
        tag: String,
        session: String,
    },
    List {
        cwd: String,
    },
}

impl Pending {
    fn tag(&self) -> Option<&str> {
        match self {
            Pending::Init | Pending::Load { .. } | Pending::List { .. } => None,
            Pending::New { tag, .. }
            | Pending::Resume { tag, .. }
            | Pending::SetModel { tag, .. }
            | Pending::SetMode { tag, .. }
            | Pending::Prompt { tag, .. }
            | Pending::Close { tag, .. } => Some(tag),
        }
    }
}

/// An approval waiting for the phone, by permission id.
struct Ask {
    /// agy's request id, echoed verbatim in the one answer.
    id: Json,
    session: String,
    /// The tool card (agy's toolCallId).
    item: String,
    /// The native option ids offered on the phone, by phone option id.
    options: Vec<PermissionOption>,
}

/// One turn in flight: the command that started it and its state.
struct Turn {
    tag: String,
    state: TurnState,
}

/// A session this agy process has open.
#[derive(Default)]
struct Sess {
    cwd: String,
    model: Option<String>,
    /// The phone's name (`acceptEdits`), not agy's.
    mode: Option<String>,
    commands: Vec<String>,
    /// In flight, oldest first; new items belong to the last one.
    turns: Vec<Turn>,
    /// The streaming text, thinking or (while loading) user item.
    open: Option<Item>,
    /// Tool cards not settled yet, by toolCallId.
    tools: HashMap<String, Item>,
    /// `session/load` is replaying the history.
    loading: bool,
    /// Interrupts waiting for the turns to end.
    interrupts: Vec<String>,
}

/// One `agy acp` process: its JSON-RPC state and every session inside it.
/// Not thread-safe by itself; the runtime holds it in a Mutex, feeds it the
/// `ChildIo` events and calls the command methods.
pub struct Agy {
    io: ChildIo,
    adapter: String,
    /// Distinguishes this process's request ids from an earlier one's.
    generation: u64,
    next_id: i64,
    ready: bool,
    queued: Vec<String>,
    pending: HashMap<i64, Pending>,
    asks: HashMap<String, Ask>,
    sessions: HashMap<String, Sess>,
    /// Sessions waiting for `session/resume` or `session/load`.
    resuming: HashSet<String>,
    /// Interrupt deadlines: (command, deadline).
    timers: Vec<(String, i64)>,
    /// The models agy offers, (id, label).
    models: Vec<(String, String)>,
    pub interrupt_ms: i64,
    seq: u64,
    malformed: u64,
    exited: bool,
    /// Closed on purpose (idle): the exit posts no error notice.
    quiet: bool,
}

impl Agy {
    /// Takes over an `agy acp` child's stdio and sends `initialize`.
    pub fn new(io: ChildIo, adapter_id: &str, generation: u64, version: &str) -> Agy {
        let mut agy = Agy {
            io,
            adapter: adapter_id.into(),
            generation,
            next_id: 1,
            ready: false,
            queued: Vec::new(),
            pending: HashMap::new(),
            asks: HashMap::new(),
            sessions: HashMap::new(),
            resuming: HashSet::new(),
            timers: Vec::new(),
            models: Vec::new(),
            interrupt_ms: INTERRUPT_MS,
            seq: 0,
            malformed: 0,
            exited: false,
            quiet: false,
        };
        let params = json::obj(vec![
            ("protocolVersion", Json::Int(1)),
            // no client fs or terminal: agy works on disk itself
            ("clientCapabilities", json::obj(vec![])),
            (
                "clientInfo",
                json::obj(vec![
                    ("name", json::s("chernobyl")),
                    ("version", json::s(version)),
                ]),
            ),
        ]);
        let id = agy.alloc(Pending::Init);
        let _ = agy.io.try_send(request(id, "initialize", params));
        agy
    }

    /// `create_session`: `session/new` in `cwd` (an allow-listed workspace),
    /// then `model` if any, then `prompt`.
    pub fn start_session(
        &mut self,
        cwd: &str,
        prompt: Prompt,
        model: Option<String>,
        tag: &str,
    ) -> Result<(), &'static str> {
        let params = json::obj(vec![
            ("cwd", json::s(cwd)),
            ("mcpServers", Json::Arr(vec![])),
        ]);
        let pending = Pending::New {
            tag: tag.into(),
            cwd: cwd.into(),
            prompt,
            model,
        };
        self.call("session/new", params, pending)
    }

    /// `submit_prompt` to a session agy keeps on disk but this process has
    /// not opened (it exited, or the session is from history): `session/
    /// resume`, then the prompt. Not a create.
    pub fn resume_session(
        &mut self,
        session: &str,
        cwd: &str,
        prompt: Prompt,
        tag: &str,
    ) -> Result<Vec<Event>, &'static str> {
        if self.resuming.contains(session) {
            return Err("Antigravity is still opening this session; send again in a moment");
        }
        let pending = Pending::Resume {
            tag: tag.into(),
            session: session.into(),
            cwd: cwd.into(),
            prompt,
        };
        self.call("session/resume", open_params(session, cwd), pending)?;
        self.resuming.insert(session.into());
        let state = LiveState {
            status: "starting",
            ..Default::default()
        };
        Ok(vec![Event::Op(session.into(), Op::State(state))])
    }

    /// The first subscriber of a session from history: `session/load` makes
    /// agy replay it as updates (no model call).
    pub fn load_session(&mut self, session: &str, cwd: &str) -> Result<(), &'static str> {
        if self.sessions.contains_key(session) || self.resuming.contains(session) {
            return Ok(());
        }
        let pending = Pending::Load {
            session: session.into(),
        };
        self.call("session/load", open_params(session, cwd), pending)?;
        self.resuming.insert(session.into());
        let sess = Sess {
            cwd: cwd.into(),
            loading: true,
            ..Default::default()
        };
        self.sessions.insert(session.into(), sess);
        Ok(())
    }

    /// `submit_prompt` on an open session: a new turn, or a steer of the
    /// running one (agy merges it).
    pub fn prompt(
        &mut self,
        session: &str,
        prompt: &Prompt,
        tag: &str,
    ) -> Result<Vec<Event>, &'static str> {
        let mut out = Vec::new();
        self.send_prompt(session, prompt, tag, &mut out)?;
        Ok(out)
    }

    /// `interrupt`: `session/cancel` (a notification). Open approvals are
    /// answered `cancelled` as ACP asks; the command ends with the turns.
    pub fn interrupt(&mut self, session: &str, tag: &str) -> Result<Vec<Event>, &'static str> {
        let sess = self.sessions.get(session).ok_or("unknown session")?;
        if sess.turns.is_empty() {
            return Ok(vec![done(tag.into(), "completed", None)]);
        }
        let frame = notification(
            "session/cancel",
            json::obj(vec![("sessionId", json::s(session))]),
        );
        self.io.try_send(frame)?;
        let mut out = vec![Event::Progress {
            tag: tag.into(),
            status: "accepted",
        }];
        let asks: Vec<String> = self
            .asks
            .iter()
            .filter(|(_, a)| a.session == session)
            .map(|(k, _)| k.clone())
            .collect();
        for perm in asks {
            if let Some(ask) = self.asks.get(&perm) {
                let cancelled = json::obj(vec![("outcome", json::s("cancelled"))]);
                self.send(reply(&ask.id, json::obj(vec![("outcome", cancelled)])));
            }
            self.withdraw(&perm, ToolStatus::Cancelled, &mut out);
        }
        if let Some(sess) = self.sessions.get_mut(session) {
            sess.interrupts.push(tag.into());
        }
        self.timers.push((tag.into(), now_ms() + self.interrupt_ms));
        Ok(out)
    }

    /// `set_model`: `session/set_model` (agy's model ids).
    pub fn set_model(&mut self, session: &str, model: &str, tag: &str) -> Result<(), &'static str> {
        if !self.sessions.contains_key(session) {
            return Err("unknown session");
        }
        let pending = Pending::SetModel {
            tag: tag.into(),
            session: session.into(),
            model: model.into(),
            then: None,
        };
        self.call("session/set_model", model_params(session, model), pending)
    }

    /// `set_permission_mode` with the phone's name (`MODES`).
    pub fn set_mode(&mut self, session: &str, mode: &str, tag: &str) -> Result<(), &'static str> {
        let native = MODES
            .iter()
            .find(|(phone, _)| *phone == mode)
            .map(|(_, native)| *native)
            .ok_or("mode must be default, acceptEdits or plan")?;
        if !self.sessions.contains_key(session) {
            return Err("unknown session");
        }
        let params = json::obj(vec![
            ("sessionId", json::s(session)),
            ("modeId", json::s(native)),
        ]);
        let pending = Pending::SetMode {
            tag: tag.into(),
            session: session.into(),
            mode: mode.into(),
        };
        self.call("session/set_mode", params, pending)
    }

    /// `session_end`: `session/close`; the session ends with the reply.
    pub fn close(&mut self, session: &str, tag: &str) -> Result<(), &'static str> {
        if !self.sessions.contains_key(session) {
            return Err("unknown session");
        }
        let pending = Pending::Close {
            tag: tag.into(),
            session: session.into(),
        };
        let params = json::obj(vec![("sessionId", json::s(session))]);
        self.call("session/close", params, pending)
    }

    /// `session/list` for one workspace; answered as [`Event::Listed`].
    pub fn list(&mut self, cwd: &str) -> Result<(), &'static str> {
        let params = json::obj(vec![("cwd", json::s(cwd))]);
        self.call("session/list", params, Pending::List { cwd: cwd.into() })
    }

    /// `permission_decision`, after the engine validated it (G2).
    pub fn decide(
        &mut self,
        permission_id: &str,
        decision: &str,
        option_id: Option<&str>,
    ) -> Result<Vec<Event>, &'static str> {
        let ask = self
            .asks
            .get(permission_id)
            .ok_or("request is no longer pending")?;
        let option = match option_id {
            Some(id) => ask
                .options
                .iter()
                .find(|o| o.id == id)
                .ok_or("unknown option")?,
            None => ask
                .options
                .iter()
                .find(|o| o.decision == decision)
                .ok_or("decision is not offered")?,
        };
        if option.decision != decision {
            return Err("option does not match the decision");
        }
        let outcome = if option.id == CANCEL_OPTION {
            json::obj(vec![("outcome", json::s("cancelled"))])
        } else {
            json::obj(vec![
                ("outcome", json::s("selected")),
                ("optionId", json::s(&option.id)),
            ])
        };
        self.io
            .try_send(reply(&ask.id, json::obj(vec![("outcome", outcome)])))?;
        let ask = self.asks.remove(permission_id).expect("looked up above");
        let status = if decision == "reject" {
            ToolStatus::Denied
        } else {
            ToolStatus::Running
        };
        let mut out = Vec::new();
        self.set_card(&ask.session, &ask.item, status, &mut out);
        let state = self.state(&ask.session);
        out.extend(state);
        Ok(out)
    }

    /// The interrupt timers, run by the runtime between child events.
    pub fn tick(&mut self, now: i64) -> Vec<Event> {
        let (late, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.timers)
            .into_iter()
            .partition(|t| now >= t.1);
        self.timers = kept;
        let mut out = Vec::new();
        for (tag, _) in late {
            let mut open = false;
            for sess in self.sessions.values_mut() {
                let before = sess.interrupts.len();
                sess.interrupts.retain(|t| *t != tag);
                open |= sess.interrupts.len() < before;
            }
            if open {
                let why = "no turn end from Antigravity within 30 s".to_string();
                out.push(done(tag, "unknown", Some(why)));
            }
        }
        out
    }

    pub fn owns_session(&self, id: &str) -> bool {
        self.sessions.contains_key(id) || self.resuming.contains(id)
    }

    pub fn knows_permission(&self, id: &str) -> bool {
        self.asks.contains_key(id)
    }

    /// Nothing in flight: no request, turn, approval or replay.
    pub fn is_quiet(&self) -> bool {
        self.pending.is_empty()
            && self.asks.is_empty()
            && self.resuming.is_empty()
            && self.sessions.values().all(|s| s.turns.is_empty())
    }

    /// The models agy offered, (id, label); empty before the first session.
    pub fn models(&self) -> &[(String, String)] {
        &self.models
    }

    pub fn session_ids(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    pub fn malformed(&self) -> u64 {
        self.malformed
    }

    pub fn stderr_tail(&self) -> String {
        self.io.stderr_tail()
    }

    /// Graceful exit: agy sees EOF on stdin. `quiet` = an idle close, whose
    /// exit is not an error.
    pub fn shutdown(&mut self, quiet: bool) {
        self.quiet = quiet;
        self.io.close_stdin();
    }

    /// Feeds one event from the `ChildIo` receiver.
    pub fn on_event(&mut self, event: ChildEvent) -> Vec<Event> {
        let mut out = Vec::new();
        match event {
            ChildEvent::Line(line) => self.line(&line, &mut out),
            ChildEvent::Skipped(_) => {
                self.malformed += 1;
                let ids: Vec<String> = self.sessions.keys().cloned().collect();
                for s in ids {
                    let text = "Antigravity sent a message over 16 MiB; it was skipped";
                    out.push(self.notice(&s, "warning", text));
                }
            }
            ChildEvent::Exit(code) => self.exit(code, &mut out),
        }
        out
    }

    /* -------------------------- requests -------------------------- */

    /// A fresh item or turn number. Scoped by the process generation: after a
    /// respawn the hub still holds the earlier process's items, by id.
    fn next(&mut self) -> String {
        self.seq += 1;
        format!("{}.{}", self.generation, self.seq)
    }

    fn alloc(&mut self, pending: Pending) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, pending);
        id
    }

    fn call(&mut self, method: &str, params: Json, pending: Pending) -> Result<(), &'static str> {
        if self.exited {
            return Err("Antigravity has exited");
        }
        let frame = request(self.next_id, method, params);
        if self.ready {
            self.io.try_send(frame)?;
        } else {
            self.queued.push(frame);
        }
        self.alloc(pending);
        Ok(())
    }

    fn send(&self, frame: String) {
        // a full or closed input means agy is stuck or gone; Exit settles the rest
        let _ = self.io.try_send(frame);
    }

    fn send_prompt(
        &mut self,
        session: &str,
        prompt: &Prompt,
        tag: &str,
        out: &mut Vec<Event>,
    ) -> Result<(), &'static str> {
        let sess = self.sessions.get(session).ok_or("unknown session")?;
        if sess.loading {
            return Err("Antigravity is still loading this session; send again in a moment");
        }
        let mut blocks = vec![json::obj(vec![
            ("type", json::s("text")),
            ("text", json::s(&prompt.text)),
        ])];
        for im in &prompt.images {
            blocks.push(json::obj(vec![
                ("type", json::s("image")),
                ("mimeType", json::s(&im.media_type)),
                ("data", json::s(&im.data)),
            ]));
        }
        let params = json::obj(vec![
            ("sessionId", json::s(session)),
            ("prompt", Json::Arr(blocks)),
        ]);
        let pending = Pending::Prompt {
            tag: tag.into(),
            session: session.into(),
        };
        self.call("session/prompt", params, pending)?;
        let n = self.next();
        let turn = TurnState {
            id: format!("turn:{n}"),
            status: "running",
            started_at: now_ms(),
            ..Default::default()
        };
        let mut user = new_item(&format!("user:{n}"), ItemKind::User, Some(turn.id.clone()));
        user.text = redact(&prompt.text);
        user.images = prompt.images.len() as u32;
        // a steer ends the turn before it: its streaming item is complete
        self.close_open(session, out);
        out.push(Event::Op(session.into(), Op::Item(user)));
        out.push(Event::Op(session.into(), Op::Turn(turn.clone())));
        let sess = self.sessions.get_mut(session).expect("looked up above");
        sess.turns.push(Turn {
            tag: tag.into(),
            state: turn,
        });
        out.push(Event::Progress {
            tag: tag.into(),
            status: "running",
        });
        out.extend(self.state(session));
        Ok(())
    }

    /* --------------------------- input ---------------------------- */

    fn line(&mut self, line: &str, out: &mut Vec<Event>) {
        let Ok(msg) = json::parse(line) else {
            self.malformed += 1;
            return;
        };
        let params = json::get(&msg, "params").unwrap_or(&Json::Null);
        match (json::get_str(&msg, "method"), json::get(&msg, "id")) {
            (Some(method), Some(id)) => self.agent_request(method, id, params, out),
            (Some(method), None) => self.notification(method, params, out),
            (None, Some(Json::Int(id))) => {
                let res = match json::get(&msg, "error") {
                    Some(e) => Err(json::get_str(e, "message")
                        .unwrap_or("Antigravity returned an error")
                        .to_string()),
                    None => Ok(json::get(&msg, "result").cloned().unwrap_or(Json::Null)),
                };
                self.response(*id, res, out);
            }
            _ => self.malformed += 1,
        }
    }

    fn response(&mut self, id: i64, res: Result<Json, String>, out: &mut Vec<Event>) {
        let Some(pending) = self.pending.remove(&id) else {
            return;
        };
        let failed = |tag: String, error: String| done(tag, "failed", Some(redact(&error)));
        match (pending, res) {
            (Pending::Init, _) => {
                // a refused initialize moves on too: later requests fail on their own
                self.ready = true;
                for frame in std::mem::take(&mut self.queued) {
                    self.send(frame);
                }
            }
            (
                Pending::New {
                    tag,
                    cwd,
                    prompt,
                    model,
                },
                Ok(r),
            ) => {
                let Some(session) = json::get_str(&r, "sessionId").map(str::to_string) else {
                    out.push(failed(tag, "session/new returned no session".into()));
                    return;
                };
                out.push(Event::SessionStarted {
                    tag: tag.clone(),
                    session: session.clone(),
                });
                self.opened(&session, cwd, &r, out);
                match model {
                    Some(model) => {
                        let pending = Pending::SetModel {
                            tag: tag.clone(),
                            session: session.clone(),
                            model: model.clone(),
                            then: Some(prompt),
                        };
                        if let Err(e) =
                            self.call("session/set_model", model_params(&session, &model), pending)
                        {
                            out.push(failed(tag, e.into()));
                        }
                    }
                    None => self.prompt_or_fail(&session, &prompt, tag, out),
                }
            }
            (
                Pending::Resume {
                    tag,
                    session,
                    cwd,
                    prompt,
                },
                res,
            ) => {
                self.resuming.remove(&session);
                match res {
                    Ok(r) => {
                        self.opened(&session, cwd, &r, out);
                        self.prompt_or_fail(&session, &prompt, tag, out);
                    }
                    Err(e) => {
                        let why = format!("Antigravity could not resume this session: {e}");
                        out.push(failed(tag, why.clone()));
                        self.closed(session, &why, "ended", out);
                    }
                }
            }
            (Pending::Load { session }, res) => {
                self.resuming.remove(&session);
                match res {
                    Ok(r) => {
                        self.close_open(&session, out);
                        let cwd = self
                            .sessions
                            .get(&session)
                            .map(|s| s.cwd.clone())
                            .unwrap_or_default();
                        self.opened(&session, cwd, &r, out);
                    }
                    Err(e) => {
                        self.sessions.remove(&session);
                        let why = format!("Antigravity could not load this session: {e}");
                        out.push(self.notice(&session, "error", &why));
                    }
                }
            }
            (
                Pending::SetModel {
                    tag,
                    session,
                    model,
                    then,
                },
                res,
            ) => {
                let ok = res.is_ok();
                if let (true, Some(s)) = (ok, self.sessions.get_mut(&session)) {
                    s.model = Some(model.clone());
                }
                match (then, res) {
                    (Some(prompt), res) => {
                        if let Err(e) = res {
                            let why = format!("Could not switch to {model}: {e}");
                            out.push(self.notice(&session, "warning", &why));
                        }
                        self.prompt_or_fail(&session, &prompt, tag, out);
                    }
                    (None, Ok(_)) => {
                        out.extend(self.state(&session));
                        out.push(done(tag, "completed", None));
                    }
                    (None, Err(e)) => out.push(failed(tag, e)),
                }
            }
            (Pending::SetMode { tag, session, mode }, Ok(_)) => {
                if let Some(s) = self.sessions.get_mut(&session) {
                    s.mode = Some(mode);
                }
                out.extend(self.state(&session));
                out.push(done(tag, "completed", None));
            }
            (Pending::Prompt { tag, session }, res) => self.turn_ended(&session, &tag, res, out),
            (Pending::Close { tag, session }, _) => {
                // closed or not, the phone is done with it
                self.end_session(&session, out);
                let state = LiveState {
                    status: "ended",
                    ..Default::default()
                };
                out.push(Event::Op(session, Op::State(state)));
                out.push(done(tag, "completed", None));
            }
            (Pending::List { cwd }, Ok(r)) => {
                let sessions = json::get(&r, "sessions")
                    .and_then(json::as_arr)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(listed)
                    .take(LISTED_MAX)
                    .collect();
                out.push(Event::Listed { cwd, sessions });
            }
            (pending, Err(e)) => {
                if let Some(tag) = pending.tag() {
                    out.push(failed(tag.into(), e));
                }
            }
        }
    }

    fn prompt_or_fail(
        &mut self,
        session: &str,
        prompt: &Prompt,
        tag: String,
        out: &mut Vec<Event>,
    ) {
        if let Err(e) = self.send_prompt(session, prompt, &tag, out) {
            out.push(done(tag, "failed", Some(e.into())));
        }
    }

    /// A session agy opened (new, resumed or loaded): its models, mode and
    /// current model from the reply, then its state.
    fn opened(&mut self, session: &str, cwd: String, r: &Json, out: &mut Vec<Event>) {
        let models = json::get(r, "models");
        let offered: Vec<(String, String)> = models
            .and_then(|m| json::get(m, "availableModels"))
            .and_then(json::as_arr)
            .unwrap_or(&[])
            .iter()
            .filter_map(|m| {
                let id = json::get_str(m, "modelId")?;
                let label = json::get_str(m, "name").unwrap_or(id);
                Some((id.to_string(), label.to_string()))
            })
            .collect();
        if !offered.is_empty() {
            self.models = offered;
        }
        let sess = self.sessions.entry(session.into()).or_default();
        sess.cwd = cwd;
        sess.loading = false;
        if sess.model.is_none() {
            sess.model = models
                .and_then(|m| json::get_str(m, "currentModelId"))
                .map(str::to_string);
        }
        let mode = json::get(r, "modes").and_then(|m| json::get_str(m, "currentModeId"));
        if let Some(phone) = mode.and_then(phone_mode) {
            sess.mode = Some(phone.into());
        }
        out.extend(self.state(session));
    }

    /// One `session/prompt` reply: its turn ends; when it was the last one
    /// the session idles, open approvals go stale and interrupts are done.
    fn turn_ended(
        &mut self,
        session: &str,
        tag: &str,
        res: Result<Json, String>,
        out: &mut Vec<Event>,
    ) {
        // gone (closed, or agy exited): its turns were settled then
        let Some(sess) = self.sessions.get_mut(session) else {
            return;
        };
        let Some(at) = sess.turns.iter().position(|t| t.tag == tag) else {
            return;
        };
        let Turn { tag, mut state } = sess.turns.remove(at);
        let (status, error) = match &res {
            Ok(r) => match json::get_str(r, "stopReason") {
                Some("cancelled") => ("interrupted", None),
                Some("refusal") => ("failed", Some("the model refused to continue".to_string())),
                _ => ("completed", None),
            },
            Err(e) => ("failed", Some(redact(e))),
        };
        state.status = status;
        state.ended_at = Some(now_ms());
        state.error = error.clone();
        if let Some(u) = res.as_ref().ok().and_then(|r| json::get(r, "usage")) {
            state.usage = Some(Usage {
                input: json::get_i64(u, "inputTokens").unwrap_or(0),
                output: json::get_i64(u, "outputTokens").unwrap_or(0),
                cache_read: json::get_i64(u, "cachedReadTokens"),
                reasoning: json::get_i64(u, "thoughtTokens"),
            });
        }
        let last = sess.turns.is_empty();
        let interrupts = if last {
            std::mem::take(&mut sess.interrupts)
        } else {
            Vec::new()
        };
        self.close_open(session, out);
        if last {
            self.settle(session, out);
        }
        out.push(Event::Op(session.into(), Op::Turn(state)));
        let outcome = match status {
            "completed" => done(tag, "completed", None),
            "interrupted" => done(tag, "cancelled", None),
            _ => done(tag, "failed", error),
        };
        out.push(outcome);
        for t in interrupts {
            out.push(done(t, "completed", None));
        }
        out.extend(self.state(session));
    }

    /// No turn runs any more: open approvals go stale, unsettled tool cards
    /// are cancelled.
    fn settle(&mut self, session: &str, out: &mut Vec<Event>) {
        let asks: Vec<String> = self
            .asks
            .iter()
            .filter(|(_, a)| a.session == session)
            .map(|(k, _)| k.clone())
            .collect();
        for perm in asks {
            self.withdraw(&perm, ToolStatus::Cancelled, out);
        }
        let Some(sess) = self.sessions.get_mut(session) else {
            return;
        };
        for (_, mut it) in sess.tools.drain() {
            if let Some(t) = it.tool.as_mut()
                && matches!(
                    t.status,
                    ToolStatus::Pending | ToolStatus::Running | ToolStatus::AwaitingApproval
                )
            {
                t.status = ToolStatus::Cancelled;
                out.push(Event::Op(session.into(), Op::Item(it)));
            }
        }
    }

    /* ----------------------- agy's requests ----------------------- */

    fn agent_request(&mut self, method: &str, id: &Json, p: &Json, out: &mut Vec<Event>) {
        let session = json::get_str(p, "sessionId")
            .filter(|s| self.sessions.contains_key(*s))
            .map(str::to_string);
        match (method, session) {
            ("session/request_permission", Some(session)) => self.ask(id, p, &session, out),
            ("session/request_permission", None) => {
                self.send(reply_error(id, -32602, "unknown session"))
            }
            // no client fs or terminal was offered (initialize)
            _ => self.send(reply_error(
                id,
                -32601,
                &format!("Method not found: {method}"),
            )),
        }
    }

    /// An approval: a `PermissionRequest` for the engine plus its tool card.
    fn ask(&mut self, id: &Json, p: &Json, session: &str, out: &mut Vec<Event>) {
        let rid = match id {
            Json::Int(n) => n.to_string(),
            Json::Str(s) => s.clone(),
            _ => return self.send(reply_error(id, -32600, "bad id")),
        };
        let call = json::get(p, "toolCall").unwrap_or(&Json::Null);
        let Some(item) = json::get_str(call, "toolCallId").map(str::to_string) else {
            return self.send(reply_error(id, -32602, "toolCall without toolCallId"));
        };
        let perm = format!("{}:{}-{rid}", self.adapter, self.generation);
        let options = options(
            json::get(p, "options")
                .and_then(json::as_arr)
                .unwrap_or(&[]),
        );
        // the card as agy describes it, merged into what we have
        self.tool_update(session, call, out);
        let card = self
            .sessions
            .get(session)
            .and_then(|s| s.tools.get(&item))
            .and_then(|it| it.tool.clone())
            .unwrap_or_else(|| new_tool("tool", ToolStatus::Pending));
        let now = now_ms();
        let kind = json::get_str(call, "kind").unwrap_or("other");
        let command = command_line(call);
        let (action, risk, title) = match kind {
            "execute" => (
                "shell",
                "high",
                "Antigravity wants to run a command".to_string(),
            ),
            "edit" | "delete" | "move" => (
                "file-write",
                "medium",
                "Antigravity wants to change a file".to_string(),
            ),
            "fetch" => (
                "network",
                "medium",
                "Antigravity wants to fetch a URL".to_string(),
            ),
            _ => (
                "agent-control",
                "low",
                format!("Antigravity wants to use {}", card.name),
            ),
        };
        let shown = command
            .clone()
            .or_else(|| card.title.clone())
            .unwrap_or_default();
        let cwd = self.sessions.get(session).map(|s| s.cwd.clone());
        let detail = match (kind, &command) {
            ("execute", Some(c)) => Some(detail_shell(c, cwd.as_deref(), "")),
            ("edit" | "delete" | "move", _) if !card.diffs.is_empty() => {
                Some(detail_edit(&card.diffs))
            }
            _ => None,
        };
        let pr = PermissionRequest {
            id: perm.clone(),
            native_request_id: rid,
            session_id: session.into(),
            agent_id: self.adapter.clone(),
            host_id: self.adapter.clone(),
            action: action.into(),
            title,
            command: clip(&redact(&shown), COMMAND_CAP).0,
            args: Vec::new(),
            directory: cwd,
            risk: risk.into(),
            status: "pending".into(),
            supported_decisions: legacy(&options),
            revision: 1,
            fingerprint: None,
            requested_at: now,
            // `Engine::ingest_permission` sets the runtime TTL (ADR-015 §4.1)
            expires_at: now,
            context: String::new(),
            decided_at: None,
            decision: None,
            tool_item_id: Some(item.clone()),
            options: options.clone(),
            detail,
            from_pc: false,
        };
        // ingest first, then the card that points at it (§4.5)
        out.push(Event::Permission(Box::new(pr)));
        if let Some(it) = self
            .sessions
            .get_mut(session)
            .and_then(|s| s.tools.get_mut(&item))
            && let Some(t) = it.tool.as_mut()
        {
            t.status = ToolStatus::AwaitingApproval;
            t.permission_id = Some(perm.clone());
            out.push(Event::Op(session.into(), Op::Item(it.clone())));
        }
        let ask = Ask {
            id: id.clone(),
            session: session.into(),
            item,
            options,
        };
        self.asks.insert(perm, ask);
        out.extend(self.state(session));
    }

    /// Never answered by us afterwards; the card takes `status`.
    fn withdraw(&mut self, perm: &str, status: ToolStatus, out: &mut Vec<Event>) {
        let Some(ask) = self.asks.remove(perm) else {
            return;
        };
        out.push(Event::Withdrawn(perm.into()));
        self.set_card(&ask.session, &ask.item, status, out);
    }

    fn set_card(&mut self, session: &str, item: &str, status: ToolStatus, out: &mut Vec<Event>) {
        if let Some(it) = self
            .sessions
            .get_mut(session)
            .and_then(|s| s.tools.get_mut(item))
            && let Some(t) = it.tool.as_mut()
        {
            t.status = status;
            out.push(Event::Op(session.into(), Op::Item(it.clone())));
        }
    }

    /* ------------------------ notifications ----------------------- */

    fn notification(&mut self, method: &str, p: &Json, out: &mut Vec<Event>) {
        if method != "session/update" {
            return;
        }
        let Some(session) = json::get_str(p, "sessionId")
            .filter(|s| self.sessions.contains_key(*s))
            .map(str::to_string)
        else {
            return;
        };
        let Some(u) = json::get(p, "update") else {
            return;
        };
        let text = || {
            json::get(u, "content")
                .filter(|c| json::get_str(c, "type") == Some("text"))
                .and_then(|c| json::get_str(c, "text"))
                .unwrap_or("")
        };
        match json::get_str(u, "sessionUpdate").unwrap_or("") {
            "agent_message_chunk" => self.stream(&session, ItemKind::Text, text(), out),
            "agent_thought_chunk" => self.stream(&session, ItemKind::Thinking, text(), out),
            // live: the bubble was made when the prompt went out
            "user_message_chunk" if self.loading(&session) => {
                self.stream(&session, ItemKind::User, text(), out)
            }
            "tool_call" | "tool_call_update" => {
                self.close_open(&session, out);
                self.tool_update(&session, u, out);
            }
            "plan" => {
                let turn = self.current_turn(&session);
                let id = format!("plan:{}", turn.as_deref().unwrap_or("history"));
                let mut plan = new_item(&id, ItemKind::Plan, turn);
                plan.steps = json::get(u, "entries")
                    .and_then(json::as_arr)
                    .unwrap_or(&[])
                    .iter()
                    .map(|e| PlanStep {
                        text: redact(json::get_str(e, "content").unwrap_or("")),
                        status: match json::get_str(e, "status") {
                            Some("in_progress") => "in_progress",
                            Some("completed") => "completed",
                            _ => "pending",
                        },
                    })
                    .collect();
                out.push(Event::Op(session, Op::Item(plan)));
            }
            "available_commands_update" => {
                let names = json::get(u, "availableCommands")
                    .and_then(json::as_arr)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|c| json::get_str(c, "name"))
                    .filter(|n| is_command(n))
                    .map(str::to_string)
                    .collect();
                if let Some(s) = self.sessions.get_mut(&session) {
                    s.commands = names;
                }
                out.extend(self.state(&session));
            }
            "current_mode_update" => {
                let mode = json::get_str(u, "currentModeId").and_then(phone_mode);
                if let (Some(mode), Some(s)) = (mode, self.sessions.get_mut(&session)) {
                    s.mode = Some(mode.into());
                }
                out.extend(self.state(&session));
            }
            // config_option_update, session_info_update (agy's own title): not shown
            _ => {}
        }
    }

    fn loading(&self, session: &str) -> bool {
        self.sessions.get(session).is_some_and(|s| s.loading)
    }

    fn current_turn(&self, session: &str) -> Option<String> {
        let sess = self.sessions.get(session)?;
        sess.turns.last().map(|t| t.state.id.clone())
    }

    /// A text, thinking or (history) user chunk: grows the open item of that
    /// kind, or closes the open one and starts a new item.
    fn stream(&mut self, session: &str, kind: ItemKind, delta: &str, out: &mut Vec<Event>) {
        if delta.is_empty() {
            return;
        }
        if self
            .sessions
            .get(session)
            .and_then(|s| s.open.as_ref())
            .is_some_and(|o| o.kind != kind)
        {
            self.close_open(session, out);
        }
        let turn = self.current_turn(session);
        let seq = self.next();
        let sess = self
            .sessions
            .get_mut(session)
            .expect("checked by the caller");
        let cap = if kind == ItemKind::Thinking {
            THINKING_CAP
        } else {
            TEXT_CAP
        };
        match sess.open.as_mut() {
            Some(open) => {
                if open.text.len() + delta.len() <= cap {
                    open.text.push_str(delta);
                } else {
                    open.truncated = true;
                }
                let op = Op::Append {
                    id: open.id.clone(),
                    field: Field::Text,
                    text: redact(delta),
                };
                out.push(Event::Op(session.into(), op));
            }
            None => {
                let prefix = match kind {
                    ItemKind::Thinking => "thinking",
                    ItemKind::User => "user",
                    _ => "text",
                };
                let mut it = new_item(&format!("{prefix}:{seq}"), kind, turn);
                (it.text, it.truncated) = clip(delta, cap);
                let mut shown = it.clone();
                shown.text = redact(&shown.text);
                out.push(Event::Op(session.into(), Op::Item(shown)));
                sess.open = Some(it);
            }
        }
    }

    /// The open streaming item is complete: its final text, redacted whole.
    fn close_open(&mut self, session: &str, out: &mut Vec<Event>) {
        let Some(mut it) = self.sessions.get_mut(session).and_then(|s| s.open.take()) else {
            return;
        };
        it.done = it.kind != ItemKind::User || it.done;
        it.text = redact(&it.text);
        out.push(Event::Op(session.into(), Op::Item(it)));
    }

    /// `tool_call` / `tool_call_update` (and the toolCall of an approval):
    /// merged into the session's card for that toolCallId.
    fn tool_update(&mut self, session: &str, u: &Json, out: &mut Vec<Event>) {
        let Some(id) = json::get_str(u, "toolCallId") else {
            return;
        };
        let turn = self.current_turn(session);
        let Some(sess) = self.sessions.get_mut(session) else {
            return;
        };
        let it = sess.tools.entry(id.into()).or_insert_with(|| {
            let mut it = new_item(id, ItemKind::Tool, turn);
            it.tool = Some(new_tool("tool", ToolStatus::Pending));
            it
        });
        let t = it.tool.as_mut().expect("tool items carry a tool");
        merge_tool(t, u);
        let it = it.clone();
        let settled = it.tool.as_ref().is_some_and(|t| {
            matches!(
                t.status,
                ToolStatus::Completed | ToolStatus::Failed | ToolStatus::Denied
            )
        });
        if settled {
            sess.tools.remove(id);
        }
        out.push(Event::Op(session.into(), Op::Item(it)));
    }

    /// The session's live state as an op.
    fn state(&self, session: &str) -> Option<Event> {
        let sess = self.sessions.get(session)?;
        let waiting = self.asks.values().any(|a| a.session == session);
        let status = match (sess.loading, waiting, sess.turns.is_empty()) {
            (true, _, _) => "starting",
            (_, true, _) => "waiting",
            (_, _, false) => "running",
            _ => "idle",
        };
        let state = LiveState {
            status,
            permission_mode: sess.mode.clone(),
            model: sess.model.clone(),
            models: self.models.clone(),
            commands: sess.commands.clone(),
            context: None,
        };
        Some(Event::Op(session.into(), Op::State(state)))
    }

    fn notice(&mut self, session: &str, level: &'static str, text: &str) -> Event {
        let n = self.next();
        let turn = self.current_turn(session);
        let mut it = new_item(&format!("notice:{n}"), ItemKind::Notice, turn);
        it.level = Some(level);
        it.text = redact(text);
        Event::Op(session.into(), Op::Item(it))
    }

    /// The session is over for this process: open items close, approvals
    /// go stale, turns and their commands end `failed` with `why`.
    fn drop_session(&mut self, session: &str, why: &str, out: &mut Vec<Event>) {
        self.close_open(session, out);
        self.settle(session, out);
        let Some(mut sess) = self.sessions.remove(session) else {
            return;
        };
        // their turns end here; a late reply finds no session
        self.pending
            .retain(|_, p| !matches!(p, Pending::Prompt { session: s, .. } if s == session));
        for mut t in sess.turns.drain(..) {
            (t.state.status, t.state.ended_at, t.state.error) =
                ("failed", Some(now_ms()), Some(why.into()));
            out.push(Event::Op(session.into(), Op::Turn(t.state)));
            out.push(done(t.tag, "failed", Some(why.into())));
        }
        for tag in sess.interrupts {
            out.push(done(tag, "unknown", Some(why.into())));
        }
    }

    fn end_session(&mut self, session: &str, out: &mut Vec<Event>) {
        self.drop_session(session, "the session ended", out);
    }

    fn exit(&mut self, code: Option<i32>, out: &mut Vec<Event>) {
        self.exited = true;
        let why = code.map_or_else(
            || "Antigravity exited".to_string(),
            |c| format!("Antigravity exited with code {c}"),
        );
        self.timers.clear();
        let prompts = |p: &Pending| matches!(p, Pending::Prompt { .. });
        for (_, pending) in self.pending.extract_if(|_, p| !prompts(p)) {
            if let Some(tag) = pending.tag() {
                out.push(done(tag.into(), "failed", Some(why.clone())));
            }
        }
        // agy keeps its conversations on disk: the next prompt resumes them
        let resumable = format!("{why}; the next prompt resumes this session");
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for session in ids {
            self.drop_session(&session, &why, out);
            if self.quiet {
                let state = LiveState::default();
                out.push(Event::Op(session, Op::State(state)));
            } else {
                self.closed(session, &resumable, "idle", out);
            }
        }
        for session in std::mem::take(&mut self.resuming) {
            self.closed(session, &resumable, "idle", out);
        }
    }

    /// An error notice, then the session's state.
    fn closed(&mut self, session: String, why: &str, status: &'static str, out: &mut Vec<Event>) {
        out.push(self.notice(&session, "error", why));
        let state = LiveState {
            status,
            ..Default::default()
        };
        out.push(Event::Op(session, Op::State(state)));
    }
}

/* ------------------------------------------------------------------ */
/* Decoding helpers                                                    */
/* ------------------------------------------------------------------ */

fn done(tag: String, status: &'static str, error: Option<String>) -> Event {
    Event::Done { tag, status, error }
}

fn phone_mode(native: &str) -> Option<&'static str> {
    MODES.iter().find(|(_, n)| *n == native).map(|(p, _)| *p)
}

/// A slash command name the phone may show: what `TRANSCRIPT` caps allow.
fn is_command(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_:.".contains(&b))
}

/// `/C:/x/y` (how agy writes Windows paths in URIs) → `C:/x/y`.
pub fn native_path(p: &str) -> &str {
    let b = p.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        &p[1..]
    } else {
        p
    }
}

fn listed(s: &Json) -> Option<Listed> {
    let id = json::get_str(s, "sessionId")?;
    if id.is_empty() || id.len() > 128 {
        return None;
    }
    let updated = json::get_str(s, "updatedAt")
        .and_then(crate::claude_history::iso_ms)
        .unwrap_or(0);
    Some(Listed {
        id: id.into(),
        cwd: json::get_str(s, "cwd").unwrap_or("").into(),
        title: json::get_str(s, "title")
            .filter(|t| !t.trim().is_empty())
            .map(|t| clip(&redact(t), 200).0)
            .unwrap_or_else(|| "Antigravity session".into()),
        updated_at: updated,
    })
}

/// The command line of a `run_command` call, when it has one.
fn command_line(call: &Json) -> Option<String> {
    json::get(call, "rawInput")
        .and_then(|r| json::get_str(r, "CommandLine"))
        .map(str::to_string)
}

/// One ACP tool call update folded into a card: fields agy sent replace
/// what the card had.
fn merge_tool(t: &mut ToolCall, u: &Json) {
    if let Some(name) = json::get_str(u, "name").or_else(|| json::get_str(u, "kind")) {
        t.name = clip(name, 64).0;
    }
    let raw = json::get(u, "rawInput").filter(|r| matches!(r, Json::Obj(_)));
    let command = command_line(u);
    let path = raw
        .and_then(|r| json::get_str(r, "Path"))
        .map(|p| native_path(p).to_string());
    if let Some(c) = &command {
        t.title = Some(first_line(&redact(c)));
        t.input = clip(&redact(c), INPUT_CAP).0;
    } else if let Some(raw) = raw {
        t.input = clip(&redact(&json::pretty(raw)), INPUT_CAP).0;
        t.title = path
            .clone()
            .or_else(|| json::get_str(u, "title").map(|s| first_line(&redact(s))));
    } else if let Some(title) = json::get_str(u, "title") {
        t.title = Some(first_line(&redact(title)));
    }
    let content = json::get(u, "content").and_then(json::as_arr);
    let texts: Vec<&str> = content
        .unwrap_or(&[])
        .iter()
        .filter(|c| json::get_str(c, "type") == Some("content"))
        .filter_map(|c| json::get(c, "content").and_then(|c| json::get_str(c, "text")))
        .collect();
    let output = if texts.is_empty() {
        json::get(u, "rawOutput")
            .and_then(|r| json::get_str(r, "output"))
            .unwrap_or("")
            .to_string()
    } else {
        texts.join("\n")
    };
    if !output.is_empty() {
        (t.output, t.output_truncated) = head_tail(&redact(&output), OUTPUT_CAP);
    }
    let diffs: Vec<FileDiff> = content
        .unwrap_or(&[])
        .iter()
        .filter(|c| json::get_str(c, "type") == Some("diff"))
        .filter_map(acp_diff)
        .collect();
    if !diffs.is_empty() {
        t.title = Some(paths_title(&diffs));
        t.diffs = diffs;
    } else if t.diffs.is_empty()
        && json::get_str(u, "kind") == Some("edit")
        && let Some(raw) = raw
    {
        // before the edit runs: the change as agy described it
        t.diffs = proposed_diff(raw, path.as_deref()).into_iter().collect();
    }
    let status = match json::get_str(u, "status") {
        Some("pending") => Some(ToolStatus::Pending),
        Some("in_progress") => Some(ToolStatus::Running),
        Some("completed") => Some(ToolStatus::Completed),
        Some("failed") => Some(ToolStatus::Failed),
        _ => None,
    };
    if let Some(s) = status {
        t.status = match (t.status, s) {
            // a denied call ends failed: it stays denied
            (ToolStatus::Denied, _) => ToolStatus::Denied,
            // agy re-sends `pending` with the request: the approval wins
            (ToolStatus::AwaitingApproval, ToolStatus::Pending) => ToolStatus::AwaitingApproval,
            (_, s) => s,
        };
    }
    if t.status == ToolStatus::Failed && t.error.is_none() {
        t.error = json::get(u, "rawOutput")
            .and_then(|r| json::get_str(r, "error"))
            .map(|e| clip(&redact(e), 1024).0);
    }
}

/// An ACP `diff` content block: whole new file, or old → new text.
fn acp_diff(c: &Json) -> Option<FileDiff> {
    let path = native_path(json::get_str(c, "path")?).to_string();
    let new = json::get_str(c, "newText").unwrap_or("");
    Some(match json::get_str(c, "oldText") {
        Some(old) => file_diff(&path, "modified", &replace_patch(old, new)),
        None => file_diff(&path, "added", &added_patch(new)),
    })
}

/// write_file `{Path, Content}` or edit_file `{Path, Target, Replacement}`
/// before it ran.
fn proposed_diff(raw: &Json, path: Option<&str>) -> Option<FileDiff> {
    let path = path?;
    if let Some(content) = json::get_str(raw, "Content") {
        return Some(file_diff(path, "added", &added_patch(content)));
    }
    let target = json::get_str(raw, "Target")?;
    let replacement = json::get_str(raw, "Replacement").unwrap_or("");
    Some(file_diff(
        path,
        "modified",
        &replace_patch(target, replacement),
    ))
}

/// The phone's options for agy's: once-only allow and deny, labelled as agy
/// labels them. `allow_always` / `reject_always` are never offered: their
/// scope is agy's own settings, and the phone grants nothing persistent.
/// Without a deny, "Deny and stop" answers `cancelled`.
fn options(native: &[Json]) -> Vec<PermissionOption> {
    let option = |id: &str, decision: &str, label: &str| PermissionOption {
        id: id.into(),
        decision: decision.into(),
        label: clip(label, 80).0,
        rule: None,
        scope: "once",
        destination: None,
        takes_message: false,
        stops_turn: false,
    };
    let mut out: Vec<PermissionOption> = native
        .iter()
        .filter_map(|o| {
            let id = json::get_str(o, "optionId")?;
            let label = json::get_str(o, "name").unwrap_or(id);
            match json::get_str(o, "kind")? {
                "allow_once" => Some(option(id, "approve-once", label)),
                "reject_once" => Some(option(id, "reject", label)),
                _ => None,
            }
        })
        .filter(|o| o.id != CANCEL_OPTION && o.id.len() <= 128)
        .collect();
    if !out.iter().any(|o| o.decision == "reject") {
        out.push(PermissionOption {
            stops_turn: true,
            ..option(CANCEL_OPTION, "reject", "Deny and stop")
        });
    }
    out
}

/// The phone-side option that answers `cancelled` (never an agy option id).
const CANCEL_OPTION: &str = "chernobyl-cancel";

fn legacy(options: &[PermissionOption]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for o in options {
        if !out.contains(&o.decision) {
            out.push(o.decision.clone());
        }
    }
    out
}

/* ------------------------------------------------------------------ */
/* Frames                                                              */
/* ------------------------------------------------------------------ */

fn request(id: i64, method: &str, params: Json) -> String {
    json::stringify(&json::obj(vec![
        ("jsonrpc", json::s("2.0")),
        ("id", Json::Int(id)),
        ("method", json::s(method)),
        ("params", params),
    ]))
}

fn notification(method: &str, params: Json) -> String {
    json::stringify(&json::obj(vec![
        ("jsonrpc", json::s("2.0")),
        ("method", json::s(method)),
        ("params", params),
    ]))
}

fn reply(id: &Json, result: Json) -> String {
    json::stringify(&json::obj(vec![
        ("jsonrpc", json::s("2.0")),
        ("id", id.clone()),
        ("result", result),
    ]))
}

fn reply_error(id: &Json, code: i64, message: &str) -> String {
    let error = json::obj(vec![
        ("code", Json::Int(code)),
        ("message", json::s(message)),
    ]);
    json::stringify(&json::obj(vec![
        ("jsonrpc", json::s("2.0")),
        ("id", id.clone()),
        ("error", error),
    ]))
}

fn open_params(session: &str, cwd: &str) -> Json {
    json::obj(vec![
        ("sessionId", json::s(session)),
        ("cwd", json::s(cwd)),
        ("mcpServers", Json::Arr(vec![])),
    ])
}

fn model_params(session: &str, model: &str) -> Json {
    json::obj(vec![
        ("sessionId", json::s(session)),
        ("modelId", json::s(model)),
    ])
}

/* ------------------------------------------------------------------ */
/* Launch configuration (ADR-022, the ADR-015 §4.2.3 workspace policy)  */
/* ------------------------------------------------------------------ */

/// An `antigravity` adapter entry: `{"type": "antigravity", "workspaces":
/// [...], "binary"?, "model"?, "maxSessions"?}`.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchConfig {
    pub binary: PathBuf,
    /// Before `acp`. Tests only (`node fake-agy-acp.mjs`); never read from
    /// companion.json, so no flag reaches agy from the file or the phone.
    pub args: Vec<String>,
    pub workspaces: Vec<LiveWorkspace>,
    /// The model a session starts on when the phone picks none.
    pub model: Option<String>,
    pub max_sessions: usize,
}

impl LaunchConfig {
    /// `<binary> <args…> acp` for `ChildIo::spawn` (no shell; prompts go on
    /// stdin, never argv; never `--dangerously-skip-permissions`).
    pub fn argv(&self) -> Vec<String> {
        let mut argv = self.args.clone();
        argv.push("acp".into());
        argv
    }

    /// `Agent.live`: a follow-up steers the running turn (agy merges it).
    pub fn agent_live(&self, models: &[(String, String)]) -> AgentLive {
        let mut composer = vec!["images", "commands", "rename"];
        if !models.is_empty() {
            composer.insert(0, "model");
        }
        AgentLive {
            prompt_while_busy: "merge",
            interrupt: true,
            end: true,
            permission_modes: MODES.iter().map(|(p, _)| p.to_string()).collect(),
            workspaces: self.workspaces.clone(),
            composer,
            models: models.to_vec(),
        }
    }
}

/// Validates an `antigravity` adapter entry. Workspaces follow the Claude
/// policy (`claude_live::validate_workspace`): absolute, existing, canonical
/// folders; never a drive root, the user profile or a folder containing the
/// CHERNOBYL data directory. The phone only ever picks one of them.
pub fn launch_config(entry: &Json, data_dir: &Path) -> Result<LaunchConfig, String> {
    let binary = match json::get(entry, "binary") {
        None | Some(Json::Null) => default_binary(std::env::var_os("LOCALAPPDATA").as_deref()),
        Some(Json::Str(b)) if !b.is_empty() => PathBuf::from(b),
        _ => return Err("antigravity 'binary' must be a non-empty string or null".into()),
    };
    let model = match json::get(entry, "model") {
        None | Some(Json::Null) => None,
        Some(Json::Str(m)) if crate::protocol::is_model_name(m) => Some(m.clone()),
        _ => {
            return Err(
                "antigravity 'model' must be a model id such as gemini-3.8-flash-low".into(),
            );
        }
    };
    let max_sessions = match json::get(entry, "maxSessions") {
        None | Some(Json::Null) => 3,
        Some(Json::Int(n)) if (1..=16).contains(n) => *n as usize,
        _ => return Err("antigravity 'maxSessions' must be 1-16".into()),
    };
    let listed = json::get(entry, "workspaces")
        .and_then(json::as_arr)
        .ok_or("antigravity 'workspaces' must be an array of folders")?;
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let mut workspaces: Vec<LiveWorkspace> = Vec::new();
    for (i, w) in listed.iter().enumerate() {
        let raw = json::as_str(w)
            .ok_or_else(|| format!("antigravity workspaces[{i}] must be a string"))?;
        let ws = crate::claude_live::validate_workspace(raw, data_dir, profile.as_deref())
            .map_err(|e| format!("antigravity workspaces[{i}]: {e}"))?;
        if workspaces.iter().any(|w| w.id == ws.id) {
            return Err(format!(
                "antigravity workspaces[{i}] repeats an earlier folder"
            ));
        }
        workspaces.push(ws);
    }
    if workspaces.is_empty() {
        return Err("antigravity needs at least one workspace".into());
    }
    Ok(LaunchConfig {
        binary,
        args: Vec::new(),
        workspaces,
        model,
        max_sessions,
    })
}

/// The installer's `%LOCALAPPDATA%\agy\bin\agy.exe` when it exists, else
/// `agy.exe` on PATH.
fn default_binary(local: Option<&std::ffi::OsStr>) -> PathBuf {
    local
        .map(|l| Path::new(l).join("agy").join("bin").join("agy.exe"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("agy.exe"))
}

/* ------------------------------------------------------------------ */
/* Tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, BufRead, BufReader, Write};
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);
    const SID: &str = "6b27e1ab-ee11-459e-912b-7b2dfb3a440f";

    /// An in-memory agy over `std::io::pipe()` ends: it reads what the core
    /// writes and writes what agy 1.5.6 said in the live captures.
    struct Peer {
        frames: Receiver<String>,
        to_core: io::PipeWriter,
        events: Receiver<ChildEvent>,
    }

    impl Peer {
        fn recv(&self) -> String {
            self.frames
                .recv_timeout(WAIT)
                .expect("the core wrote a frame")
        }

        fn say(&mut self, line: &str) {
            writeln!(self.to_core, "{line}").unwrap();
        }

        /// Feeds what agy said into the core until it produced `n` events
        /// or more.
        fn feed(&self, agy: &mut Agy) -> Vec<Event> {
            let ev = self.events.recv_timeout(WAIT).expect("a line from agy");
            agy.on_event(ev)
        }

        fn update(&mut self, agy: &mut Agy, update: &str) -> Vec<Event> {
            self.say(&format!(
                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{SID}","update":{update}}}}}"#
            ));
            self.feed(agy)
        }
    }

    fn connect() -> (Agy, Peer) {
        let (core_stdout, to_core) = io::pipe().unwrap();
        let (from_core, core_stdin) = io::pipe().unwrap();
        let (child, events) = ChildIo::from_pipes(core_stdout, core_stdin).unwrap();
        let (frames_tx, frames) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(from_core).lines().map_while(Result::ok) {
                let _ = frames_tx.send(line);
            }
        });
        let mut agy = Agy::new(child, "agy-1", 1, "0.0.0");
        let mut peer = Peer {
            frames,
            to_core,
            events,
        };
        let init = peer.recv();
        assert!(init.contains(r#""method":"initialize""#), "{init}");
        assert!(init.contains(r#""protocolVersion":1"#), "{init}");
        peer.say(r#"{"jsonrpc":"2.0","result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":true}}},"id":1}"#);
        assert!(peer.feed(&mut agy).is_empty());
        (agy, peer)
    }

    const NEW_OK: &str = r#"{"sessionId":"6b27e1ab-ee11-459e-912b-7b2dfb3a440f","models":{"availableModels":[{"modelId":"gemini-3.8-flash-high","name":"Gemini 3.8 Flash (High)"},{"modelId":"gemini-3.8-flash-low","name":"Gemini 3.8 Flash (Low)"}],"currentModelId":"gemini-3.8-flash-high"},"modes":{"currentModeId":"default","availableModes":[]}}"#;

    fn id_of(frame: &str) -> i64 {
        json::get_i64(&json::parse(frame).unwrap(), "id").unwrap()
    }

    fn answer(peer: &mut Peer, frame: &str, result: &str) {
        peer.say(&format!(
            r#"{{"jsonrpc":"2.0","result":{result},"id":{}}}"#,
            id_of(frame)
        ));
    }

    /// session/new → the first prompt is on the wire.
    fn session(model: Option<&str>) -> (Agy, Peer, Vec<Event>) {
        let (mut agy, mut peer) = connect();
        let prompt = Prompt {
            text: "hello".into(),
            images: vec![],
        };
        agy.start_session(r"C:\work", prompt, model.map(str::to_string), "c1")
            .unwrap();
        let new = peer.recv();
        assert!(
            new.contains(r#""method":"session/new","params":{"cwd":"C:\\work","mcpServers":[]}"#),
            "{new}"
        );
        answer(&mut peer, &new, NEW_OK);
        let mut events = peer.feed(&mut agy);
        if let Some(m) = model {
            let set = peer.recv();
            assert!(set.contains(&format!(r#""method":"session/set_model","params":{{"sessionId":"{SID}","modelId":"{m}"}}"#)), "{set}");
            answer(&mut peer, &set, "{}");
            events.extend(peer.feed(&mut agy));
        }
        let prompt = peer.recv();
        assert!(prompt.contains(r#""method":"session/prompt""#), "{prompt}");
        assert!(
            prompt.contains(r#""prompt":[{"type":"text","text":"hello"}]"#),
            "{prompt}"
        );
        (agy, peer, events)
    }

    fn ops(events: &[Event]) -> Vec<&Op> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Op(_, op) => Some(op),
                _ => None,
            })
            .collect()
    }

    fn items(events: &[Event]) -> Vec<&Item> {
        ops(events)
            .into_iter()
            .filter_map(|op| match op {
                Op::Item(it) => Some(it),
                _ => None,
            })
            .collect()
    }

    fn dones(events: &[Event]) -> Vec<(&str, &str)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Done { tag, status, .. } => Some((tag.as_str(), *status)),
                _ => None,
            })
            .collect()
    }

    fn last_state(events: &[Event]) -> &LiveState {
        ops(events)
            .into_iter()
            .rev()
            .find_map(|op| match op {
                Op::State(s) => Some(s),
                _ => None,
            })
            .expect("a state op")
    }

    #[test]
    fn new_session_streams_text_and_ends_the_turn_with_usage() {
        let (mut agy, mut peer, events) = session(Some("gemini-3.8-flash-low"));
        assert!(
            matches!(&events[0], Event::SessionStarted { tag, session } if tag == "c1" && session == SID)
        );
        let state = last_state(&events);
        assert_eq!(state.status, "running");
        assert_eq!(state.model.as_deref(), Some("gemini-3.8-flash-low"));
        assert_eq!(state.permission_mode.as_deref(), Some("default"));
        assert_eq!(state.models.len(), 2);
        assert_eq!(agy.models()[1].0, "gemini-3.8-flash-low");
        let user = items(&events)
            .into_iter()
            .find(|i| i.kind == ItemKind::User)
            .unwrap();
        assert_eq!(user.text, "hello");
        // agy echoes the prompt: no second bubble
        let echo = peer.update(
            &mut agy,
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}"#,
        );
        assert!(echo.is_empty(), "{echo:?}");
        let first = peer.update(
            &mut agy,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hi"}}"#,
        );
        let text = items(&first)[0];
        assert_eq!(
            (text.kind, text.text.as_str(), text.done),
            (ItemKind::Text, "Hi", false)
        );
        let more = peer.update(
            &mut agy,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":" there"}}"#,
        );
        assert!(matches!(ops(&more)[0], Op::Append { text, .. } if text == " there"));
        let prompt_id = 4; // initialize, session/new, set_model, then the prompt
        peer.say(&format!(r#"{{"jsonrpc":"2.0","result":{{"stopReason":"end_turn","usage":{{"inputTokens":24724,"outputTokens":245,"cachedReadTokens":10,"cachedWriteTokens":0,"thoughtTokens":3,"totalTokens":24969}}}},"id":{prompt_id}}}"#));
        let end = peer.feed(&mut agy);
        let text = items(&end)[0];
        assert_eq!((text.text.as_str(), text.done), ("Hi there", true));
        let turn = ops(&end)
            .into_iter()
            .find_map(|op| match op {
                Op::Turn(t) => Some(t),
                _ => None,
            })
            .unwrap();
        assert_eq!(turn.status, "completed");
        let usage = turn.usage.as_ref().unwrap();
        assert_eq!(
            (usage.input, usage.output, usage.cache_read, usage.reasoning),
            (24724, 245, Some(10), Some(3))
        );
        assert_eq!(dones(&end), [("c1", "completed")]);
        assert_eq!(last_state(&end).status, "idle");
        assert!(agy.is_quiet());
    }

    #[test]
    fn tool_cards_carry_command_output_and_edit_diffs() {
        let (mut agy, mut peer, _) = session(None);
        let run = peer.update(&mut agy, r#"{"sessionUpdate":"tool_call","toolCallId":"call_1","name":"run_command","args":{"CommandLine":"hostname"},"status":"in_progress","kind":"execute","title":"hostname","rawInput":{"CommandLine":"hostname","WaitMsBeforeAsync":5000,"toolSummary":"Run hostname command"}}"#);
        let card = items(&run)[0].tool.clone().unwrap();
        assert_eq!(
            (card.name.as_str(), card.status),
            ("run_command", ToolStatus::Running)
        );
        assert_eq!(
            (card.title.as_deref(), card.input.as_str()),
            (Some("hostname"), "hostname")
        );
        let ran = peer.update(&mut agy, r#"{"sessionUpdate":"tool_call_update","toolCallId":"call_1","name":"run_command","status":"completed","kind":"execute","title":"hostname","rawInput":{"CommandLine":"hostname"},"rawOutput":{"output":"Command output:\nDESKTOP-1\n","exit_code":0},"content":[{"type":"content","content":{"type":"text","text":"Command output:\nDESKTOP-1\n"}}]}"#);
        let card = items(&ran)[0].tool.clone().unwrap();
        assert_eq!(card.status, ToolStatus::Completed);
        assert!(card.output.contains("DESKTOP-1"), "{}", card.output);
        // an edit: the proposed change first, agy's diff when done
        let edit = peer.update(&mut agy, r#"{"sessionUpdate":"tool_call","toolCallId":"call_2","name":"edit_file","status":"in_progress","kind":"edit","title":"File out.txt","rawInput":{"Path":"C:\\work\\out.txt","Replacement":"bye","Target":"hello"}}"#);
        let card = items(&edit)[0].tool.clone().unwrap();
        assert_eq!(card.title.as_deref(), Some(r"C:\work\out.txt"));
        assert_eq!(card.diffs[0].patch, "@@ -1 +1 @@\n-hello\n+bye");
        let edited = peer.update(&mut agy, r#"{"sessionUpdate":"tool_call_update","toolCallId":"call_2","name":"edit_file","status":"completed","kind":"edit","title":"File /C:/work/out.txt","rawInput":{"Path":"C:\\work\\out.txt","Replacement":"bye","Target":"hello"},"rawOutput":{},"content":[{"type":"diff","path":"/C:/work/out.txt","newText":"bye\n","oldText":"hello\n"}]}"#);
        let card = items(&edited)[0].tool.clone().unwrap();
        assert_eq!(card.status, ToolStatus::Completed);
        let d = &card.diffs[0];
        assert_eq!(
            (d.path.as_str(), d.status, d.additions, d.deletions),
            ("C:/work/out.txt", "modified", 1, 1)
        );
        // a new file is all added
        let wrote = peer.update(&mut agy, r#"{"sessionUpdate":"tool_call_update","toolCallId":"call_3","name":"write_file","status":"completed","kind":"edit","rawInput":{"Content":"hi","Path":"C:\\work\\a.txt"},"content":[{"type":"diff","path":"/C:/work/a.txt","newText":"hi\n"}]}"#);
        let d = &items(&wrote)[0].tool.as_ref().unwrap().diffs[0];
        assert_eq!(
            (d.status, d.patch.as_str()),
            ("added", "@@ -0,0 +1 @@\n+hi")
        );
        // text after a tool is a new item
        let after = peer.update(
            &mut agy,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}"#,
        );
        assert_eq!(items(&after)[0].kind, ItemKind::Text);
        // thinking, a plan, commands and the mode
        let thought = peer.update(
            &mut agy,
            r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hmm"}}"#,
        );
        let its = items(&thought);
        assert_eq!((its[0].kind, its[0].done), (ItemKind::Text, true));
        assert_eq!(its[1].kind, ItemKind::Thinking);
        let plan = peer.update(&mut agy, r#"{"sessionUpdate":"plan","entries":[{"content":"Read","priority":"high","status":"completed"},{"content":"Edit","priority":"high","status":"in_progress"}]}"#);
        assert_eq!(items(&plan)[0].steps[1].status, "in_progress");
        let cmds = peer.update(&mut agy, r#"{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"compact","description":"x"},{"name":"bad name","description":"y"},{"name":"goal"}]}"#);
        assert_eq!(last_state(&cmds).commands, ["compact", "goal"]);
        let mode = peer.update(
            &mut agy,
            r#"{"sessionUpdate":"current_mode_update","currentModeId":"accept-edits"}"#,
        );
        assert_eq!(
            last_state(&mode).permission_mode.as_deref(),
            Some("acceptEdits")
        );
        // unknown kinds and other sessions are ignored
        assert!(
            peer.update(
                &mut agy,
                r#"{"sessionUpdate":"session_info_update","title":"x"}"#
            )
            .is_empty()
        );
        peer.say(r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"other","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x"}}}}"#);
        assert!(peer.feed(&mut agy).is_empty());
    }

    const ASK: &str = r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"sessionId":"6b27e1ab-ee11-459e-912b-7b2dfb3a440f","toolCall":{"sessionUpdate":"tool_call","toolCallId":"call_9","name":"run_command","status":"pending","kind":"execute","title":"hostname","rawInput":{"CommandLine":"hostname"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"always","name":"Always allow","kind":"allow_always"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"1"}"#;

    fn permission(events: &[Event]) -> &PermissionRequest {
        events
            .iter()
            .find_map(|e| match e {
                Event::Permission(p) => Some(p.as_ref()),
                _ => None,
            })
            .expect("a permission")
    }

    #[test]
    fn approvals_offer_once_only_options_and_answer_inline() {
        let (mut agy, mut peer, _) = session(None);
        peer.update(&mut agy, r#"{"sessionUpdate":"tool_call","toolCallId":"call_9","name":"run_command","status":"in_progress","kind":"execute","title":"hostname","rawInput":{"CommandLine":"hostname"}}"#);
        peer.say(ASK);
        let asked = peer.feed(&mut agy);
        let pr = permission(&asked);
        assert_eq!(pr.id, "agy-1:1-1");
        assert_eq!(
            (pr.action.as_str(), pr.risk.as_str(), pr.command.as_str()),
            ("shell", "high", "hostname")
        );
        assert_eq!(pr.title, "Antigravity wants to run a command");
        assert_eq!(pr.tool_item_id.as_deref(), Some("call_9"));
        // allow_always is never offered: the phone grants nothing persistent
        let ids: Vec<&str> = pr.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["allow", "deny"]);
        assert!(pr.options.iter().all(|o| o.scope == "once"));
        assert_eq!(pr.supported_decisions, ["approve-once", "reject"]);
        let card = items(&asked)[1].tool.clone().unwrap();
        assert_eq!(
            (card.status, card.permission_id.as_deref()),
            (ToolStatus::AwaitingApproval, Some("agy-1:1-1"))
        );
        assert_eq!(last_state(&asked).status, "waiting");
        assert!(agy.knows_permission("agy-1:1-1"));
        // the bucket must match; then exactly one answer with agy's id
        assert!(agy.decide("agy-1:1-1", "reject", Some("allow")).is_err());
        assert!(
            agy.decide("agy-1:1-1", "approve-session", Some("always"))
                .is_err()
        );
        let ok = agy
            .decide("agy-1:1-1", "approve-once", Some("allow"))
            .unwrap();
        assert_eq!(
            peer.recv(),
            r#"{"jsonrpc":"2.0","id":"1","result":{"outcome":{"outcome":"selected","optionId":"allow"}}}"#
        );
        assert_eq!(
            items(&ok)[0].tool.as_ref().unwrap().status,
            ToolStatus::Running
        );
        assert_eq!(last_state(&ok).status, "running");
        assert!(
            agy.decide("agy-1:1-1", "approve-once", Some("allow"))
                .is_err()
        );
        // a deny: the card stays denied when agy then reports it failed
        peer.say(&ASK.replace(r#""id":"1""#, r#""id":"2""#));
        peer.feed(&mut agy);
        agy.decide("agy-1:1-2", "reject", None).unwrap();
        assert!(peer.recv().contains(r#""optionId":"deny""#));
        let failed = peer.update(&mut agy, r#"{"sessionUpdate":"tool_call_update","toolCallId":"call_9","status":"failed","kind":"execute","rawOutput":{"output":""}}"#);
        assert_eq!(
            items(&failed)[0].tool.as_ref().unwrap().status,
            ToolStatus::Denied
        );
        // only allow_always offered: a synthetic deny answers `cancelled`
        let only = ASK.replace(r#""id":"1""#, r#""id":"3""#).replace(
            r#"{"optionId":"deny","name":"Deny","kind":"reject_once"}"#,
            r#"{"optionId":"never","name":"Never","kind":"reject_always"}"#,
        );
        peer.say(&only);
        let pr = permission(&peer.feed(&mut agy)).clone();
        let ids: Vec<&str> = pr.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["allow", CANCEL_OPTION]);
        agy.decide(&pr.id, "reject", Some(CANCEL_OPTION)).unwrap();
        assert_eq!(
            peer.recv(),
            r#"{"jsonrpc":"2.0","id":"3","result":{"outcome":{"outcome":"cancelled"}}}"#
        );
        // an unknown session and an unsupported method get errors
        peer.say(
            &ASK.replace(SID, "nope")
                .replace(r#""id":"1""#, r#""id":"4""#),
        );
        assert!(peer.feed(&mut agy).is_empty());
        assert!(peer.recv().contains(r#""id":"4","error""#));
        peer.say(r#"{"jsonrpc":"2.0","method":"fs/read_text_file","params":{"sessionId":"x","path":"C:\\secret"},"id":"5"}"#);
        peer.feed(&mut agy);
        assert!(peer.recv().contains(r#""code":-32601"#));
    }

    #[test]
    fn interrupt_cancels_asks_and_completes_with_the_turn() {
        let (mut agy, mut peer, _) = session(None);
        // idle elsewhere: an interrupt with nothing running is done at once
        peer.update(&mut agy, r#"{"sessionUpdate":"tool_call","toolCallId":"call_9","name":"run_command","status":"in_progress","kind":"execute","rawInput":{"CommandLine":"ping -n 10 127.0.0.1"}}"#);
        peer.say(ASK);
        peer.feed(&mut agy);
        let stop = agy.interrupt(SID, "i1").unwrap();
        assert_eq!(
            peer.recv(),
            format!(
                r#"{{"jsonrpc":"2.0","method":"session/cancel","params":{{"sessionId":"{SID}"}}}}"#
            )
        );
        assert_eq!(
            peer.recv(),
            r#"{"jsonrpc":"2.0","id":"1","result":{"outcome":{"outcome":"cancelled"}}}"#
        );
        assert!(
            stop.iter()
                .any(|e| matches!(e, Event::Withdrawn(p) if p == "agy-1:1-1"))
        );
        assert!(
            stop.iter()
                .any(|e| matches!(e, Event::Progress { tag, status: "accepted" } if tag == "i1"))
        );
        assert!(!agy.knows_permission("agy-1:1-1"));
        peer.say(r#"{"jsonrpc":"2.0","result":{"stopReason":"cancelled","usage":{"inputTokens":1,"outputTokens":1}},"id":3}"#);
        let end = peer.feed(&mut agy);
        let mut got = dones(&end);
        got.sort();
        assert_eq!(got, [("c1", "cancelled"), ("i1", "completed")]);
        assert!(
            ops(&end)
                .iter()
                .any(|op| matches!(op, Op::Turn(t) if t.status == "interrupted"))
        );
        assert!(
            agy.tick(now_ms() + INTERRUPT_MS + 1).is_empty(),
            "closed once"
        );
        assert_eq!(
            dones(&agy.interrupt(SID, "i2").unwrap()),
            [("i2", "completed")]
        );
    }

    #[test]
    fn an_interrupt_without_a_turn_end_is_unknown() {
        let (mut agy, peer, _) = session(None);
        agy.interrupt_ms = 0;
        agy.interrupt(SID, "i1").unwrap();
        peer.recv();
        assert_eq!(dones(&agy.tick(now_ms() + 1)), [("i1", "unknown")]);
        assert!(agy.tick(now_ms() + 1).is_empty());
    }

    #[test]
    fn a_follow_up_steers_and_each_prompt_is_a_turn() {
        let (mut agy, mut peer, _) = session(None);
        peer.update(
            &mut agy,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"A poem"}}"#,
        );
        let steer = agy
            .prompt(
                SID,
                &Prompt {
                    text: "stop".into(),
                    images: vec![],
                },
                "p1",
            )
            .unwrap();
        let frame = peer.recv();
        assert!(frame.contains(r#""text":"stop""#), "{frame}");
        // the streaming item closes; the new bubble opens turn 2
        let its = items(&steer);
        assert_eq!((its[0].kind, its[0].done), (ItemKind::Text, true));
        assert_eq!(its[1].kind, ItemKind::User);
        // agy ends the first prompt at once, without usage
        peer.say(r#"{"jsonrpc":"2.0","result":{"stopReason":"end_turn"},"id":3}"#);
        let first = peer.feed(&mut agy);
        assert_eq!(dones(&first), [("c1", "completed")]);
        assert_eq!(last_state(&first).status, "running");
        let more = peer.update(
            &mut agy,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"steered"}}"#,
        );
        assert_eq!(
            items(&more)[0].turn_id.as_deref(),
            its[1].turn_id.as_deref()
        );
        peer.say(r#"{"jsonrpc":"2.0","result":{"stopReason":"end_turn","usage":{"inputTokens":2,"outputTokens":1}},"id":4}"#);
        let second = peer.feed(&mut agy);
        assert_eq!(dones(&second), [("p1", "completed")]);
        assert_eq!(last_state(&second).status, "idle");
    }

    #[test]
    fn images_go_as_acp_image_blocks() {
        let (mut agy, peer, _) = session(None);
        let prompt = Prompt {
            text: "what color".into(),
            images: vec![PromptImage {
                media_type: "image/png".into(),
                data: "iVBORw0KGgo=".into(),
            }],
        };
        let out = agy.prompt(SID, &prompt, "p1").unwrap();
        assert!(peer.recv().contains(r#""prompt":[{"type":"text","text":"what color"},{"type":"image","mimeType":"image/png","data":"iVBORw0KGgo="}]"#));
        let user = items(&out)
            .into_iter()
            .find(|i| i.kind == ItemKind::User)
            .unwrap();
        assert_eq!(user.images, 1);
    }

    #[test]
    fn model_mode_close_and_errors() {
        let (mut agy, mut peer, _) = session(None);
        agy.set_model(SID, "gemini-3.8-flash-low", "m1").unwrap();
        let f = peer.recv();
        answer(&mut peer, &f, "{}");
        let ok = peer.feed(&mut agy);
        assert_eq!(
            last_state(&ok).model.as_deref(),
            Some("gemini-3.8-flash-low")
        );
        assert_eq!(dones(&ok), [("m1", "completed")]);
        assert!(agy.set_mode(SID, "bypassPermissions", "x").is_err());
        assert!(agy.set_mode(SID, "auto", "x").is_err());
        agy.set_mode(SID, "acceptEdits", "s1").unwrap();
        let f = peer.recv();
        assert!(f.contains(r#""modeId":"accept-edits""#), "{f}");
        answer(&mut peer, &f, "{}");
        assert_eq!(
            last_state(&peer.feed(&mut agy)).permission_mode.as_deref(),
            Some("acceptEdits")
        );
        agy.set_model(SID, "nope", "m2").unwrap();
        let f = peer.recv();
        peer.say(&format!(
            r#"{{"jsonrpc":"2.0","error":{{"code":-32602,"message":"unknown model"}},"id":{}}}"#,
            id_of(&f)
        ));
        assert_eq!(dones(&peer.feed(&mut agy)), [("m2", "failed")]);
        // close: the running prompt fails, the session ends
        agy.close(SID, "e1").unwrap();
        let f = peer.recv();
        assert!(f.contains(r#""method":"session/close""#), "{f}");
        answer(&mut peer, &f, "{}");
        let end = peer.feed(&mut agy);
        let mut got = dones(&end);
        got.sort();
        assert_eq!(got, [("c1", "failed"), ("e1", "completed")]);
        assert_eq!(last_state(&end).status, "ended");
        assert!(!agy.owns_session(SID));
    }

    #[test]
    fn exit_fails_commands_and_idles_sessions() {
        let (mut agy, mut peer, _) = session(None);
        peer.say(ASK);
        peer.feed(&mut agy);
        drop(peer.to_core);
        let out = agy.on_event(peer.events.recv_timeout(WAIT).unwrap());
        assert!(out.iter().any(|e| matches!(e, Event::Withdrawn(_))));
        assert_eq!(dones(&out), [("c1", "failed")]);
        assert_eq!(last_state(&out).status, "idle");
        assert!(
            items(&out)
                .iter()
                .any(|i| i.text.contains("the next prompt resumes"))
        );
        assert!(
            agy.start_session("C:\\w", Prompt::default(), None, "c2")
                .is_err()
        );
    }

    #[test]
    fn resume_load_and_list() {
        let (mut agy, mut peer) = connect();
        // history: load replays the conversation, user chunks included
        agy.load_session(SID, r"C:\work").unwrap();
        let f = peer.recv();
        assert!(f.contains(r#""method":"session/load","params":{"sessionId":"6b27e1ab-ee11-459e-912b-7b2dfb3a440f","cwd":"C:\\work","mcpServers":[]}"#), "{f}");
        let u = peer.update(&mut agy, r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Run hostname"}}"#);
        assert_eq!(items(&u)[0].kind, ItemKind::User);
        peer.update(&mut agy, r#"{"sessionUpdate":"tool_call","toolCallId":"call_1","name":"run_command","status":"completed","kind":"execute","rawInput":{"CommandLine":"hostname"},"rawOutput":{"output":"PC"}}"#);
        let busy = agy.prompt(SID, &Prompt::default(), "p0");
        assert!(busy.is_err(), "no prompt while loading");
        answer(&mut peer, &f, NEW_OK);
        let loaded = peer.feed(&mut agy);
        assert_eq!(last_state(&loaded).status, "idle");
        // a session this process never opened: resume, then the prompt
        let other = "11111111-2222-3333-4444-555555555555";
        let starting = agy
            .resume_session(
                other,
                r"C:\work",
                Prompt {
                    text: "go on".into(),
                    images: vec![],
                },
                "p1",
            )
            .unwrap();
        assert_eq!(last_state(&starting).status, "starting");
        assert!(
            agy.resume_session(other, r"C:\work", Prompt::default(), "p2")
                .is_err()
        );
        let f = peer.recv();
        assert!(f.contains(r#""method":"session/resume""#), "{f}");
        answer(&mut peer, &f, &NEW_OK.replace(SID, other));
        peer.feed(&mut agy);
        assert!(peer.recv().contains(r#""text":"go on""#));
        // a resume agy refuses ends the session
        let gone = "99999999-2222-3333-4444-555555555555";
        agy.resume_session(gone, r"C:\work", Prompt::default(), "p3")
            .unwrap();
        let f = peer.recv();
        peer.say(&format!(r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"conversation not found"}},"id":{}}}"#, id_of(&f)));
        let refused = peer.feed(&mut agy);
        assert_eq!(dones(&refused), [("p3", "failed")]);
        assert_eq!(last_state(&refused).status, "ended");
        // the conversations agy has in a workspace
        agy.list(r"C:\work").unwrap();
        let f = peer.recv();
        answer(
            &mut peer,
            &f,
            r#"{"sessions":[{"sessionId":"fc39bab1","cwd":"/C:/work","title":"Ping And Summarize","updatedAt":"2026-11-11T23:29:14Z"},{"cwd":"/C:/work"}]}"#,
        );
        let listed = peer.feed(&mut agy);
        let Event::Listed { cwd, sessions } = &listed[0] else {
            panic!("{listed:?}")
        };
        assert_eq!(cwd, r"C:\work");
        assert_eq!(sessions.len(), 1);
        assert_eq!(native_path(&sessions[0].cwd), "C:/work");
        assert_eq!(sessions[0].updated_at, 1_794_439_754_000);
    }

    #[test]
    fn launch_config_applies_the_workspace_policy() {
        let root = std::env::temp_dir().join(format!("chernobyl-agy-cfg-{}", std::process::id()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let data = root.join("data");
        let entry = |extra: &str| {
            json::parse(&format!(
                r#"{{"type":"antigravity","workspaces":[{}]{extra}}}"#,
                json::stringify(&json::s(&ws.to_string_lossy()))
            ))
            .unwrap()
        };
        let cfg = launch_config(
            &entry(
                r#","model":"gemini-3.8-flash-low","maxSessions":2,"binary":"C:\\agy\\agy.exe""#,
            ),
            &data,
        )
        .unwrap();
        assert_eq!(cfg.workspaces.len(), 1);
        assert_eq!(cfg.model.as_deref(), Some("gemini-3.8-flash-low"));
        assert_eq!(cfg.max_sessions, 2);
        assert_eq!(cfg.argv(), ["acp"]);
        let live = cfg.agent_live(&[]);
        assert_eq!(live.composer, ["images", "commands", "rename"]);
        assert_eq!(live.permission_modes, ["default", "acceptEdits", "plan"]);
        assert_eq!(live.prompt_while_busy, "merge");
        let models = [("gemini-3.8-flash-low".to_string(), "Low".to_string())];
        assert_eq!(cfg.agent_live(&models).composer[0], "model");
        for bad in [
            r#","model":"--dangerously-skip-permissions""#,
            r#","model":"a b""#,
            r#","maxSessions":0"#,
            r#","maxSessions":17"#,
            r#","binary":"""#,
        ] {
            assert!(launch_config(&entry(bad), &data).is_err(), "{bad}");
        }
        let refuse = |w: &str| {
            let e = json::parse(&format!(
                r#"{{"workspaces":[{}]}}"#,
                json::stringify(&json::s(w))
            ))
            .unwrap();
            launch_config(&e, &data).is_err()
        };
        assert!(refuse("relative"));
        assert!(refuse(r"C:\"));
        assert!(refuse(&root.join("missing").to_string_lossy()));
        // a folder holding the companion's data
        std::fs::create_dir_all(&data).unwrap();
        assert!(refuse(&root.to_string_lossy()));
        assert!(launch_config(&json::parse(r#"{"workspaces":[]}"#).unwrap(), &data).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
