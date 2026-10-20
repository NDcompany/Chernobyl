//! Protocol v2 — exact Rust port of `@chernobyl/protocol` + the domain
//! permission authority. Validators mirror `isClientMessage` field-for-field;
//! entities serialize to the same JSON the mobile app already consumes.

use crate::json::{self, Json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const PROTOCOL_VERSION: u32 = 2;
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
}

impl Host {
    pub fn to_json(&self) -> Json {
        json::obj(vec![
            ("id", json::s(&self.id)),
            ("name", json::s(&self.name)),
            ("kind", json::s(&self.kind)),
            ("platform", json::s(&self.platform)),
            ("address", json::s(&self.address)),
            ("status", json::s(&self.status)),
            ("lastSeenAt", Json::Int(self.last_seen_at)),
        ])
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
}

impl Agent {
    pub fn to_json(&self) -> Json {
        json::obj(vec![
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
        ])
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
    },
    ListState {
        command_id: String,
    },
}

impl ClientMsg {
    pub fn command_id(&self) -> Option<&str> {
        match self {
            ClientMsg::Hello { .. } => None,
            ClientMsg::Ping { command_id }
            | ClientMsg::CreateSession { command_id, .. }
            | ClientMsg::SubmitPrompt { command_id, .. }
            | ClientMsg::Interrupt { command_id, .. }
            | ClientMsg::PermissionDecision { command_id, .. }
            | ClientMsg::ListState { command_id } => Some(command_id),
        }
    }

    pub fn is_mutating(&self) -> bool {
        matches!(
            self,
            ClientMsg::CreateSession { .. }
                | ClientMsg::SubmitPrompt { .. }
                | ClientMsg::Interrupt { .. }
                | ClientMsg::PermissionDecision { .. }
        )
    }
}

fn is_str(v: &Json) -> Option<&str> {
    json::as_str(v)
}

fn is_num(v: &Json) -> Option<f64> {
    json::as_f64(v)
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
            text: req("text")?,
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
            })
        }
        "list_state" => Some(ClientMsg::ListState {
            command_id: req("commandId")?,
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
            } => json::obj(vec![
                ("type", json::s("hello_ack")),
                ("protocol", Json::Int(*protocol as i64)),
                ("serverTime", Json::Int(*server_time)),
                ("snapshot", snapshot.to_json()),
                ("lastSeq", Json::Int(*last_seq)),
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
        }
    }

    pub fn to_text(&self) -> String {
        json::stringify(&self.to_json())
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

    #[test]
    fn permission_decision_authority_fail_closed() {
        let base = PermissionRequest {
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
        };
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
        };
        let text = json::stringify(&h.to_json());
        let v = json::parse(&text).unwrap();
        assert_eq!(json::get(&v, "lastSeenAt").unwrap(), &Json::Int(42));
    }
}
