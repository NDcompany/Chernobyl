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

use crate::adapters::{Capability, LiveRuntime};
use crate::client::{HttpResponse, http_request, split_url};
use crate::json::{self, Json};
use crate::protocol::{
    ActivityEvent, Agent, ClientMsg, EventKind, PermissionRequest, ServerMsg, Session, next_id,
    now_ms,
};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    /// In-flight prompt turns: (session_id, command_id) — the correlation
    /// that turns a later `session.error` into the right `command_result`.
    pending: Vec<(String, String)>,
    /// Native permission requests we surfaced (native id → record), so a
    /// `permission.replied` can be resolved without fabricating state.
    asked: HashMap<String, PermissionRequest>,
}

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
}

pub struct OpenCodeAdapter {
    pub id: String,
    inner: Arc<Inner>,
}

/* ------------------------------------------------------------------ */
/* Construction + public surface                                       */
/* ------------------------------------------------------------------ */

impl OpenCodeAdapter {
    pub fn new(id: &str, endpoint: &str, basic_auth: Option<&str>) -> OpenCodeAdapter {
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
            Capability::new(
                "session.interrupt",
                "limited",
                Some(
                    "abort confirmed on idle and mid-turn probes; stopping a healthy long generation still needs release-gate re-verification (opencode.md §5)",
                ),
            ),
            Capability::new(
                "session.diff",
                "limited",
                Some(
                    "endpoint verified; the probe's turn produced an empty diff, populated capture pending (opencode.md §6)",
                ),
            ),
            Capability::new(
                "permission.reply",
                "limited",
                Some(
                    "reply contract once|always|reject verified from the served OpenAPI; a live asked→reply exchange still needs a policy that asks (opencode.md §7)",
                ),
            ),
            Capability::new(
                "session.output",
                "limited",
                Some(
                    "text parts normalized only; tool-call parts have no protocol representation yet",
                ),
            ),
            Capability::new(
                "event.replay",
                "unavailable",
                Some(
                    "SSE has no cursor/ack on 1.17.x: after reconnect state is re-listed instead of replayed",
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
        match msg {
            ClientMsg::CreateSession {
                command_id,
                title,
                prompt,
                project,
                ..
            } => self
                .inner
                .create_session(command_id, title, prompt, project.as_deref()),
            ClientMsg::SubmitPrompt {
                command_id,
                session_id,
                text,
            } => {
                self.inner.prompt_accept(session_id, text)?;
                let mut st = self.inner.state.lock().unwrap();
                // one tracked turn per session; a superseded entry can never
                // surface as a command_result for the wrong command
                st.pending.retain(|(sid, _)| sid != session_id);
                st.pending.push((session_id.clone(), command_id.clone()));
                Ok(ack(command_id, true, None, Some(session_id), None))
            }
            ClientMsg::Interrupt {
                command_id,
                session_id,
            } => self.inner.interrupt(command_id, session_id),
            ClientMsg::PermissionDecision {
                command_id,
                permission_id,
                decision,
                ..
            } => self
                .inner
                .permission_decision(command_id, permission_id, decision),
            ClientMsg::Hello { .. }
            | ClientMsg::Ping { .. }
            | ClientMsg::ListState { .. }
            | ClientMsg::HsHello { .. }
            | ClientMsg::HsProof { .. } => Err(AdapterError::Unsupported(
                "session-neutral messages stay with the engine".into(),
            )),
            // ADR-015 live messages for OpenCode sessions land here: no
            // end/mode equivalent (§4.4), MCP arrives with rich parts (step 5).
            // Subscriptions never do (the server owns them per connection).
            ClientMsg::SessionSubscribe { .. }
            | ClientMsg::SessionUnsubscribe { .. }
            | ClientMsg::SessionEnd { .. }
            | ClientMsg::SetPermissionMode { .. }
            | ClientMsg::McpAuth { .. }
            | ClientMsg::McpAuthCallback { .. }
            | ClientMsg::McpReconnect { .. } => Err(AdapterError::Unsupported(
                "not supported by this agent".into(),
            )),
        }
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

/// ADR-015 step 4: OpenCode behind the router, behaviour unchanged. Each
/// method delegates to the inherent one of the same name.
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
            let sessions = self.fetch_sessions()?;
            let agents = self.fetch_agents()?;
            let mut st = self.state.lock().unwrap();
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
            .filter_map(|info| Self::map_session(info, &self.id, &st.statuses))
            .collect())
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
        let healthy = self.state.lock().unwrap().healthy;
        Ok(arr
            .iter()
            .filter_map(|a| Self::map_agent(a, &self.id, healthy))
            .collect())
    }

    /// Maps a native session info object onto the protocol `Session`.
    /// `agent_id` is always `build`: OpenCode binds the agent per message,
    /// not per session (probe evidence has no agent field on sessions).
    fn map_session(
        info: &Json,
        host_id: &str,
        statuses: &HashMap<String, String>,
    ) -> Option<Session> {
        let id = json::get_str(info, "id")?.to_string();
        let (created, updated) = match json::get(info, "time") {
            Some(t) => (
                json::get_i64(t, "created").unwrap_or(0),
                json::get_i64(t, "updated").unwrap_or(0),
            ),
            None => (0, 0),
        };
        let status = statuses
            .get(&id)
            .cloned()
            .unwrap_or_else(|| "completed".to_string());
        Some(Session {
            id,
            agent_id: "build".to_string(),
            host_id: host_id.to_string(),
            title: json::get_str(info, "title")
                .or_else(|| json::get_str(info, "slug"))
                .unwrap_or("(untitled)")
                .to_string(),
            project: json::get_str(info, "directory")
                .or_else(|| json::get_str(info, "projectID"))
                .unwrap_or("")
                .to_string(),
            status,
            progress: 0,
            created_at: created,
            last_activity_at: updated,
            prompt: None,
            timeline: vec![],
            output: vec![],
            files: vec![],
            transcript: false,
        })
    }

    fn map_agent(v: &Json, host_id: &str, healthy: bool) -> Option<Agent> {
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
            live: None,
        })
    }

    fn upsert_session(list: &mut Vec<Session>, s: Session) {
        match list.iter_mut().find(|e| e.id == s.id) {
            Some(e) => *e = s,
            None => list.push(s),
        }
    }

    fn create_session(
        &self,
        command_id: &str,
        title: &str,
        prompt: &str,
        project: Option<&str>,
    ) -> Result<ServerMsg, AdapterError> {
        let mut url = format!("{}/session", self.endpoint);
        if let Some(p) = project
            && !p.is_empty()
        {
            url.push_str(&format!("?directory={}", urlencode(p)));
        }
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
        if let Some(mapped) = Self::map_session(&v, &self.id, &HashMap::new()) {
            let mut st = self.state.lock().unwrap();
            Self::upsert_session(&mut st.sessions, mapped);
        }
        if !prompt.trim().is_empty()
            && let Err(e) = self.prompt_accept(&session_id, prompt)
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

    /// `POST /session/:id/prompt_async` — acceptance is 204-with-empty-body
    /// or 200+JSON; anything else (the HTML-200 fallback above all) is a
    /// validation failure, never success (G4.2).
    fn prompt_accept(&self, session_id: &str, text: &str) -> Result<(), AdapterError> {
        let body = json::stringify(&json::obj(vec![(
            "parts",
            Json::Arr(vec![json::obj(vec![
                ("type", json::s("text")),
                ("text", json::s(text)),
            ])]),
        )]));
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

    fn interrupt(&self, command_id: &str, session_id: &str) -> Result<ServerMsg, AdapterError> {
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
            Ok(Json::Bool(true)) => Ok(ack(command_id, true, None, Some(session_id), None)),
            Ok(Json::Bool(false)) => Ok(ack(
                command_id,
                false,
                Some("native abort reported it did nothing (turn already finished?)".into()),
                Some(session_id),
                None,
            )),
            _ => Err(AdapterError::InvalidResponse(format!(
                "abort answered HTTP {} with a non-boolean body",
                resp.status
            ))),
        }
    }

    fn permission_decision(
        &self,
        command_id: &str,
        permission_id: &str,
        decision: &str,
    ) -> Result<ServerMsg, AdapterError> {
        let reply = match decision {
            "approve-once" => "once",
            "approve-session" => "always",
            "reject" => "reject",
            other => {
                return Err(AdapterError::Unsupported(format!(
                    "unknown decision '{other}'"
                )));
            }
        };
        let body = json::stringify(&json::obj(vec![("reply", json::s(reply))]));
        let resp = self.req(
            "POST",
            &format!("{}/permission/{permission_id}/reply", self.endpoint),
            Some(&body),
        )?;
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
                self.state.lock().unwrap().asked.remove(permission_id);
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
    fn sse_session(&self) -> Result<(), AdapterError> {
        let mut body = self.sse_connect()?;
        // Every (re)connect re-lists state: events lost in the gap are
        // replaced by the fresh snapshot (restart/recovery criterion).
        self.refresh()?;
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
        let reader_stream = stream
            .try_clone()
            .map_err(|e| AdapterError::Unknown(e.to_string()))?;
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
        let Some(kind) = self.normalize(event_id, etype, props) else {
            return;
        };
        // Snapshot the subscriber list so callbacks can (un)subscribe
        // without deadlocking against the notification.
        let subs = self.subs.lock().unwrap().clone();
        for (_, cb) in subs {
            cb(kind.clone());
        }
    }

    /// Native SSE event → protocol `EventKind`. Side effects (cache,
    /// pending, asked) happen under the state lock; `None` means the event
    /// has no protocol representation (`sync`, catalogs, comments…).
    fn normalize(&self, event_id: &str, etype: &str, props: Option<&Json>) -> Option<EventKind> {
        match etype {
            "session.created" | "session.updated" => {
                let props = props?;
                let info = json::get(props, "info")?;
                let created = etype == "session.created";
                let mut st = self.state.lock().unwrap();
                let session = Self::map_session(info, &self.id, &st.statuses)?;
                Self::upsert_session(&mut st.sessions, session.clone());
                drop(st);
                Some(if created {
                    EventKind::SessionCreated(session)
                } else {
                    EventKind::SessionUpdated(session)
                })
            }
            "session.status" | "session.idle" => {
                let props = props?;
                let sid = json::get_str(props, "sessionID")?.to_string();
                let proto = if etype == "session.idle" {
                    "completed".to_string()
                } else {
                    let t = json::get(props, "status")
                        .and_then(|s| json::get_str(s, "type"))
                        .unwrap_or("");
                    match t {
                        "idle" => "completed".to_string(),
                        "error" | "failed" => "interrupted".to_string(),
                        _ => "running".to_string(),
                    }
                };
                let mut st = self.state.lock().unwrap();
                st.statuses.insert(sid.clone(), proto.clone());
                let session = st.sessions.iter_mut().find(|s| s.id == sid)?;
                if session.status == proto {
                    return None;
                }
                session.status = proto;
                session.last_activity_at = now_ms();
                Some(EventKind::SessionUpdated(session.clone()))
            }
            "session.error" => {
                let props = props?;
                let sid = json::get_str(props, "sessionID")?.to_string();
                let msg = clip(
                    json::get(props, "error")
                        .map(err_text)
                        .unwrap_or_else(|| "unknown native error".to_string()),
                );
                let mut st = self.state.lock().unwrap();
                let cmd = st
                    .pending
                    .iter()
                    .position(|(s, _)| *s == sid)
                    .map(|i| st.pending.remove(i).1);
                match cmd {
                    // G4.1: the failure channel reaches the ORIGINAL command
                    Some(command_id) => Some(EventKind::CommandResult {
                        command_id,
                        status: "failed".to_string(),
                        error: Some(msg),
                        session_id: Some(sid),
                        permission_id: None,
                    }),
                    // No correlated command (native-side turn): honest
                    // activity entry — never a fabricated commandId.
                    None => Some(EventKind::Activity(ActivityEvent {
                        id: if event_id.is_empty() {
                            next_id("act")
                        } else {
                            event_id.to_string()
                        },
                        ts: now_ms(),
                        kind: "session".to_string(),
                        text: clip(format!("opencode: {msg}")),
                        ref_id: Some(sid),
                    })),
                }
            }
            "permission.asked" => {
                let props = props?;
                let native_id = json::get_str(props, "id")?.to_string();
                let sid = json::get_str(props, "sessionID").unwrap_or("").to_string();
                let permission = json::get_str(props, "permission").unwrap_or("permission");
                let patterns: Vec<String> = json::get(props, "patterns")
                    .and_then(json::as_arr)
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| json::as_str(p).map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let context = match json::get(props, "tool") {
                    Some(t) => format!(
                        "native {permission} (message {})",
                        json::get_str(t, "messageID").unwrap_or("?")
                    ),
                    None => format!("native {permission}"),
                };
                let now = now_ms();
                let pr = PermissionRequest {
                    // Native id doubles as the protocol id: per_* is already
                    // globally unique and keeps the mapping reversible.
                    id: native_id.clone(),
                    native_request_id: native_id.clone(),
                    session_id: sid,
                    agent_id: "build".to_string(),
                    host_id: self.id.clone(),
                    action: native_action(permission).to_string(),
                    title: format!("OpenCode permission: {permission}"),
                    command: if patterns.is_empty() {
                        permission.to_string()
                    } else {
                        format!("{permission} {}", patterns.join(" "))
                    },
                    args: patterns,
                    directory: None,
                    risk: "medium".to_string(),
                    status: "pending".to_string(),
                    supported_decisions: vec![
                        "approve-once".into(),
                        "approve-session".into(),
                        "reject".into(),
                    ],
                    revision: 1,
                    fingerprint: None,
                    requested_at: now,
                    // Engine TTL policy (120 s, same as mock permissions).
                    // Native has no TTL — divergence noted for wiring review.
                    expires_at: now + 120_000,
                    context,
                    decided_at: None,
                    decision: None,
                    tool_item_id: None,
                    options: vec![],
                    detail: None,
                };
                self.state
                    .lock()
                    .unwrap()
                    .asked
                    .insert(native_id, pr.clone());
                Some(EventKind::PermissionRequested(pr))
            }
            "permission.replied" => {
                let props = props?;
                let native_id = json::get_str(props, "id")
                    .or_else(|| json::get_str(props, "requestID"))
                    .or_else(|| json::get_str(props, "requestId"))?
                    .to_string();
                let mut st = self.state.lock().unwrap();
                // Unknown/untracked request → honest skip, no fabrication.
                let mut pr = st.asked.remove(&native_id)?;
                let reply = json::get_str(props, "reply");
                let (status, decision) = match reply {
                    Some("once") => ("approved", Some("approve-once")),
                    Some("always") => ("approved", Some("approve-session")),
                    Some("reject") => ("rejected", Some("reject")),
                    // Answered but the payload didn't say how: stale (still
                    // honest — no outcome is claimed).
                    _ => ("stale", None),
                };
                pr.status = status.to_string();
                pr.decision = decision.map(str::to_string);
                pr.decided_at = Some(now_ms());
                Some(EventKind::PermissionResolved(pr))
            }
            "message.part.updated" => {
                let props = props?;
                let part = json::get(props, "part")?;
                if json::get_str(part, "type") != Some("text") {
                    // tool-call parts: no protocol representation yet
                    // (capability `session.output` says so)
                    return None;
                }
                let text = json::get_str(part, "text")?;
                let sid = json::get_str(part, "sessionID")
                    .or_else(|| json::get_str(props, "sessionID"))?
                    .to_string();
                Some(EventKind::Activity(ActivityEvent {
                    id: if event_id.is_empty() {
                        next_id("act")
                    } else {
                        event_id.to_string()
                    },
                    ts: now_ms(),
                    kind: "session".to_string(),
                    text: text.to_string(),
                    ref_id: Some(sid),
                }))
            }
            // server.connected/heartbeat: handled by the connect+refresh
            // cycle; sync (aggregate replay wrapper) has no cursor — state
            // re-list replaces it (capability `event.replay` = unavailable).
            _ => None,
        }
    }
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
    }

    impl Default for Opts {
        fn default() -> Self {
            Opts {
                html_prompt: false,
                sse: None,
                chunked_sse: false,
                sse_hold: true,
                oversize_http: false,
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
    fn create_session_maps_directory_and_rides_the_first_prompt() {
        let f = start(Opts::default());
        let ad = adapter(&f);
        let msg = ad
            .send(&ClientMsg::CreateSession {
                command_id: "cmd-c".into(),
                agent_id: "build".into(),
                title: "Fresh".into(),
                prompt: "first turn".into(),
                project: Some(r"C:\proj\demo".into()),
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
        assert!(
            reqs.iter().any(|r| r.starts_with("POST /session ?directory=")
                || r.starts_with("POST /session ") /* path recorded without query */),
        );
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
        assert_eq!(err.code(), "invalid-response");
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
}
