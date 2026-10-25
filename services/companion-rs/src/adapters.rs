//! Adapter discovery + status (port of the Node companion's adapters.ts,
//! mock-only scope). Unknown types are recorded as discovery-failed and the
//! engine serves as the in-process mock adapter. Process adapters (OpenCode
//! etc.) arrive with Phase 3+; the manager shape is already here.
//!
//! ADR-015 §3.3 adds [`LiveRuntime`], the trait the companion routes live
//! sessions through, and [`AdapterError`], the error set every runtime shares.

use crate::config::AdapterEntryConfig;
use crate::json::{self, Json};
use crate::protocol::{Agent, ClientMsg, ServerMsg, Session};
use std::collections::HashMap;

/* ------------------------------------------------------------------ */
/* Errors (adapter-sdk code set)                                       */
/* ------------------------------------------------------------------ */

/// Adapter-sdk error codes. Lives here since ADR-015 step 4 so every runtime
/// shares one type; `opencode::AdapterError` re-exports it.
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterError {
    Unsupported(String),
    RuntimeUnavailable(String),
    InvalidResponse(String),
    Unknown(String),
}

impl AdapterError {
    pub fn code(&self) -> &'static str {
        match self {
            AdapterError::Unsupported(_) => "unsupported",
            AdapterError::RuntimeUnavailable(_) => "runtime-unavailable",
            AdapterError::InvalidResponse(_) => "invalid-response",
            AdapterError::Unknown(_) => "unknown",
        }
    }
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            AdapterError::Unsupported(m)
            | AdapterError::RuntimeUnavailable(m)
            | AdapterError::InvalidResponse(m)
            | AdapterError::Unknown(m) => m,
        };
        write!(f, "{}: {}", self.code(), msg)
    }
}

/* ------------------------------------------------------------------ */
/* Live runtimes (ADR-015 §3.3, §4.1)                                  */
/* ------------------------------------------------------------------ */

/// A runtime that owns live sessions: OpenCode today, the Claude and Codex
/// drivers next. The companion keeps one `Arc<dyn LiveRuntime>` per attached
/// adapter (`Companion::runtimes`) and routes client messages by it:
///
/// - `create_session` goes to the runtime hosting the agent: one of its
///   `agents()` has that id and `host_id == id()`;
/// - session-scoped commands (`submit_prompt`, `interrupt`, `session_end`,
///   `set_permission_mode`, `mcp_*`) go to the runtime whose `owns_session`
///   is true;
/// - `permission_decision` goes to the runtime whose `knows_permission` is
///   true, and only after the engine validated it (G2, see `send`);
/// - everything else stays with the engine, and so does anything no runtime
///   claims. `session_subscribe`/`session_unsubscribe` never reach `send`:
///   the server handles them per connection.
///
/// Only healthy runtimes are routed to. While any runtime is healthy, the
/// snapshot shows the healthy runtimes' agents and sessions instead of the
/// engine's demo ones (ADR-009).
///
/// Contract for implementors:
/// - Ids must not collide across runtimes. A session or permission id two
///   runtimes claim is refused and logged, never guessed. New runtimes mint
///   permission ids as `<adapterId>:<nativeId>` (PRD §8.3); OpenCode keeps its
///   native `per_…` ids.
/// - `send` is acceptance only (G1/G4.1): `Ok(ack)` never claims an outcome,
///   which arrives later as engine events. `Err` is a transport or
///   validation fault; the companion turns it into an error ack.
/// - A live message the runtime does not support gets an error ack
///   (`Err(AdapterError::Unsupported)` or `Ok` with `ok: false`), never
///   silence.
/// - Runtimes stream transcripts through the `transcript::HubHandle` they
///   get at construction (`Companion::hub()`): `push` whole items, turns,
///   state and the MCP list, `append` deltas, `update` one item,
///   `end_session`. The hub is a leaf lock: never call the handle while
///   holding the engine lock. Set `Session::transcript` on sessions that
///   stream, so the phone picks its conversation view before the first
///   subscribe (`on_subscribe` then hydrates).
/// - Every method may be called from any thread, and must not block on the
///   network for long: routing calls `is_healthy`, `agents`, `owns_session`
///   and `knows_permission` on the WS read path.
pub trait LiveRuntime: Send + Sync {
    /// The adapter id from the config; equals `host_id` on its agents and
    /// sessions.
    fn id(&self) -> &str;
    fn is_healthy(&self) -> bool;
    fn agents(&self) -> Vec<Agent>;
    fn sessions(&self) -> Vec<Session>;
    fn owns_session(&self, session_id: &str) -> bool;
    /// True while the runtime surfaced this permission request and still
    /// holds it natively (the routing gate for `permission_decision`).
    fn knows_permission(&self, permission_id: &str) -> bool;
    /// Executes one routed command: `create_session`, `submit_prompt`,
    /// `interrupt`, `permission_decision`, `session_end`,
    /// `set_permission_mode`, `mcp_auth`, `mcp_auth_callback`,
    /// `mcp_reconnect`. A `permission_decision` arrives here only after the
    /// engine validated it (pending, expiry, revision, option bucket,
    /// message, answers), with `option_id` set to the option the engine
    /// selected (a legacy tap without one included) and `message`/`answers`
    /// verbatim: write exactly that option's native answer. On `Ok` with
    /// `ok: true` the engine records the resolution; a request the runtime
    /// no longer holds is answered `ok: false` ("request is no longer
    /// pending"), and one resolved natively goes through
    /// `Engine::resolve_native` (publish `PermissionResolved`).
    fn send(&self, msg: &ClientMsg) -> Result<ServerMsg, AdapterError>;
    /// The first subscriber for one of this runtime's sessions arrived:
    /// hydrate history, start MCP polling. Called outside every companion
    /// lock.
    fn on_subscribe(&self, _session_id: &str) {}
    /// Probe loop (`probeIntervalMs`): re-check health and refresh the cached
    /// lists. `Err` marks the adapter unhealthy in `/adapters`.
    fn refresh(&self) -> Result<(), AdapterError> {
        Ok(())
    }
    /// PRD §7.3 honest capability declaration.
    fn capabilities(&self) -> Vec<Capability>;
    /// Stops threads and children. Idempotent; called once at shutdown.
    fn dispose(&self);
}

/// PRD §7.3 capability declaration — what an adapter honestly supports
/// (TS `@chernobyl/domain` `Capability`, mirrored). `status` values are the
/// domain set: available | unavailable | limited | dependent-config.
#[derive(Debug, Clone, PartialEq)]
pub struct Capability {
    pub id: String,
    pub status: String,
    pub detail: Option<String>,
}

impl Capability {
    pub fn new(id: &str, status: &str, detail: Option<&str>) -> Capability {
        Capability {
            id: id.to_string(),
            status: status.to_string(),
            detail: detail.map(str::to_string),
        }
    }

    pub fn to_json(&self) -> Json {
        let mut fields = vec![("id", json::s(&self.id)), ("status", json::s(&self.status))];
        if let Some(d) = &self.detail {
            fields.push(("detail", json::s(d)));
        }
        json::obj(fields)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AdapterStatus {
    pub id: String,
    pub kind: String,
    pub supported: bool,
    pub healthy: bool,
    pub last_error: Option<String>,
    /// PRD §7.3 declaration; empty until an adapter attaches its list
    /// (discovery alone cannot speak for a runtime it hasn't probed).
    pub capabilities: Vec<Capability>,
}

/// Capability mirror of `packages/adapter-sdk/src/mockAdapter.ts` (the mock
/// adapter's own honest declaration — parity so diagnostics read the same).
fn mock_capabilities() -> Vec<Capability> {
    vec![
        Capability::new("session.list", "available", None),
        Capability::new("session.message.send", "available", None),
        Capability::new("session.interrupt", "available", None),
        Capability::new("permission.review", "available", None),
        Capability::new(
            "diff.read",
            "unavailable",
            Some("not simulated by the mock runtime"),
        ),
        Capability::new("files.read", "limited", Some("metadata only")),
    ]
}

pub struct AdapterManager {
    statuses: HashMap<String, AdapterStatus>,
}

impl AdapterManager {
    /// Discovery: `mock` is served in-process by the engine; `opencode` is
    /// attachable (health resolves from the real boot/probe call); anything
    /// else is recorded as unsupported (never silently dropped).
    pub fn discover(entries: &[AdapterEntryConfig]) -> AdapterManager {
        let mut statuses = HashMap::new();
        for entry in entries.iter().filter(|e| e.enabled) {
            let (supported, healthy, last_error, capabilities) = if entry.kind == "mock" {
                (true, true, None, mock_capabilities())
            } else if entry.kind == "opencode" {
                // Phase 3: attachable — actual health comes from the HTTP
                // probe at boot, never from discovery alone.
                (
                    true,
                    false,
                    Some("attachable — endpoint probe runs at boot".into()),
                    Vec::new(),
                )
            } else if entry.kind == "ollama" {
                // Phase 7: attachable model-runtime provider (PRD §7.7) —
                // health + inventory come from the boot/probe HTTP calls.
                (
                    true,
                    false,
                    Some("attachable — model-runtime probe runs at boot".into()),
                    Vec::new(),
                )
            } else if entry.kind == "claude" {
                // Phase 7: hook INGRESS (PRD §7.5) — no endpoint to probe;
                // the companion exposes a loopback route and a Claude Code
                // hook bridge posts to it. Healthy = the ingress is mounted;
                // whether a bridge is actually delivering is a runtime signal
                // the adapter reports, not a discovery-time claim.
                (true, true, None, Vec::new())
            } else {
                (
                    false,
                    false,
                    Some(format!(
                        "unsupported adapter type '{}' — discovery failed (recorded, not fatal)",
                        entry.kind
                    )),
                    Vec::new(),
                )
            };
            statuses.insert(
                entry.id.clone(),
                AdapterStatus {
                    id: entry.id.clone(),
                    kind: entry.kind.clone(),
                    supported,
                    healthy,
                    last_error,
                    capabilities,
                },
            );
        }
        AdapterManager { statuses }
    }

    /// Phase 3: record the live OpenCode adapter's capability declaration
    /// (boot, right after the adapter is constructed).
    pub fn attach_opencode(&mut self, id: &str, capabilities: Vec<Capability>) {
        if let Some(s) = self.statuses.get_mut(id) {
            s.supported = true;
            s.capabilities = capabilities;
        }
    }

    /// Phase 7: record the live Ollama adapter's capability declaration
    /// (boot, right after the adapter is constructed). Same shape as
    /// OpenCode — the honest §7.3 list replaces the discovery placeholder.
    pub fn attach_ollama(&mut self, id: &str, capabilities: Vec<Capability>) {
        if let Some(s) = self.statuses.get_mut(id) {
            s.supported = true;
            s.capabilities = capabilities;
        }
    }

    /// Phase 7: record the Claude hook-ingress capability declaration (boot).
    /// The ingress has no probe; its honest §7.3 list replaces discovery's.
    pub fn attach_claude(&mut self, id: &str, capabilities: Vec<Capability>) {
        if let Some(s) = self.statuses.get_mut(id) {
            s.supported = true;
            s.capabilities = capabilities;
        }
    }

    /// Phase 3: fold a probe result (boot probe or probe loop) into the
    /// status. `None` = healthy.
    pub fn probe_result(&mut self, id: &str, error: Option<String>) {
        if let Some(s) = self.statuses.get_mut(id) {
            s.healthy = error.is_none();
            s.last_error = error;
        }
    }

    /// Auto-routing (Phase 2 rule): exactly one healthy adapter → route to it.
    pub fn sole_healthy(&self) -> Option<&AdapterStatus> {
        let healthy: Vec<_> = self.statuses.values().filter(|s| s.healthy).collect();
        match healthy.len() {
            1 => Some(healthy[0]),
            _ => None,
        }
    }

    pub fn list(&self) -> Vec<AdapterStatus> {
        let mut out: Vec<_> = self.statuses.values().cloned().collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Re-attempt discovery for failed entries (kept for the probe loop of
    /// future process adapters; harmless now).
    pub fn reprobe(&mut self, entries: &[AdapterEntryConfig]) {
        let rediscovered = Self::discover(entries);
        for (id, status) in rediscovered.statuses {
            if let Some(existing) = self.statuses.get_mut(&id)
                && !existing.supported
            {
                *existing = status;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, kind: &str) -> AdapterEntryConfig {
        AdapterEntryConfig {
            id: id.into(),
            kind: kind.into(),
            enabled: true,
            endpoint: None,
            basic_auth: None,
        }
    }

    #[test]
    fn mock_is_healthy_and_opencode_is_attachable_unknown_is_recorded() {
        let mut m = AdapterManager::discover(&[
            entry("mock-1", "mock"),
            entry("oc-1", "opencode"),
            AdapterEntryConfig {
                id: "off-1".into(),
                kind: "mock".into(),
                enabled: false,
                endpoint: None,
                basic_auth: None,
            },
        ]);
        let list = m.list();
        assert_eq!(list.len(), 2); // disabled entry skipped
        let oc = list.iter().find(|s| s.id == "oc-1").unwrap();
        // attachable, but health only ever comes from a real probe
        assert!(oc.supported);
        assert!(!oc.healthy);
        assert!(oc.last_error.as_deref().unwrap().contains("attachable"));
        let mock = list.iter().find(|s| s.id == "mock-1").unwrap();
        assert!(!mock.capabilities.is_empty()); // PRD §7.3 declaration
        assert!(m.sole_healthy().is_some()); // only mock-1 is healthy

        // boot + probe loop fold their results into the status
        m.attach_opencode(
            "oc-1",
            vec![Capability::new("session.list", "available", None)],
        );
        m.probe_result("oc-1", None);
        let oc = m.list().into_iter().find(|s| s.id == "oc-1").unwrap();
        assert!(oc.healthy);
        assert_eq!(oc.capabilities.len(), 1);
        m.probe_result("oc-1", Some("runtime-unavailable: connect refused".into()));
        let oc = m.list().into_iter().find(|s| s.id == "oc-1").unwrap();
        assert!(!oc.healthy);
        assert!(oc.last_error.unwrap().contains("runtime-unavailable"));
    }

    #[test]
    fn unknown_type_is_recorded_not_fatal() {
        let m = AdapterManager::discover(&[entry("x-1", "browser")]);
        let x = &m.list()[0];
        assert!(!x.supported);
        assert!(!x.healthy);
        assert!(x.last_error.as_deref().unwrap().contains("unsupported"));
    }

    #[test]
    fn multiple_healthy_adapters_disable_auto_routing() {
        let m = AdapterManager::discover(&[entry("m1", "mock"), entry("m2", "mock")]);
        assert!(m.sole_healthy().is_none());
    }
}
