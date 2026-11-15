//! Codex app-server adapter core (ADR-015 §4.3, step 7): the JSON-RPC
//! client for a `codex app-server` child, the notification decoder (§3.6
//! Codex table) and the approval mapping (§4.5). Protocol code only; the
//! companion wiring (LiveRuntime, transcript hub, engine) is
//! `codex_runtime.rs`.
//!
//! Wire (schema pin openai/codex@970111f): one JSON object per line,
//! JSON-RPC 2.0 shapes without the `"jsonrpc"` field. Codex numbers its own
//! requests from 0, so its ids overlap ours: a message routes on `method`
//! (method + id = server request, method alone = notification, id alone = a
//! reply to us). Every server request is answered exactly once.
//!
//! IO: [`Codex`] writes through a [`ChildIo`] and is fed that child's
//! [`ChildEvent`]s, so it runs over any byte pipes: `ChildIo::spawn` in
//! production, `ChildIo::from_pipes` with `std::io::pipe()` ends in tests.

use crate::adapters::Capability;
use crate::crypto;
use crate::json::{self, Json};
use crate::protocol::{
    AgentLive, LiveWorkspace, PermissionOption, PermissionRequest, PromptImage, now_ms,
};
use crate::runtime_io::{AUTH_TTL_MS, ChildEvent, ChildIo, redact};
use crate::transcript::{
    Field, FileDiff, Item, ItemKind, LiveState, McpAuthState, McpServer, Op, PlanStep, ToolCall,
    ToolStatus, TurnState, Usage,
};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

/// The codex build the wire was checked against (`generate-ts` + live runs).
pub const VERIFIED: &str = "codex-cli 0.167.0-alpha.4.1";
/// Thread policy. Verified live on Windows: `untrusted` asks the phone
/// before every file change and every command codex does not know as
/// read-only (`git status`, `ls`, … run unasked). The Windows sandbox is not
/// used: with `workspace-write` a command in a fresh workspace ran in `C:\`
/// after a 3-minute stall, so commands run like Claude Code's, unsandboxed
/// and approved. codex records each workspace as trusted in its config.toml.
const APPROVAL_POLICY: &str = "untrusted";
const SANDBOX: &str = "danger-full-access";
/// History replay (`thread/items/list`): the newest this many items.
const HISTORY_ITEMS: i64 = 300;
/// `thread/list`: the newest this many threads in the workspaces.
const THREADS_MAX: i64 = 50;
/// §3.5 tool output: forwarding stops here; completion keeps head + tail.
const OUTPUT_CAP: usize = 32 << 10;
/// A partial output line longer than this is forwarded without its newline.
const LINE_FLUSH: usize = 4 << 10;
/// §3.5 text and thinking items, as cached for a replace at turn end.
const TEXT_CAP: usize = 256 << 10;
const THINKING_CAP: usize = 64 << 10;
/// Permission records are journaled: legacy command, context (§4.5) and
/// detail stay small.
const COMMAND_CAP: usize = 2 << 10;
const CONTEXT_CAP: usize = 500;
const DETAIL_CAP: usize = 64 << 10;
/// What codex itself answers to requests it cannot serve, unknown methods
/// included (its message_processor uses -32600, not -32601).
const INVALID_REQUEST: i64 = -32600;
/// `mcpServerStatus/list` pages are followed up to this many servers (the
/// hub keeps 32 anyway).
const MCP_SERVERS_MAX: usize = 256;
/// §4.6: an interrupt with no turn end by then is `unknown`.
pub const INTERRUPT_MS: i64 = 30_000;

/* ------------------------------------------------------------------ */
/* Public API                                                          */
/* ------------------------------------------------------------------ */

/// What the companion does with codex's output (`codex_runtime` wires each one).
// most events are ops (one per streamed delta): boxing them would allocate per delta
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A transcript op for one session; the codex thread id is the session id.
    Op(String, Op),
    /// A new approval for the engine (G2); its tool card op follows it.
    Permission(Box<PermissionRequest>),
    /// Answered elsewhere, ended with its turn, or codex exited: the engine
    /// resolves it `stale`. It is never answered afterwards.
    Withdrawn(String),
    /// `thread/start` (or `thread/fork`) succeeded for command `tag`: the
    /// session exists. A fork's history ops follow this event.
    SessionStarted { tag: String, thread: String },
    /// `thread/name/set` succeeded: the session's title.
    Renamed { thread: String, title: String },
    /// A `thread/list` page: codex's threads in the workspaces, newest first.
    Threads(Vec<FoundThread>),
    /// G1 progress for command `tag`: `running` (prompt) or `accepted` (interrupt).
    Progress { tag: String, status: &'static str },
    /// G1 result for command `tag`: completed | cancelled | failed | unknown.
    Done {
        tag: String,
        status: &'static str,
        error: Option<String>,
    },
    /// `mcpServer/oauth/login` returned codex's own sign-in URL for command
    /// `tag`: open it with `runtime_io::open_url_on_desktop`.
    OpenUrl { tag: String, url: String },
}

/// The Codex capability declaration (PRD §7.3): `available` = run live
/// against [`VERIFIED`] (`tests` `live_codex_*`); the rest says what is not.
pub fn capabilities() -> Vec<Capability> {
    let ok = |id| Capability::new(id, "available", None);
    let limited = |id, why| Capability::new(id, "limited", Some(why));
    vec![
        ok("session.create"),
        ok("session.prompt"),
        ok("session.interrupt"),
        ok("session.activity.stream"),
        ok("permission.review"),
        ok("diff.read"),
        limited(
            "session.list",
            "the 50 most recent Codex threads whose folder is an allow-listed workspace, from the \
             phone or the Codex app; one the Codex app wrote in the last 2 minutes is not resumed \
             (fork it to continue on the phone)",
        ),
        limited(
            "session.history.read",
            "the newest 300 items of a thread, redacted; a thread running in the Codex app is \
             re-read at the probe cadence, not streamed",
        ),
        limited(
            "server.spawn",
            "codex app-server is started only in workspaces the operator allow-listed",
        ),
        limited(
            "mcp.status",
            "server status verified live; per-thread status is not reported",
        ),
        limited(
            "mcp.auth",
            "desktop sign-in (codex opens the browser on the PC) verified against a fake \
             app-server only",
        ),
    ]
}

/// One thread of `thread/list`, as the session list shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct FoundThread {
    pub id: String,
    /// The thread's name, else its first prompt (one line, redacted).
    pub title: String,
    pub cwd: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A prompt on its way to a thread (ADR-017 images ride along).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Prompt {
    pub text: String,
    pub images: Vec<PromptImage>,
}

/// A request we sent, by id, with what its reply means.
enum Pending {
    Init,
    Models,
    Threads,
    ThreadStart {
        tag: String,
        cwd: String,
        prompt: Prompt,
    },
    /// `submit_prompt` to a thread whose app-server exited: the prompt
    /// follows once codex has loaded the thread again.
    ThreadResume {
        tag: String,
        thread: String,
        cwd: String,
        prompt: Prompt,
    },
    /// ADR-018 fork: the new thread's history, then the prompt.
    Fork {
        tag: String,
        cwd: String,
        prompt: Prompt,
    },
    /// `thread/items/list`: the history of `thread`; `then` = a fork's
    /// first prompt, sent once the history is in place.
    History {
        thread: String,
        cwd: String,
        then: Option<(String, Prompt)>,
    },
    Rename {
        tag: String,
        thread: String,
        title: String,
    },
    TurnStart {
        tag: String,
        thread: String,
    },
    Steer {
        tag: String,
        thread: String,
        prompt: Prompt,
    },
    Interrupt {
        tag: String,
        thread: String,
        turn: String,
    },
    /// One `mcpServerStatus/list` page; the servers of the pages before it.
    McpList(Vec<McpServer>),
    McpLogin {
        tag: String,
        server: String,
    },
    McpReload {
        tag: String,
        server: String,
    },
}

impl Pending {
    /// The phone command riding on this request, and whether it is an interrupt.
    fn tag(&self) -> Option<(&str, bool)> {
        match self {
            Pending::Init | Pending::Models | Pending::Threads | Pending::McpList(_) => None,
            Pending::History { then, .. } => then.as_ref().map(|(tag, _)| (tag.as_str(), false)),
            Pending::Interrupt { tag, .. } => Some((tag, true)),
            Pending::ThreadStart { tag, .. }
            | Pending::ThreadResume { tag, .. }
            | Pending::Fork { tag, .. }
            | Pending::Rename { tag, .. }
            | Pending::TurnStart { tag, .. }
            | Pending::Steer { tag, .. }
            | Pending::McpLogin { tag, .. }
            | Pending::McpReload { tag, .. } => Some((tag, false)),
        }
    }
}

/// An approval waiting for the phone, by permission id.
struct Ask {
    /// Codex's request id, echoed verbatim in the one answer.
    id: Json,
    turn: String,
    item: String,
    kind: AskKind,
}

enum AskKind {
    /// `available`: codex's `availableDecisions` names, when it sent them
    /// (live: `["accept", {acceptWithExecpolicyAmendment}, "cancel"]`).
    Command {
        amendment: Option<Json>,
        available: Option<Vec<String>>,
    },
    FileChange,
    Question {
        ids: Vec<String>,
    },
}

struct Thread {
    cwd: String,
    model: Option<String>,
    /// ADR-017 `set_model`: sent with every turn/start and resume.
    model_override: Option<String>,
    /// The last `thread/status/changed`, for the state ops in between.
    status: &'static str,
    /// (used, max) tokens from `thread/tokenUsage/updated`.
    context: Option<(i64, i64)>,
    /// The turn in progress, as far as we know.
    turn: Option<String>,
    /// The last ended turn: a reply that raced its turn/completed binds here.
    last: Option<TurnState>,
}

/// A live turn and the commands that end with it.
struct Turn {
    thread: String,
    state: TurnState,
    tags: Vec<(String, bool)>,
}

/// An item between item/started and item/completed, as the phone has it, so a
/// replace (approval, turn end) never drops what streamed in. Text is cached
/// raw and redacted when replayed; tool output is cached as forwarded.
struct Live {
    thread: String,
    item: Item,
    /// commandExecution: the partial line not forwarded yet.
    line: String,
    /// commandExecution: output bytes forwarded so far.
    forwarded: usize,
    /// reasoning: which stream feeds the text (true = summary).
    summary: Option<bool>,
    /// False for a reasoning item until it has text: codex sends many
    /// empty ones (live), which the phone never sees.
    shown: bool,
}

/// One `codex app-server` process: its JSON-RPC state and every session
/// (thread) inside it. Not thread-safe by itself; the runtime holds it in a Mutex,
/// feeds it from the `ChildIo` receiver and calls the command methods.
pub struct Codex {
    io: ChildIo,
    adapter: String,
    next_id: i64,
    /// Ids of [`Codex::background`] requests.
    next_bg: i64,
    /// `initialized` sent; until then requests wait in `queued`.
    ready: bool,
    queued: Vec<String>,
    pending: HashMap<i64, Pending>,
    asks: HashMap<String, Ask>,
    threads: HashMap<String, Thread>,
    turns: HashMap<String, Turn>,
    items: HashMap<String, Live>,
    mcp: Vec<McpServer>,
    /// `model/list` (value, label), visible models only.
    models: Vec<(String, String)>,
    /// One `mcpServerStatus/list` at a time; a trigger meanwhile re-lists once after.
    listing: bool,
    relist: bool,
    /// MCP server → its `mcp_auth` command while codex waits for the browser.
    logins: HashMap<String, String>,
    /// `mcp_reconnect` commands after their reload: (server, tag, armed).
    /// Armed once a list started after the reload; settled by that list.
    reconnects: Vec<(String, String, bool)>,
    /// §4.6 timers: (command, deadline, outcome) for interrupts (`unknown`)
    /// and sign-ins (`expired`). Ones whose command ended meanwhile lapse.
    timers: Vec<(String, i64, &'static str)>,
    /// Threads waiting for their `thread/resume` reply.
    resuming: HashSet<String>,
    /// The §4.6 durations ([`INTERRUPT_MS`], `AUTH_TTL_MS`); tests shrink them.
    pub interrupt_ms: i64,
    pub auth_ttl_ms: i64,
    seq: u64,
    malformed: u64,
    exited: bool,
}

impl Codex {
    /// Takes over a codex child's stdio and starts the handshake (`initialize`).
    pub fn new(io: ChildIo, adapter_id: &str, version: &str) -> Codex {
        let mut codex = Codex {
            io,
            adapter: adapter_id.into(),
            next_id: 1,
            next_bg: 1 << 40,
            ready: false,
            queued: Vec::new(),
            pending: HashMap::new(),
            asks: HashMap::new(),
            threads: HashMap::new(),
            turns: HashMap::new(),
            items: HashMap::new(),
            mcp: Vec::new(),
            models: Vec::new(),
            listing: false,
            relist: false,
            logins: HashMap::new(),
            reconnects: Vec::new(),
            timers: Vec::new(),
            resuming: HashSet::new(),
            interrupt_ms: INTERRUPT_MS,
            auth_ttl_ms: AUTH_TTL_MS,
            seq: 0,
            malformed: 0,
            exited: false,
        };
        let params = json::obj(vec![
            (
                "clientInfo",
                json::obj(vec![
                    ("name", json::s("chernobyl")),
                    ("title", json::s("CHERNOBYL")),
                    ("version", json::s(version)),
                ]),
            ),
            (
                "capabilities",
                json::obj(vec![("experimentalApi", Json::Bool(false))]),
            ),
        ]);
        let id = codex.alloc(Pending::Init);
        // a child that is already gone reports Exit next
        let _ = codex.io.try_send(request(id, "initialize", Some(params)));
        // queued until `initialized`; the picker shows what this lists
        codex.background("model/list", json::obj(vec![]), Pending::Models);
        codex
    }

    /// `create_session`: `thread/start` in `cwd` (an allow-listed workspace),
    /// then `turn/start` with `prompt` once codex names the thread.
    pub fn start_session(
        &mut self,
        cwd: &str,
        prompt: Prompt,
        tag: &str,
    ) -> Result<(), &'static str> {
        let pending = Pending::ThreadStart {
            tag: tag.into(),
            cwd: cwd.into(),
            prompt,
        };
        self.call("thread/start", Some(policy(None, cwd, None)), pending)
    }

    /// ADR-018 fork of a thread on disk (`thread/fork`): a new thread with
    /// the old one's history, then `prompt` as its first turn. The original
    /// thread is not touched.
    pub fn fork(
        &mut self,
        thread: &str,
        cwd: &str,
        prompt: Prompt,
        tag: &str,
    ) -> Result<(), &'static str> {
        let pending = Pending::Fork {
            tag: tag.into(),
            cwd: cwd.into(),
            prompt,
        };
        let mut params = policy(Some(thread), cwd, None);
        if let Json::Obj(f) = &mut params {
            // the history comes from thread/items/list (full hydration is deprecated)
            f.push(("excludeTurns".into(), Json::Bool(true)));
        }
        self.call("thread/fork", Some(params), pending)
    }

    /// `thread/list` for the workspaces: answered as [`Event::Threads`].
    pub fn list_threads(&mut self, cwds: &[String]) -> Result<(), &'static str> {
        let params = json::obj(vec![
            ("limit", Json::Int(THREADS_MAX)),
            ("sortKey", json::s("updated_at")),
            ("cwd", Json::Arr(cwds.iter().map(|c| json::s(c)).collect())),
        ]);
        self.call("thread/list", Some(params), Pending::Threads)
    }

    /// A thread's history (`thread/items/list`, newest [`HISTORY_ITEMS`]) as
    /// transcript ops for `thread`, whether codex has it loaded or not.
    pub fn history(&mut self, thread: &str, cwd: &str) -> Result<(), &'static str> {
        self.items_list(thread, cwd, None)
    }

    fn items_list(
        &mut self,
        thread: &str,
        cwd: &str,
        then: Option<(String, Prompt)>,
    ) -> Result<(), &'static str> {
        let params = json::obj(vec![
            ("threadId", json::s(thread)),
            ("limit", Json::Int(HISTORY_ITEMS)),
            ("sortDirection", json::s("desc")),
        ]);
        let pending = Pending::History {
            thread: thread.into(),
            cwd: cwd.into(),
            then,
        };
        self.call("thread/items/list", Some(params), pending)
    }

    /// ADR-017 `set_model`: one of codex's listed models, used from the next
    /// turn on (codex takes `model` per turn and keeps it).
    pub fn set_model(&mut self, thread: &str, model: &str) -> Result<Vec<Event>, &'static str> {
        // only a model codex listed (the phone's fallback list is Claude's)
        if !self.models.iter().any(|(m, _)| m == model) {
            return Err("Codex does not offer this model");
        }
        let t = self.threads.get_mut(thread).ok_or("unknown session")?;
        t.model = Some(model.into());
        t.model_override = Some(model.into());
        let status = t.status;
        Ok(vec![self.state_op(thread, status)])
    }

    /// ADR-017 `rename_session`: `thread/name/set` (codex keeps the name).
    pub fn rename(&mut self, thread: &str, title: &str, tag: &str) -> Result<(), &'static str> {
        let params = json::obj(vec![
            ("threadId", json::s(thread)),
            ("name", json::s(title)),
        ]);
        let pending = Pending::Rename {
            tag: tag.into(),
            thread: thread.into(),
            title: title.into(),
        };
        self.call("thread/name/set", Some(params), pending)
    }

    /// The thread's [`LiveState`]: status plus model, models and context.
    fn state_op(&self, thread: &str, status: &'static str) -> Event {
        let t = self.threads.get(thread);
        let state = LiveState {
            status,
            permission_mode: None,
            model: t.and_then(|t| t.model.clone()),
            models: self.models.clone(),
            commands: Vec::new(),
            context: t.and_then(|t| t.context),
        };
        Event::Op(thread.into(), Op::State(state))
    }

    /// `submit_prompt` to a session whose app-server exited (ADR-015 §10):
    /// `thread/resume` with codex's thread id in its workspace, then the
    /// prompt as a new turn once codex has loaded the thread. Not a create.
    pub fn resume_session(
        &mut self,
        thread: &str,
        cwd: &str,
        model: Option<&str>,
        prompt: Prompt,
        tag: &str,
    ) -> Result<Vec<Event>, &'static str> {
        if self.resuming.contains(thread) {
            return Err("Codex is still resuming this session; send again in a moment");
        }
        let pending = Pending::ThreadResume {
            tag: tag.into(),
            thread: thread.into(),
            cwd: cwd.into(),
            prompt,
        };
        let mut params = policy(Some(thread), cwd, model);
        if let Json::Obj(f) = &mut params {
            // the phone has the history already
            f.push(("excludeTurns".into(), Json::Bool(true)));
        }
        self.call("thread/resume", Some(params), pending)?;
        self.resuming.insert(thread.into());
        let state = LiveState {
            status: "starting",
            model: model.map(str::to_string),
            models: self.models.clone(),
            ..Default::default()
        };
        Ok(vec![Event::Op(thread.into(), Op::State(state))])
    }

    /// The model `set_model` chose for a live thread (kept across a respawn).
    pub fn model_override(&self, thread: &str) -> Option<String> {
        self.threads.get(thread)?.model_override.clone()
    }

    /// `submit_prompt`: steers the running turn, or starts one when idle. The
    /// user bubble is made here; codex's echoed `userMessage` is dropped.
    pub fn prompt(
        &mut self,
        thread: &str,
        prompt: Prompt,
        tag: &str,
    ) -> Result<Vec<Event>, &'static str> {
        let mut out = Vec::new();
        self.send_prompt(thread, prompt, tag, &mut out)?;
        Ok(out)
    }

    /// `interrupt`: `turn/interrupt` for the active turn; done at once when idle.
    pub fn interrupt(&mut self, thread: &str, tag: &str) -> Result<Vec<Event>, &'static str> {
        let active = self
            .threads
            .get(thread)
            .ok_or("unknown session")?
            .turn
            .clone();
        let Some(turn) = active else {
            return Ok(vec![done(tag.into(), "completed", None)]);
        };
        let params = json::obj(vec![
            ("threadId", json::s(thread)),
            ("turnId", json::s(&turn)),
        ]);
        let pending = Pending::Interrupt {
            tag: tag.into(),
            thread: thread.into(),
            turn,
        };
        self.call("turn/interrupt", Some(params), pending)?;
        let deadline = now_ms() + self.interrupt_ms;
        self.timers.push((tag.into(), deadline, "unknown"));
        Ok(Vec::new())
    }

    /// `permission_decision`, after the engine validated it (G2): writes the
    /// one answer for `option_id` (or the first non-persistent option of
    /// `decision`), unless codex withdrew the request meanwhile.
    pub fn decide(
        &mut self,
        permission_id: &str,
        decision: &str,
        option_id: Option<&str>,
        answers: &[(String, Vec<String>)],
    ) -> Result<Vec<Event>, &'static str> {
        let ask = self
            .asks
            .get(permission_id)
            .ok_or("request is no longer pending")?;
        let offered = options(&ask.kind);
        let option = match option_id {
            Some(id) => offered
                .iter()
                .find(|o| o.id == id)
                .ok_or("unknown option")?,
            None => offered
                .iter()
                .find(|o| o.decision == decision && o.scope != "persistent")
                .ok_or("decision is not offered")?,
        };
        if option.decision != decision {
            return Err("option does not match the decision");
        }
        self.io
            .try_send(reply(&ask.id, answer(&ask.kind, &option.id, answers)))?;
        let ask = self.asks.remove(permission_id).expect("looked up above");
        // the card leaves awaiting-approval; a question has no item/completed
        let question = matches!(ask.kind, AskKind::Question { .. });
        let status = match (decision != "reject", question) {
            (true, true) => ToolStatus::Completed,
            (true, false) => ToolStatus::Running,
            (false, _) => ToolStatus::Denied,
        };
        Ok(self
            .set_card(&ask.item, status, question)
            .into_iter()
            .collect())
    }

    /// `mcp_auth {flow: desktop}`: asks codex to start an OAuth login; its URL
    /// comes back as [`Event::OpenUrl`].
    pub fn mcp_login(&mut self, server: &str, tag: &str) -> Result<(), &'static str> {
        let params = json::obj(vec![("name", json::s(server))]);
        let pending = Pending::McpLogin {
            tag: tag.into(),
            server: server.into(),
        };
        self.call("mcpServer/oauth/login", Some(params), pending)?;
        let deadline = now_ms() + self.auth_ttl_ms;
        self.timers.push((tag.into(), deadline, "expired"));
        Ok(())
    }

    /// The §4.6 timers, run by the runtime between child events: an
    /// interrupt with no turn end in time is `unknown`, a sign-in that did
    /// not complete in time is `expired`. A late answer then closes nothing.
    pub fn tick(&mut self, now: i64) -> Vec<Event> {
        let mut out = Vec::new();
        let (late, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.timers)
            .into_iter()
            .partition(|t| now >= t.1);
        self.timers = kept;
        let mut expired = false;
        for (tag, _, status) in late {
            if !self.forget(&tag) {
                continue; // ended in time
            }
            expired |= status == "expired";
            let why = (status == "unknown").then(|| "no turn end from Codex within 30 s".into());
            out.push(done(tag, status, why));
        }
        if expired {
            self.emit_mcp(&mut out); // the "awaiting browser" badge goes
        }
        out
    }

    /// Drops every reference to an open command; false when it had none.
    fn forget(&mut self, tag: &str) -> bool {
        let refs = |c: &Codex| {
            c.pending.len() + c.logins.len() + c.turns.values().map(|t| t.tags.len()).sum::<usize>()
        };
        let before = refs(self);
        self.pending
            .retain(|_, p| p.tag().is_none_or(|(t, _)| t != tag));
        self.logins.retain(|_, t| t != tag);
        for turn in self.turns.values_mut() {
            turn.tags.retain(|(t, _)| t != tag);
        }
        refs(self) < before
    }

    /// `mcp_reconnect`: codex's closest equivalent reloads every server
    /// (§4.7). Done when a list after the reload shows `server` connected
    /// (completed) or settled otherwise (failed).
    pub fn mcp_reload(&mut self, server: &str, tag: &str) -> Result<(), &'static str> {
        let pending = Pending::McpReload {
            tag: tag.into(),
            server: server.into(),
        };
        self.call("config/mcpServer/reload", None, pending)
    }

    pub fn owns_session(&self, id: &str) -> bool {
        self.threads.contains_key(id)
    }

    pub fn knows_permission(&self, id: &str) -> bool {
        self.asks.contains_key(id)
    }

    /// Lines that were not readable JSON-RPC (T7 counter).
    pub fn malformed(&self) -> u64 {
        self.malformed
    }

    pub fn stderr_tail(&self) -> String {
        self.io.stderr_tail()
    }

    /// Graceful exit: codex sees EOF on stdin.
    pub fn shutdown(&self) {
        self.io.close_stdin();
    }

    /// Feeds one event from the `ChildIo` receiver.
    pub fn on_event(&mut self, event: ChildEvent) -> Vec<Event> {
        let mut out = Vec::new();
        match event {
            ChildEvent::Line(line) => self.line(&line, &mut out),
            ChildEvent::Skipped(_) => {
                // its content is gone, so nothing can be answered or salvaged
                self.malformed += 1;
                let threads: Vec<String> = self.threads.keys().cloned().collect();
                for t in threads {
                    let text = "Codex sent a message over 16 MiB; it was skipped";
                    out.push(self.notice(&t, "warning", text, None));
                }
            }
            ChildEvent::Exit(code) => self.exit(code, &mut out),
        }
        out
    }

    /* -------------------------- requests -------------------------- */

    fn alloc(&mut self, pending: Pending) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, pending);
        id
    }

    fn call(
        &mut self,
        method: &str,
        params: Option<Json>,
        pending: Pending,
    ) -> Result<(), &'static str> {
        if self.exited {
            return Err("Codex has exited");
        }
        let id = self.next_id;
        self.write(request(id, method, params))?;
        self.alloc(pending);
        Ok(())
    }

    /// A request of the companion's own (`model/list`): numbered apart from
    /// the commands' requests, so neither shifts the other.
    fn background(&mut self, method: &str, params: Json, pending: Pending) {
        if self.exited {
            return;
        }
        let id = self.next_bg;
        if self.write(request(id, method, Some(params))).is_ok() {
            self.next_bg += 1;
            self.pending.insert(id, pending);
        }
    }

    fn write(&mut self, frame: String) -> Result<(), &'static str> {
        if self.ready {
            self.io.try_send(frame)
        } else {
            self.queued.push(frame);
            Ok(())
        }
    }

    fn send_prompt(
        &mut self,
        thread: &str,
        prompt: Prompt,
        tag: &str,
        out: &mut Vec<Event>,
    ) -> Result<(), &'static str> {
        let t = self.threads.get(thread).ok_or("unknown session")?;
        let (active, model) = (t.turn.clone(), t.model_override.clone());
        let mut params = vec![("threadId", json::s(thread)), ("input", input(&prompt))];
        let (text, images) = (redact(&prompt.text), prompt.images.len());
        match active {
            Some(turn) => {
                params.push(("expectedTurnId", json::s(&turn)));
                let pending = Pending::Steer {
                    tag: tag.into(),
                    thread: thread.into(),
                    prompt,
                };
                self.call("turn/steer", Some(json::obj(params)), pending)?;
            }
            // codex steers a turn/start on a busy thread as well, so a turn we
            // have not heard of yet is fine
            None => {
                if let Some(m) = model {
                    params.push(("model", json::s(&m)));
                }
                let pending = Pending::TurnStart {
                    tag: tag.into(),
                    thread: thread.into(),
                };
                self.call("turn/start", Some(json::obj(params)), pending)?;
            }
        }
        self.seq += 1;
        let mut user = new_item(&format!("user:{}", self.seq), ItemKind::User, None);
        user.text = text;
        user.images = images as u32;
        out.push(Event::Op(thread.into(), Op::Item(user)));
        Ok(())
    }

    fn send(&self, frame: String) {
        // a full or closed input means codex is stuck or gone; Exit settles the rest
        let _ = self.io.try_send(frame);
    }

    /* --------------------------- input ---------------------------- */

    fn line(&mut self, line: &str, out: &mut Vec<Event>) {
        let Ok(msg) = json::parse(line) else {
            self.malformed += 1;
            match salvage(line) {
                Some((id, true)) => self.send(reply_error(
                    &Json::Int(id),
                    "Invalid request: CHERNOBYL could not read this request",
                )),
                Some((id, false)) => {
                    let why = "Codex sent a reply CHERNOBYL could not read".to_string();
                    self.response(id, Err(why), out);
                }
                None => {}
            }
            return;
        };
        let params = json::get(&msg, "params").unwrap_or(&Json::Null);
        match (json::get_str(&msg, "method"), json::get(&msg, "id")) {
            (Some(method), Some(id)) => self.server_request(method, id, params, out),
            (Some(method), None) => self.notification(method, params, out),
            (None, Some(Json::Int(id))) => {
                let res = match json::get(&msg, "error") {
                    Some(e) => Err(json::get_str(e, "message")
                        .unwrap_or("Codex returned an error")
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
        let failed = |tag: String, error: String| done(tag, "failed", Some(error));
        match (pending, res) {
            (Pending::Init, _) => {
                // a refused initialize moves on too: later requests then fail on their own
                self.ready = true;
                self.send(json::stringify(&json::obj(vec![(
                    "method",
                    json::s("initialized"),
                )])));
                for frame in std::mem::take(&mut self.queued) {
                    self.send(frame);
                }
            }
            (Pending::McpList(mut servers), res) => {
                if let Ok(r) = &res {
                    let data = json::get(r, "data").and_then(json::as_arr).unwrap_or(&[]);
                    servers.extend(data.iter().filter_map(mcp_server));
                    // follow the cursor; the page cap stops a codex that repeats itself
                    if let Some(cursor) = json::get_str(r, "nextCursor")
                        && servers.len() < MCP_SERVERS_MAX
                    {
                        let cursor = cursor.to_string();
                        if self.page(Some(&cursor), servers).is_ok() {
                            return;
                        }
                        // codex exited meanwhile: Exit settles the rest
                        self.listing = false;
                        return;
                    }
                    self.mcp = servers;
                }
                self.listing = false;
                if res.is_ok() {
                    self.settle_reconnects(out);
                    self.emit_mcp(out);
                }
                if std::mem::take(&mut self.relist) {
                    self.list();
                }
            }
            (Pending::Models, Ok(r)) => {
                let data = json::get(&r, "data").and_then(json::as_arr).unwrap_or(&[]);
                self.models = data
                    .iter()
                    .filter(|m| json::get_bool(m, "hidden") != Some(true))
                    .filter_map(|m| {
                        let id = json::get_str(m, "model").or(json::get_str(m, "id"))?;
                        let label = json::get_str(m, "displayName").unwrap_or(id);
                        Some((id.to_string(), label.to_string()))
                    })
                    .collect();
                let threads: Vec<(String, &'static str)> = self
                    .threads
                    .iter()
                    .map(|(id, t)| (id.clone(), t.status))
                    .collect();
                for (thread, status) in threads {
                    out.push(self.state_op(&thread, status));
                }
            }
            (Pending::Threads, Ok(r)) => {
                let data = json::get(&r, "data").and_then(json::as_arr).unwrap_or(&[]);
                out.push(Event::Threads(
                    data.iter().filter_map(found_thread).collect(),
                ));
            }
            (Pending::ThreadStart { tag, cwd, prompt }, Ok(r)) => {
                self.thread_started(&r, tag, cwd, prompt, out)
            }
            (Pending::Fork { tag, cwd, prompt }, Ok(r)) => {
                let Some(thread) = json::get(&r, "thread").and_then(|t| json::get_str(t, "id"))
                else {
                    return out.push(failed(tag, "thread/fork returned no thread".into()));
                };
                let thread = thread.to_string();
                out.push(Event::SessionStarted {
                    tag: tag.clone(),
                    thread: thread.clone(),
                });
                if let Err(e) = self.items_list(&thread, &cwd, Some((tag.clone(), prompt))) {
                    out.push(failed(tag, e.into()));
                }
            }
            (Pending::History { thread, cwd, then }, res) => {
                match &res {
                    Ok(r) => {
                        let data = json::get(r, "data").and_then(json::as_arr).unwrap_or(&[]);
                        let ops = history_ops(data.iter().rev(), &cwd);
                        out.extend(ops.into_iter().map(|op| Event::Op(thread.clone(), op)));
                    }
                    Err(e) => {
                        let text = format!("The history of this session could not be read: {e}");
                        out.push(self.notice(&thread, "warning", &text, None));
                    }
                }
                if let Some((tag, prompt)) = then {
                    let text = "Forked from a Codex thread; the original thread is unchanged";
                    out.push(self.notice(&thread, "info", text, None));
                    self.open_thread(&thread, cwd, None, tag, prompt, out);
                }
            }
            (Pending::Rename { tag, thread, title }, Ok(_)) => {
                out.push(Event::Renamed { thread, title });
                out.push(done(tag, "completed", None));
            }
            (
                Pending::ThreadResume {
                    tag,
                    thread,
                    cwd,
                    prompt,
                },
                res,
            ) => {
                self.resuming.remove(&thread);
                match res {
                    Ok(r) => {
                        let model = json::get_str(&r, "model").map(str::to_string);
                        self.open_thread(&thread, cwd, model, tag, prompt, out);
                    }
                    // the thread is gone for good: the session ends
                    Err(e) => {
                        let why = format!("Codex could not resume this session: {e}");
                        out.push(failed(tag, why.clone()));
                        self.closed(thread, None, &why, "ended", out);
                    }
                }
            }
            (Pending::TurnStart { tag, thread }, Ok(r)) => {
                match json::get(&r, "turn").and_then(|t| json::get_str(t, "id")) {
                    Some(turn) => self.bind(tag, &thread, turn, false, out),
                    None => out.push(failed(tag, "turn/start returned no turn".into())),
                }
            }
            (Pending::Steer { tag, thread, .. }, Ok(r)) => match json::get_str(&r, "turnId") {
                Some(turn) => self.bind(tag, &thread, turn, false, out),
                None => out.push(failed(tag, "turn/steer returned no turn".into())),
            },
            (
                Pending::Steer {
                    tag,
                    thread,
                    prompt,
                },
                Err(e),
            ) if e.contains("no active turn") => {
                // the turn ended before the steer landed: start one with the same input
                if let Some(t) = self.threads.get_mut(&thread) {
                    t.turn = None;
                }
                let params = json::obj(vec![
                    ("threadId", json::s(&thread)),
                    ("input", input(&prompt)),
                ]);
                let pending = Pending::TurnStart {
                    tag: tag.clone(),
                    thread,
                };
                if let Err(e) = self.call("turn/start", Some(params), pending) {
                    out.push(failed(tag, e.into()));
                }
            }
            (Pending::Interrupt { tag, thread, turn }, Ok(_)) => {
                out.push(Event::Progress {
                    tag: tag.clone(),
                    status: "accepted",
                });
                self.bind(tag, &thread, &turn, true, out);
            }
            // nothing left to stop
            (Pending::Interrupt { tag, .. }, Err(e)) if e.contains("no active turn") => {
                out.push(done(tag, "completed", None))
            }
            (Pending::McpLogin { tag, server }, Ok(r)) => {
                match json::get_str(&r, "authorizationUrl") {
                    Some(url) => {
                        // codex cancels the previous login for this server
                        if let Some(old) = self.logins.insert(server, tag.clone()) {
                            out.push(failed(old, "replaced by a newer sign-in".into()));
                        }
                        out.push(Event::OpenUrl {
                            tag: tag.clone(),
                            url: url.into(),
                        });
                        out.push(Event::Progress {
                            tag,
                            status: "running",
                        });
                        self.emit_mcp(out);
                    }
                    None => out.push(failed(tag, "codex returned no sign-in URL".into())),
                }
            }
            (Pending::McpReload { tag, server }, Ok(_)) => {
                self.reconnects.push((server, tag, false));
                self.list();
            }
            (pending, Err(e)) => {
                if let Some((tag, _)) = pending.tag() {
                    out.push(failed(tag.into(), e));
                }
            }
        }
    }

    fn thread_started(
        &mut self,
        r: &Json,
        tag: String,
        cwd: String,
        prompt: Prompt,
        out: &mut Vec<Event>,
    ) {
        let Some(thread) = json::get(r, "thread").and_then(|t| json::get_str(t, "id")) else {
            out.push(done(
                tag,
                "failed",
                Some("thread/start returned no thread".into()),
            ));
            return;
        };
        let model = json::get_str(r, "model").map(str::to_string);
        out.push(Event::SessionStarted {
            tag: tag.clone(),
            thread: thread.into(),
        });
        self.open_thread(thread, cwd, model, tag, prompt, out);
    }

    /// A thread codex started or resumed: idle, then `prompt` as command `tag`.
    fn open_thread(
        &mut self,
        thread: &str,
        cwd: String,
        model: Option<String>,
        tag: String,
        prompt: Prompt,
        out: &mut Vec<Event>,
    ) {
        let state = Thread {
            cwd,
            model_override: None,
            model,
            status: "idle",
            context: None,
            turn: None,
            last: None,
        };
        self.threads.insert(thread.into(), state);
        out.push(self.state_op(thread, "idle"));
        out.push(Event::Op(thread.into(), Op::Mcp(self.mcp_view())));
        if let Err(e) = self.send_prompt(thread, prompt, &tag, out) {
            out.push(done(tag, "failed", Some(e.into())));
        }
    }

    /// Command `tag` now rides on `turn` and ends when the turn does.
    fn bind(
        &mut self,
        tag: String,
        thread: &str,
        turn: &str,
        interrupt: bool,
        out: &mut Vec<Event>,
    ) {
        let Some(t) = self.threads.get_mut(thread) else {
            return;
        };
        // the reply raced the turn's end
        if let Some(last) = t.last.as_ref().filter(|l| l.id == turn) {
            out.push(done_for(tag, interrupt, last));
            return;
        }
        if !interrupt {
            t.turn = Some(turn.into());
            out.push(Event::Progress {
                tag: tag.clone(),
                status: "running",
            });
        }
        self.turn_entry(thread, turn).tags.push((tag, interrupt));
    }

    fn turn_entry(&mut self, thread: &str, turn: &str) -> &mut Turn {
        self.turns.entry(turn.into()).or_insert_with(|| Turn {
            thread: thread.into(),
            state: TurnState {
                id: turn.into(),
                status: "running",
                started_at: now_ms(),
                ended_at: None,
                usage: None,
                cost_usd: None,
                error: None,
                diffs: Vec::new(),
            },
            tags: Vec::new(),
        })
    }

    /// The live turn's state, or the thread's last one for late updates.
    fn turn_state(&mut self, thread: &str, turn: &str) -> Option<&mut TurnState> {
        if let Some(t) = self.turns.get_mut(turn) {
            return Some(&mut t.state);
        }
        let last = self.threads.get_mut(thread)?.last.as_mut()?;
        (last.id == turn).then_some(last)
    }

    /* ----------------------- server requests ---------------------- */

    fn server_request(&mut self, method: &str, id: &Json, p: &Json, out: &mut Vec<Event>) {
        let thread = self.known(p);
        let turn = json::get_str(p, "turnId");
        match method {
            "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/tool/requestUserInput" => match (thread, json::get_str(p, "itemId")) {
                (Some(thread), Some(item)) => self.ask(method, id, p, &thread, item, out),
                _ => self.send(reply_error(id, "Invalid request: unknown thread or item")),
            },
            // Local intervention (PRD §13.5): grant nothing from the phone
            "item/permissions/requestApproval" => {
                let grant = json::obj(vec![
                    ("permissions", json::obj(vec![])),
                    ("scope", json::s("turn")),
                ]);
                self.send(reply(id, grant));
                if let Some(thread) = thread {
                    let why =
                        json::get_str(p, "reason").map_or(String::new(), |r| format!(" ({r})"));
                    let text = format!(
                        "Codex asked for extra sandbox permissions{why}. Nothing was granted from the \
                         phone: grant extra sandbox permissions on the desktop."
                    );
                    out.push(self.notice(&thread, "warning", &text, turn));
                }
            }
            // MCP forms are a later step (§8)
            "mcpServer/elicitation/request" => {
                let declined = json::obj(vec![
                    ("action", json::s("decline")),
                    ("content", Json::Null),
                    ("_meta", Json::Null),
                ]);
                self.send(reply(id, declined));
                if let Some(thread) = thread {
                    let server = json::get_str(p, "serverName").unwrap_or("an MCP server");
                    let message = json::get_str(p, "message").unwrap_or("");
                    let text = format!(
                        "MCP server {server} asked for input (\"{}\"); it was declined. Answer MCP forms on \
                         the desktop.",
                        clip(message, CONTEXT_CAP).0
                    );
                    out.push(self.notice(&thread, "warning", &text, turn));
                }
            }
            _ => self.send(reply_error(
                id,
                &format!("Invalid request: unsupported method `{method}`"),
            )),
        }
    }

    /// An approval: a `PermissionRequest` for the engine plus its tool card.
    fn ask(
        &mut self,
        method: &str,
        id: &Json,
        p: &Json,
        thread: &str,
        item: &str,
        out: &mut Vec<Event>,
    ) {
        let rid = match id {
            Json::Int(n) => n.to_string(),
            Json::Str(s) => s.clone(),
            _ => return self.send(reply_error(id, "Invalid request: bad id")),
        };
        let perm = format!("{}:{rid}", self.adapter);
        let turn = json::get_str(p, "turnId").unwrap_or_default().to_string();
        let reason = redact(json::get_str(p, "reason").unwrap_or(""));
        let now = now_ms();
        let mut pr = PermissionRequest {
            id: perm.clone(),
            native_request_id: rid,
            session_id: thread.into(),
            agent_id: self.adapter.clone(),
            host_id: self.adapter.clone(),
            action: "agent-control".into(),
            title: String::new(),
            command: String::new(),
            args: Vec::new(),
            directory: self.threads.get(thread).map(|t| t.cwd.clone()),
            risk: "low".into(),
            status: "pending".into(),
            supported_decisions: Vec::new(),
            revision: 1,
            fingerprint: None,
            requested_at: now,
            // `Engine::ingest_permission` sets the runtime TTL (ADR §4.1)
            expires_at: now,
            context: clip(&reason, CONTEXT_CAP).0,
            decided_at: None,
            decision: None,
            from_pc: false,
            tool_item_id: Some(item.into()),
            options: Vec::new(),
            detail: None,
        };
        let (kind, card) = match method {
            "item/commandExecution/requestApproval" => {
                let command = redact(json::get_str(p, "command").unwrap_or(""));
                pr.action = "shell".into();
                pr.risk = "high".into();
                pr.title = if json::get_str(p, "kind") == Some("writeStdin") {
                    "Codex wants to send input to a running command"
                } else {
                    "Codex wants to run a command"
                }
                .into();
                // the headline is what codex parsed out of its shell wrapper;
                // the detail keeps the full command line that runs
                let title = command_title(p).unwrap_or_else(|| first_line(&command));
                pr.command = clip(&title, COMMAND_CAP).0;
                pr.detail = Some(detail_shell(&command, json::get_str(p, "cwd"), &reason));
                // an empty prefix would allow every command: never offered
                let amendment = json::get(p, "proposedExecpolicyAmendment")
                    .filter(|a| {
                        json::as_arr(a).is_some_and(|v| {
                            !v.is_empty() && v.iter().all(|t| json::as_str(t).is_some())
                        })
                    })
                    .cloned();
                // a decision codex did not list is never offered
                let available = json::get(p, "availableDecisions")
                    .and_then(json::as_arr)
                    .map(|list| list.iter().filter_map(decision_name).collect());
                let mut card = new_tool("shell", ToolStatus::Running);
                card.title = Some(title);
                card.input = command;
                (
                    AskKind::Command {
                        amendment,
                        available,
                    },
                    card,
                )
            }
            "item/fileChange/requestApproval" => {
                // the diff came with item/started (codex starts the item, then asks)
                let diffs = self
                    .items
                    .get(item)
                    .and_then(|l| l.item.tool.as_ref())
                    .map(|t| t.diffs.clone())
                    .unwrap_or_default();
                pr.action = "file-write".into();
                pr.risk = "medium".into();
                pr.title = "Codex wants to change files".into();
                pr.command = clip(&paths_title(&diffs), COMMAND_CAP).0;
                pr.detail = Some(detail_edit(&diffs));
                let mut card = new_tool("apply_patch", ToolStatus::Running);
                card.title = Some(paths_title(&diffs));
                card.diffs = diffs;
                (AskKind::FileChange, card)
            }
            _ => {
                let questions = json::get(p, "questions")
                    .and_then(json::as_arr)
                    .unwrap_or(&[]);
                let first = questions
                    .first()
                    .and_then(|q| json::get_str(q, "question"))
                    .map(redact)
                    .unwrap_or_default();
                pr.title = "Codex has a question".into();
                pr.command = clip(&first, COMMAND_CAP).0;
                pr.detail = Some(detail_question(questions));
                let ids = questions
                    .iter()
                    .filter_map(|q| json::get_str(q, "id").map(str::to_string))
                    .collect();
                let mut card = new_tool("request_user_input", ToolStatus::Running);
                card.title = Some(first);
                (AskKind::Question { ids }, card)
            }
        };
        pr.options = options(&kind);
        for o in &pr.options {
            // legacy decisions select only non-persistent options (§4.1)
            if o.scope != "persistent" && !pr.supported_decisions.contains(&o.decision) {
                pr.supported_decisions.push(o.decision.clone());
            }
        }
        // ingest first, then the card that points at it (§4.5)
        out.push(Event::Permission(Box::new(pr)));
        let live = self.items.entry(item.into()).or_insert_with(|| {
            let mut it = new_item(item, ItemKind::Tool, Some(turn.clone()));
            it.tool = Some(card);
            Live::new(thread, it)
        });
        if let Some(t) = live.item.tool.as_mut() {
            t.status = ToolStatus::AwaitingApproval;
            t.permission_id = Some(perm.clone());
        }
        out.push(Event::Op(thread.into(), Op::Item(live.item.clone())));
        let ask = Ask {
            id: id.clone(),
            turn,
            item: item.into(),
            kind,
        };
        self.asks.insert(perm, ask);
    }

    /// Answered elsewhere (or ended): never answered by us afterwards.
    fn withdraw(&mut self, perm: &str, out: &mut Vec<Event>) {
        let Some(ask) = self.asks.remove(perm) else {
            return;
        };
        out.push(Event::Withdrawn(perm.into()));
        // the outcome of a command or edit follows in item/completed
        let question = matches!(ask.kind, AskKind::Question { .. });
        let status = if question {
            ToolStatus::Cancelled
        } else {
            ToolStatus::Running
        };
        out.extend(self.set_card(&ask.item, status, question));
    }

    /// Re-emits a cached tool card with a new status; `last` drops it from
    /// the cache (no item/completed will follow).
    fn set_card(&mut self, item: &str, status: ToolStatus, last: bool) -> Option<Event> {
        let live = self.items.get_mut(item)?;
        live.item.tool.as_mut()?.status = status;
        let event = Event::Op(live.thread.clone(), Op::Item(live.item.clone()));
        if last {
            self.items.remove(item);
        }
        Some(event)
    }

    /* ------------------------ notifications ----------------------- */

    /// The params' thread, if it is one of ours.
    fn known(&self, p: &Json) -> Option<String> {
        json::get_str(p, "threadId")
            .filter(|t| self.threads.contains_key(*t))
            .map(str::to_string)
    }

    fn notification(&mut self, method: &str, p: &Json, out: &mut Vec<Event>) {
        match method {
            "mcpServer/startupStatus/updated" => return self.list(),
            "mcpServer/oauthLogin/completed" => {
                if let Some(tag) = json::get_str(p, "name").and_then(|n| self.logins.remove(n)) {
                    if json::get_bool(p, "success") == Some(true) {
                        out.push(done(tag, "completed", None));
                    } else {
                        let why = json::get_str(p, "error").unwrap_or("sign-in failed");
                        out.push(done(tag, "failed", Some(redact(why))));
                    }
                }
                return self.list();
            }
            "serverRequest/resolved" => {
                let rid = match json::get(p, "requestId") {
                    Some(Json::Int(n)) => n.to_string(),
                    Some(Json::Str(s)) => s.clone(),
                    _ => return,
                };
                // after our own answer it is gone already: a no-op
                return self.withdraw(&format!("{}:{rid}", self.adapter), out);
            }
            _ => {}
        }
        let Some(thread) = self.known(p) else {
            return;
        };
        let turn = json::get_str(p, "turnId").unwrap_or_default();
        match method {
            "item/started" | "item/completed" => {
                self.item(thread, p, method == "item/completed", out)
            }
            "item/agentMessage/delta" | "item/plan/delta" => {
                if let (Some(live), Some(delta)) = (self.live(p), json::get_str(p, "delta")) {
                    out.push(append_text(live, delta, TEXT_CAP));
                }
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                let summary = method.ends_with("summaryTextDelta");
                let (Some(live), Some(delta)) = (self.live(p), json::get_str(p, "delta")) else {
                    return;
                };
                // summaries win: raw text after one is ignored, raw text before one is replaced
                if live.summary == Some(true) && !summary {
                    return;
                }
                if live.summary == Some(false) && summary {
                    live.item.text.clear();
                    out.push(Event::Op(live.thread.clone(), Op::Item(live.item.clone())));
                }
                live.summary = Some(summary);
                if live.shown {
                    out.push(append_text(live, delta, THINKING_CAP));
                } else {
                    // the first words of an item the phone has not seen yet
                    live.shown = true;
                    (live.item.text, live.item.truncated) = clip(delta, THINKING_CAP);
                    let mut it = live.item.clone();
                    it.text = redact(&it.text);
                    out.push(Event::Op(live.thread.clone(), Op::Item(it)));
                }
            }
            "item/reasoning/summaryPartAdded" => {
                if let Some(live) = self.live(p)
                    && live.summary == Some(true)
                    && !live.item.text.is_empty()
                {
                    out.push(append_text(live, "\n\n", THINKING_CAP));
                }
            }
            "item/commandExecution/outputDelta" => {
                if let (Some(live), Some(delta)) = (self.live(p), json::get_str(p, "delta")) {
                    out.extend(output_delta(live, delta));
                }
            }
            "turn/started" => {
                let Some(id) = json::get(p, "turn").and_then(|t| json::get_str(t, "id")) else {
                    return;
                };
                if let Some(t) = self.threads.get_mut(&thread)
                    && t.last.as_ref().is_none_or(|l| l.id != id)
                {
                    t.turn = Some(id.into());
                }
                let started = json::get(p, "turn")
                    .and_then(|t| json::get_i64(t, "startedAt"))
                    .map(|secs| secs * 1000);
                let t = self.turn_entry(&thread, id);
                if let Some(ms) = started {
                    t.state.started_at = ms;
                }
                out.push(Event::Op(thread, Op::Turn(t.state.clone())));
            }
            "turn/completed" => self.turn_completed(thread, p, out),
            "turn/diff/updated" => {
                let diffs = split_turn_diff(json::get_str(p, "diff").unwrap_or(""));
                if let Some(state) = self.turn_state(&thread, turn) {
                    state.diffs = diffs;
                    let state = state.clone();
                    out.push(Event::Op(thread, Op::Turn(state)));
                }
            }
            "thread/tokenUsage/updated" => {
                let usage = json::get(p, "tokenUsage");
                let Some(last) = usage.and_then(|u| json::get(u, "last")) else {
                    return;
                };
                // ADR-017 context meter: what the last request put in the window
                let window = usage.and_then(|u| json::get_i64(u, "modelContextWindow"));
                if let (Some(max), Some(used)) = (window, json::get_i64(last, "totalTokens"))
                    && let Some(t) = self.threads.get_mut(&thread)
                    && max > 0
                {
                    t.context = Some((used, max));
                    let status = t.status;
                    out.push(self.state_op(&thread, status));
                }
                let usage = Usage {
                    input: json::get_i64(last, "inputTokens").unwrap_or(0),
                    output: json::get_i64(last, "outputTokens").unwrap_or(0),
                    cache_read: json::get_i64(last, "cachedInputTokens"),
                    reasoning: json::get_i64(last, "reasoningOutputTokens"),
                };
                if let Some(state) = self.turn_state(&thread, turn) {
                    state.usage = Some(usage);
                    let state = state.clone();
                    out.push(Event::Op(thread, Op::Turn(state)));
                }
            }
            "turn/plan/updated" => {
                let steps = json::get(p, "plan").and_then(json::as_arr).unwrap_or(&[]);
                let mut plan = new_item(&format!("plan:{turn}"), ItemKind::Plan, Some(turn.into()));
                plan.steps = steps
                    .iter()
                    .map(|s| PlanStep {
                        text: redact(json::get_str(s, "step").unwrap_or("")),
                        status: match json::get_str(s, "status") {
                            Some("inProgress") => "in_progress",
                            Some("completed") => "completed",
                            _ => "pending",
                        },
                    })
                    .collect();
                out.push(Event::Op(thread, Op::Item(plan)));
            }
            "thread/status/changed" => {
                let status = json::get(p, "status");
                let status = match status.and_then(|s| json::get_str(s, "type")) {
                    Some("idle") => "idle",
                    Some("active") => {
                        let flags = status.and_then(|s| json::get(s, "activeFlags"));
                        if flags.and_then(json::as_arr).is_some_and(|f| !f.is_empty()) {
                            "waiting"
                        } else {
                            "running"
                        }
                    }
                    _ => return,
                };
                if let Some(t) = self.threads.get_mut(&thread) {
                    t.status = status;
                }
                out.push(self.state_op(&thread, status));
            }
            "thread/name/updated" => {
                if let Some(title) = json::get_str(p, "threadName") {
                    let title = clip(&redact(title), TITLE_CAP).0;
                    out.push(Event::Renamed { thread, title });
                }
            }
            "error" => {
                let message = json::get(p, "error").and_then(|e| json::get_str(e, "message"));
                let level = if json::get_bool(p, "willRetry") == Some(true) {
                    "warning"
                } else {
                    "error"
                };
                let text = message.unwrap_or("Codex reported an error");
                out.push(self.notice(&thread, level, text, Some(turn)));
            }
            _ => {}
        }
    }

    /// The in-flight item a delta names.
    fn live(&mut self, p: &Json) -> Option<&mut Live> {
        self.items.get_mut(json::get_str(p, "itemId")?)
    }

    fn item(&mut self, thread: String, p: &Json, completed: bool, out: &mut Vec<Event>) {
        let Some(item) = json::get(p, "item") else {
            return;
        };
        let (Some(id), Some(kind)) = (json::get_str(item, "id"), json::get_str(item, "type"))
        else {
            return;
        };
        if kind == "userMessage" {
            // codex echoes the input; the bubble was made when the prompt went out
            return;
        }
        let prev = self.items.remove(id);
        let cwd = self.threads.get(&thread).map_or("", |t| t.cwd.as_str());
        let turn = json::get_str(p, "turnId");
        let Some(it) = decode(item, turn, completed, prev.as_ref().map(|l| &l.item), cwd) else {
            return;
        };
        if it.kind == ItemKind::Notice {
            return out.push(Event::Op(thread, Op::Item(it)));
        }
        // an empty reasoning item waits for its first words (codex sends many)
        let shown = it.kind != ItemKind::Thinking
            || !it.text.is_empty()
            || prev.as_ref().is_some_and(|l| l.shown);
        if shown {
            out.push(Event::Op(thread.clone(), Op::Item(it.clone())));
        }
        if !completed {
            let mut live = Live::new(&thread, it);
            live.shown = shown;
            self.items.insert(id.into(), live);
        }
    }

    fn turn_completed(&mut self, thread: String, p: &Json, out: &mut Vec<Event>) {
        let Some(turn) = json::get(p, "turn") else {
            return;
        };
        let Some(id) = json::get_str(turn, "id") else {
            return;
        };
        self.turn_entry(&thread, id);
        let mut t = self.turns.remove(id).expect("inserted above");
        t.state.status = match json::get_str(turn, "status") {
            Some("interrupted") => "interrupted",
            Some("failed") => "failed",
            _ => "completed",
        };
        t.state.ended_at = Some(now_ms());
        t.state.error = json::get(turn, "error")
            .and_then(|e| json::get_str(e, "message"))
            .map(redact);
        // codex resolves open requests first (serverRequest/resolved); backstop
        let stale: Vec<String> = self
            .asks
            .iter()
            .filter(|(_, a)| a.turn == id)
            .map(|(k, _)| k.clone())
            .collect();
        for perm in stale {
            self.withdraw(&perm, out);
        }
        self.finish_items(Some(id), out);
        out.push(Event::Op(thread.clone(), Op::Turn(t.state.clone())));
        for (tag, interrupt) in t.tags {
            out.push(done_for(tag, interrupt, &t.state));
        }
        if let Some(th) = self.threads.get_mut(&thread) {
            if th.turn.as_deref() == Some(id) {
                th.turn = None;
            }
            th.last = Some(t.state);
        }
    }

    /// Items an ended turn left without item/completed (codex does so on
    /// interrupt), or every item (`None`, exit): tools are cancelled, text is
    /// final as streamed.
    fn finish_items(&mut self, turn: Option<&str>, out: &mut Vec<Event>) {
        let ended = self
            .items
            .extract_if(|_, l| turn.is_none() || l.item.turn_id.as_deref() == turn);
        for (_, mut l) in ended.filter(|(_, l)| l.shown) {
            match l.item.tool.as_mut() {
                Some(t) => t.status = ToolStatus::Cancelled,
                None => {
                    l.item.done = true;
                    l.item.text = redact(&l.item.text);
                }
            }
            out.push(Event::Op(l.thread, Op::Item(l.item)));
        }
    }

    fn notice(
        &mut self,
        thread: &str,
        level: &'static str,
        text: &str,
        turn: Option<&str>,
    ) -> Event {
        self.seq += 1;
        let mut it = new_item(
            &format!("notice:{}", self.seq),
            ItemKind::Notice,
            turn.filter(|t| !t.is_empty()).map(str::to_string),
        );
        it.level = Some(level);
        it.text = redact(text);
        Event::Op(thread.into(), Op::Item(it))
    }

    fn exit(&mut self, code: Option<i32>, out: &mut Vec<Event>) {
        self.exited = true;
        let why = code.map_or_else(
            || "Codex exited".to_string(),
            |c| format!("Codex exited with code {c}"),
        );
        let perms: Vec<String> = self.asks.keys().cloned().collect();
        for perm in perms {
            self.withdraw(&perm, out);
        }
        self.finish_items(None, out);
        // §4.2.4: prompts fail, a pending interrupt is unknown
        let fail = |tag: String, interrupt: bool| {
            let status = if interrupt { "unknown" } else { "failed" };
            done(tag, status, Some(why.clone()))
        };
        self.timers.clear();
        for (_, pending) in self.pending.drain() {
            if let Some((tag, interrupt)) = pending.tag() {
                out.push(fail(tag.into(), interrupt));
            }
        }
        for (_, tag) in self.logins.drain() {
            out.push(fail(tag, false));
        }
        for (_, tag, _) in self.reconnects.drain(..) {
            out.push(fail(tag, false));
        }
        for (_, mut t) in self.turns.drain() {
            for (tag, interrupt) in std::mem::take(&mut t.tags) {
                out.push(fail(tag, interrupt));
            }
            (t.state.status, t.state.ended_at, t.state.error) =
                ("failed", Some(now_ms()), Some(why.clone()));
            out.push(Event::Op(t.thread, Op::Turn(t.state)));
        }
        let threads: Vec<(String, Option<String>)> = self
            .threads
            .iter()
            .map(|(id, t)| (id.clone(), t.model.clone()))
            .collect();
        // codex keeps its threads on disk: the next prompt resumes them
        // (ADR-015 §10), so they idle instead of ending
        let resumable = format!("{why}; the next prompt resumes this session");
        for (thread, model) in threads {
            self.closed(thread, model, &resumable, "idle", out);
        }
        // a resume cut short by the exit is a failed resume: those sessions end
        for thread in std::mem::take(&mut self.resuming) {
            self.closed(thread, None, &why, "ended", out);
        }
    }

    /// An error notice, then the thread's state once its process (`idle`)
    /// or its resume (`ended`) is gone.
    fn closed(
        &mut self,
        thread: String,
        model: Option<String>,
        why: &str,
        status: &'static str,
        out: &mut Vec<Event>,
    ) {
        out.push(self.notice(&thread, "error", why, None));
        let state = LiveState {
            status,
            permission_mode: None,
            model,
            ..Default::default()
        };
        out.push(Event::Op(thread, Op::State(state)));
    }

    /* ----------------------------- MCP ---------------------------- */

    fn list(&mut self) {
        if self.listing {
            self.relist = true;
        } else if self.page(None, Vec::new()).is_ok() {
            self.listing = true;
            // what this list reports comes after every reload answered so far
            for r in &mut self.reconnects {
                r.2 = true;
            }
        }
    }

    fn page(&mut self, cursor: Option<&str>, servers: Vec<McpServer>) -> Result<(), &'static str> {
        let params = match cursor {
            Some(c) => json::obj(vec![("cursor", json::s(c))]),
            None => json::obj(vec![]),
        };
        self.call(
            "mcpServerStatus/list",
            Some(params),
            Pending::McpList(servers),
        )
    }

    /// `mcp_reconnect` outcomes (§4.6): connected → completed; still
    /// starting → wait for the next list; anything else → failed.
    fn settle_reconnects(&mut self, out: &mut Vec<Event>) {
        let mcp = &self.mcp;
        self.reconnects.retain(|(server, tag, armed)| {
            if !*armed {
                return true;
            }
            let status = mcp.iter().find(|s| s.name == *server).map(|s| s.status);
            let (status, error) = match status {
                Some("pending") => return true,
                Some("connected") => ("completed", None),
                Some(other) => (
                    "failed",
                    Some(format!("{server} is {other} after the reload")),
                ),
                None => (
                    "failed",
                    Some(format!("Codex has no MCP server named {server}")),
                ),
            };
            out.push(done(tag.clone(), status, error));
            false
        });
    }

    /// The servers as the phone sees them: a sign-in in progress shows on its server.
    fn mcp_view(&self) -> Vec<McpServer> {
        let waiting = McpAuthState {
            state: "awaiting-browser",
            url: None,
            callback_prefix: None,
            error: None,
        };
        let mut servers = self.mcp.clone();
        for s in servers
            .iter_mut()
            .filter(|s| self.logins.contains_key(&s.name))
        {
            s.auth = Some(waiting.clone());
        }
        servers
    }

    /// One MCP list for every session: codex runs one set of servers per process.
    fn emit_mcp(&self, out: &mut Vec<Event>) {
        let servers = self.mcp_view();
        out.extend(
            self.threads
                .keys()
                .map(|t| Event::Op(t.clone(), Op::Mcp(servers.clone()))),
        );
    }
}

impl Live {
    fn new(thread: &str, item: Item) -> Live {
        Live {
            thread: thread.into(),
            item,
            line: String::new(),
            forwarded: 0,
            summary: None,
            shown: true,
        }
    }
}

/* ------------------------------------------------------------------ */
/* Decoding helpers                                                    */
/* ------------------------------------------------------------------ */

pub(crate) fn new_item(id: &str, kind: ItemKind, turn: Option<String>) -> Item {
    Item {
        id: id.into(),
        kind,
        ts: now_ms(),
        turn_id: turn,
        parent_id: None,
        text: String::new(),
        done: false,
        truncated: false,
        queued: false,
        level: None,
        tool: None,
        steps: Vec::new(),
        images: 0,
    }
}

pub(crate) fn new_tool(name: &str, status: ToolStatus) -> ToolCall {
    ToolCall {
        name: name.into(),
        title: None,
        status,
        input: String::new(),
        output: String::new(),
        output_truncated: false,
        error: None,
        diffs: Vec::new(),
        permission_id: None,
        auto_approved: None,
        duration_ms: None,
    }
}

fn done(tag: String, status: &'static str, error: Option<String>) -> Event {
    Event::Done { tag, status, error }
}

/// G1 outcome of a command whose turn ended (§4.6): a prompt follows its
/// turn, an interrupt is done either way.
fn done_for(tag: String, interrupt: bool, turn: &TurnState) -> Event {
    match (interrupt, turn.status) {
        (true, _) | (false, "completed") => done(tag, "completed", None),
        (false, "interrupted") => done(tag, "cancelled", None),
        _ => done(tag, "failed", turn.error.clone()),
    }
}

/// Streams `delta` into a text or thinking item.
fn append_text(live: &mut Live, delta: &str, cap: usize) -> Event {
    if live.item.text.len() + delta.len() <= cap {
        live.item.text.push_str(delta);
    } else {
        live.item.truncated = true;
    }
    let op = Op::Append {
        id: live.item.id.clone(),
        field: Field::Text,
        text: redact(delta),
    };
    Event::Op(live.thread.clone(), op)
}

/// commandExecution output: whole lines only (a secret is never split across
/// two redactions), forwarding stops at [`OUTPUT_CAP`]; the completed item
/// brings head + tail of everything.
fn output_delta(live: &mut Live, delta: &str) -> Option<Event> {
    if live.forwarded >= OUTPUT_CAP {
        return None;
    }
    live.line.push_str(delta);
    let cut = match live.line.rfind(['\n', '\r']) {
        Some(i) => i + 1,
        None if live.line.len() >= LINE_FLUSH => live.line.len(),
        None => return None,
    };
    let budget = OUTPUT_CAP - live.forwarded;
    let (cut, last) = if cut >= budget {
        (live.line.floor_char_boundary(budget), true)
    } else {
        (cut, false)
    };
    let chunk: String = live.line.drain(..cut).collect();
    if last {
        live.forwarded = OUTPUT_CAP;
        live.line = String::new();
    } else {
        live.forwarded += chunk.len();
    }
    if chunk.is_empty() {
        return None;
    }
    let text = redact(&chunk);
    if let Some(t) = live.item.tool.as_mut() {
        t.output.push_str(&text);
    }
    let op = Op::Append {
        id: live.item.id.clone(),
        field: Field::Output,
        text,
    };
    Some(Event::Op(live.thread.clone(), op))
}

/// A codex ThreadItem as a tool card (§3.6). `prev` is the card the phone
/// has already: its approval link and the output streamed so far.
fn tool_call(
    kind: &str,
    item: &Json,
    completed: bool,
    prev: Option<&ToolCall>,
    cwd: &str,
) -> ToolCall {
    let status = match json::get_str(item, "status") {
        Some("completed") => ToolStatus::Completed,
        Some("failed") => ToolStatus::Failed,
        Some("declined") => ToolStatus::Denied,
        Some("inProgress") => ToolStatus::Running,
        _ if completed => ToolStatus::Completed,
        _ => ToolStatus::Running,
    };
    let mut t = new_tool(kind, status);
    if let Some(prev) = prev {
        t.permission_id = prev.permission_id.clone();
        (t.output, t.output_truncated) = (prev.output.clone(), prev.output_truncated);
    }
    t.duration_ms = json::get_i64(item, "durationMs");
    match kind {
        "commandExecution" => {
            let command = redact(json::get_str(item, "command").unwrap_or(""));
            t.name = "shell".into();
            t.title = Some(command_title(item).unwrap_or_else(|| first_line(&command)));
            if let Some(all) = json::get_str(item, "aggregatedOutput") {
                (t.output, t.output_truncated) = head_tail(&redact(all), OUTPUT_CAP);
            }
            t.error = json::get_i64(item, "exitCode")
                .filter(|c| *c != 0)
                .map(|c| format!("exit code {c}"));
            t.input = command;
        }
        "fileChange" => {
            t.name = "apply_patch".into();
            let changes = json::get(item, "changes")
                .and_then(json::as_arr)
                .unwrap_or(&[]);
            t.diffs = changes.iter().filter_map(|c| change_diff(c, cwd)).collect();
            t.title = Some(paths_title(&t.diffs));
        }
        "mcpToolCall" => {
            let server = json::get_str(item, "server").unwrap_or("?");
            t.name = format!("{server}/{}", json::get_str(item, "tool").unwrap_or("?"));
            t.input = json::get(item, "arguments")
                .map(|a| redact(&json::stringify(a)))
                .unwrap_or_default();
            if let Some(result) = json::get(item, "result").filter(|r| **r != Json::Null) {
                (t.output, t.output_truncated) = head_tail(&redact(&mcp_text(result)), OUTPUT_CAP);
            }
            t.error = json::get(item, "error")
                .and_then(|e| json::get_str(e, "message"))
                .map(redact);
        }
        _ => {}
    }
    t
}

/// An MCP CallToolResult's text parts; other content as JSON.
fn mcp_text(result: &Json) -> String {
    match json::get(result, "content").and_then(json::as_arr) {
        Some(parts) => parts
            .iter()
            .map(|c| json::get_str(c, "text").map_or_else(|| json::stringify(c), str::to_string))
            .collect::<Vec<_>>()
            .join("\n"),
        None => json::stringify(result),
    }
}

fn strings(v: Option<&Json>) -> Vec<&str> {
    v.and_then(json::as_arr)
        .unwrap_or(&[])
        .iter()
        .filter_map(json::as_str)
        .collect()
}

pub(crate) fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.len() < s.trim_end().len() {
        format!("{line} …")
    } else {
        line.into()
    }
}

/// A codex `ThreadItem` as a transcript item, for live items and history
/// alike. `prev` is what the phone already has of it. `None`: nothing to show.
fn decode(
    item: &Json,
    turn: Option<&str>,
    completed: bool,
    prev: Option<&Item>,
    cwd: &str,
) -> Option<Item> {
    let (id, kind) = (json::get_str(item, "id")?, json::get_str(item, "type")?);
    let mut it = new_item(id, ItemKind::Tool, turn.map(str::to_string));
    if let Some(ts) = prev
        .map(|p| p.ts)
        .or_else(|| json::get_i64(item, "startedAtMs"))
    {
        it.ts = ts;
    }
    match kind {
        "userMessage" => {
            let parts = json::get(item, "content")
                .and_then(json::as_arr)
                .unwrap_or(&[]);
            let text: Vec<&str> = parts
                .iter()
                .filter_map(|p| json::get_str(p, "text"))
                .collect();
            it.kind = ItemKind::User;
            (it.text, it.truncated) = clip(&redact(&text.join("\n")), TEXT_CAP);
            it.images = parts
                .iter()
                .filter(|p| matches!(json::get_str(p, "type"), Some("image" | "localImage")))
                .count() as u32;
        }
        "contextCompaction" => {
            it.kind = ItemKind::Notice;
            it.level = Some("info");
            it.text = "Context compacted".into();
        }
        "agentMessage" | "plan" => {
            it.kind = ItemKind::Text;
            let text = json::get_str(item, "text").unwrap_or("");
            (it.text, it.truncated) = clip(&redact(text), TEXT_CAP);
            it.done = completed;
        }
        "reasoning" => {
            it.kind = ItemKind::Thinking;
            let mut text = strings(json::get(item, "summary")).join("\n\n");
            if text.is_empty() {
                text = strings(json::get(item, "content")).join("\n\n");
            }
            (it.text, it.truncated) = clip(&redact(&text), THINKING_CAP);
            it.done = completed;
        }
        _ => {
            let prev_tool = prev.and_then(|p| p.tool.as_ref());
            it.tool = Some(tool_call(kind, item, completed, prev_tool, cwd));
        }
    }
    Some(it)
}

/// `thread/items/list` entries (oldest first) as a finished transcript:
/// tools still `inProgress` on disk were cut off, so they are cancelled.
fn history_ops<'a>(entries: impl Iterator<Item = &'a Json>, cwd: &str) -> Vec<Op> {
    let mut ops = Vec::new();
    for e in entries {
        let Some(item) = json::get(e, "item") else {
            continue;
        };
        let turn = json::get_str(e, "turnId");
        let Some(mut it) = decode(item, turn, true, None, cwd) else {
            continue;
        };
        if it.kind == ItemKind::Thinking && it.text.is_empty() {
            continue;
        }
        if let Some(t) = it.tool.as_mut().filter(|t| t.status == ToolStatus::Running) {
            t.status = ToolStatus::Cancelled;
        }
        it.done = true;
        ops.push(Op::Item(it));
    }
    ops.push(Op::State(LiveState::default()));
    ops
}

/// The phone's session title: at most this many bytes.
const TITLE_CAP: usize = 200;

/// One `thread/list` entry; subagent and ephemeral threads are not listed.
fn found_thread(t: &Json) -> Option<FoundThread> {
    if json::get_bool(t, "ephemeral") == Some(true) || json::get_str(t, "parentThreadId").is_some()
    {
        return None;
    }
    let named = json::get_str(t, "name").filter(|n| !n.trim().is_empty());
    let title = named
        .or_else(|| json::get_str(t, "preview"))
        .map(|s| {
            s.lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or("Codex thread");
    let secs = |k| json::get_i64(t, k).unwrap_or(0) * 1000;
    Some(FoundThread {
        id: json::get_str(t, "id")?.into(),
        title: clip(&redact(title), TITLE_CAP).0,
        cwd: json::get_str(t, "cwd")?.into(),
        created_at: secs("createdAt"),
        updated_at: secs("updatedAt"),
    })
}

/// What a command does, as codex parsed it out of its shell wrapper (live:
/// `"…\powershell.exe" -Command 'git add x'` → `git add x`).
fn command_title(v: &Json) -> Option<String> {
    let actions = json::get(v, "commandActions").and_then(json::as_arr)?;
    let parts: Vec<String> = actions
        .iter()
        .filter_map(|a| json::get_str(a, "command"))
        .map(|c| redact(&first_line(c)))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" && "))
}

/// The variant name of a `CommandExecutionApprovalDecision`.
fn decision_name(d: &Json) -> Option<String> {
    match d {
        Json::Str(s) => Some(s.clone()),
        Json::Obj(f) => f.first().map(|(k, _)| k.clone()),
        _ => None,
    }
}

/// A file codex names by absolute path (live), shown relative to the
/// thread's folder with forward slashes, as `turn/diff/updated` names it.
fn relative(path: &str, cwd: &str) -> String {
    // same byte lengths as the originals, so `c.len()` indexes `path`
    let norm = |s: &str| s.to_ascii_lowercase().replace('\\', "/");
    let (p, c) = (norm(path), norm(cwd.trim_end_matches(['\\', '/'])));
    if !c.is_empty() && p.len() > c.len() + 1 && p.starts_with(&c) && p.as_bytes()[c.len()] == b'/'
    {
        path[c.len() + 1..].replace('\\', "/")
    } else {
        path.into()
    }
}

pub(crate) fn paths_title(diffs: &[FileDiff]) -> String {
    match diffs {
        [] => String::new(),
        [one] => one.path.clone(),
        [first, rest @ ..] => format!("{} (+{} more)", first.path, rest.len()),
    }
}

/// One FileUpdateChange as a FileDiff. Codex sends a unified diff for
/// `update` (plus "\n\nMoved to: <path>" when renamed) and the whole file for
/// `add` and `delete` (item_builders.rs `format_file_change_diff`).
fn change_diff(change: &Json, cwd: &str) -> Option<FileDiff> {
    let path = &relative(json::get_str(change, "path")?, cwd);
    let kind = json::get(change, "kind");
    let raw = json::get_str(change, "diff").unwrap_or("");
    let moved = kind.and_then(|k| json::get_str(k, "move_path"));
    Some(match kind.and_then(|k| json::get_str(k, "type")) {
        Some("add") => file_diff(path, "added", &whole(raw, '+')),
        Some("delete") => file_diff(path, "deleted", &whole(raw, '-')),
        _ => match moved {
            Some(to) => {
                let trailer = format!("\n\nMoved to: {to}");
                let patch = raw.strip_suffix(&trailer).unwrap_or(raw);
                file_diff(&relative(to, cwd), "renamed", patch)
            }
            None => file_diff(path, "modified", raw),
        },
    })
}

/// A whole file as an all-added (`+`) or all-removed (`-`) hunk.
fn whole(content: &str, sign: char) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let n = lines.len();
    let mut out = if sign == '+' {
        format!("@@ -0,0 +1,{n} @@\n")
    } else {
        format!("@@ -1,{n} +0,0 @@\n")
    };
    for line in lines {
        out.push(sign);
        out.push_str(line);
        out.push('\n');
    }
    out
}

pub(crate) fn file_diff(path: &str, status: &'static str, patch: &str) -> FileDiff {
    let patch = redact(hunks(patch));
    let count = |sign: char| patch.lines().filter(|l| l.starts_with(sign)).count() as i64;
    FileDiff {
        path: path.into(),
        status,
        additions: count('+'),
        deletions: count('-'),
        truncated: false,
        patch,
    }
}

/// A FileDiff patch is hunks only: `diff --git`, `index`, `---`, `+++` go.
fn hunks(patch: &str) -> &str {
    if patch.starts_with("@@") {
        return patch;
    }
    patch.find("\n@@").map_or("", |i| &patch[i + 1..])
}

/// `turn/diff/updated`: the turn's aggregated git diff, one FileDiff per
/// `diff --git` header.
fn split_turn_diff(diff: &str) -> Vec<FileDiff> {
    let starts: Vec<usize> = diff
        .match_indices("diff --git ")
        .map(|(i, _)| i)
        .filter(|&i| i == 0 || diff.as_bytes()[i - 1] == b'\n')
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(k, &start)| {
            let chunk = &diff[start..starts.get(k + 1).copied().unwrap_or(diff.len())];
            let header = chunk.lines().next().unwrap_or("");
            let path = chunk
                .lines()
                .find_map(|l| l.strip_prefix("+++ b/"))
                .or_else(|| chunk.lines().find_map(|l| l.strip_prefix("--- a/")))
                .or_else(|| header.rsplit_once(" b/").map(|(_, p)| p))
                .unwrap_or("");
            let status = if chunk.contains("\nnew file mode") {
                "added"
            } else if chunk.contains("\ndeleted file mode") {
                "deleted"
            } else if chunk.contains("\nrename from ") {
                "renamed"
            } else {
                "modified"
            };
            file_diff(path, status, chunk)
        })
        .collect()
}

/// At most `max` bytes, cut at a char boundary; true when something was cut.
pub(crate) fn clip(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.into(), false);
    }
    (s[..s.floor_char_boundary(max)].into(), true)
}

/// Head and tail of `s` within `max` bytes (§3.5 tool output).
pub(crate) fn head_tail(s: &str, max: usize) -> (String, bool) {
    const CUT: &str = "\n…\n";
    if s.len() <= max {
        return (s.into(), false);
    }
    let half = (max - CUT.len()) / 2;
    let head = &s[..s.floor_char_boundary(half)];
    let tail = &s[s.ceil_char_boundary(s.len() - half)..];
    (format!("{head}{CUT}{tail}"), true)
}

/* ------------------------------------------------------------------ */
/* Approvals                                                           */
/* ------------------------------------------------------------------ */

/// The runtime's own choices (§4.3), labelled verbatim.
fn options(kind: &AskKind) -> Vec<PermissionOption> {
    let option = |id: &str, decision: &str, label: &str, scope: &'static str| PermissionOption {
        id: id.into(),
        decision: decision.into(),
        label: label.into(),
        rule: None,
        scope,
        destination: None,
        takes_message: false,
        stops_turn: false,
    };
    if let AskKind::Question { .. } = kind {
        return vec![
            option("answer", "approve-once", "Submit answers", "once"),
            option("skip", "reject", "Skip", "once"),
        ];
    }
    let mut offered = vec![
        option("accept", "approve-once", "Allow", "once"),
        option(
            "acceptForSession",
            "approve-session",
            "Allow for this session",
            "session",
        ),
    ];
    if let AskKind::Command {
        amendment: Some(a), ..
    } = kind
    {
        let rule = redact(&strings(Some(a)).join(" "));
        let label = format!("Always allow commands starting with `{rule}`");
        offered.push(PermissionOption {
            rule: Some(rule),
            ..option(
                "acceptWithExecpolicyAmendment",
                "approve-session",
                &label,
                "persistent",
            )
        });
    }
    offered.push(option("decline", "reject", "Deny", "once"));
    offered.push(PermissionOption {
        stops_turn: true,
        ..option("cancel", "reject", "Deny and stop", "once")
    });
    if let AskKind::Command {
        available: Some(names),
        ..
    } = kind
    {
        offered.retain(|o| names.contains(&o.id));
    }
    offered
}

/// The `result` of the one answer: `{decision}` with the amendment exactly as
/// proposed, or `{answers: {[id]: {answers}}}` for a question.
fn answer(kind: &AskKind, option: &str, answers: &[(String, Vec<String>)]) -> Json {
    match kind {
        AskKind::Question { ids } => {
            let picked = answers
                .iter()
                .filter(|(id, _)| option == "answer" && ids.contains(id))
                .map(|(id, a)| {
                    let list = Json::Arr(a.iter().map(|x| json::s(x)).collect());
                    (id.clone(), json::obj(vec![("answers", list)]))
                })
                .collect();
            json::obj(vec![("answers", Json::Obj(picked))])
        }
        AskKind::Command {
            amendment: Some(amendment),
            ..
        } if option == "acceptWithExecpolicyAmendment" => {
            let inner = json::obj(vec![("execpolicy_amendment", amendment.clone())]);
            json::obj(vec![(
                "decision",
                json::obj(vec![("acceptWithExecpolicyAmendment", inner)]),
            )])
        }
        _ => json::obj(vec![("decision", json::s(option))]),
    }
}

// PermissionDetail builders (ADR §3.2); only codex uses them so far.

pub(crate) fn detail_shell(command: &str, cwd: Option<&str>, reason: &str) -> Json {
    let mut fields = vec![
        ("kind", json::s("shell")),
        ("command", json::s(&clip(command, DETAIL_CAP).0)),
    ];
    if let Some(cwd) = cwd {
        fields.push(("cwd", json::s(cwd)));
    }
    if !reason.is_empty() {
        fields.push(("reason", json::s(&clip(reason, CONTEXT_CAP).0)));
    }
    json::obj(fields)
}

pub(crate) fn detail_edit(diffs: &[FileDiff]) -> Json {
    let patch: String = diffs
        .iter()
        .map(|d| format!("diff --git a/{0} b/{0}\n{1}", d.path, d.patch))
        .collect();
    json::obj(vec![
        ("kind", json::s("edit")),
        ("path", json::s(&paths_title(diffs))),
        ("patch", json::s(&clip(&patch, DETAIL_CAP).0)),
    ])
}

fn detail_question(questions: &[Json]) -> Json {
    let list = questions
        .iter()
        .map(|q| {
            let options = json::get(q, "options").and_then(json::as_arr);
            let choices = options
                .unwrap_or(&[])
                .iter()
                .map(|o| {
                    json::obj(vec![
                        ("label", json::s(json::get_str(o, "label").unwrap_or(""))),
                        (
                            "description",
                            json::s(json::get_str(o, "description").unwrap_or("")),
                        ),
                    ])
                })
                .collect();
            let free = json::get_bool(q, "isOther") == Some(true) || options.is_none();
            json::obj(vec![
                ("id", json::s(json::get_str(q, "id").unwrap_or(""))),
                (
                    "text",
                    json::s(&redact(json::get_str(q, "question").unwrap_or(""))),
                ),
                ("header", json::s(json::get_str(q, "header").unwrap_or(""))),
                ("options", Json::Arr(choices)),
                ("multiSelect", Json::Bool(false)),
                ("allowFreeText", Json::Bool(free)),
            ])
        })
        .collect();
    json::obj(vec![
        ("kind", json::s("question")),
        ("questions", Json::Arr(list)),
    ])
}

/// The servers of a `mcpServerStatus/list` page (§3.6 status values).
fn mcp_server(s: &Json) -> Option<McpServer> {
    let status = match json::get_str(s, "runtimeStatus") {
        Some("connected") => "connected",
        Some("authenticationRequired") => "needs-auth",
        Some("failed" | "cancelled") => "failed",
        Some("disabled") => "disabled",
        // notStarted, starting, or null while unknown
        _ => "pending",
    };
    // codex: "OAuth login is only supported for streamable HTTP servers"
    let http = json::get_str(s, "httpOrigin").is_some();
    let oauth = !matches!(
        json::get_str(s, "authStatus"),
        Some("unsupported" | "bearerToken")
    );
    Some(McpServer {
        name: json::get_str(s, "name")?.into(),
        status,
        error: json::get_str(s, "toolsError").map(redact),
        auth_flows: if http && oauth {
            vec!["desktop"]
        } else {
            Vec::new()
        },
        auth: None,
    })
}

/* ------------------------------------------------------------------ */
/* Frames                                                              */
/* ------------------------------------------------------------------ */

fn request(id: i64, method: &str, params: Option<Json>) -> String {
    let mut fields = vec![("id", Json::Int(id)), ("method", json::s(method))];
    if let Some(p) = params {
        fields.push(("params", p));
    }
    json::stringify(&json::obj(fields))
}

fn reply(id: &Json, result: Json) -> String {
    json::stringify(&json::obj(vec![("id", id.clone()), ("result", result)]))
}

fn reply_error(id: &Json, message: &str) -> String {
    let error = json::obj(vec![
        ("code", Json::Int(INVALID_REQUEST)),
        ("message", json::s(message)),
    ]);
    json::stringify(&json::obj(vec![("id", id.clone()), ("error", error)]))
}

/// `UserInput[]`: ADR-017 images first, as `data:` URLs (`{type: "image",
/// url}`, accepted live), then the text.
fn input(prompt: &Prompt) -> Json {
    let mut list: Vec<Json> = prompt
        .images
        .iter()
        .map(|im| {
            let url = format!("data:{};base64,{}", im.media_type, im.data);
            json::obj(vec![("type", json::s("image")), ("url", json::s(&url))])
        })
        .collect();
    list.push(json::obj(vec![
        ("type", json::s("text")),
        ("text", json::s(&prompt.text)),
    ]));
    Json::Arr(list)
}

/// `thread/start` | `thread/resume` | `thread/fork` params: the thread, its
/// folder, the model and the policy ([`APPROVAL_POLICY`], [`SANDBOX`]).
fn policy(thread: Option<&str>, cwd: &str, model: Option<&str>) -> Json {
    let mut f = Vec::new();
    if let Some(t) = thread {
        f.push(("threadId", json::s(t)));
    }
    f.push(("cwd", json::s(cwd)));
    if let Some(m) = model {
        f.push(("model", json::s(m)));
    }
    f.push(("approvalPolicy", json::s(APPROVAL_POLICY)));
    f.push(("sandbox", json::s(SANDBOX)));
    json::obj(f)
}

/// The id of a line json.rs refused (nesting past 32): codex writes `id`
/// first in requests and replies (rpc.rs field order). `true` = a request,
/// which still needs its one answer.
fn salvage(line: &str) -> Option<(i64, bool)> {
    let rest = line.strip_prefix("{\"id\":")?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let id = rest[..digits].parse().ok()?;
    let rest = &rest[digits..];
    if rest.starts_with(",\"method\":") {
        Some((id, true))
    } else if rest.starts_with(",\"result\":") {
        Some((id, false))
    } else {
        None
    }
}

/* ------------------------------------------------------------------ */
/* Launch configuration (§4.3, §4.2.3)                                 */
/* ------------------------------------------------------------------ */

/// A codex adapter entry: `{"type": "codex", "workspaces": [...],
/// "binary": null, "args": []}`.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchConfig {
    /// `None`: found by [`find_codex`] at every spawn, so an update of the
    /// Codex app (a new `bin\<hash>` folder) never breaks a configured agent.
    pub binary: Option<PathBuf>,
    /// Before `app-server`; lets tests run `node fake-codex-app-server.mjs`.
    pub args: Vec<String>,
    pub workspaces: Vec<LiveWorkspace>,
}

impl LaunchConfig {
    /// The binary to spawn now: the configured one, else [`find_codex`].
    pub fn resolve_binary(&self) -> Result<PathBuf, String> {
        match &self.binary {
            Some(b) => Ok(b.clone()),
            None => find_codex(
                std::env::var_os("PATH").as_deref(),
                std::env::var_os("LOCALAPPDATA")
                    .map(PathBuf::from)
                    .as_deref(),
            ),
        }
    }

    /// `<binary> <args…> app-server` for `ChildIo::spawn` (no shell; prompts
    /// go on stdin, never argv).
    pub fn argv(&self) -> Vec<String> {
        let mut argv = self.args.clone();
        argv.push("app-server".into());
        argv
    }

    /// `Agent.live`: follow-ups merge into the running turn via `turn/steer`.
    pub fn agent_live(&self) -> AgentLive {
        AgentLive {
            prompt_while_busy: "merge",
            interrupt: true,
            end: false,
            permission_modes: Vec::new(),
            workspaces: self.workspaces.clone(),
            composer: Vec::new(),
            models: Vec::new(),
        }
    }
}

/// Validates a codex adapter entry. Workspaces follow the Claude policy
/// (§4.2.3): absolute, existing, canonical folders; never a drive root, the
/// user profile or a folder containing the CHERNOBYL data directory.
pub fn launch_config(entry: &Json, data_dir: &Path) -> Result<LaunchConfig, String> {
    let binary = match json::get(entry, "binary") {
        None | Some(Json::Null) => None,
        Some(Json::Str(b)) if !b.is_empty() => Some(PathBuf::from(b)),
        _ => return Err("codex 'binary' must be a non-empty string or null".into()),
    };
    let args = match json::get(entry, "args") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Arr(a)) => a
            .iter()
            .map(|v| json::as_str(v).map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or("codex 'args' must be an array of strings")?,
        _ => return Err("codex 'args' must be an array of strings".into()),
    };
    let listed = json::get(entry, "workspaces")
        .and_then(json::as_arr)
        .ok_or("codex 'workspaces' must be an array of folders")?;
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let mut workspaces: Vec<LiveWorkspace> = Vec::new();
    for (i, w) in listed.iter().enumerate() {
        let raw =
            json::as_str(w).ok_or_else(|| format!("codex workspaces[{i}] must be a string"))?;
        let ws = workspace(raw, data_dir, profile.as_deref())
            .map_err(|e| format!("codex workspaces[{i}]: {e}"))?;
        if workspaces.iter().any(|w| w.id == ws.id) {
            return Err(format!("codex workspaces[{i}] repeats an earlier folder"));
        }
        workspaces.push(ws);
    }
    if workspaces.is_empty() {
        return Err("codex needs at least one workspace".into());
    }
    Ok(LaunchConfig {
        binary,
        args,
        workspaces,
    })
}

/// `codex.exe` when the config names none: the first real one on `path`,
/// else the newest the Codex desktop app installed
/// (`<local_app_data>\OpenAI\Codex\bin\<hash>\codex.exe`; the hash folder
/// changes with app updates, and some hold no codex.exe). The Store's 0-byte
/// app-execution aliases (anything under a `WindowsApps` folder) are never
/// taken: they are not the CLI and cannot be spawned with piped stdio.
pub fn find_codex(path: Option<&OsStr>, local_app_data: Option<&Path>) -> Result<PathBuf, String> {
    let real = |p: &Path| {
        p.metadata().is_ok_and(|m| m.is_file() && m.len() > 0)
            && !p
                .components()
                .any(|c| c.as_os_str().eq_ignore_ascii_case("WindowsApps"))
    };
    let on_path = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join("codex.exe"))
        .find(|p| real(p));
    if let Some(p) = on_path {
        return Ok(p);
    }
    let bin = local_app_data.map(|d| d.join("OpenAI").join("Codex").join("bin"));
    let newest = bin
        .and_then(|b| std::fs::read_dir(b).ok())
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let exe = e.ok()?.path().join("codex.exe");
            let modified = exe.metadata().ok()?.modified().ok()?;
            real(&exe).then_some((modified, exe))
        })
        .max_by_key(|(modified, _)| *modified);
    newest.map(|(_, exe)| exe).ok_or_else(|| {
        "Codex was not found: install the Codex app (or put codex.exe on PATH), or set \
         \"binary\" in companion.json"
            .into()
    })
}

// ponytail: the Claude driver grows the same policy in parallel; dedupe at merge.
fn workspace(raw: &str, data_dir: &Path, profile: Option<&Path>) -> Result<LiveWorkspace, String> {
    if !Path::new(raw).is_absolute() {
        return Err(format!("'{raw}' is not an absolute path"));
    }
    let path = canonical(Path::new(raw)).map_err(|_| format!("'{raw}' does not exist"))?;
    if !path.is_dir() {
        return Err(format!("'{raw}' is not a folder"));
    }
    if path.parent().is_none() {
        return Err(format!("'{raw}' is a drive root"));
    }
    if let Some(home) = profile.and_then(|h| canonical(h).ok())
        && within(&home, &path)
    {
        return Err(format!("'{raw}' is or contains the user profile folder"));
    }
    // an agent reads its cwd without asking: never the control key or journal
    let data = canonical(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    if within(&data, &path) {
        return Err(format!("'{raw}' contains the CHERNOBYL data folder"));
    }
    let text = path.to_string_lossy().into_owned();
    let hash = crypto::sha256(text.as_bytes());
    Ok(LiveWorkspace {
        id: hash[..6].iter().map(|b| format!("{b:02x}")).collect(),
        label: path
            .file_name()
            .map_or_else(|| text.clone(), |n| n.to_string_lossy().into_owned()),
        path: text,
    })
}

/// `fs::canonicalize` without Windows' `\\?\` prefix: codex gets the path as
/// its `cwd`, and the phone shows it.
fn canonical(p: &Path) -> io::Result<PathBuf> {
    let c = std::fs::canonicalize(p)?;
    let s = c.to_string_lossy();
    Ok(match s.strip_prefix(r"\\?\") {
        Some(unc) if unc.starts_with(r"UNC\") => PathBuf::from(format!(r"\\{}", &unc[4..])),
        Some(local) => PathBuf::from(local),
        None => c,
    })
}

/// `inner` is `outer` or inside it; Windows paths compare without case.
pub(crate) fn within(inner: &Path, outer: &Path) -> bool {
    let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
    lower(inner).starts_with(lower(outer))
}

/* ------------------------------------------------------------------ */
/* Tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Prompt {
        Prompt {
            text: text.into(),
            images: Vec::new(),
        }
    }
    use std::io::{BufRead, BufReader, Write};
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::{Duration, Instant};

    const WAIT: Duration = Duration::from_secs(5);
    const INIT_OK: &str = r#"{"id":1,"result":{"userAgent":"fake/0.0.0","codexHome":"C:\\codex","platformFamily":"windows","platformOs":"windows"}}"#;

    /// An in-memory codex over `std::io::pipe()` ends: it reads what the core
    /// writes and writes what codex would say (shapes from the pinned schema).
    struct Peer {
        frames: Receiver<String>,
        to_core: io::PipeWriter,
        events: Receiver<ChildEvent>,
    }

    fn connect() -> (Codex, Peer) {
        let (core_stdout, to_core) = io::pipe().unwrap();
        let (from_core, core_stdin) = io::pipe().unwrap();
        let (child, events) = ChildIo::from_pipes(core_stdout, core_stdin).unwrap();
        let (tx, frames) = mpsc::channel();
        thread::spawn(move || {
            // the core's own model/list (a background id) is not under test here
            let lines = BufReader::new(from_core).lines().map_while(Result::ok);
            for line in lines.filter(|l| !l.contains(r#""method":"model/list""#)) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let peer = Peer {
            frames,
            to_core,
            events,
        };
        (Codex::new(child, "codex-1", "9.9.9"), peer)
    }

    impl Peer {
        /// The next frame the core wrote to codex's stdin.
        fn recv(&self) -> String {
            self.frames
                .recv_timeout(WAIT)
                .expect("the core wrote a frame")
        }

        /// The core wrote nothing more.
        fn quiet(&self) {
            assert_eq!(
                self.frames.recv_timeout(Duration::from_millis(150)).ok(),
                None
            );
        }

        /// One codex line in, the core's events out.
        fn send(&mut self, codex: &mut Codex, line: &str) -> Vec<Event> {
            writeln!(self.to_core, "{line}").unwrap();
            codex.on_event(
                self.events
                    .recv_timeout(WAIT)
                    .expect("the line reached the core"),
            )
        }

        fn note(&mut self, codex: &mut Codex, method: &str, params: &str) -> Vec<Event> {
            let line =
                format!(r#"{{"method":"{method}","params":{params},"emittedAtMs":1761450000000}}"#);
            self.send(codex, &line)
        }

        fn delta(&mut self, codex: &mut Codex, method: &str, item: &str, text: &str) -> Vec<Event> {
            let params = on_turn(vec![
                ("itemId", s(item)),
                ("delta", s(text)),
                ("summaryIndex", Json::Int(1)),
            ]);
            self.note(codex, method, &params)
        }
    }

    /// Handshake, `thread/start` (thr_1), the first `turn/start` (turn_1) and
    /// `turn/started`: a busy session whose first prompt `c1` is running.
    fn session() -> (Codex, Peer) {
        let (mut codex, mut peer) = connect();
        peer.recv();
        peer.send(&mut codex, INIT_OK);
        peer.recv();
        codex.start_session(r"C:\work", p("hello"), "c1").unwrap();
        peer.recv();
        peer.send(
            &mut codex,
            r#"{"id":2,"result":{"thread":{"id":"thr_1"},"model":"gpt-test"}}"#,
        );
        peer.recv();
        peer.send(
            &mut codex,
            r#"{"id":3,"result":{"turn":{"id":"turn_1","status":"inProgress"}}}"#,
        );
        let started = r#"{"threadId":"thr_1","turn":{"id":"turn_1","status":"inProgress","startedAt":1761450000}}"#;
        peer.note(&mut codex, "turn/started", started);
        (codex, peer)
    }

    fn s(v: &str) -> Json {
        json::s(v)
    }

    /// Params on thr_1 / turn_1 plus `extra`.
    fn on_turn(extra: Vec<(&str, Json)>) -> String {
        let mut fields = vec![("threadId", s("thr_1")), ("turnId", s("turn_1"))];
        fields.extend(extra);
        json::stringify(&json::obj(fields))
    }

    fn turn_completed(status: &str) -> String {
        format!(
            r#"{{"threadId":"thr_1","turn":{{"id":"turn_1","status":"{status}","error":null}}}}"#
        )
    }

    /// A command approval request on item `call_<id>`.
    fn command_ask(id: i64, amend: bool) -> String {
        let amendment = if amend {
            r#","proposedExecpolicyAmendment":["npm","test"]"#
        } else {
            ""
        };
        format!(
            r#"{{"id":{id},"method":"item/commandExecution/requestApproval","params":{{"kind":"command","threadId":"thr_1","turnId":"turn_1","itemId":"call_{id}","startedAtMs":1,"environmentId":null,"command":"npm test","cwd":"C:\\work","reason":"Run tests"{amendment}}}}}"#
        )
    }

    fn command_item(id: &str, status: &str, output: Json, exit: Json) -> String {
        let item = json::obj(vec![
            ("type", s("commandExecution")),
            ("id", s(id)),
            ("command", s("npm test")),
            ("cwd", s(r"C:\work")),
            ("processId", Json::Null),
            ("source", s("agent")),
            ("status", s(status)),
            ("commandActions", Json::Arr(Vec::new())),
            ("aggregatedOutput", output),
            ("exitCode", exit),
            ("durationMs", Json::Null),
        ]);
        on_turn(vec![("item", item)])
    }

    /// The ops of `events`, all on thr_1.
    fn ops(events: &[Event]) -> Vec<&Op> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Op(thread, op) => {
                    assert_eq!(thread, "thr_1");
                    Some(op)
                }
                _ => None,
            })
            .collect()
    }

    /// The items of `events`, on any thread.
    fn any_items(events: &[Event]) -> Vec<&Item> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Op(_, Op::Item(i)) => Some(i),
                _ => None,
            })
            .collect()
    }

    fn items(events: &[Event]) -> Vec<&Item> {
        ops(events)
            .into_iter()
            .filter_map(|op| match op {
                Op::Item(i) => Some(i),
                _ => None,
            })
            .collect()
    }

    fn tools(events: &[Event]) -> Vec<&ToolCall> {
        items(events)
            .into_iter()
            .filter_map(|i| i.tool.as_ref())
            .collect()
    }

    fn appended(events: &[Event], field: Field) -> Vec<String> {
        ops(events)
            .into_iter()
            .filter_map(|op| match op {
                Op::Append { field: f, text, .. } if *f == field => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn turn_op(events: &[Event]) -> &TurnState {
        ops(events)
            .into_iter()
            .find_map(|op| match op {
                Op::Turn(t) => Some(t),
                _ => None,
            })
            .expect("a turn op")
    }

    fn permission(events: &[Event]) -> &PermissionRequest {
        events
            .iter()
            .find_map(|e| match e {
                Event::Permission(p) => Some(&**p),
                _ => None,
            })
            .expect("a permission request")
    }

    fn dones(events: &[Event]) -> Vec<(&str, &str, Option<&str>)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Done { tag, status, error } => {
                    Some((tag.as_str(), *status, error.as_deref()))
                }
                _ => None,
            })
            .collect()
    }

    fn mcp(name: &str, status: &'static str, flows: Vec<&'static str>) -> McpServer {
        McpServer {
            name: name.into(),
            status,
            error: None,
            auth_flows: flows,
            auth: None,
        }
    }

    fn server(name: &str, runtime: Json, http: bool, auth: &str) -> Json {
        let origin = if http {
            s("https://mcp.example.test")
        } else {
            Json::Null
        };
        json::obj(vec![
            ("name", s(name)),
            ("runtimeStatus", runtime),
            ("pluginId", Json::Null),
            ("httpOrigin", origin),
            ("serverInfo", Json::Null),
            ("serverCapabilities", Json::Null),
            ("toolListMeta", Json::Null),
            ("tools", json::obj(vec![])),
            ("toolsError", Json::Null),
            ("resources", Json::Arr(Vec::new())),
            ("resourceTemplates", Json::Arr(Vec::new())),
            ("authStatus", s(auth)),
        ])
    }

    fn list_reply(id: i64, servers: Vec<Json>) -> String {
        let result = json::obj(vec![
            ("data", Json::Arr(servers)),
            ("nextCursor", Json::Null),
        ]);
        json::stringify(&json::obj(vec![("id", Json::Int(id)), ("result", result)]))
    }

    const STARTUP: &str = r#"{"threadId":null,"name":"fake-docs","status":"ready","error":null,"failureReason":null}"#;

    #[test]
    fn handshake_initialize_initialized_thread_start() {
        let (mut codex, mut peer) = connect();
        assert_eq!(
            peer.recv(),
            r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"chernobyl","title":"CHERNOBYL","version":"9.9.9"},"capabilities":{"experimentalApi":false}}}"#
        );
        // a session asked for during the handshake waits for `initialized`
        codex.start_session(r"C:\work", p("hello"), "c1").unwrap();
        peer.quiet();
        assert!(peer.send(&mut codex, INIT_OK).is_empty());
        assert_eq!(peer.recv(), r#"{"method":"initialized"}"#);
        assert_eq!(
            peer.recv(),
            r#"{"id":2,"method":"thread/start","params":{"cwd":"C:\\work","approvalPolicy":"untrusted","sandbox":"danger-full-access"}}"#
        );
        // the reply's `sandbox` is a policy object, not the mode string we sent
        let events = peer.send(
            &mut codex,
            r#"{"id":2,"result":{"thread":{"id":"thr_1","status":{"type":"idle"}},"model":"gpt-test","approvalPolicy":"on-request","sandbox":{"type":"workspaceWrite","writableRoots":[],"networkAccess":false}}}"#,
        );
        assert_eq!(
            peer.recv(),
            r#"{"id":3,"method":"turn/start","params":{"threadId":"thr_1","input":[{"type":"text","text":"hello"}]}}"#
        );
        let idle = LiveState {
            status: "idle",
            permission_mode: None,
            model: Some("gpt-test".into()),
            ..Default::default()
        };
        assert_eq!(
            events[..3],
            [
                Event::SessionStarted {
                    tag: "c1".into(),
                    thread: "thr_1".into()
                },
                Event::Op("thr_1".into(), Op::State(idle)),
                Event::Op("thr_1".into(), Op::Mcp(Vec::new())),
            ]
        );
        let user = items(&events);
        assert_eq!(user.len(), 1);
        assert_eq!(
            (user[0].kind, user[0].text.as_str()),
            (ItemKind::User, "hello")
        );
        assert!(codex.owns_session("thr_1"));
        let events = peer.send(&mut codex, r#"{"id":3,"result":{"turn":{"id":"turn_1","status":"inProgress","items":[],"error":null}}}"#);
        assert_eq!(
            events,
            [Event::Progress {
                tag: "c1".into(),
                status: "running"
            }]
        );
        // codex echoes the input as a userMessage item: the bubble exists already
        let echo = json::parse(r#"{"type":"userMessage","id":"user_9","clientId":null,"content":[{"type":"text","text":"hello","text_elements":[]}]}"#).unwrap();
        for method in ["item/started", "item/completed"] {
            assert!(
                peer.note(&mut codex, method, &on_turn(vec![("item", echo.clone())]))
                    .is_empty()
            );
        }
    }

    #[test]
    fn agent_message_deltas_become_text_appends() {
        let (mut codex, mut peer) = session();
        let message = |text: &str| {
            let item = json::obj(vec![
                ("type", s("agentMessage")),
                ("id", s("msg_1")),
                ("text", s(text)),
                ("phase", Json::Null),
            ]);
            on_turn(vec![("item", item)])
        };
        let events = peer.note(&mut codex, "item/started", &message(""));
        let it = items(&events)[0];
        assert_eq!(
            (
                it.id.as_str(),
                it.kind,
                it.text.as_str(),
                it.done,
                it.turn_id.as_deref()
            ),
            ("msg_1", ItemKind::Text, "", false, Some("turn_1"))
        );
        for delta in ["Hello ", "from ", "Codex."] {
            let events = peer.delta(&mut codex, "item/agentMessage/delta", "msg_1", delta);
            let append = Op::Append {
                id: "msg_1".into(),
                field: Field::Text,
                text: delta.into(),
            };
            assert_eq!(events, [Event::Op("thr_1".into(), append)]);
        }
        let events = peer.note(&mut codex, "item/completed", &message("Hello from Codex."));
        let it = items(&events)[0];
        assert_eq!((it.text.as_str(), it.done), ("Hello from Codex.", true));
        // the turn ends: its footer, then the first prompt's result
        let events = peer.note(&mut codex, "turn/completed", &turn_completed("completed"));
        assert_eq!(turn_op(&events).status, "completed");
        assert_eq!(dones(&events), [("c1", "completed", None)]);
    }

    #[test]
    fn command_output_deltas_are_line_buffered_and_redacted() {
        let (mut codex, mut peer) = session();
        let key = format!("sk-ant-api03-{}", "FAKE".repeat(6));
        let started = command_item("call_1", "inProgress", Json::Null, Json::Null);
        let events = peer.note(&mut codex, "item/started", &started);
        let card = tools(&events)[0];
        assert_eq!(
            (
                card.name.as_str(),
                card.title.as_deref(),
                card.status,
                card.input.as_str()
            ),
            ("shell", Some("npm test"), ToolStatus::Running, "npm test")
        );
        let secret_line = format!("loaded env: token {key}");
        let mut forwarded = Vec::new();
        for chunk in [
            "> npm test\n",
            "PASS a\nPASS b",
            "\n",
            secret_line.as_str(),
            " (from .env)\n",
            "no newline yet",
        ] {
            let events = peer.delta(
                &mut codex,
                "item/commandExecution/outputDelta",
                "call_1",
                chunk,
            );
            forwarded.extend(appended(&events, Field::Output));
        }
        assert_eq!(
            forwarded,
            [
                "> npm test\n",
                "PASS a\n",
                "PASS b\n",
                "loaded env: token «redacted:anthropic-key» (from .env)\n"
            ]
        );
        // completion: all of aggregatedOutput, redacted, partial line included
        let all = format!("> npm test\nPASS a\nPASS b\n{secret_line} (from .env)\nno newline yet");
        let done = command_item("call_1", "completed", s(&all), Json::Int(0));
        let events = peer.note(&mut codex, "item/completed", &done);
        let card = tools(&events)[0];
        assert_eq!(card.output, all.replace(&key, "«redacted:anthropic-key»"));
        assert_eq!(
            (card.status, card.output_truncated, card.error.as_deref()),
            (ToolStatus::Completed, false, None)
        );

        // forwarding stops at 32 KiB; completion keeps head and tail
        peer.note(
            &mut codex,
            "item/started",
            &command_item("call_2", "inProgress", Json::Null, Json::Null),
        );
        let line = format!("{}\n", "x".repeat(1023));
        let mut sent = 0;
        for _ in 0..40 {
            let events = peer.delta(
                &mut codex,
                "item/commandExecution/outputDelta",
                "call_2",
                &line,
            );
            sent += appended(&events, Field::Output)
                .iter()
                .map(String::len)
                .sum::<usize>();
        }
        assert_eq!(sent, OUTPUT_CAP);
        let failed = command_item("call_2", "failed", s(&line.repeat(40)), Json::Int(1));
        let events = peer.note(&mut codex, "item/completed", &failed);
        let card = tools(&events)[0];
        assert!(card.output_truncated && card.output.len() <= OUTPUT_CAP);
        assert!(
            card.output.starts_with("xxx")
                && card.output.contains("\n…\n")
                && card.output.ends_with("x\n")
        );
        assert_eq!(
            (card.status, card.error.as_deref()),
            (ToolStatus::Failed, Some("exit code 1"))
        );
    }

    #[test]
    fn file_change_item_carries_unified_diff() {
        let (mut codex, mut peer) = session();
        let change = |path: &str, kind: Json, diff: &str| {
            json::obj(vec![("path", s(path)), ("kind", kind), ("diff", s(diff))])
        };
        let changes = Json::Arr(vec![
            change(
                r"C:\work\src\greeting.txt",
                json::obj(vec![("type", s("update")), ("move_path", Json::Null)]),
                "@@ -1,3 +1,3 @@\n line one\n-hello world\n+hello codex\n line three\n",
            ),
            // add and delete carry the whole file, not a diff
            change(
                r"C:\work\src\notes.md",
                json::obj(vec![("type", s("add"))]),
                "# Notes\n\nCreated by the fake app-server.\n",
            ),
            change(
                r"C:\work\old.txt",
                json::obj(vec![("type", s("delete"))]),
                "bye\n",
            ),
            change(
                r"C:\work\a.txt",
                json::obj(vec![
                    ("type", s("update")),
                    ("move_path", s(r"C:\work\b.txt")),
                ]),
                "@@ -1 +1 @@\n-a\n+b\n\n\nMoved to: C:\\work\\b.txt",
            ),
        ]);
        let item = |status: &str| {
            let item = json::obj(vec![
                ("type", s("fileChange")),
                ("id", s("call_7")),
                ("changes", changes.clone()),
                ("status", s(status)),
            ]);
            on_turn(vec![("item", item)])
        };
        let events = peer.note(&mut codex, "item/started", &item("inProgress"));
        let card = tools(&events)[0];
        assert_eq!(
            (card.name.as_str(), card.title.as_deref(), card.status),
            (
                "apply_patch",
                Some("src/greeting.txt (+3 more)"),
                ToolStatus::Running
            )
        );
        let shape: Vec<_> = card
            .diffs
            .iter()
            .map(|d| {
                (
                    d.path.as_str(),
                    d.status,
                    d.additions,
                    d.deletions,
                    d.patch.as_str(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (
                    "src/greeting.txt",
                    "modified",
                    1,
                    1,
                    "@@ -1,3 +1,3 @@\n line one\n-hello world\n+hello codex\n line three\n"
                ),
                (
                    "src/notes.md",
                    "added",
                    3,
                    0,
                    "@@ -0,0 +1,3 @@\n+# Notes\n+\n+Created by the fake app-server.\n"
                ),
                ("old.txt", "deleted", 0, 1, "@@ -1,1 +0,0 @@\n-bye\n"),
                ("b.txt", "renamed", 1, 1, "@@ -1 +1 @@\n-a\n+b\n"),
            ]
        );
        // codex starts the item, then asks: the approval points at the card and carries its diff
        let ask = r#"{"id":0,"method":"item/fileChange/requestApproval","params":{"threadId":"thr_1","turnId":"turn_1","itemId":"call_7","startedAtMs":1,"reason":"Edit the greeting"}}"#;
        let events = peer.send(&mut codex, ask);
        let pr = permission(&events);
        assert_eq!(
            (
                pr.action.as_str(),
                pr.risk.as_str(),
                pr.tool_item_id.as_deref(),
                pr.context.as_str()
            ),
            ("file-write", "medium", Some("call_7"), "Edit the greeting")
        );
        let ids: Vec<&str> = pr.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["accept", "acceptForSession", "decline", "cancel"]);
        let detail = pr.detail.as_ref().unwrap();
        assert_eq!(json::get_str(detail, "kind"), Some("edit"));
        assert!(
            json::get_str(detail, "patch")
                .unwrap()
                .contains("+hello codex")
        );
        let card = tools(&events)[0];
        assert_eq!(
            (card.status, card.permission_id.as_deref(), card.diffs.len()),
            (ToolStatus::AwaitingApproval, Some("codex-1:0"), 4)
        );
        codex
            .decide("codex-1:0", "approve-once", Some("accept"), &[])
            .unwrap();
        assert_eq!(peer.recv(), r#"{"id":0,"result":{"decision":"accept"}}"#);
        let events = peer.note(&mut codex, "item/completed", &item("completed"));
        let card = tools(&events)[0];
        assert_eq!(
            (card.status, card.permission_id.as_deref()),
            (ToolStatus::Completed, Some("codex-1:0"))
        );
    }

    #[test]
    fn turn_diff_splits_per_file() {
        let (mut codex, mut peer) = session();
        let diff = "diff --git a/src/greeting.txt b/src/greeting.txt\nindex 3b18e51..a1f6c2d 100644\n\
                    --- a/src/greeting.txt\n+++ b/src/greeting.txt\n@@ -1,3 +1,3 @@\n line one\n\
                    -hello world\n+hello codex\n line three\n\
                    diff --git a/src/notes.md b/src/notes.md\nnew file mode 100644\nindex 0000000..5c4b2f1\n\
                    --- /dev/null\n+++ b/src/notes.md\n@@ -0,0 +1,3 @@\n+# Notes\n+\n+Created here.\n\
                    diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n\
                    +++ /dev/null\n@@ -1 +0,0 @@\n-bye\n";
        let events = peer.note(
            &mut codex,
            "turn/diff/updated",
            &on_turn(vec![("diff", s(diff))]),
        );
        let turn = turn_op(&events);
        assert_eq!((turn.id.as_str(), turn.status), ("turn_1", "running"));
        let files: Vec<_> = turn
            .diffs
            .iter()
            .map(|d| (d.path.as_str(), d.status, d.additions, d.deletions))
            .collect();
        assert_eq!(
            files,
            [
                ("src/greeting.txt", "modified", 1, 1),
                ("src/notes.md", "added", 3, 0),
                ("gone.txt", "deleted", 0, 1)
            ]
        );
        assert_eq!(
            turn.diffs[0].patch,
            "@@ -1,3 +1,3 @@\n line one\n-hello world\n+hello codex\n line three\n"
        );
        // the turn footer keeps them
        let events = peer.note(&mut codex, "turn/completed", &turn_completed("completed"));
        assert_eq!(turn_op(&events).diffs.len(), 3);
    }

    /// Live shapes (codex-cli 0.167.0-alpha.4.1): `availableDecisions`, the
    /// PowerShell wrapper, absolute fileChange paths.
    #[test]
    fn live_shapes_offer_only_listed_decisions_and_readable_titles() {
        let (mut codex, mut peer) = session();
        let ask = r#"{"id":1,"method":"item/commandExecution/requestApproval","params":{"kind":"command","threadId":"thr_1","turnId":"turn_1","itemId":"exec-1","startedAtMs":1,"environmentId":"local","command":"\"C:\\\\WINDOWS\\\\System32\\\\WindowsPowerShell\\\\v1.0\\\\powershell.exe\" -Command 'git add probe.txt'","cwd":"C:\\work","commandActions":[{"type":"unknown","command":"git add probe.txt"}],"proposedExecpolicyAmendment":["git","add","probe.txt"],"availableDecisions":["accept",{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["git","add","probe.txt"]}},"cancel"]}}"#;
        let events = peer.send(&mut codex, ask);
        let pr = permission(&events).clone();
        let ids: Vec<&str> = pr.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["accept", "acceptWithExecpolicyAmendment", "cancel"]);
        assert_eq!(pr.supported_decisions, ["approve-once", "reject"]);
        assert_eq!(pr.command, "git add probe.txt");
        let detail = json::stringify(pr.detail.as_ref().unwrap());
        assert!(
            detail.contains("powershell.exe"),
            "the detail keeps the real command: {detail}"
        );
        assert_eq!(
            tools(&events)[0].title.as_deref(),
            Some("git add probe.txt")
        );
        // a legacy reject is the one listed: cancel
        assert!(codex.decide(&pr.id, "reject", None, &[]).is_ok());
        assert_eq!(peer.recv(), r#"{"id":1,"result":{"decision":"cancel"}}"#);
        // fileChange paths come absolute; the card shows them relative
        let item = r#"{"threadId":"thr_1","turnId":"turn_1","item":{"type":"fileChange","id":"exec-2","changes":[{"path":"c:\\WORK\\sub\\probe.txt","kind":{"type":"add"},"diff":"ok\n"}],"status":"inProgress"}}"#;
        let events = peer.note(&mut codex, "item/started", item);
        assert_eq!(tools(&events)[0].diffs[0].path, "sub/probe.txt");
        assert_eq!(
            relative(r"D:\elsewhere\x.txt", r"C:\work"),
            r"D:\elsewhere\x.txt"
        );
        assert_eq!(
            relative(r"C:\workshop\x.txt", r"C:\work"),
            r"C:\workshop\x.txt"
        );
    }

    #[test]
    fn models_context_rename_and_history() {
        let (mut codex, mut peer) = session();
        // no model/list yet: nothing to pick
        assert_eq!(
            codex.set_model("thr_1", "gpt-5.6-luna"),
            Err("Codex does not offer this model")
        );
        let models = r#"{"id":1099511627776,"result":{"data":[{"id":"gpt-6-luna","model":"gpt-6-luna","displayName":"GPT-6-Luna","hidden":false},{"id":"x","model":"secret-x","displayName":"X","hidden":true},{"id":"gpt-5.6-luna","model":"gpt-5.6-luna","displayName":"GPT-5.6-Luna","hidden":false}],"nextCursor":null}}"#;
        let events = peer.send(&mut codex, models);
        let state = |events: &[Event]| -> LiveState {
            ops(events)
                .into_iter()
                .find_map(|op| match op {
                    Op::State(s) => Some(s.clone()),
                    _ => None,
                })
                .expect("a state op")
        };
        assert_eq!(
            state(&events).models,
            [
                ("gpt-6-luna".to_string(), "GPT-6-Luna".to_string()),
                ("gpt-5.6-luna".to_string(), "GPT-5.6-Luna".to_string())
            ]
        );
        let events = codex.set_model("thr_1", "gpt-5.6-luna").unwrap();
        assert_eq!(state(&events).model.as_deref(), Some("gpt-5.6-luna"));
        assert!(codex.set_model("thr_1", "secret-x").is_err());
        // context: the last request's tokens against the window
        let usage = r#"{"threadId":"thr_1","turnId":"turn_1","tokenUsage":{"total":{"totalTokens":43732,"inputTokens":43588,"cachedInputTokens":24576,"cacheWriteInputTokens":0,"outputTokens":144,"reasoningOutputTokens":0},"last":{"totalTokens":22259,"inputTokens":22228,"cachedInputTokens":22016,"cacheWriteInputTokens":0,"outputTokens":31,"reasoningOutputTokens":0},"modelContextWindow":258400}}"#;
        let events = peer.note(&mut codex, "thread/tokenUsage/updated", usage);
        assert_eq!(state(&events).context, Some((22259, 258400)));
        peer.note(&mut codex, "turn/completed", &turn_completed("completed"));
        // the next turn carries the chosen model
        codex.prompt("thr_1", p("next"), "c2").unwrap();
        let frame = peer.recv();
        assert!(frame.contains(r#""model":"gpt-5.6-luna""#), "{frame}");
        // rename: thread/name/set, then the title
        codex.rename("thr_1", "Probe", "r1").unwrap();
        let frame = peer.recv();
        assert!(
            frame.contains(
                r#""method":"thread/name/set","params":{"threadId":"thr_1","name":"Probe"}"#
            ),
            "{frame}"
        );
        let id = json::get_i64(&json::parse(&frame).unwrap(), "id").unwrap();
        let events = peer.send(&mut codex, &format!(r#"{{"id":{id},"result":{{}}}}"#));
        assert_eq!(
            events,
            [
                Event::Renamed {
                    thread: "thr_1".into(),
                    title: "Probe".into()
                },
                done("r1".into(), "completed", None)
            ]
        );
        // history of a thread codex has not loaded: items newest first, replayed oldest first
        codex.history("thr_old", r"C:\work").unwrap();
        let frame = peer.recv();
        assert!(frame.contains(r#""method":"thread/items/list","params":{"threadId":"thr_old","limit":300,"sortDirection":"desc"}"#), "{frame}");
        let id = json::get_i64(&json::parse(&frame).unwrap(), "id").unwrap();
        let data = r#"[{"turnId":"t2","item":{"type":"commandExecution","id":"e1","command":"x","commandActions":[{"type":"unknown","command":"git add a"}],"status":"inProgress","aggregatedOutput":null,"exitCode":null,"startedAtMs":5}},{"turnId":"t1","item":{"type":"reasoning","id":"r1","summary":[],"content":[]}},{"turnId":"t1","item":{"type":"agentMessage","id":"m1","text":"pong","phase":"final_answer"}},{"turnId":"t1","item":{"type":"userMessage","id":"u1","clientId":null,"content":[{"type":"image","url":"data:image/png;base64,iVBORw0KGgo="},{"type":"text","text":"Reply pong","text_elements":[]}]}}]"#;
        let events = peer.send(
            &mut codex,
            &format!(r#"{{"id":{id},"result":{{"data":{data},"nextCursor":null}}}}"#),
        );
        assert!(
            events
                .iter()
                .all(|e| matches!(e, Event::Op(t, _) if t == "thr_old"))
        );
        let replay = any_items(&events);
        let shape: Vec<_> = replay
            .iter()
            .map(|i| (i.id.as_str(), i.kind, i.text.as_str(), i.images))
            .collect();
        assert_eq!(
            shape,
            [
                ("u1", ItemKind::User, "Reply pong", 1),
                ("m1", ItemKind::Text, "pong", 0),
                ("e1", ItemKind::Tool, "", 0)
            ]
        );
        assert_eq!(
            replay[2].tool.as_ref().unwrap().status,
            ToolStatus::Cancelled
        );
        assert!(matches!(events.last(), Some(Event::Op(_, Op::State(s))) if s.status == "idle"));
    }

    #[test]
    fn thread_list_and_fork() {
        let (mut codex, mut peer) = session();
        codex.list_threads(&[r"C:\work".to_string()]).unwrap();
        let frame = peer.recv();
        assert!(frame.contains(r#""method":"thread/list","params":{"limit":50,"sortKey":"updated_at","cwd":["C:\\work"]}"#), "{frame}");
        let id = json::get_i64(&json::parse(&frame).unwrap(), "id").unwrap();
        let data = r#"[{"id":"a","name":null,"preview":"\nFix the login bug\nmore","cwd":"C:\\work","ephemeral":false,"parentThreadId":null,"createdAt":10,"updatedAt":20},{"id":"b","name":"Named","preview":"x","cwd":"C:\\work","ephemeral":false,"parentThreadId":null,"createdAt":1,"updatedAt":2},{"id":"sub","name":null,"preview":"x","cwd":"C:\\work","ephemeral":false,"parentThreadId":"a","createdAt":1,"updatedAt":2},{"id":"eph","name":null,"preview":"x","cwd":"C:\\work","ephemeral":true,"parentThreadId":null,"createdAt":1,"updatedAt":2}]"#;
        let events = peer.send(
            &mut codex,
            &format!(r#"{{"id":{id},"result":{{"data":{data},"nextCursor":null}}}}"#),
        );
        let Some(Event::Threads(list)) = events.first() else {
            panic!("{events:?}");
        };
        let shape: Vec<_> = list
            .iter()
            .map(|t| (t.id.as_str(), t.title.as_str(), t.updated_at))
            .collect();
        assert_eq!(
            shape,
            [("a", "Fix the login bug", 20_000), ("b", "Named", 2_000)]
        );
        // fork: a new thread, its history, a notice, then the prompt
        codex.fork("a", r"C:\work", p("go on"), "f1").unwrap();
        let frame = peer.recv();
        assert!(frame.contains(r#""method":"thread/fork","params":{"threadId":"a","cwd":"C:\\work","approvalPolicy":"untrusted","sandbox":"danger-full-access","excludeTurns":true}"#), "{frame}");
        let id = json::get_i64(&json::parse(&frame).unwrap(), "id").unwrap();
        let events = peer.send(&mut codex, &format!(r#"{{"id":{id},"result":{{"thread":{{"id":"thr_f","forkedFromId":"a"}},"model":"gpt-6-luna"}}}}"#));
        assert_eq!(
            events,
            [Event::SessionStarted {
                tag: "f1".into(),
                thread: "thr_f".into()
            }]
        );
        let frame = peer.recv();
        assert!(
            frame.contains(r#""threadId":"thr_f""#) && frame.contains("thread/items/list"),
            "{frame}"
        );
        let id = json::get_i64(&json::parse(&frame).unwrap(), "id").unwrap();
        let data =
            r#"[{"turnId":"t1","item":{"type":"agentMessage","id":"m1","text":"old answer"}}]"#;
        let events = peer.send(
            &mut codex,
            &format!(r#"{{"id":{id},"result":{{"data":{data},"nextCursor":null}}}}"#),
        );
        let texts: Vec<&str> = any_items(&events).iter().map(|i| i.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "old answer",
                "Forked from a Codex thread; the original thread is unchanged",
                "go on"
            ]
        );
        assert!(
            peer.recv()
                .contains(r#""method":"turn/start","params":{"threadId":"thr_f""#)
        );
        assert!(codex.owns_session("thr_f"));
    }

    #[test]
    fn approval_options_map_the_decision_enum() {
        let (mut codex, mut peer) = session();
        let events = peer.send(&mut codex, &command_ask(0, true));
        let pr = permission(&events);
        assert_eq!(
            (
                pr.id.as_str(),
                pr.native_request_id.as_str(),
                pr.tool_item_id.as_deref(),
                pr.session_id.as_str()
            ),
            ("codex-1:0", "0", Some("call_0"), "thr_1")
        );
        assert_eq!(
            (
                pr.action.as_str(),
                pr.risk.as_str(),
                pr.command.as_str(),
                pr.context.as_str()
            ),
            ("shell", "high", "npm test", "Run tests")
        );
        assert_eq!(pr.directory.as_deref(), Some(r"C:\work"));
        assert_eq!(
            pr.supported_decisions,
            ["approve-once", "approve-session", "reject"]
        );
        let shape: Vec<_> = pr
            .options
            .iter()
            .map(|o| {
                (
                    o.id.as_str(),
                    o.decision.as_str(),
                    o.label.as_str(),
                    o.scope,
                    o.rule.as_deref(),
                    o.stops_turn,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("accept", "approve-once", "Allow", "once", None, false),
                (
                    "acceptForSession",
                    "approve-session",
                    "Allow for this session",
                    "session",
                    None,
                    false
                ),
                (
                    "acceptWithExecpolicyAmendment",
                    "approve-session",
                    "Always allow commands starting with `npm test`",
                    "persistent",
                    Some("npm test"),
                    false
                ),
                ("decline", "reject", "Deny", "once", None, false),
                ("cancel", "reject", "Deny and stop", "once", None, true),
            ]
        );
        let detail =
            r#"{"kind":"shell","command":"npm test","cwd":"C:\\work","reason":"Run tests"}"#;
        assert_eq!(pr.detail, Some(json::parse(detail).unwrap()));
        let card = tools(&events)[0];
        assert_eq!(
            (card.status, card.permission_id.as_deref()),
            (ToolStatus::AwaitingApproval, Some("codex-1:0"))
        );
        // the amendment goes back exactly as proposed, once
        codex
            .decide(
                "codex-1:0",
                "approve-session",
                Some("acceptWithExecpolicyAmendment"),
                &[],
            )
            .unwrap();
        assert_eq!(
            peer.recv(),
            r#"{"id":0,"result":{"decision":{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["npm","test"]}}}}"#
        );
        assert_eq!(
            codex.decide("codex-1:0", "approve-once", None, &[]),
            Err("request is no longer pending")
        );
        // every other choice, by option id and by legacy decision
        for (id, decision, option, wire) in [
            (1, "approve-once", Some("accept"), "\"accept\""),
            (
                2,
                "approve-session",
                Some("acceptForSession"),
                "\"acceptForSession\"",
            ),
            (3, "reject", Some("decline"), "\"decline\""),
            (4, "reject", Some("cancel"), "\"cancel\""),
            (5, "approve-once", None, "\"accept\""),
            (6, "approve-session", None, "\"acceptForSession\""),
            (7, "reject", None, "\"decline\""),
        ] {
            let events = peer.send(&mut codex, &command_ask(id, false));
            let pr = permission(&events);
            assert_eq!(pr.options.len(), 4, "no amendment proposed, none offered");
            codex.decide(&pr.id, decision, option, &[]).unwrap();
            assert_eq!(
                peer.recv(),
                format!(r#"{{"id":{id},"result":{{"decision":{wire}}}}}"#)
            );
        }
        // a bucket mismatch or an option that was not offered never reaches codex
        peer.send(&mut codex, &command_ask(8, false));
        assert_eq!(
            codex.decide("codex-1:8", "approve-once", Some("decline"), &[]),
            Err("option does not match the decision")
        );
        assert_eq!(
            codex.decide(
                "codex-1:8",
                "approve-session",
                Some("acceptWithExecpolicyAmendment"),
                &[]
            ),
            Err("unknown option")
        );
        peer.quiet();
        assert!(codex.knows_permission("codex-1:8"));
    }

    #[test]
    fn user_input_question_round_trip() {
        let (mut codex, mut peer) = session();
        let ask = |id: i64| {
            format!(
                r#"{{"id":{id},"method":"item/tool/requestUserInput","params":{{"threadId":"thr_1","turnId":"turn_1","itemId":"call_q{id}","questions":[{{"id":"confirm_path","header":"Confirm","question":"Proceed with the plan?","isOther":false,"isSecret":false,"options":[{{"label":"Yes (Recommended)","description":"Continue."}},{{"label":"No","description":"Stop."}}]}},{{"id":"favorite_color","header":"Color","question":"Which color?","isOther":true,"isSecret":false,"options":null}}],"isBlocking":true,"autoResolutionMs":null}}}}"#
            )
        };
        let events = peer.send(&mut codex, &ask(3));
        let pr = permission(&events);
        assert_eq!(pr.supported_decisions, ["approve-once", "reject"]);
        let ids: Vec<&str> = pr.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["answer", "skip"]);
        let detail = r#"{"kind":"question","questions":[{"id":"confirm_path","text":"Proceed with the plan?","header":"Confirm","options":[{"label":"Yes (Recommended)","description":"Continue."},{"label":"No","description":"Stop."}],"multiSelect":false,"allowFreeText":false},{"id":"favorite_color","text":"Which color?","header":"Color","options":[],"multiSelect":false,"allowFreeText":true}]}"#;
        assert_eq!(pr.detail, Some(json::parse(detail).unwrap()));
        // no codex item exists for a question: the card is made for it
        let card = tools(&events)[0];
        assert_eq!(
            (card.name.as_str(), card.status, card.title.as_deref()),
            (
                "request_user_input",
                ToolStatus::AwaitingApproval,
                Some("Proceed with the plan?")
            )
        );
        let answers = vec![
            (
                "confirm_path".to_string(),
                vec!["Yes (Recommended)".to_string()],
            ),
            ("favorite_color".to_string(), vec!["teal".to_string()]),
            ("not_asked".to_string(), vec!["x".to_string()]),
        ];
        let events = codex
            .decide("codex-1:3", "approve-once", None, &answers)
            .unwrap();
        assert_eq!(
            peer.recv(),
            r#"{"id":3,"result":{"answers":{"confirm_path":{"answers":["Yes (Recommended)"]},"favorite_color":{"answers":["teal"]}}}}"#
        );
        assert_eq!(tools(&events)[0].status, ToolStatus::Completed);
        // skipping answers nothing
        peer.send(&mut codex, &ask(4));
        codex
            .decide("codex-1:4", "reject", Some("skip"), &answers)
            .unwrap();
        assert_eq!(peer.recv(), r#"{"id":4,"result":{"answers":{}}}"#);
    }

    #[test]
    fn server_request_resolved_withdraws() {
        let (mut codex, mut peer) = session();
        peer.note(
            &mut codex,
            "item/started",
            &command_item("call_5", "inProgress", Json::Null, Json::Null),
        );
        peer.send(&mut codex, &command_ask(5, false));
        assert!(codex.knows_permission("codex-1:5"));
        // another client answered it: withdraw, and never answer afterwards
        let events = peer.note(
            &mut codex,
            "serverRequest/resolved",
            r#"{"threadId":"thr_1","requestId":5}"#,
        );
        assert_eq!(events[0], Event::Withdrawn("codex-1:5".into()));
        assert_eq!(tools(&events)[0].status, ToolStatus::Running);
        assert_eq!(
            codex.decide("codex-1:5", "approve-once", None, &[]),
            Err("request is no longer pending")
        );
        peer.quiet();
        // codex resolves our own answers too: nothing left to withdraw
        peer.send(&mut codex, &command_ask(6, false));
        codex
            .decide("codex-1:6", "approve-once", None, &[])
            .unwrap();
        peer.recv();
        assert!(
            peer.note(
                &mut codex,
                "serverRequest/resolved",
                r#"{"threadId":"thr_1","requestId":6}"#
            )
            .is_empty()
        );
        // a request still open when its turn ends is withdrawn as well
        peer.send(&mut codex, &command_ask(7, false));
        let events = peer.note(&mut codex, "turn/completed", &turn_completed("interrupted"));
        assert!(events.contains(&Event::Withdrawn("codex-1:7".into())));
        assert!(!codex.knows_permission("codex-1:7"));
        assert_eq!(dones(&events), [("c1", "cancelled", None)]);
    }

    #[test]
    fn unknown_server_request_gets_method_not_found() {
        // codex answers unknown methods with -32600 "Invalid request" (its
        // message_processor), not -32601; the core answers codex the same way
        let (mut codex, mut peer) = session();
        let unknown =
            r#"{"id":9,"method":"fake/unknownServerRequest","params":{"threadId":"thr_1"}}"#;
        assert!(peer.send(&mut codex, unknown).is_empty());
        assert_eq!(
            peer.recv(),
            r#"{"id":9,"error":{"code":-32600,"message":"Invalid request: unsupported method `fake/unknownServerRequest`"}}"#
        );
        // a request json.rs cannot read (nesting past 32) is still answered once
        let deep = format!(
            r#"{{"id":10,"method":"item/tool/requestUserInput","params":{}{}}}"#,
            "[".repeat(40),
            "]".repeat(40)
        );
        assert!(peer.send(&mut codex, &deep).is_empty());
        assert_eq!(
            peer.recv(),
            r#"{"id":10,"error":{"code":-32600,"message":"Invalid request: CHERNOBYL could not read this request"}}"#
        );
        assert_eq!(codex.malformed(), 1);
        peer.quiet();
    }

    #[test]
    fn permissions_request_gets_empty_grant_and_notice() {
        let (mut codex, mut peer) = session();
        let ask = r#"{"id":4,"method":"item/permissions/requestApproval","params":{"threadId":"thr_1","turnId":"turn_1","itemId":"call_3","environmentId":null,"startedAtMs":1,"cwd":"C:\\work","reason":"Needs network","permissions":{"network":{"enabled":true},"fileSystem":{"read":null,"write":["C:\\work"]}}}}"#;
        let events = peer.send(&mut codex, ask);
        assert_eq!(
            peer.recv(),
            r#"{"id":4,"result":{"permissions":{},"scope":"turn"}}"#
        );
        let notice = items(&events)[0];
        assert_eq!(
            (notice.kind, notice.level),
            (ItemKind::Notice, Some("warning"))
        );
        assert!(
            notice
                .text
                .contains("grant extra sandbox permissions on the desktop"),
            "{}",
            notice.text
        );
        assert!(notice.text.contains("Needs network"));
        assert!(!codex.knows_permission("codex-1:4"));
        // an MCP elicitation is declined the same way
        let elicit = r#"{"id":5,"method":"mcpServer/elicitation/request","params":{"threadId":"thr_1","turnId":"turn_1","serverName":"fake-docs","mode":"form","_meta":null,"message":"Which workspace?","requestedSchema":{"type":"object","properties":{}}}}"#;
        let events = peer.send(&mut codex, elicit);
        assert_eq!(
            peer.recv(),
            r#"{"id":5,"result":{"action":"decline","content":null,"_meta":null}}"#
        );
        let notice = items(&events)[0];
        assert!(
            notice.text.contains("fake-docs") && notice.text.contains("Which workspace?"),
            "{}",
            notice.text
        );
    }

    #[test]
    fn steer_when_busy_start_when_idle() {
        let (mut codex, mut peer) = session();
        // busy: the follow-up steers turn_1
        let events = codex.prompt("thr_1", p("focus on tests"), "c2").unwrap();
        assert_eq!(items(&events)[0].text, "focus on tests");
        assert_eq!(
            peer.recv(),
            r#"{"id":4,"method":"turn/steer","params":{"threadId":"thr_1","input":[{"type":"text","text":"focus on tests"}],"expectedTurnId":"turn_1"}}"#
        );
        let running = |tag: &str| Event::Progress {
            tag: tag.into(),
            status: "running",
        };
        assert_eq!(
            peer.send(&mut codex, r#"{"id":4,"result":{"turnId":"turn_1"}}"#),
            [running("c2")]
        );
        // both prompts complete with the turn they joined
        let events = peer.note(&mut codex, "turn/completed", &turn_completed("completed"));
        assert_eq!(
            dones(&events),
            [("c1", "completed", None), ("c2", "completed", None)]
        );
        // idle: a new turn
        codex.prompt("thr_1", p("next"), "c3").unwrap();
        assert_eq!(
            peer.recv(),
            r#"{"id":5,"method":"turn/start","params":{"threadId":"thr_1","input":[{"type":"text","text":"next"}]}}"#
        );
        assert_eq!(
            peer.send(&mut codex, r#"{"id":5,"result":{"turn":{"id":"turn_2"}}}"#),
            [running("c3")]
        );
        // a steer that loses the race with the turn's end falls back to
        // turn/start, without a second bubble
        assert_eq!(
            items(&codex.prompt("thr_1", p("late"), "c4").unwrap()).len(),
            1
        );
        assert!(peer.recv().contains(r#""method":"turn/steer""#));
        let ended =
            r#"{"threadId":"thr_1","turn":{"id":"turn_2","status":"completed","error":null}}"#;
        assert_eq!(
            dones(&peer.note(&mut codex, "turn/completed", ended)),
            [("c3", "completed", None)]
        );
        let refused = r#"{"error":{"code":-32600,"message":"no active turn to steer"},"id":6}"#;
        assert!(peer.send(&mut codex, refused).is_empty());
        assert_eq!(
            peer.recv(),
            r#"{"id":7,"method":"turn/start","params":{"threadId":"thr_1","input":[{"type":"text","text":"late"}]}}"#
        );
        assert_eq!(
            peer.send(&mut codex, r#"{"id":7,"result":{"turn":{"id":"turn_3"}}}"#),
            [running("c4")]
        );
        // a steer accepted just before its turn ended completes at once
        codex.prompt("thr_1", p("also"), "c5").unwrap();
        peer.recv();
        let ended =
            r#"{"threadId":"thr_1","turn":{"id":"turn_3","status":"completed","error":null}}"#;
        assert_eq!(
            dones(&peer.note(&mut codex, "turn/completed", ended)),
            [("c4", "completed", None)]
        );
        let late = peer.send(&mut codex, r#"{"id":8,"result":{"turnId":"turn_3"}}"#);
        assert_eq!(dones(&late), [("c5", "completed", None)]);
        assert_eq!(codex.prompt("nope", p("x"), "c6"), Err("unknown session"));
    }

    #[test]
    fn interrupt_uses_the_active_turn_id() {
        let (mut codex, mut peer) = session();
        let message = json::obj(vec![
            ("type", s("agentMessage")),
            ("id", s("msg_1")),
            ("text", s("")),
        ]);
        peer.note(
            &mut codex,
            "item/started",
            &on_turn(vec![("item", message)]),
        );
        peer.delta(
            &mut codex,
            "item/agentMessage/delta",
            "msg_1",
            "partial answer",
        );
        assert!(codex.interrupt("thr_1", "i1").unwrap().is_empty());
        assert_eq!(
            peer.recv(),
            r#"{"id":4,"method":"turn/interrupt","params":{"threadId":"thr_1","turnId":"turn_1"}}"#
        );
        // codex replies just before turn/completed
        let accepted = Event::Progress {
            tag: "i1".into(),
            status: "accepted",
        };
        assert_eq!(peer.send(&mut codex, r#"{"id":4,"result":{}}"#), [accepted]);
        let events = peer.note(&mut codex, "turn/completed", &turn_completed("interrupted"));
        // the unfinished message is final as streamed (codex sends no item/completed)
        let it = items(&events)[0];
        assert_eq!(
            (it.id.as_str(), it.text.as_str(), it.done),
            ("msg_1", "partial answer", true)
        );
        assert_eq!(turn_op(&events).status, "interrupted");
        assert_eq!(
            dones(&events),
            [("c1", "cancelled", None), ("i1", "completed", None)]
        );
        // ended in time: its 30 s timer lapses quietly
        assert!(codex.tick(now_ms() + INTERRUPT_MS).is_empty());
        // idle: nothing to stop
        let events = codex.interrupt("thr_1", "i2").unwrap();
        assert_eq!(dones(&events), [("i2", "completed", None)]);
        peer.quiet();
    }

    #[test]
    fn mcp_status_and_startup_updates_map() {
        let (mut codex, mut peer) = session();
        // a startup update re-lists; another one meanwhile waits for that reply
        assert!(
            peer.note(&mut codex, "mcpServer/startupStatus/updated", STARTUP)
                .is_empty()
        );
        assert_eq!(
            peer.recv(),
            r#"{"id":4,"method":"mcpServerStatus/list","params":{}}"#
        );
        assert!(
            peer.note(&mut codex, "mcpServer/startupStatus/updated", STARTUP)
                .is_empty()
        );
        peer.quiet();
        let mut failed = server("crashy", s("failed"), false, "unsupported");
        if let Json::Obj(fields) = &mut failed {
            fields.retain(|(k, _)| k != "toolsError");
            fields.push(("toolsError".into(), s("tool discovery failed")));
        }
        let list = list_reply(
            4,
            vec![
                server("fake-docs", s("connected"), false, "unsupported"),
                server(
                    "fake-linear",
                    s("authenticationRequired"),
                    true,
                    "notLoggedIn",
                ),
                server("cold", s("notStarted"), false, "unsupported"),
                server("warming", s("starting"), true, "oAuth"),
                failed,
                server("stopped", s("cancelled"), false, "unsupported"),
                server("off", s("disabled"), false, "unsupported"),
                server("unknown", Json::Null, false, "unknown"),
                server("token", s("connected"), true, "bearerToken"),
            ],
        );
        let events = peer.send(&mut codex, &list);
        let mut crashy = mcp("crashy", "failed", vec![]);
        crashy.error = Some("tool discovery failed".into());
        let want = vec![
            mcp("fake-docs", "connected", vec![]),
            mcp("fake-linear", "needs-auth", vec!["desktop"]),
            mcp("cold", "pending", vec![]),
            mcp("warming", "pending", vec!["desktop"]),
            crashy,
            mcp("stopped", "failed", vec![]),
            mcp("off", "disabled", vec![]),
            mcp("unknown", "pending", vec![]),
            mcp("token", "connected", vec![]),
        ];
        assert_eq!(events, [Event::Op("thr_1".into(), Op::Mcp(want))]);
        // the coalesced re-list
        assert_eq!(
            peer.recv(),
            r#"{"id":5,"method":"mcpServerStatus/list","params":{}}"#
        );
        // a reply json.rs cannot read still frees the list for the next update
        let deep = format!(
            r#"{{"id":5,"result":{}{}}}"#,
            "[".repeat(40),
            "]".repeat(40)
        );
        assert!(peer.send(&mut codex, &deep).is_empty());
        peer.note(&mut codex, "mcpServer/startupStatus/updated", STARTUP);
        assert_eq!(
            peer.recv(),
            r#"{"id":6,"method":"mcpServerStatus/list","params":{}}"#
        );
    }

    #[test]
    fn oauth_login_completed_marks_connected() {
        let (mut codex, mut peer) = session();
        peer.note(&mut codex, "mcpServer/startupStatus/updated", STARTUP);
        peer.recv();
        let linear = |status: &str, auth: &str| server("fake-linear", s(status), true, auth);
        peer.send(
            &mut codex,
            &list_reply(4, vec![linear("authenticationRequired", "notLoggedIn")]),
        );
        codex.mcp_login("fake-linear", "m1").unwrap();
        assert_eq!(
            peer.recv(),
            r#"{"id":5,"method":"mcpServer/oauth/login","params":{"name":"fake-linear"}}"#
        );
        let url = "https://auth.fake-linear.example/oauth/authorize?response_type=code&state=abc";
        let events = peer.send(
            &mut codex,
            &format!(r#"{{"id":5,"result":{{"authorizationUrl":"{url}","loginId":"l1"}}}}"#),
        );
        assert_eq!(
            events[..2],
            [
                Event::OpenUrl {
                    tag: "m1".into(),
                    url: url.into()
                },
                Event::Progress {
                    tag: "m1".into(),
                    status: "running"
                },
            ]
        );
        let mut waiting = mcp("fake-linear", "needs-auth", vec!["desktop"]);
        waiting.auth = Some(McpAuthState {
            state: "awaiting-browser",
            url: None,
            callback_prefix: None,
            error: None,
        });
        assert_eq!(events[2], Event::Op("thr_1".into(), Op::Mcp(vec![waiting])));
        let completed = r#"{"name":"fake-linear","threadId":null,"loginId":"l1","success":true}"#;
        let events = peer.note(&mut codex, "mcpServer/oauthLogin/completed", completed);
        assert_eq!(dones(&events), [("m1", "completed", None)]);
        assert_eq!(
            peer.recv(),
            r#"{"id":6,"method":"mcpServerStatus/list","params":{}}"#
        );
        let events = peer.send(
            &mut codex,
            &list_reply(6, vec![linear("connected", "oAuth")]),
        );
        let connected = mcp("fake-linear", "connected", vec!["desktop"]);
        assert_eq!(
            events,
            [Event::Op("thr_1".into(), Op::Mcp(vec![connected]))]
        );
        // a refused sign-in fails its command
        codex.mcp_login("fake-linear", "m2").unwrap();
        peer.recv();
        peer.send(
            &mut codex,
            &format!(r#"{{"id":7,"result":{{"authorizationUrl":"{url}","loginId":"l2"}}}}"#),
        );
        let refused = r#"{"name":"fake-linear","threadId":null,"loginId":"l2","success":false,"error":"access_denied"}"#;
        let events = peer.note(&mut codex, "mcpServer/oauthLogin/completed", refused);
        assert_eq!(dones(&events), [("m2", "failed", Some("access_denied"))]);
        peer.recv();
        peer.send(
            &mut codex,
            &list_reply(8, vec![linear("authenticationRequired", "notLoggedIn")]),
        );
        // reconnect is codex's config reload, done when a later list settles
        codex.mcp_reload("fake-linear", "r1").unwrap();
        codex.mcp_reload("nope", "r2").unwrap();
        assert_eq!(
            peer.recv(),
            r#"{"id":9,"method":"config/mcpServer/reload"}"#
        );
        peer.recv();
        assert!(dones(&peer.send(&mut codex, r#"{"id":9,"result":{}}"#)).is_empty());
        assert_eq!(
            peer.recv(),
            r#"{"id":11,"method":"mcpServerStatus/list","params":{}}"#
        );
        // r2's reload is answered while that list runs: it waits for the next one
        peer.send(&mut codex, r#"{"id":10,"result":{}}"#);
        // still starting: nothing settles; the coalesced list comes in pages
        let first = list_reply(11, vec![linear("starting", "oAuth")]);
        assert!(dones(&peer.send(&mut codex, &first)).is_empty());
        assert_eq!(
            peer.recv(),
            r#"{"id":12,"method":"mcpServerStatus/list","params":{}}"#
        );
        let page = |id: i64, servers: Vec<Json>, next: Json| {
            let result = json::obj(vec![("data", Json::Arr(servers)), ("nextCursor", next)]);
            json::stringify(&json::obj(vec![("id", Json::Int(id)), ("result", result)]))
        };
        let events = peer.send(
            &mut codex,
            &page(
                12,
                vec![server("fake-docs", s("connected"), false, "unsupported")],
                s("p2"),
            ),
        );
        assert!(events.is_empty());
        assert_eq!(
            peer.recv(),
            r#"{"id":13,"method":"mcpServerStatus/list","params":{"cursor":"p2"}}"#
        );
        let events = peer.send(
            &mut codex,
            &page(13, vec![linear("connected", "oAuth")], Json::Null),
        );
        assert_eq!(
            dones(&events),
            [
                ("r1", "completed", None),
                ("r2", "failed", Some("Codex has no MCP server named nope"))
            ]
        );
        let names: Vec<String> = match events.last() {
            Some(Event::Op(_, Op::Mcp(list))) => list.iter().map(|m| m.name.clone()).collect(),
            other => panic!("{other:?}"),
        };
        assert_eq!(names, ["fake-docs", "fake-linear"]);
    }

    #[test]
    fn exit_withdraws_requests_and_fails_commands() {
        let (mut codex, mut peer) = session();
        peer.send(&mut codex, &command_ask(0, false));
        codex.interrupt("thr_1", "i1").unwrap();
        peer.recv();
        let events = codex.on_event(ChildEvent::Exit(Some(3)));
        let why = Some("Codex exited with code 3");
        assert!(events.contains(&Event::Withdrawn("codex-1:0".into())));
        let mut results = dones(&events);
        results.sort_unstable();
        assert_eq!(results, [("c1", "failed", why), ("i1", "unknown", why)]);
        assert_eq!(turn_op(&events).status, "failed");
        // resumable: the thread idles, it does not end
        let states: Vec<&str> = ops(&events)
            .into_iter()
            .filter_map(|op| match op {
                Op::State(s) => Some(s.status),
                _ => None,
            })
            .collect();
        assert_eq!(states, ["idle"]);
        assert_eq!(codex.prompt("thr_1", p("x"), "c2"), Err("Codex has exited"));
    }

    #[test]
    fn interrupt_without_a_turn_end_is_unknown_once() {
        let (mut codex, mut peer) = session();
        codex.interrupt_ms = 50;
        codex.interrupt("thr_1", "i1").unwrap();
        peer.recv();
        assert!(codex.tick(now_ms()).is_empty(), "not due yet");
        let late = now_ms() + 50;
        let events = codex.tick(late);
        let why = Some("no turn end from Codex within 30 s");
        assert_eq!(dones(&events), [("i1", "unknown", why)]);
        assert!(codex.tick(late).is_empty(), "fires once");
        // the late reply and turn end close the prompt only
        assert!(dones(&peer.send(&mut codex, r#"{"id":4,"result":{}}"#)).is_empty());
        let events = peer.note(&mut codex, "turn/completed", &turn_completed("interrupted"));
        assert_eq!(dones(&events), [("c1", "cancelled", None)]);
    }

    #[test]
    fn desktop_sign_in_expires_and_a_late_completion_is_ignored() {
        let (mut codex, mut peer) = session();
        codex.auth_ttl_ms = 50;
        codex.mcp_login("fake-linear", "m1").unwrap();
        peer.recv();
        let url = "https://auth.fake-linear.example/oauth/authorize?state=abc";
        peer.send(
            &mut codex,
            &format!(r#"{{"id":4,"result":{{"authorizationUrl":"{url}","loginId":"l1"}}}}"#),
        );
        let events = codex.tick(now_ms() + 50);
        assert_eq!(dones(&events), [("m1", "expired", None)]);
        // the badge goes with it
        assert!(matches!(events.last(), Some(Event::Op(_, Op::Mcp(_)))));
        let completed = r#"{"name":"fake-linear","threadId":null,"loginId":"l1","success":true}"#;
        let events = peer.note(&mut codex, "mcpServer/oauthLogin/completed", completed);
        assert!(dones(&events).is_empty(), "{events:?}");
        // a sign-in codex never answered expires too, and its late URL opens nothing
        codex.mcp_login("fake-linear", "m2").unwrap();
        assert_eq!(dones(&codex.tick(now_ms() + 50)), [("m2", "expired", None)]);
        let login = loop {
            let frame = peer.recv();
            if frame.contains("oauth/login") {
                break json::parse(&frame).unwrap();
            }
        };
        let id = json::get_i64(&login, "id").unwrap();
        let reply =
            format!(r#"{{"id":{id},"result":{{"authorizationUrl":"{url}","loginId":"l2"}}}}"#);
        assert!(peer.send(&mut codex, &reply).is_empty());
    }

    #[test]
    fn resume_reloads_the_thread_then_prompts() {
        let (mut codex, mut peer) = connect();
        peer.recv();
        peer.send(&mut codex, INIT_OK);
        peer.recv();
        let events = codex
            .resume_session("thr_1", r"C:\work", None, p("again"), "p1")
            .unwrap();
        assert!(
            matches!(&events[..], [Event::Op(t, Op::State(s))] if t == "thr_1" && s.status == "starting")
        );
        assert_eq!(
            peer.recv(),
            r#"{"id":2,"method":"thread/resume","params":{"threadId":"thr_1","cwd":"C:\\work","approvalPolicy":"untrusted","sandbox":"danger-full-access","excludeTurns":true}}"#
        );
        // one resume at a time
        assert!(
            codex
                .resume_session("thr_1", r"C:\work", None, p("x"), "p2")
                .is_err()
        );
        assert!(!codex.owns_session("thr_1"));
        let events = peer.send(
            &mut codex,
            r#"{"id":2,"result":{"thread":{"id":"thr_1","turns":[]},"model":"gpt-test"}}"#,
        );
        assert!(codex.owns_session("thr_1"));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::SessionStarted { .. }))
        );
        assert!(items(&events).iter().any(|i| i.text == "again"));
        assert_eq!(
            peer.recv(),
            r#"{"id":3,"method":"turn/start","params":{"threadId":"thr_1","input":[{"type":"text","text":"again"}]}}"#
        );
        // a thread codex no longer has: the command fails and the session ends
        let events = codex
            .resume_session("thr_9", r"C:\work", None, p("hi"), "p3")
            .unwrap();
        assert_eq!(events.len(), 1);
        peer.recv();
        let events = peer.send(
            &mut codex,
            r#"{"id":4,"error":{"code":-32600,"message":"no rollout found for thread id thr_9"}}"#,
        );
        let why = "Codex could not resume this session: no rollout found for thread id thr_9";
        assert_eq!(dones(&events), [("p3", "failed", Some(why))]);
        assert!(
            matches!(events.last(), Some(Event::Op(t, Op::State(s))) if t == "thr_9" && s.status == "ended")
        );
        assert!(!codex.owns_session("thr_9"));
    }

    #[test]
    fn reasoning_plan_usage_and_errors_map() {
        let (mut codex, mut peer) = session();
        let reasoning = |id: &str, summary: &[&str], content: &[&str]| {
            let list = |v: &[&str]| Json::Arr(v.iter().map(|x| s(x)).collect());
            let item = json::obj(vec![
                ("type", s("reasoning")),
                ("id", s(id)),
                ("summary", list(summary)),
                ("content", list(content)),
            ]);
            on_turn(vec![("item", item)])
        };
        // an empty reasoning item stays off the phone until its first words
        let events = peer.note(&mut codex, "item/started", &reasoning("rs_1", &[], &[]));
        assert!(events.is_empty(), "{events:?}");
        let first = peer.delta(
            &mut codex,
            "item/reasoning/summaryTextDelta",
            "rs_1",
            "**Reading**",
        );
        let it = items(&first)[0];
        assert_eq!(
            (it.kind, it.text.as_str()),
            (ItemKind::Thinking, "**Reading**")
        );
        // summaries win over raw reasoning text
        let mut text = Vec::new();
        for (method, delta) in [
            ("item/reasoning/summaryPartAdded", ""),
            ("item/reasoning/summaryTextDelta", "Plan"),
            ("item/reasoning/textDelta", "raw words"),
        ] {
            text.extend(appended(
                &peer.delta(&mut codex, method, "rs_1", delta),
                Field::Text,
            ));
        }
        assert_eq!(text, ["\n\n", "Plan"]);
        // one that never gets words is never shown
        assert!(
            peer.note(&mut codex, "item/started", &reasoning("rs_0", &[], &[]))
                .is_empty()
        );
        assert!(
            peer.note(&mut codex, "item/completed", &reasoning("rs_0", &[], &[]))
                .is_empty()
        );
        let done = reasoning("rs_1", &["**Reading**", "Plan"], &["raw words"]);
        let events = peer.note(&mut codex, "item/completed", &done);
        let it = items(&events)[0];
        assert_eq!(
            (it.kind, it.text.as_str(), it.done),
            (ItemKind::Thinking, "**Reading**\n\nPlan", true)
        );
        // raw text shown first is replaced once a summary arrives
        peer.note(&mut codex, "item/started", &reasoning("rs_2", &[], &[]));
        let events = peer.delta(&mut codex, "item/reasoning/textDelta", "rs_2", "raw ");
        assert_eq!(items(&events)[0].text, "raw ");
        let events = peer.delta(&mut codex, "item/reasoning/summaryTextDelta", "rs_2", "sum");
        assert_eq!(items(&events)[0].text, "");
        assert_eq!(appended(&events, Field::Text), ["sum"]);

        let plan = r#"[{"step":"Read","status":"completed"},{"step":"Fix","status":"inProgress"},{"step":"Test","status":"pending"}]"#;
        let params = on_turn(vec![
            ("explanation", s("Fixing")),
            ("plan", json::parse(plan).unwrap()),
        ]);
        let events = peer.note(&mut codex, "turn/plan/updated", &params);
        let it = items(&events)[0];
        assert_eq!((it.id.as_str(), it.kind), ("plan:turn_1", ItemKind::Plan));
        let steps: Vec<_> = it
            .steps
            .iter()
            .map(|s| (s.text.as_str(), s.status))
            .collect();
        assert_eq!(
            steps,
            [
                ("Read", "completed"),
                ("Fix", "in_progress"),
                ("Test", "pending")
            ]
        );

        // usage comes from `last`, not the thread total
        let usage = r#"{"total":{"totalTokens":9999,"inputTokens":9000,"cachedInputTokens":8000,"cacheWriteInputTokens":0,"outputTokens":999,"reasoningOutputTokens":99},"last":{"totalTokens":1248,"inputTokens":1200,"cachedInputTokens":1024,"cacheWriteInputTokens":0,"outputTokens":48,"reasoningOutputTokens":64},"modelContextWindow":272000}"#;
        let params = on_turn(vec![("tokenUsage", json::parse(usage).unwrap())]);
        let events = peer.note(&mut codex, "thread/tokenUsage/updated", &params);
        let want = Usage {
            input: 1200,
            output: 48,
            cache_read: Some(1024),
            reasoning: Some(64),
        };
        assert_eq!(turn_op(&events).usage, Some(want));

        // a retry is a warning, the final error an error notice and a failed turn
        let error = |retry: bool, message: &str| {
            let e = json::obj(vec![
                ("message", s(message)),
                ("codexErrorInfo", s("serverOverloaded")),
                ("additionalDetails", Json::Null),
                ("misalignment", Json::Null),
            ]);
            on_turn(vec![("error", e), ("willRetry", Json::Bool(retry))])
        };
        for (retry, message, level) in [
            (true, "Reconnecting... 1/5", "warning"),
            (false, "The model is overloaded.", "error"),
        ] {
            let events = peer.note(&mut codex, "error", &error(retry, message));
            let it = items(&events)[0];
            assert_eq!(
                (it.kind, it.level, it.text.as_str()),
                (ItemKind::Notice, Some(level), message)
            );
        }
        let failed = r#"{"threadId":"thr_1","turn":{"id":"turn_1","status":"failed","error":{"message":"The model is overloaded.","codexErrorInfo":"serverOverloaded","additionalDetails":null,"misalignment":null}}}"#;
        let events = peer.note(&mut codex, "turn/completed", failed);
        let turn = turn_op(&events);
        assert_eq!(
            (turn.status, turn.error.as_deref(), turn.usage.is_some()),
            ("failed", Some("The model is overloaded."), true)
        );
        assert_eq!(
            dones(&events),
            [("c1", "failed", Some("The model is overloaded."))]
        );
    }

    #[test]
    fn launch_config_applies_the_workspace_policy() {
        let root = std::env::temp_dir().join(format!("chb-codex-{}", std::process::id()));
        let (ws, data, home) = (root.join("ws"), root.join("data"), root.join("home"));
        for dir in [&ws, &data, &home, &root.join("ws2").join("data")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(root.join("file.txt"), "x").unwrap();
        let check = |p: &Path, data: &Path| workspace(&p.to_string_lossy(), data, Some(&home));
        let w = check(&ws, &data).unwrap();
        assert_eq!(w.path, canonical(&ws).unwrap().to_string_lossy());
        assert!(!w.path.starts_with(r"\\?\"), "{}", w.path);
        assert_eq!(w.label, "ws");
        let hash: String = crypto::sha256(w.path.as_bytes())[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(w.id, hash);
        // stable: another spelling of the same folder keeps the id
        assert_eq!(check(&ws.join("..").join("ws"), &data).unwrap().id, w.id);
        for (path, why) in [
            (PathBuf::from("ws"), "is not an absolute path"),
            (root.join("missing"), "does not exist"),
            (root.join("file.txt"), "is not a folder"),
            (PathBuf::from(r"C:\"), "is a drive root"),
            (home.clone(), "user profile"),
            (root.clone(), "user profile"),
        ] {
            let err = check(&path, &data).unwrap_err();
            assert!(err.contains(why), "{path:?}: {err}");
        }
        let err = check(&root.join("ws2"), &root.join("ws2").join("data")).unwrap_err();
        assert!(err.contains("CHERNOBYL data folder"), "{err}");

        let entry = |folders: &[&Path], extra: &str| {
            let list = Json::Arr(folders.iter().map(|p| s(&p.to_string_lossy())).collect());
            json::parse(&format!(
                r#"{{"type":"codex","workspaces":{}{extra}}}"#,
                json::stringify(&list)
            ))
            .unwrap()
        };
        let cfg = launch_config(
            &entry(&[&ws], r#","binary":null,"args":["fake.mjs"]"#),
            &data,
        )
        .unwrap();
        assert_eq!(cfg.binary, None, "found at every spawn");
        assert_eq!(cfg.argv(), ["fake.mjs", "app-server"]);
        assert_eq!(cfg.workspaces, std::slice::from_ref(&w));
        let live = cfg.agent_live();
        assert_eq!(
            (live.prompt_while_busy, live.interrupt, live.end),
            ("merge", true, false)
        );
        assert_eq!(live.workspaces, [w]);
        let custom =
            launch_config(&entry(&[&ws], r#","binary":"C:\\tools\\codex.exe""#), &data).unwrap();
        assert_eq!(custom.binary, Some(PathBuf::from(r"C:\tools\codex.exe")));
        assert_eq!(
            custom.resolve_binary().unwrap(),
            PathBuf::from(r"C:\tools\codex.exe")
        );
        let again = ws.join("..").join("ws");
        for (bad, why) in [
            (entry(&[&ws, &again], ""), "repeats an earlier folder"),
            (entry(&[], ""), "at least one workspace"),
            (entry(&[&ws], r#","args":[1]"#), "array of strings"),
            (entry(&[&ws], r#","binary":"""#), "non-empty string"),
            (
                json::parse(r#"{"type":"codex"}"#).unwrap(),
                "array of folders",
            ),
        ] {
            let err = launch_config(&bad, &data).unwrap_err();
            assert!(err.contains(why), "{err}");
        }
        // honesty: a limited capability says why
        let caps = capabilities();
        assert!(
            caps.iter()
                .all(|c| (c.status == "limited") == c.detail.is_some())
        );
        assert!(
            caps.iter()
                .any(|c| c.id == "session.list" && c.status == "limited")
        );
        let mut ids: Vec<&str> = caps.iter().map(|c| c.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), caps.len());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_codex_takes_path_then_the_newest_app_install_never_a_stub() {
        let root = std::env::temp_dir().join(format!("chb-find-codex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (on_path, stubs, lad) = (
            root.join("path"),
            root.join("WindowsApps"),
            root.join("lad"),
        );
        let bin = lad.join("OpenAI").join("Codex").join("bin");
        for d in [
            &on_path,
            &stubs,
            &bin.join("aaa"),
            &bin.join("bbb"),
            &bin.join("rg"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        let write = |p: &Path, body: &str, secs: u64| {
            std::fs::write(p, body).unwrap();
            let at = std::time::UNIX_EPOCH + Duration::from_secs(secs);
            let f = std::fs::File::options().write(true).open(p).unwrap();
            f.set_modified(at).unwrap();
        };
        // an app-execution alias (WindowsApps) and a 0-byte file are not codex
        write(&stubs.join("codex.exe"), "alias", 3_000_000_000);
        write(&on_path.join("codex.exe"), "", 3_000_000_000);
        write(&bin.join("rg").join("rg.exe"), "rg", 3_000_000_000);
        let paths = std::env::join_paths([&stubs, &on_path]).unwrap();
        let find = || find_codex(Some(&paths), Some(&lad));
        assert!(find().unwrap_err().contains("not found"), "{:?}", find());
        assert!(find_codex(None, None).is_err());
        // the newest install wins: an app update adds a folder
        write(&bin.join("aaa").join("codex.exe"), "old", 2_000_000_000);
        write(&bin.join("bbb").join("codex.exe"), "new", 2_100_000_000);
        assert_eq!(find().unwrap(), bin.join("bbb").join("codex.exe"));
        write(&bin.join("aaa").join("codex.exe"), "newer", 2_200_000_000);
        assert_eq!(find().unwrap(), bin.join("aaa").join("codex.exe"));
        // a real codex.exe on PATH comes first
        write(&on_path.join("codex.exe"), "cli", 1_000_000_000);
        assert_eq!(find().unwrap(), on_path.join("codex.exe"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The real spawn path against the committed fake app-server (an
    /// independent reading of the same schema): it exits 3 on any protocol
    /// violation (FAKE-CODEX-VIOLATION on stderr), 0 on a clean run.
    #[test]
    fn fake_app_server_runs_hello_run_edit_and_exits_clean() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tools")
            .join("fake-codex-app-server.mjs");
        let env = crate::runtime_io::child_env(std::env::vars_os(), &[]);
        let cwd = std::env::temp_dir();
        let (child, rx) = match ChildIo::spawn(Path::new("node"), &[script.as_os_str()], &cwd, &env)
        {
            Ok(spawned) => spawned,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                eprintln!("skipped: node is not on PATH");
                return;
            }
            Err(e) => panic!("spawn node: {e}"),
        };
        let mut codex = Codex::new(child, "codex-1", "9.9.9");
        codex
            .start_session(&cwd.to_string_lossy(), p("hello run edit"), "c1")
            .unwrap();
        let (mut seen, mut asked) = (Vec::new(), 0);
        let deadline = Instant::now() + Duration::from_secs(60);
        let code = loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("the fake finishes within a minute");
            if let ChildEvent::Exit(code) = event {
                break code;
            }
            for e in codex.on_event(event) {
                match &e {
                    Event::Permission(p) => {
                        asked += 1;
                        codex
                            .decide(&p.id, "approve-once", Some("accept"), &[])
                            .unwrap();
                    }
                    Event::Done { tag, .. } if tag == "c1" => codex.shutdown(),
                    _ => {}
                }
                seen.push(e);
            }
        };
        assert_eq!(
            code,
            Some(0),
            "fake app-server stderr: {}",
            codex.stderr_tail()
        );
        assert_eq!(asked, 2, "a command approval and a file-change approval");
        assert!(seen.contains(&done("c1".into(), "completed", None)));
        let items: Vec<&Item> = seen
            .iter()
            .filter_map(|e| match e {
                Event::Op(_, Op::Item(i)) => Some(i),
                _ => None,
            })
            .collect();
        let users = items.iter().filter(|i| i.kind == ItemKind::User).count();
        assert_eq!(users, 1, "the echoed input is not a second bubble");
        let finished = |name: &str| {
            items
                .iter()
                .filter_map(|i| i.tool.as_ref())
                .find(|t| t.name == name && t.status == ToolStatus::Completed)
                .unwrap_or_else(|| panic!("a completed {name}"))
        };
        let shell = finished("shell");
        assert!(
            shell.output.contains("«redacted:anthropic-key»") && !shell.output.contains("sk-ant-"),
            "{}",
            shell.output
        );
        assert_eq!(finished("apply_patch").diffs.len(), 2);
        let turn_files = seen
            .iter()
            .filter_map(|e| match e {
                Event::Op(_, Op::Turn(t)) => Some(t.diffs.len()),
                _ => None,
            })
            .max();
        assert_eq!(turn_files, Some(2));
        assert!(seen.iter().any(|e| matches!(
            e,
            Event::Op(
                _,
                Op::Append {
                    field: Field::Text,
                    ..
                }
            )
        )));
    }
}
