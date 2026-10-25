//! Protocol v2 — exact Rust port of `@chernobyl/protocol` + the domain
//! permission authority. Validators mirror `isClientMessage` field-for-field;
//! entities serialize to the same JSON the mobile app already consumes.

use crate::json::{self, Json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const PROTOCOL_VERSION: u32 = 2;
/// ADR-015: the live transcript extension (additive, protocol stays 2).
pub const FEATURE_TRANSCRIPT: &str = "transcript.v1";
/// What `hello_ack.features` advertises.
pub const FEATURES: &[&str] = &[FEATURE_TRANSCRIPT];
pub const PERMISSION_TTL_MS: i64 = 120_000;
pub const HEARTBEAT_MS: u64 = 15_000;
pub const HEARTBEAT_MISS_LIMIT: u32 = 3;

static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `${prefix}_${base36(time)}${base36(seq)}` — same shape as the TS nextId.
pub fn next_id(prefix: &str) -> String {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    format!("{}_{:x}{}", prefix, now_ms(), seq)
}

/* ------------------------------------------------------------------ */
/* Entities                                                            */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, PartialEq)]
pub struct Host {
    pub id: String,
    pub name: String,
    pub kind: String, // local | remote
    pub platform: String,
    pub address: String,
    pub status: String, // connected | connecting | offline
    pub last_seen_at: i64,
    /// ADR-009 provenance: demo data from the mock seed/engine. Serialized
    /// only when true, so real-adapter output stays byte-identical.
    pub simulated: bool,
}

impl Host {
    pub fn to_json(&self) -> Json {
        let mut fields = vec![
            ("id", json::s(&self.id)),
            ("name", json::s(&self.name)),
            ("kind", json::s(&self.kind)),
            ("platform", json::s(&self.platform)),
            ("address", json::s(&self.address)),
            ("status", json::s(&self.status)),
            ("lastSeenAt", Json::Int(self.last_seen_at)),
        ];
        if self.simulated {
            fields.push(("simulated", Json::Bool(true)));
        }
        json::obj(fields)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub id: String,
    pub host_id: String,
    pub kind: String,
    pub name: String,
    pub state: String, // online | busy | idle | offline
    pub capabilities: Vec<String>,
    pub accepts_new_session: bool,
    /// ADR-009 provenance, same rule as `Host::simulated`.
    pub simulated: bool,
    /// ADR-015: present only for runtimes that stream a live transcript.
    pub live: Option<AgentLive>,
}

impl Agent {
    pub fn to_json(&self) -> Json {
        let mut fields = vec![
            ("id", json::s(&self.id)),
            ("hostId", json::s(&self.host_id)),
            ("kind", json::s(&self.kind)),
            ("name", json::s(&self.name)),
            ("state", json::s(&self.state)),
            (
                "capabilities",
                Json::Arr(self.capabilities.iter().map(|c| json::s(c)).collect()),
            ),
            ("acceptsNewSession", Json::Bool(self.accepts_new_session)),
        ];
        if self.simulated {
            fields.push(("simulated", Json::Bool(true)));
        }
        if let Some(live) = &self.live {
            fields.push(("live", live.to_json()));
        }
        json::obj(fields)
    }
}

/// ADR-015 `Agent.live`: what the phone may do with this runtime's sessions.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentLive {
    pub prompt_while_busy: &'static str, // merge | queue | none
    pub interrupt: bool,
    /// `session_end` supported (Claude).
    pub end: bool,
    /// Claude: default | acceptEdits | plan. Serialized only when non-empty.
    pub permission_modes: Vec<String>,
    /// Spawning runtimes only. Serialized only when non-empty.
    pub workspaces: Vec<LiveWorkspace>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiveWorkspace {
    pub id: String,
    pub label: String,
    pub path: String,
}

impl AgentLive {
    fn to_json(&self) -> Json {
        let mut fields = vec![
            ("transcript", Json::Bool(true)),
            ("promptWhileBusy", json::s(self.prompt_while_busy)),
            ("interrupt", Json::Bool(self.interrupt)),
            ("end", Json::Bool(self.end)),
        ];
        if !self.permission_modes.is_empty() {
            fields.push((
                "permissionModes",
                Json::Arr(self.permission_modes.iter().map(|m| json::s(m)).collect()),
            ));
        }
        if !self.workspaces.is_empty() {
            fields.push((
                "workspaces",
                Json::Arr(
                    self.workspaces
                        .iter()
                        .map(|w| {
                            json::obj(vec![
                                ("id", json::s(&w.id)),
                                ("label", json::s(&w.label)),
                                ("path", json::s(&w.path)),
                            ])
                        })
                        .collect(),
                ),
            ));
        }
        json::obj(fields)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputLine {
    pub id: String,
    pub ts: i64,
    pub stream: String, // stdout | stderr | system
    pub text: String,
}

impl OutputLine {
    pub fn to_json(&self) -> Json {
        json::obj(vec![
            ("id", json::s(&self.id)),
            ("ts", Json::Int(self.ts)),
            ("stream", json::s(&self.stream)),
            ("text", json::s(&self.text)),
        ])
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionStep {
    pub id: String,
    pub label: String,
    pub status: String, // done | active | pending
}

impl SessionStep {
    fn to_json(&self) -> Json {
        json::obj(vec![
            ("id", json::s(&self.id)),
            ("label", json::s(&self.label)),
            ("status", json::s(&self.status)),
        ])
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileChange {
    pub path: String,
    pub additions: i64,
    pub deletions: i64,
    pub status: String, // modified | added | removed
}

impl FileChange {
    fn to_json(&self) -> Json {
        json::obj(vec![
            ("path", json::s(&self.path)),
            ("additions", Json::Int(self.additions)),
            ("deletions", Json::Int(self.deletions)),
            ("status", json::s(&self.status)),
        ])
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    pub agent_id: String,
    pub host_id: String,
    pub title: String,
    pub project: String,
    pub status: String, // running | waiting | completed | interrupted | idle
    pub progress: i64,  // 0..100
    pub created_at: i64,
    pub last_activity_at: i64,
    pub prompt: Option<String>,
    pub timeline: Vec<SessionStep>,
    pub output: Vec<OutputLine>,
    pub files: Vec<FileChange>,
    /// ADR-015: a transcript stream exists for this session. Serialized only
    /// when true (the `simulated` precedent).
    pub transcript: bool,
}

impl Session {
    pub fn to_json(&self) -> Json {
        let mut fields = vec![
            ("id", json::s(&self.id)),
            ("agentId", json::s(&self.agent_id)),
            ("hostId", json::s(&self.host_id)),
            ("title", json::s(&self.title)),
            ("project", json::s(&self.project)),
            ("status", json::s(&self.status)),
            ("progress", Json::Int(self.progress)),
            ("createdAt", Json::Int(self.created_at)),
            ("lastActivityAt", Json::Int(self.last_activity_at)),
            (
                "timeline",
                Json::Arr(self.timeline.iter().map(|t| t.to_json()).collect()),
            ),
            (
                "output",
                Json::Arr(self.output.iter().map(|o| o.to_json()).collect()),
            ),
            (
                "files",
                Json::Arr(self.files.iter().map(|f| f.to_json()).collect()),
            ),
        ];
        if let Some(p) = &self.prompt {
            fields.push(("prompt", json::s(p)));
        }
        if self.transcript {
            fields.push(("transcript", Json::Bool(true)));
        }
        json::obj(fields)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub id: String,
    pub native_request_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub host_id: String,
    pub action: String, // shell | file-write | network | agent-control
    pub title: String,
    pub command: String,
    pub args: Vec<String>,
    pub directory: Option<String>,
    pub risk: String,   // low | medium | high
    pub status: String, // pending | stale | expired | approved | rejected
    pub supported_decisions: Vec<String>,
    pub revision: i64,
    pub fingerprint: Option<String>,
    pub requested_at: i64,
    pub expires_at: i64,
    pub context: String,
    pub decided_at: Option<i64>,
    pub decision: Option<String>,
    /// ADR-015 extras, serialized only when present (the `simulated`
    /// precedent): the transcript tool item this request belongs to, the
    /// runtime's own options (never empty when present, always one reject),
    /// and the request detail (built by the transcript.rs helpers).
    pub tool_item_id: Option<String>,
    pub options: Vec<PermissionOption>,
    pub detail: Option<Json>,
}

impl PermissionRequest {
    pub fn to_json(&self) -> Json {
        let mut fields = vec![
            ("id", json::s(&self.id)),
            ("nativeRequestId", json::s(&self.native_request_id)),
            ("sessionId", json::s(&self.session_id)),
            ("agentId", json::s(&self.agent_id)),
            ("hostId", json::s(&self.host_id)),
            ("action", json::s(&self.action)),
            ("title", json::s(&self.title)),
            ("command", json::s(&self.command)),
            (
                "args",
                Json::Arr(self.args.iter().map(|a| json::s(a)).collect()),
            ),
            ("risk", json::s(&self.risk)),
            ("status", json::s(&self.status)),
            (
                "supportedDecisions",
                Json::Arr(
                    self.supported_decisions
                        .iter()
                        .map(|d| json::s(d))
                        .collect(),
                ),
            ),
            ("revision", Json::Int(self.revision)),
            ("requestedAt", Json::Int(self.requested_at)),
            ("expiresAt", Json::Int(self.expires_at)),
            ("context", json::s(&self.context)),
        ];
        if let Some(d) = &self.directory {
            fields.push(("directory", json::s(d)));
        }
        if let Some(f) = &self.fingerprint {
            fields.push(("fingerprint", json::s(f)));
        }
        if let Some(t) = self.decided_at {
            fields.push(("decidedAt", Json::Int(t)));
        }
        if let Some(d) = &self.decision {
            fields.push(("decision", json::s(d)));
        }
        if let Some(t) = &self.tool_item_id {
            fields.push(("toolItemId", json::s(t)));
        }
        if !self.options.is_empty() {
            fields.push((
                "options",
                Json::Arr(self.options.iter().map(|o| o.to_json()).collect()),
            ));
        }
        if let Some(d) = &self.detail {
            fields.push(("detail", d.clone()));
        }
        json::obj(fields)
    }
}

/// ADR-015: one of the runtime's own choices for a permission request.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionOption {
    pub id: String,
    pub decision: String, // approve-once | approve-session | reject
    pub label: String,
    pub rule: Option<String>,
    pub scope: &'static str, // once | session | persistent
    pub destination: Option<String>,
    pub takes_message: bool,
    pub stops_turn: bool,
}

impl PermissionOption {
    pub fn to_json(&self) -> Json {
        let mut fields = vec![
            ("id", json::s(&self.id)),
            ("decision", json::s(&self.decision)),
            ("label", json::s(&self.label)),
            ("scope", json::s(self.scope)),
        ];
        if let Some(r) = &self.rule {
            fields.push(("rule", json::s(r)));
        }
        if let Some(d) = &self.destination {
            fields.push(("destination", json::s(d)));
        }
        if self.takes_message {
            fields.push(("takesMessage", Json::Bool(true)));
        }
        if self.stops_turn {
            fields.push(("stopsTurn", Json::Bool(true)));
        }
        json::obj(fields)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActivityEvent {
    pub id: String,
    pub ts: i64,
    pub kind: String, // session | permission | host | agent
    pub text: String,
    pub ref_id: Option<String>,
}

impl ActivityEvent {
    pub fn to_json(&self) -> Json {
        let mut fields = vec![
            ("id", json::s(&self.id)),
            ("ts", Json::Int(self.ts)),
            ("kind", json::s(&self.kind)),
            ("text", json::s(&self.text)),
        ];
        if let Some(r) = &self.ref_id {
            fields.push(("refId", json::s(r)));
        }
        json::obj(fields)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub hosts: Vec<Host>,
    pub agents: Vec<Agent>,
    pub sessions: Vec<Session>,
    pub permissions: Vec<PermissionRequest>,
    pub activity: Vec<ActivityEvent>,
}

impl Snapshot {
    pub fn to_json(&self) -> Json {
        json::obj(vec![
            (
                "hosts",
                Json::Arr(self.hosts.iter().map(|h| h.to_json()).collect()),
            ),
            (
                "agents",
                Json::Arr(self.agents.iter().map(|a| a.to_json()).collect()),
            ),
            (
                "sessions",
                Json::Arr(self.sessions.iter().map(|s| s.to_json()).collect()),
            ),
            (
                "permissions",
                Json::Arr(self.permissions.iter().map(|p| p.to_json()).collect()),
            ),
            (
                "activity",
                Json::Arr(self.activity.iter().map(|e| e.to_json()).collect()),
            ),
        ])
    }
}

/* ------------------------------------------------------------------ */
/* Client messages (trust boundary)                                    */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, PartialEq)]
pub enum ClientMsg {
    Hello {
        client_id: String,
        protocol: f64,
    },
    Ping {
        command_id: String,
    },
    CreateSession {
        command_id: String,
        agent_id: String,
        title: String,
        prompt: String,
        project: Option<String>,
    },
    SubmitPrompt {
        command_id: String,
        session_id: String,
        text: String,
    },
    Interrupt {
        command_id: String,
        session_id: String,
    },
    PermissionDecision {
        command_id: String,
        permission_id: String,
        decision: String,
        revision: f64,
        /// ADR-015: a PermissionOption id; its bucket must equal `decision`.
        option_id: Option<String>,
        /// ADR-015: free-text reason the model sees.
        message: Option<String>,
        /// ADR-015: question id → chosen labels or free text.
        answers: Vec<(String, Vec<String>)>,
    },
    ListState {
        command_id: String,
    },
    /// ADR-015 live sessions. Subscribe/unsubscribe are non-mutating (no
    /// journal row); the other five are persisted before execution.
    SessionSubscribe {
        command_id: String,
        session_id: String,
        epoch: Option<String>,
        after_seq: Option<u64>,
    },
    SessionUnsubscribe {
        command_id: String,
        session_id: String,
    },
    SessionEnd {
        command_id: String,
        session_id: String,
    },
    /// `mode` is whitelisted: default | acceptEdits | plan.
    SetPermissionMode {
        command_id: String,
        session_id: String,
        mode: String,
    },
    /// `flow` is whitelisted: desktop | phone-callback.
    McpAuth {
        command_id: String,
        session_id: String,
        server: String,
        flow: String,
    },
    /// `callback_url` carries a one-time OAuth code: it is journaled as
    /// "<redacted>" and must never be logged.
    McpAuthCallback {
        command_id: String,
        session_id: String,
        server: String,
        callback_url: String,
    },
    /// §4.7 "Reconnect a failed server".
    McpReconnect {
        command_id: String,
        session_id: String,
        server: String,
    },
    /// E2E handshake, step 1 (ADR-011): request to connect as `device_id`.
    /// Plaintext; the server answers with a fresh nonce+salt to bind the proof.
    /// `cnonce` is the client's fresh 16-byte nonce (base64), bound into both
    /// proofs and the keys. Optional here so the server can answer a missing
    /// or malformed one with an explicit `hs_err` (security review 2026-10-25).
    HsHello {
        device_id: String,
        cnonce: Option<String>,
    },
    /// E2E handshake, step 2: HMAC proof of PSK knowledge over the server's
    /// fresh nonce+salt (base64 of the 32-byte HMAC).
    HsProof {
        proof: String,
    },
}

impl ClientMsg {
    pub fn command_id(&self) -> Option<&str> {
        match self {
            ClientMsg::Hello { .. } | ClientMsg::HsHello { .. } | ClientMsg::HsProof { .. } => None,
            ClientMsg::Ping { command_id }
            | ClientMsg::CreateSession { command_id, .. }
            | ClientMsg::SubmitPrompt { command_id, .. }
            | ClientMsg::Interrupt { command_id, .. }
            | ClientMsg::PermissionDecision { command_id, .. }
            | ClientMsg::ListState { command_id }
            | ClientMsg::SessionSubscribe { command_id, .. }
            | ClientMsg::SessionUnsubscribe { command_id, .. }
            | ClientMsg::SessionEnd { command_id, .. }
            | ClientMsg::SetPermissionMode { command_id, .. }
            | ClientMsg::McpAuth { command_id, .. }
            | ClientMsg::McpAuthCallback { command_id, .. }
            | ClientMsg::McpReconnect { command_id, .. } => Some(command_id),
        }
    }

    pub fn is_mutating(&self) -> bool {
        matches!(
            self,
            ClientMsg::CreateSession { .. }
                | ClientMsg::SubmitPrompt { .. }
                | ClientMsg::Interrupt { .. }
                | ClientMsg::PermissionDecision { .. }
                | ClientMsg::SessionEnd { .. }
                | ClientMsg::SetPermissionMode { .. }
                | ClientMsg::McpAuth { .. }
                | ClientMsg::McpAuthCallback { .. }
                | ClientMsg::McpReconnect { .. }
        )
    }
}

fn is_str(v: &Json) -> Option<&str> {
    json::as_str(v)
}

fn is_num(v: &Json) -> Option<f64> {
    json::as_f64(v)
}

/// TS `s.length <= max`: UTF-16 code units, so both validators agree on any
/// text (UTF-8 bytes are never fewer, hence the fast path).
fn fits(s: &str, max: usize) -> bool {
    s.len() <= max || s.encode_utf16().count() <= max
}

/// ADR-015 optional string: absent → `Some(None)`; present but not a string
/// of at most `max` units → `None` (the whole message is malformed).
fn opt_str(obj: &Json, key: &str, max: usize) -> Option<Option<String>> {
    match json::get(obj, key) {
        None => Some(None),
        Some(v) => json::as_str(v)
            .filter(|s| fits(s, max))
            .map(|s| Some(s.to_string())),
    }
}

/// TS `isSeq`: absent, or a non-negative safe integer (`3.0` counts, `3.5`
/// does not), mirroring `Number.isSafeInteger`.
fn opt_seq(obj: &Json, key: &str) -> Option<Option<u64>> {
    const MAX_SAFE: i64 = 9_007_199_254_740_991;
    match json::get(obj, key) {
        None => Some(None),
        Some(Json::Int(n)) if (0..=MAX_SAFE).contains(n) => Some(Some(*n as u64)),
        Some(Json::Float(f)) if f.fract() == 0.0 && *f >= 0.0 && *f <= MAX_SAFE as f64 => {
            Some(Some(*f as u64))
        }
        Some(_) => None,
    }
}

/// ADR-015 §3.4 `answers`: absent → empty; otherwise an object of ≤ 8
/// question ids, each an array of ≤ 16 strings of ≤ 1,024 units.
fn opt_answers(obj: &Json) -> Option<Vec<(String, Vec<String>)>> {
    let Some(v) = json::get(obj, "answers") else {
        return Some(Vec::new());
    };
    let Json::Obj(pairs) = v else {
        return None;
    };
    if pairs.len() > 8 {
        return None;
    }
    pairs
        .iter()
        .map(|(id, list)| {
            let list = json::as_arr(list).filter(|l| l.len() <= 16)?;
            let answers = list
                .iter()
                .map(|a| {
                    json::as_str(a)
                        .filter(|s| fits(s, 1024))
                        .map(str::to_string)
                })
                .collect::<Option<Vec<_>>>()?;
            Some((id.clone(), answers))
        })
        .collect()
}

/// Validates an untrusted payload as a ClientMessage. Mirrors the TS
/// `isClientMessage` exactly: same field requirements, same strictness
/// (every required string must be non-empty, decisions are whitelisted).
pub fn parse_client_message(raw: &str) -> Option<ClientMsg> {
    let v = json::parse(raw).ok()?;
    let obj = match &v {
        Json::Obj(_) => &v,
        _ => return None,
    };
    let typ = is_str(json::get(obj, "type")?)?;
    // TS isStr: non-empty strings only, on every required string field
    let req = |k: &str| -> Option<String> {
        json::as_str(json::get(obj, k)?)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    match typ {
        "hello" => Some(ClientMsg::Hello {
            client_id: req("clientId")?,
            protocol: is_num(json::get(obj, "protocol")?)?,
        }),
        "ping" => Some(ClientMsg::Ping {
            command_id: req("commandId")?,
        }),
        "create_session" => Some(ClientMsg::CreateSession {
            command_id: req("commandId")?,
            agent_id: req("agentId")?,
            title: req("title")?,
            prompt: req("prompt")?,
            project: json::get(obj, "project")
                .and_then(json::as_str)
                .map(str::to_string),
        }),
        "submit_prompt" => Some(ClientMsg::SubmitPrompt {
            command_id: req("commandId")?,
            session_id: req("sessionId")?,
            // ADR-015 §3.4: 32,000 characters. Counting chars keeps the
            // companion never stricter than the phone, whose zod counts UTF-16
            // units (an emoji is 2 there, 1 here); bytes ≥ chars, hence the fast path.
            text: req("text").filter(|t| t.len() <= 32_000 || t.chars().count() <= 32_000)?,
        }),
        "interrupt" => Some(ClientMsg::Interrupt {
            command_id: req("commandId")?,
            session_id: req("sessionId")?,
        }),
        "permission_decision" => {
            let decision = req("decision")?;
            // decisions are whitelisted at the trust boundary (G2: never
            // translated downstream)
            if !matches!(
                decision.as_str(),
                "approve-once" | "approve-session" | "reject"
            ) {
                return None;
            }
            Some(ClientMsg::PermissionDecision {
                command_id: req("commandId")?,
                permission_id: req("permissionId")?,
                decision,
                revision: is_num(json::get(obj, "revision")?)?,
                option_id: opt_str(obj, "optionId", 128)?,
                message: opt_str(obj, "message", 4096)?,
                answers: opt_answers(obj)?,
            })
        }
        "list_state" => Some(ClientMsg::ListState {
            command_id: req("commandId")?,
        }),
        "session_subscribe" => Some(ClientMsg::SessionSubscribe {
            command_id: req("commandId")?,
            session_id: req("sessionId")?,
            // a fresh view's epoch "" is fine: it never matches, so a snapshot follows
            epoch: opt_str(obj, "epoch", 64)?,
            after_seq: opt_seq(obj, "afterSeq")?,
        }),
        "session_unsubscribe" => Some(ClientMsg::SessionUnsubscribe {
            command_id: req("commandId")?,
            session_id: req("sessionId")?,
        }),
        "session_end" => Some(ClientMsg::SessionEnd {
            command_id: req("commandId")?,
            session_id: req("sessionId")?,
        }),
        "set_permission_mode" => {
            let mode = req("mode")?;
            // whitelist: bypassPermissions / dontAsk / auto are never
            // reachable from a phone
            if !matches!(mode.as_str(), "default" | "acceptEdits" | "plan") {
                return None;
            }
            Some(ClientMsg::SetPermissionMode {
                command_id: req("commandId")?,
                session_id: req("sessionId")?,
                mode,
            })
        }
        "mcp_auth" => {
            let flow = req("flow")?;
            if !matches!(flow.as_str(), "desktop" | "phone-callback") {
                return None;
            }
            Some(ClientMsg::McpAuth {
                command_id: req("commandId")?,
                session_id: req("sessionId")?,
                server: req("server").filter(|s| fits(s, 256))?,
                flow,
            })
        }
        // shape only; host, port, path and state are checked by the adapter (§4.7)
        "mcp_auth_callback" => Some(ClientMsg::McpAuthCallback {
            command_id: req("commandId")?,
            session_id: req("sessionId")?,
            server: req("server").filter(|s| fits(s, 256))?,
            callback_url: req("callbackUrl")
                .filter(|u| u.starts_with("http://") && fits(u, 4096))?,
        }),
        "mcp_reconnect" => Some(ClientMsg::McpReconnect {
            command_id: req("commandId")?,
            session_id: req("sessionId")?,
            server: req("server").filter(|s| fits(s, 256))?,
        }),
        "hs_hello" => Some(ClientMsg::HsHello {
            device_id: req("deviceId")?,
            cnonce: json::get(obj, "cnonce")
                .and_then(json::as_str)
                .map(str::to_string),
        }),
        "hs_proof" => Some(ClientMsg::HsProof {
            proof: req("proof")?,
        }),
        _ => None,
    }
}

/* ------------------------------------------------------------------ */
/* Server messages (serialization)                                     */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone)]
pub struct Envelope {
    pub seq: i64,
    pub event_id: String,
    pub occurred_at: i64,
}

// HelloAck and Event carry a full snapshot/entry; boxing them would churn
// every match arm for a message that is already heap-allocated on the wire.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ServerMsg {
    HelloAck {
        protocol: u32,
        server_time: i64,
        snapshot: Snapshot,
        last_seq: i64,
        /// ADR-015: protocol extensions this server understands ([`FEATURES`]).
        features: &'static [&'static str],
    },
    Pong {
        command_id: String,
    },
    Ack {
        command_id: String,
        ok: bool,
        error: Option<String>,
        duplicate: bool,
        session_id: Option<String>,
        permission_id: Option<String>,
    },
    State {
        snapshot: Snapshot,
        last_seq: i64,
    },
    Event {
        kind: EventKind,
        env: Envelope,
    },
    /// E2E handshake, server reply to `hs_hello`: fresh nonce+salt (base64) for
    /// the client to bind its proof to.
    HsChallenge {
        nonce: String,
        salt: String,
    },
    /// E2E handshake, server reply to a valid `hs_proof`: the server's own proof
    /// (base64), letting the client mutually authenticate the host.
    HsOk {
        server_proof: String,
    },
    /// E2E handshake failure (unknown/expired/revoked device, bad proof).
    HsErr {
        reason: String,
    },
    /// ADR-015: contiguous transcript ops for one session, sent only to
    /// subscribers and never journaled. Each op is serialized once when it
    /// enters the ring; `to_text` splices them verbatim.
    Transcript {
        session_id: String,
        epoch: String,
        ops: Vec<String>,
    },
    /// ADR-015: `body` holds items, truncatedBefore, turns, state and mcp.
    TranscriptSnapshot {
        session_id: String,
        epoch: String,
        seq: u64,
        body: Json,
    },
}

#[derive(Debug, Clone)]
pub enum EventKind {
    HostUpdated(Host),
    AgentUpdated(Agent),
    SessionCreated(Session),
    SessionUpdated(Session),
    SessionOutput {
        session_id: String,
        lines: Vec<OutputLine>,
        progress: i64,
        status: String,
    },
    PermissionRequested(PermissionRequest),
    PermissionResolved(PermissionRequest),
    Activity(ActivityEvent),
    CommandProgress {
        command_id: String,
        state: String, // accepted | running
        session_id: Option<String>,
    },
    CommandResult {
        command_id: String,
        status: String, // completed|failed|cancelled|expired|unknown
        error: Option<String>,
        session_id: Option<String>,
        permission_id: Option<String>,
    },
}

impl EventKind {
    pub fn type_str(&self) -> &'static str {
        match self {
            EventKind::HostUpdated(_) => "host_updated",
            EventKind::AgentUpdated(_) => "agent_updated",
            EventKind::SessionCreated(_) => "session_created",
            EventKind::SessionUpdated(_) => "session_updated",
            EventKind::SessionOutput { .. } => "session_output",
            EventKind::PermissionRequested(_) => "permission_requested",
            EventKind::PermissionResolved(_) => "permission_resolved",
            EventKind::Activity(_) => "activity",
            EventKind::CommandProgress { .. } => "command_progress",
            EventKind::CommandResult { .. } => "command_result",
        }
    }
}

impl ServerMsg {
    pub fn ack_ok(command_id: &str, session_id: Option<&str>) -> ServerMsg {
        ServerMsg::Ack {
            command_id: command_id.to_string(),
            ok: true,
            error: None,
            duplicate: false,
            session_id: session_id.map(str::to_string),
            permission_id: None,
        }
    }

    pub fn ack_err(command_id: &str, error: &str) -> ServerMsg {
        ServerMsg::Ack {
            command_id: command_id.to_string(),
            ok: false,
            error: Some(error.to_string()),
            duplicate: false,
            session_id: None,
            permission_id: None,
        }
    }

    pub fn duplicate_of(cached: &ServerMsg) -> ServerMsg {
        match cached {
            ServerMsg::Ack {
                command_id,
                ok,
                error,
                session_id,
                permission_id,
                ..
            } => ServerMsg::Ack {
                command_id: command_id.clone(),
                ok: *ok,
                error: error.clone(),
                duplicate: true,
                session_id: session_id.clone(),
                permission_id: permission_id.clone(),
            },
            other => other.clone(),
        }
    }

    /// Serializes exactly the JSON shape the app's parseServerMessage expects.
    pub fn to_json(&self) -> Json {
        match self {
            ServerMsg::HelloAck {
                protocol,
                server_time,
                snapshot,
                last_seq,
                features,
            } => json::obj(vec![
                ("type", json::s("hello_ack")),
                ("protocol", Json::Int(*protocol as i64)),
                ("serverTime", Json::Int(*server_time)),
                ("snapshot", snapshot.to_json()),
                ("lastSeq", Json::Int(*last_seq)),
                (
                    "features",
                    Json::Arr(features.iter().map(|f| json::s(f)).collect()),
                ),
            ]),
            ServerMsg::Pong { command_id } => json::obj(vec![
                ("type", json::s("pong")),
                ("commandId", json::s(command_id)),
            ]),
            ServerMsg::Ack {
                command_id,
                ok,
                error,
                duplicate,
                session_id,
                permission_id,
            } => {
                let mut fields = vec![
                    ("type", json::s("ack")),
                    ("commandId", json::s(command_id)),
                    ("ok", Json::Bool(*ok)),
                ];
                if let Some(e) = error {
                    fields.push(("error", json::s(e)));
                }
                if *duplicate {
                    fields.push(("duplicate", Json::Bool(true)));
                }
                if let Some(s) = session_id {
                    fields.push(("sessionId", json::s(s)));
                }
                if let Some(p) = permission_id {
                    fields.push(("permissionId", json::s(p)));
                }
                json::obj(fields)
            }
            ServerMsg::State { snapshot, last_seq } => json::obj(vec![
                ("type", json::s("state")),
                ("snapshot", snapshot.to_json()),
                ("lastSeq", Json::Int(*last_seq)),
            ]),
            ServerMsg::Event { kind, env } => {
                let mut fields = vec![
                    ("type", json::s(kind.type_str())),
                    ("seq", Json::Int(env.seq)),
                    ("eventId", json::s(&env.event_id)),
                    ("occurredAt", Json::Int(env.occurred_at)),
                ];
                match kind {
                    EventKind::HostUpdated(h) => fields.push(("host", h.to_json())),
                    EventKind::AgentUpdated(a) => fields.push(("agent", a.to_json())),
                    EventKind::SessionCreated(s) | EventKind::SessionUpdated(s) => {
                        fields.push(("session", s.to_json()))
                    }
                    EventKind::SessionOutput {
                        session_id,
                        lines,
                        progress,
                        status,
                    } => {
                        fields.push(("sessionId", json::s(session_id)));
                        fields.push((
                            "lines",
                            Json::Arr(lines.iter().map(|l| l.to_json()).collect()),
                        ));
                        fields.push(("progress", Json::Int(*progress)));
                        fields.push(("status", json::s(status)));
                    }
                    EventKind::PermissionRequested(p) | EventKind::PermissionResolved(p) => {
                        fields.push(("permission", p.to_json()))
                    }
                    EventKind::Activity(e) => fields.push(("event", e.to_json())),
                    EventKind::CommandProgress {
                        command_id,
                        state,
                        session_id,
                    } => {
                        fields.push(("commandId", json::s(command_id)));
                        fields.push(("state", json::s(state)));
                        if let Some(s) = session_id {
                            fields.push(("sessionId", json::s(s)));
                        }
                    }
                    EventKind::CommandResult {
                        command_id,
                        status,
                        error,
                        session_id,
                        permission_id,
                    } => {
                        fields.push(("commandId", json::s(command_id)));
                        fields.push(("status", json::s(status)));
                        if let Some(e) = error {
                            fields.push(("error", json::s(e)));
                        }
                        if let Some(s) = session_id {
                            fields.push(("sessionId", json::s(s)));
                        }
                        if let Some(p) = permission_id {
                            fields.push(("permissionId", json::s(p)));
                        }
                    }
                }
                json::obj(fields)
            }
            ServerMsg::HsChallenge { nonce, salt } => json::obj(vec![
                ("type", json::s("hs_challenge")),
                ("nonce", json::s(nonce)),
                ("salt", json::s(salt)),
            ]),
            ServerMsg::HsOk { server_proof } => json::obj(vec![
                ("type", json::s("hs_ok")),
                ("serverProof", json::s(server_proof)),
            ]),
            ServerMsg::HsErr { reason } => json::obj(vec![
                ("type", json::s("hs_err")),
                ("reason", json::s(reason)),
            ]),
            // the ops are already text: re-parse only here (tests, benches)
            ServerMsg::Transcript { .. } => json::parse(&self.to_text()).unwrap_or(Json::Null),
            ServerMsg::TranscriptSnapshot {
                session_id,
                epoch,
                seq,
                body,
            } => {
                let mut fields = vec![
                    ("type".to_string(), json::s("transcript_snapshot")),
                    ("sessionId".to_string(), json::s(session_id)),
                    ("epoch".to_string(), json::s(epoch)),
                    ("seq".to_string(), Json::Int(*seq as i64)),
                ];
                if let Json::Obj(pairs) = body {
                    fields.extend(pairs.iter().cloned());
                }
                Json::Obj(fields)
            }
        }
    }

    pub fn to_text(&self) -> String {
        match self {
            // splice the pre-serialized ops: a fan-out never re-encodes them
            ServerMsg::Transcript {
                session_id,
                epoch,
                ops,
            } => {
                let mut out = String::from("{\"type\":\"transcript\",\"sessionId\":");
                json::write_str(&mut out, session_id);
                out.push_str(",\"epoch\":");
                json::write_str(&mut out, epoch);
                out.push_str(",\"ops\":[");
                out.push_str(&ops.join(","));
                out.push_str("]}");
                out
            }
            _ => json::stringify(&self.to_json()),
        }
    }
}

/* ------------------------------------------------------------------ */
/* Permission decision authority (port of @chernobyl/domain)           */
/* ------------------------------------------------------------------ */

#[derive(Debug, PartialEq)]
pub enum DecisionError {
    IncompleteMetadata,
    NotPending,
    Expired,
    StaleRevision,
    UnsupportedDecision,
}

impl DecisionError {
    pub fn message(&self) -> &'static str {
        match self {
            DecisionError::IncompleteMetadata => "permission request metadata is incomplete",
            DecisionError::NotPending => "request is no longer pending",
            DecisionError::Expired => "request expired before a decision was made",
            DecisionError::StaleRevision => "request changed — review it again",
            DecisionError::UnsupportedDecision => "decision is not supported by this runtime",
        }
    }
}

/// The single fail-closed authority (PRD §13, G2). Pure: never mutates.
pub fn evaluate_permission_decision(
    request: &PermissionRequest,
    decision: &str,
    revision: f64,
    now: i64,
) -> Result<(), DecisionError> {
    if request.native_request_id.is_empty() || request.supported_decisions.is_empty() {
        return Err(DecisionError::IncompleteMetadata);
    }
    if request.status != "pending" {
        return Err(DecisionError::NotPending);
    }
    if now >= request.expires_at {
        return Err(DecisionError::Expired);
    }
    if revision != request.revision as f64 {
        return Err(DecisionError::StaleRevision);
    }
    if !request.supported_decisions.iter().any(|d| d == decision) {
        return Err(DecisionError::UnsupportedDecision);
    }
    Ok(())
}

/* ------------------------------------------------------------------ */
/* Tests — mirror the TS validator + domain tests                      */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_id_shape() {
        let id = next_id("cmd");
        assert!(id.starts_with("cmd_"));
        assert!(id.len() > 4);
    }

    #[test]
    fn accepts_valid_client_messages() {
        assert!(
            parse_client_message(r#"{"type":"hello","clientId":"app_1","protocol":2}"#).is_some()
        );
        assert!(parse_client_message(r#"{"type":"ping","commandId":"c1"}"#).is_some());
        assert!(
            parse_client_message(r#"{"type":"create_session","commandId":"c1","agentId":"a1","title":"t","prompt":"p"}"#)
                .is_some()
        );
        assert!(
            parse_client_message(
                r#"{"type":"submit_prompt","commandId":"c1","sessionId":"s1","text":"hi"}"#
            )
            .is_some()
        );
        assert!(
            parse_client_message(r#"{"type":"interrupt","commandId":"c1","sessionId":"s1"}"#)
                .is_some()
        );
        assert!(
            parse_client_message(
                r#"{"type":"permission_decision","commandId":"c1","permissionId":"p1","decision":"approve-once","revision":1}"#
            )
            .is_some()
        );
        assert!(parse_client_message(r#"{"type":"list_state","commandId":"c1"}"#).is_some());
        // optional project field
        assert!(
            parse_client_message(
                r#"{"type":"create_session","commandId":"c","agentId":"a","title":"t","prompt":"p","project":"x"}"#
            )
            .is_some()
        );
    }

    #[test]
    fn rejects_malformed_client_messages() {
        // unknown type
        assert!(parse_client_message(r#"{"type":"evil"}"#).is_none());
        // missing fields
        assert!(parse_client_message(r#"{"type":"hello","clientId":"c"}"#).is_none());
        assert!(parse_client_message(r#"{"type":"ping"}"#).is_none());
        assert!(
            parse_client_message(
                r#"{"type":"create_session","commandId":"c","agentId":"a","title":"t"}"#
            )
            .is_none()
        );
        // empty strings rejected (TS isStr requires length > 0)
        assert!(
            parse_client_message(r#"{"type":"interrupt","commandId":"","sessionId":"s"}"#)
                .is_none()
        );
        // non-string commandId
        assert!(parse_client_message(r#"{"type":"ping","commandId":7}"#).is_none());
        // bad decision value
        assert!(parse_client_message(
            r#"{"type":"permission_decision","commandId":"c","permissionId":"p","decision":"yes","revision":1}"#
        )
        .is_none());
        // missing revision (v2 shape enforcement)
        assert!(parse_client_message(
            r#"{"type":"permission_decision","commandId":"c","permissionId":"p","decision":"reject"}"#
        )
        .is_none());
        // non-object
        assert!(parse_client_message("[1,2]").is_none());
        assert!(parse_client_message("null").is_none());
    }

    #[test]
    fn hello_ack_serialization_shape() {
        let msg = ServerMsg::HelloAck {
            protocol: PROTOCOL_VERSION,
            server_time: 123,
            snapshot: Snapshot::default(),
            last_seq: 7,
            features: FEATURES,
        };
        let text = msg.to_text();
        let v: Json = json::parse(&text).unwrap();
        assert_eq!(json::get(&v, "type").unwrap(), &json::s("hello_ack"));
        assert_eq!(json::get(&v, "protocol").unwrap(), &Json::Int(2));
        assert_eq!(json::get(&v, "lastSeq").unwrap(), &Json::Int(7));
        let snap = json::get(&v, "snapshot").unwrap();
        assert!(json::get(snap, "hosts").is_some());
        assert!(json::get(snap, "permissions").is_some());
    }

    #[test]
    fn ack_duplicate_shape() {
        let cached = ServerMsg::ack_ok("c1", Some("s9"));
        let dup = ServerMsg::duplicate_of(&cached);
        let v = json::parse(&dup.to_text()).unwrap();
        assert_eq!(json::get(&v, "duplicate").unwrap(), &Json::Bool(true));
        assert_eq!(json::get(&v, "ok").unwrap(), &Json::Bool(true));
        assert_eq!(json::get(&v, "sessionId").unwrap(), &json::s("s9"));
    }

    #[test]
    fn envelope_stamped_events_serialize() {
        let msg = ServerMsg::Event {
            kind: EventKind::CommandProgress {
                command_id: "c1".into(),
                state: "accepted".into(),
                session_id: Some("s1".into()),
            },
            env: Envelope {
                seq: 5,
                event_id: "evt_x".into(),
                occurred_at: 1000,
            },
        };
        let v = json::parse(&msg.to_text()).unwrap();
        assert_eq!(json::get(&v, "seq").unwrap(), &Json::Int(5));
        assert_eq!(json::get(&v, "eventId").unwrap(), &json::s("evt_x"));
        assert_eq!(json::get(&v, "occurredAt").unwrap(), &Json::Int(1000));
        assert_eq!(json::get(&v, "state").unwrap(), &json::s("accepted"));
    }

    fn sample_permission() -> PermissionRequest {
        PermissionRequest {
            id: "p1".into(),
            native_request_id: "p1".into(),
            session_id: "s1".into(),
            agent_id: "a1".into(),
            host_id: "h1".into(),
            action: "shell".into(),
            title: "Run shell command".into(),
            command: "npm install".into(),
            args: vec![],
            directory: None,
            risk: "medium".into(),
            status: "pending".into(),
            supported_decisions: vec![
                "approve-once".into(),
                "approve-session".into(),
                "reject".into(),
            ],
            revision: 1,
            fingerprint: None,
            requested_at: 0,
            expires_at: 10_000,
            context: String::new(),
            decided_at: None,
            decision: None,
            tool_item_id: None,
            options: vec![],
            detail: None,
        }
    }

    #[test]
    fn permission_decision_authority_fail_closed() {
        let base = sample_permission();
        // ok path
        assert_eq!(
            evaluate_permission_decision(&base, "approve-once", 1.0, 5_000),
            Ok(())
        );
        // incomplete metadata
        let mut missing = base.clone();
        missing.native_request_id = String::new();
        assert_eq!(
            evaluate_permission_decision(&missing, "reject", 1.0, 5_000),
            Err(DecisionError::IncompleteMetadata)
        );
        let mut no_decisions = base.clone();
        no_decisions.supported_decisions.clear();
        assert_eq!(
            evaluate_permission_decision(&no_decisions, "reject", 1.0, 5_000),
            Err(DecisionError::IncompleteMetadata)
        );
        // not pending
        let mut resolved = base.clone();
        resolved.status = "approved".into();
        assert_eq!(
            evaluate_permission_decision(&resolved, "reject", 1.0, 5_000),
            Err(DecisionError::NotPending)
        );
        // expired
        assert_eq!(
            evaluate_permission_decision(&base, "reject", 1.0, 10_000),
            Err(DecisionError::Expired)
        );
        // stale revision
        assert_eq!(
            evaluate_permission_decision(&base, "reject", 3.0, 5_000),
            Err(DecisionError::StaleRevision)
        );
        // unsupported decision
        let mut limited = base.clone();
        limited.supported_decisions = vec!["reject".into()];
        assert_eq!(
            evaluate_permission_decision(&limited, "approve-once", 1.0, 5_000),
            Err(DecisionError::UnsupportedDecision)
        );
    }

    #[test]
    fn entity_serialization_round_trips() {
        let h = Host {
            id: "h1".into(),
            name: "n".into(),
            kind: "local".into(),
            platform: "win".into(),
            address: "127.0.0.1".into(),
            status: "connected".into(),
            last_seen_at: 42,
            simulated: false,
        };
        let text = json::stringify(&h.to_json());
        let v = json::parse(&text).unwrap();
        assert_eq!(json::get(&v, "lastSeenAt").unwrap(), &Json::Int(42));
    }

    #[test]
    fn simulated_key_is_serialized_only_when_true() {
        // real-adapter entities (simulated: false) keep today's exact bytes
        let mut h = Host {
            id: "h1".into(),
            name: "n".into(),
            kind: "local".into(),
            platform: "win".into(),
            address: "127.0.0.1".into(),
            status: "connected".into(),
            last_seen_at: 42,
            simulated: false,
        };
        let mut a = Agent {
            id: "build".into(),
            host_id: "oc-1".into(),
            kind: "opencode".into(),
            name: "build".into(),
            state: "online".into(),
            capabilities: vec![],
            accepts_new_session: true,
            simulated: false,
            live: None,
        };
        assert!(!json::stringify(&h.to_json()).contains("simulated"));
        assert!(!json::stringify(&a.to_json()).contains("simulated"));
        h.simulated = true;
        a.simulated = true;
        assert!(json::stringify(&h.to_json()).ends_with(",\"simulated\":true}"));
        assert!(json::stringify(&a.to_json()).ends_with(",\"simulated\":true}"));
    }

    #[test]
    fn hostile_json_at_the_trust_boundary_is_none_not_a_panic() {
        let bomb = format!("{}{}", "[".repeat(64), "]".repeat(64));
        assert!(parse_client_message(&bomb).is_none());
        // duplicate keys
        assert!(
            parse_client_message(r#"{"type":"ping","commandId":"a","commandId":"b"}"#).is_none()
        );
        // raw control char inside a string
        assert!(parse_client_message("{\"type\":\"ping\",\"commandId\":\"\u{1}\"}").is_none());
        // lone surrogate escape
        assert!(parse_client_message(r#"{"type":"ping","commandId":"\ud800"}"#).is_none());
        // trailing garbage after a valid value
        assert!(parse_client_message("{\"type\":\"ping\",\"commandId\":\"c\"} x").is_none());
        // truncated document
        assert!(parse_client_message(r#"{"type":"ping","commandId":"c"#).is_none());
        // non-object JSON scalars
        assert!(parse_client_message("123").is_none());
        assert!(parse_client_message("\"str\"").is_none());
        assert!(parse_client_message("true").is_none());
        // type must be a non-empty string
        assert!(parse_client_message(r#"{"type":7}"#).is_none());
        assert!(parse_client_message(r#"{"type":""}"#).is_none());
        // ADR-015: bad afterSeq (2^53 is past Number.isSafeInteger)
        for seq in ["-1", "1.5", "1e999", "\"3\"", "null", "9007199254740992"] {
            let msg = format!(
                r#"{{"type":"session_subscribe","commandId":"c","sessionId":"s","afterSeq":{seq}}}"#
            );
            assert!(parse_client_message(&msg).is_none(), "afterSeq {seq}");
        }
        // ADR-015: oversize answers (9 ids, 17 answers, a 1,025-unit answer)
        let decision = |answers: String| {
            format!(
                r#"{{"type":"permission_decision","commandId":"c","permissionId":"p","decision":"approve-once","revision":1,"answers":{answers}}}"#
            )
        };
        let nine: Vec<String> = (0..9).map(|i| format!(r#""q{i}":["a"]"#)).collect();
        for answers in [
            format!("{{{}}}", nine.join(",")),
            format!(r#"{{"q":[{}]}}"#, vec![r#""a""#; 17].join(",")),
            format!(r#"{{"q":["{}"]}}"#, "x".repeat(1025)),
        ] {
            assert!(parse_client_message(&decision(answers)).is_none());
        }
        // ADR-015: a phone can never select a permission-bypassing mode
        for mode in ["bypassPermissions", "dontAsk", "auto", "Default", ""] {
            let msg = format!(
                r#"{{"type":"set_permission_mode","commandId":"c","sessionId":"s","mode":"{mode}"}}"#
            );
            assert!(parse_client_message(&msg).is_none(), "mode {mode}");
        }
    }

    #[test]
    fn protocol_number_field_follows_is_num_boundaries() {
        // is_num = finite number: ints and finite floats parse; strings,
        // non-finite exponents, and missing values are None. Version
        // mismatch itself is rejected later by the server's version gate.
        let hello =
            |proto: &str| format!(r#"{{"type":"hello","clientId":"c","protocol":{proto}}}"#);
        assert!(parse_client_message(&hello("2")).is_some());
        assert!(parse_client_message(&hello("2.5")).is_some());
        assert!(parse_client_message(&hello("-1")).is_some());
        assert!(parse_client_message(&hello("1e999")).is_none()); // → inf → filtered
        assert!(parse_client_message(&hello("\"2\"")).is_none()); // string is not a number
    }

    #[test]
    fn known_good_message_is_stable_across_hostile_input() {
        // determinism of a stateless parser: the same valid message parses
        // identically before and after a hostile corpus (a purity check, not
        // a claim about recovering from internal state corruption)
        let good = r#"{"type":"interrupt","commandId":"c1","sessionId":"s1"}"#;
        let before = parse_client_message(good);
        assert!(before.is_some());
        let bomb = format!("{}{}", "[".repeat(64), "]".repeat(64));
        for hostile in [
            bomb.as_str(),
            "\u{1}",
            r#"{"type":"ping","commandId":"\ud800"}"#,
            "123",
            "",
        ] {
            let _ = parse_client_message(hostile);
        }
        let after = parse_client_message(good);
        assert_eq!(before, after);
    }

    /* ---- ADR-015: golden fixtures shared with the TS tests ---- */

    macro_rules! fixture {
        ($name:literal) => {
            json::parse(include_str!(concat!(
                "../../../packages/protocol/fixtures/live/",
                $name
            )))
            .unwrap()
        };
    }

    /// Key order is not part of the wire contract: compare objects sorted.
    fn canon(v: &Json) -> Json {
        match v {
            Json::Obj(pairs) => {
                let mut sorted: Vec<(String, Json)> =
                    pairs.iter().map(|(k, v)| (k.clone(), canon(v))).collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                Json::Obj(sorted)
            }
            Json::Arr(items) => Json::Arr(items.iter().map(canon).collect()),
            other => other.clone(),
        }
    }

    #[test]
    fn parses_live_client_messages_and_rejects_bad_fields() {
        let variant = |m: &ClientMsg| match m {
            ClientMsg::SessionSubscribe { .. } => "session_subscribe",
            ClientMsg::SessionUnsubscribe { .. } => "session_unsubscribe",
            ClientMsg::SessionEnd { .. } => "session_end",
            ClientMsg::SetPermissionMode { .. } => "set_permission_mode",
            ClientMsg::McpAuth { .. } => "mcp_auth",
            ClientMsg::McpAuthCallback { .. } => "mcp_auth_callback",
            ClientMsg::McpReconnect { .. } => "mcp_reconnect",
            ClientMsg::PermissionDecision { .. } => "permission_decision",
            _ => "other",
        };
        for case in json::as_arr(&fixture!("client-valid.json")).unwrap() {
            let text = json::stringify(case);
            let msg = parse_client_message(&text).unwrap_or_else(|| panic!("rejected {text}"));
            assert_eq!(Some(variant(&msg)), json::get_str(case, "type"), "{text}");
        }
        for case in json::as_arr(&fixture!("client-invalid.json")).unwrap() {
            let text = json::stringify(json::get(case, "msg").unwrap());
            let why = json::get_str(case, "why").unwrap();
            assert!(parse_client_message(&text).is_none(), "{why}: {text}");
        }

        // field mapping; 3.0 is an integer to JS as well
        assert_eq!(
            parse_client_message(
                r#"{"type":"session_subscribe","commandId":"c","sessionId":"s","epoch":"e1","afterSeq":3.0}"#
            ),
            Some(ClientMsg::SessionSubscribe {
                command_id: "c".into(),
                session_id: "s".into(),
                epoch: Some("e1".into()),
                after_seq: Some(3),
            })
        );
        assert_eq!(
            parse_client_message(
                r#"{"type":"mcp_auth_callback","commandId":"c","sessionId":"s","server":"x","callbackUrl":"http://localhost:1/cb?code=k"}"#
            ),
            Some(ClientMsg::McpAuthCallback {
                command_id: "c".into(),
                session_id: "s".into(),
                server: "x".into(),
                callback_url: "http://localhost:1/cb?code=k".into(),
            })
        );

        // §3.4 bounds: the maximum passes, one more unit fails
        let mcp = |server: String| {
            format!(
                r#"{{"type":"mcp_auth","commandId":"c","sessionId":"s","server":"{server}","flow":"desktop"}}"#
            )
        };
        assert!(parse_client_message(&mcp("s".repeat(256))).is_some());
        assert!(parse_client_message(&mcp("s".repeat(257))).is_none());
        let reconnect = |server: String| {
            format!(
                r#"{{"type":"mcp_reconnect","commandId":"c","sessionId":"s","server":"{server}"}}"#
            )
        };
        assert_eq!(
            parse_client_message(&reconnect("s".repeat(256))),
            Some(ClientMsg::McpReconnect {
                command_id: "c".into(),
                session_id: "s".into(),
                server: "s".repeat(256),
            })
        );
        assert!(parse_client_message(&reconnect("s".repeat(257))).is_none());
        let callback = |len: usize| {
            format!(
                r#"{{"type":"mcp_auth_callback","commandId":"c","sessionId":"s","server":"x","callbackUrl":"http://{}"}}"#,
                "a".repeat(len - "http://".len())
            )
        };
        assert!(parse_client_message(&callback(4096)).is_some());
        assert!(parse_client_message(&callback(4097)).is_none());
        let epoch = |e: String| {
            format!(
                r#"{{"type":"session_subscribe","commandId":"c","sessionId":"s","epoch":"{e}"}}"#
            )
        };
        assert!(parse_client_message(&epoch("e".repeat(64))).is_some());
        assert!(parse_client_message(&epoch("e".repeat(65))).is_none());
        // submit_prompt: 32,000 chars. Never stricter than the phone, which
        // counts UTF-16 units: 2-byte 'é' and 4-byte emoji each count once
        let prompt = |t: String| {
            format!(r#"{{"type":"submit_prompt","commandId":"c","sessionId":"s","text":"{t}"}}"#)
        };
        assert!(parse_client_message(&prompt("p".repeat(32_000))).is_some());
        assert!(parse_client_message(&prompt("p".repeat(32_001))).is_none());
        assert!(parse_client_message(&prompt("\u{e9}".repeat(32_000))).is_some());
        assert!(parse_client_message(&prompt("\u{1F600}".repeat(32_000))).is_some());
        assert!(parse_client_message(&prompt("\u{1F600}".repeat(32_001))).is_none());
    }

    #[test]
    fn permission_decision_accepts_option_id_message_answers() {
        assert_eq!(
            parse_client_message(
                r#"{"type":"permission_decision","commandId":"c","permissionId":"p","decision":"reject","revision":2,"optionId":"deny","message":"use staging","answers":{"q1":["Postgres"],"q2":["Unit tests","Lint"]}}"#
            ),
            Some(ClientMsg::PermissionDecision {
                command_id: "c".into(),
                permission_id: "p".into(),
                decision: "reject".into(),
                revision: 2.0,
                option_id: Some("deny".into()),
                message: Some("use staging".into()),
                answers: vec![
                    ("q1".into(), vec!["Postgres".into()]),
                    ("q2".into(), vec!["Unit tests".into(), "Lint".into()]),
                ],
            })
        );
        // the legacy v2 shape is unchanged: no extras
        assert!(matches!(
            parse_client_message(
                r#"{"type":"permission_decision","commandId":"c","permissionId":"p","decision":"approve-once","revision":1}"#
            ),
            Some(ClientMsg::PermissionDecision { option_id: None, message: None, ref answers, .. })
                if answers.is_empty()
        ));
        // bounds at the limit pass, one past fails
        let decision = |extra: String| {
            format!(
                r#"{{"type":"permission_decision","commandId":"c","permissionId":"p","decision":"approve-once","revision":1,{extra}}}"#
            )
        };
        let option = |n: usize| format!(r#""optionId":"{}""#, "o".repeat(n));
        assert!(parse_client_message(&decision(option(128))).is_some());
        assert!(parse_client_message(&decision(option(129))).is_none());
        let message = |n: usize| format!(r#""message":"{}""#, "m".repeat(n));
        assert!(parse_client_message(&decision(message(4096))).is_some());
        assert!(parse_client_message(&decision(message(4097))).is_none());
        let answer = format!(r#""{}""#, "a".repeat(1024));
        let ids: Vec<String> = (0..8)
            .map(|i| format!(r#""q{i}":[{}]"#, vec![answer.as_str(); 16].join(",")))
            .collect();
        let full = decision(format!(r#""answers":{{{}}}"#, ids.join(",")));
        assert!(matches!(
            parse_client_message(&full),
            Some(ClientMsg::PermissionDecision { ref answers, .. })
                if answers.len() == 8 && answers.iter().all(|(_, a)| a.len() == 16)
        ));
        // answers must be an object of arrays of strings
        for bad in [r#"[["a"]]"#, r#"{"q":"a"}"#, r#"{"q":[1]}"#, "null"] {
            assert!(parse_client_message(&decision(format!(r#""answers":{bad}"#))).is_none());
        }
    }

    #[test]
    fn hello_ack_advertises_transcript_feature() {
        assert_eq!(FEATURES, ["transcript.v1"]);
        let msg = ServerMsg::HelloAck {
            protocol: PROTOCOL_VERSION,
            server_time: 1_761_436_800_000,
            snapshot: Snapshot::default(),
            last_seq: 0,
            features: FEATURES,
        };
        assert_eq!(canon(&msg.to_json()), canon(&fixture!("hello-ack.json")));
    }

    #[test]
    fn transcript_messages_match_golden_fixtures() {
        // the ring keeps each op serialized once; the envelope splices them
        let golden = fixture!("transcript.json");
        let ops: Vec<String> = json::as_arr(json::get(&golden, "ops").unwrap())
            .unwrap()
            .iter()
            .map(json::stringify)
            .collect();
        assert_eq!(ops.len(), 7);
        let batch = ServerMsg::Transcript {
            session_id: "ses_live_1".into(),
            epoch: "9f86d081884c7d65".into(),
            ops,
        };
        let text = batch.to_text();
        assert_eq!(canon(&json::parse(&text).unwrap()), canon(&golden));
        assert_eq!(canon(&batch.to_json()), canon(&golden));

        let golden = fixture!("transcript-snapshot.json");
        let body = json::obj(
            ["items", "truncatedBefore", "turns", "state", "mcp"]
                .into_iter()
                .map(|k| (k, json::get(&golden, k).unwrap().clone()))
                .collect(),
        );
        let snapshot = ServerMsg::TranscriptSnapshot {
            session_id: "ses_live_1".into(),
            epoch: "9f86d081884c7d65".into(),
            seq: 3,
            body,
        };
        assert_eq!(
            canon(&json::parse(&snapshot.to_text()).unwrap()),
            canon(&golden)
        );
    }

    #[test]
    fn permission_extras_serialize_only_when_present() {
        // without extras: exactly today's keys, so v2 phones see no change
        let plain = sample_permission().to_json();
        for key in ["toolItemId", "options", "detail"] {
            assert!(json::get(&plain, key).is_none(), "{key}");
        }
        // with extras: the golden permission_requested frame
        let option =
            |id: &str, decision: &str, label: &str, scope: &'static str| PermissionOption {
                id: id.into(),
                decision: decision.into(),
                label: label.into(),
                rule: None,
                scope,
                destination: None,
                takes_message: false,
                stops_turn: false,
            };
        let pr = PermissionRequest {
            id: "claude-1:toolu_01".into(),
            native_request_id: "req_7c1e".into(),
            session_id: "ses_live_1".into(),
            agent_id: "claude-1".into(),
            host_id: "claude-1".into(),
            action: "file-write".into(),
            title: "Claude wants to write a.txt".into(),
            command: "a.txt".into(),
            args: vec![],
            directory: Some("C:\\work\\demo".into()),
            risk: "medium".into(),
            status: "pending".into(),
            supported_decisions: vec!["approve-once".into(), "reject".into()],
            revision: 1,
            fingerprint: None,
            requested_at: 1_761_436_800_300,
            expires_at: 1_761_523_200_300,
            context: "Create a.txt containing hi".into(),
            decided_at: None,
            decision: None,
            tool_item_id: Some("toolu_01".into()),
            options: vec![
                option("allow", "approve-once", "Allow", "once"),
                PermissionOption {
                    rule: Some("Edit(./**)".into()),
                    destination: Some(".claude/settings.local.json".into()),
                    ..option(
                        "allow-dir",
                        "approve-session",
                        "Always allow edits in this project",
                        "persistent",
                    )
                },
                PermissionOption {
                    takes_message: true,
                    ..option("deny", "reject", "Deny", "once")
                },
                PermissionOption {
                    stops_turn: true,
                    ..option("deny-stop", "reject", "Deny and stop", "once")
                },
            ],
            detail: Some(json::obj(vec![
                ("kind", json::s("edit")),
                ("path", json::s("a.txt")),
                ("patch", json::s("@@ -0,0 +1 @@\n+hi")),
            ])),
        };
        let msg = ServerMsg::Event {
            kind: EventKind::PermissionRequested(pr),
            env: Envelope {
                seq: 17,
                event_id: "evt_live_1".into(),
                occurred_at: 1_761_436_800_300,
            },
        };
        assert_eq!(
            canon(&msg.to_json()),
            canon(&fixture!("permission-requested.json"))
        );

        // Agent.live and Session.transcript follow the same rule
        let mut agent = Agent {
            id: "claude-1".into(),
            host_id: "claude-1".into(),
            kind: "claude-code".into(),
            name: "Claude Code".into(),
            state: "online".into(),
            capabilities: vec![],
            accepts_new_session: true,
            simulated: false,
            live: None,
        };
        assert!(json::get(&agent.to_json(), "live").is_none());
        agent.live = Some(AgentLive {
            prompt_while_busy: "merge",
            interrupt: true,
            end: true,
            permission_modes: vec!["default".into(), "acceptEdits".into(), "plan".into()],
            workspaces: vec![LiveWorkspace {
                id: "3f2a9c1d0e4b".into(),
                label: "demo".into(),
                path: "C:\\work\\demo".into(),
            }],
        });
        assert_eq!(
            json::stringify(json::get(&agent.to_json(), "live").unwrap()),
            r#"{"transcript":true,"promptWhileBusy":"merge","interrupt":true,"end":true,"permissionModes":["default","acceptEdits","plan"],"workspaces":[{"id":"3f2a9c1d0e4b","label":"demo","path":"C:\\work\\demo"}]}"#
        );
        let mut session = Session {
            id: "ses_live_1".into(),
            agent_id: "claude-1".into(),
            host_id: "claude-1".into(),
            title: "t".into(),
            project: "p".into(),
            status: "idle".into(),
            progress: 0,
            created_at: 0,
            last_activity_at: 0,
            prompt: None,
            timeline: vec![],
            output: vec![],
            files: vec![],
            transcript: false,
        };
        assert!(json::get(&session.to_json(), "transcript").is_none());
        session.transcript = true;
        assert_eq!(
            json::get(&session.to_json(), "transcript"),
            Some(&Json::Bool(true))
        );
    }
}
