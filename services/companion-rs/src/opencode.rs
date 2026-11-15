//! OpenCode adapter (Phase 3 — PRD §7.4): attach-only HTTP/SSE client for an
//! existing `opencode serve` instance. Every endpoint used here was live-
//! verified in Phase 0 against opencode 1.17.7 — see
//! `docs/reverse-engineering/opencode.md` (raw captures in `evidence/`).
//!
//! Safety rules carried from that probe (adapter-sdk G4):
//!  * **G4.3** — never spawn a second server: the adapter only attaches; a
//!    dead endpoint is `runtime-unavailable`, not a spawn request (two CLI
//!    generations sharing `~/.local/share/opencode` kill turns on FK skew —
//!    probe §9).
//!  * **G4.2** — status codes prove nothing: unknown routes answer the
//!    web-UI HTML page with HTTP 200 (probe §9.3), so every response is
//!    content-type + shape validated before it is trusted.
//!  * **G4.1** — `prompt_async`'s 204 is acceptance only: the turn outcome
//!    arrives later on the SSE stream (`session.error` is the failure
//!    channel, probe §8) and is correlated back to the original commandId
//!    through the in-flight map.
//!
//! The adapter is transport + normalization only: it emits `EventKind`s via
//! `subscribe` and answers `send` with acceptance acks; sequence numbers,
//! persistence and fan-out stay with the companion (Phase 3 wiring step).
//!
//! ADR-015 step 5 (§3.6 OpenCode table, §4.4): rich parts. Text and
//! reasoning parts become transcript items that grow by `message.part.delta`
//! appends; tool parts become tool items keyed by `callID`, which is what a
//! `permission.asked` or `question.asked` names, so approvals sit inline on
//! their card. Conversation text goes to the transcript hub only (memory),
//! never to journaled Activity. Turns close on idle (prompt commands
//! `completed`), on `MessageAbortedError` (interrupted, prompts `cancelled`)
//! or on another `session.error` (failed). A watched session is hydrated
//! from `GET /session/{id}/message` on its first subscriber and after every
//! SSE reconnect (deltas are not replayable), and `GET /mcp` is polled while
//! anyone watches. Verified against 1.19.4 captures
//! (`tests/fixtures/opencode/`).
//!
//! ADR-021 (parity with Claude Code): the serve's project folder (`GET
//! /path`) is the agent's one workspace when it passes the same rules as a
//! configured Claude workspace, so Files, Changes and Terminal work. The
//! composer takes a model (`GET /config/providers`, sent with each prompt),
//! photos (`file` parts with `data:` URLs), slash commands (`GET /command`,
//! run through `POST /session/{id}/command`), the context meter (the last
//! assistant message's tokens against the model's limit) and rename (`PATCH
//! /session/{id}`); `submit_prompt {fork}` forks natively.

use crate::adapters::{Capability, LiveRuntime};
use crate::claude_live::validate_workspace;
use crate::client::{HttpResponse, http_request, http_request_waiting, split_url};
use crate::json::{self, Json};
use crate::protocol::{
    ActivityEvent, Agent, AgentLive, ClientMsg, EventKind, FileChange, LIVE_PERMISSION_TTL_MS,
    LiveWorkspace, PermissionOption, PermissionRequest, PromptImage, ServerMsg, Session,
    is_provider_model, next_id, now_ms,
};
use crate::runtime_io::{AUTH_TTL_MS, PendingAuth, check_callback_url, form_decode, redact};
use crate::transcript::{
    Field, FileDiff, HubHandle, Item, ItemKind, LiveState, McpAuthState, McpServer, Op, PlanStep,
    ToolCall, ToolStatus, TurnState, Usage,
};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Moved to `adapters` with the `LiveRuntime` trait (ADR-015 step 4); the
/// old path keeps working for `ollama`, `claude` and external callers.
pub use crate::adapters::AdapterError;

type Sub = Arc<dyn Fn(EventKind) + Send + Sync>;

/* ------------------------------------------------------------------ */
/* State                                                               */
/* ------------------------------------------------------------------ */

#[derive(Default)]
struct OcState {
    healthy: bool,
    detail: Option<String>,
    version: String,
    sessions: Vec<Session>,
    agents: Vec<Agent>,
    /// sessionID → protocol status, tracked from SSE (`session.status`/
    /// `session.idle`). Sessions listed before attach have no observed
    /// status and default to `completed` — see `capabilities()`.
    statuses: HashMap<String, String>,
    /// In-flight prompt commands: (session_id, command_id, running sent) —
    /// the correlation that turns the turn end into the right
    /// `command_result` (G1). A prompt sent mid-turn is queued natively and
    /// the session goes idle only after every queued turn ran, so the turn
    /// end closes all of the session's prompts.
    pending: Vec<(String, String, bool)>,
    /// Native permission requests and questions we surfaced (native id →
    /// record), so a reply from elsewhere resolves without fabricating state.
    asked: HashMap<String, PermissionRequest>,
    /// partID → kind of the assistant text and reasoning parts:
    /// `message.part.delta` names its part by id only (the delta itself
    /// carries the messageID).
    parts: HashMap<String, ItemKind>,
    /// messageID → (is a user message, its turn id = the user messageID).
    roles: HashMap<String, (bool, String)>,
    /// sessionID → its recent turns and the interrupt awaiting a turn end.
    live: HashMap<String, Live>,
    /// callID → the permission or question asked for that tool call.
    perms: HashMap<String, String>,
    /// callIDs whose request was rejected: their tool `error` is a denial.
    rejected: HashSet<String>,
    /// Sessions a phone subscribed to (MCP polling, re-hydration on
    /// reconnect); the poller drops the ones nobody watches any more.
    watched: HashSet<String>,
    poller: bool,
    /// MCP sign-ins in flight (§4.7), keyed (session, server).
    auths: PendingAuth<Auth>,
    /// The last `GET /mcp`, mapped.
    mcp: Vec<McpServer>,
    /// server → the sign-in URL OpenCode could not open on the desktop.
    open_failed: HashMap<String, String>,
    /// ADR-021: the serve's project folder, once it passed
    /// `validate_workspace`.
    workspace: Option<LiveWorkspace>,
    /// ADR-021: models, the configured model and the slash commands.
    catalog: Catalog,
    /// sessionID → the `provider/model` the phone chose (sent with every
    /// prompt of that session).
    chosen: HashMap<String, String>,
    /// sessionID → the model its last assistant message ran on.
    last_model: HashMap<String, String>,
    /// sessionID → (used, max) context tokens of its last assistant message.
    context: HashMap<String, (i64, i64)>,
    /// user messageID → its image part ids (the user item shows a count).
    images: HashMap<String, Vec<String>>,
    /// sessionID → the title of the session it was forked from.
    forks: HashMap<String, String>,
    /// ADR-021: sessions the phone ended. OpenCode has no end route (and
    /// `DELETE /session` destroys the history), so ending is ours: the turn
    /// is aborted and the session refuses prompts until a restart.
    ended: HashSet<String>,
    /// ADR-021: the phone's permission modes OpenCode's agents back
    /// (`default` = the built-in `build` agent, `plan` = `plan`).
    offered_modes: Vec<&'static str>,
    /// sessionID → the agent the phone chose (`build` | `plan`), sent with
    /// every prompt of that session.
    picked: HashMap<String, &'static str>,
    /// sessionID → the agent its last answer ran as.
    last_agent: HashMap<String, String>,
}

/// ADR-021: the model and agent a session's prompts carry.
#[derive(Default)]
struct Choice {
    model: Option<String>,
    agent: Option<&'static str>,
}

/// ADR-021: the phone's permission mode → OpenCode's built-in agent. Only
/// these two: a user-defined agent may allow what `build` asks for.
fn mode_agent(mode: &str) -> Option<&'static str> {
    match mode {
        "default" => Some("build"),
        "plan" => Some("plan"),
        _ => None,
    }
}

fn agent_mode(agent: &str) -> Option<&'static str> {
    match agent {
        "build" => Some("default"),
        "plan" => Some("plan"),
        _ => None,
    }
}

const ENDED: &str = "This session was ended; start a new one";

/// ADR-021: what `GET /config/providers`, `GET /config` and `GET /command`
/// answered on the last probe.
#[derive(Default, Clone)]
struct Catalog {
    /// (`provider/model`, label, context limit), the configured model first.
    models: Vec<(String, String, i64)>,
    /// `GET /config` `model`.
    default_model: Option<String>,
    /// Slash command names, without the slash.
    commands: Vec<String>,
}

#[derive(Default)]
struct Live {
    /// Newest last, at most `TURNS_KEPT`.
    turns: Vec<TurnState>,
    /// The `interrupt` command waiting for the turn to end.
    interrupt: Option<String>,
    /// The last `state` op status pushed ("" = none yet).
    state: &'static str,
}

/// One MCP sign-in (§4.7). `callback` is set for the phone flow: the
/// redirect port and path, and OpenCode's `oauthState`.
struct Auth {
    command_id: String,
    state: &'static str,
    url: Option<String>,
    prefix: Option<String>,
    callback: Option<(u16, String, String)>,
}

/// Bounds for the id maps (ids are creation-ordered; the older half goes).
const MAP_MAX: usize = 4096;
/// Desktop sign-in threads open at once (each waits up to `AUTH_TTL_MS`).
const MAX_DESKTOP_AUTHS: usize = 4;
static DESKTOP_AUTHS: AtomicUsize = AtomicUsize::new(0);
const TURNS_KEPT: usize = 8;
const SESSIONS_KEPT: usize = 64;
const MCP_POLL: Duration = Duration::from_secs(30);
/// §4.6: an interrupt with no turn end in this long ends `unknown`.
const INTERRUPT_WAIT: Duration = Duration::from_secs(30);
/// A slash command's `POST /session/{id}/command` answers when its turn
/// ends; the turn's outcome comes from the event stream before that.
const COMMAND_WAIT: Duration = Duration::from_secs(3600);
/// Models offered to the phone (the transcript hub keeps as many).
const MODELS_MAX: usize = 32;
/// Permission records are journaled: legacy command and detail stay small.
const COMMAND_CAP: usize = 2 << 10;
const DETAIL_CAP: usize = 64 << 10;

struct Inner {
    /// Adapter id (also the `host_id` of every entity it maps).
    id: String,
    endpoint: String,
    /// `user:pass` for servers started with `OPENCODE_SERVER_PASSWORD`
    /// (loopback HTTP carries it as Basic; no TLS — local-only surface).
    auth: Option<String>,
    state: Mutex<OcState>,
    subs: Mutex<Vec<(u64, Sub)>>,
    next_sub: AtomicU64,
    stop: AtomicBool,
    /// The live SSE socket, kept so `dispose()` can unblock the reader.
    killer: Mutex<Option<TcpStream>>,
    started: AtomicBool,
    sse_read_timeout: Mutex<Duration>,
    malformed_events: AtomicU64,
    /// ADR-015: transcripts of the sessions a phone watches.
    hub: HubHandle,
    /// Asks the MCP poller for a poll now (auth calls, MCP toasts).
    poke: AtomicBool,
    /// ADR-021: the companion's data directory, which a workspace must not
    /// contain; `None` (probes) advertises no workspace.
    data_dir: Option<PathBuf>,
}

pub struct OpenCodeAdapter {
    pub id: String,
    inner: Arc<Inner>,
}

/* ------------------------------------------------------------------ */
/* Construction + public surface                                       */
/* ------------------------------------------------------------------ */

impl OpenCodeAdapter {
    /// An adapter streaming into a detached hub (probes, tests).
    pub fn new(id: &str, endpoint: &str, basic_auth: Option<&str>) -> OpenCodeAdapter {
        OpenCodeAdapter::with_hub(id, endpoint, basic_auth, HubHandle::default(), None)
    }

    /// The companion's adapter: transcripts go to `hub` (`Companion::hub`);
    /// `data_dir` is the companion's (ADR-021 workspace rule).
    pub fn with_hub(
        id: &str,
        endpoint: &str,
        basic_auth: Option<&str>,
        hub: HubHandle,
        data_dir: Option<&Path>,
    ) -> OpenCodeAdapter {
        let ep = endpoint.trim();
        let ep = if ep.starts_with("http://") || ep.starts_with("https://") {
            ep.to_string()
        } else {
            format!("http://{ep}")
        };
        OpenCodeAdapter {
            id: id.to_string(),
            inner: Arc::new(Inner {
                id: id.to_string(),
                endpoint: ep.trim_end_matches('/').to_string(),
                auth: basic_auth.map(str::to_string),
                state: Mutex::new(OcState::default()),
                subs: Mutex::new(Vec::new()),
                next_sub: AtomicU64::new(0),
                stop: AtomicBool::new(false),
                killer: Mutex::new(None),
                started: AtomicBool::new(false),
                sse_read_timeout: Mutex::new(Duration::from_secs(90)),
                malformed_events: AtomicU64::new(0),
                hub,
                poke: AtomicBool::new(false),
                data_dir: data_dir.map(Path::to_path_buf),
            }),
        }
    }

    /// Total malformed/corrupted SSE events ignored since boot.
    pub fn malformed_events(&self) -> u64 {
        self.inner.malformed_events.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub fn set_sse_read_timeout(&self, timeout: Duration) {
        *self.inner.sse_read_timeout.lock().unwrap() = timeout;
    }

    /// PRD §7.3 honest capability declaration. Structural rules (tested):
    /// unique ids, every non-`available` entry carries a `detail`, and at
    /// least one entry is not `available`.
    pub fn capabilities(&self) -> Vec<Capability> {
        vec![
            Capability::new("server.discovery", "available", None),
            Capability::new("server.health", "available", None),
            Capability::new("session.list", "available", None),
            Capability::new("agent.list", "available", None),
            Capability::new("session.create", "available", None),
            Capability::new("session.prompt", "available", None),
            // ADR-015 step 5: live-verified on 1.19.4 (tests/opencode_e2e.rs
            // `live_opencode_rich_parts`): abort mid-stream, inline replies
            // once/reject+message, streaming text/reasoning/tool parts
            Capability::new("session.interrupt", "available", None),
            Capability::new("permission.reply", "available", None),
            Capability::new("session.output", "available", None),
            Capability::new("mcp.status", "available", None),
            // ADR-021, live-verified on 1.19.4 (`live_opencode_parity`)
            Capability::new("session.model", "available", None),
            Capability::new("session.rename", "available", None),
            Capability::new("session.commands", "available", None),
            Capability::new("session.fork", "available", None),
            Capability::new(
                "session.images",
                "limited",
                Some(
                    "sent as data: file parts; whether the model sees them depends on the model OpenCode runs",
                ),
            ),
            Capability::new(
                "workspace.files",
                "dependent-config",
                Some(
                    "the folder opencode serve runs in, when it is not a drive root, the profile folder or a folder holding the CHERNOBYL data directory",
                ),
            ),
            Capability::new(
                "session.diff",
                "limited",
                Some(
                    "summary.diffs are mapped to turn diffs and Session.files; no populated capture yet (opencode-live-events.md §2.7)",
                ),
            ),
            Capability::new(
                "mcp.auth",
                "limited",
                Some(
                    "start, callback check and reconnect verified against the served API shapes; completing a real OAuth login is not verified live",
                ),
            ),
            Capability::new(
                "event.replay",
                "unavailable",
                Some(
                    "SSE has no cursor: after reconnect state is re-listed and watched transcripts are rebuilt from GET /session/{id}/message",
                ),
            ),
            Capability::new(
                "server.spawn",
                "unavailable",
                Some(
                    "attach-only by design (G4.3): never start a second server on a possibly-shared data directory",
                ),
            ),
        ]
    }

    /// Health + list probe; latches failure into `healthy`/`detail`.
    pub fn refresh(&self) -> Result<(), AdapterError> {
        self.inner.refresh()
    }

    pub fn is_healthy(&self) -> bool {
        self.inner.state.lock().unwrap().healthy
    }

    pub fn detail(&self) -> Option<String> {
        self.inner.state.lock().unwrap().detail.clone()
    }

    pub fn version(&self) -> String {
        self.inner.state.lock().unwrap().version.clone()
    }

    pub fn sessions(&self) -> Vec<Session> {
        self.inner.state.lock().unwrap().sessions.clone()
    }

    pub fn agents(&self) -> Vec<Agent> {
        self.inner.state.lock().unwrap().agents.clone()
    }

    /// Routes one client message to the native surface. Acceptance-only:
    /// `Ok(Ack)` never claims an outcome (G4.1), `Err` is a transport or
    /// validation fault for the caller to surface.
    pub fn send(&self, msg: &ClientMsg) -> Result<ServerMsg, AdapterError> {
        // ADR-021: an ended session takes no prompt or setting (a fork is a
        // new session, so it still may)
        let refused = match msg {
            ClientMsg::SubmitPrompt {
                command_id,
                session_id,
                fork: false,
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
            }
            | ClientMsg::SetPermissionMode {
                command_id,
                session_id,
                ..
            } => Some((command_id, session_id)),
            _ => None,
        };
        if let Some((command_id, session_id)) = refused
            && self.inner.state.lock().unwrap().ended.contains(session_id)
        {
            return Ok(ack(
                command_id,
                false,
                Some(ENDED.into()),
                Some(session_id),
                None,
            ));
        }
        match msg {
            ClientMsg::CreateSession {
                command_id,
                agent_id,
                title,
                prompt,
                project,
                images,
                model,
                ..
            } => self.inner.create_session(
                command_id,
                agent_id,
                title,
                prompt,
                project.as_deref(),
                images,
                model.as_deref(),
            ),
            ClientMsg::SessionEnd {
                command_id,
                session_id,
            } => self.inner.end(command_id, session_id),
            ClientMsg::SetPermissionMode {
                command_id,
                session_id,
                mode,
            } => Ok(self.inner.set_mode(command_id, session_id, mode)),
            // ADR-021: a native fork (`POST /session/{id}/fork`), then the
            // prompt in the fork
            ClientMsg::SubmitPrompt {
                command_id,
                session_id,
                text,
                images,
                fork: true,
            } => self.inner.fork(command_id, session_id, text, images),
            ClientMsg::SubmitPrompt {
                command_id,
                session_id,
                text,
                images,
                ..
            } => {
                self.inner.prompt(command_id, session_id, text, images)?;
                Ok(ack(command_id, true, None, Some(session_id), None))
            }
            ClientMsg::SetModel {
                command_id,
                session_id,
                model,
            } => Ok(self.inner.set_model(command_id, session_id, model)),
            ClientMsg::RenameSession {
                command_id,
                session_id,
                title,
            } => self.inner.rename(command_id, session_id, title),
            ClientMsg::Interrupt {
                command_id,
                session_id,
            } => self.inner.interrupt(command_id, session_id),
            ClientMsg::PermissionDecision {
                command_id,
                permission_id,
                decision,
                option_id,
                message,
                answers,
                ..
            } => self.inner.permission_decision(
                command_id,
                permission_id,
                // the engine always selects an option; a bare legacy bucket
                // only reaches here from callers without one
                option_id.as_deref().unwrap_or(match decision.as_str() {
                    "approve-once" => "once",
                    "approve-session" => "always",
                    "reject" => "reject",
                    _ => "",
                }),
                message.as_deref(),
                answers,
            ),
            ClientMsg::McpAuth {
                command_id,
                session_id,
                server,
                flow,
            } => self.inner.mcp_auth(command_id, session_id, server, flow),
            ClientMsg::McpAuthCallback {
                command_id,
                session_id,
                server,
                callback_url,
            } => self
                .inner
                .mcp_auth_callback(command_id, session_id, server, callback_url),
            ClientMsg::McpReconnect {
                command_id,
                session_id,
                server,
            } => Ok(self.inner.mcp_reconnect(command_id, session_id, server)),
            ClientMsg::Hello { .. }
            | ClientMsg::Ping { .. }
            | ClientMsg::ListState { .. }
            | ClientMsg::HsHello { .. }
            | ClientMsg::HsProof { .. }
            | ClientMsg::Workspace { .. } => Err(AdapterError::Unsupported(
                "session-neutral messages stay with the engine".into(),
            )),
            // Subscriptions never land here (the server owns them per
            // connection).
            ClientMsg::SessionSubscribe { .. } | ClientMsg::SessionUnsubscribe { .. } => Err(
                AdapterError::Unsupported("not supported by this agent".into()),
            ),
        }
    }

    /// ADR-015: the first subscriber of one of our sessions arrived. Hydrate
    /// its transcript on a short-lived thread and keep `GET /mcp` polled
    /// while anyone watches.
    pub fn on_subscribe(&self, session_id: &str) {
        let inner = &self.inner;
        {
            let mut st = inner.state.lock().unwrap();
            st.watched.insert(session_id.to_string());
            if !st.poller {
                let poller = inner.clone();
                st.poller = std::thread::Builder::new()
                    .name("opencode-mcp".into())
                    .spawn(move || poller.poll_loop())
                    .is_ok();
            }
        }
        inner.poke.store(true, Ordering::SeqCst);
        inner.spawn_hydrate(session_id.to_string());
    }

    /// `GET /session/:id/diff` — validated raw payload (shape mapping lands
    /// with the app-facing wiring; the probe verified the endpoint).
    pub fn diff(&self, session_id: &str) -> Result<Json, AdapterError> {
        let resp = self.inner.req(
            "GET",
            &format!("{}/session/{session_id}/diff", self.inner.endpoint),
            None,
        )?;
        if resp.status != 200 {
            return Err(AdapterError::InvalidResponse(format!(
                "diff answered HTTP {}",
                resp.status
            )));
        }
        validate_json(&resp)
    }

    /// `GET /session/:id/message` — validated raw payload (probe §8).
    pub fn messages(&self, session_id: &str) -> Result<Json, AdapterError> {
        let resp = self.inner.req(
            "GET",
            &format!("{}/session/{session_id}/message", self.inner.endpoint),
            None,
        )?;
        if resp.status != 200 {
            return Err(AdapterError::InvalidResponse(format!(
                "session messages answered HTTP {}",
                resp.status
            )));
        }
        validate_json(&resp)
    }

    /// Event-stream subscription. Events arrive as normalized `EventKind`s;
    /// the subscriber owns seq/persistence/fan-out.
    pub fn subscribe<F: Fn(EventKind) + Send + Sync + 'static>(&self, cb: F) -> u64 {
        let id = self.inner.next_sub.fetch_add(1, Ordering::SeqCst) + 1;
        self.inner.subs.lock().unwrap().push((id, Arc::new(cb)));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        self.inner.subs.lock().unwrap().retain(|(i, _)| *i != id);
    }

    /// Whether the adapter surfaced this native permission request — the
    /// routing gate that keeps engine-owned permission decisions on the
    /// engine (companion.rs `routes_to_adapter`).
    pub fn knows_permission(&self, permission_id: &str) -> bool {
        self.inner
            .state
            .lock()
            .unwrap()
            .asked
            .contains_key(permission_id)
    }

    /// Spawns the SSE reader (once). It reconnects with a bounded backoff
    /// until `dispose` — restart/recovery is part of the loop, not an
    /// afterthought (PRD Phase 3 exit criteria).
    pub fn start_events(&self) -> Result<(), String> {
        if self.inner.stop.load(Ordering::SeqCst) {
            return Err("adapter disposed".into());
        }
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let inner = self.inner.clone();
        std::thread::Builder::new()
            .name("opencode-sse".into())
            .spawn(move || Inner::sse_loop(inner))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Stops the event stream (shutdown unblocks the reader immediately)
    /// and drops subscribers. Idempotent.
    pub fn dispose(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        if let Some(s) = self.inner.killer.lock().unwrap().take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        self.inner.subs.lock().unwrap().clear();
    }
}

/// ADR-015 §3.3: OpenCode behind the router. Each method delegates to the
/// inherent one of the same name.
impl LiveRuntime for OpenCodeAdapter {
    fn id(&self) -> &str {
        &self.id
    }
    fn is_healthy(&self) -> bool {
        OpenCodeAdapter::is_healthy(self)
    }
    fn agents(&self) -> Vec<Agent> {
        OpenCodeAdapter::agents(self)
    }
    fn sessions(&self) -> Vec<Session> {
        OpenCodeAdapter::sessions(self)
    }
    /// The cached list (re-listed on every probe and SSE reconnect, upserted
    /// by `session.created` and by our own create).
    fn owns_session(&self, session_id: &str) -> bool {
        let st = self.inner.state.lock().unwrap();
        st.sessions.iter().any(|s| s.id == session_id)
    }
    fn knows_permission(&self, permission_id: &str) -> bool {
        OpenCodeAdapter::knows_permission(self, permission_id)
    }
    fn send(&self, msg: &ClientMsg) -> Result<ServerMsg, AdapterError> {
        OpenCodeAdapter::send(self, msg)
    }
    fn on_subscribe(&self, session_id: &str) {
        OpenCodeAdapter::on_subscribe(self, session_id)
    }
    fn refresh(&self) -> Result<(), AdapterError> {
        OpenCodeAdapter::refresh(self)
    }
    fn capabilities(&self) -> Vec<Capability> {
        OpenCodeAdapter::capabilities(self)
    }
    fn dispose(&self) {
        OpenCodeAdapter::dispose(self)
    }
}

/* ------------------------------------------------------------------ */
/* HTTP path                                                           */
/* ------------------------------------------------------------------ */

fn ack(
    command_id: &str,
    ok: bool,
    error: Option<String>,
    session_id: Option<&str>,
    permission_id: Option<&str>,
) -> ServerMsg {
    ServerMsg::Ack {
        command_id: command_id.to_string(),
        ok,
        error,
        duplicate: false,
        session_id: session_id.map(str::to_string),
        permission_id: permission_id.map(str::to_string),
    }
}

/// Maximum HTTP response body size accepted from OpenCode REST endpoints (audit §6 bounded output).
pub const MAX_HTTP_BODY_BYTES: usize = 4 * 1024 * 1024; // 4 MiB

/// G4.2: content-type first, then shape — an HTML page with HTTP 200 (the
/// live SPA fallback) must never parse as success.
fn validate_json(resp: &HttpResponse) -> Result<Json, AdapterError> {
    if resp.body.len() > MAX_HTTP_BODY_BYTES {
        return Err(AdapterError::InvalidResponse(format!(
            "response body exceeds {MAX_HTTP_BODY_BYTES} byte limit (was {})",
            resp.body.len()
        )));
    }
    if !resp.content_type.contains("application/json") {
        let ct = if resp.content_type.is_empty() {
            "(no content-type)".to_string()
        } else {
            resp.content_type.clone()
        };
        return Err(AdapterError::InvalidResponse(format!(
            "expected application/json, got {ct} (HTTP {} — status alone proves nothing)",
            resp.status
        )));
    }
    json::parse(&resp.body)
        .map_err(|e| AdapterError::InvalidResponse(format!("body is not valid JSON: {e:?}")))
}

/// Truncates hostile/unbounded native error text for protocol fields.
fn clip(s: String) -> String {
    if s.chars().count() > 400 {
        let out: String = s.chars().take(400).collect();
        format!("{out}…")
    } else {
        s
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Inner {
    fn req(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
    ) -> Result<HttpResponse, AdapterError> {
        http_request(method, url, self.auth.as_deref(), body).map_err(|e| {
            if e.starts_with("connect") || e.starts_with("resolve") {
                AdapterError::RuntimeUnavailable(e)
            } else {
                AdapterError::Unknown(e)
            }
        })
    }

    fn health(&self) -> Result<(bool, String), AdapterError> {
        let resp = self.req("GET", &format!("{}/global/health", self.endpoint), None)?;
        if resp.status != 200 {
            return Err(AdapterError::InvalidResponse(format!(
                "health answered HTTP {}",
                resp.status
            )));
        }
        let v = validate_json(&resp)?;
        let healthy = json::get_bool(&v, "healthy").ok_or_else(|| {
            AdapterError::InvalidResponse("health response has no `healthy` boolean".into())
        })?;
        let version = json::get_str(&v, "version")
            .unwrap_or("(unknown)")
            .to_string();
        Ok((healthy, version))
    }

    /// Probe: health + session list + agent list into the cache. Any failure
    /// latches `healthy=false` with a human `detail` (never silent).
    fn refresh(&self) -> Result<(), AdapterError> {
        let outcome = (|| {
            let (healthy, version) = self.health()?;
            if !healthy {
                return Err(AdapterError::RuntimeUnavailable(
                    "server reports healthy:false".into(),
                ));
            }
            // Latch health FIRST: fetch_agents maps entity state against it.
            {
                let mut st = self.state.lock().unwrap();
                st.healthy = true;
                st.detail = None;
                st.version = version;
            }
            // ADR-021 extras first: sessions are placed in the workspace and
            // agents advertise it and the composer
            self.fetch_extras();
            let mut sessions = self.fetch_sessions()?;
            let agents = self.fetch_agents()?;
            let mut st = self.state.lock().unwrap();
            // `files` come from turn diffs, which the list does not carry
            for s in &mut sessions {
                if let Some(old) = st.sessions.iter().find(|o| o.id == s.id) {
                    s.files = old.files.clone();
                }
            }
            st.sessions = sessions;
            st.agents = agents;
            Ok(())
        })();
        if let Err(e) = &outcome {
            let mut st = self.state.lock().unwrap();
            st.healthy = false;
            st.detail = Some(e.to_string());
        }
        outcome
    }

    fn fetch_sessions(&self) -> Result<Vec<Session>, AdapterError> {
        let resp = self.req("GET", &format!("{}/session", self.endpoint), None)?;
        if resp.status != 200 {
            return Err(AdapterError::InvalidResponse(format!(
                "session list answered HTTP {}",
                resp.status
            )));
        }
        let v = validate_json(&resp)?;
        let arr = json::as_arr(&v).ok_or_else(|| {
            AdapterError::InvalidResponse("`GET /session` did not answer an array".into())
        })?;
        let st = self.state.lock().unwrap();
        Ok(arr
            .iter()
            .filter_map(|info| Self::map_session(info, &self.id, &st))
            .collect())
    }

    /// ADR-021: `GET /path` (the workspace), `GET /config/providers` and
    /// `GET /config` (models), `GET /command` (slash commands). Best effort:
    /// what a server does not answer stays as it was, and never fails the
    /// probe.
    fn fetch_extras(&self) {
        let get = |path: &str| -> Option<Json> {
            let resp = self
                .req("GET", &format!("{}{path}", self.endpoint), None)
                .ok()?;
            if resp.status != 200 {
                return None;
            }
            validate_json(&resp).ok()
        };
        let workspace = get("/path").map(|p| {
            let dir = json::get_str(&p, "directory")?;
            let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
            validate_workspace(dir, self.data_dir.as_deref()?, profile.as_deref()).ok()
        });
        let providers = get("/config/providers");
        let config = get("/config");
        let commands = get("/command");
        let mut st = self.state.lock().unwrap();
        if let Some(ws) = workspace {
            st.workspace = ws;
        }
        if let Some(c) = &config {
            st.catalog.default_model = json::get_str(c, "model")
                .filter(|m| is_provider_model(m))
                .map(str::to_string);
        }
        if let Some(p) = &providers {
            st.catalog.models = catalog_models(p, st.catalog.default_model.as_deref());
        }
        if let Some(list) = commands.as_ref().and_then(json::as_arr) {
            st.catalog.commands = list
                .iter()
                .filter_map(|c| json::get_str(c, "name"))
                .filter(|n| !n.is_empty() && !n.contains(char::is_whitespace))
                .map(str::to_string)
                .collect();
        }
    }

    fn fetch_agents(&self) -> Result<Vec<Agent>, AdapterError> {
        let resp = self.req("GET", &format!("{}/agent", self.endpoint), None)?;
        if resp.status != 200 {
            return Err(AdapterError::InvalidResponse(format!(
                "agent list answered HTTP {}",
                resp.status
            )));
        }
        let v = validate_json(&resp)?;
        let arr = json::as_arr(&v).ok_or_else(|| {
            AdapterError::InvalidResponse("`GET /agent` did not answer an array".into())
        })?;
        let mut st = self.state.lock().unwrap();
        // ADR-021: the modes the built-in `build` and `plan` agents back
        st.offered_modes = ["build", "plan"]
            .into_iter()
            .filter(|name| {
                arr.iter().any(|a| {
                    json::get_str(a, "name") == Some(*name)
                        && matches!(json::get_str(a, "mode"), Some("primary" | "all"))
                        && json::get_bool(a, "hidden") != Some(true)
                })
            })
            .filter_map(agent_mode)
            .collect();
        Ok(arr
            .iter()
            .filter_map(|a| Self::map_agent(a, &self.id, &st))
            .collect())
    }

    /// Maps a native session info object onto the protocol `Session`.
    /// `agent_id` is always `build`: OpenCode binds the agent per message,
    /// not per session (probe evidence has no agent field on sessions). A
    /// session in the workspace folder carries the workspace's own path, so
    /// the phone finds its Files and Changes.
    fn map_session(info: &Json, host_id: &str, st: &OcState) -> Option<Session> {
        let id = json::get_str(info, "id")?.to_string();
        let directory = json::get_str(info, "directory");
        let project = match (directory, &st.workspace) {
            (Some(d), Some(ws)) if same_dir(d, &ws.path) => ws.path.clone(),
            _ => directory
                .or_else(|| json::get_str(info, "projectID"))
                .unwrap_or("")
                .to_string(),
        };
        let (created, updated) = match json::get(info, "time") {
            Some(t) => (
                json::get_i64(t, "created").unwrap_or(0),
                json::get_i64(t, "updated").unwrap_or(0),
            ),
            None => (0, 0),
        };
        let status = st
            .statuses
            .get(&id)
            .cloned()
            .unwrap_or_else(|| "completed".to_string());
        let forked_from = st.forks.get(&id).cloned();
        Some(Session {
            id,
            agent_id: "build".to_string(),
            host_id: host_id.to_string(),
            title: json::get_str(info, "title")
                .or_else(|| json::get_str(info, "slug"))
                .unwrap_or("(untitled)")
                .to_string(),
            project,
            status,
            progress: 0,
            created_at: created,
            last_activity_at: updated,
            prompt: None,
            timeline: vec![],
            output: vec![],
            files: vec![],
            // ADR-015: every OpenCode session streams on demand (the first
            // subscriber hydrates it), so the phone opens the conversation
            transcript: true,
            live_pc: false,
            forked_from,
        })
    }

    fn map_agent(v: &Json, host_id: &str, st: &OcState) -> Option<Agent> {
        let healthy = st.healthy;
        let cat = &st.catalog;
        // hidden agents (compaction, summary, title) are OpenCode's own
        if json::get_bool(v, "hidden") == Some(true) {
            return None;
        }
        let name = json::get_str(v, "name")?.to_string();
        let mode = json::get_str(v, "mode").unwrap_or("primary");
        let mut caps = vec![mode.to_string()];
        if let Some(m) = json::get_str(v, "model") {
            caps.push(m.to_string());
        }
        Some(Agent {
            id: name.clone(),
            host_id: host_id.to_string(),
            kind: "opencode".to_string(),
            name,
            state: if healthy { "online" } else { "offline" }.to_string(),
            capabilities: caps,
            accepts_new_session: mode == "primary" && healthy,
            simulated: false,
            // §4.4/§4.6: a mid-turn prompt runs as its own turn afterwards;
            // abort works. ADR-021: End (companion-side), the build/plan
            // agents as modes, the serve's folder and the composer extras
            live: Some(AgentLive {
                prompt_while_busy: "queue",
                interrupt: true,
                end: true,
                permission_modes: st.offered_modes.iter().map(|m| m.to_string()).collect(),
                workspaces: st.workspace.iter().cloned().collect(),
                composer: [
                    ("model", !cat.models.is_empty()),
                    ("images", true),
                    ("commands", !cat.commands.is_empty()),
                    ("context", true),
                    ("rename", true),
                ]
                .into_iter()
                .filter_map(|(f, on)| on.then_some(f))
                .collect(),
                models: cat
                    .models
                    .iter()
                    .map(|(v, l, _)| (v.clone(), l.clone()))
                    .collect(),
            }),
        })
    }

    fn upsert_session(list: &mut Vec<Session>, mut s: Session) {
        match list.iter_mut().find(|e| e.id == s.id) {
            Some(e) => {
                if s.files.is_empty() {
                    s.files = std::mem::take(&mut e.files);
                }
                *e = s
            }
            None => list.push(s),
        }
    }

    /// ADR-017/021 `state` op for a session: its model (chosen, else the one
    /// its last answer ran on, else OpenCode's configured one), the models
    /// and commands on offer, and its context use.
    fn live_state(st: &OcState, sid: &str, status: &'static str) -> LiveState {
        let mode = st
            .picked
            .get(sid)
            .copied()
            .or_else(|| st.last_agent.get(sid).map(String::as_str))
            .and_then(agent_mode)
            .unwrap_or("default");
        LiveState {
            status: if st.ended.contains(sid) {
                "ended"
            } else {
                status
            },
            permission_mode: (!st.offered_modes.is_empty()).then(|| mode.to_string()),
            model: st
                .chosen
                .get(sid)
                .or_else(|| st.last_model.get(sid))
                .or(st.catalog.default_model.as_ref())
                .cloned(),
            models: st
                .catalog
                .models
                .iter()
                .map(|(v, l, _)| (v.clone(), l.clone()))
                .collect(),
            commands: st.catalog.commands.clone(),
            context: st.context.get(sid).copied(),
        }
    }

    /// ADR-021 `session_end`: aborts a running turn, then the session is
    /// ended here (`state` ended, prompts refused). Never `DELETE /session`:
    /// that destroys the history on the PC.
    fn end(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
    ) -> Result<ServerMsg, AdapterError> {
        let running = {
            let st = self.state.lock().unwrap();
            st.statuses.get(session_id).is_some_and(|s| s == "running")
                && !st.ended.contains(session_id)
        };
        if running {
            self.abort(session_id)?;
        }
        {
            let mut st = self.state.lock().unwrap();
            st.ended.insert(session_id.to_string());
            if st.ended.len() > MAP_MAX {
                st.ended.clear();
            }
        }
        if self.hub.streams(session_id) {
            self.hub.end_session(session_id);
        }
        self.emit(vec![result(
            command_id,
            "completed",
            None,
            Some(session_id.to_string()),
        )]);
        Ok(ack(command_id, true, None, Some(session_id), None))
    }

    /// ADR-021 `set_permission_mode`: the phone's `default` / `plan` pick
    /// OpenCode's `build` / `plan` agent for the session's next prompts.
    fn set_mode(&self, command_id: &str, session_id: &str, mode: &str) -> ServerMsg {
        let state = {
            let mut st = self.state.lock().unwrap();
            let agent = mode_agent(mode).filter(|_| st.offered_modes.contains(&mode));
            let Some(agent) = agent else {
                return ack(
                    command_id,
                    false,
                    Some(format!("OpenCode has no agent for the {mode} mode")),
                    Some(session_id),
                    None,
                );
            };
            st.picked.insert(session_id.to_string(), agent);
            trim(&mut st.picked);
            let status = match st.live.get(session_id).map_or("", |l| l.state) {
                "" => "idle",
                s => s,
            };
            Self::live_state(&st, session_id, status)
        };
        self.push(session_id, Op::State(state));
        self.emit(vec![result(
            command_id,
            "completed",
            None,
            Some(session_id.to_string()),
        )]);
        ack(command_id, true, None, Some(session_id), None)
    }

    #[allow(clippy::too_many_arguments)]
    fn create_session(
        self: &Arc<Self>,
        command_id: &str,
        agent_id: &str,
        title: &str,
        prompt: &str,
        project: Option<&str>,
        images: &[PromptImage],
        model: Option<&str>,
    ) -> Result<ServerMsg, AdapterError> {
        // ADR-015 §4.2.3: a phone never picks a folder. OpenCode sessions
        // start where `opencode serve` runs; its id is the only `project`
        // taken (ADR-021), anything else is refused.
        let refusal = {
            let st = self.state.lock().unwrap();
            let ws = st.workspace.as_ref().map(|w| w.id.as_str());
            let model = model.filter(|m| *m != "default");
            if project.is_some_and(|p| !p.is_empty() && Some(p) != ws) {
                Some("OpenCode sessions start in the folder opencode serve runs in".to_string())
            } else {
                model
                    .filter(|m| !st.catalog.models.iter().any(|c| c.0 == *m))
                    .map(|m| format!("OpenCode does not offer the model {m}"))
            }
        };
        if let Some(error) = refusal {
            return Ok(ack(command_id, false, Some(error), None, None));
        }
        let url = format!("{}/session", self.endpoint);
        let body = json::stringify(&json::obj(vec![("title", json::s(title))]));
        let resp = self.req("POST", &url, Some(&body))?;
        if !(200..300).contains(&resp.status) {
            return Err(AdapterError::InvalidResponse(format!(
                "POST /session answered HTTP {}",
                resp.status
            )));
        }
        let v = validate_json(&resp)?;
        let session_id = json::get_str(&v, "id")
            .ok_or_else(|| {
                AdapterError::InvalidResponse("session create response has no id".into())
            })?
            .to_string();
        // Optimistic cache insert so the snapshot sees the session even if
        // the SSE stream is down; `session.created` upserts the same id.
        let state = {
            let mut st = self.state.lock().unwrap();
            match model.filter(|m| *m != "default") {
                Some(m) => st.chosen.insert(session_id.clone(), m.to_string()),
                None => st.chosen.remove(&session_id),
            };
            trim(&mut st.chosen);
            // a task started on the `plan` agent runs as plan
            match agent_mode(agent_id).filter(|m| st.offered_modes.contains(m)) {
                Some(_) if agent_id == "plan" => st.picked.insert(session_id.clone(), "plan"),
                _ => st.picked.remove(&session_id),
            };
            trim(&mut st.picked);
            st.ended.remove(&session_id);
            if let Some(mapped) = Self::map_session(&v, &self.id, &st) {
                Self::upsert_session(&mut st.sessions, mapped);
            }
            Self::live_state(&st, &session_id, "idle")
        };
        // the phone opens this session next: stream it from the first event
        self.hub.push(&session_id, Op::State(state));
        // §4.6: create_session ends with its first turn
        if (!prompt.trim().is_empty() || !images.is_empty())
            && let Err(e) = self.prompt(command_id, &session_id, prompt, images)
        {
            // The create DID happen — surface it as a domain refusal with
            // the session id, not as a transport error hiding success.
            return Ok(ack(
                command_id,
                false,
                Some(format!("session created, prompt not accepted: {e}")),
                Some(&session_id),
                None,
            ));
        }
        Ok(ack(command_id, true, None, Some(&session_id), None))
    }

    /// A prompt, tracked until its turn ends (G1): a known slash command
    /// (`/name args`) goes to `POST /session/:id/command`, anything else to
    /// `prompt_async`, with the session's chosen model and the images.
    fn prompt(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
        text: &str,
        images: &[PromptImage],
    ) -> Result<(), AdapterError> {
        let (choice, command) = {
            let st = self.state.lock().unwrap();
            (
                Choice {
                    model: st.chosen.get(session_id).cloned(),
                    agent: st.picked.get(session_id).copied(),
                },
                slash(text, &st.catalog.commands).map(|(n, a)| (n.to_string(), a.to_string())),
            )
        };
        match command {
            Some((name, args)) => {
                self.track_prompt(session_id, command_id);
                self.run_command(command_id, session_id, &name, &args, images, choice)
            }
            None => {
                self.prompt_accept(session_id, text, images, &choice)?;
                self.track_prompt(session_id, command_id);
            }
        }
        Ok(())
    }

    /// ADR-021: `POST /session/:id/command {command, arguments, model?,
    /// parts?}` answers only when the command's turn ends, so it runs on a
    /// thread; the turn's outcome closes the prompt from the event stream.
    /// A refusal before the turn started fails the prompt.
    fn run_command(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
        name: &str,
        args: &str,
        images: &[PromptImage],
        choice: Choice,
    ) {
        let mut fields = vec![("command", json::s(name)), ("arguments", json::s(args))];
        fields.extend(choice.agent.map(|a| ("agent", json::s(a))));
        fields.extend(choice.model.map(|m| ("model", json::s(&m))));
        if !images.is_empty() {
            fields.push(("parts", Json::Arr(image_parts(images))));
        }
        let body = json::stringify(&json::obj(fields));
        let url = format!("{}/session/{session_id}/command", self.endpoint);
        let (inner, cmd, sid, name) = (
            self.clone(),
            command_id.to_string(),
            session_id.to_string(),
            name.to_string(),
        );
        let _ = std::thread::Builder::new()
            .name("opencode-command".into())
            .spawn(move || {
                let outcome = http_request_waiting(
                    "POST",
                    &url,
                    inner.auth.as_deref(),
                    Some(&body),
                    COMMAND_WAIT,
                );
                let error = match outcome {
                    Ok(r) if (200..300).contains(&r.status) => return,
                    Ok(r) => format!("OpenCode refused /{name}: HTTP {}", r.status),
                    Err(e) => format!("/{name} did not reach OpenCode: {e}"),
                };
                // still waiting for its turn to start = it never ran
                let never_ran = {
                    let mut st = inner.state.lock().unwrap();
                    let at = st.pending.iter().position(|p| p.1 == cmd && !p.2);
                    at.map(|i| st.pending.remove(i)).is_some()
                };
                if never_ran {
                    inner.emit(vec![result(&cmd, "failed", Some(clip(error)), Some(sid))]);
                }
            });
    }

    /// ADR-021: a native fork (`POST /session/:id/fork`, all messages), then
    /// the prompt in the fork; the ack names the fork. The original session
    /// is never written.
    fn fork(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
        text: &str,
        images: &[PromptImage],
    ) -> Result<ServerMsg, AdapterError> {
        let resp = self.req(
            "POST",
            &format!("{}/session/{session_id}/fork", self.endpoint),
            Some("{}"),
        )?;
        if !(200..300).contains(&resp.status) {
            return Err(AdapterError::InvalidResponse(format!(
                "fork answered HTTP {}",
                resp.status
            )));
        }
        let v = validate_json(&resp)?;
        let fork = json::get_str(&v, "id")
            .ok_or_else(|| AdapterError::InvalidResponse("fork response has no id".into()))?
            .to_string();
        let (state, created) = {
            let mut st = self.state.lock().unwrap();
            let from = st
                .sessions
                .iter()
                .find(|s| s.id == session_id)
                .map_or_else(|| "a session".to_string(), |s| s.title.clone());
            st.forks.insert(fork.clone(), from);
            trim(&mut st.forks);
            if let Some(m) = st.chosen.get(session_id).cloned() {
                st.chosen.insert(fork.clone(), m);
            }
            let mapped = Self::map_session(&v, &self.id, &st);
            if let Some(s) = mapped.clone() {
                Self::upsert_session(&mut st.sessions, s);
            }
            (Self::live_state(&st, &fork, "idle"), mapped)
        };
        self.emit(created.into_iter().map(EventKind::SessionCreated).collect());
        self.hub.push(&fork, Op::State(state));
        if let Err(e) = self.prompt(command_id, &fork, text, images) {
            return Ok(ack(
                command_id,
                false,
                Some(format!("forked, prompt not accepted: {e}")),
                Some(&fork),
                None,
            ));
        }
        Ok(ack(command_id, true, None, Some(&fork), None))
    }

    /// ADR-021 `set_model`: OpenCode takes the model with each prompt, so
    /// the choice is kept here and sent from the next prompt on. Only a
    /// model `GET /config/providers` listed is taken.
    fn set_model(&self, command_id: &str, session_id: &str, model: &str) -> ServerMsg {
        let state = {
            let mut st = self.state.lock().unwrap();
            if !st.catalog.models.iter().any(|m| m.0 == model) {
                return ack(
                    command_id,
                    false,
                    Some(format!("OpenCode does not offer the model {model}")),
                    Some(session_id),
                    None,
                );
            }
            st.chosen.insert(session_id.to_string(), model.to_string());
            trim(&mut st.chosen);
            let status = st.live.get(session_id).map_or("idle", |l| l.state);
            Self::live_state(
                &st,
                session_id,
                if status.is_empty() { "idle" } else { status },
            )
        };
        self.push(session_id, Op::State(state));
        self.emit(vec![result(
            command_id,
            "completed",
            None,
            Some(session_id.to_string()),
        )]);
        ack(command_id, true, None, Some(session_id), None)
    }

    /// ADR-021 `rename_session`: `PATCH /session/:id {title}` answers the
    /// renamed session.
    fn rename(
        &self,
        command_id: &str,
        session_id: &str,
        title: &str,
    ) -> Result<ServerMsg, AdapterError> {
        let body = json::stringify(&json::obj(vec![("title", json::s(title))]));
        let resp = self.req(
            "PATCH",
            &format!("{}/session/{session_id}", self.endpoint),
            Some(&body),
        )?;
        if !(200..300).contains(&resp.status) {
            return Err(AdapterError::InvalidResponse(format!(
                "rename answered HTTP {}",
                resp.status
            )));
        }
        let v = validate_json(&resp)?;
        let mut out = Vec::new();
        {
            let mut st = self.state.lock().unwrap();
            if let Some(s) = Self::map_session(&v, &self.id, &st) {
                Self::upsert_session(&mut st.sessions, s);
            }
            out.extend(
                st.sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .cloned()
                    .map(EventKind::SessionUpdated),
            );
        }
        out.push(result(
            command_id,
            "completed",
            None,
            Some(session_id.to_string()),
        ));
        self.emit(out);
        Ok(ack(command_id, true, None, Some(session_id), None))
    }

    /// `POST /session/:id/prompt_async` — acceptance is 204-with-empty-body
    /// or 200+JSON; anything else (the HTML-200 fallback above all) is a
    /// validation failure, never success (G4.2). ADR-021: the images follow
    /// the text as `file` parts, and a chosen model rides along.
    fn prompt_accept(
        &self,
        session_id: &str,
        text: &str,
        images: &[PromptImage],
        choice: &Choice,
    ) -> Result<(), AdapterError> {
        let mut parts = vec![json::obj(vec![
            ("type", json::s("text")),
            ("text", json::s(text)),
        ])];
        parts.extend(image_parts(images));
        let mut fields = vec![("parts", Json::Arr(parts))];
        fields.extend(choice.agent.map(|a| ("agent", json::s(a))));
        if let Some((provider, id)) = choice.model.as_deref().and_then(|m| m.split_once('/')) {
            fields.push((
                "model",
                json::obj(vec![
                    ("providerID", json::s(provider)),
                    ("modelID", json::s(id)),
                ]),
            ));
        }
        let body = json::stringify(&json::obj(fields));
        let resp = self.req(
            "POST",
            &format!("{}/session/{session_id}/prompt_async", self.endpoint),
            Some(&body),
        )?;
        if resp.status == 204 && resp.body.trim().is_empty() {
            return Ok(());
        }
        if resp.status == 200 && resp.content_type.contains("application/json") {
            return Ok(());
        }
        let ct = if resp.content_type.is_empty() {
            "(no content-type)".to_string()
        } else {
            resp.content_type.clone()
        };
        Err(AdapterError::InvalidResponse(format!(
            "prompt_async answered HTTP {} with {ct} (acceptance must be 204 or JSON-200)",
            resp.status
        )))
    }

    /// G1: a prompt (or a create's first prompt) is tracked until its turn
    /// ends.
    fn track_prompt(&self, session_id: &str, command_id: &str) {
        let mut st = self.state.lock().unwrap();
        st.pending
            .push((session_id.to_string(), command_id.to_string(), false));
        if st.pending.len() > MAP_MAX {
            st.pending.remove(0);
        }
    }

    /// §4.6: acked once OpenCode accepts the abort, `accepted` then, and
    /// `completed` when the running turn ends (at once when none ran);
    /// `unknown` when no turn end follows within 30 s.
    fn interrupt(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
    ) -> Result<ServerMsg, AdapterError> {
        // registered BEFORE the call: the abort's session.error can arrive
        // on the SSE thread before the HTTP answer does
        let running = {
            let mut st = self.state.lock().unwrap();
            let running = st.statuses.get(session_id).is_some_and(|s| s == "running");
            if running {
                st.live.entry(session_id.to_string()).or_default().interrupt =
                    Some(command_id.to_string());
            }
            running
        };
        let outcome = self.abort(session_id);
        // still ours = no turn end yet (taking it back on failure, too)
        let waiting = {
            let mut st = self.state.lock().unwrap();
            let live = st.live.entry(session_id.to_string()).or_default();
            let ours = live.interrupt.as_deref() == Some(command_id);
            if ours && !matches!(outcome, Ok(true)) {
                live.interrupt = None;
            }
            ours
        };
        match outcome? {
            false => Ok(ack(
                command_id,
                false,
                Some("native abort reported it did nothing (turn already finished?)".into()),
                Some(session_id),
                None,
            )),
            true => {
                let sid = Some(session_id.to_string());
                if !running || waiting {
                    self.emit(vec![EventKind::CommandProgress {
                        command_id: command_id.to_string(),
                        state: "accepted".into(),
                        session_id: sid.clone(),
                    }]);
                }
                if !running {
                    self.emit(vec![result(command_id, "completed", None, sid)]);
                } else if waiting {
                    let (inner, cmd, ses) =
                        (self.clone(), command_id.to_string(), session_id.to_string());
                    std::thread::spawn(move || {
                        std::thread::sleep(INTERRUPT_WAIT);
                        let late = {
                            let mut st = inner.state.lock().unwrap();
                            let live = st.live.entry(ses.clone()).or_default();
                            let late = live.interrupt.as_deref() == Some(cmd.as_str());
                            if late {
                                live.interrupt = None;
                            }
                            late
                        };
                        if late {
                            inner.emit(vec![result(&cmd, "unknown", None, Some(ses))]);
                        }
                    });
                }
                Ok(ack(command_id, true, None, Some(session_id), None))
            }
        }
    }

    fn abort(&self, session_id: &str) -> Result<bool, AdapterError> {
        // OpenAPI: abort has NO requestBody — sending none is contract-
        // faithful. (The Phase-3 live run's apparent ">15 s wedge" was the
        // old read-to-end client: the server delivered a complete 200 `true`
        // ~3.4 s post-turn but kept the socket open; client.rs now frames
        // by Content-Length.)
        let resp = self.req(
            "POST",
            &format!("{}/session/{session_id}/abort", self.endpoint),
            None,
        )?;
        if !(200..300).contains(&resp.status) {
            return Err(AdapterError::InvalidResponse(format!(
                "abort answered HTTP {}",
                resp.status
            )));
        }
        // Shape check, header not required: an HTML body can't parse as a
        // boolean, which is the same guarantee G4.2 buys with the header.
        match json::parse(resp.body.trim()) {
            Ok(Json::Bool(b)) => Ok(b),
            _ => Err(AdapterError::InvalidResponse(format!(
                "abort answered HTTP {} with a non-boolean body",
                resp.status
            ))),
        }
    }

    /// Writes the option the engine selected (§4.4): a permission goes to
    /// `POST /permission/{id}/reply {reply, message?}`, a question to
    /// `POST /question/{id}/reply {answers}` or `/reject`.
    fn permission_decision(
        &self,
        command_id: &str,
        permission_id: &str,
        option: &str,
        message: Option<&str>,
        answers: &[(String, Vec<String>)],
    ) -> Result<ServerMsg, AdapterError> {
        let question = {
            let st = self.state.lock().unwrap();
            st.asked
                .get(permission_id)
                .and_then(|p| p.detail.as_ref())
                .filter(|d| json::get_str(d, "kind") == Some("question"))
                .and_then(|d| json::get(d, "questions").and_then(json::as_arr))
                .map(<[Json]>::len)
        };
        let (url, body, rejects) = match (question, option) {
            (Some(n), "answer" | "once" | "always") => {
                // one array of labels per question, in order; ids are indexes
                let picked = (0..n)
                    .map(|i| {
                        let labels = answers
                            .iter()
                            .find(|(id, _)| *id == i.to_string())
                            .map_or(&[][..], |(_, a)| a.as_slice());
                        Json::Arr(labels.iter().map(|l| json::s(l)).collect())
                    })
                    .collect();
                let body = json::obj(vec![("answers", Json::Arr(picked))]);
                (format!("question/{permission_id}/reply"), Some(body), false)
            }
            (Some(_), "skip" | "reject") => {
                (format!("question/{permission_id}/reject"), None, true)
            }
            (None, "once" | "always" | "reject") => {
                let mut fields = vec![("reply", json::s(option))];
                if let (Some(m), "reject") = (message, option) {
                    fields.push(("message", json::s(m)));
                }
                let body = json::obj(fields);
                (
                    format!("permission/{permission_id}/reply"),
                    Some(body),
                    option == "reject",
                )
            }
            _ => {
                return Err(AdapterError::Unsupported(format!(
                    "unknown option '{option}'"
                )));
            }
        };
        let body = body.map(|b| json::stringify(&b));
        let resp = self.req("POST", &format!("{}/{url}", self.endpoint), body.as_deref())?;
        if resp.status == 404 {
            // Real path, real answer: stale/unknown request. A refusal on
            // the outcome side is safe even before body validation.
            return Ok(ack(
                command_id,
                false,
                Some("permission request not found (stale or already answered)".into()),
                None,
                Some(permission_id),
            ));
        }
        if !(200..300).contains(&resp.status) {
            return Err(AdapterError::InvalidResponse(format!(
                "permission reply answered HTTP {}",
                resp.status
            )));
        }
        let v = validate_json(&resp)?;
        match v {
            Json::Bool(true) => {
                let mut st = self.state.lock().unwrap();
                // our own reply: the `replied` event finds nothing to resolve
                if let Some(call) = st
                    .asked
                    .remove(permission_id)
                    .and_then(|p| p.tool_item_id)
                    .filter(|_| rejects)
                {
                    st.rejected.insert(call);
                }
                Ok(ack(command_id, true, None, None, Some(permission_id)))
            }
            Json::Bool(false) => Ok(ack(
                command_id,
                false,
                Some("native reply was not accepted".into()),
                None,
                Some(permission_id),
            )),
            _ => Err(AdapterError::InvalidResponse(
                "permission reply answered with a non-boolean body".into(),
            )),
        }
    }
}

/* ------------------------------------------------------------------ */
/* MCP status and sign-in (§4.7)                                       */
/* ------------------------------------------------------------------ */

impl Inner {
    /// `mcp_auth`. Desktop: `POST /mcp/{name}/auth/authenticate` on a thread
    /// (OpenCode opens the desktop browser and answers once signed in).
    /// Phone: `POST /mcp/{name}/auth` returns the URL the phone opens and
    /// the loopback redirect whose dead page the user pastes back.
    fn mcp_auth(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
        server: &str,
        flow: &str,
    ) -> Result<ServerMsg, AdapterError> {
        let name = urlencode(server);
        // each desktop sign-in holds a thread for up to AUTH_TTL_MS, and a
        // newer one replaces (not ends) the older request: bound them
        if flow == "desktop" && DESKTOP_AUTHS.load(Ordering::SeqCst) >= MAX_DESKTOP_AUTHS {
            return Ok(ack(
                command_id,
                false,
                Some("too many sign-ins are open on the PC: finish or close one first".into()),
                Some(session_id),
                None,
            ));
        }
        let auth = if flow == "desktop" {
            Auth {
                command_id: command_id.to_string(),
                state: "awaiting-browser",
                url: None,
                prefix: None,
                callback: None,
            }
        } else {
            let resp = self.req("POST", &format!("{}/mcp/{name}/auth", self.endpoint), None)?;
            let v = validate_json(&resp)?;
            if resp.status != 200 {
                let e = json::get_str(&v, "error")
                    .or_else(|| json::get_str(&v, "message"))
                    .map_or_else(
                        || format!("sign-in start answered HTTP {}", resp.status),
                        str::to_string,
                    );
                return Ok(ack(
                    command_id,
                    false,
                    Some(clip(e)),
                    Some(session_id),
                    None,
                ));
            }
            let (Some(url), Some(state)) = (
                json::get_str(&v, "authorizationUrl"),
                json::get_str(&v, "oauthState"),
            ) else {
                return Err(AdapterError::InvalidResponse(
                    "sign-in start has no authorizationUrl/oauthState".into(),
                ));
            };
            let (prefix, port, path) = loopback_redirect(url).ok_or_else(|| {
                AdapterError::InvalidResponse("sign-in URL has no loopback redirect_uri".into())
            })?;
            Auth {
                command_id: command_id.to_string(),
                state: "awaiting-callback",
                url: Some(url.to_string()),
                prefix: Some(prefix),
                callback: Some((port, path, state.to_string())),
            }
        };
        let replaced = {
            let mut st = self.state.lock().unwrap();
            st.auths.insert(session_id, server, auth, now_ms())
        };
        let sid = Some(session_id.to_string());
        let mut events = Vec::new();
        if let Some(old) = replaced {
            events.push(result(&old.command_id, "cancelled", None, sid.clone()));
        }
        events.push(EventKind::CommandProgress {
            command_id: command_id.to_string(),
            state: "running".into(),
            session_id: sid,
        });
        self.emit(events);
        self.push_mcp();
        if flow == "desktop" {
            let (inner, cmd, ses, srv) = (
                self.clone(),
                command_id.to_string(),
                session_id.to_string(),
                server.to_string(),
            );
            DESKTOP_AUTHS.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                let url = format!(
                    "{}/mcp/{}/auth/authenticate",
                    inner.endpoint,
                    urlencode(&srv)
                );
                let wait = Duration::from_millis(AUTH_TTL_MS as u64);
                let outcome = http_request_waiting("POST", &url, inner.auth.as_deref(), None, wait)
                    .map_err(clip)
                    .and_then(|r| mcp_outcome(&r));
                DESKTOP_AUTHS.fetch_sub(1, Ordering::SeqCst);
                if inner.end_auth(&ses, &srv, &cmd) {
                    inner.emit(vec![done(&cmd, outcome, &ses)]);
                }
                let _ = inner.poll_mcp();
            });
        }
        Ok(ack(command_id, true, None, Some(session_id), None))
    }

    /// Takes the sign-in when `command_id` still owns it (not replaced,
    /// expired or finished): its command is then ours to close.
    fn end_auth(&self, session_id: &str, server: &str, command_id: &str) -> bool {
        let mut st = self.state.lock().unwrap();
        let ours = st
            .auths
            .get(session_id, server, now_ms())
            .is_some_and(|a| a.command_id == command_id);
        if ours {
            st.auths.take(session_id, server, now_ms());
        }
        ours
    }

    /// `mcp_auth_callback` (T5): the pasted dead-page URL must match the
    /// sign-in's loopback port and path and carry OpenCode's `oauthState`;
    /// single use. Then `POST /mcp/{name}/auth/callback {code}` on a thread.
    fn mcp_auth_callback(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
        server: &str,
        callback_url: &str,
    ) -> Result<ServerMsg, AdapterError> {
        let auth = self
            .state
            .lock()
            .unwrap()
            .auths
            .take(session_id, server, now_ms());
        let Some(auth) = auth else {
            return Ok(ServerMsg::ack_err(
                command_id,
                "no sign-in is pending for this server",
            ));
        };
        let checked = match &auth.callback {
            Some((port, path, state)) => check_callback_url(callback_url, *port, path, state),
            None => Err("this sign-in finishes in the desktop browser"),
        };
        let sid = Some(session_id.to_string());
        let code = match checked {
            Ok(params) => params.code,
            Err(e) => {
                self.emit(vec![result(
                    &auth.command_id,
                    "failed",
                    Some(e.into()),
                    sid,
                )]);
                self.push_mcp();
                return Ok(ServerMsg::ack_err(command_id, e));
            }
        };
        self.push_mcp();
        let (inner, cmd, ses, srv) = (
            self.clone(),
            command_id.to_string(),
            session_id.to_string(),
            server.to_string(),
        );
        std::thread::spawn(move || {
            let body = json::stringify(&json::obj(vec![("code", json::s(&code))]));
            let url = format!("{}/mcp/{}/auth/callback", inner.endpoint, urlencode(&srv));
            let outcome = inner
                .req("POST", &url, Some(&body))
                .map_err(|e| clip(e.to_string()))
                .and_then(|r| mcp_outcome(&r));
            inner.emit(vec![
                done(&auth.command_id, outcome.clone(), &ses),
                done(&cmd, outcome, &ses),
            ]);
            let _ = inner.poll_mcp();
        });
        Ok(ack(command_id, true, None, Some(session_id), None))
    }

    /// `mcp_reconnect`: `POST /mcp/{name}/connect`, then the server's status
    /// in a fresh `GET /mcp` decides the outcome.
    fn mcp_reconnect(
        self: &Arc<Self>,
        command_id: &str,
        session_id: &str,
        server: &str,
    ) -> ServerMsg {
        let (inner, cmd, ses, srv) = (
            self.clone(),
            command_id.to_string(),
            session_id.to_string(),
            server.to_string(),
        );
        std::thread::spawn(move || {
            let url = format!("{}/mcp/{}/connect", inner.endpoint, urlencode(&srv));
            let outcome = match inner.req("POST", &url, None) {
                Err(e) => Err(clip(e.to_string())),
                Ok(_) => match inner.poll_mcp() {
                    Err(e) => Err(clip(e.to_string())),
                    Ok(list) => match list.iter().find(|s| s.name == srv) {
                        Some(s) if s.status == "connected" => Ok(()),
                        Some(s) => Err(s
                            .error
                            .clone()
                            .unwrap_or_else(|| format!("{srv} is {}", s.status))),
                        None => Err(format!("{srv} is not configured")),
                    },
                },
            };
            inner.emit(vec![done(&cmd, outcome, &ses)]);
        });
        ack(command_id, true, None, Some(session_id), None)
    }

    /// `GET /mcp` → the list every watched session shows. Connected servers
    /// lose their failed-browser URL.
    fn poll_mcp(&self) -> Result<Vec<McpServer>, AdapterError> {
        let resp = self.req("GET", &format!("{}/mcp", self.endpoint), None)?;
        if resp.status != 200 {
            return Err(AdapterError::InvalidResponse(format!(
                "mcp status answered HTTP {}",
                resp.status
            )));
        }
        let list = mcp_servers(&validate_json(&resp)?);
        {
            let mut st = self.state.lock().unwrap();
            for s in list.iter().filter(|s| s.status == "connected") {
                st.open_failed.remove(&s.name);
            }
            st.mcp = list.clone();
        }
        self.push_mcp();
        Ok(list)
    }

    /// Pushes each watched session its MCP view: the polled list plus the
    /// sign-in that session started.
    fn push_mcp(&self) {
        let now = now_ms();
        let views: Vec<(String, Vec<McpServer>)> = {
            let st = self.state.lock().unwrap();
            st.watched
                .iter()
                .map(|sid| {
                    let view = st
                        .mcp
                        .iter()
                        .map(|s| {
                            let auth = match st.auths.get(sid, &s.name, now) {
                                Some(a) => Some(McpAuthState {
                                    state: a.state,
                                    url: a.url.clone(),
                                    callback_prefix: a.prefix.clone(),
                                    error: None,
                                }),
                                None => st.open_failed.get(&s.name).map(|url| McpAuthState {
                                    state: "awaiting-browser",
                                    url: Some(url.clone()),
                                    ..Default::default()
                                }),
                            };
                            McpServer { auth, ..s.clone() }
                        })
                        .collect();
                    (sid.clone(), view)
                })
                .collect()
        };
        for (sid, view) in views {
            self.hub.push(&sid, Op::Mcp(view));
        }
    }

    /// While a phone watches: polls `GET /mcp` every 30 s or when poked,
    /// expires sign-ins after 10 min, exits when nobody watches.
    fn poll_loop(self: Arc<Self>) {
        let mut last: Option<Instant> = None;
        loop {
            let poked = self.poke.swap(false, Ordering::SeqCst);
            if poked || last.is_none_or(|t| t.elapsed() >= MCP_POLL) {
                let _ = self.poll_mcp();
                last = Some(Instant::now());
            }
            let expired = self.state.lock().unwrap().auths.sweep(now_ms());
            if !expired.is_empty() {
                self.emit(
                    expired
                        .iter()
                        .map(|a| result(&a.command_id, "expired", None, None))
                        .collect(),
                );
                self.push_mcp();
            }
            {
                let mut st = self.state.lock().unwrap();
                let hub = &self.hub;
                st.watched.retain(|s| hub.is_subscribed(s));
                if st.watched.is_empty() || self.stop.load(Ordering::SeqCst) {
                    st.poller = false;
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// `GET /mcp` (`{name: MCPStatus}`) → §3.2 servers, sorted by name. Only
/// `needs_auth` offers sign-in; `needs_client_registration` needs the
/// desktop (ADR §3.6 status values).
fn mcp_servers(v: &Json) -> Vec<McpServer> {
    let Json::Obj(fields) = v else {
        return Vec::new();
    };
    let mut out: Vec<McpServer> = fields
        .iter()
        .map(|(name, s)| {
            let (status, error, flows) = match json::get_str(s, "status") {
                Some("connected") => ("connected", None, vec![]),
                Some("disabled") => ("disabled", None, vec![]),
                Some("needs_auth") => ("needs-auth", None, vec!["desktop", "phone-callback"]),
                Some("needs_client_registration") => (
                    "failed",
                    Some("configure the client on the desktop".to_string()),
                    vec![],
                ),
                Some("failed") => ("failed", json::get_str(s, "error").map(redact), vec![]),
                _ => ("pending", None, vec![]),
            };
            McpServer {
                name: name.clone(),
                status,
                error,
                auth_flows: flows,
                auth: None,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// An `MCPStatus` answer (authenticate, callback) → the command outcome.
fn mcp_outcome(resp: &HttpResponse) -> Result<(), String> {
    let v = validate_json(resp).map_err(|e| clip(e.to_string()))?;
    match json::get_str(&v, "status") {
        Some("connected") => Ok(()),
        _ => Err(clip(
            json::get_str(&v, "error")
                .or_else(|| json::get_str(&v, "message"))
                .map_or_else(|| format!("sign-in answered HTTP {}", resp.status), redact),
        )),
    }
}

/// The `redirect_uri` of OpenCode's sign-in URL when it is a loopback
/// `http://127.0.0.1:<port>/<path>` (or localhost): (prefix, port, path).
fn loopback_redirect(url: &str) -> Option<(String, u16, String)> {
    let query = url.split_once('?')?.1;
    let raw = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("redirect_uri="))?;
    let uri = form_decode(raw).ok()?;
    let rest = uri
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| uri.strip_prefix("http://localhost:"))?;
    let (port, path) = rest.split_at(rest.find('/')?);
    if path.contains(['?', '#']) {
        return None;
    }
    Some((uri.clone(), port.parse().ok()?, path.to_string()))
}

fn result(
    command_id: &str,
    status: &str,
    error: Option<String>,
    session_id: Option<String>,
) -> EventKind {
    EventKind::CommandResult {
        agent_config_changed: Vec::new(),
        command_id: command_id.to_string(),
        status: status.to_string(),
        error,
        session_id,
        permission_id: None,
    }
}

fn done(command_id: &str, outcome: Result<(), String>, session_id: &str) -> EventKind {
    let sid = Some(session_id.to_string());
    match outcome {
        Ok(()) => result(command_id, "completed", None, sid),
        Err(e) => result(command_id, "failed", Some(e), sid),
    }
}

/* ------------------------------------------------------------------ */
/* SSE stream                                                          */
/* ------------------------------------------------------------------ */

/// Maximum length of a single SSE line (audit §6 bounded output). Oversize
/// lines are dropped without accumulating unbounded memory.
pub const MAX_SSE_LINE_BYTES: usize = 1024 * 1024; // 1 MiB

/// Maximum length of an accumulated SSE event payload (audit §6 bounded output).
pub const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024; // 1 MiB

enum BodyMode {
    Raw,
    Chunked { remaining: usize },
}

enum SseLine {
    Line(String),
    Oversize,
}

/// Incremental response body: raw-until-close or chunked (HTTP/1.1 servers
/// may pick either for a stream; the reader handles both so framing never
/// leaks into the SSE parser).
struct SseBody {
    reader: BufReader<TcpStream>,
    mode: BodyMode,
    buf: Vec<u8>,
    eof: bool,
    oversize_discarding: bool,
}

impl SseBody {
    /// Reads more decoded bytes. `Ok(true)` = bytes added, `Ok(false)` = EOF.
    fn fill(&mut self) -> std::io::Result<bool> {
        if self.eof {
            return Ok(false);
        }
        match &mut self.mode {
            BodyMode::Raw => {
                let mut tmp = [0u8; 4096];
                let n = self.reader.read(&mut tmp)?;
                if n == 0 {
                    self.eof = true;
                    return Ok(false);
                }
                if self.oversize_discarding {
                    if let Some(pos) = tmp[..n].iter().position(|b| *b == b'\n') {
                        self.oversize_discarding = false;
                        self.buf.extend_from_slice(&tmp[pos + 1..n]);
                    }
                } else {
                    self.buf.extend_from_slice(&tmp[..n]);
                }
                Ok(true)
            }
            BodyMode::Chunked { remaining } => {
                if *remaining == 0 {
                    // size line (blocks until complete — bounded to 1024 bytes)
                    let mut line = Vec::new();
                    let mut b = [0u8; 1];
                    while line.len() < 1024 {
                        let n = self.reader.read(&mut b)?;
                        if n == 0 {
                            self.eof = true;
                            return Ok(false);
                        }
                        line.push(b[0]);
                        if b[0] == b'\n' {
                            break;
                        }
                    }
                    if !line.ends_with(b"\n") {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "chunk size line exceeded 1024 bytes",
                        ));
                    }
                    let text = String::from_utf8_lossy(&line);
                    let size_hex = text
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .trim_end_matches('\r');
                    let size = usize::from_str_radix(size_hex, 16).map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("bad chunk size: {size_hex}"),
                        )
                    })?;
                    if size == 0 {
                        // drain optional trailers up to the blank line (bounded)
                        let mut trailer_bytes = 0usize;
                        loop {
                            let mut t = Vec::new();
                            let mut tb = [0u8; 1];
                            while t.len() < 1024 && trailer_bytes < 8192 {
                                let n = self.reader.read(&mut tb)?;
                                if n == 0 {
                                    break;
                                }
                                t.push(tb[0]);
                                trailer_bytes += 1;
                                if tb[0] == b'\n' {
                                    break;
                                }
                            }
                            if t.is_empty() || t == b"\r\n" || t == b"\n" || trailer_bytes >= 8192 {
                                break;
                            }
                        }
                        self.eof = true;
                        return Ok(false);
                    }
                    *remaining = size;
                }
                let mut tmp = vec![0u8; (*remaining).min(4096)];
                let n = self.reader.read(&mut tmp)?;
                if n == 0 {
                    self.eof = true;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "chunked stream ended mid-chunk",
                    ));
                }
                *remaining -= n;
                if self.oversize_discarding {
                    if let Some(pos) = tmp[..n].iter().position(|b| *b == b'\n') {
                        self.oversize_discarding = false;
                        self.buf.extend_from_slice(&tmp[pos + 1..n]);
                    }
                } else {
                    self.buf.extend_from_slice(&tmp[..n]);
                }
                if *remaining == 0 {
                    // consume the chunk's CRLF terminator
                    let mut term = [0u8; 2];
                    self.reader.read_exact(&mut term)?;
                    if &term != b"\r\n" {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "missing chunk terminator",
                        ));
                    }
                }
                Ok(true)
            }
        }
    }

    /// Next SSE line (`\n`- or `\r\n`-terminated), `None` at EOF.
    /// Oversize lines (> MAX_SSE_LINE_BYTES) return `Ok(Some(SseLine::Oversize))`
    /// and discard bytes until the next newline without allocating unbounded memory.
    fn next_line(&mut self) -> std::io::Result<Option<SseLine>> {
        loop {
            if let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                if line.len() > MAX_SSE_LINE_BYTES {
                    return Ok(Some(SseLine::Oversize));
                }
                let mut s = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                if s.ends_with('\r') {
                    s.pop();
                }
                return Ok(Some(SseLine::Line(s)));
            }
            if self.buf.len() > MAX_SSE_LINE_BYTES {
                self.buf.clear();
                self.oversize_discarding = true;
                return Ok(Some(SseLine::Oversize));
            }
            if !self.fill()? {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                if self.buf.len() > MAX_SSE_LINE_BYTES {
                    self.buf.clear();
                    return Ok(Some(SseLine::Oversize));
                }
                let s = String::from_utf8_lossy(&self.buf).into_owned();
                self.buf.clear();
                return Ok(Some(SseLine::Line(s)));
            }
        }
    }
}

/// SSE reconnect delay (audit P5): the ceiling doubles 500 → 1000 → 1250 ms
/// and the delay is uniform in [250, ceiling], so companions that lost the
/// same `opencode serve` spread their retries instead of reconnecting in
/// lockstep. Bounds are the pre-P5 ones: never below 250, never above 1250.
fn reconnect_delay_ms(attempt: u32, rand: u64) -> u64 {
    let ceiling = (250u64 << (attempt.min(3) + 1)).min(1250);
    250 + rand % (ceiling - 249)
}

/// OS CSPRNG bytes, xor'd with the clock so a CSPRNG failure still jitters.
fn jitter_seed() -> u64 {
    let mut b = [0u8; 8];
    let _ = crate::ffi::csprng_fill(&mut b);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() as u64);
    u64::from_le_bytes(b) ^ nanos
}

impl Inner {
    fn sse_loop(inner: Arc<Inner>) {
        let mut attempt: u32 = 0;
        while !inner.stop.load(Ordering::SeqCst) {
            let started = std::time::Instant::now();
            match inner.sse_session() {
                Ok(()) => {
                    let mut st = inner.state.lock().unwrap();
                    st.healthy = false;
                    st.detail = Some("event stream: connection closed by peer".into());
                }
                Err(e) => {
                    let mut st = inner.state.lock().unwrap();
                    st.healthy = false;
                    st.detail = Some(format!("event stream: {e}"));
                }
            }
            // a stream that stayed up 30 s was healthy: the next drop must
            // not inherit the old penalty (audit P5)
            if started.elapsed() >= Duration::from_secs(30) {
                attempt = 0;
            }
            let step = reconnect_delay_ms(attempt, jitter_seed());
            attempt = attempt.saturating_add(1);
            let mut waited: u64 = 0;
            while waited < step && !inner.stop.load(Ordering::SeqCst) {
                let slice = (step - waited).min(100);
                std::thread::sleep(Duration::from_millis(slice));
                waited += slice;
            }
        }
    }

    /// One connection: connect → resync state → pump until EOF/error.
    fn sse_session(self: &Arc<Self>) -> Result<(), AdapterError> {
        let mut body = self.sse_connect()?;
        // Every (re)connect re-lists state: events lost in the gap are
        // replaced by the fresh snapshot (restart/recovery criterion), and
        // watched transcripts are rebuilt (deltas are not replayed).
        self.refresh()?;
        let watched: Vec<String> = self.state.lock().unwrap().watched.iter().cloned().collect();
        for sid in watched {
            self.spawn_hydrate(sid);
        }
        let mut data = String::new();
        let mut event_oversize = false;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            let line = match body.next_line() {
                Ok(Some(SseLine::Line(l))) => l,
                Ok(Some(SseLine::Oversize)) => {
                    data.clear();
                    event_oversize = true;
                    let mut st = self.state.lock().unwrap();
                    st.detail =
                        Some("event stream: dropped oversize event line (> 1 MiB cap)".into());
                    continue;
                }
                Ok(None) => return Ok(()),
                Err(e) => {
                    return Err(AdapterError::RuntimeUnavailable(format!(
                        "event stream read: {e}"
                    )));
                }
            };
            if line.is_empty() {
                if event_oversize {
                    event_oversize = false;
                    data.clear();
                } else if !data.is_empty() {
                    self.on_sse_data(&data);
                    data.clear();
                }
            } else if !event_oversize
                && let Some(d) = line.strip_prefix("data:").map(str::trim_start)
            {
                if data.len() + d.len() + 1 > MAX_SSE_EVENT_BYTES {
                    data.clear();
                    event_oversize = true;
                    let mut st = self.state.lock().unwrap();
                    st.detail =
                        Some("event stream: dropped oversize event payload (> 1 MiB cap)".into());
                } else {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(d);
                }
            }
            // event:/id:/retry:/comments carry nothing we normalize
        }
    }

    fn sse_connect(&self) -> Result<SseBody, AdapterError> {
        let (authority, path) =
            split_url(&format!("{}/global/event", self.endpoint)).map_err(AdapterError::Unknown)?;
        let addr = authority
            .to_socket_addrs()
            .map_err(|e| AdapterError::Unknown(format!("resolve {authority}: {e}")))?
            .next()
            .ok_or_else(|| AdapterError::Unknown(format!("no addresses for {authority}")))?;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
            .map_err(|e| AdapterError::RuntimeUnavailable(format!("connect {addr}: {e}")))?;
        let timeout = *self.sse_read_timeout.lock().unwrap();
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| AdapterError::Unknown(e.to_string()))?;
        let _ = stream.set_nodelay(true);
        let reader_stream =
            crate::ffi::clone_stream(&stream).map_err(|e| AdapterError::Unknown(e.to_string()))?;
        reader_stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| AdapterError::Unknown(e.to_string()))?;

        let mut req =
            format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nAccept: text/event-stream\r\n");
        if let Some(a) = &self.auth {
            req.push_str(&format!(
                "Authorization: Basic {}\r\n",
                crate::base64::encode(a.as_bytes())
            ));
        }
        req.push_str("\r\n");
        {
            let mut s = &stream;
            s.write_all(req.as_bytes())
                .map_err(|e| AdapterError::Unknown(format!("write: {e}")))?;
        }

        let mut reader = BufReader::new(reader_stream);
        let mut status = 0u16;
        let mut ct = String::new();
        let mut chunked = false;
        loop {
            let mut line = String::new();
            let n = reader
                .read_line(&mut line)
                .map_err(|e| AdapterError::RuntimeUnavailable(format!("handshake read: {e}")))?;
            if n == 0 {
                return Err(AdapterError::RuntimeUnavailable(
                    "event stream closed during handshake".into(),
                ));
            }
            let t = line.trim_end();
            if t.is_empty() {
                break;
            }
            if let Some(rest) = t.strip_prefix("HTTP/") {
                status = rest
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            } else if let Some((k, v)) = t.split_once(':') {
                let k = k.trim().to_ascii_lowercase();
                let v = v.trim();
                if k == "content-type" {
                    ct = v.to_ascii_lowercase();
                } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                    chunked = true;
                }
            }
        }
        if status != 200 {
            return Err(AdapterError::InvalidResponse(format!(
                "event stream answered HTTP {status}"
            )));
        }
        if !ct.contains("text/event-stream") {
            let ct = if ct.is_empty() { "(none)" } else { ct.as_str() };
            return Err(AdapterError::InvalidResponse(format!(
                "event stream content-type was {ct} (HTML-200 fallback)"
            )));
        }
        // Register the socket for dispose() BEFORE returning so a shutdown
        // during a blocked read always unblocks the reader.
        *self.killer.lock().unwrap() = Some(stream);
        Ok(SseBody {
            reader,
            mode: if chunked {
                BodyMode::Chunked { remaining: 0 }
            } else {
                BodyMode::Raw
            },
            buf: Vec::new(),
            eof: false,
            oversize_discarding: false,
        })
    }

    fn on_sse_data(&self, raw: &str) {
        let Ok(v) = json::parse(raw) else {
            self.malformed_events.fetch_add(1, Ordering::SeqCst);
            let mut st = self.state.lock().unwrap();
            st.detail = Some(format!(
                "ignored malformed SSE event (non-JSON; total: {})",
                self.malformed_events.load(Ordering::SeqCst)
            ));
            return;
        };
        let Some(payload) = json::get(&v, "payload") else {
            self.malformed_events.fetch_add(1, Ordering::SeqCst);
            let mut st = self.state.lock().unwrap();
            st.detail = Some(format!(
                "ignored malformed SSE event (missing payload; total: {})",
                self.malformed_events.load(Ordering::SeqCst)
            ));
            return;
        };
        let Some(etype) = json::get_str(payload, "type") else {
            self.malformed_events.fetch_add(1, Ordering::SeqCst);
            let mut st = self.state.lock().unwrap();
            st.detail = Some(format!(
                "ignored malformed SSE event (missing type string; total: {})",
                self.malformed_events.load(Ordering::SeqCst)
            ));
            return;
        };
        let props = json::get(payload, "properties");
        let event_id = json::get_str(payload, "id").unwrap_or("");
        let mut out = Vec::new();
        if let Some(props) = props {
            self.normalize(event_id, etype, props, &mut out);
        }
        self.emit(out);
    }

    /// Hands normalized events to the subscribers (the companion: engine
    /// ingest/publish). Never called with the state lock held.
    fn emit(&self, kinds: Vec<EventKind>) {
        if kinds.is_empty() {
            return;
        }
        // Snapshot the subscriber list so callbacks can (un)subscribe
        // without deadlocking against the notification.
        let subs = self.subs.lock().unwrap().clone();
        for kind in kinds {
            for (_, cb) in &subs {
                cb(kind.clone());
            }
        }
    }

    /// A transcript op for a session somebody watches: OpenCode reports
    /// every session of its server, the hub keeps only watched ones.
    fn push(&self, session_id: &str, op: Op) {
        if self.hub.streams(session_id) {
            self.hub.push(session_id, op);
        }
    }

    /// Native SSE event → protocol events (`out`) and transcript ops (§3.6
    /// OpenCode table). Cache side effects happen under the state lock;
    /// events with no representation (`sync` twins, heartbeats, catalogs)
    /// produce nothing.
    fn normalize(&self, event_id: &str, etype: &str, props: &Json, out: &mut Vec<EventKind>) {
        let sid = json::get_str(props, "sessionID").unwrap_or("").to_string();
        match etype {
            "session.created" | "session.updated" => {
                let Some(info) = json::get(props, "info") else {
                    return;
                };
                let mut st = self.state.lock().unwrap();
                let Some(session) = Self::map_session(info, &self.id, &st) else {
                    return;
                };
                Self::upsert_session(&mut st.sessions, session.clone());
                let session = st.sessions.iter().find(|s| s.id == session.id).cloned();
                out.extend(session.map(if etype == "session.created" {
                    EventKind::SessionCreated
                } else {
                    EventKind::SessionUpdated
                }));
            }
            "session.status" => {
                let status = json::get(props, "status");
                match status.and_then(|s| json::get_str(s, "type")).unwrap_or("") {
                    "idle" => self.end_turn(&sid, "completed", None, out),
                    "error" | "failed" => self.session_status(&sid, "interrupted", out),
                    t => {
                        self.session_status(&sid, "running", out);
                        self.busy(&sid, out);
                        if let (Some(status), "retry") = (status, t) {
                            self.retry_notice(&sid, status);
                        }
                    }
                }
            }
            // idempotent: it arrives twice per turn after an abort
            "session.idle" => self.end_turn(&sid, "completed", None, out),
            "session.error" if !sid.is_empty() => {
                let err = json::get(props, "error");
                let name = err.and_then(|e| json::get_str(e, "name"));
                if name == Some("MessageAbortedError") {
                    self.end_turn(&sid, "interrupted", None, out);
                    return;
                }
                let msg = clip(err.map_or_else(|| "unknown native error".to_string(), err_text));
                let before = out.len();
                self.end_turn(&sid, "failed", Some(msg.clone()), out);
                if out.len() == before {
                    // No correlated command (native-side turn): honest
                    // activity entry — never a fabricated commandId.
                    out.push(EventKind::Activity(ActivityEvent {
                        id: if event_id.is_empty() {
                            next_id("act")
                        } else {
                            event_id.to_string()
                        },
                        ts: now_ms(),
                        kind: "session".to_string(),
                        text: clip(format!("opencode: {msg}")),
                        ref_id: Some(sid),
                    }));
                }
            }
            "message.updated" => self.message(props, out),
            "message.part.updated" => self.part(props),
            "message.part.delta" => {
                let (Some(part), Some(delta)) = (
                    json::get_str(props, "partID"),
                    json::get_str(props, "delta"),
                ) else {
                    return;
                };
                // text and reasoning only; a user part has no item of its id
                let known = self.state.lock().unwrap().parts.contains_key(part);
                if known && json::get_str(props, "field") == Some("text") && self.hub.streams(&sid)
                {
                    self.hub.append(&sid, part, Field::Text, delta);
                }
            }
            "permission.asked" | "question.asked" => {
                let question = etype == "question.asked";
                let Some(pr) = permission_request(&self.id, props, question) else {
                    return;
                };
                {
                    let mut st = self.state.lock().unwrap();
                    st.asked.insert(pr.id.clone(), pr.clone());
                    if let Some(call) = &pr.tool_item_id {
                        st.perms.insert(call.clone(), pr.id.clone());
                        trim(&mut st.perms);
                    }
                }
                if let Some(call) = &pr.tool_item_id {
                    self.hub.update(&pr.session_id, call, |item| {
                        if let Some(tool) = &mut item.tool {
                            tool.status = ToolStatus::AwaitingApproval;
                            tool.permission_id = Some(pr.id.clone());
                        }
                    });
                }
                out.push(EventKind::PermissionRequested(pr));
            }
            "permission.replied" | "question.replied" | "question.rejected" => {
                let Some(native_id) = json::get_str(props, "id")
                    .or_else(|| json::get_str(props, "requestID"))
                    .or_else(|| json::get_str(props, "requestId"))
                else {
                    return;
                };
                let mut st = self.state.lock().unwrap();
                // Unknown/untracked request → honest skip, no fabrication.
                let Some(mut pr) = st.asked.remove(native_id) else {
                    return;
                };
                let (status, decision) = match (etype, json::get_str(props, "reply")) {
                    ("question.replied", _) => ("approved", Some("approve-once")),
                    ("question.rejected", _) | (_, Some("reject")) => ("rejected", Some("reject")),
                    (_, Some("once")) => ("approved", Some("approve-once")),
                    (_, Some("always")) => ("approved", Some("approve-session")),
                    // Answered but the payload didn't say how: stale (still
                    // honest — no outcome is claimed).
                    _ => ("stale", None),
                };
                if let (Some(call), "rejected") = (&pr.tool_item_id, status) {
                    st.rejected.insert(call.clone());
                    if st.rejected.len() > MAP_MAX {
                        st.rejected.clear();
                    }
                }
                pr.status = status.to_string();
                pr.decision = decision.map(str::to_string);
                pr.decided_at = Some(now_ms());
                out.push(EventKind::PermissionResolved(pr));
            }
            "todo.updated" => {
                let steps = json::get(props, "todos")
                    .and_then(json::as_arr)
                    .unwrap_or(&[])
                    .iter()
                    .map(|t| PlanStep {
                        text: json::get_str(t, "content").unwrap_or("").to_string(),
                        status: match json::get_str(t, "status") {
                            Some("in_progress") => "in_progress",
                            Some("completed") => "completed",
                            Some("cancelled") => "cancelled",
                            _ => "pending",
                        },
                    })
                    .collect();
                let item = Item {
                    id: "plan".into(),
                    kind: ItemKind::Plan,
                    ts: now_ms(),
                    turn_id: self.latest_turn(&sid),
                    steps,
                    ..Default::default()
                };
                self.push(&sid, Op::Item(item));
            }
            "session.compacted" => {
                let id = format!(
                    "compacted:{}",
                    if event_id.is_empty() {
                        next_id("c")
                    } else {
                        event_id.to_string()
                    }
                );
                self.notice(&sid, id, "info", "Context compacted".into());
            }
            "tui.toast.show" => {
                if json::get_str(props, "title").is_some_and(|t| t.starts_with("MCP")) {
                    self.poke.store(true, Ordering::SeqCst);
                }
            }
            "mcp.tools.changed" => self.poke.store(true, Ordering::SeqCst),
            "mcp.browser.open.failed" => {
                let (Some(name), Some(url)) =
                    (json::get_str(props, "mcpName"), json::get_str(props, "url"))
                else {
                    return;
                };
                self.state
                    .lock()
                    .unwrap()
                    .open_failed
                    .insert(name.to_string(), url.to_string());
                self.push_mcp();
            }
            // server.connected/heartbeat: handled by the connect+refresh
            // cycle; sync (aggregate replay wrapper) has no cursor — state
            // re-list and hydration replace it.
            _ => {}
        }
    }

    /// Session status cache + `session_updated` on change.
    fn session_status(&self, sid: &str, proto: &str, out: &mut Vec<EventKind>) {
        let mut st = self.state.lock().unwrap();
        st.statuses.insert(sid.to_string(), proto.to_string());
        if let Some(session) = st.sessions.iter_mut().find(|s| s.id == sid)
            && session.status != proto
        {
            session.status = proto.to_string();
            session.last_activity_at = now_ms();
            out.push(EventKind::SessionUpdated(session.clone()));
        }
    }

    /// `session.status busy`: the session's prompts are `running` (once
    /// each), live state `running`.
    fn busy(&self, sid: &str, out: &mut Vec<EventKind>) {
        let op = {
            let mut st = self.state.lock().unwrap();
            for (_, command_id, started) in st.pending.iter_mut().filter(|p| p.0 == sid && !p.2) {
                *started = true;
                out.push(EventKind::CommandProgress {
                    command_id: command_id.clone(),
                    state: "running".into(),
                    session_id: Some(sid.to_string()),
                });
            }
            let live = st.live.entry(sid.to_string()).or_default();
            let changed = live.state != "running";
            live.state = "running";
            changed.then(|| Op::State(Self::live_state(&st, sid, "running")))
        };
        if let Some(op) = op {
            self.push(sid, op);
        }
    }

    /// Turn end (§4.6): idle → completed, abort → interrupted, other errors →
    /// failed. Closes the running turns, the session's prompt commands
    /// (completed / cancelled / failed) and a waiting interrupt (completed).
    /// Idempotent: a second idle finds nothing open.
    fn end_turn(
        &self,
        sid: &str,
        status: &'static str,
        error: Option<String>,
        out: &mut Vec<EventKind>,
    ) {
        if status == "completed" {
            self.session_status(sid, "completed", out);
        }
        let now = now_ms();
        let mut ops = Vec::new();
        let mut st = self.state.lock().unwrap();
        let (prompt, prompt_error) = match status {
            "completed" => ("completed", None),
            "interrupted" => ("cancelled", None),
            _ => ("failed", error.clone()),
        };
        let mut i = 0;
        while i < st.pending.len() {
            if st.pending[i].0 == sid {
                let (_, command_id, _) = st.pending.remove(i);
                out.push(result(
                    &command_id,
                    prompt,
                    prompt_error.clone(),
                    Some(sid.to_string()),
                ));
            } else {
                i += 1;
            }
        }
        let live = st.live.entry(sid.to_string()).or_default();
        for turn in live.turns.iter_mut().filter(|t| t.status == "running") {
            turn.status = status;
            turn.ended_at = Some(now);
            turn.error = error.clone();
            ops.push(Op::Turn(turn.clone()));
        }
        if let Some(command_id) = live.interrupt.take() {
            out.push(result(
                &command_id,
                "completed",
                None,
                Some(sid.to_string()),
            ));
        }
        if live.state != "idle" {
            live.state = "idle";
            ops.push(Op::State(Self::live_state(&st, sid, "idle")));
        }
        drop(st);
        for op in ops {
            self.push(sid, op);
        }
    }

    fn latest_turn(&self, sid: &str) -> Option<String> {
        let st = self.state.lock().unwrap();
        st.live.get(sid)?.turns.last().map(|t| t.id.clone())
    }

    fn notice(&self, sid: &str, id: String, level: &'static str, text: String) {
        let item = Item {
            id,
            kind: ItemKind::Notice,
            ts: now_ms(),
            turn_id: self.latest_turn(sid),
            level: Some(level),
            text,
            ..Default::default()
        };
        self.push(sid, Op::Item(item));
    }

    /// `session.status retry` → "Retrying: <message>" (+ the action's label
    /// and link), one notice per turn updated in place: a quota retry loops.
    fn retry_notice(&self, sid: &str, status: &Json) {
        let mut text = format!(
            "Retrying: {}",
            json::get_str(status, "message").unwrap_or("the provider failed")
        );
        if let Some(action) = json::get(status, "action") {
            let label = json::get_str(action, "label").unwrap_or("details");
            if let Some(link) = json::get_str(action, "link") {
                text.push_str(&format!(" ({label}: {link})"));
            }
        }
        let anchor = self.latest_turn(sid).unwrap_or_else(|| sid.to_string());
        self.notice(sid, format!("retry:{anchor}"), "warning", text);
    }

    /// `message.updated`: records the role and turn of every message. An
    /// assistant message starts its turn (id = the user messageID, so a
    /// queued prompt's turn starts when it actually runs); a user message's
    /// `summary.diffs` become the turn's diffs and `Session.files`.
    fn message(&self, props: &Json, out: &mut Vec<EventKind>) {
        let Some(info) = json::get(props, "info") else {
            return;
        };
        let (Some(id), Some(sid)) = (json::get_str(info, "id"), json::get_str(info, "sessionID"))
        else {
            return;
        };
        let user = json::get_str(info, "role") == Some("user");
        let turn_id = match user {
            true => id,
            false => json::get_str(info, "parentID").unwrap_or(id),
        };
        let mut st = self.state.lock().unwrap();
        // ADR-021: the model it ran on and the context it filled
        if !user && note_usage(&mut st, sid, info) {
            let status = match st.live.get(sid).map_or("", |l| l.state) {
                "" => "idle",
                s => s,
            };
            let state = Self::live_state(&st, sid, status);
            drop(st);
            self.push(sid, Op::State(state));
            st = self.state.lock().unwrap();
        }
        st.roles.insert(id.to_string(), (user, turn_id.to_string()));
        trim(&mut st.roles);
        if st.live.len() > SESSIONS_KEPT {
            let watched = st.watched.clone();
            st.live.retain(|s, _| watched.contains(s));
        }
        let op = if user {
            let diffs = summary_diffs(info);
            if diffs.is_empty() {
                return;
            }
            if let Some(session) = st.sessions.iter_mut().find(|s| s.id == sid) {
                for d in &diffs {
                    let change = FileChange {
                        path: d.path.clone(),
                        additions: d.additions,
                        deletions: d.deletions,
                        status: if d.status == "deleted" {
                            "removed"
                        } else {
                            d.status
                        }
                        .into(),
                    };
                    match session.files.iter_mut().find(|f| f.path == d.path) {
                        Some(f) => *f = change,
                        None => session.files.push(change),
                    }
                }
                out.push(EventKind::SessionUpdated(session.clone()));
            }
            let live = st.live.entry(sid.to_string()).or_default();
            let Some(turn) = live.turns.iter_mut().find(|t| t.id == turn_id) else {
                return;
            };
            turn.diffs = diffs;
            Op::Turn(turn.clone())
        } else {
            let running = st.statuses.get(sid).is_some_and(|s| s == "running");
            let time = json::get(info, "time");
            let live = st.live.entry(sid.to_string()).or_default();
            if live.turns.iter().any(|t| t.id == turn_id) {
                return;
            }
            let completed = time.and_then(|t| json::get_i64(t, "completed"));
            let turn = TurnState {
                id: turn_id.to_string(),
                status: if running || completed.is_none() {
                    "running"
                } else {
                    "completed"
                },
                started_at: time
                    .and_then(|t| json::get_i64(t, "created"))
                    .unwrap_or_else(now_ms),
                ended_at: completed.filter(|_| !running),
                ..Default::default()
            };
            live.turns.push(turn.clone());
            if live.turns.len() > TURNS_KEPT {
                live.turns.remove(0);
            }
            Op::Turn(turn)
        };
        drop(st);
        self.push(sid, op);
    }

    /// `message.part.updated` → user / text / thinking / tool items and the
    /// step-finish usage. A part of a message whose role is unknown waits
    /// for hydration (never guess: a user's text must not show as the
    /// agent's).
    fn part(&self, props: &Json) {
        let Some(part) = json::get(props, "part") else {
            return;
        };
        let (Some(id), Some(msg), Some(sid)) = (
            json::get_str(part, "id"),
            json::get_str(part, "messageID"),
            json::get_str(part, "sessionID").or_else(|| json::get_str(props, "sessionID")),
        ) else {
            return;
        };
        let mut st = self.state.lock().unwrap();
        let Some((user, turn)) = st.roles.get(msg).cloned() else {
            return;
        };
        // ADR-021: a photo of the prompt counts on its user item
        if user && is_image(part) {
            let list = st.images.entry(msg.to_string()).or_default();
            if !list.iter().any(|p| p == id) {
                list.push(id.to_string());
            }
            let n = list.len() as u32;
            trim(&mut st.images);
            drop(st);
            if self.hub.streams(sid) {
                self.hub.update(sid, msg, |item| item.images = n);
            }
            return;
        }
        let op = match json::get_str(part, "type") {
            Some("step-finish") => {
                let Some(turn) = st
                    .live
                    .get_mut(sid)
                    .and_then(|l| l.turns.iter_mut().find(|t| t.id == turn))
                else {
                    return;
                };
                add_usage(turn, part);
                Op::Turn(turn.clone())
            }
            _ => match part_item(part, id, msg, user, &turn, &st) {
                Some(item) => {
                    if matches!(item.kind, ItemKind::Text | ItemKind::Thinking) {
                        st.parts.insert(id.to_string(), item.kind);
                        trim(&mut st.parts);
                    }
                    Op::Item(item)
                }
                None => return,
            },
        };
        drop(st);
        self.push(sid, op);
    }

    fn spawn_hydrate(self: &Arc<Self>, sid: String) {
        let inner = self.clone();
        let _ = std::thread::Builder::new()
            .name("opencode-hydrate".into())
            .spawn(move || inner.hydrate(&sid));
    }

    /// Rebuilds a watched session's transcript from `GET
    /// /session/{id}/message` (§4.4): roles, items in message order, and the
    /// turns with their usage and outcome. Items replace by id, so running
    /// it again (every SSE reconnect) only refreshes what changed.
    fn hydrate(&self, sid: &str) {
        let Ok(resp) = self.req(
            "GET",
            &format!("{}/session/{sid}/message", self.endpoint),
            None,
        ) else {
            return;
        };
        let Some(msgs) = validate_json(&resp).ok().filter(|_| resp.status == 200) else {
            return;
        };
        let mut ops = Vec::new();
        let mut turns: Vec<(TurnState, bool)> = Vec::new();
        let mut st = self.state.lock().unwrap();
        let busy = st.statuses.get(sid).is_some_and(|s| s == "running");
        for m in json::as_arr(&msgs).unwrap_or(&[]) {
            let (Some(info), Some(parts)) = (
                json::get(m, "info"),
                json::get(m, "parts").and_then(json::as_arr),
            ) else {
                continue;
            };
            let Some(id) = json::get_str(info, "id") else {
                continue;
            };
            let user = json::get_str(info, "role") == Some("user");
            let turn_id = match user {
                true => id,
                false => json::get_str(info, "parentID").unwrap_or(id),
            };
            st.roles.insert(id.to_string(), (user, turn_id.to_string()));
            if user {
                let photos: Vec<String> = parts
                    .iter()
                    .filter(|p| is_image(p))
                    .filter_map(|p| json::get_str(p, "id").map(str::to_string))
                    .collect();
                if !photos.is_empty() {
                    st.images.insert(id.to_string(), photos);
                }
            } else {
                note_usage(&mut st, sid, info);
            }
            let time = json::get(info, "time");
            let at = |k| time.and_then(|t| json::get_i64(t, k));
            if user {
                turns.push((
                    TurnState {
                        id: id.to_string(),
                        status: "completed",
                        started_at: at("created").unwrap_or_else(now_ms),
                        diffs: summary_diffs(info),
                        ..Default::default()
                    },
                    false,
                ));
            }
            let Some((turn, seen)) = turns.iter_mut().find(|(t, _)| t.id == turn_id) else {
                continue;
            };
            if !user {
                *seen = true;
                turn.ended_at = at("completed").max(turn.ended_at);
                match json::get(info, "error") {
                    Some(e) if json::get_str(e, "name") == Some("MessageAbortedError") => {
                        turn.status = "interrupted"
                    }
                    Some(e) => {
                        turn.status = "failed";
                        turn.error = Some(clip(err_text(e)));
                    }
                    None => {}
                }
            }
            for part in parts {
                let Some(pid) = json::get_str(part, "id") else {
                    continue;
                };
                if json::get_str(part, "type") == Some("step-finish") {
                    add_usage(turn, part);
                } else if let Some(mut item) = part_item(part, pid, id, user, turn_id, &st) {
                    if matches!(item.kind, ItemKind::Text | ItemKind::Thinking) {
                        // a finished message's parts are done even when cut
                        item.done |= at("completed").is_some();
                        st.parts.insert(pid.to_string(), item.kind);
                    }
                    ops.push(Op::Item(item));
                }
            }
        }
        trim(&mut st.roles);
        trim(&mut st.parts);
        // the newest turn still runs while the session is busy
        if let Some((last, _)) = turns.iter_mut().rev().find(|(_, seen)| *seen)
            && busy
            && last.status == "completed"
        {
            last.status = "running";
            last.ended_at = None;
        }
        let live = st.live.entry(sid.to_string()).or_default();
        for (turn, _) in turns.into_iter().filter(|(_, seen)| *seen) {
            match live.turns.iter_mut().find(|t| t.id == turn.id) {
                Some(t) => *t = turn.clone(),
                None => live.turns.push(turn.clone()),
            }
            ops.push(Op::Turn(turn));
        }
        let keep = live.turns.len().saturating_sub(TURNS_KEPT);
        live.turns.drain(..keep);
        // the composer's view (model, models, commands, context)
        let status = if busy { "running" } else { "idle" };
        live.state = status;
        ops.push(Op::State(Self::live_state(&st, sid, status)));
        trim(&mut st.images);
        drop(st);
        for op in ops {
            self.hub.push(sid, op);
        }
    }
}

/// Drops the older half of an id map past `MAP_MAX` (OpenCode's `msg_` and
/// `prt_` ids sort by creation time).
fn trim<V>(map: &mut HashMap<String, V>) {
    if map.len() <= MAP_MAX {
        return;
    }
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    for k in &keys[..keys.len() / 2] {
        map.remove(k);
    }
}

/// One part as a transcript item: a user's typed text (synthetic and
/// ignored parts are context OpenCode added, not what the user wrote),
/// assistant text or reasoning (`done` once `time.end` is set), or a tool
/// call keyed by `callID`.
fn part_item(
    part: &Json,
    id: &str,
    msg: &str,
    user: bool,
    turn: &str,
    st: &OcState,
) -> Option<Item> {
    let time = json::get(part, "time");
    let ts = time
        .and_then(|t| json::get_i64(t, "start"))
        .unwrap_or_else(now_ms);
    let text = || json::get_str(part, "text").unwrap_or("").to_string();
    let base = Item {
        ts,
        turn_id: Some(turn.to_string()),
        ..Default::default()
    };
    match json::get_str(part, "type")? {
        "text" if user => {
            let extra = |k| json::get_bool(part, k) == Some(true);
            (!extra("synthetic") && !extra("ignored")).then(|| Item {
                id: msg.to_string(),
                kind: ItemKind::User,
                text: text(),
                images: st.images.get(msg).map_or(0, |p| p.len() as u32),
                ..base
            })
        }
        kind @ ("text" | "reasoning") => Some(Item {
            id: id.to_string(),
            kind: if kind == "text" {
                ItemKind::Text
            } else {
                ItemKind::Thinking
            },
            text: text(),
            done: time.and_then(|t| json::get_i64(t, "end")).is_some(),
            ..base
        }),
        "tool" => {
            let call = json::get_str(part, "callID")?;
            let state = json::get(part, "state");
            let tool = tool_call(part, state, st.perms.get(call), st, call);
            let ts = state
                .and_then(|s| json::get(s, "time"))
                .and_then(|t| json::get_i64(t, "start"))
                .unwrap_or(ts);
            Some(Item {
                id: call.to_string(),
                kind: ItemKind::Tool,
                ts,
                tool: Some(tool),
                ..base
            })
        }
        _ => None,
    }
}

/// A tool part's `state` → `ToolCall` (§3.6): pending → running (repeated
/// `metadata.output` is the output so far) → completed / error, which is a
/// denial after a rejected request. While a request for the call is pending
/// the card awaits approval.
fn tool_call(
    part: &Json,
    state: Option<&Json>,
    perm: Option<&String>,
    st: &OcState,
    call: &str,
) -> ToolCall {
    let get = |k| state.and_then(|s| json::get(s, k));
    let input = get("input");
    let input_str = |k| input.and_then(|i| json::get_str(i, k));
    let meta = get("metadata");
    let error = get("error").and_then(json::as_str).map(str::to_string);
    let status = match get("status").and_then(json::as_str) {
        Some("pending" | "running") if perm.is_some_and(|p| st.asked.contains_key(p)) => {
            ToolStatus::AwaitingApproval
        }
        Some("running") => ToolStatus::Running,
        Some("completed") => ToolStatus::Completed,
        Some("error")
            if st.rejected.contains(call)
                || error
                    .as_deref()
                    .is_some_and(|e| e.starts_with("The user rejected permission")) =>
        {
            ToolStatus::Denied
        }
        Some("error") => ToolStatus::Failed,
        _ => ToolStatus::Pending,
    };
    let output = match status {
        ToolStatus::Completed => get("output"),
        _ => meta.and_then(|m| json::get(m, "output")),
    };
    let path = meta
        .and_then(|m| json::get_str(m, "filepath"))
        .or_else(|| input_str("filePath"))
        .unwrap_or("");
    let diffs = meta
        .and_then(|m| json::get_str(m, "diff"))
        .map(|patch| vec![file_diff(path, "modified", patch)])
        .unwrap_or_default();
    let title = get("title")
        .and_then(json::as_str)
        .filter(|t| !t.is_empty())
        .or_else(|| input_str("command"))
        .or_else(|| input_str("filePath"))
        .or_else(|| input_str("url"))
        .or_else(|| input_str("pattern"))
        .map(str::to_string);
    let time = get("time");
    let at = |k| time.and_then(|t| json::get_i64(t, k));
    ToolCall {
        name: json::get_str(part, "tool").unwrap_or("tool").to_string(),
        title,
        status,
        // the raw command for a shell, else the input as JSON
        input: match (input_str("command"), input) {
            (Some(c), _) => c.to_string(),
            (None, Some(v @ Json::Obj(f))) if !f.is_empty() => json::stringify(v),
            _ => String::new(),
        },
        output: output.and_then(json::as_str).unwrap_or("").to_string(),
        error,
        diffs,
        permission_id: perm.cloned(),
        duration_ms: at("end").zip(at("start")).map(|(e, s)| e - s),
        ..Default::default()
    }
}

/// ADR-021: a `file` part holding a picture.
fn is_image(part: &Json) -> bool {
    json::get_str(part, "type") == Some("file")
        && json::get_str(part, "mime").is_some_and(|m| m.starts_with("image/"))
}

/// ADR-021: prompt images as OpenCode `file` parts with `data:` URLs.
fn image_parts(images: &[PromptImage]) -> Vec<Json> {
    images
        .iter()
        .enumerate()
        .map(|(i, im)| {
            let ext = im.media_type.trim_start_matches("image/");
            json::obj(vec![
                ("type", json::s("file")),
                ("mime", json::s(&im.media_type)),
                ("filename", json::s(&format!("photo-{}.{ext}", i + 1))),
                (
                    "url",
                    json::s(&format!("data:{};base64,{}", im.media_type, im.data)),
                ),
            ])
        })
        .collect()
}

/// ADR-021: `/name args` when `name` is one of OpenCode's commands.
fn slash<'a>(text: &'a str, commands: &[String]) -> Option<(&'a str, &'a str)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    commands
        .iter()
        .any(|c| c == name)
        .then(|| (name, args.trim()))
}

/// Whether OpenCode's `directory` is the workspace folder (Windows paths
/// compare without case; either slash).
fn same_dir(dir: &str, workspace: &str) -> bool {
    let norm = |p: &str| p.trim_end_matches(['\\', '/']).replace('/', "\\");
    norm(dir).eq_ignore_ascii_case(&norm(workspace))
}

/// ADR-021 `GET /config/providers` (`{providers: [{id, name, models: {id:
/// {id, name, limit: {context}, status}}}]}`) → (`provider/model`, label,
/// context limit), the configured model first, deprecated ones left out,
/// at most `MODELS_MAX`.
fn catalog_models(v: &Json, default: Option<&str>) -> Vec<(String, String, i64)> {
    let mut out = Vec::new();
    for p in json::get(v, "providers")
        .and_then(json::as_arr)
        .unwrap_or(&[])
    {
        let (Some(provider), Some(Json::Obj(models))) =
            (json::get_str(p, "id"), json::get(p, "models"))
        else {
            continue;
        };
        for (key, m) in models {
            if json::get_str(m, "status") == Some("deprecated") {
                continue;
            }
            let id = json::get_str(m, "id").unwrap_or(key);
            let value = format!("{provider}/{id}");
            if !is_provider_model(&value) {
                continue;
            }
            let limit = json::get(m, "limit")
                .and_then(|l| json::get_i64(l, "context"))
                .unwrap_or(0);
            let label = json::get_str(m, "name").unwrap_or(id).to_string();
            out.push((value, label, limit));
        }
    }
    out.sort_by_key(|m| Some(m.0.as_str()) != default);
    out.truncate(MODELS_MAX);
    out
}

/// ADR-021: an assistant message's model (`providerID/modelID`) and its
/// context use (every token of the message: input, output, reasoning and
/// cache) against that model's limit. True when the context changed.
fn note_usage(st: &mut OcState, sid: &str, info: &Json) -> bool {
    if let Some(agent) = json::get_str(info, "agent").or_else(|| json::get_str(info, "mode")) {
        st.last_agent.insert(sid.to_string(), agent.to_string());
        trim(&mut st.last_agent);
    }
    if let (Some(p), Some(m)) = (
        json::get_str(info, "providerID"),
        json::get_str(info, "modelID"),
    ) {
        st.last_model.insert(sid.to_string(), format!("{p}/{m}"));
        trim(&mut st.last_model);
    }
    let Some(tokens) = json::get(info, "tokens") else {
        return false;
    };
    let n = |v: &Json, k| json::get_i64(v, k).unwrap_or(0);
    let cache = json::get(tokens, "cache");
    let used = n(tokens, "input")
        + n(tokens, "output")
        + n(tokens, "reasoning")
        + cache.map_or(0, |c| n(c, "read") + n(c, "write"));
    let model = st.last_model.get(sid);
    let max = st
        .catalog
        .models
        .iter()
        .find(|c| Some(&c.0) == model)
        .map_or(0, |c| c.2);
    if used <= 0 || max <= 0 || st.context.get(sid) == Some(&(used, max)) {
        return false;
    }
    st.context.insert(sid.to_string(), (used, max));
    trim(&mut st.context);
    true
}

/// A unified diff for one file, its +/- lines counted (headers excluded).
fn file_diff(path: &str, status: &'static str, patch: &str) -> FileDiff {
    let count = |sign: char, head: &str| {
        patch
            .lines()
            .filter(|l| l.starts_with(sign) && !l.starts_with(head))
            .count() as i64
    };
    FileDiff {
        path: path.to_string(),
        status,
        additions: count('+', "+++"),
        deletions: count('-', "---"),
        patch: patch.to_string(),
        truncated: false,
    }
}

/// A user message's `summary.diffs` (`[{file, patch, additions, deletions,
/// status}]`): the turn's file changes.
fn summary_diffs(info: &Json) -> Vec<FileDiff> {
    let list = json::get(info, "summary")
        .and_then(|s| json::get(s, "diffs"))
        .and_then(json::as_arr)
        .unwrap_or(&[]);
    list.iter()
        .filter_map(|d| {
            let path = json::get_str(d, "file")?;
            let status = match json::get_str(d, "status") {
                Some("added") => "added",
                Some("deleted" | "removed") => "deleted",
                _ => "modified",
            };
            Some(FileDiff {
                additions: json::get_i64(d, "additions").unwrap_or(0),
                deletions: json::get_i64(d, "deletions").unwrap_or(0),
                ..file_diff(path, status, json::get_str(d, "patch").unwrap_or(""))
            })
        })
        .collect()
}

/// A `step-finish` part's tokens and cost, added to its turn.
fn add_usage(turn: &mut TurnState, part: &Json) {
    let Some(tokens) = json::get(part, "tokens") else {
        return;
    };
    let n = |k| json::get_i64(tokens, k).unwrap_or(0);
    let cache = json::get(tokens, "cache").and_then(|c| json::get_i64(c, "read"));
    let usage = turn.usage.get_or_insert_with(Usage::default);
    usage.input += n("input");
    usage.output += n("output");
    if let Some(read) = cache {
        *usage.cache_read.get_or_insert(0) += read;
    }
    if n("reasoning") > 0 {
        *usage.reasoning.get_or_insert(0) += n("reasoning");
    }
    if let Some(cost) = json::get(part, "cost")
        .and_then(json::as_f64)
        .filter(|c| *c > 0.0)
    {
        *turn.cost_usd.get_or_insert(0.0) += cost;
    }
}

/// `permission.asked` / `question.asked` → the engine's record (§4.5):
/// `toolItemId` = the tool's `callID`, the native choices as options, the
/// detail the phone renders inside the card. Legacy fields stay filled for
/// v2 phones. Everything here is journaled: redacted and capped.
fn permission_request(host: &str, props: &Json, question: bool) -> Option<PermissionRequest> {
    let native_id = json::get_str(props, "id")?.to_string();
    let call = json::get(props, "tool")
        .and_then(|t| json::get_str(t, "callID"))
        .map(str::to_string);
    let message = json::get(props, "tool").and_then(|t| json::get_str(t, "messageID"));
    let strings = |k: &str| -> Vec<String> {
        json::get(props, k)
            .and_then(json::as_arr)
            .unwrap_or(&[])
            .iter()
            .filter_map(|p| json::as_str(p).map(redact))
            .collect()
    };
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
    let now = now_ms();
    let (permission, patterns, detail, options) = if question {
        let questions = json::get(props, "questions")
            .and_then(json::as_arr)
            .unwrap_or(&[]);
        let first = questions
            .first()
            .and_then(|q| json::get_str(q, "question"))
            .unwrap_or("question");
        (
            "question".to_string(),
            vec![redact(first)],
            detail_question(questions),
            vec![
                option("answer", "approve-once", "Submit answers", "once"),
                option("skip", "reject", "Skip", "once"),
            ],
        )
    } else {
        let permission = json::get_str(props, "permission").unwrap_or("permission");
        let patterns = strings("patterns");
        let always = strings("always");
        let rule = always.join(" ");
        let label = if rule.is_empty() {
            "Always allow until OpenCode restarts".to_string()
        } else {
            format!("Always allow `{rule}` until OpenCode restarts")
        };
        let options = vec![
            option("once", "approve-once", "Allow once", "once"),
            PermissionOption {
                rule: (!rule.is_empty()).then_some(rule),
                ..option("always", "approve-session", &label, "session")
            },
            // rejecting with a message keeps the turn going (§3.3)
            PermissionOption {
                takes_message: true,
                ..option("reject", "reject", "Deny", "once")
            },
        ];
        let detail = permission_detail(permission, json::get(props, "metadata"), &patterns);
        (permission.to_string(), patterns, detail, options)
    };
    let action = if question {
        "agent-control"
    } else {
        native_action(&permission)
    };
    Some(PermissionRequest {
        // Native id doubles as the protocol id: per_*/que_* is already
        // globally unique and keeps the mapping reversible.
        id: native_id.clone(),
        native_request_id: native_id,
        session_id: json::get_str(props, "sessionID").unwrap_or("").to_string(),
        agent_id: "build".to_string(),
        host_id: host.to_string(),
        action: action.to_string(),
        title: if question {
            "OpenCode asks a question".to_string()
        } else {
            format!("OpenCode permission: {permission}")
        },
        command: cap(
            if patterns.is_empty() || question {
                patterns.join(" ")
            } else {
                format!("{permission} {}", patterns.join(" "))
            }
            .trim()
            .to_string(),
            COMMAND_CAP,
        ),
        args: patterns,
        directory: None,
        risk: match action {
            "shell" => "high",
            "file-write" | "network" => "medium",
            _ => "low",
        }
        .to_string(),
        status: "pending".to_string(),
        supported_decisions: vec![
            "approve-once".into(),
            "approve-session".into(),
            "reject".into(),
        ],
        revision: 1,
        fingerprint: None,
        requested_at: now,
        // none of OpenCode's requests expire (the engine applies the same)
        expires_at: now + LIVE_PERMISSION_TTL_MS,
        context: match message {
            Some(m) => format!("native {permission} (message {m})"),
            None => format!("native {permission}"),
        },
        decided_at: None,
        decision: None,
        from_pc: false,
        tool_item_id: call,
        options,
        detail: Some(detail),
    })
}

/// What the phone shows inside the card: the bash command, the edit's diff
/// (`metadata.diff`), the URL, or the patterns.
fn permission_detail(permission: &str, meta: Option<&Json>, patterns: &[String]) -> Json {
    let meta_str = |k| meta.and_then(|m| json::get_str(m, k)).map(redact);
    let first = patterns.first().cloned().unwrap_or_default();
    let kind = |k: &str, fields: Vec<(&str, String)>| {
        let mut all = vec![("kind", json::s(k))];
        all.extend(
            fields
                .into_iter()
                .map(|(k, v)| (k, json::s(&cap(v, DETAIL_CAP)))),
        );
        json::obj(all)
    };
    match permission {
        "bash" => kind(
            "shell",
            vec![(
                "command",
                meta_str("command").unwrap_or_else(|| patterns.join(" ")),
            )],
        ),
        "edit" => {
            let mut fields = vec![("path", meta_str("filepath").unwrap_or(first))];
            fields.extend(meta_str("diff").map(|d| ("patch", d)));
            kind("edit", fields)
        }
        "webfetch" => kind("fetch", vec![("url", meta_str("url").unwrap_or(first))]),
        "read" => kind(
            "read",
            vec![("path", meta_str("filePath").unwrap_or(first))],
        ),
        _ => kind(
            "generic",
            vec![
                ("tool", permission.to_string()),
                ("input", patterns.join("\n")),
            ],
        ),
    }
}

/// `question.asked` questions → `detail.question` (ids are indexes: the
/// reply is one label array per question, in order). `custom` (free text)
/// defaults to allowed.
fn detail_question(questions: &[Json]) -> Json {
    let list = questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let choices = json::get(q, "options")
                .and_then(json::as_arr)
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
            json::obj(vec![
                ("id", json::s(&i.to_string())),
                (
                    "text",
                    json::s(&redact(json::get_str(q, "question").unwrap_or(""))),
                ),
                ("header", json::s(json::get_str(q, "header").unwrap_or(""))),
                ("options", Json::Arr(choices)),
                (
                    "multiSelect",
                    Json::Bool(json::get_bool(q, "multiple") == Some(true)),
                ),
                (
                    "allowFreeText",
                    Json::Bool(json::get_bool(q, "custom") != Some(false)),
                ),
            ])
        })
        .collect();
    json::obj(vec![
        ("kind", json::s("question")),
        ("questions", Json::Arr(list)),
    ])
}

/// Cuts `s` to `max` bytes at a char boundary.
fn cap(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut at = max;
        while !s.is_char_boundary(at) {
            at -= 1;
        }
        s.truncate(at);
    }
    s
}
/// Native error payload → message. Observed shape (probe §8):
/// `{name, data:{message}}`; string and bare-name fallbacks kept.
fn err_text(e: &Json) -> String {
    if let Some(s) = json::as_str(e) {
        return s.to_string();
    }
    if let Some(d) = json::get(e, "data")
        && let Some(m) = json::get_str(d, "message")
    {
        return m.to_string();
    }
    if let Some(n) = json::get_str(e, "name") {
        return n.to_string();
    }
    json::stringify(e)
}

/// Native permission name → protocol action (heuristic; unknown names fall
/// back to the most restricted interpretation, `agent-control`).
fn native_action(permission: &str) -> &'static str {
    let p = permission.to_ascii_lowercase();
    if p.contains("bash") || p.contains("shell") || p.contains("exec") {
        "shell"
    } else if p.contains("edit") || p.contains("write") || p.contains("patch") {
        "file-write"
    } else if p.contains("fetch")
        || p.contains("web")
        || p.contains("network")
        || p.contains("http")
    {
        "network"
    } else {
        "agent-control"
    }
}

/* ------------------------------------------------------------------ */
/* Tests                                                               */
/* ------------------------------------------------------------------ */

/// Fake `opencode serve` for integration tests — `pub(crate)` so the
/// companion wiring tests can drive the same fixture (threads per
/// connection: SSE holds one open while other requests keep being served).
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[derive(Clone)]
    pub struct Opts {
        pub html_prompt: bool,
        pub sse: Option<String>,
        pub chunked_sse: bool,
        /// Hold the SSE connection open after the script (else close it,
        /// which drives the adapter's reconnect loop).
        pub sse_hold: bool,
        /// Serve an oversize (> 4 MiB) response body on /session.
        pub oversize_http: bool,
        /// Extra JSON answers by exact "METHOD /path" (checked first).
        pub routes: Vec<(String, String)>,
        /// Wait this long before writing the SSE script.
        pub sse_delay_ms: u64,
    }

    impl Default for Opts {
        fn default() -> Self {
            Opts {
                html_prompt: false,
                sse: None,
                chunked_sse: false,
                sse_hold: true,
                oversize_http: false,
                routes: Vec::new(),
                sse_delay_ms: 0,
            }
        }
    }

    /// Fake `opencode serve` fixture (see module comment).
    pub struct Fake {
        pub addr: String,
        pub requests: Arc<Mutex<Vec<String>>>,
        pub sse_conns: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Fake {
        pub fn port(&self) -> u16 {
            self.addr.rsplit_once(':').unwrap().1.parse().unwrap()
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.addr.trim_start_matches("http://"));
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn respond(stream: &mut TcpStream, status: &str, ct: &str, body: &str) {
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
    }

    const SESSION_INFO: &str = r#"{"id":"ses_test1","slug":"test-session","projectID":"global","directory":"C:\\proj\\demo","title":"Demo session","time":{"created":1700000000000,"updated":1700000005000}}"#;
    const AGENTS: &str = r#"[{"name":"build","mode":"primary","model":null,"tools":0},{"name":"explore","mode":"subagent","model":null,"tools":0}]"#;

    pub fn start(opts: Opts) -> Fake {
        start_on_listener(TcpListener::bind("127.0.0.1:0").unwrap(), opts)
    }

    pub fn start_on_port(port: u16, opts: Opts) -> Fake {
        start_on_listener(
            TcpListener::bind(format!("127.0.0.1:{port}")).unwrap(),
            opts,
        )
    }

    fn start_on_listener(listener: TcpListener, opts: Opts) -> Fake {
        let port = listener.local_addr().unwrap().port();
        let addr = format!("http://127.0.0.1:{port}");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sse_conns = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (o, r, s, st) = (
            opts.clone(),
            requests.clone(),
            sse_conns.clone(),
            stop.clone(),
        );
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if st.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let (o, r, s, st) = (o.clone(), r.clone(), s.clone(), st.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(match stream.try_clone() {
                        Ok(c) => c,
                        Err(_) => return,
                    });
                    let mut req_line = String::new();
                    if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut content_length = 0usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" || line == "\n" {
                            break;
                        }
                        if let Some((k, v)) = line.split_once(':')
                            && k.trim().eq_ignore_ascii_case("content-length")
                        {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    if content_length > 0 && reader.read_exact(&mut body).is_err() {
                        return;
                    }
                    let body = String::from_utf8_lossy(&body).into_owned();
                    let mut parts = req_line.split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("/").to_string();
                    let (path, _query) = path.split_once('?').unwrap_or((path.as_str(), ""));
                    r.lock().unwrap().push(format!("{method} {path} {body}"));
                    let key = format!("{method} {path}");
                    if let Some((_, answer)) = o.routes.iter().find(|(k, _)| *k == key) {
                        respond(&mut stream, "200 OK", "application/json", answer);
                        return;
                    }

                    match (method.as_str(), path) {
                        ("GET", "/global/health") => respond(
                            &mut stream,
                            "200 OK",
                            "application/json",
                            r#"{"healthy":true,"version":"1.17.7"}"#,
                        ),
                        ("GET", "/session") => {
                            if o.oversize_http {
                                let huge = format!(
                                    r#"[{},{{"padding":"{}"}}]"#,
                                    SESSION_INFO,
                                    "x".repeat(MAX_HTTP_BODY_BYTES + 1024)
                                );
                                respond(&mut stream, "200 OK", "application/json", &huge);
                            } else {
                                respond(
                                    &mut stream,
                                    "200 OK",
                                    "application/json",
                                    &format!("[{SESSION_INFO}]"),
                                )
                            }
                        }
                        ("GET", "/agent") => {
                            respond(&mut stream, "200 OK", "application/json", AGENTS)
                        }
                        ("GET", p) if p.ends_with("/message") => {
                            respond(&mut stream, "200 OK", "application/json", "[]")
                        }
                        ("GET", p) if p.ends_with("/diff") => {
                            respond(&mut stream, "200 OK", "application/json", "[]")
                        }
                        ("GET", "/global/event") => {
                            s.fetch_add(1, Ordering::SeqCst);
                            let mut head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n".to_string();
                            if o.chunked_sse {
                                head.push_str("Transfer-Encoding: chunked\r\n");
                            }
                            head.push_str("\r\n");
                            if stream.write_all(head.as_bytes()).is_err() {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(o.sse_delay_ms));
                            if let Some(script) = &o.sse {
                                if o.chunked_sse {
                                    // split into two chunks so the reader's
                                    // partial-line reassembly is exercised
                                    let bytes = script.as_bytes();
                                    let mid = bytes.len() / 2;
                                    let mut out = Vec::new();
                                    for part in [&bytes[..mid], &bytes[mid..]] {
                                        out.extend_from_slice(
                                            format!("{:X}\r\n", part.len()).as_bytes(),
                                        );
                                        out.extend_from_slice(part);
                                        out.extend_from_slice(b"\r\n");
                                    }
                                    if !o.sse_hold {
                                        out.extend_from_slice(b"0\r\n\r\n");
                                    }
                                    let _ = stream.write_all(&out);
                                } else {
                                    let _ = stream.write_all(script.as_bytes());
                                }
                                let _ = stream.flush();
                            }
                            if o.sse_hold {
                                while !st.load(Ordering::SeqCst) {
                                    std::thread::sleep(Duration::from_millis(50));
                                }
                            }
                            let _ = stream.flush();
                        }
                        ("POST", "/session") => respond(
                            &mut stream,
                            "201 Created",
                            "application/json",
                            r#"{"id":"ses_created1","slug":"fresh","projectID":"global","title":"Fresh","time":{"created":1700000009000,"updated":1700000009000}}"#,
                        ),
                        ("POST", p) if p.ends_with("/prompt_async") => {
                            if o.html_prompt {
                                respond(
                                    &mut stream,
                                    "200 OK",
                                    "text/html; charset=utf-8",
                                    "<html><body>SPA fallback</body></html>",
                                );
                            } else {
                                respond(&mut stream, "204 No Content", "", "")
                            }
                        }
                        ("POST", p) if p.ends_with("/abort") => {
                            respond(&mut stream, "200 OK", "application/json", "true")
                        }
                        // ADR-021: a slash command answers its assistant
                        // message once the turn ends; "broken" is refused
                        ("POST", p) if p.ends_with("/command") => {
                            if body.contains(r#""command":"broken""#) {
                                respond(
                                    &mut stream,
                                    "400 Bad Request",
                                    "application/json",
                                    r#"{"error":"unknown command"}"#,
                                )
                            } else {
                                respond(&mut stream, "200 OK", "application/json", "{}")
                            }
                        }
                        ("POST", p) if p.starts_with("/question/") => {
                            respond(&mut stream, "200 OK", "application/json", "true")
                        }
                        ("POST", p) if p.contains("/permission/") && p.ends_with("/reply") => {
                            if p.contains("per_missing") {
                                respond(
                                    &mut stream,
                                    "404 Not Found",
                                    "application/json",
                                    r#"{"error":"PermissionNotFoundError"}"#,
                                );
                            } else {
                                respond(&mut stream, "200 OK", "application/json", "true")
                            }
                        }
                        // The live server's SPA fallback: 200 + HTML for
                        // unknown routes — G4.2's origin story.
                        _ => respond(
                            &mut stream,
                            "200 OK",
                            "text/html; charset=utf-8",
                            "<html><body>web ui</body></html>",
                        ),
                    }
                });
            }
        });
        Fake {
            addr,
            requests,
            sse_conns,
            stop,
            handle: Some(handle),
        }
    }

    fn adapter(f: &Fake) -> OpenCodeAdapter {
        OpenCodeAdapter::new("oc-1", &f.addr, None)
    }

    fn submit(ad: &OpenCodeAdapter, cmd: &str) -> ServerMsg {
        ad.send(&ClientMsg::SubmitPrompt {
            command_id: cmd.into(),
            session_id: "ses_test1".into(),
            text: "hello".into(),
            images: vec![],
            fork: false,
        })
        .unwrap()
    }

    #[test]
    fn probes_health_lists_sessions_and_agents() {
        let f = start(Opts::default());
        let ad = adapter(&f);
        ad.refresh().unwrap();
        assert!(ad.is_healthy());
        assert_eq!(ad.version(), "1.17.7");
        let sessions = ad.sessions();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "ses_test1");
        assert_eq!(s.host_id, "oc-1");
        assert_eq!(s.agent_id, "build");
        assert_eq!(s.title, "Demo session");
        assert!(s.project.ends_with("demo"));
        assert_eq!(s.status, "completed"); // unobserved default
        assert_eq!(s.created_at, 1700000000000);
        let agents = ad.agents();
        assert_eq!(agents.len(), 2);
        let build = agents.iter().find(|a| a.name == "build").unwrap();
        assert!(build.accepts_new_session);
        assert_eq!(build.state, "online");
        assert_eq!(build.kind, "opencode");
        let explore = agents.iter().find(|a| a.name == "explore").unwrap();
        assert!(!explore.accepts_new_session);
        assert_eq!(explore.capabilities, vec!["subagent".to_string()]);
        // ADR-009: a real runtime's agents never carry the demo marker
        for a in &agents {
            assert!(!json::stringify(&a.to_json()).contains("simulated"));
        }
    }

    #[test]
    fn html_200_answer_is_rejected_not_treated_as_success() {
        // G4.2: unknown/odd routes answer the web UI with HTTP 200.
        let f = start(Opts {
            html_prompt: true,
            ..Default::default()
        });
        let ad = adapter(&f);
        let err = ad
            .send(&ClientMsg::SubmitPrompt {
                command_id: "cmd-1".into(),
                session_id: "ses_test1".into(),
                text: "hi".into(),
                images: vec![],
                fork: false,
            })
            .unwrap_err();
        assert_eq!(err.code(), "invalid-response");
        assert!(err.to_string().contains("text/html"), "{err}");
        // and the health probe's HTML trap is covered by the SPA fallback:
        assert!(!f.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn prompt_acceptance_tracks_pending_and_session_error_fails_the_command() {
        // G4.1: 204 is acceptance; the later session.error must fail the
        // ORIGINAL commandId.
        let script = concat!(
            r#"data: {"payload":{"id":"evt_e1","type":"session.error","properties":{"sessionID":"ses_test1","error":{"name":"UnknownError","data":{"message":"violates foreign key constraint"}}}}}"#,
            "\n\n"
        );
        let f = start(Opts {
            sse: Some(script.to_string()),
            ..Default::default()
        });
        let ad = adapter(&f);
        let ack_msg = submit(&ad, "cmd-7");
        match &ack_msg {
            ServerMsg::Ack { ok, session_id, .. } => {
                assert!(ok);
                assert_eq!(session_id.as_deref(), Some("ses_test1"));
            }
            other => panic!("expected ack, got {other:?}"),
        }
        let (tx, rx) = mpsc::channel();
        ad.subscribe(move |k| {
            let _ = tx.send(k);
        });
        ad.start_events().unwrap();
        let kind = rx.recv_timeout(Duration::from_secs(5)).expect("event");
        match kind {
            EventKind::CommandResult {
                command_id,
                status,
                error,
                session_id,
                permission_id,
                ..
            } => {
                assert_eq!(command_id, "cmd-7");
                assert_eq!(status, "failed");
                assert!(error.unwrap().contains("foreign key constraint"));
                assert_eq!(session_id.as_deref(), Some("ses_test1"));
                assert!(permission_id.is_none());
            }
            other => panic!("expected command_result, got {other:?}"),
        }
        ad.dispose();
    }

    #[test]
    fn session_lifecycle_events_normalize() {
        let script = concat!(
            r#"data: {"payload":{"id":"evt_c","type":"session.created","properties":{"sessionID":"ses_new","info":{"id":"ses_new","slug":"new-one","projectID":"global","directory":"C:\\proj\\x","title":"New","time":{"created":1700000001000,"updated":1700000002000}}}}}"#,
            "\n\n",
            r#"data: {"payload":{"id":"evt_s","type":"session.status","properties":{"sessionID":"ses_new","status":{"type":"busy"}}}}"#,
            "\n\n",
            r#"data: {"payload":{"id":"evt_i","type":"session.idle","properties":{"sessionID":"ses_new"}}}"#,
            "\n\n"
        );
        let f = start(Opts {
            sse: Some(script.to_string()),
            ..Default::default()
        });
        let ad = adapter(&f);
        let (tx, rx) = mpsc::channel();
        ad.subscribe(move |k| {
            let _ = tx.send(k);
        });
        ad.start_events().unwrap();
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(rx.recv_timeout(Duration::from_secs(5)).expect("event"));
        }
        match &seen[0] {
            EventKind::SessionCreated(s) => {
                assert_eq!(s.id, "ses_new");
                assert_eq!(s.title, "New");
                assert_eq!(s.status, "completed");
            }
            other => panic!("expected session_created, got {other:?}"),
        }
        match &seen[1] {
            EventKind::SessionUpdated(s) => assert_eq!(s.status, "running"),
            other => panic!("expected session_updated(running), got {other:?}"),
        }
        match &seen[2] {
            EventKind::SessionUpdated(s) => assert_eq!(s.status, "completed"),
            other => panic!("expected session_updated(completed), got {other:?}"),
        }
        // the upserted session reflects the tracked status (idle last)
        assert_eq!(
            ad.sessions()
                .iter()
                .find(|s| s.id == "ses_new")
                .map(|s| s.status.as_str()),
            Some("completed")
        );
        ad.dispose();
    }

    #[test]
    fn permission_asked_and_replied_normalize_roundtrip() {
        let script = concat!(
            r#"data: {"payload":{"id":"evt_pa","type":"permission.asked","properties":{"id":"per_123","sessionID":"ses_test1","permission":"bash","patterns":["git status"],"tool":{"messageID":"msg_1","callID":"c1"}}}}"#,
            "\n\n",
            r#"data: {"payload":{"id":"evt_pr","type":"permission.replied","properties":{"id":"per_123","reply":"once"}}}"#,
            "\n\n"
        );
        let f = start(Opts {
            sse: Some(script.to_string()),
            ..Default::default()
        });
        let ad = adapter(&f);
        let (tx, rx) = mpsc::channel();
        ad.subscribe(move |k| {
            let _ = tx.send(k);
        });
        ad.start_events().unwrap();
        let asked = rx.recv_timeout(Duration::from_secs(5)).expect("asked");
        match asked {
            EventKind::PermissionRequested(p) => {
                assert_eq!(p.id, "per_123");
                assert_eq!(p.native_request_id, "per_123");
                assert_eq!(p.action, "shell");
                assert_eq!(p.command, "bash git status");
                assert_eq!(p.revision, 1);
                assert_eq!(p.status, "pending");
                assert_eq!(p.supported_decisions.len(), 3);
                assert_eq!(p.agent_id, "build");
            }
            other => panic!("expected permission_requested, got {other:?}"),
        }
        let replied = rx.recv_timeout(Duration::from_secs(5)).expect("replied");
        match replied {
            EventKind::PermissionResolved(p) => {
                assert_eq!(p.status, "approved");
                assert_eq!(p.decision.as_deref(), Some("approve-once"));
                assert!(p.decided_at.is_some());
            }
            other => panic!("expected permission_resolved, got {other:?}"),
        }
        ad.dispose();
    }

    #[test]
    fn permission_decisions_map_to_native_replies() {
        let f = start(Opts::default());
        let ad = adapter(&f);
        for (decision, want) in [
            ("approve-once", "\"reply\":\"once\""),
            ("approve-session", "\"reply\":\"always\""),
            ("reject", "\"reply\":\"reject\""),
        ] {
            let msg = ad
                .send(&ClientMsg::PermissionDecision {
                    command_id: "cmd-p".into(),
                    permission_id: "per_1".into(),
                    decision: decision.into(),
                    revision: 1.0,
                    option_id: None,
                    message: None,
                    answers: vec![],
                })
                .unwrap();
            assert!(
                matches!(&msg, ServerMsg::Ack { ok: true, .. }),
                "{decision}"
            );
            assert!(
                f.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.contains("/permission/per_1/reply") && r.contains(want)),
                "missing {want} for {decision}"
            );
        }
        // 404 = stale/unknown request → domain refusal, never success
        let msg = ad
            .send(&ClientMsg::PermissionDecision {
                command_id: "cmd-m".into(),
                permission_id: "per_missing".into(),
                decision: "approve-once".into(),
                revision: 1.0,
                option_id: None,
                message: None,
                answers: vec![],
            })
            .unwrap();
        match msg {
            ServerMsg::Ack { ok, error, .. } => {
                assert!(!ok);
                assert!(error.unwrap().contains("not found"));
            }
            other => panic!("expected refusal ack, got {other:?}"),
        }
    }

    #[test]
    fn create_session_refuses_a_phone_chosen_directory_and_rides_the_first_prompt() {
        let f = start(Opts::default());
        let ad = adapter(&f);
        let refused = ad
            .send(&ClientMsg::CreateSession {
                command_id: "cmd-d".into(),
                agent_id: "build".into(),
                title: "Fresh".into(),
                prompt: "first turn".into(),
                project: Some(r"C:\Users".into()),
                images: vec![],
                model: None,
            })
            .unwrap();
        assert!(
            matches!(refused, ServerMsg::Ack { ok: false, .. }),
            "{refused:?}"
        );
        assert!(
            f.requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| !r.starts_with("POST /session"))
        );
        let msg = ad
            .send(&ClientMsg::CreateSession {
                command_id: "cmd-c".into(),
                agent_id: "build".into(),
                title: "Fresh".into(),
                prompt: "first turn".into(),
                project: None,
                images: vec![],
                model: None,
            })
            .unwrap();
        match msg {
            ServerMsg::Ack { ok, session_id, .. } => {
                assert!(ok);
                assert_eq!(session_id.as_deref(), Some("ses_created1"));
            }
            other => panic!("expected ack, got {other:?}"),
        }
        let reqs = f.requests.lock().unwrap();
        assert!(reqs.iter().any(|r| r.starts_with("POST /session ")),);
        assert!(
            reqs.iter()
                .any(|r| r.contains("prompt_async") && r.contains("first turn"))
        );
        drop(reqs);
        // the created session is visible without waiting for SSE
        assert_eq!(
            ad.sessions()
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["ses_created1"]
        );
    }

    #[test]
    fn session_neutral_messages_are_refused_as_unsupported() {
        let f = start(Opts::default());
        let ad = adapter(&f);
        for msg in [
            ClientMsg::Hello {
                client_id: "c".into(),
                protocol: 2.0,
            },
            ClientMsg::Ping {
                command_id: "cmd-1".into(),
            },
            ClientMsg::ListState {
                command_id: "cmd-2".into(),
            },
        ] {
            let err = ad.send(&msg).unwrap_err();
            assert_eq!(err.code(), "unsupported");
        }
    }

    #[test]
    fn down_server_is_runtime_unavailable_not_a_crash() {
        // bind then drop → refused port
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let ad = OpenCodeAdapter::new("oc-down", &format!("http://127.0.0.1:{port}"), None);
        let err = ad.refresh().unwrap_err();
        assert_eq!(err.code(), "runtime-unavailable");
        assert!(!ad.is_healthy());
        assert!(ad.detail().unwrap().contains("runtime-unavailable"));
        let err = ad
            .send(&ClientMsg::SubmitPrompt {
                command_id: "cmd-x".into(),
                session_id: "s".into(),
                text: "t".into(),
                images: vec![],
                fork: false,
            })
            .unwrap_err();
        assert_eq!(err.code(), "runtime-unavailable");
    }

    #[test]
    fn event_stream_reconnects_after_drop() {
        // server closes each SSE after the script → adapter must come back
        let script = concat!(
            r#"data: {"payload":{"id":"evt_r","type":"session.created","properties":{"sessionID":"ses_r","info":{"id":"ses_r","title":"R","time":{"created":1,"updated":2}}}}}"#,
            "\n\n"
        );
        let f = start(Opts {
            sse: Some(script.to_string()),
            sse_hold: false,
            ..Default::default()
        });
        let ad = adapter(&f);
        ad.start_events().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        while f.sse_conns.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            f.sse_conns.load(Ordering::SeqCst) >= 2,
            "adapter did not reconnect (conns={})",
            f.sse_conns.load(Ordering::SeqCst)
        );
        ad.dispose();
    }

    #[test]
    fn chunked_event_stream_parses_the_same() {
        let script = concat!(
            r#"data: {"payload":{"id":"evt_k","type":"session.created","properties":{"sessionID":"ses_k","info":{"id":"ses_k","title":"K","time":{"created":1,"updated":2}}}}}"#,
            "\n\n"
        );
        let f = start(Opts {
            sse: Some(script.to_string()),
            chunked_sse: true,
            ..Default::default()
        });
        let ad = adapter(&f);
        let (tx, rx) = mpsc::channel();
        ad.subscribe(move |k| {
            let _ = tx.send(k);
        });
        ad.start_events().unwrap();
        let kind = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|e| {
            panic!(
                "chunked event: {e}; conns={}; detail={:?}",
                f.sse_conns.load(Ordering::SeqCst),
                ad.detail()
            )
        });
        match kind {
            EventKind::SessionCreated(s) => assert_eq!(s.id, "ses_k"),
            other => panic!("expected session_created, got {other:?}"),
        }
        ad.dispose();
    }

    #[test]
    fn capabilities_are_structurally_honest() {
        let f = start(Opts::default());
        let ad = adapter(&f);
        let caps = ad.capabilities();
        let mut ids: Vec<_> = caps.iter().map(|c| c.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), caps.len(), "capability ids must be unique");
        assert!(
            caps.iter().any(|c| c.status != "available"),
            "must declare at least one non-available entry"
        );
        for c in &caps {
            if c.status != "available" {
                assert!(
                    c.detail.is_some() && !c.detail.as_deref().unwrap().is_empty(),
                    "{} needs a detail",
                    c.id
                );
            }
        }
        // domain status set only
        for c in &caps {
            assert!(matches!(
                c.status.as_str(),
                "available" | "unavailable" | "limited" | "dependent-config"
            ));
        }
    }

    #[test]
    fn diff_and_messages_validate_responses() {
        let f = start(Opts::default());
        let ad = adapter(&f);
        assert!(ad.diff("ses_test1").is_ok());
        assert!(ad.messages("ses_test1").is_ok());
        let err = ad
            .send(&ClientMsg::Interrupt {
                command_id: "cmd-i".into(),
                session_id: "ses_test1".into(),
            })
            .unwrap();
        match err {
            ServerMsg::Ack { ok, .. } => assert!(ok),
            other => panic!("expected interrupt ack, got {other:?}"),
        }
    }

    #[test]
    fn unsubscribe_stops_delivery() {
        let f = start(Opts::default());
        let ad = adapter(&f);
        let hits = Arc::new(AtomicU64::new(0));
        let h = hits.clone();
        let id = ad.subscribe(move |_| {
            h.fetch_add(1, Ordering::SeqCst);
        });
        ad.unsubscribe(id);
        ad.inner.on_sse_data(
            r#"{"payload":{"id":"e","type":"session.created","properties":{"sessionID":"s","info":{"id":"s","title":"t","time":{"created":1,"updated":2}}}}}"#,
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn reconnect_delay_is_jittered_growing_and_bounded() {
        for attempt in 0..64 {
            for r in [0, 1, 249, 250, 999, 12_345, u64::MAX / 3, u64::MAX] {
                let d = reconnect_delay_ms(attempt, r);
                let ceiling = [500, 1000, 1250][attempt.min(2) as usize];
                assert!(
                    (250..=ceiling).contains(&d),
                    "attempt {attempt} rand {r}: {d}"
                );
            }
        }
        // jitter: different seeds, different delays (no lockstep)
        let spread: std::collections::BTreeSet<u64> =
            (0..100).map(|r| reconnect_delay_ms(2, r * 7_919)).collect();
        assert!(spread.len() > 50);
        // the ceiling (rand = ceiling - 250) grows with the attempt, then caps
        assert_eq!(reconnect_delay_ms(0, 250), 500);
        assert_eq!(reconnect_delay_ms(1, 750), 1000);
        assert_eq!(reconnect_delay_ms(9, 1000), 1250);
    }

    #[test]
    fn oversize_sse_line_and_event_are_dropped_without_unbounded_memory() {
        let ev1 = r#"data: {"payload":{"id":"e1","type":"session.created","properties":{"sessionID":"ses_o1","info":{"id":"ses_o1","title":"O1","time":{"created":1,"updated":2}}}}}"#;
        let big_line = format!("data: {}\n\n", "x".repeat(MAX_SSE_LINE_BYTES + 2048));
        let ev2 = r#"data: {"payload":{"id":"e2","type":"session.created","properties":{"sessionID":"ses_o2","info":{"id":"ses_o2","title":"O2","time":{"created":1,"updated":2}}}}}"#;
        let big_payload = format!(
            "data: {}\ndata: {}\n\n",
            "y".repeat(600 * 1024),
            "z".repeat(600 * 1024)
        );
        let ev3 = r#"data: {"payload":{"id":"e3","type":"session.created","properties":{"sessionID":"ses_o3","info":{"id":"ses_o3","title":"O3","time":{"created":1,"updated":2}}}}}"#;
        let script = format!("{ev1}\n\n{big_line}{ev2}\n\n{big_payload}{ev3}\n\n");

        let f = start(Opts {
            sse: Some(script),
            ..Default::default()
        });
        let ad = adapter(&f);
        let (tx, rx) = mpsc::channel();
        ad.subscribe(move |k| {
            let _ = tx.send(k);
        });
        ad.start_events().unwrap();

        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(rx.recv_timeout(Duration::from_secs(5)).expect("event"));
        }
        assert_eq!(seen.len(), 3);
        match &seen[0] {
            EventKind::SessionCreated(s) => assert_eq!(s.id, "ses_o1"),
            other => panic!("expected session_created(ses_o1), got {other:?}"),
        }
        match &seen[1] {
            EventKind::SessionCreated(s) => assert_eq!(s.id, "ses_o2"),
            other => panic!("expected session_created(ses_o2), got {other:?}"),
        }
        match &seen[2] {
            EventKind::SessionCreated(s) => assert_eq!(s.id, "ses_o3"),
            other => panic!("expected session_created(ses_o3), got {other:?}"),
        }
        assert!(
            ad.detail()
                .unwrap_or_default()
                .contains("dropped oversize event"),
            "detail was: {:?}",
            ad.detail()
        );
        ad.dispose();
    }

    #[test]
    fn oversize_http_body_is_rejected_as_invalid_response() {
        let f = start(Opts {
            oversize_http: true,
            ..Default::default()
        });
        let ad = adapter(&f);
        let err = ad.refresh().unwrap_err();
        // flaked once under a loaded full run as "unknown": print the cause
        assert_eq!(err.code(), "invalid-response", "{err}");
        assert!(!ad.is_healthy());
        assert!(ad.detail().unwrap().contains("byte limit"));
    }

    #[test]
    fn stalled_sse_stream_times_out_and_reconnects() {
        let f = start(Opts {
            sse: None,
            sse_hold: true,
            ..Default::default()
        });
        let ad = adapter(&f);
        ad.set_sse_read_timeout(Duration::from_millis(100));
        ad.start_events().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while f.sse_conns.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            f.sse_conns.load(Ordering::SeqCst) >= 2,
            "stalled stream did not trigger reconnect (conns={})",
            f.sse_conns.load(Ordering::SeqCst)
        );
        ad.dispose();
    }

    #[test]
    fn malformed_sse_data_is_ignored_with_counter_and_detail_no_panic() {
        let script = concat!(
            "data: {not json\n\n",
            "data: {\"missing\":\"payload\"}\n\n",
            "data: {\"payload\":{\"missing\":\"type\"}}\n\n",
            "data: {\"payload\":{\"id\":\"evt_ok\",\"type\":\"session.created\",\"properties\":{\"sessionID\":\"ses_mal1\",\"info\":{\"id\":\"ses_mal1\",\"title\":\"M\",\"time\":{\"created\":1,\"updated\":2}}}}}\n\n",
        );
        let f = start(Opts {
            sse: Some(script.to_string()),
            ..Default::default()
        });
        let ad = adapter(&f);
        let (tx, rx) = mpsc::channel();
        ad.subscribe(move |k| {
            let _ = tx.send(k);
        });
        ad.start_events().unwrap();
        let kind = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("event arrived");
        match kind {
            EventKind::SessionCreated(s) => assert_eq!(s.id, "ses_mal1"),
            other => panic!("expected session_created, got {other:?}"),
        }
        assert_eq!(ad.malformed_events(), 3);
        assert!(
            ad.detail()
                .unwrap_or_default()
                .contains("ignored malformed SSE event"),
            "detail was: {:?}",
            ad.detail()
        );
        ad.dispose();
    }

    #[test]
    fn crash_midstream_goes_unhealthy_and_recovers_on_restart_single_thread() {
        let f1 = start(Opts::default());
        let port = f1.port();
        let ad = adapter(&f1);
        let hits = Arc::new(AtomicU64::new(0));
        let h = hits.clone();
        ad.subscribe(move |_| {
            h.fetch_add(1, Ordering::SeqCst);
        });
        ad.start_events().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while !ad.is_healthy() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            ad.is_healthy(),
            "adapter failed to connect to initial server"
        );

        // Crash: drop fake server mid-stream
        drop(f1);

        // Adapter must detect failure and mark itself unhealthy
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while ad.is_healthy() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !ad.is_healthy(),
            "adapter must become unhealthy when server dies"
        );
        assert!(
            ad.detail().unwrap_or_default().contains("event stream"),
            "detail was: {:?}",
            ad.detail()
        );

        // Duplicate start_events is a no-op (no duplicate sse_loop thread)
        assert!(ad.start_events().is_ok());

        // Restart fake server on the same port
        let script = concat!(
            r#"data: {"payload":{"id":"evt_rec","type":"session.created","properties":{"sessionID":"ses_rec","info":{"id":"ses_rec","title":"R","time":{"created":1,"updated":2}}}}}"#,
            "\n\n"
        );
        let f2 = start_on_port(
            port,
            Opts {
                sse: Some(script.to_string()),
                ..Default::default()
            },
        );

        // Adapter recovers to healthy state
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        while !ad.is_healthy() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ad.is_healthy(), "adapter did not recover after restart");

        // Exactly one event delivery (single thread, no duplicate subscription)
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while hits.load(Ordering::SeqCst) < 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        ad.dispose();
        drop(f2);
    }

    #[test]
    fn credentials_in_endpoint_are_never_leaked_in_detail_or_errors() {
        let ad = OpenCodeAdapter::new("oc-sec", "http://user:supersecretpass@127.0.0.1:1", None);
        let err = ad.refresh().unwrap_err();
        assert!(!err.to_string().contains("supersecretpass"));
        assert!(!err.to_string().contains("user:"));
        assert!(!ad.detail().unwrap().contains("supersecretpass"));
        assert!(!ad.detail().unwrap().contains("user:"));
    }

    /* ---------------- ADR-015 step 5: rich parts ---------------- */
    // Scripts: sanitized `/global/event` captures from an isolated
    // `opencode serve` 1.19.4 on local Ollama (`tests/fixtures/opencode/`),
    // fed to the normalizer in order. `retry.sse` carries the live retry
    // shape recorded in `opencode-live-events.md` §2.6.

    use crate::transcript::Hub;

    /// The captured session.
    pub const SID: &str = "ses_e590d63d1ffeINRRRmnvST2KP5";
    pub const APPROVE: &str = include_str!("../tests/fixtures/opencode/bash-approve.sse");
    const REJECT: &str = include_str!("../tests/fixtures/opencode/bash-reject.sse");
    const TODO: &str = include_str!("../tests/fixtures/opencode/todo.sse");
    const ABORT: &str = include_str!("../tests/fixtures/opencode/abort.sse");
    const RETRY: &str = include_str!("../tests/fixtures/opencode/retry.sse");
    /// A second capture: an edit approved inline, then the question tool.
    const EDIT_SID: &str = "ses_e58fe4c49ffeLOPOlJuistwhl7";
    const EDIT: &str = include_str!("../tests/fixtures/opencode/edit.sse");
    const QUESTION_SID: &str = EDIT_SID;
    const QUESTION: &str = include_str!("../tests/fixtures/opencode/question.sse");
    const MESSAGES: &str = include_str!("../tests/fixtures/opencode/messages.json");
    const MCP: &str = include_str!("../tests/fixtures/opencode/mcp.json");

    /// An adapter on a fake server, streaming into a hub where one
    /// connection watches `sid` (what the companion's subscribe sets up).
    struct Rig {
        f: Fake,
        ad: OpenCodeAdapter,
        hub: HubHandle,
        rx: mpsc::Receiver<EventKind>,
        _out: mpsc::Receiver<(crate::server::MsgClass, Vec<u8>)>,
    }

    fn rig(opts: Opts, sid: &str) -> Rig {
        rig_in(opts, sid, None)
    }

    /// `rig` with the companion's data directory (ADR-021 workspace rule).
    fn rig_in(opts: Opts, sid: &str, data_dir: Option<&Path>) -> Rig {
        let f = start(opts);
        let shared = Arc::new(Mutex::new(Hub::default()));
        let (out, rx_out) =
            crate::server::Outbound::new(Arc::new(crate::companion::Saturation::default()), false);
        {
            let mut hub = shared.lock().unwrap();
            hub.ensure(sid, now_ms());
            hub.subscribe(1, out, sid, None).unwrap();
        }
        let hub = HubHandle::new(shared);
        let ad = OpenCodeAdapter::with_hub("oc-1", &f.addr, None, hub.clone(), data_dir);
        let (tx, rx) = mpsc::channel();
        ad.subscribe(move |k| {
            let _ = tx.send(k);
        });
        Rig {
            f,
            ad,
            hub,
            rx,
            _out: rx_out,
        }
    }

    impl Rig {
        fn feed(&self, script: &str) {
            for line in script.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    self.ad.inner.on_sse_data(data);
                }
            }
        }
        /// The transcript ops so far (deltas flushed), parsed.
        fn ops(&self, sid: &str) -> Vec<Json> {
            self.hub
                .ops(sid)
                .iter()
                .map(|o| json::parse(o).unwrap())
                .collect()
        }
        fn events(&self) -> Vec<EventKind> {
            self.rx.try_iter().collect()
        }
        fn requests(&self, needle: &str) -> Vec<String> {
            let reqs = self.f.requests.lock().unwrap();
            reqs.iter()
                .filter(|r| r.contains(needle))
                .cloned()
                .collect()
        }
        fn decide(
            &self,
            permission: &str,
            option: &str,
            message: Option<&str>,
            answers: Vec<(String, Vec<String>)>,
        ) -> ServerMsg {
            self.ad
                .send(&ClientMsg::PermissionDecision {
                    command_id: format!("cmd-{option}"),
                    permission_id: permission.into(),
                    decision: if option == "reject" || option == "skip" {
                        "reject"
                    } else {
                        "approve-once"
                    }
                    .into(),
                    revision: 1.0,
                    option_id: Some(option.into()),
                    message: message.map(str::to_string),
                    answers,
                })
                .unwrap()
        }
    }

    /// `script` up to (and from) the event line containing `marker`.
    fn split<'a>(script: &'a str, marker: &str) -> (&'a str, &'a str) {
        let at = script.find(marker).expect("marker in script");
        script.split_at(script[..at].rfind("data: ").unwrap())
    }

    /// The item ops for `id`, oldest first.
    fn items<'a>(ops: &'a [Json], id: &str) -> Vec<&'a Json> {
        ops.iter()
            .filter(|o| json::get_str(o, "op") == Some("item"))
            .filter_map(|o| json::get(o, "item"))
            .filter(|i| json::get_str(i, "id") == Some(id))
            .collect()
    }

    fn tool_statuses(ops: &[Json], call: &str) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for item in items(ops, call) {
            let s = json::get(item, "tool")
                .and_then(|t| json::get_str(t, "status"))
                .unwrap()
                .to_string();
            if seen.last() != Some(&s) {
                seen.push(s);
            }
        }
        seen
    }

    fn turn<'a>(ops: &'a [Json], id: &str) -> Option<&'a Json> {
        ops.iter()
            .rev()
            .filter(|o| json::get_str(o, "op") == Some("turn"))
            .filter_map(|o| json::get(o, "turn"))
            .find(|t| json::get_str(t, "id") == Some(id))
    }

    fn results(events: &[EventKind]) -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|e| match e {
                EventKind::CommandResult {
                    command_id, status, ..
                } => Some((command_id.clone(), status.clone())),
                _ => None,
            })
            .collect()
    }

    fn submit_to(ad: &OpenCodeAdapter, sid: &str, cmd: &str) {
        let ack = ad
            .send(&ClientMsg::SubmitPrompt {
                command_id: cmd.into(),
                session_id: sid.into(),
                text: "hello".into(),
                images: vec![],
                fork: false,
            })
            .unwrap();
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }));
    }

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn text_and_reasoning_deltas_become_appends() {
        let r = rig(Opts::default(), SID);
        r.feed(APPROVE);
        let ops = r.ops(SID);
        // reasoning: created empty, grown by appends, replaced whole at its end
        let think = "prt_1a6f3caca001Duu7ANPZATPIYE";
        let versions = items(&ops, think);
        assert_eq!(json::get_str(versions[0], "kind"), Some("thinking"));
        assert_eq!(json::get_str(versions[0], "text"), Some(""));
        assert_eq!(json::get_bool(versions[0], "done"), Some(false));
        let last = versions.last().unwrap();
        assert_eq!(json::get_bool(last, "done"), Some(true));
        let full = json::get_str(last, "text").unwrap();
        assert!(full.starts_with("The user wants me to run"), "{full}");
        let appended: String = ops
            .iter()
            .filter(|o| {
                json::get_str(o, "op") == Some("append") && json::get_str(o, "id") == Some(think)
            })
            .map(|o| {
                assert_eq!(json::get_str(o, "field"), Some("text"));
                json::get_str(o, "text").unwrap()
            })
            .collect();
        assert_eq!(appended, full, "the deltas rebuild the final text");
        // the answer streams the same way, as a text item of its turn
        let done: Vec<&Json> = ops
            .iter()
            .filter_map(|o| json::get(o, "item"))
            .filter(|i| json::get_str(i, "kind") == Some("text"))
            .collect();
        let answer = done.last().unwrap();
        assert_eq!(json::get_str(answer, "text"), Some("DONE"));
        assert_eq!(
            json::get_str(answer, "turnId"),
            Some("msg_1a6f29c8f001OaOj6wOaB61V5O")
        );
        assert!(ops.iter().any(|o| json::get_str(o, "op") == Some("append")
            && json::get_str(o, "text") == Some("DONE")));
    }

    #[test]
    fn tool_part_lifecycle_maps_to_tool_item() {
        let r = rig(Opts::default(), SID);
        r.feed(APPROVE);
        r.feed(REJECT);
        let ops = r.ops(SID);
        // approved: the card waits inside its own lifecycle, then completes
        assert_eq!(
            tool_statuses(&ops, "call_22tnronp"),
            [
                "pending",
                "running",
                "awaiting-approval",
                "running",
                "completed"
            ]
        );
        let done = items(&ops, "call_22tnronp").pop().unwrap();
        let tool = json::get(done, "tool").unwrap();
        assert_eq!(json::get_str(tool, "name"), Some("bash"));
        assert_eq!(json::get_str(tool, "title"), Some("echo hi > note.txt"));
        assert_eq!(json::get_str(tool, "input"), Some("echo hi > note.txt"));
        assert_eq!(json::get_str(tool, "output"), Some("(no output)"));
        assert_eq!(
            json::get_str(tool, "permissionId"),
            Some("per_1a6f3f4040012zUvvUmJzvZF6K")
        );
        assert!(json::get_i64(tool, "durationMs").is_some());
        // rejected (asked while still pending): the error is a denial
        assert_eq!(
            tool_statuses(&ops, "call_mncm01jw"),
            ["pending", "awaiting-approval", "denied"]
        );
        let denied = items(&ops, "call_mncm01jw").pop().unwrap();
        let error = json::get(denied, "tool")
            .and_then(|t| json::get_str(t, "error"))
            .unwrap();
        assert!(error.contains("rejected permission"), "{error}");
    }

    #[test]
    fn permission_links_to_tool_by_call_id_with_diff() {
        let r = rig(Opts::default(), EDIT_SID);
        r.feed(EDIT);
        let asked = r
            .events()
            .into_iter()
            .find_map(|e| match e {
                EventKind::PermissionRequested(p) => Some(p),
                _ => None,
            })
            .expect("permission_requested");
        let call = asked.tool_item_id.clone().expect("toolItemId");
        assert_eq!(asked.action, "file-write");
        let detail = asked.detail.as_ref().unwrap();
        assert_eq!(json::get_str(detail, "kind"), Some("edit"));
        assert!(
            json::get_str(detail, "path")
                .unwrap()
                .ends_with("hello.txt")
        );
        assert!(
            json::get_str(detail, "patch")
                .unwrap()
                .contains("+hello world")
        );
        let options: Vec<(&str, &str, &str)> = asked
            .options
            .iter()
            .map(|o| (o.id.as_str(), o.decision.as_str(), o.scope))
            .collect();
        assert_eq!(
            options,
            [
                ("once", "approve-once", "once"),
                ("always", "approve-session", "session"),
                ("reject", "reject", "once")
            ]
        );
        assert!(asked.options[1].label.ends_with("until OpenCode restarts"));
        assert!(asked.options[2].takes_message);
        // the card with that callID waited for this very request, and the
        // completed edit carries the diff
        let ops = r.ops(EDIT_SID);
        let waiting = items(&ops, &call)
            .into_iter()
            .map(|i| json::get(i, "tool").unwrap())
            .find(|t| json::get_str(t, "status") == Some("awaiting-approval"))
            .expect("awaiting-approval");
        assert_eq!(
            json::get_str(waiting, "permissionId"),
            Some(asked.id.as_str())
        );
        let done = items(&ops, &call).pop().unwrap();
        let tool = json::get(done, "tool").unwrap();
        assert_eq!(json::get_str(tool, "status"), Some("completed"));
        let diffs = json::get(tool, "diffs")
            .and_then(json::as_arr)
            .expect("diffs");
        assert!(
            json::get_str(&diffs[0], "patch")
                .unwrap()
                .contains("+hello world")
        );
        assert_eq!(json::get_i64(&diffs[0], "additions"), Some(1));
    }

    #[test]
    fn reject_with_message_reaches_reply_route() {
        let r = rig(Opts::default(), SID);
        let (asked, rest) = split(REJECT, "permission.replied");
        r.feed(asked);
        let pid = "per_1a6f433780014gQkcE3qRjTXtd";
        assert!(r.ad.knows_permission(pid));
        let ack = r.decide(
            pid,
            "reject",
            Some("Not now - phone user declined."),
            vec![],
        );
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }), "{ack:?}");
        let sent = r.requests(&format!("/permission/{pid}/reply"));
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0].ends_with(r#"{"reply":"reject","message":"Not now - phone user declined."}"#),
            "{}",
            sent[0]
        );
        assert!(!r.ad.knows_permission(pid));
        // our own reply: the echo resolves nothing twice, the card is denied
        r.events();
        r.feed(rest);
        assert!(
            !r.events()
                .iter()
                .any(|e| matches!(e, EventKind::PermissionResolved(_)))
        );
        assert_eq!(
            tool_statuses(&r.ops(SID), "call_mncm01jw").last().unwrap(),
            "denied"
        );
    }

    #[test]
    fn question_round_trip() {
        let r = rig(Opts::default(), QUESTION_SID);
        let (asked, rest) = split(QUESTION, "question.replied");
        r.feed(asked);
        let q = r
            .events()
            .into_iter()
            .find_map(|e| match e {
                EventKind::PermissionRequested(p) => Some(p),
                _ => None,
            })
            .expect("question asked");
        assert!(q.id.starts_with("que_"));
        let detail = q.detail.as_ref().unwrap();
        assert_eq!(json::get_str(detail, "kind"), Some("question"));
        let first = &json::get(detail, "questions")
            .and_then(json::as_arr)
            .unwrap()[0];
        assert_eq!(json::get_str(first, "id"), Some("0"));
        let labels: Vec<&str> = json::get(first, "options")
            .and_then(json::as_arr)
            .unwrap()
            .iter()
            .filter_map(|o| json::get_str(o, "label"))
            .collect();
        assert!(
            labels.contains(&"tabs") && labels.contains(&"spaces"),
            "{labels:?}"
        );
        assert_eq!(
            q.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
            ["answer", "skip"]
        );
        let call = q.tool_item_id.clone().expect("question tool call");
        assert_eq!(
            tool_statuses(&r.ops(QUESTION_SID), &call).last().unwrap(),
            "awaiting-approval"
        );
        // answered from the phone: one label array per question, in order
        let ack = r.decide(
            &q.id,
            "answer",
            None,
            vec![("0".into(), vec!["tabs".into()])],
        );
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }), "{ack:?}");
        let sent = r.requests(&format!("/question/{}/reply", q.id));
        assert!(
            sent[0].ends_with(r#"{"answers":[["tabs"]]}"#),
            "{}",
            sent[0]
        );
        r.feed(rest);
        assert_eq!(
            tool_statuses(&r.ops(QUESTION_SID), &call).last().unwrap(),
            "completed"
        );

        // asked again and answered in the TUI → resolved natively; skipping
        // from the phone uses the reject route
        let (asked, _) = split(QUESTION, "question.replied");
        r.feed(asked);
        r.feed(&r#"data: {"payload":{"type":"question.replied","properties":{"sessionID":"S","requestID":"Q","answers":[["spaces"]]}}}"#.replace("S", QUESTION_SID).replace("Q", &q.id));
        let resolved = r.events().into_iter().find_map(|e| match e {
            EventKind::PermissionResolved(p) => Some(p),
            _ => None,
        });
        assert_eq!(
            resolved.map(|p| (p.status, p.decision)),
            Some(("approved".into(), Some("approve-once".into())))
        );
        r.feed(asked);
        let ack = r.decide(&q.id, "skip", None, vec![]);
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }), "{ack:?}");
        assert_eq!(r.requests(&format!("/question/{}/reject", q.id)).len(), 1);
    }

    #[test]
    fn todo_updated_becomes_plan() {
        let r = rig(Opts::default(), SID);
        r.feed(TODO);
        let ops = r.ops(SID);
        let plan = items(&ops, "plan").pop().expect("plan item");
        assert_eq!(json::get_str(plan, "kind"), Some("plan"));
        let steps: Vec<(&str, &str)> = json::get(plan, "steps")
            .and_then(json::as_arr)
            .unwrap()
            .iter()
            .map(|s| {
                (
                    json::get_str(s, "text").unwrap(),
                    json::get_str(s, "status").unwrap(),
                )
            })
            .collect();
        assert_eq!(steps, [("write tests", "pending"), ("ship it", "pending")]);
    }

    #[test]
    fn abort_is_interrupted_not_failed() {
        let r = rig(Opts::default(), SID);
        submit_to(&r.ad, SID, "cmd-p");
        let (streaming, rest) = split(ABORT, "session.error");
        r.feed(streaming);
        let ack =
            r.ad.send(&ClientMsg::Interrupt {
                command_id: "cmd-i".into(),
                session_id: SID.into(),
            })
            .unwrap();
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }));
        // accepted on the native confirmation; the turn has not ended yet
        let before = r.events();
        assert!(before.iter().any(|e| matches!(e, EventKind::CommandProgress { command_id, state, .. } if command_id == "cmd-i" && state == "accepted")));
        assert!(results(&before).is_empty(), "{before:?}");
        r.feed(rest);
        let after = r.events();
        assert_eq!(
            results(&after),
            [
                ("cmd-p".to_string(), "cancelled".to_string()),
                ("cmd-i".to_string(), "completed".to_string())
            ]
        );
        let ops = r.ops(SID);
        let turn = turn(&ops, "msg_1a6f4d6d2001gIe4k034JayBNS").expect("turn");
        assert_eq!(json::get_str(turn, "status"), Some("interrupted"));
        assert!(json::get_str(turn, "error").is_none());
    }

    #[test]
    fn idle_completes_the_prompt_command() {
        let r = rig(Opts::default(), SID);
        submit_to(&r.ad, SID, "cmd-1");
        r.feed(APPROVE);
        let events = r.events();
        let running = events
            .iter()
            .filter(|e| matches!(e, EventKind::CommandProgress { command_id, state, .. } if command_id == "cmd-1" && state == "running"))
            .count();
        assert_eq!(running, 1, "running once, however often busy repeats");
        assert_eq!(
            results(&events),
            [("cmd-1".to_string(), "completed".to_string())]
        );
        let ops = r.ops(SID);
        let turn = turn(&ops, "msg_1a6f29c8f001OaOj6wOaB61V5O").unwrap();
        assert_eq!(json::get_str(turn, "status"), Some("completed"));
        let usage = json::get(turn, "usage").unwrap();
        assert_eq!(json::get_i64(usage, "inputTokens"), Some(7815 + 54));
        assert_eq!(json::get_i64(usage, "outputTokens"), Some(62 + 23));
        assert_eq!(json::get_i64(usage, "cacheReadTokens"), Some(7811));
        assert!(
            json::get(turn, "costUsd").is_none(),
            "a local model costs nothing"
        );
    }

    #[test]
    fn double_idle_is_idempotent() {
        let r = rig(Opts::default(), SID);
        r.feed(APPROVE);
        let idle: String = APPROVE
            .lines()
            .filter(|l| {
                l.contains(r#""type":"session.idle""#) || l.contains(r#""status":{"type":"idle"}"#)
            })
            .map(|l| format!("{l}\n\n"))
            .collect();
        assert!(!idle.is_empty());
        let (events, ops) = (r.events().len(), r.ops(SID).len());
        assert!(events > 0);
        r.feed(&idle);
        r.feed(&idle);
        assert!(r.events().is_empty());
        assert_eq!(r.ops(SID).len(), ops);
    }

    #[test]
    fn retry_status_becomes_notice() {
        let r = rig(Opts::default(), SID);
        r.feed(RETRY);
        let ops = r.ops(SID);
        let notices: Vec<&Json> = ops
            .iter()
            .filter_map(|o| json::get(o, "item"))
            .filter(|i| json::get_str(i, "kind") == Some("notice"))
            .collect();
        let ids: std::collections::BTreeSet<&str> = notices
            .iter()
            .filter_map(|n| json::get_str(n, "id"))
            .collect();
        assert_eq!(ids.len(), 1, "one notice per turn, updated in place");
        let last = notices.last().unwrap();
        assert_eq!(json::get_str(last, "level"), Some("warning"));
        assert_eq!(
            json::get_str(last, "text"),
            Some(
                "Retrying: Free usage exceeded, subscribe to Go (subscribe: https://opencode.ai/go)"
            )
        );
        // still a running turn, never a failure
        assert!(
            !r.events()
                .iter()
                .any(|e| matches!(e, EventKind::CommandResult { .. }))
        );
    }

    #[test]
    fn mcp_needs_auth_is_polled_and_auth_start_returns_url() {
        let auth = r#"{"authorizationUrl":"https://mcp.sentry.dev/oauth/authorize?response_type=code&client_id=<redacted>&code_challenge=<redacted>&code_challenge_method=S256&redirect_uri=http%3A%2F%2F127.0.0.1%3A19876%2Fmcp%2Foauth%2Fcallback&state=st-1&scope=org%3Aread","oauthState":"st-1"}"#;
        let r = rig(
            Opts {
                routes: vec![
                    ("GET /mcp".into(), MCP.into()),
                    ("POST /mcp/sentry/auth".into(), auth.into()),
                    (
                        "POST /mcp/sentry/auth/callback".into(),
                        r#"{"status":"connected"}"#.into(),
                    ),
                    (
                        "POST /mcp/sentry/auth/authenticate".into(),
                        r#"{"status":"connected"}"#.into(),
                    ),
                ],
                ..Default::default()
            },
            SID,
        );
        let mcp = |r: &Rig| -> Option<Json> {
            r.ops(SID)
                .into_iter()
                .rev()
                .find(|o| json::get_str(o, "op") == Some("mcp"))
                .and_then(|o| {
                    json::get(&o, "servers")
                        .and_then(json::as_arr)
                        .map(|a| a[0].clone())
                })
        };
        // polled when the first phone subscribes
        r.ad.on_subscribe(SID);
        wait_for("mcp op", || mcp(&r).is_some());
        let sentry = mcp(&r).unwrap();
        assert_eq!(json::get_str(&sentry, "name"), Some("sentry"));
        assert_eq!(json::get_str(&sentry, "status"), Some("needs-auth"));
        assert_eq!(
            json::stringify(json::get(&sentry, "authFlows").unwrap()),
            r#"["desktop","phone-callback"]"#
        );
        // phone flow: the URL to open and the dead page's prefix
        let ack =
            r.ad.send(&ClientMsg::McpAuth {
                command_id: "cmd-auth".into(),
                session_id: SID.into(),
                server: "sentry".into(),
                flow: "phone-callback".into(),
            })
            .unwrap();
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }), "{ack:?}");
        let sentry = mcp(&r).unwrap();
        let state = json::get(&sentry, "auth").expect("auth state");
        assert_eq!(json::get_str(state, "state"), Some("awaiting-callback"));
        assert!(
            json::get_str(state, "url")
                .unwrap()
                .starts_with("https://mcp.sentry.dev/oauth/authorize?")
        );
        assert_eq!(
            json::get_str(state, "callbackPrefix"),
            Some("http://127.0.0.1:19876/mcp/oauth/callback")
        );
        // the pasted dead page completes it (code forwarded), once
        let callback = |cmd: &str| {
            r.ad.send(&ClientMsg::McpAuthCallback {
                command_id: cmd.into(),
                session_id: SID.into(),
                server: "sentry".into(),
                callback_url: "http://127.0.0.1:19876/mcp/oauth/callback?code=c0de&state=st-1"
                    .into(),
            })
            .unwrap()
        };
        assert!(matches!(
            callback("cmd-cb"),
            ServerMsg::Ack { ok: true, .. }
        ));
        let mut done = Vec::new();
        wait_for("callback results", || {
            done.extend(results(&r.events()));
            done.len() >= 2
        });
        assert!(
            done.contains(&("cmd-auth".into(), "completed".into())),
            "{done:?}"
        );
        assert!(
            done.contains(&("cmd-cb".into(), "completed".into())),
            "{done:?}"
        );
        assert!(r.requests("/auth/callback")[0].ends_with(r#"{"code":"c0de"}"#));
        assert!(matches!(
            callback("cmd-cb2"),
            ServerMsg::Ack { ok: false, .. }
        ));
        // desktop flow: OpenCode signs in through the desktop browser
        let ack =
            r.ad.send(&ClientMsg::McpAuth {
                command_id: "cmd-desk".into(),
                session_id: SID.into(),
                server: "sentry".into(),
                flow: "desktop".into(),
            })
            .unwrap();
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }));
        let mut done = Vec::new();
        wait_for("desktop result", || {
            done.extend(results(&r.events()));
            !done.is_empty()
        });
        assert_eq!(done, [("cmd-desk".to_string(), "completed".to_string())]);
        r.ad.dispose();
    }

    #[test]
    fn hydrates_items_from_messages_on_subscribe_and_reconnect() {
        let r = rig(
            Opts {
                routes: vec![(format!("GET /session/{SID}/message"), MESSAGES.into())],
                sse_hold: false,
                ..Default::default()
            },
            SID,
        );
        r.ad.on_subscribe(SID);
        let has = |r: &Rig, id: &str| !items(&r.ops(SID), id).is_empty();
        wait_for("hydrated", || has(&r, "call_mncm01jw"));
        let ops = r.ops(SID);
        let user = items(&ops, "msg_1a6f29c8f001OaOj6wOaB61V5O")[0];
        assert_eq!(json::get_str(user, "kind"), Some("user"));
        assert!(
            json::get_str(user, "text")
                .unwrap()
                .contains("echo hi > note.txt")
        );
        assert_eq!(tool_statuses(&ops, "call_22tnronp"), ["completed"]);
        assert_eq!(tool_statuses(&ops, "call_mncm01jw"), ["denied"]);
        let answer = items(&ops, "prt_1a6f4061b001rO2jCZOA6vpqqz");
        assert_eq!(
            answer.first().and_then(|i| json::get_str(i, "text")),
            Some("DONE")
        );
        let first = turn(&ops, "msg_1a6f29c8f001OaOj6wOaB61V5O").unwrap();
        assert_eq!(json::get_str(first, "status"), Some("completed"));
        assert!(json::get(first, "usage").is_some());
        let aborted = turn(&ops, "msg_1a6f4d6d2001gIe4k034JayBNS").unwrap();
        assert_eq!(json::get_str(aborted, "status"), Some("interrupted"));
        // every SSE (re)connect rebuilds the watched transcript
        let path = format!("/session/{SID}/message");
        r.ad.start_events().unwrap();
        wait_for("re-hydrated after reconnect", || {
            r.requests(&path).len() >= 3
        });
        r.ad.dispose();
    }

    /* ------------------------ ADR-021 parity ------------------------ */

    /// Shaped like 1.19.4's `GET /config/providers` (trimmed): a hosted
    /// provider, Ollama with the configured model, a deprecated model and an
    /// id the phone could not send.
    const PROVIDERS: &str = r#"{"providers":[
{"id":"opencode","name":"OpenCode","models":{
 "big-pickle":{"id":"big-pickle","providerID":"opencode","name":"Big Pickle","limit":{"context":200000,"output":32000},"status":"active"},
 "old-one":{"id":"old-one","name":"Old","limit":{"context":1000},"status":"deprecated"},
 "bad id":{"id":"bad id","name":"Bad","limit":{"context":1000}}}},
{"id":"ollama","name":"Ollama","models":{
 "qwen3.5:4b":{"id":"qwen3.5:4b","name":"qwen3.5 4b","limit":{"context":8192,"output":2048},"status":"active"},
 "chernobyl-qwen35-24k":{"id":"chernobyl-qwen35-24k","name":"qwen3.5 4b 24k","limit":{"context":24576,"output":4096},"status":"active"}}}],
"default":{"opencode":"big-pickle","ollama":"qwen3.5:4b"}}"#;
    const COMMANDS: &str = r#"[{"name":"init","source":"command","template":"t","hints":["$ARGUMENTS"]},{"name":"review","source":"command","template":"t","hints":[]},{"name":"broken","template":"t","hints":[]},{"name":"has space","template":"t","hints":[]}]"#;
    const QWEN24: &str = "ollama/chernobyl-qwen35-24k";
    /// 1.19.4's built-ins (hidden ones included) plus a user agent that
    /// allows everything: never offered as a mode.
    const PARITY_AGENTS: &str = r#"[{"name":"build","mode":"primary","native":true},{"name":"plan","mode":"primary","native":true},{"name":"explore","mode":"subagent","native":true},{"name":"title","mode":"primary","native":true,"hidden":true},{"name":"yolo","mode":"primary","permission":[{"permission":"*","pattern":"*","action":"allow"}]}]"#;

    /// A workspace folder (the serve's cwd) and a data dir outside it.
    struct Dirs {
        root: PathBuf,
        work: PathBuf,
        data: PathBuf,
    }

    impl Drop for Dirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn dirs() -> Dirs {
        let root = std::env::temp_dir().join(format!("chb-oc-parity-{}", next_id("t")));
        let (work, data) = (root.join("Work"), root.join("data"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let canon = |p: &Path| crate::claude_live::plain(&std::fs::canonicalize(p).unwrap());
        Dirs {
            work: PathBuf::from(canon(&work)),
            data: PathBuf::from(canon(&data)),
            root,
        }
    }

    /// The parity routes: `/path` names `dir`, the session lists in it
    /// (spelled in lower case with forward slashes, as a client may).
    fn parity_opts(dir: &str) -> Opts {
        let q = |s: &str| json::stringify(&json::s(s));
        let session_dir = dir.to_lowercase().replace('\\', "/");
        Opts {
            routes: vec![
                (
                    "GET /path".into(),
                    format!(r#"{{"home":"h","state":"s","config":"c","worktree":{0},"directory":{0}}}"#, q(dir)),
                ),
                ("GET /config/providers".into(), PROVIDERS.into()),
                ("GET /config".into(), format!(r#"{{"model":"{QWEN24}"}}"#)),
                ("GET /command".into(), COMMANDS.into()),
                ("GET /agent".into(), PARITY_AGENTS.into()),
                (
                    "GET /session".into(),
                    format!(
                        r#"[{{"id":"ses_test1","directory":{},"title":"Demo session","time":{{"created":1,"updated":2}}}}]"#,
                        q(&session_dir)
                    ),
                ),
                (
                    "PATCH /session/ses_test1".into(),
                    r#"{"id":"ses_test1","title":"Renamed","time":{"created":1,"updated":3}}"#.into(),
                ),
                (
                    "POST /session/ses_test1/fork".into(),
                    r#"{"id":"ses_fork1","title":"Demo session (fork #1)","time":{"created":4,"updated":4}}"#.into(),
                ),
            ],
            ..Default::default()
        }
    }

    fn last_state(ops: &[Json]) -> Json {
        ops.iter()
            .rev()
            .find(|o| json::get_str(o, "op") == Some("state"))
            .and_then(|o| json::get(o, "state"))
            .cloned()
            .expect("a state op")
    }

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEUlEQVR4nGP4z8DwnwGMgRQAH+4D/dJQfRoAAAAASUVORK5CYII=";

    fn png() -> Vec<PromptImage> {
        vec![PromptImage {
            media_type: "image/png".into(),
            data: PNG.into(),
        }]
    }

    #[test]
    fn serve_folder_is_the_workspace_and_the_composer_is_advertised() {
        let d = dirs();
        let r = rig_in(
            parity_opts(&d.work.to_string_lossy()),
            "ses_test1",
            Some(&d.data),
        );
        r.ad.refresh().unwrap();
        let build = r.ad.agents().into_iter().find(|a| a.id == "build").unwrap();
        let live = build.live.unwrap();
        assert_eq!(live.workspaces.len(), 1);
        let ws = &live.workspaces[0];
        assert_eq!(ws.path, d.work.to_string_lossy());
        assert_eq!(ws.label, "Work");
        assert_eq!(ws.id, crate::claude_live::workspace_id(&ws.path));
        assert_eq!(
            live.composer,
            ["model", "images", "commands", "context", "rename"]
        );
        // the configured model first; deprecated and unsendable ids left out
        let values: Vec<&str> = live.models.iter().map(|m| m.0.as_str()).collect();
        assert_eq!(values, [QWEN24, "opencode/big-pickle", "ollama/qwen3.5:4b"]);
        assert_eq!(live.models[0].1, "qwen3.5 4b 24k");
        // a session in that folder carries the workspace's own spelling
        assert_eq!(r.ad.sessions()[0].project, ws.path);
        // the serve in a folder that holds the data dir: no workspace
        let r = rig_in(parity_opts(&d.root.to_string_lossy()), "s", Some(&d.data));
        r.ad.refresh().unwrap();
        assert!(
            r.ad.agents()[0]
                .live
                .as_ref()
                .unwrap()
                .workspaces
                .is_empty()
        );
        // a drive root, and a probe without a data dir: none either
        let root = d
            .work
            .ancestors()
            .last()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let r = rig_in(parity_opts(&root), "s", Some(&d.data));
        r.ad.refresh().unwrap();
        assert!(
            r.ad.agents()[0]
                .live
                .as_ref()
                .unwrap()
                .workspaces
                .is_empty()
        );
        let r = rig(parity_opts(&d.work.to_string_lossy()), "s");
        r.ad.refresh().unwrap();
        assert!(
            r.ad.agents()[0]
                .live
                .as_ref()
                .unwrap()
                .workspaces
                .is_empty()
        );
        // a server without the extras still probes, with no composer extras
        // beyond the ones that need no list
        let r = rig(Opts::default(), "s");
        r.ad.refresh().unwrap();
        let live = r.ad.agents()[0].live.clone().unwrap();
        assert_eq!(live.composer, ["images", "context", "rename"]);
        assert!(live.models.is_empty() && live.workspaces.is_empty());
    }

    #[test]
    fn create_takes_the_workspace_a_listed_model_and_photos() {
        let d = dirs();
        let r = rig_in(
            parity_opts(&d.work.to_string_lossy()),
            "ses_created1",
            Some(&d.data),
        );
        r.ad.refresh().unwrap();
        let ws = r.ad.agents()[0].live.clone().unwrap().workspaces[0]
            .id
            .clone();
        let create = |project: Option<&str>, model: Option<&str>, cmd: &str| {
            r.ad.send(&ClientMsg::CreateSession {
                command_id: cmd.into(),
                agent_id: "build".into(),
                title: "Fresh".into(),
                prompt: "what is this?".into(),
                project: project.map(str::to_string),
                images: png(),
                model: model.map(str::to_string),
            })
            .unwrap()
        };
        let refused = |m: ServerMsg| match m {
            ServerMsg::Ack {
                ok: false, error, ..
            } => error.unwrap(),
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert!(refused(create(Some("0123456789ab"), None, "c1")).contains("folder"));
        assert!(refused(create(Some(&ws), Some("ollama/nope"), "c2")).contains("does not offer"));
        assert!(r.requests("POST /session ").is_empty(), "nothing created");
        let ok = create(Some(&ws), Some(QWEN24), "c3");
        assert!(matches!(ok, ServerMsg::Ack { ok: true, .. }), "{ok:?}");
        let sent = r.requests("prompt_async").pop().unwrap();
        assert!(
            sent.contains(r#""model":{"providerID":"ollama","modelID":"chernobyl-qwen35-24k"}"#),
            "{sent}"
        );
        assert!(sent.contains(&format!(
            r#"{{"type":"file","mime":"image/png","filename":"photo-1.png","url":"data:image/png;base64,{PNG}"}}"#
        )));
        // text first, then the photo
        assert!(sent.find("what is this?").unwrap() < sent.find("data:image/png").unwrap());
        let state = last_state(&r.ops("ses_created1"));
        assert_eq!(json::get_str(&state, "model"), Some(QWEN24));
        assert_eq!(
            json::get(&state, "slashCommands")
                .and_then(json::as_arr)
                .map(<[Json]>::len),
            Some(3)
        );
        // "default" means OpenCode's own choice: no model is sent
        assert!(matches!(
            create(None, Some("default"), "c4"),
            ServerMsg::Ack { ok: true, .. }
        ));
        assert!(
            !r.requests("prompt_async")
                .pop()
                .unwrap()
                .contains("providerID")
        );
    }

    #[test]
    fn set_model_rename_and_slash_commands() {
        let d = dirs();
        let r = rig_in(
            parity_opts(&d.work.to_string_lossy()),
            "ses_test1",
            Some(&d.data),
        );
        r.ad.refresh().unwrap();
        let send = |m: ClientMsg| r.ad.send(&m).unwrap();
        let model = |cmd: &str, m: &str| {
            send(ClientMsg::SetModel {
                command_id: cmd.into(),
                session_id: "ses_test1".into(),
                model: m.into(),
            })
        };
        assert!(matches!(
            model("m1", "ollama/nope"),
            ServerMsg::Ack { ok: false, .. }
        ));
        assert!(matches!(
            model("m2", "opencode/big-pickle"),
            ServerMsg::Ack { ok: true, .. }
        ));
        assert_eq!(
            json::get_str(&last_state(&r.ops("ses_test1")), "model"),
            Some("opencode/big-pickle")
        );
        submit_to(&r.ad, "ses_test1", "p1");
        assert!(
            r.requests("prompt_async")
                .pop()
                .unwrap()
                .contains(r#""modelID":"big-pickle""#)
        );
        // rename: PATCH with the title, the session updated, the command done
        let ack = send(ClientMsg::RenameSession {
            command_id: "r1".into(),
            session_id: "ses_test1".into(),
            title: "Renamed".into(),
        });
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }));
        assert!(
            r.requests("PATCH /session/ses_test1")
                .pop()
                .unwrap()
                .contains(r#"{"title":"Renamed"}"#)
        );
        assert_eq!(r.ad.sessions()[0].title, "Renamed");
        let events = r.events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, EventKind::SessionUpdated(s) if s.title == "Renamed"))
        );
        let done = results(&events);
        assert!(done.contains(&("m2".into(), "completed".into())));
        assert!(done.contains(&("r1".into(), "completed".into())));
        // a known command runs through /command with its arguments and the
        // chosen model; an unknown one is plain prompt text
        let slash = |cmd: &str, text: &str| {
            let ack = send(ClientMsg::SubmitPrompt {
                command_id: cmd.into(),
                session_id: "ses_test1".into(),
                text: text.into(),
                images: vec![],
                fork: false,
            });
            assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }), "{ack:?}");
        };
        slash("s1", "/init keep it short");
        wait_for("the command call", || {
            !r.requests("ses_test1/command").is_empty()
        });
        let call = r.requests("POST /session/ses_test1/command").pop().unwrap();
        assert!(
            call.contains(
                r#""command":"init","arguments":"keep it short","model":"opencode/big-pickle""#
            ),
            "{call}"
        );
        slash("s2", "/nosuch thing");
        assert!(
            r.requests("prompt_async")
                .pop()
                .unwrap()
                .contains("/nosuch thing")
        );
        // the turn closes a command like any prompt
        r.feed(&format!(
            "data: {}\n",
            r#"{"payload":{"type":"session.idle","properties":{"sessionID":"ses_test1"}}}"#
        ));
        let done = results(&r.events());
        assert!(
            done.contains(&("s1".into(), "completed".into())),
            "{done:?}"
        );
        // a command OpenCode refuses before its turn started fails at once
        slash("s3", "/broken");
        wait_for("the refusal", || {
            r.rx.try_iter().any(|e| {
                matches!(&e, EventKind::CommandResult { command_id, status, error, .. }
                    if command_id == "s3" && status == "failed"
                        && error.as_deref().is_some_and(|m| m.contains("HTTP 400")))
            })
        });
    }

    #[test]
    fn fork_creates_the_fork_and_prompts_it() {
        let d = dirs();
        let r = rig_in(
            parity_opts(&d.work.to_string_lossy()),
            "ses_fork1",
            Some(&d.data),
        );
        r.ad.refresh().unwrap();
        let ack =
            r.ad.send(&ClientMsg::SubmitPrompt {
                command_id: "f1".into(),
                session_id: "ses_test1".into(),
                text: "carry on".into(),
                images: vec![],
                fork: true,
            })
            .unwrap();
        match ack {
            ServerMsg::Ack {
                ok: true,
                session_id,
                ..
            } => assert_eq!(session_id.as_deref(), Some("ses_fork1")),
            other => panic!("{other:?}"),
        }
        assert_eq!(r.requests("POST /session/ses_test1/fork").len(), 1);
        assert!(
            r.requests("/session/ses_fork1/prompt_async")
                .pop()
                .unwrap()
                .contains("carry on")
        );
        assert!(
            r.requests("/session/ses_test1/prompt_async").is_empty(),
            "the original is untouched"
        );
        let forked =
            r.ad.sessions()
                .into_iter()
                .find(|s| s.id == "ses_fork1")
                .unwrap();
        assert_eq!(forked.forked_from.as_deref(), Some("Demo session"));
        assert!(
            r.events()
                .iter()
                .any(|e| matches!(e, EventKind::SessionCreated(s) if s.id == "ses_fork1"))
        );
        // the marker survives OpenCode's own updates of the fork
        r.feed(r#"data: {"payload":{"type":"session.updated","properties":{"sessionID":"ses_fork1","info":{"id":"ses_fork1","title":"Fork title","time":{"created":4,"updated":5}}}}}"#);
        let forked =
            r.ad.sessions()
                .into_iter()
                .find(|s| s.id == "ses_fork1")
                .unwrap();
        assert_eq!(forked.title, "Fork title");
        assert_eq!(forked.forked_from.as_deref(), Some("Demo session"));
    }

    #[test]
    fn photos_count_on_the_user_item_and_the_context_meter_fills() {
        let d = dirs();
        let r = rig_in(
            parity_opts(&d.work.to_string_lossy()),
            "ses_test1",
            Some(&d.data),
        );
        r.ad.refresh().unwrap();
        let ev = |t: &str, props: &str| {
            format!(r#"data: {{"payload":{{"type":"{t}","properties":{props}}}}}"#)
        };
        let script = [
            ev("message.updated", r#"{"info":{"id":"msg_u1","sessionID":"ses_test1","role":"user","time":{"created":1}}}"#),
            ev("message.part.updated", r#"{"part":{"id":"prt_t1","messageID":"msg_u1","sessionID":"ses_test1","type":"text","text":"what is this?"}}"#),
            ev("message.part.updated", r#"{"part":{"id":"prt_f1","messageID":"msg_u1","sessionID":"ses_test1","type":"file","mime":"image/png","url":"data:image/png;base64,x"}}"#),
            ev("message.part.updated", r#"{"part":{"id":"prt_f1","messageID":"msg_u1","sessionID":"ses_test1","type":"file","mime":"image/png","url":"data:image/png;base64,x"}}"#),
            ev("message.updated", r#"{"info":{"id":"msg_a1","sessionID":"ses_test1","role":"assistant","parentID":"msg_u1","providerID":"ollama","modelID":"chernobyl-qwen35-24k","time":{"created":2},"tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}}}"#),
            ev("message.updated", r#"{"info":{"id":"msg_a1","sessionID":"ses_test1","role":"assistant","parentID":"msg_u1","providerID":"ollama","modelID":"chernobyl-qwen35-24k","time":{"created":2,"completed":3},"tokens":{"input":12000,"output":200,"reasoning":50,"cache":{"read":40,"write":0}}}}"#),
        ]
        .join("\n");
        r.feed(&script);
        let ops = r.ops("ses_test1");
        let user = items(&ops, "msg_u1").pop().unwrap().clone();
        assert_eq!(json::get_i64(&user, "images"), Some(1), "counted once");
        let state = last_state(&ops);
        let context = json::get(&state, "context").expect("context");
        assert_eq!(json::get_i64(context, "usedTokens"), Some(12_290));
        assert_eq!(json::get_i64(context, "maxTokens"), Some(24_576));
        assert_eq!(json::get_str(&state, "model"), Some(QWEN24));
        // hydration counts the photos too and rebuilds the meter
        let messages = r#"[
{"info":{"id":"msg_u2","sessionID":"ses_h","role":"user","time":{"created":1}},"parts":[
 {"id":"prt_t2","messageID":"msg_u2","sessionID":"ses_h","type":"text","text":"two photos"},
 {"id":"prt_f2","messageID":"msg_u2","sessionID":"ses_h","type":"file","mime":"image/jpeg","url":"data:"},
 {"id":"prt_f3","messageID":"msg_u2","sessionID":"ses_h","type":"file","mime":"image/png","url":"data:"}]},
{"info":{"id":"msg_a2","sessionID":"ses_h","role":"assistant","parentID":"msg_u2","providerID":"opencode","modelID":"big-pickle","time":{"created":2,"completed":3},"tokens":{"input":1000,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}},"parts":[]}]"#;
        let mut opts = parity_opts(&d.work.to_string_lossy());
        opts.routes
            .push(("GET /session/ses_h/message".into(), messages.into()));
        let r = rig_in(opts, "ses_h", Some(&d.data));
        r.ad.refresh().unwrap();
        r.ad.on_subscribe("ses_h");
        wait_for("hydrated", || !items(&r.ops("ses_h"), "msg_u2").is_empty());
        let ops = r.ops("ses_h");
        assert_eq!(json::get_i64(items(&ops, "msg_u2")[0], "images"), Some(2));
        wait_for("state", || {
            r.ops("ses_h")
                .iter()
                .any(|o| json::get_str(o, "op") == Some("state"))
        });
        let state = last_state(&r.ops("ses_h"));
        assert_eq!(json::get_str(&state, "model"), Some("opencode/big-pickle"));
        let context = json::get(&state, "context").unwrap();
        assert_eq!(json::get_i64(context, "maxTokens"), Some(200_000));
        r.ad.dispose();
    }

    #[test]
    fn end_aborts_the_turn_and_the_session_refuses_prompts() {
        let d = dirs();
        let r = rig_in(
            parity_opts(&d.work.to_string_lossy()),
            "ses_test1",
            Some(&d.data),
        );
        r.ad.refresh().unwrap();
        assert!(
            r.ad.agents()[0].live.as_ref().unwrap().end,
            "End is advertised"
        );
        let ev = |t: &str, status: &str| {
            format!(
                r#"data: {{"payload":{{"type":"{t}","properties":{{"sessionID":"ses_test1"{status}}}}}}}"#
            )
        };
        r.feed(&ev("session.status", r#","status":{"type":"busy"}"#));
        let end =
            r.ad.send(&ClientMsg::SessionEnd {
                command_id: "e1".into(),
                session_id: "ses_test1".into(),
            })
            .unwrap();
        assert!(matches!(end, ServerMsg::Ack { ok: true, .. }), "{end:?}");
        assert_eq!(r.requests("POST /session/ses_test1/abort").len(), 1);
        assert!(r.requests("DELETE").is_empty(), "the history stays");
        assert!(results(&r.events()).contains(&("e1".into(), "completed".into())));
        let status =
            |r: &Rig| json::get_str(&last_state(&r.ops("ses_test1")), "status").map(str::to_string);
        assert_eq!(status(&r).as_deref(), Some("ended"));
        // the aborted turn ends; the session stays ended
        r.feed(&ev("session.idle", ""));
        assert_eq!(status(&r).as_deref(), Some("ended"));
        let refusal = |m: ClientMsg| match r.ad.send(&m).unwrap() {
            ServerMsg::Ack {
                ok: false, error, ..
            } => error.unwrap(),
            other => panic!("{other:?}"),
        };
        let prompt = |fork| ClientMsg::SubmitPrompt {
            command_id: "p".into(),
            session_id: "ses_test1".into(),
            text: "more".into(),
            images: vec![],
            fork,
        };
        assert_eq!(refusal(prompt(false)), ENDED);
        assert_eq!(
            refusal(ClientMsg::SetPermissionMode {
                command_id: "m".into(),
                session_id: "ses_test1".into(),
                mode: "plan".into(),
            }),
            ENDED
        );
        assert_eq!(
            refusal(ClientMsg::RenameSession {
                command_id: "n".into(),
                session_id: "ses_test1".into(),
                title: "x".into(),
            }),
            ENDED
        );
        assert!(r.requests("prompt_async").is_empty());
        // a fork is a new session, so it may; ending twice is a no-op
        assert!(matches!(
            r.ad.send(&prompt(true)).unwrap(),
            ServerMsg::Ack { ok: true, .. }
        ));
        let again =
            r.ad.send(&ClientMsg::SessionEnd {
                command_id: "e2".into(),
                session_id: "ses_test1".into(),
            })
            .unwrap();
        assert!(matches!(again, ServerMsg::Ack { ok: true, .. }));
        assert_eq!(
            r.requests("/abort").len(),
            1,
            "an idle session is not aborted"
        );
        // a re-hydration keeps it ended
        r.ad.on_subscribe("ses_test1");
        wait_for("re-hydrated", || {
            r.requests("/session/ses_test1/message").len() == 1
        });
        wait_for("its state", || status(&r).as_deref() == Some("ended"));
        r.ad.dispose();
    }

    #[test]
    fn permission_modes_are_the_build_and_plan_agents() {
        let d = dirs();
        let r = rig_in(
            parity_opts(&d.work.to_string_lossy()),
            "ses_test1",
            Some(&d.data),
        );
        r.ad.refresh().unwrap();
        let agents = r.ad.agents();
        let names: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            names,
            ["build", "plan", "explore", "yolo"],
            "hidden ones left out"
        );
        let live = agents[0].live.clone().unwrap();
        assert_eq!(live.permission_modes, ["default", "plan"], "never `yolo`");
        let mode = |cmd: &str, m: &str| {
            r.ad.send(&ClientMsg::SetPermissionMode {
                command_id: cmd.into(),
                session_id: "ses_test1".into(),
                mode: m.into(),
            })
            .unwrap()
        };
        assert!(matches!(
            mode("m1", "acceptEdits"),
            ServerMsg::Ack { ok: false, .. }
        ));
        assert!(matches!(
            mode("m2", "plan"),
            ServerMsg::Ack { ok: true, .. }
        ));
        let state = last_state(&r.ops("ses_test1"));
        assert_eq!(json::get_str(&state, "permissionMode"), Some("plan"));
        submit_to(&r.ad, "ses_test1", "p1");
        assert!(
            r.requests("prompt_async")
                .pop()
                .unwrap()
                .contains(r#""agent":"plan""#)
        );
        // slash commands run as the same agent
        let ack =
            r.ad.send(&ClientMsg::SubmitPrompt {
                command_id: "s1".into(),
                session_id: "ses_test1".into(),
                text: "/review".into(),
                images: vec![],
                fork: false,
            })
            .unwrap();
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }));
        wait_for("the command", || {
            !r.requests("ses_test1/command").is_empty()
        });
        assert!(
            r.requests("ses_test1/command")
                .pop()
                .unwrap()
                .contains(r#""agent":"plan""#)
        );
        assert!(matches!(
            mode("m3", "default"),
            ServerMsg::Ack { ok: true, .. }
        ));
        submit_to(&r.ad, "ses_test1", "p2");
        assert!(
            r.requests("prompt_async")
                .pop()
                .unwrap()
                .contains(r#""agent":"build""#)
        );
        let done = results(&r.events());
        assert!(done.contains(&("m2".into(), "completed".into())));
        // a task started on the plan agent runs as plan; a session nobody
        // chose for reports the agent its last answer ran as
        let ack =
            r.ad.send(&ClientMsg::CreateSession {
                command_id: "c1".into(),
                agent_id: "plan".into(),
                title: "t".into(),
                prompt: "plan it".into(),
                project: None,
                images: vec![],
                model: None,
            })
            .unwrap();
        assert!(matches!(ack, ServerMsg::Ack { ok: true, .. }));
        assert!(
            r.requests("ses_created1/prompt_async")
                .pop()
                .unwrap()
                .contains(r#""agent":"plan""#)
        );
        r.feed(r#"data: {"payload":{"type":"message.updated","properties":{"info":{"id":"msg_a9","sessionID":"ses_test2","role":"assistant","parentID":"msg_u9","agent":"plan","providerID":"ollama","modelID":"qwen3.5:4b","time":{"created":1}}}}}"#);
        let st = r.ad.inner.state.lock().unwrap();
        assert_eq!(
            Inner::live_state(&st, "ses_test2", "idle")
                .permission_mode
                .as_deref(),
            Some("plan")
        );
        assert_eq!(
            Inner::live_state(&st, "ses_other", "idle")
                .permission_mode
                .as_deref(),
            Some("default")
        );
    }

    #[test]
    fn slash_and_model_helpers() {
        let cmds = vec!["init".to_string(), "review".to_string()];
        assert_eq!(slash("/init", &cmds), Some(("init", "")));
        assert_eq!(slash("/review  HEAD~1 ", &cmds), Some(("review", "HEAD~1")));
        assert_eq!(slash("/initx", &cmds), None);
        assert_eq!(slash("init", &cmds), None);
        assert!(same_dir(r"C:\Proj\Demo\", "c:/proj/demo"));
        assert!(!same_dir(r"C:\Proj\Demo2", r"C:\Proj\Demo"));
        let models = catalog_models(&json::parse(PROVIDERS).unwrap(), None);
        assert_eq!(models[0].0, "opencode/big-pickle");
        assert_eq!(models.len(), 3);
    }

    #[test]
    fn build_plan_mode_mapping_pure_helpers_and_accept_edits_refused() {
        // pure mode_agent and agent_mode mappings
        assert_eq!(mode_agent("default"), Some("build"));
        assert_eq!(mode_agent("plan"), Some("plan"));
        assert_eq!(mode_agent("acceptEdits"), None); // refused: OpenCode has no acceptEdits mode
        assert_eq!(mode_agent("yolo"), None);
        assert_eq!(mode_agent(""), None);

        assert_eq!(agent_mode("build"), Some("default"));
        assert_eq!(agent_mode("plan"), Some("plan"));
        assert_eq!(agent_mode("explore"), None);
        assert_eq!(agent_mode("compaction"), None);

        // filtering agents: hidden ones are dropped, non-build/plan are dropped
        let agents_raw = json::parse(
            r#"[
                {"id":"build","name":"Builder","hidden":false},
                {"id":"plan","name":"Planner","hidden":true},
                {"id":"summary","name":"Summary","hidden":true},
                {"id":"explore","name":"Explorer","hidden":false}
            ]"#,
        )
        .unwrap();
        let agents = json::as_arr(&agents_raw).unwrap();
        let offered: Vec<&'static str> = ["build", "plan"]
            .iter()
            .filter(|m| {
                agents.iter().any(|a| {
                    json::get_str(a, "id") == Some(**m) && json::get_bool(a, "hidden") != Some(true)
                })
            })
            .filter_map(|a| agent_mode(a))
            .collect();
        assert_eq!(offered, ["default"]); // plan was hidden so dropped; explore is not build/plan

        // Fake-server test: verify acceptEdits is explicitly refused by set_mode
        let d = dirs();
        let r = rig_in(
            parity_opts(&d.work.to_string_lossy()),
            "ses_test1",
            Some(&d.data),
        );
        let res =
            r.ad.send(&ClientMsg::SetPermissionMode {
                command_id: "cmd_mode".into(),
                session_id: "ses_test1".into(),
                mode: "acceptEdits".into(),
            })
            .unwrap();
        assert!(matches!(
            res,
            ServerMsg::Ack {
                ok: false,
                error: Some(err),
                ..
            } if err.contains("acceptEdits")
        ));
    }

    #[test]
    fn end_behaviour_helpers_refusal() {
        let d = dirs();
        let r = rig_in(
            parity_opts(&d.work.to_string_lossy()),
            "ses_test1",
            Some(&d.data),
        );

        // End the session
        let end =
            r.ad.send(&ClientMsg::SessionEnd {
                command_id: "cmd_end".into(),
                session_id: "ses_test1".into(),
            })
            .unwrap();
        assert!(matches!(end, ServerMsg::Ack { ok: true, .. }));

        // Now prompt to ended session is refused with ENDED message
        let prompt_res =
            r.ad.send(&ClientMsg::SubmitPrompt {
                command_id: "cmd_p".into(),
                session_id: "ses_test1".into(),
                text: "next turn".into(),
                images: vec![],
                fork: false,
            })
            .unwrap();
        assert!(matches!(
            prompt_res,
            ServerMsg::Ack {
                ok: false,
                error: Some(err),
                ..
            } if err == ENDED
        ));

        // SetPermissionMode is refused on ended session
        let mode_res =
            r.ad.send(&ClientMsg::SetPermissionMode {
                command_id: "cmd_m".into(),
                session_id: "ses_test1".into(),
                mode: "plan".into(),
            })
            .unwrap();
        assert!(matches!(
            mode_res,
            ServerMsg::Ack {
                ok: false,
                error: Some(err),
                ..
            } if err == ENDED
        ));

        // SetModel is refused on ended session
        let model_res =
            r.ad.send(&ClientMsg::SetModel {
                command_id: "cmd_mod".into(),
                session_id: "ses_test1".into(),
                model: "ollama/qwen3.5:4b".into(),
            })
            .unwrap();
        assert!(matches!(
            model_res,
            ServerMsg::Ack {
                ok: false,
                error: Some(err),
                ..
            } if err == ENDED
        ));
    }

    #[test]
    fn provider_model_list_ordering_default_first_and_filtering() {
        let providers_raw = json::parse(
            r#"{
                "providers": [
                    {
                        "id": "provider_a",
                        "models": {
                            "m1": {"id": "m1", "name": "Model 1", "limit": {"context": 8192}},
                            "m_dep": {"id": "m_dep", "name": "Deprecated", "status": "deprecated"},
                            "bad id": {"id": "bad id", "name": "Invalid"}
                        }
                    },
                    {
                        "id": "provider_b",
                        "models": {
                            "m2": {"id": "m2", "name": "Model 2", "limit": {"context": 32768}}
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        // When default is "provider_b/m2", it is sorted to index 0
        let models_def = catalog_models(&providers_raw, Some("provider_b/m2"));
        assert_eq!(models_def.len(), 2);
        assert_eq!(models_def[0].0, "provider_b/m2");
        assert_eq!(models_def[0].1, "Model 2");
        assert_eq!(models_def[0].2, 32768);
        assert_eq!(models_def[1].0, "provider_a/m1");

        // When default is None, standard order
        let models_no_def = catalog_models(&providers_raw, None);
        assert_eq!(models_no_def.len(), 2);
        assert!(!models_no_def.iter().any(|m| m.0.contains("dep")));
        assert!(!models_no_def.iter().any(|m| m.0.contains("bad")));

        // Truncation at MODELS_MAX (30)
        let mut many = Vec::new();
        for i in 0..40 {
            many.push(format!(
                r#""m_{i}": {{"id": "m_{i}", "name": "Model {i}"}}"#
            ));
        }
        let many_json = json::parse(&format!(
            r#"{{"providers": [{{"id": "p", "models": {{{}}} }}]}}"#,
            many.join(",")
        ))
        .unwrap();
        let capped = catalog_models(&many_json, None);
        assert_eq!(capped.len(), MODELS_MAX);
    }
}
