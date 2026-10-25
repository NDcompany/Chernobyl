//! Companion orchestration — the combined local backend: engine (mock agent
//! authority) + durable store + adapter manager + counters. Mirrors the Node
//! companion's guarantees: persist-before-execute, idempotency by commandId,
//! boot reconciliation, latched fail-closed on write failure, event seq
//! continuity (C2).

use crate::adapters::AdapterManager;
use crate::claude::ClaudeAdapter;
use crate::config::CompanionConfig;
use crate::engine::Engine;
use crate::ffi;
use crate::ollama::OllamaAdapter;
use crate::opencode::OpenCodeAdapter;
use crate::protocol::{ClientMsg, EventKind, PROTOCOL_VERSION, ServerMsg, now_ms};
use crate::server::Outbound;
use crate::storage::{CommandRecord, Storage};
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

#[derive(Debug, PartialEq, Eq)]
pub enum RevokeResult {
    NotFound,
    Revoked { terminated: usize },
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
    /// Last adapter probe timestamp for the probe-interval gate.
    last_probe: Mutex<i64>,
    pub events_total: Arc<std::sync::atomic::AtomicU64>,
    pub malformed_total: std::sync::atomic::AtomicU64,
}

impl Companion {
    pub fn boot(config: CompanionConfig) -> Result<Companion, String> {
        let storage = Storage::open(&config.data_dir, config.events_cap, config.commands_cap)?;
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
        let engine = Engine::new(
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
                // clients and passes plaintext through for loopback parity.
                // Classed Bulk (mandate §4): fan-out gets its own, lower bound
                // so it can never starve reply/ack frames.
                clients.retain(|(_, q)| {
                    q.enqueue_sealed(text.clone().into_bytes(), crate::server::MsgClass::Bulk)
                        .is_ok()
                });
            }),
        );

        let companion = Companion {
            shared,
            engine: Arc::new(Mutex::new(engine)),
            last_probe: Mutex::new(now_ms()),
            events_total,
            malformed_total: std::sync::atomic::AtomicU64::new(0),
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

        // Phase 3 routing: a HEALTHY attached adapter owns the session-
        // scoped mutating commands (the native runtime is the truth for
        // sessions it knows); permission decisions go native only when the
        // adapter surfaced that request — otherwise engine authority (G2).
        // Session-neutral messages always stay with the engine.
        let oc = self.shared.opencode.lock().unwrap().clone();
        let to_adapter = matches!(
            (&oc, &msg),
            (Some(oc), m) if oc.is_healthy() && Self::routes_to_adapter(m, oc)
        );
        let reply = if to_adapter {
            let oc = oc.as_ref().expect("routed only when attached");
            match &msg {
                // G2 stays with the engine: it validates the decision
                // (native metadata, pending, expiry, revision), the native
                // runtime only EXECUTES it, then the engine records the
                // accepted outcome — success can never outrun validation.
                ClientMsg::PermissionDecision {
                    command_id,
                    permission_id,
                    decision,
                    revision,
                } => {
                    let verdict = {
                        let mut engine = self.engine.lock().unwrap();
                        engine.validate_native_permission_decision(
                            permission_id,
                            decision,
                            *revision,
                        )
                    };
                    if let Err(e) = verdict {
                        Some(ServerMsg::ack_err(command_id, &e))
                    } else {
                        match oc.send(&msg) {
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
                _ => Some(match oc.send(&msg) {
                    Ok(reply) => reply,
                    // transport/validation faults surface as refusals — never
                    // as a fabricated engine outcome
                    Err(e) => {
                        ServerMsg::ack_err(msg_cmd_id.as_deref().unwrap_or(""), &e.to_string())
                    }
                }),
            }
        } else {
            let mut engine = self.engine.lock().unwrap();
            engine.handle(msg)
        };

        // capture the ORIGINAL ack outcome (ok, error, ids) so replays —
        // including after a restart, when the engine's ack cache is gone —
        // return the very same ack
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
        ) = (&reply, msg_cmd_id)
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
        if let Some(oc) = self.shared.opencode.lock().unwrap().as_ref()
            && oc.is_healthy()
        {
            // A healthy real runtime is the truth for entities it owns;
            // mock seed sessions/agents shown beside real ones would pass
            // demo data off as live (PRD §13.1). Permissions/activity/hosts
            // stay engine-owned — adapter permissions ARE engine records
            // (ingested at subscribe time).
            snap.sessions = oc.sessions();
            snap.agents = oc.agents();
        }
        snap
    }

    /// Phase 3 routing rule (one place so tests can pin it).
    fn routes_to_adapter(msg: &ClientMsg, oc: &OpenCodeAdapter) -> bool {
        match msg {
            ClientMsg::CreateSession { .. }
            | ClientMsg::SubmitPrompt { .. }
            | ClientMsg::Interrupt { .. } => true,
            // engine-authoritative unless the adapter surfaced the request
            // natively (revision/expiry checked by the engine either way)
            ClientMsg::PermissionDecision { permission_id, .. } => {
                oc.knows_permission(permission_id)
            }
            ClientMsg::Hello { .. }
            | ClientMsg::Ping { .. }
            | ClientMsg::ListState { .. }
            | ClientMsg::HsHello { .. }
            | ClientMsg::HsProof { .. } => false,
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
            // clone the Arc out first: no companion lock is held across
            // the HTTP probe (a dead endpoint costs its connect timeout)
            let oc = self.shared.opencode.lock().unwrap().clone();
            if let Some(oc) = oc {
                let err = oc.refresh().err().map(|e| e.to_string());
                self.shared
                    .adapters
                    .lock()
                    .unwrap()
                    .probe_result(&oc.id, err);
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
        // stop the adapter's event thread before the audit line
        if let Some(oc) = self.shared.opencode.lock().unwrap().take() {
            oc.dispose();
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
        } => json::obj(vec![
            ("type", json::s("permission_decision")),
            ("commandId", json::s(command_id)),
            ("permissionId", json::s(permission_id)),
            ("decision", json::s(decision)),
            ("revision", Json::Int(*revision as i64)),
        ]),
        ClientMsg::ListState { command_id } => json::obj(vec![
            ("type", json::s("list_state")),
            ("commandId", json::s(command_id)),
        ]),
        ClientMsg::HsHello { device_id } => json::obj(vec![
            ("type", json::s("hs_hello")),
            ("deviceId", json::s(device_id)),
        ]),
        ClientMsg::HsProof { proof } => json::obj(vec![
            ("type", json::s("hs_proof")),
            ("proof", json::s(proof)),
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

    fn json_len(v: &crate::json::Json, key: &str) -> usize {
        crate::json::get(v, key)
            .and_then(crate::json::as_arr)
            .map(|a| a.len())
            .unwrap_or(0)
    }
}
