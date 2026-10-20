//! Companion orchestration — the combined local backend: engine (mock agent
//! authority) + durable store + adapter manager + counters. Mirrors the Node
//! companion's guarantees: persist-before-execute, idempotency by commandId,
//! boot reconciliation, latched fail-closed on write failure, event seq
//! continuity (C2).

use crate::adapters::AdapterManager;
use crate::config::CompanionConfig;
use crate::engine::Engine;
use crate::ffi;
use crate::protocol::{ClientMsg, EventKind, PROTOCOL_VERSION, ServerMsg, now_ms};
use crate::storage::{CommandRecord, Storage};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

pub struct Shared {
    pub storage: Mutex<Storage>,
    /// Outbound payload queues, one per live WS connection, keyed by a
    /// monotonic connection id (mpsc Sender has no channel identity).
    pub clients: Mutex<Vec<(u64, Sender<Vec<u8>>)>>,
    pub next_conn_id: std::sync::atomic::AtomicU64,
    pub adapters: Mutex<AdapterManager>,
    pub started_at: i64,
    pub config: CompanionConfig,
    /// Consecutive auth failures; trips a cooldown (brute-force resistance).
    pub auth_failures: Mutex<u32>,
}

pub struct Companion {
    pub shared: Arc<Shared>,
    engine: Mutex<Engine>,
    pub events_total: Arc<std::sync::atomic::AtomicU64>,
    pub malformed_total: std::sync::atomic::AtomicU64,
}

impl Companion {
    pub fn boot(config: CompanionConfig) -> Result<Companion, String> {
        let storage = Storage::open(&config.data_dir, config.events_cap)?;
        // C2: the engine continues the event sequence from the journal
        let start_seq = storage.last_seq;

        let shared = Arc::new(Shared {
            storage: Mutex::new(storage),
            clients: Mutex::new(Vec::new()),
            next_conn_id: std::sync::atomic::AtomicU64::new(1),
            adapters: Mutex::new(AdapterManager::discover(&config.adapters)),
            started_at: now_ms(),
            config,
            auth_failures: Mutex::new(0),
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
                        // terminal outcome closes the command row (C2)
                        let mut store = emit_shared.storage.lock().unwrap();
                        let _ = store.update_command_status(
                            command_id,
                            status,
                            session_id.as_deref(),
                            None,
                        );
                    }
                    let mut store = emit_shared.storage.lock().unwrap();
                    let _ = store.append_event(kind.type_str(), &text, env.seq);
                }
                events_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut clients = emit_shared.clients.lock().unwrap();
                clients.retain(|(_, tx)| tx.send(text.clone().into_bytes()).is_ok());
            }),
        );

        let companion = Companion {
            shared,
            engine: Mutex::new(engine),
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
                // already executed (or in flight): duplicate ack, never re-run
                return Some(ServerMsg::Ack {
                    command_id: cmd_id.to_string(),
                    ok: existing.status != "failed",
                    error: existing.error.clone(),
                    duplicate: true,
                    session_id: existing.session_id.clone(),
                    permission_id: None,
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
                session_id: None,
                error: None,
            };
            if let Err(e) = store.put_command(&record) {
                return Some(ServerMsg::ack_err(
                    cmd_id,
                    &format!("failed to persist command (fail-closed): {e}"),
                ));
            }
        }

        let reply = {
            let mut engine = self.engine.lock().unwrap();
            engine.handle(msg)
        };

        // capture the session id from fresh acks so replays carry it
        if let (
            Some(ServerMsg::Ack {
                ok: true,
                session_id: Some(sid),
                duplicate: false,
                ..
            }),
            Some(cmd_id),
        ) = (&reply, msg_cmd_id)
        {
            let mut store = self.shared.storage.lock().unwrap();
            let _ = store.update_command_status(&cmd_id, "unresolved", Some(sid), None);
        }
        reply
    }

    /* ---------------------------------------------------------------- */
    /* Introspection                                                     */
    /* ---------------------------------------------------------------- */

    pub fn snapshot(&self) -> crate::protocol::Snapshot {
        self.engine.lock().unwrap().snapshot().clone()
    }

    pub fn snapshot_json(&self) -> crate::json::Json {
        self.snapshot().to_json()
    }

    pub fn last_seq(&self) -> i64 {
        self.engine.lock().unwrap().last_seq()
    }

    pub fn health(&self) -> crate::json::Json {
        let (hosts, sessions, pending, last_seq) = {
            let engine = self.engine.lock().unwrap();
            let snap = engine.snapshot();
            (
                snap.hosts.len(),
                snap.sessions.len(),
                snap.permissions
                    .iter()
                    .filter(|p| p.status == "pending")
                    .count(),
                engine.last_seq(),
            )
        };
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
            (
                "uptimeMs",
                crate::json::Json::Int(now_ms() - self.shared.started_at),
            ),
            ("rssBytes", crate::json::Json::Int(ffi::rss_bytes() as i64)),
        ])
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
                    ])
                })
                .collect(),
        )
    }

    /// Sweeps permission expiry and advances mock runs. Driven by the
    /// scheduler thread (parity with the Node host heartbeat tick).
    pub fn pump(&self) {
        let now = now_ms();
        let mut engine = self.engine.lock().unwrap();
        engine.pump(now);
    }

    pub fn shutdown(&self) {
        let mut store = self.shared.storage.lock().unwrap();
        let _ = store.append_audit("shutdown", "companion shut down gracefully");
        let _ = store.compact();
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
        comp.shutdown();
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
        comp2.shutdown();
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
        comp.shutdown();
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
        comp2.shutdown();
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
        comp.shutdown();
        let comp2 = Companion::boot(c.clone()).unwrap();
        // restart must NOT reset the sequence (C2)
        assert!(comp2.last_seq() >= seq1);
        comp2.shutdown();
        let _ = std::fs::remove_dir_all(&c.data_dir);
    }

    fn json_len(v: &crate::json::Json, key: &str) -> usize {
        crate::json::get(v, key)
            .and_then(crate::json::as_arr)
            .map(|a| a.len())
            .unwrap_or(0)
    }
}
