//! Companion orchestration — the combined local backend: engine (mock agent
//! authority) + durable store + adapter manager + counters. Mirrors the Node
//! companion's guarantees: persist-before-execute, idempotency by commandId,
//! boot reconciliation, latched fail-closed on write failure, event seq
//! continuity (C2).

use crate::adapters::{AdapterManager, LiveRuntime};
use crate::claude::ClaudeAdapter;
use crate::config::CompanionConfig;
use crate::engine::Engine;
use crate::ffi;
use crate::ollama::OllamaAdapter;
use crate::opencode::OpenCodeAdapter;
use crate::protocol::{Agent, ClientMsg, EventKind, PROTOCOL_VERSION, ServerMsg, Session, now_ms};
use crate::server::Outbound;
use crate::storage::{CommandRecord, Storage};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Process-wide outbound saturation counters (mandate §4): the production
/// overload story must be readable from `/health` and `/metrics` instead of
/// inferred from client-side timing. Latched per connection — once a queue is
/// saturated the connection is going down, so later refusals don't inflate
/// the teardown count.
#[derive(Default)]
pub struct Saturation {
    /// bulk (event fan-out) frames refused admission by the class bound
    pub shed_bulk: std::sync::atomic::AtomicU64,
    /// control (reply/ack) frames refused admission by the class bound
    pub shed_ctrl: std::sync::atomic::AtomicU64,
    /// connections torn down because an outbound bound was hit (D8)
    pub teardowns: std::sync::atomic::AtomicU64,
}

/// File in the data dir holding the loopback control key (security review
/// 2026-10-25): 64 lowercase hex chars from the OS CSPRNG, minted on first
/// boot. POST /pairing/offer and /devices/revoke require it, so only an
/// account that can read the data dir (user-private under %APPDATA%, the
/// ACL the journal already relies on) — the operator and their CLI — can
/// mint a PSK or revoke a phone.
pub const CONTROL_KEY_FILE: &str = "control.key";

/// Reads the control key, minting it when absent. Anything but 64 lowercase
/// hex chars (a torn first write, a hand edit) is replaced: an empty or short
/// key must never be what a request is compared against.
fn load_control_key(dir: &std::path::Path) -> Result<String, String> {
    let path = dir.join(CONTROL_KEY_FILE);
    if let Ok(key) = std::fs::read_to_string(&path)
        && key.len() == 64
        && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Ok(key);
    }
    let key: String = ffi::csprng_bytes(32)?
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::fs::write(&path, &key).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(key)
}

#[derive(Debug, PartialEq, Eq)]
pub enum RevokeResult {
    NotFound,
    Revoked { terminated: usize },
}

/// Where `Companion::route` sends one client message.
enum Route {
    Engine,
    Runtime(Arc<dyn LiveRuntime>),
    /// Two healthy runtimes claim the same agent, session or permission id.
    Ambiguous,
}

pub struct Shared {
    pub storage: Mutex<Storage>,
    /// Outbound payload queues, one per live WS connection, keyed by a
    /// monotonic connection id (mpsc Sender has no channel identity).
    /// Bounded by count and bytes (D8); a saturated queue marks its
    /// connection terminated and the broadcast drops the registration.
    pub clients: Mutex<Vec<(u64, Arc<Outbound>)>>,
    /// Saturation counters aggregated across every connection (§4).
    pub saturation: Arc<Saturation>,
    pub next_conn_id: std::sync::atomic::AtomicU64,
    pub adapters: Mutex<AdapterManager>,
    /// Phase 3: the attached OpenCode adapter, when configured (attach-only).
    /// A typed handle for OpenCode-only surfaces (`diff`, `messages`) and
    /// their tests; routing goes through `Companion::runtimes` (ADR-015).
    pub opencode: Mutex<Option<Arc<OpenCodeAdapter>>>,
    /// Phase 7: the attached Ollama model-runtime adapter, when configured
    /// (attach-only — PRD §7.7, a model provider not a session manager).
    pub ollama: Mutex<Option<Arc<OllamaAdapter>>>,
    /// Phase 7: the Claude Code hook ingress, when configured (PRD §7.5) —
    /// Claude Code pushes hook events to the companion's loopback route.
    pub claude: Mutex<Option<Arc<ClaudeAdapter>>>,
    pub started_at: i64,
    pub config: CompanionConfig,
    /// Consecutive auth failures; trips a cooldown (brute-force resistance).
    pub auth_failures: Mutex<u32>,
    /// Active connection count across all listeners (mandate §2, audit P7).
    pub connections_active: std::sync::atomic::AtomicU64,
    /// Connections rejected due to listener cap or spawn failure (audit P7).
    pub connections_rejected: std::sync::atomic::AtomicU64,
    /// Command outcome persistence failures (mandate §5, audit P9).
    pub outcome_write_failures: std::sync::atomic::AtomicU64,
}

pub struct Companion {
    pub shared: Arc<Shared>,
    /// Arc'd so the adapter's event thread can publish into the same
    /// sequence/pipeline (single seq authority).
    engine: Arc<Mutex<Engine>>,
    /// ADR-015 §4.1: every attached live runtime, in config order. Filled at
    /// boot and never changed afterwards, so routing needs no lock.
    runtimes: Vec<Arc<dyn LiveRuntime>>,
    /// Last adapter probe timestamp for the probe-interval gate.
    last_probe: Mutex<i64>,
    pub events_total: Arc<std::sync::atomic::AtomicU64>,
    pub malformed_total: std::sync::atomic::AtomicU64,
    /// See [`CONTROL_KEY_FILE`].
    control_key: String,
}

impl Companion {
    pub fn boot(config: CompanionConfig) -> Result<Companion, String> {
        let storage = Storage::open(&config.data_dir, config.events_cap, config.commands_cap)?;
        let control_key = load_control_key(&config.data_dir)?;
        // C2: the engine continues the event sequence from the journal
        let start_seq = storage.last_seq;

        // Phase 3: locate the attach target before `config` moves into
        // Shared. Only the first enabled opencode entry attaches — a second
        // would be exactly the competing-server shape G4.3 forbids.
        let oc_cfg = config
            .adapters
            .iter()
            .find(|e| e.enabled && e.kind == "opencode")
            .map(|e| (e.id.clone(), e.endpoint.clone(), e.basic_auth.clone()));

        // Phase 7: locate the Ollama attach target the same way — first
        // enabled entry wins (attach-only; no second server, G4.3).
        let ol_cfg = config
            .adapters
            .iter()
            .find(|e| e.enabled && e.kind == "ollama")
            .map(|e| (e.id.clone(), e.endpoint.clone()));

        // Phase 7: the Claude Code hook ingress — no endpoint (it is a
        // route the companion exposes), just the enabled adapter id.
        let claude_cfg = config
            .adapters
            .iter()
            .find(|e| e.enabled && e.kind == "claude")
            .map(|e| e.id.clone());

        // ADR-009: the demo seed exists only while a mock adapter is enabled
        // (`"enabled": false` in companion.json turns the demo data off).
        let demo_seed = config
            .adapters
            .iter()
            .any(|e| e.enabled && e.kind == "mock");

        let shared = Arc::new(Shared {
            storage: Mutex::new(storage),
            clients: Mutex::new(Vec::new()),
            saturation: Arc::new(Saturation::default()),
            next_conn_id: std::sync::atomic::AtomicU64::new(1),
            adapters: Mutex::new(AdapterManager::discover(&config.adapters)),
            opencode: Mutex::new(None),
            ollama: Mutex::new(None),
            claude: Mutex::new(None),
            started_at: now_ms(),
            config,
            auth_failures: Mutex::new(0),
            connections_active: std::sync::atomic::AtomicU64::new(0),
            connections_rejected: std::sync::atomic::AtomicU64::new(0),
            outcome_write_failures: std::sync::atomic::AtomicU64::new(0),
        });

        // Engine emit: PERSIST FIRST, then broadcast. A write failure latches
        // the store (fail-closed): the event is dropped rather than delivered
        // without a durable record (PRD §19.4).
        let emit_shared = shared.clone();
        let events_total = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let events_counter = events_total.clone();
        let mut engine = Engine::new(
            start_seq,
            Box::new(move |msg: ServerMsg| {
                let text = msg.to_text();
                if let ServerMsg::Event { kind, env } = &msg {
                    if let EventKind::CommandResult {
                        command_id,
                        status,
                        session_id,
                        ..
                    } = kind
                    {
                        // terminal outcome closes the command row (C2); the
                        // ack outcome (ok) stays as captured — replays return
                        // the original acceptance ack, like the Node host
                        let mut store = emit_shared.storage.lock().unwrap();
                        let _ = store.update_command_outcome(
                            command_id,
                            status,
                            None,
                            session_id.as_deref(),
                            None,
                            None,
                        );
                    }
                    let mut store = emit_shared.storage.lock().unwrap();
                    let _ = store.append_event(kind.type_str(), &text, env.seq);
                }
                events_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut clients = emit_shared.clients.lock().unwrap();
                // Non-blocking enqueue (D8): a saturated peer is marked
                // terminated (its read loop closes the socket) and dropped
                // from the registry; the engine is never parked by a slow
                // client and no event is silently dropped for healthy ones.
                // Events are sealed per connection (wire contract: from hs_ok
                // on every frame is sealed) - enqueue_sealed seals for session
                // clients and passes plaintext through for loopback parity; at
                // remote posture a sessionless client gets no events at all.
                // Classed Bulk (mandate §4): fan-out gets its own, lower bound
                // so it can never starve reply/ack frames.
                clients.retain(|(_, q)| {
                    q.enqueue_sealed(text.clone().into_bytes(), crate::server::MsgClass::Bulk)
                        .is_ok()
                });
            }),
        );
        if !demo_seed {
            engine.drop_seed();
        }

        let mut companion = Companion {
            shared,
            engine: Arc::new(Mutex::new(engine)),
            runtimes: Vec::new(),
            last_probe: Mutex::new(now_ms()),
            events_total,
            malformed_total: std::sync::atomic::AtomicU64::new(0),
            control_key,
        };

        // Boot reconciliation: unresolved commands → unknown (never re-run),
        // expired pending approvals → stale.
        {
            let mut store = companion.shared.storage.lock().unwrap();
            let unresolved = store.reconcile_unresolved()?;
            let expired = store.reconcile_expired_approvals(now_ms())?;
            let _ = store.append_audit(
                "boot",
                &format!(
                    "companion booted: reconciled {} unresolved command(s), {} expired approval(s)",
                    unresolved.len(),
                    expired.len()
                ),
            );
        }

        // Phase 3: attach the OpenCode adapter (attach-only — G4.3 forbids
        // spawning a competing server on a possibly-shared data dir).
        if let Some((id, endpoint, auth)) = oc_cfg {
            // File configs validate the endpoint (parse-time); direct
            // programmatic configs fall back to OpenCode's documented
            // serve default.
            let endpoint = endpoint.unwrap_or_else(|| "http://127.0.0.1:4096".to_string());
            let oc = Arc::new(OpenCodeAdapter::new(&id, &endpoint, auth.as_deref()));
            let probe = oc.refresh();
            {
                let mut mgr = companion.shared.adapters.lock().unwrap();
                mgr.attach_opencode(&id, oc.capabilities());
                mgr.probe_result(&id, probe.as_ref().err().map(|e| e.to_string()));
            }
            // Native events enter the SAME seq/persist/fan-out pipeline as
            // engine events; permissions are ingested first so the engine
            // stays the G2 authority over them.
            let cb_engine = companion.engine.clone();
            oc.subscribe(move |kind| {
                let mut engine = cb_engine.lock().unwrap();
                match kind {
                    EventKind::PermissionRequested(pr) => engine.ingest_permission(pr),
                    other => engine.publish_external(other),
                }
            });
            *companion.shared.opencode.lock().unwrap() = Some(oc.clone());
            companion.runtimes.push(oc.clone());
            if let Err(e) = oc.start_events() {
                // thread spawn failure: HTTP attach still works, but the
                // failure channel doesn't — fail the status visible.
                companion
                    .shared
                    .adapters
                    .lock()
                    .unwrap()
                    .probe_result(&id, Some(format!("event stream unavailable: {e}")));
            }
        }

        // Phase 7: attach the Ollama model-runtime adapter (attach-only —
        // G4.3 forbids spawning a second server; PRD §7.7 keeps it a model
        // provider, never a session manager). Poll-only: no SSE, no session
        // subscription, so nothing enters the engine's session pipeline.
        if let Some((id, endpoint)) = ol_cfg {
            // File configs validate the endpoint (parse-time); direct
            // programmatic configs fall back to Ollama's documented default.
            let endpoint = endpoint.unwrap_or_else(|| "http://127.0.0.1:11434".to_string());
            let ol = Arc::new(OllamaAdapter::new(&id, &endpoint));
            let probe = ol.refresh();
            {
                let mut mgr = companion.shared.adapters.lock().unwrap();
                mgr.attach_ollama(&id, ol.capabilities());
                mgr.probe_result(&id, probe.as_ref().err().map(|e| e.to_string()));
            }
            *companion.shared.ollama.lock().unwrap() = Some(ol);
        }

        // Phase 7: mount the Claude Code hook ingress (PRD §7.5). No probe
        // and no endpoint — the companion exposes a loopback route and the
        // user-installed bridge posts to it; health is the route being ready.
        if let Some(id) = claude_cfg {
            let cl = Arc::new(ClaudeAdapter::new(&id));
            {
                let mut mgr = companion.shared.adapters.lock().unwrap();
                mgr.attach_claude(&id, cl.capabilities());
            }
            *companion.shared.claude.lock().unwrap() = Some(cl);
        }

        Ok(companion)
    }

    /* ---------------------------------------------------------------- */
    /* Command path (trust boundary)                                     */
    /* ---------------------------------------------------------------- */

    pub fn handle_client_msg(&self, msg: ClientMsg) -> Option<ServerMsg> {
        let msg_cmd_id = msg.command_id().map(str::to_string);
        let msg_mutating = msg.is_mutating();
        // Mutating commands are persisted BEFORE execution, idempotent by
        // commandId (PRD §10.4). The engine keeps a session-local ack cache;
        // the store makes dedupe survive restarts.
        if let (Some(cmd_id), true) = (msg_cmd_id.as_deref(), msg_mutating) {
            let mut store = self.shared.storage.lock().unwrap();
            if store.is_failed() {
                return Some(ServerMsg::ack_err(
                    cmd_id,
                    "companion storage is in a failed state — command refused (fail-closed)",
                ));
            }
            if let Some(existing) = store.commands.get(cmd_id) {
                // already executed (or in flight): replay the ORIGINAL ack
                // verbatim — refusals replay as refusals, never as success
                // (parity with the Node host's engine-cache replay; found by
                // the A/B differential against services/host)
                let (ok, error, session_id, permission_id) = match existing.ok {
                    Some(ok) => (
                        ok,
                        existing.error.clone(),
                        existing.session_id.clone(),
                        existing.permission_id.clone(),
                    ),
                    None => (
                        false,
                        Some("command is still in flight — outcome not recorded yet".into()),
                        None,
                        None,
                    ),
                };
                return Some(ServerMsg::Ack {
                    command_id: cmd_id.to_string(),
                    ok,
                    error,
                    duplicate: true,
                    session_id,
                    permission_id,
                });
            }
            let kind = match msg {
                ClientMsg::CreateSession { .. } => "create_session",
                ClientMsg::SubmitPrompt { .. } => "submit_prompt",
                ClientMsg::Interrupt { .. } => "interrupt",
                ClientMsg::PermissionDecision { .. } => "permission_decision",
                ClientMsg::SessionEnd { .. } => "session_end",
                ClientMsg::SetPermissionMode { .. } => "set_permission_mode",
                ClientMsg::McpAuth { .. } => "mcp_auth",
                ClientMsg::McpAuthCallback { .. } => "mcp_auth_callback",
                ClientMsg::McpReconnect { .. } => "mcp_reconnect",
                _ => unreachable!(),
            };
            let payload = json_text(&msg);
            let record = CommandRecord {
                command_id: cmd_id.to_string(),
                kind: kind.into(),
                payload,
                status: "unresolved".into(),
                created_at: now_ms(),
                updated_at: now_ms(),
                ok: None,
                session_id: None,
                permission_id: None,
                error: None,
            };
            if let Err(e) = store.put_command(&record) {
                return Some(ServerMsg::ack_err(
                    cmd_id,
                    &format!("failed to persist command (fail-closed): {e}"),
                ));
            }
        }

        // ADR-015 §4.1 routing: a HEALTHY live runtime that claims the
        // command runs it (the native runtime is the truth for sessions it
        // owns); permission decisions go native only when a runtime surfaced
        // that request — otherwise engine authority (G2). Unclaimed and
        // session-neutral messages stay with the engine.
        let reply = match self.route(&msg) {
            Route::Runtime(rt) => match &msg {
                // G2 stays with the engine: it validates the decision
                // (native metadata, pending, expiry, revision, option bucket,
                // message, answers), the native runtime only EXECUTES it,
                // then the engine records the accepted outcome — success can
                // never outrun validation.
                ClientMsg::PermissionDecision {
                    command_id,
                    permission_id,
                    decision,
                    revision,
                    option_id,
                    message,
                    answers,
                } => {
                    let verdict = {
                        let mut engine = self.engine.lock().unwrap();
                        engine.validate_native_permission_decision(
                            permission_id,
                            decision,
                            option_id.as_deref(),
                            message.as_deref(),
                            answers,
                            *revision,
                        )
                    };
                    match verdict {
                        Err(e) => Some(ServerMsg::ack_err(command_id, &e)),
                        Ok(selected) => {
                            // the runtime writes exactly the option the engine
                            // selected, a legacy tap (no optionId) included
                            let mut forwarded = msg.clone();
                            if let (
                                Some(option),
                                ClientMsg::PermissionDecision {
                                    option_id: chosen, ..
                                },
                            ) = (selected, &mut forwarded)
                            {
                                *chosen = Some(option.id);
                            }
                            match rt.send(&forwarded) {
                                Ok(ack @ ServerMsg::Ack { ok: true, .. }) => {
                                    let mut engine = self.engine.lock().unwrap();
                                    engine.complete_native_decision(
                                        command_id,
                                        permission_id,
                                        decision,
                                    );
                                    Some(ack)
                                }
                                // native refused → record stays pending; the
                                // engine's expiry sweep resolves it honestly
                                Ok(ack) => Some(ack),
                                Err(e) => Some(ServerMsg::ack_err(command_id, &e.to_string())),
                            }
                        }
                    }
                }
                _ => Some(match rt.send(&msg) {
                    Ok(reply) => reply,
                    // transport/validation faults surface as refusals — never
                    // as a fabricated engine outcome
                    Err(e) => {
                        ServerMsg::ack_err(msg_cmd_id.as_deref().unwrap_or(""), &e.to_string())
                    }
                }),
            },
            Route::Ambiguous => Some(ServerMsg::ack_err(
                msg_cmd_id.as_deref().unwrap_or(""),
                "claimed by more than one runtime — refused",
            )),
            Route::Engine => {
                let mut engine = self.engine.lock().unwrap();
                engine.handle(msg)
            }
        };

        // capture the ORIGINAL ack outcome (ok, error, ids) so replays —
        // including after a restart, when the engine's ack cache is gone —
        // return the very same ack. Mutating commands only: a non-mutating
        // ack (session_subscribe) must never rewrite a persisted command
        // that happens to share its commandId.
        if let (
            Some(ServerMsg::Ack {
                ok,
                error,
                duplicate: false,
                session_id,
                permission_id,
                ..
            }),
            Some(cmd_id),
            true,
        ) = (&reply, msg_cmd_id, msg_mutating)
        {
            let mut store = self.shared.storage.lock().unwrap();
            if let Err(e) = store.update_command_outcome(
                &cmd_id,
                if *ok { "executed" } else { "refused" },
                Some(*ok),
                session_id.as_deref(),
                permission_id.as_deref(),
                error.as_deref(),
            ) {
                // audit P9: log failure and never pretend outcome was recorded
                eprintln!("[companion] outcome write failed for {cmd_id}: {e}");
                self.shared
                    .outcome_write_failures
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        reply
    }

    /* ---------------------------------------------------------------- */
    /* Introspection                                                     */
    /* ---------------------------------------------------------------- */

    pub fn snapshot(&self) -> crate::protocol::Snapshot {
        let mut snap = self.engine.lock().unwrap().snapshot().clone();
        if let Some((agents, sessions, _)) = self.live_lists() {
            // Healthy real runtimes are the truth for entities they own;
            // mock seed sessions/agents shown beside real ones would pass
            // demo data off as live (PRD §13.1, ADR-009). Permissions/
            // activity/hosts stay engine-owned — adapter permissions ARE
            // engine records (ingested at subscribe time).
            snap.sessions = sessions;
            snap.agents = agents;
        }
        snap
    }

    /// Agents and sessions of every healthy runtime, concatenated in config
    /// order, plus the session ids two runtimes both claim: those are left
    /// out (refused, never guessed). `None` while no runtime is healthy.
    fn live_lists(&self) -> Option<(Vec<Agent>, Vec<Session>, Vec<String>)> {
        let healthy: Vec<_> = self.runtimes.iter().filter(|r| r.is_healthy()).collect();
        if healthy.is_empty() {
            return None;
        }
        let agents = healthy.iter().flat_map(|r| r.agents()).collect();
        let mut sessions: Vec<Session> = healthy.iter().flat_map(|r| r.sessions()).collect();
        let mut seen = HashSet::new();
        let mut dupes: Vec<String> = sessions
            .iter()
            .filter(|s| !seen.insert(s.id.as_str()))
            .map(|s| s.id.clone())
            .collect();
        dupes.sort();
        dupes.dedup();
        sessions.retain(|s| !dupes.contains(&s.id));
        Some((agents, sessions, dupes))
    }

    /// ADR-015 §4.1 routing rule (one place so tests can pin it). Only
    /// healthy runtimes claim anything; two claimants are refused and logged.
    fn route(&self, msg: &ClientMsg) -> Route {
        let claims = |claimed: &dyn Fn(&dyn LiveRuntime) -> bool| -> Vec<Arc<dyn LiveRuntime>> {
            self.runtimes
                .iter()
                .filter(|r| r.is_healthy() && claimed(r.as_ref()))
                .cloned()
                .collect()
        };
        let (what, id, claimants) = match msg {
            // by the agent's host: the runtime whose id is the agent's hostId
            ClientMsg::CreateSession { agent_id, .. } => (
                "agent",
                agent_id,
                claims(&|r| {
                    r.agents()
                        .iter()
                        .any(|a| a.id == *agent_id && a.host_id == r.id())
                }),
            ),
            ClientMsg::SubmitPrompt { session_id, .. }
            | ClientMsg::Interrupt { session_id, .. }
            | ClientMsg::SessionEnd { session_id, .. }
            | ClientMsg::SetPermissionMode { session_id, .. }
            | ClientMsg::McpAuth { session_id, .. }
            | ClientMsg::McpAuthCallback { session_id, .. }
            | ClientMsg::McpReconnect { session_id, .. } => (
                "session",
                session_id,
                claims(&|r| r.owns_session(session_id)),
            ),
            // engine-authoritative unless a runtime surfaced the request
            // natively (revision/expiry checked by the engine either way)
            ClientMsg::PermissionDecision { permission_id, .. } => (
                "permission",
                permission_id,
                claims(&|r| r.knows_permission(permission_id)),
            ),
            // session-neutral; subscriptions need the connection, so the
            // server answers those itself (never journaled, never routed)
            ClientMsg::Hello { .. }
            | ClientMsg::Ping { .. }
            | ClientMsg::ListState { .. }
            | ClientMsg::HsHello { .. }
            | ClientMsg::HsProof { .. }
            | ClientMsg::SessionSubscribe { .. }
            | ClientMsg::SessionUnsubscribe { .. } => return Route::Engine,
        };
        match claimants.as_slice() {
            [] => Route::Engine,
            [one] => Route::Runtime(one.clone()),
            many => {
                let ids: Vec<&str> = many.iter().map(|r| r.id()).collect();
                eprintln!(
                    "[companion] {what} {id} is claimed by runtimes {}: refused",
                    ids.join(", ")
                );
                Route::Ambiguous
            }
        }
    }

    pub fn snapshot_json(&self) -> crate::json::Json {
        self.snapshot().to_json()
    }

    pub fn last_seq(&self) -> i64 {
        self.engine.lock().unwrap().last_seq()
    }

    pub fn health(&self) -> crate::json::Json {
        // merged view: with a healthy adapter attached, entity counts must
        // match what the snapshot actually serves
        let snap = self.snapshot();
        let pending = snap
            .permissions
            .iter()
            .filter(|p| p.status == "pending")
            .count();
        let last_seq = self.engine.lock().unwrap().last_seq();
        let (hosts, sessions) = (snap.hosts.len(), snap.sessions.len());
        let store = self.shared.storage.lock().unwrap();
        let commands = store.commands.len();
        let unresolved = store
            .commands
            .values()
            .filter(|c| c.status == "unresolved")
            .count();
        let storage_failed = store.is_failed();
        let journal_corrupt = store.corrupt_events;
        drop(store);
        let clients = self.shared.clients.lock().unwrap().len();
        let sat = &self.shared.saturation;
        crate::json::obj(vec![
            ("ok", crate::json::Json::Bool(true)),
            ("protocol", crate::json::Json::Int(PROTOCOL_VERSION as i64)),
            ("clients", crate::json::Json::Int(clients as i64)),
            ("hosts", crate::json::Json::Int(hosts as i64)),
            ("sessions", crate::json::Json::Int(sessions as i64)),
            ("pending", crate::json::Json::Int(pending as i64)),
            ("lastSeq", crate::json::Json::Int(last_seq)),
            ("commands", crate::json::Json::Int(commands as i64)),
            ("unresolved", crate::json::Json::Int(unresolved as i64)),
            (
                "events",
                crate::json::Json::Int(
                    self.events_total.load(std::sync::atomic::Ordering::Relaxed) as i64,
                ),
            ),
            (
                "malformed",
                crate::json::Json::Int(
                    self.malformed_total
                        .load(std::sync::atomic::Ordering::Relaxed) as i64,
                ),
            ),
            ("storageFailed", crate::json::Json::Bool(storage_failed)),
            // journal integrity (mandate §5): mid-file corruption latches a
            // counter instead of truncating silently (a torn tail stays 0)
            (
                "journalCorrupt",
                crate::json::Json::Int(journal_corrupt as i64),
            ),
            // outbound saturation (mandate §4): production overload is
            // described by counters, never inferred from dropped clients
            (
                "outboundShedBulk",
                crate::json::Json::Int(
                    sat.shed_bulk.load(std::sync::atomic::Ordering::Relaxed) as i64
                ),
            ),
            (
                "outboundShedCtrl",
                crate::json::Json::Int(
                    sat.shed_ctrl.load(std::sync::atomic::Ordering::Relaxed) as i64
                ),
            ),
            (
                "outboundSatTeardowns",
                crate::json::Json::Int(
                    sat.teardowns.load(std::sync::atomic::Ordering::Relaxed) as i64
                ),
            ),
            // connection bounds (mandate §2, audit P7)
            (
                "connectionsActive",
                crate::json::Json::Int(
                    self.shared
                        .connections_active
                        .load(std::sync::atomic::Ordering::Relaxed) as i64,
                ),
            ),
            (
                "connectionsRejected",
                crate::json::Json::Int(
                    self.shared
                        .connections_rejected
                        .load(std::sync::atomic::Ordering::Relaxed) as i64,
                ),
            ),
            // command outcome write failures (mandate §5, audit P9)
            (
                "outcomeWriteFailures",
                crate::json::Json::Int(
                    self.shared
                        .outcome_write_failures
                        .load(std::sync::atomic::Ordering::Relaxed) as i64,
                ),
            ),
            (
                "uptimeMs",
                crate::json::Json::Int(now_ms() - self.shared.started_at),
            ),
            ("rssBytes", crate::json::Json::Int(ffi::rss_bytes() as i64)),
        ])
    }

    /// Phase 8: local metrics in Prometheus text exposition format, served on
    /// the LOOPBACK diagnostics listener only (`/metrics`). No secrets, no
    /// card/PSK material — only counters and gauges the operator already sees
    /// on `/health`, in a scrapeable shape (PRD §7.x "local metrics").
    pub fn metrics_text(&self) -> String {
        let snap = self.snapshot();
        let pending = snap
            .permissions
            .iter()
            .filter(|p| p.status == "pending")
            .count();
        let last_seq = self.engine.lock().unwrap().last_seq();
        let (hosts, sessions) = (snap.hosts.len(), snap.sessions.len());
        let store = self.shared.storage.lock().unwrap();
        let commands = store.commands.len();
        let unresolved = store
            .commands
            .values()
            .filter(|c| c.status == "unresolved")
            .count();
        let storage_failed = store.is_failed();
        drop(store);
        let clients = self.shared.clients.lock().unwrap().len();
        let auth_failures = *self.shared.auth_failures.lock().unwrap();
        let sat = &self.shared.saturation;
        let uptime_ms = now_ms() - self.shared.started_at;
        let g = |name: &str, help: &str, kind: &str, v: i64| {
            format!("# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {v}\n")
        };
        let mut out = String::with_capacity(1024);
        out += &g(
            "chernobyl_up",
            "always 1 once the companion is serving",
            "gauge",
            1,
        );
        out += &g(
            "chernobyl_uptime_ms",
            "process uptime since boot",
            "gauge",
            uptime_ms,
        );
        out += &g(
            "chernobyl_events_total",
            "events published into the pipeline",
            "counter",
            self.events_total.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        out += &g(
            "chernobyl_malformed_total",
            "malformed payloads rejected",
            "counter",
            self.malformed_total
                .load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        out += &g(
            "chernobyl_auth_failures",
            "consecutive WS auth failures since last success",
            "gauge",
            auth_failures as i64,
        );
        out += &g(
            "chernobyl_outbound_shed_bulk_total",
            "bulk (event fan-out) frames refused admission by the class bound",
            "counter",
            sat.shed_bulk.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        out += &g(
            "chernobyl_outbound_shed_ctrl_total",
            "control (reply/ack) frames refused admission by the class bound",
            "counter",
            sat.shed_ctrl.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        out += &g(
            "chernobyl_outbound_saturation_teardowns_total",
            "connections torn down because an outbound bound was hit",
            "counter",
            sat.teardowns.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        out += &g(
            "chernobyl_clients",
            "connected WS clients",
            "gauge",
            clients as i64,
        );
        out += &g(
            "chernobyl_hosts",
            "hosts in the current snapshot",
            "gauge",
            hosts as i64,
        );
        out += &g(
            "chernobyl_sessions",
            "sessions in the current snapshot",
            "gauge",
            sessions as i64,
        );
        out += &g(
            "chernobyl_pending_permissions",
            "pending permission requests",
            "gauge",
            pending as i64,
        );
        out += &g(
            "chernobyl_commands",
            "command rows in the journal",
            "gauge",
            commands as i64,
        );
        out += &g(
            "chernobyl_unresolved_commands",
            "commands still unresolved",
            "gauge",
            unresolved as i64,
        );
        out += &g(
            "chernobyl_last_seq",
            "last event sequence number",
            "gauge",
            last_seq,
        );
        out += &g(
            "chernobyl_storage_failed",
            "1 if the journal is in a failed state",
            "gauge",
            i64::from(storage_failed),
        );
        out += &g(
            "chernobyl_connections_active",
            "concurrent active connections across listeners",
            "gauge",
            self.shared
                .connections_active
                .load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        out += &g(
            "chernobyl_connections_rejected_total",
            "connections rejected due to listener cap or spawn failure",
            "counter",
            self.shared
                .connections_rejected
                .load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        out += &g(
            "chernobyl_outcome_write_failures_total",
            "command outcome persistence failures",
            "counter",
            self.shared
                .outcome_write_failures
                .load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        out += &g(
            "chernobyl_rss_bytes",
            "resident set size",
            "gauge",
            ffi::rss_bytes() as i64,
        );
        out
    }

    pub fn adapters_json(&self) -> crate::json::Json {
        let adapters = self.shared.adapters.lock().unwrap();
        crate::json::Json::Arr(
            adapters
                .list()
                .into_iter()
                .map(|a| {
                    crate::json::obj(vec![
                        ("id", crate::json::s(&a.id)),
                        ("type", crate::json::s(&a.kind)),
                        ("supported", crate::json::Json::Bool(a.supported)),
                        ("healthy", crate::json::Json::Bool(a.healthy)),
                        (
                            "lastError",
                            match &a.last_error {
                                Some(e) => crate::json::s(e),
                                None => crate::json::Json::Null,
                            },
                        ),
                        // PRD §7.3: what this adapter honestly supports
                        (
                            "capabilities",
                            crate::json::Json::Arr(
                                a.capabilities.iter().map(|c| c.to_json()).collect(),
                            ),
                        ),
                    ])
                })
                .collect(),
        )
    }

    /// Phase 7: accept one Claude Code hook payload (already read off the
    /// wire) and route it into the SAME seq/persist/fan-out pipeline as
    /// engine and OpenCode events — one sequence authority (C2). Returns the
    /// HTTP status the ingress should answer: 200 on acceptance (including an
    /// ignorable unknown hook, which is counted but not surfaced), 400 on a
    /// malformed payload, 404 when no Claude adapter is mounted.
    pub fn ingest_claude_hook(&self, payload: &str) -> u16 {
        let Some(cl) = self.shared.claude.lock().unwrap().clone() else {
            return 404;
        };
        match cl.ingest(payload) {
            Ok(Some(kind)) => {
                let mut engine = self.engine.lock().unwrap();
                engine.publish_external(kind);
                200
            }
            // an unknown-but-well-formed hook is counted, never faked
            Ok(None) => 200,
            Err(e) => {
                self.malformed_total
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                eprintln!("[companion] claude hook rejected: {e}");
                400
            }
        }
    }

    /// Sweeps permission expiry and advances mock runs. Driven by the
    /// scheduler thread (parity with the Node host heartbeat tick). Also
    /// hosts the adapter probe loop (`probe_interval_ms`): health + list
    /// resync so the status line never lies about a dead native runtime.
    pub fn pump(&self) {
        let now = now_ms();
        {
            let mut engine = self.engine.lock().unwrap();
            engine.pump(now);
        }
        let due = {
            let mut last = self.last_probe.lock().unwrap();
            if now - *last >= self.shared.config.probe_interval_ms as i64 {
                *last = now;
                true
            } else {
                false
            }
        };
        if due {
            // no companion lock is held across a probe (a dead OpenCode
            // endpoint costs its connect timeout)
            for rt in &self.runtimes {
                let err = rt.refresh().err().map(|e| e.to_string());
                self.shared
                    .adapters
                    .lock()
                    .unwrap()
                    .probe_result(rt.id(), err);
            }
            if let Some((_, _, dupes)) = self.live_lists() {
                for id in dupes {
                    eprintln!("[companion] session {id} is claimed by several runtimes: hidden");
                }
            }
            // Phase 7: same probe cadence for Ollama (poll-only model runtime)
            let ol = self.shared.ollama.lock().unwrap().clone();
            if let Some(ol) = ol {
                let err = ol.refresh().err().map(|e| e.to_string());
                self.shared
                    .adapters
                    .lock()
                    .unwrap()
                    .probe_result(&ol.id, err);
            }
        }
    }

    pub fn shutdown(&self, graceful: bool) {
        // stop the runtimes' event threads before the audit line
        for rt in &self.runtimes {
            rt.dispose();
        }
        let mut store = self.shared.storage.lock().unwrap();
        let fallback;
        let message = if graceful {
            "companion shut down gracefully"
        } else {
            let active = self
                .shared
                .connections_active
                .load(std::sync::atomic::Ordering::SeqCst);
            fallback = if active > 0 {
                format!("companion shut down with {active} connection(s) still active")
            } else {
                "companion shut down with connection(s) still active".to_string()
            };
            eprintln!("[companion] {fallback}");
            fallback.as_str()
        };
        let _ = store.append_audit("shutdown", message);
        let _ = store.compact();
    }

    /// Marks a device revoked in the durable store, appends an audit record,
    /// and terminates any live connections whose E2E session belongs to that device.
    pub fn revoke_device(&self, device_id: &str) -> Result<RevokeResult, String> {
        let mut store = self.shared.storage.lock().unwrap();
        let Some(mut d) = store.devices.get(device_id).cloned() else {
            return Ok(RevokeResult::NotFound);
        };
        d.revoked = true;
        store.put_device(&d)?;
        let _ = store.append_audit("revoke", &format!("device {device_id} revoked"));
        drop(store);

        let to_close: Vec<Arc<Outbound>> = {
            let mut clients = self.shared.clients.lock().unwrap();
            let mut matching = Vec::new();
            clients.retain(|(_, outbound)| {
                if outbound.device_id().as_deref() == Some(device_id) {
                    matching.push(outbound.clone());
                    false
                } else {
                    true
                }
            });
            matching
        };
        let terminated = to_close.len();
        for outbound in to_close {
            outbound.close(crate::server::CLOSE_DEVICE_REVOKED, "device revoked");
        }
        Ok(RevokeResult::Revoked { terminated })
    }

    /// Creates a fresh pairing offer in the companion's durable store and returns
    /// the QR payload JSON string the phone client parses.
    pub fn create_pairing_offer(&self, endpoints: &[String]) -> Result<String, String> {
        let eps = if endpoints.is_empty() {
            vec![format!(
                "ws://{}:{}",
                self.shared.config.server_bind, self.shared.config.server_port
            )]
        } else {
            endpoints.to_vec()
        };
        let mut store = self.shared.storage.lock().unwrap();
        crate::pairing::create_pairing_offer(&mut store, &eps)
    }

    /// The loopback control key (see [`CONTROL_KEY_FILE`]).
    pub fn control_key(&self) -> &str {
        &self.control_key
    }
}

fn json_text(msg: &ClientMsg) -> String {
    // re-serialize a parsed client message for the command payload record
    use crate::json::{self, Json};
    let v = match msg {
        ClientMsg::Hello {
            client_id,
            protocol,
        } => json::obj(vec![
            ("type", json::s("hello")),
            ("clientId", json::s(client_id)),
            ("protocol", Json::Int(*protocol as i64)),
        ]),
        ClientMsg::Ping { command_id } => json::obj(vec![
            ("type", json::s("ping")),
            ("commandId", json::s(command_id)),
        ]),
        ClientMsg::CreateSession {
            command_id,
            agent_id,
            title,
            prompt,
            project,
        } => {
            let mut fields = vec![
                ("type", json::s("create_session")),
                ("commandId", json::s(command_id)),
                ("agentId", json::s(agent_id)),
                ("title", json::s(title)),
                ("prompt", json::s(prompt)),
            ];
            if let Some(p) = project {
                fields.push(("project", json::s(p)));
            }
            json::obj(fields)
        }
        ClientMsg::SubmitPrompt {
            command_id,
            session_id,
            text,
        } => json::obj(vec![
            ("type", json::s("submit_prompt")),
            ("commandId", json::s(command_id)),
            ("sessionId", json::s(session_id)),
            ("text", json::s(text)),
        ]),
        ClientMsg::Interrupt {
            command_id,
            session_id,
        } => json::obj(vec![
            ("type", json::s("interrupt")),
            ("commandId", json::s(command_id)),
            ("sessionId", json::s(session_id)),
        ]),
        ClientMsg::PermissionDecision {
            command_id,
            permission_id,
            decision,
            revision,
            option_id,
            message,
            answers,
        } => {
            let mut fields = vec![
                ("type", json::s("permission_decision")),
                ("commandId", json::s(command_id)),
                ("permissionId", json::s(permission_id)),
                ("decision", json::s(decision)),
                ("revision", Json::Int(*revision as i64)),
            ];
            if let Some(o) = option_id {
                fields.push(("optionId", json::s(o)));
            }
            if let Some(m) = message {
                fields.push(("message", json::s(m)));
            }
            if !answers.is_empty() {
                let answers = answers
                    .iter()
                    .map(|(id, list)| {
                        let list = list.iter().map(|a| json::s(a)).collect();
                        (id.clone(), Json::Arr(list))
                    })
                    .collect();
                fields.push(("answers", Json::Obj(answers)));
            }
            json::obj(fields)
        }
        ClientMsg::ListState { command_id } => json::obj(vec![
            ("type", json::s("list_state")),
            ("commandId", json::s(command_id)),
        ]),
        ClientMsg::HsHello { device_id, .. } => json::obj(vec![
            ("type", json::s("hs_hello")),
            ("deviceId", json::s(device_id)),
        ]),
        ClientMsg::HsProof { proof } => json::obj(vec![
            ("type", json::s("hs_proof")),
            ("proof", json::s(proof)),
        ]),
        ClientMsg::SessionSubscribe {
            command_id,
            session_id,
            epoch,
            after_seq,
        } => {
            let mut fields = vec![
                ("type", json::s("session_subscribe")),
                ("commandId", json::s(command_id)),
                ("sessionId", json::s(session_id)),
            ];
            if let Some(e) = epoch {
                fields.push(("epoch", json::s(e)));
            }
            if let Some(s) = after_seq {
                fields.push(("afterSeq", Json::Int(*s as i64)));
            }
            json::obj(fields)
        }
        ClientMsg::SessionUnsubscribe {
            command_id,
            session_id,
        } => json::obj(vec![
            ("type", json::s("session_unsubscribe")),
            ("commandId", json::s(command_id)),
            ("sessionId", json::s(session_id)),
        ]),
        ClientMsg::SessionEnd {
            command_id,
            session_id,
        } => json::obj(vec![
            ("type", json::s("session_end")),
            ("commandId", json::s(command_id)),
            ("sessionId", json::s(session_id)),
        ]),
        ClientMsg::SetPermissionMode {
            command_id,
            session_id,
            mode,
        } => json::obj(vec![
            ("type", json::s("set_permission_mode")),
            ("commandId", json::s(command_id)),
            ("sessionId", json::s(session_id)),
            ("mode", json::s(mode)),
        ]),
        ClientMsg::McpAuth {
            command_id,
            session_id,
            server,
            flow,
        } => json::obj(vec![
            ("type", json::s("mcp_auth")),
            ("commandId", json::s(command_id)),
            ("sessionId", json::s(session_id)),
            ("server", json::s(server)),
            ("flow", json::s(flow)),
        ]),
        // ADR-015 §3.2 / T5: the URL carries a one-time OAuth code, so the
        // journal row records only that a callback was relayed
        ClientMsg::McpAuthCallback {
            command_id,
            session_id,
            server,
            ..
        } => json::obj(vec![
            ("type", json::s("mcp_auth_callback")),
            ("commandId", json::s(command_id)),
            ("sessionId", json::s(session_id)),
            ("server", json::s(server)),
            ("callbackUrl", json::s("<redacted>")),
        ]),
        ClientMsg::McpReconnect {
            command_id,
            session_id,
            server,
        } => json::obj(vec![
            ("type", json::s("mcp_reconnect")),
            ("commandId", json::s(command_id)),
            ("sessionId", json::s(session_id)),
            ("server", json::s(server)),
        ]),
    };
    json::stringify(&v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;

    fn temp_config(tag: &str) -> CompanionConfig {
        let dir = std::env::temp_dir().join(format!(
            "chernobyl-comp-{}-{}-{}",
            tag,
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = default_config();
        c.data_dir = dir;
        c
    }

    /// Security review 2026-10-25: the control key is minted once into the
    /// data dir (the CLI reads it there) and kept across restarts; a torn or
    /// empty file is replaced, never compared against.
    #[test]
    fn control_key_is_minted_once_and_an_empty_file_is_replaced() {
        let c = temp_config("control-key");
        let path = c.data_dir.join(CONTROL_KEY_FILE);
        let comp = Companion::boot(c.clone()).unwrap();
        let key = comp.control_key().to_string();
        assert_eq!(key.len(), 64);
        assert!(key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), key);
        comp.shutdown(true);
        drop(comp);
        let comp = Companion::boot(c.clone()).unwrap();
        assert_eq!(comp.control_key(), key, "kept across restarts");
        comp.shutdown(true);
        drop(comp);
        std::fs::write(&path, "").unwrap();
        let comp = Companion::boot(c.clone()).unwrap();
        assert_eq!(
            comp.control_key().len(),
            64,
            "an empty key file is replaced"
        );
        assert_ne!(comp.control_key(), key);
        comp.shutdown(true);
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    #[test]
    fn boot_persists_and_reconciles() {
        let c = temp_config("boot");
        let comp = Companion::boot(c.clone()).unwrap();
        assert!(comp.last_seq() >= 0);
        assert!(crate::json::stringify(&comp.health()).contains("storageFailed"));
        comp.shutdown(true);
        // second boot reconciles nothing new
        let comp2 = Companion::boot(c.clone()).unwrap();
        let store = comp2.shared.storage.lock().unwrap();
        assert_eq!(
            store
                .commands
                .values()
                .filter(|cmd| cmd.status == "unresolved")
                .count(),
            0
        );
        drop(store);
        comp2.shutdown(true);
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    #[test]
    fn mutating_commands_persist_and_dedupe_across_restart() {
        let c = temp_config("dedupe");
        let comp = Companion::boot(c.clone()).unwrap();
        let msg = ClientMsg::CreateSession {
            command_id: "cmd-restart-1".into(),
            agent_id: "a4".into(),
            title: "Restart test".into(),
            prompt: "p".into(),
            project: None,
        };
        let first = comp.handle_client_msg(msg.clone());
        assert!(matches!(
            first,
            Some(ServerMsg::Ack {
                ok: true,
                duplicate: false,
                ..
            })
        ));
        comp.shutdown(true);
        drop(comp);

        let comp2 = Companion::boot(c.clone()).unwrap();
        // the command survived: replay is a duplicate ack, not a re-run
        let sessions_before = {
            let engine = comp2.snapshot_json();
            json_len(&engine, "sessions")
        };
        let replay = comp2.handle_client_msg(msg);
        match replay {
            Some(ServerMsg::Ack { duplicate, ok, .. }) => {
                assert!(duplicate);
                assert!(ok);
            }
            other => panic!("expected duplicate ack, got {other:?}"),
        }
        let sessions_after = {
            let engine = comp2.snapshot_json();
            json_len(&engine, "sessions")
        };
        assert_eq!(
            sessions_before, sessions_after,
            "replay must not create a session"
        );
        comp2.shutdown(true);
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    /// Regression from the A/B differential against services/host: a REFUSED
    /// command must replay its refusal (ok:false + error) across restart, and
    /// a permission ack must replay with its permissionId — verbatim, never
    /// rewritten as success.
    #[test]
    fn refusals_and_permission_acks_replay_verbatim_across_restart() {
        let c = temp_config("replay-verbatim");
        let comp = Companion::boot(c.clone()).unwrap();

        // create a session (its plan starts immediately), then a submit the
        // engine refuses — "session is already running"
        let created = comp.handle_client_msg(ClientMsg::CreateSession {
            command_id: "cmd-v1".into(),
            agent_id: "a4".into(),
            title: "Verbatim".into(),
            prompt: "p".into(),
            project: None,
        });
        let session_id = match created {
            Some(ServerMsg::Ack {
                session_id: Some(s),
                ..
            }) => s,
            other => panic!("expected create ack, got {other:?}"),
        };
        let refused = comp.handle_client_msg(ClientMsg::SubmitPrompt {
            command_id: "cmd-v2".into(),
            session_id: session_id.clone(),
            text: "again".into(),
        });
        assert!(matches!(
            refused,
            Some(ServerMsg::Ack {
                ok: false,
                duplicate: false,
                ..
            })
        ));

        // a permission decision: ok:true carrying permissionId p1
        let decided = comp.handle_client_msg(ClientMsg::PermissionDecision {
            command_id: "cmd-v3".into(),
            permission_id: "p1".into(),
            decision: "approve-once".into(),
            revision: 1.0,
            option_id: None,
            message: None,
            answers: vec![],
        });
        assert!(matches!(
            decided,
            Some(ServerMsg::Ack {
                ok: true,
                permission_id: Some(p),
                ..
            }) if p == "p1"
        ));

        comp.shutdown(true);
        drop(comp);

        let comp2 = Companion::boot(c.clone()).unwrap();
        // replay the refusal: same ok:false + error, marked duplicate
        let replay_refused = comp2.handle_client_msg(ClientMsg::SubmitPrompt {
            command_id: "cmd-v2".into(),
            session_id,
            text: "again".into(),
        });
        match replay_refused {
            Some(ServerMsg::Ack {
                ok: false,
                error,
                duplicate: true,
                ..
            }) => {
                assert_eq!(error.as_deref(), Some("session is already running"));
            }
            other => panic!("expected stored refusal, got {other:?}"),
        }
        // replay the permission ack: ok:true + permissionId, verbatim
        let replay_decided = comp2.handle_client_msg(ClientMsg::PermissionDecision {
            command_id: "cmd-v3".into(),
            permission_id: "p1".into(),
            decision: "approve-once".into(),
            revision: 1.0,
            option_id: None,
            message: None,
            answers: vec![],
        });
        match replay_decided {
            Some(ServerMsg::Ack {
                ok: true,
                permission_id: Some(p),
                duplicate: true,
                ..
            }) => {
                assert_eq!(p, "p1");
            }
            other => panic!("expected stored permission ack, got {other:?}"),
        }
        comp2.shutdown(true);
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    #[test]
    fn disabled_mock_adapter_serves_no_demo_data() {
        // INSTALL.md promise: `"enabled": false` on the mock entry removes the
        // demo seed itself, not just the adapter's status row
        let mut c = temp_config("no-mock");
        c.adapters[0].enabled = false;
        let comp = Companion::boot(c.clone()).unwrap();
        let snap = comp.snapshot();
        assert!(snap.hosts.is_empty() && snap.agents.is_empty());
        assert!(snap.sessions.is_empty() && snap.permissions.is_empty());
        assert!(snap.activity.is_empty());
        comp.shutdown(true);
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    #[test]
    fn event_seq_is_monotonic_and_durable() {
        let c = temp_config("seq");
        let comp = Companion::boot(c.clone()).unwrap();
        comp.handle_client_msg(ClientMsg::CreateSession {
            command_id: "seq-1".into(),
            agent_id: "a4".into(),
            title: "Seq".into(),
            prompt: "p".into(),
            project: None,
        });
        let seq1 = comp.last_seq();
        assert!(seq1 > 0);
        comp.shutdown(true);
        let comp2 = Companion::boot(c.clone()).unwrap();
        // restart must NOT reset the sequence (C2)
        assert!(comp2.last_seq() >= seq1);
        comp2.shutdown(true);
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    // ---- Phase 3 Step 2: OpenCode adapter wiring ----

    use crate::config::AdapterEntryConfig;
    use crate::opencode::tests::{Opts as FakeOpts, start as start_fake};

    fn with_opencode(mut c: CompanionConfig, endpoint: &str) -> CompanionConfig {
        c.adapters.push(AdapterEntryConfig {
            id: "oc-1".into(),
            kind: "opencode".into(),
            enabled: true,
            endpoint: Some(endpoint.into()),
            basic_auth: None,
        });
        c
    }

    #[test]
    fn opencode_wiring_routes_session_commands_and_merges_snapshot() {
        let f = start_fake(FakeOpts::default());
        let c = with_opencode(temp_config("oc-route"), &f.addr);
        let comp = Companion::boot(c.clone()).unwrap();

        // healthy attach: status carries the PRD §7.3 capability declaration
        let adapters_json = crate::json::stringify(&comp.adapters_json());
        assert!(
            adapters_json.contains("\"session.prompt\""),
            "{adapters_json}"
        );
        assert!(
            adapters_json.contains("\"event.replay\""),
            "{adapters_json}"
        );

        // merged snapshot: the REAL runtime's entities replace mock seed
        let snap = comp.snapshot();
        let session_ids: Vec<&str> = snap.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(session_ids, vec!["ses_test1"]);
        assert!(
            snap.agents
                .iter()
                .any(|a| a.kind == "opencode" && a.name == "build")
        );
        assert!(snap.agents.iter().all(|a| a.kind == "opencode"));

        // SubmitPrompt → native prompt_async, recorded executed (G1)
        let reply = comp.handle_client_msg(ClientMsg::SubmitPrompt {
            command_id: "cmd-oc-1".into(),
            session_id: "ses_test1".into(),
            text: "run it".into(),
        });
        assert!(matches!(reply, Some(ServerMsg::Ack { ok: true, .. })));
        {
            let store = comp.shared.storage.lock().unwrap();
            assert_eq!(
                store.commands.get("cmd-oc-1").map(|r| r.status.as_str()),
                Some("executed")
            );
        }

        // Interrupt → native abort
        let reply = comp.handle_client_msg(ClientMsg::Interrupt {
            command_id: "cmd-oc-2".into(),
            session_id: "ses_test1".into(),
        });
        assert!(matches!(reply, Some(ServerMsg::Ack { ok: true, .. })));
        {
            let reqs = f.requests.lock().unwrap();
            assert!(
                reqs.iter()
                    .any(|r| r.contains("prompt_async") && r.contains("run it"))
            );
            assert!(reqs.iter().any(|r| r.contains("/abort")));
        }

        // session-neutral messages stay with the engine, never sent native
        let hello = comp.handle_client_msg(ClientMsg::Hello {
            client_id: "c".into(),
            protocol: 2.0,
        });
        assert!(matches!(hello, Some(ServerMsg::HelloAck { .. })));
        assert!(
            f.requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| !r.contains("/hello"))
        );

        // permission the adapter never surfaced → engine authority refuses,
        // nothing reaches the native reply endpoint
        let reply = comp.handle_client_msg(ClientMsg::PermissionDecision {
            command_id: "cmd-oc-3".into(),
            permission_id: "engine-unknown".into(),
            decision: "approve-once".into(),
            revision: 1.0,
            option_id: None,
            message: None,
            answers: vec![],
        });
        assert!(matches!(reply, Some(ServerMsg::Ack { ok: false, .. })));
        assert!(
            f.requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| !r.contains("/permission/"))
        );

        comp.shutdown(true);
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    #[test]
    fn native_permission_is_engine_validated_then_executed_natively() {
        // native asked → SSE → ingested as an ENGINE record (G2) → every
        // decision is validated engine-side BEFORE any native call
        let script = concat!(
            r#"data: {"payload":{"id":"evt_pa","type":"permission.asked","properties":{"id":"per_wire1","sessionID":"ses_test1","permission":"bash","patterns":["git status"],"tool":{"messageID":"msg_1","callID":"c1"}}}}"#,
            "\n\n"
        );
        let f = start_fake(FakeOpts {
            sse: Some(script.to_string()),
            ..Default::default()
        });
        let c = with_opencode(temp_config("oc-perm"), &f.addr);
        let comp = Companion::boot(c.clone()).unwrap();
        let oc = comp
            .shared
            .opencode
            .lock()
            .unwrap()
            .clone()
            .expect("attached at boot");

        // the event thread is async: bounded wait for the ask
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !oc.knows_permission("per_wire1") && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            oc.knows_permission("per_wire1"),
            "adapter never saw permission.asked"
        );

        // the engine holds the ingested record: pending, native id preserved
        let snap = comp.snapshot();
        let p = snap
            .permissions
            .iter()
            .find(|p| p.id == "per_wire1")
            .expect("engine ingested the native request");
        assert_eq!(p.status, "pending");
        assert_eq!(p.native_request_id, "per_wire1");
        assert_eq!(p.revision, 1);

        // stale revision fails CLOSED before anything runs natively (G2)
        let reply = comp.handle_client_msg(ClientMsg::PermissionDecision {
            command_id: "cmd-perm-bad".into(),
            permission_id: "per_wire1".into(),
            decision: "approve-once".into(),
            revision: 99.0,
            option_id: None,
            message: None,
            answers: vec![],
        });
        assert!(matches!(reply, Some(ServerMsg::Ack { ok: false, .. })));
        assert!(
            f.requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| !r.contains("/permission/"))
        );
        assert_eq!(
            comp.snapshot()
                .permissions
                .iter()
                .find(|p| p.id == "per_wire1")
                .map(|p| p.status.as_str()),
            Some("pending")
        );

        // valid decision: native executes exactly once, engine records it
        let reply = comp.handle_client_msg(ClientMsg::PermissionDecision {
            command_id: "cmd-perm-ok".into(),
            permission_id: "per_wire1".into(),
            decision: "approve-once".into(),
            revision: 1.0,
            option_id: None,
            message: None,
            answers: vec![],
        });
        assert!(matches!(reply, Some(ServerMsg::Ack { ok: true, .. })));
        assert!(
            f.requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.contains("/permission/per_wire1/reply")
                    && r.contains("\"reply\":\"once\""))
        );
        let snap2 = comp.snapshot();
        let p = snap2
            .permissions
            .iter()
            .find(|p| p.id == "per_wire1")
            .unwrap();
        assert_eq!(p.status, "approved");
        assert_eq!(p.decision.as_deref(), Some("approve-once"));
        assert_eq!(p.revision, 2); // resolution invalidates the old revision

        comp.shutdown(true);
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    #[test]
    fn outcome_write_failure_increments_counter_and_surfaces_in_health_and_metrics() {
        // Regression for P9: when journal writes fail, the failed outcome
        // write must be logged, increment outcomeWriteFailures, and surface
        // in /health and /metrics (never silently dropped with let _ =).
        let c = temp_config("p9-outcome-fail");
        let data_dir = c.data_dir.clone();
        let comp = Companion::boot(c).unwrap();

        let health0 = comp.health();
        let get = |h: &crate::json::Json, key: &str| {
            crate::json::as_i64(crate::json::get(h, key).expect(key)).expect(key)
        };
        assert_eq!(get(&health0, "outcomeWriteFailures"), 0);

        // Fail after 1 append: put_command succeeds (first append), but the
        // subsequent update_command_outcome fails on its append.
        comp.shared.storage.lock().unwrap().fail_after_appends = Some(1);

        let msg = ClientMsg::CreateSession {
            command_id: "cmd-p9".into(),
            agent_id: "a1".into(),
            title: "T".into(),
            prompt: "P".into(),
            project: None,
        };
        let reply = comp.handle_client_msg(msg);
        assert!(matches!(reply, Some(ServerMsg::Ack { ok: true, .. })));

        assert_eq!(
            comp.shared
                .outcome_write_failures
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        let health1 = comp.health();
        assert_eq!(get(&health1, "outcomeWriteFailures"), 1);

        let metrics = comp.metrics_text();
        assert!(metrics.contains("chernobyl_outcome_write_failures_total 1"));

        comp.shutdown(true);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// ADR-015 T5: the callback URL carries a one-time OAuth code. The command
    /// is persisted before execution like every mutating command, but with
    /// the URL replaced, so the journal file never holds the code.
    #[test]
    fn mcp_auth_callback_is_journaled_redacted() {
        let c = temp_config("mcp-redact");
        let journal = c.data_dir.join("companion.journal");
        let comp = Companion::boot(c.clone()).unwrap();
        let code = "one-time-code-5f2b9d";
        let msg = ClientMsg::McpAuthCallback {
            command_id: "cmd-mcp-cb".into(),
            session_id: "s1".into(),
            server: "sentry".into(),
            callback_url: format!("http://localhost:44818/callback?code={code}&state=st-1"),
        };
        let reply = comp.handle_client_msg(msg.clone());
        assert!(matches!(
            reply,
            Some(ServerMsg::Ack { ok: false, ref error, .. })
                if error.as_deref() == Some("not supported by this agent")
        ));
        {
            let store = comp.shared.storage.lock().unwrap();
            let rec = store.commands.get("cmd-mcp-cb").expect("persisted");
            assert!(
                rec.payload.contains(r#""callbackUrl":"<redacted>""#),
                "{}",
                rec.payload
            );
            assert_eq!(rec.status, "refused");
        }
        // still deduplicated by commandId
        assert!(matches!(
            comp.handle_client_msg(msg),
            Some(ServerMsg::Ack {
                duplicate: true,
                ..
            })
        ));
        let on_disk = std::fs::read_to_string(&journal).unwrap();
        assert!(on_disk.contains("<redacted>"));
        assert!(
            !on_disk.contains(code),
            "the OAuth code reached the journal"
        );
        // compaction rewrites the journal from memory: still redacted
        comp.shutdown(true);
        let on_disk = std::fs::read_to_string(&journal).unwrap();
        assert!(!on_disk.contains(code) && !on_disk.contains("localhost:44818"));
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    /// ADR-015 step 1: no runtime claims the live messages yet, so the engine
    /// refuses them all. Subscribe/unsubscribe are non-mutating: no journal
    /// row, and no rewrite of a persisted command that shares their id.
    #[test]
    fn live_messages_are_refused_until_a_runtime_claims_them() {
        let c = temp_config("live-refused");
        let comp = Companion::boot(c.clone()).unwrap();
        let refused = |reply: Option<ServerMsg>| {
            matches!(
                reply,
                Some(ServerMsg::Ack { ok: false, ref error, duplicate: false, .. })
                    if error.as_deref() == Some("not supported by this agent")
            )
        };
        let subscribe = |command_id: &str| ClientMsg::SessionSubscribe {
            command_id: command_id.into(),
            session_id: "s1".into(),
            epoch: None,
            after_seq: None,
        };
        assert!(refused(comp.handle_client_msg(ClientMsg::SessionEnd {
            command_id: "cmd-live-1".into(),
            session_id: "s1".into(),
        })));
        assert!(refused(comp.handle_client_msg(subscribe("cmd-live-2"))));
        assert!(refused(comp.handle_client_msg(
            ClientMsg::SessionUnsubscribe {
                command_id: "cmd-live-3".into(),
                session_id: "s1".into(),
            }
        )));
        assert!(refused(comp.handle_client_msg(
            ClientMsg::SetPermissionMode {
                command_id: "cmd-live-4".into(),
                session_id: "s1".into(),
                mode: "plan".into(),
            }
        )));
        assert!(refused(comp.handle_client_msg(ClientMsg::McpAuth {
            command_id: "cmd-live-5".into(),
            session_id: "s1".into(),
            server: "sentry".into(),
            flow: "desktop".into(),
        })));
        assert!(refused(comp.handle_client_msg(ClientMsg::McpReconnect {
            command_id: "cmd-live-7".into(),
            session_id: "s1".into(),
            server: "sentry".into(),
        })));
        {
            let store = comp.shared.storage.lock().unwrap();
            for id in ["cmd-live-1", "cmd-live-4", "cmd-live-5", "cmd-live-7"] {
                let status = store.commands.get(id).map(|r| r.status.as_str());
                assert_eq!(status, Some("refused"), "{id}");
            }
            for id in ["cmd-live-2", "cmd-live-3"] {
                assert!(!store.commands.contains_key(id), "{id} is non-mutating");
            }
        }
        let created = comp.handle_client_msg(ClientMsg::CreateSession {
            command_id: "cmd-live-6".into(),
            agent_id: "a4".into(),
            title: "T".into(),
            prompt: "p".into(),
            project: None,
        });
        assert!(matches!(created, Some(ServerMsg::Ack { ok: true, .. })));
        assert!(refused(comp.handle_client_msg(subscribe("cmd-live-6"))));
        {
            let store = comp.shared.storage.lock().unwrap();
            let rec = store.commands.get("cmd-live-6").unwrap();
            assert_eq!((rec.ok, rec.error.as_deref()), (Some(true), None));
        }
        comp.shutdown(true);
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    fn json_len(v: &crate::json::Json, key: &str) -> usize {
        crate::json::get(v, key)
            .and_then(crate::json::as_arr)
            .map(|a| a.len())
            .unwrap_or(0)
    }

    // ---- ADR-015 step 4: LiveRuntime router ----

    use crate::adapters::{AdapterError, Capability};
    use crate::protocol::{PermissionOption, PermissionRequest};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Test-only runtime: canned agent, session and permission ids; accepts
    /// and records every message routed to it.
    struct FakeRuntime {
        id: String,
        healthy: AtomicBool,
        agents: Vec<String>,
        sessions: Vec<String>,
        permissions: Vec<String>,
        sent: Mutex<Vec<ClientMsg>>,
    }

    impl FakeRuntime {
        fn new(id: &str, agents: &[&str], sessions: &[&str], perms: &[&str]) -> Arc<FakeRuntime> {
            let owned = |l: &[&str]| l.iter().map(|s| s.to_string()).collect();
            Arc::new(FakeRuntime {
                id: id.into(),
                healthy: AtomicBool::new(true),
                agents: owned(agents),
                sessions: owned(sessions),
                permissions: owned(perms),
                sent: Mutex::new(Vec::new()),
            })
        }

        /// Command ids routed here, in order.
        fn got(&self) -> Vec<String> {
            let sent = self.sent.lock().unwrap();
            sent.iter()
                .filter_map(|m| m.command_id().map(str::to_string))
                .collect()
        }
    }

    impl LiveRuntime for FakeRuntime {
        fn id(&self) -> &str {
            &self.id
        }
        fn is_healthy(&self) -> bool {
            self.healthy.load(Ordering::SeqCst)
        }
        fn agents(&self) -> Vec<Agent> {
            let agent = |id: &String| Agent {
                id: id.clone(),
                host_id: self.id.clone(),
                kind: "fake".into(),
                name: id.clone(),
                state: "online".into(),
                capabilities: vec![],
                accepts_new_session: true,
                simulated: false,
                live: None,
            };
            self.agents.iter().map(agent).collect()
        }
        fn sessions(&self) -> Vec<Session> {
            let session = |id: &String| Session {
                id: id.clone(),
                agent_id: "fake".into(),
                host_id: self.id.clone(),
                title: id.clone(),
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
            self.sessions.iter().map(session).collect()
        }
        fn owns_session(&self, session_id: &str) -> bool {
            self.sessions.iter().any(|s| s == session_id)
        }
        fn knows_permission(&self, permission_id: &str) -> bool {
            self.permissions.iter().any(|p| p == permission_id)
        }
        fn send(&self, msg: &ClientMsg) -> Result<ServerMsg, AdapterError> {
            self.sent.lock().unwrap().push(msg.clone());
            Ok(ServerMsg::ack_ok(msg.command_id().unwrap_or(""), None))
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }
        fn dispose(&self) {}
    }

    fn ok(reply: Option<ServerMsg>) -> bool {
        matches!(reply, Some(ServerMsg::Ack { ok: true, .. }))
    }

    fn refusal(reply: Option<ServerMsg>) -> String {
        match reply {
            Some(ServerMsg::Ack {
                ok: false,
                error: Some(e),
                ..
            }) => e,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn create_session_routes_by_agent_host() {
        let c = temp_config("route-create");
        let mut comp = Companion::boot(c.clone()).unwrap();
        let a = FakeRuntime::new("rt-a", &["alpha"], &[], &[]);
        let b = FakeRuntime::new("rt-b", &["beta"], &[], &[]);
        comp.runtimes.push(a.clone());
        comp.runtimes.push(b.clone());
        let create = |id: &str, agent: &str| ClientMsg::CreateSession {
            command_id: id.into(),
            agent_id: agent.into(),
            title: "T".into(),
            prompt: "p".into(),
            project: None,
        };
        assert!(ok(comp.handle_client_msg(create("cr-1", "beta"))));
        assert!(ok(comp.handle_client_msg(create("cr-2", "alpha"))));
        assert_eq!(
            (a.got(), b.got()),
            (vec!["cr-2".into()], vec!["cr-1".into()])
        );
        // an agent no runtime hosts stays with the engine (demo agent a4)
        assert!(ok(comp.handle_client_msg(create("cr-3", "a4"))));
        // an unhealthy host claims nothing: the engine does not know its agent
        b.healthy.store(false, Ordering::SeqCst);
        let refused = refusal(comp.handle_client_msg(create("cr-4", "beta")));
        assert_eq!(refused, "unknown agent");
        assert_eq!((a.got().len(), b.got().len()), (1, 1));
        comp.shutdown(true);
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    #[test]
    fn session_commands_route_to_owner() {
        let c = temp_config("route-session");
        let mut comp = Companion::boot(c.clone()).unwrap();
        let a = FakeRuntime::new("rt-a", &[], &["ses-a"], &[]);
        let b = FakeRuntime::new("rt-b", &[], &["ses-b"], &["rt-b:p1", "rt-b:p2"]);
        comp.runtimes.push(a.clone());
        comp.runtimes.push(b.clone());
        let (cmd, sid) = (|n: u32| format!("sc-{n}"), "ses-b".to_string());
        for msg in [
            ClientMsg::SubmitPrompt {
                command_id: cmd(1),
                session_id: sid.clone(),
                text: "go".into(),
            },
            ClientMsg::Interrupt {
                command_id: cmd(2),
                session_id: sid.clone(),
            },
            ClientMsg::SessionEnd {
                command_id: cmd(3),
                session_id: sid.clone(),
            },
            ClientMsg::SetPermissionMode {
                command_id: cmd(4),
                session_id: sid.clone(),
                mode: "plan".into(),
            },
            ClientMsg::McpAuth {
                command_id: cmd(5),
                session_id: sid.clone(),
                server: "sentry".into(),
                flow: "desktop".into(),
            },
            ClientMsg::McpAuthCallback {
                command_id: cmd(6),
                session_id: sid.clone(),
                server: "sentry".into(),
                callback_url: "http://localhost:1/cb?code=c&state=s".into(),
            },
            ClientMsg::McpReconnect {
                command_id: cmd(7),
                session_id: sid.clone(),
                server: "sentry".into(),
            },
        ] {
            assert!(ok(comp.handle_client_msg(msg)));
        }
        assert!(ok(comp.handle_client_msg(ClientMsg::SubmitPrompt {
            command_id: cmd(8),
            session_id: "ses-a".into(),
            text: "go".into(),
        })));
        assert_eq!(b.got(), (1..=7).map(cmd).collect::<Vec<_>>());
        assert_eq!(a.got(), vec![cmd(8)]);
        // a session no runtime owns stays with the engine, which refuses it
        let refused = refusal(comp.handle_client_msg(ClientMsg::SubmitPrompt {
            command_id: cmd(9),
            session_id: "ses-none".into(),
            text: "go".into(),
        }));
        assert!(!refused.is_empty());
        assert_eq!((a.got().len(), b.got().len()), (1, 7));

        // permissions the runtime surfaced: the engine validates each decision
        // first (G2), then the runtime gets it with the option the engine
        // selected
        let now = now_ms();
        for native in ["p1", "p2"] {
            let option = |id: &str, decision: &str, takes_message| PermissionOption {
                id: id.into(),
                decision: decision.into(),
                label: id.into(),
                rule: None,
                scope: "once",
                destination: None,
                takes_message,
                stops_turn: false,
            };
            comp.engine
                .lock()
                .unwrap()
                .ingest_permission(PermissionRequest {
                    id: format!("rt-b:{native}"),
                    native_request_id: native.into(),
                    session_id: sid.clone(),
                    agent_id: "rt-b".into(),
                    host_id: "rt-b".into(),
                    action: "shell".into(),
                    title: "Run ls".into(),
                    command: "ls".into(),
                    args: vec![],
                    directory: None,
                    risk: "high".into(),
                    status: "pending".into(),
                    supported_decisions: vec![],
                    revision: 1,
                    fingerprint: None,
                    requested_at: now,
                    expires_at: now,
                    context: String::new(),
                    decided_at: None,
                    decision: None,
                    tool_item_id: Some("tool-1".into()),
                    options: vec![
                        option("allow", "approve-once", false),
                        option("deny", "reject", true),
                    ],
                    detail: None,
                });
        }
        let decide =
            |id: &str, perm: &str, decision: &str, revision: f64| ClientMsg::PermissionDecision {
                command_id: id.into(),
                permission_id: format!("rt-b:{perm}"),
                decision: decision.into(),
                revision,
                option_id: None,
                message: None,
                answers: vec![],
            };
        let last_sent = || b.sent.lock().unwrap().last().cloned();
        assert!(
            !refusal(comp.handle_client_msg(decide("pd-1", "p1", "approve-once", 9.0))).is_empty()
        );
        assert_eq!(
            b.got().len(),
            7,
            "a stale decision never reaches the runtime"
        );
        // a legacy tap (no optionId) reaches the runtime as the selected option
        assert!(ok(comp.handle_client_msg(decide(
            "pd-2",
            "p1",
            "approve-once",
            1.0
        ))));
        let mut forwarded = decide("pd-2", "p1", "approve-once", 1.0);
        if let ClientMsg::PermissionDecision { option_id, .. } = &mut forwarded {
            *option_id = Some("allow".into());
        }
        assert_eq!(last_sent(), Some(forwarded));
        // an explicit option travels with its message verbatim
        let mut with_reason = decide("pd-3", "p2", "reject", 1.0);
        if let ClientMsg::PermissionDecision {
            option_id, message, ..
        } = &mut with_reason
        {
            *option_id = Some("deny".into());
            *message = Some("use staging".into());
        }
        assert!(ok(comp.handle_client_msg(with_reason.clone())));
        assert_eq!(last_sent(), Some(with_reason));
        let snap = comp.snapshot();
        let state = |id: &str| {
            let p = snap.permissions.iter().find(|p| p.id == id).unwrap();
            (p.status.clone(), p.revision)
        };
        assert_eq!(state("rt-b:p1"), ("approved".to_string(), 2));
        assert_eq!(state("rt-b:p2"), ("rejected".to_string(), 2));
        comp.shutdown(true);
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    #[test]
    fn duplicate_session_id_across_runtimes_is_refused() {
        let c = temp_config("route-dupe");
        let mut comp = Companion::boot(c.clone()).unwrap();
        let a = FakeRuntime::new("rt-a", &[], &["ses-dup", "ses-a"], &[]);
        let b = FakeRuntime::new("rt-b", &[], &["ses-dup"], &[]);
        comp.runtimes.push(a.clone());
        comp.runtimes.push(b.clone());
        let refused = refusal(comp.handle_client_msg(ClientMsg::SubmitPrompt {
            command_id: "dup-1".into(),
            session_id: "ses-dup".into(),
            text: "which one?".into(),
        }));
        assert!(refused.contains("more than one runtime"), "{refused}");
        assert!(a.got().is_empty() && b.got().is_empty());
        {
            let store = comp.shared.storage.lock().unwrap();
            assert_eq!(store.commands["dup-1"].status, "refused");
        }
        // the snapshot never shows the ambiguous id
        let ids: Vec<String> = comp.snapshot().sessions.into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["ses-a".to_string()]);
        // the uniquely owned session still routes
        assert!(ok(comp.handle_client_msg(ClientMsg::Interrupt {
            command_id: "dup-2".into(),
            session_id: "ses-a".into(),
        })));
        assert_eq!(a.got(), vec!["dup-2".to_string()]);
        comp.shutdown(true);
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    #[test]
    fn demo_data_hidden_when_any_live_runtime_is_healthy() {
        let c = temp_config("route-demo");
        let mut comp = Companion::boot(c.clone()).unwrap();
        let demo = comp.snapshot();
        assert!(demo.agents.iter().any(|a| a.simulated));
        assert!(!demo.sessions.is_empty());
        let a = FakeRuntime::new("rt-a", &["alpha"], &["ses-a"], &[]);
        let b = FakeRuntime::new("rt-b", &["beta"], &["ses-b"], &[]);
        a.healthy.store(false, Ordering::SeqCst);
        b.healthy.store(false, Ordering::SeqCst);
        comp.runtimes.push(a.clone());
        comp.runtimes.push(b.clone());
        let lists = |comp: &Companion| {
            let snap = comp.snapshot();
            let agents: Vec<String> = snap.agents.into_iter().map(|a| a.id).collect();
            let sessions: Vec<String> = snap.sessions.into_iter().map(|s| s.id).collect();
            (agents, sessions)
        };
        // no runtime is healthy: the demo data stays
        let demo_lists = lists(&comp);
        assert_eq!(demo_lists.0.len(), demo.agents.len());
        // one healthy runtime hides it: only real entities are shown
        b.healthy.store(true, Ordering::SeqCst);
        assert_eq!(lists(&comp), (vec!["beta".into()], vec!["ses-b".into()]));
        // every healthy runtime's lists, concatenated in config order
        a.healthy.store(true, Ordering::SeqCst);
        assert_eq!(
            lists(&comp),
            (
                vec!["alpha".into(), "beta".into()],
                vec!["ses-a".into(), "ses-b".into()]
            )
        );
        // hosts and permissions stay engine-owned
        let snap = comp.snapshot();
        assert_eq!(snap.hosts.len(), demo.hosts.len());
        assert_eq!(snap.permissions.len(), demo.permissions.len());
        comp.shutdown(true);
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }
}
