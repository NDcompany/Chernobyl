//! Mock agent engine — exact port of `packages/protocol/src/engine.ts`.
//! The execution + permission authority; everything simulated, no shell.
//!
//! Timing model: the TS engine uses setTimeout/setInterval; here the server
//! drives a scheduler thread calling `pump(now)` every 50ms. Tests pass a
//! far-future `now` to fast-forward deterministically.

use crate::protocol::{
    ActivityEvent, ClientMsg, DecisionError, Envelope, EventKind, FEATURES, FileChange,
    LIVE_PERMISSION_TTL_MS, OutputLine, PermissionOption, PermissionRequest, ServerMsg, Session,
    SessionStep, Snapshot, evaluate_permission_decision, legacy_decisions, next_id, now_ms,
};
use std::collections::{HashMap, HashSet, VecDeque};

pub const TICK_MS: i64 = 1_000;
pub const STEP_MS: i64 = 450;
const MAX_ACK_CACHE: usize = 500;
const MAX_OUTPUT_LINES: usize = 300;
const MAX_ACTIVITY: usize = 60;

enum PlanStep {
    Line {
        line: String,
        stream: String,
        files: Vec<FileChange>,
    },
    Ask(AskSpec),
}

#[derive(Clone)]
struct AskSpec {
    action: String,
    title: String,
    command: String,
    args: Vec<String>,
    directory: Option<String>,
    risk: String,
    context: String,
}

struct RunState {
    session_id: String,
    plan: Vec<PlanStep>,
    cursor: usize,
    /// Absolute ms deadline for the next step (None = paused awaiting decision).
    due_at: Option<i64>,
    awaiting: Option<String>,
    command_id: Option<String>,
}

pub struct Engine {
    state: Snapshot,
    runs: HashMap<String, RunState>,
    acks: HashMap<String, ServerMsg>,
    ack_order: VecDeque<String>,
    grants: HashSet<String>,
    seq: i64,
    emit: Box<dyn Fn(ServerMsg) + Send + Sync>,
}

impl Engine {
    /// `start_seq` continues the event sequence from the durable journal
    /// (C2: seq never restarts at 0 after a reboot).
    pub fn new(start_seq: i64, emit: Box<dyn Fn(ServerMsg) + Send + Sync>) -> Engine {
        Engine {
            state: crate::seed::create_seed_snapshot(now_ms()),
            runs: HashMap::new(),
            acks: HashMap::new(),
            ack_order: VecDeque::new(),
            grants: HashSet::new(),
            seq: start_seq,
            emit,
        }
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.state
    }

    /// ADR-009: no enabled mock adapter → no demo data. Drops the seed so the
    /// snapshot only ever carries what real adapters add.
    pub fn drop_seed(&mut self) {
        self.state = Snapshot::default();
    }

    pub fn last_seq(&self) -> i64 {
        self.seq
    }

    /// Phase 3: an event originating on a native adapter stream enters the
    /// SAME seq/envelope/emit path as engine events, so one sequence stays
    /// continuous (C2) across mock and native sources.
    pub fn publish_external(&mut self, kind: EventKind) {
        match &kind {
            EventKind::Activity(a) => {
                // the snapshot must show what the event stream already says
                self.state.activity.insert(0, a.clone());
                if self.state.activity.len() > MAX_ACTIVITY {
                    self.state.activity.pop();
                }
            }
            // ADR-015: a native resolution (answered in the OpenCode TUI)
            // resolves the engine's G2 record. Broadcasting the adapter's
            // copy, with its own revision, left the record pending.
            EventKind::PermissionResolved(p) => {
                self.resolve_native(&p.id, &p.status, p.decision.as_deref());
                return;
            }
            _ => {}
        }
        self.broadcast(kind);
    }

    /// Phase 3: adapter-sourced permission request. The engine remains the
    /// G2 authority — the native record becomes an ENGINE permission record
    /// (engine revision/expiry policy applies) before anything can decide
    /// it; never a bypass around revision/pending/expiry validation.
    /// ADR-015: with options, `supported_decisions` becomes exactly the
    /// legacy buckets (an old phone never maps a tap to a persistent rule),
    /// and the expiry is the 24 h runtime TTL.
    pub fn ingest_permission(&mut self, mut pr: PermissionRequest) {
        if !pr.options.is_empty() {
            pr.supported_decisions = legacy_decisions(&pr.options);
        }
        pr.expires_at = now_ms() + LIVE_PERMISSION_TTL_MS;
        self.state.permissions.insert(0, pr.clone());
        let agent_name = self
            .find_session(&pr.session_id)
            .and_then(|s| self.find_agent(&s.agent_id).map(|a| a.name.clone()))
            .unwrap_or_else(|| "Agent".into());
        self.broadcast(EventKind::PermissionRequested(pr.clone()));
        self.push_activity(
            "permission",
            &format!("{agent_name}: requested \"{}\" permission", pr.command),
            Some(&pr.id),
        );
    }

    /// Phase 3: G2 pre-validation for a decision the NATIVE runtime will
    /// execute (`routes_to_adapter`); `decide` runs the same checks for mock
    /// records: unknown, native metadata, pending, expiry, revision, then the
    /// ADR-015 option bucket, `message` and `answers` — the authority never
    /// moves with the executor. Returns the selected option (None for a
    /// request without options): the adapter writes THAT option's answer.
    pub fn validate_native_permission_decision(
        &mut self,
        permission_id: &str,
        decision: &str,
        option_id: Option<&str>,
        message: Option<&str>,
        answers: &[(String, Vec<String>)],
        revision: f64,
    ) -> Result<Option<PermissionOption>, String> {
        let Some(perm) = self.find_permission(permission_id).cloned() else {
            return Err("unknown permission request".into());
        };
        match evaluate_permission_decision(
            &perm,
            decision,
            option_id,
            message,
            answers,
            revision,
            now_ms(),
        ) {
            Ok(option) => Ok(option.cloned()),
            Err(code) => {
                if code == DecisionError::Expired {
                    let mut p = perm.clone();
                    self.expire(&mut p);
                    if let Some(target) = self.find_permission_mut(permission_id) {
                        *target = p;
                    }
                }
                Err(code.message().to_string())
            }
        }
    }

    /// Phase 3: the native runtime ACCEPTED a decision this engine already
    /// validated — fold the outcome into the G2 record (revision bump,
    /// status, grant) and emit the same terminal events as `decide`.
    /// Session-side branches are deliberately omitted: native sessions are
    /// not engine runs, their flow arrives on the event stream instead.
    pub fn complete_native_decision(
        &mut self,
        command_id: &str,
        permission_id: &str,
        decision: &str,
    ) {
        let Some(perm) = self.find_permission(permission_id).cloned() else {
            return; // expired/evicted between validate and complete: honest no-op
        };
        let now = now_ms();
        let agent_name = self
            .find_agent(&perm.agent_id)
            .map(|a| a.name.clone())
            .unwrap_or_else(|| "Agent".into());
        self.progress(command_id, "accepted", Some(&perm.session_id));
        // resolution invalidates any older decision (G2)
        if let Some(p) = self.find_permission_mut(permission_id) {
            p.revision += 1;
        }
        if decision == "reject" {
            if let Some(p) = self.find_permission_mut(permission_id) {
                p.status = "rejected".into();
                p.decided_at = Some(now);
                p.decision = Some("reject".into());
            }
            let p = self.find_permission(permission_id).cloned().unwrap();
            self.broadcast(EventKind::PermissionResolved(p.clone()));
            self.push_activity(
                "permission",
                &format!("{agent_name}: rejected “{}”", p.command),
                Some(&p.id),
            );
        } else {
            if let Some(p) = self.find_permission_mut(permission_id) {
                p.status = "approved".into();
                p.decided_at = Some(now);
                p.decision = Some(decision.to_string());
            }
            if decision == "approve-session" {
                self.grants
                    .insert(format!("{}::{}", perm.session_id, perm.command));
            }
            let p = self.find_permission(permission_id).cloned().unwrap();
            self.broadcast(EventKind::PermissionResolved(p.clone()));
            self.push_activity(
                "permission",
                if decision == "approve-once" {
                    format!("{agent_name}: approved “{}” once", p.command)
                } else {
                    format!("{agent_name}: approved “{}” for this session", p.command)
                }
                .as_str(),
                Some(&p.id),
            );
        }
        self.result(
            command_id,
            "completed",
            None,
            Some(&perm.session_id),
            Some(&perm.id),
        );
    }

    /// ADR-015: a resolution that happened natively — the runtime withdrew
    /// the request, it was answered in another UI, or the session ended.
    /// Bumps the revision so a late phone decision fails closed ("request is
    /// no longer pending"), stores the outcome and broadcasts
    /// `permission_resolved` once. Idempotent: an unknown or already
    /// resolved record is left alone. Only a well-formed approval or a
    /// rejection is recorded as such; anything else is `stale` (resolved,
    /// no outcome claimed).
    pub fn resolve_native(&mut self, permission_id: &str, status: &str, decision: Option<&str>) {
        let (status, decision) = match (status, decision) {
            ("approved", Some(d @ ("approve-once" | "approve-session"))) => ("approved", Some(d)),
            ("rejected", _) => ("rejected", Some("reject")),
            _ => ("stale", None),
        };
        let now = now_ms();
        let Some(p) = self
            .find_permission_mut(permission_id)
            .filter(|p| p.status == "pending")
        else {
            return;
        };
        p.revision += 1;
        p.status = status.to_string();
        p.decided_at = Some(now);
        p.decision = decision.map(str::to_string);
        let p = p.clone();
        self.broadcast(EventKind::PermissionResolved(p.clone()));
        let text = match status {
            "stale" => format!("“{}” was withdrawn by the runtime", p.command),
            s => format!("“{}” was {s} in the runtime", p.command),
        };
        self.push_activity("permission", &text, Some(&p.id));
    }

    fn broadcast(&mut self, kind: EventKind) {
        self.seq += 1;
        let msg = ServerMsg::Event {
            kind,
            env: Envelope {
                seq: self.seq,
                event_id: next_id("evt"),
                occurred_at: now_ms(),
            },
        };
        (self.emit)(msg);
    }

    fn progress(&mut self, command_id: &str, state: &str, session_id: Option<&str>) {
        self.broadcast(EventKind::CommandProgress {
            command_id: command_id.to_string(),
            state: state.to_string(),
            session_id: session_id.map(str::to_string),
        });
    }

    fn result(
        &mut self,
        command_id: &str,
        status: &str,
        error: Option<&str>,
        session_id: Option<&str>,
        permission_id: Option<&str>,
    ) {
        self.broadcast(EventKind::CommandResult {
            command_id: command_id.to_string(),
            status: status.to_string(),
            error: error.map(str::to_string),
            session_id: session_id.map(str::to_string),
            permission_id: permission_id.map(str::to_string),
        });
    }

    /* ---------------------------------------------------------------- */
    /* Message handling                                                  */
    /* ---------------------------------------------------------------- */

    pub fn handle(&mut self, msg: ClientMsg) -> Option<ServerMsg> {
        let mutating = msg.is_mutating();
        if let (Some(cmd_id), true) = (msg.command_id(), mutating)
            && let Some(cached) = self.acks.get(cmd_id)
        {
            return Some(ServerMsg::duplicate_of(cached));
        }

        let cmd_id = msg.command_id().map(str::to_string);
        let reply: Option<ServerMsg> = match &msg {
            ClientMsg::Ping { command_id } => Some(ServerMsg::Pong {
                command_id: command_id.clone(),
            }),
            ClientMsg::ListState { .. } => Some(ServerMsg::State {
                snapshot: self.state.clone(),
                last_seq: self.seq,
            }),
            ClientMsg::Hello { protocol, .. } => Some(ServerMsg::HelloAck {
                protocol: *protocol as u32,
                server_time: now_ms(),
                snapshot: self.state.clone(),
                last_seq: self.seq,
                features: FEATURES,
            }),
            ClientMsg::CreateSession { .. } => self.create_session(msg),
            ClientMsg::SubmitPrompt { .. } => self.submit_prompt(msg),
            ClientMsg::Interrupt { .. } => self.interrupt(msg),
            ClientMsg::PermissionDecision { .. } => self.decide(msg),
            // handshake frames are terminated in the server, never the engine
            ClientMsg::HsHello { .. } | ClientMsg::HsProof { .. } => None,
            // ADR-015: the mock runtime streams no transcript. Everything a
            // live runtime does not claim lands here and is refused honestly
            // (parity with the TS MockEngine).
            ClientMsg::SessionSubscribe { command_id, .. }
            | ClientMsg::SessionUnsubscribe { command_id, .. }
            | ClientMsg::SessionEnd { command_id, .. }
            | ClientMsg::SetPermissionMode { command_id, .. }
            | ClientMsg::McpAuth { command_id, .. }
            | ClientMsg::McpAuthCallback { command_id, .. }
            | ClientMsg::McpReconnect { command_id, .. } => Some(ServerMsg::ack_err(
                command_id,
                "not supported by this agent",
            )),
        };

        if let (Some(ServerMsg::Ack { .. }), Some(cmd_id), true) = (&reply, &cmd_id, mutating)
            && let Some(reply) = &reply
        {
            self.cache_ack(cmd_id.clone(), reply.clone());
        }
        reply
    }

    fn cache_ack(&mut self, cmd_id: String, reply: ServerMsg) {
        if self.acks.contains_key(&cmd_id) {
            return;
        }
        self.acks.insert(cmd_id.clone(), reply);
        self.ack_order.push_back(cmd_id);
        while self.acks.len() > MAX_ACK_CACHE {
            if let Some(oldest) = self.ack_order.pop_front() {
                self.acks.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /* ---------------------------------------------------------------- */
    /* Commands                                                          */
    /* ---------------------------------------------------------------- */

    fn create_session(&mut self, msg: ClientMsg) -> Option<ServerMsg> {
        let ClientMsg::CreateSession {
            command_id,
            agent_id,
            title,
            prompt,
            project,
        } = msg
        else {
            return None;
        };
        let Some(agent) = self.find_agent(&agent_id).cloned() else {
            return Some(ServerMsg::ack_err(&command_id, "unknown agent"));
        };
        if !agent.accepts_new_session {
            return Some(ServerMsg::ack_err(
                &command_id,
                &format!("{} is not accepting new sessions", agent.name),
            ));
        }
        let now = now_ms();
        // TS: title.trim().slice(0, 90) || 'Untitled task'
        let trimmed = title.trim();
        let sliced: String = trimmed.chars().take(90).collect();
        let session = Session {
            id: next_id("s"),
            agent_id: agent.id.clone(),
            host_id: agent.host_id.clone(),
            title: if sliced.is_empty() {
                "Untitled task".into()
            } else {
                sliced
            },
            project: project
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .unwrap_or("workspace")
                .to_string(),
            status: "running".into(),
            progress: 0,
            created_at: now,
            last_activity_at: now,
            prompt: Some(prompt.clone()),
            timeline: vec![
                step("t1", "Accept task", "done"),
                step("t2", "Plan steps", "active"),
                step("t3", "Apply changes", "pending"),
                step("t4", "Verify", "pending"),
            ],
            output: vec![],
            files: vec![],
            transcript: false,
        };
        self.state.sessions.insert(0, session.clone());
        self.broadcast(EventKind::SessionCreated(session.clone()));
        self.push_activity(
            "session",
            &format!("{} started “{}”", agent.name, session.title),
            Some(&session.id),
        );
        self.refresh_agent(&agent.id);
        self.start_run(&session, None);
        self.progress(&command_id, "accepted", Some(&session.id));
        self.result(&command_id, "completed", None, Some(&session.id), None);
        Some(ServerMsg::ack_ok(&command_id, Some(&session.id)))
    }

    fn submit_prompt(&mut self, msg: ClientMsg) -> Option<ServerMsg> {
        let ClientMsg::SubmitPrompt {
            command_id,
            session_id,
            text,
        } = msg
        else {
            return None;
        };
        let Some(session) = self.find_session(&session_id).cloned() else {
            return Some(ServerMsg::ack_err(&command_id, "unknown session"));
        };
        if let Some(run) = self.runs.get(&session_id)
            && run.awaiting.is_none()
        {
            return Some(ServerMsg::ack_err(
                &command_id,
                "session is already running",
            ));
        }
        if session.status == "waiting" {
            return Some(ServerMsg::ack_err(
                &command_id,
                "session is waiting for a permission decision",
            ));
        }
        if let Some(s) = self.find_session_mut(&session_id) {
            s.prompt = Some(text.clone());
            s.status = "running".into();
            s.last_activity_at = now_ms();
        }
        let line = self.push_output(&session_id, &format!("» {text}"), "stdout");
        let _ = line;
        self.broadcast(EventKind::SessionUpdated(
            self.find_session(&session_id).cloned().unwrap(),
        ));
        self.start_run(&session, Some(&command_id));
        self.progress(&command_id, "accepted", Some(&session_id));
        self.progress(&command_id, "running", Some(&session_id));
        Some(ServerMsg::ack_ok(&command_id, Some(&session_id)))
    }

    fn interrupt(&mut self, msg: ClientMsg) -> Option<ServerMsg> {
        let ClientMsg::Interrupt {
            command_id,
            session_id,
        } = msg
        else {
            return None;
        };
        let Some(session) = self.find_session(&session_id).cloned() else {
            return Some(ServerMsg::ack_err(&command_id, "unknown session"));
        };
        self.progress(&command_id, "accepted", Some(&session_id));
        let active = session.status == "running" || session.status == "waiting";
        if !active {
            // idempotent: interrupting an idle session is not an error
            self.result(&command_id, "completed", None, Some(&session_id), None);
            return Some(ServerMsg::ack_ok(&command_id, None));
        }
        self.stop_run(&session_id, "cancelled");
        if let Some(s) = self.find_session_mut(&session_id) {
            s.status = "interrupted".into();
            s.last_activity_at = now_ms();
        }
        let pending = self
            .state
            .permissions
            .iter()
            .find(|p| p.session_id == session_id && p.status == "pending")
            .map(|p| p.id.clone());
        if let Some(pid) = pending
            && let Some(p) = self.find_permission_mut(&pid)
        {
            let mut p = p.clone();
            self.expire(&mut p);
            if let Some(target) = self.find_permission_mut(&pid) {
                *target = p;
            }
        }
        self.push_output(&session_id, "Interrupted by user", "system");
        self.broadcast(EventKind::SessionUpdated(
            self.find_session(&session_id).cloned().unwrap(),
        ));
        self.push_activity(
            "session",
            &format!("Interrupted “{}”", session.title),
            Some(&session_id),
        );
        self.refresh_agent(&session.agent_id);
        self.result(&command_id, "completed", None, Some(&session_id), None);
        Some(ServerMsg::ack_ok(&command_id, None))
    }

    fn decide(&mut self, msg: ClientMsg) -> Option<ServerMsg> {
        let ClientMsg::PermissionDecision {
            command_id,
            permission_id,
            decision,
            revision,
            option_id,
            message,
            answers,
        } = msg
        else {
            return None;
        };
        // Fail-closed authority, the same checks a native decision gets:
        // native metadata, pending status, expiry, revision, option bucket,
        // message and answers (G2). Never translated.
        if let Err(e) = self.validate_native_permission_decision(
            &permission_id,
            &decision,
            option_id.as_deref(),
            message.as_deref(),
            &answers,
            revision,
        ) {
            return Some(ServerMsg::ack_err(&command_id, &e));
        }
        let perm = self.find_permission(&permission_id).cloned()?;

        let now = now_ms();
        let session = self.find_session(&perm.session_id).cloned();
        let agent_name = self
            .find_agent(&perm.agent_id)
            .map(|a| a.name.clone())
            .unwrap_or_else(|| "Agent".into());
        self.progress(&command_id, "accepted", Some(&perm.session_id));
        // resolution invalidates any older decision (G2)
        if let Some(p) = self.find_permission_mut(&permission_id) {
            p.revision += 1;
        }

        if decision == "reject" {
            if let Some(p) = self.find_permission_mut(&permission_id) {
                p.status = "rejected".into();
                p.decided_at = Some(now);
                p.decision = Some("reject".into());
            }
            let p = self.find_permission(&permission_id).cloned().unwrap();
            self.broadcast(EventKind::PermissionResolved(p.clone()));
            self.push_activity(
                "permission",
                &format!("{agent_name}: rejected “{}”", p.command),
                Some(&p.id),
            );
            if let Some(session) = session {
                self.stop_run(&session.id, "cancelled");
                if let Some(s) = self.find_session_mut(&session.id) {
                    s.status = "interrupted".into();
                    s.last_activity_at = now;
                }
                self.push_output(
                    &session.id,
                    &format!("Rejected: {} — {} was not executed", p.title, p.command),
                    "system",
                );
                self.broadcast(EventKind::SessionUpdated(
                    self.find_session(&session.id).cloned().unwrap(),
                ));
                self.refresh_agent(&session.agent_id);
            }
        } else {
            if let Some(p) = self.find_permission_mut(&permission_id) {
                p.status = "approved".into();
                p.decided_at = Some(now);
                p.decision = Some(decision.clone());
            }
            if decision == "approve-session" {
                self.grants
                    .insert(format!("{}::{}", perm.session_id, perm.command));
            }
            let p = self.find_permission(&permission_id).cloned().unwrap();
            self.broadcast(EventKind::PermissionResolved(p.clone()));
            self.push_activity(
                "permission",
                if decision == "approve-once" {
                    format!("{agent_name}: approved “{}” once", p.command)
                } else {
                    format!("{agent_name}: approved “{}” for this session", p.command)
                }
                .as_str(),
                Some(&p.id),
            );
            if let Some(session) = session {
                if let Some(s) = self.find_session_mut(&session.id) {
                    s.status = "running".into();
                    s.last_activity_at = now;
                }
                self.push_output(
                    &session.id,
                    &format!("Approved ({decision}): {}", p.command),
                    "system",
                );
                self.broadcast(EventKind::SessionUpdated(
                    self.find_session(&session.id).cloned().unwrap(),
                ));
                self.resume(&session.id);
            }
        }
        self.result(
            &command_id,
            "completed",
            None,
            Some(&perm.session_id),
            Some(&perm.id),
        );
        Some(ServerMsg::Ack {
            command_id,
            ok: true,
            error: None,
            duplicate: false,
            session_id: None,
            permission_id: Some(permission_id),
        })
    }

    /* ---------------------------------------------------------------- */
    /* Run loop                                                          */
    /* ---------------------------------------------------------------- */

    fn start_run(&mut self, session: &Session, command_id: Option<&str>) {
        self.runs.insert(
            session.id.clone(),
            RunState {
                session_id: session.id.clone(),
                plan: build_plan(session),
                cursor: 0,
                due_at: Some(now_ms() + STEP_MS),
                awaiting: None,
                command_id: command_id.map(str::to_string),
            },
        );
    }

    /// Continue a run after a permission decision (recreates it if needed).
    fn resume(&mut self, session_id: &str) {
        if let Some(run) = self.runs.get_mut(session_id) {
            run.awaiting = None;
            run.due_at = Some(now_ms() + 350);
            return;
        }
        let Some(session) = self.find_session(session_id).cloned() else {
            return;
        };
        self.start_run(&session, None);
    }

    /// Advances every run whose deadline is due. Called by the scheduler
    /// thread (and tests, with a fast-forwarded `now`).
    pub fn pump(&mut self, now: i64) {
        self.tick(now);
        // collect due session ids first (step() needs &mut self)
        let mut due: Vec<String> = self
            .runs
            .values()
            .filter(|r| r.due_at.is_some_and(|d| now >= d))
            .map(|r| r.session_id.clone())
            .collect();
        due.sort();
        for id in due {
            self.step(&id, now);
        }
    }

    fn step(&mut self, session_id: &str, now: i64) {
        enum First {
            Done,
            Step(PlanStep, usize, usize),
        }
        // Phase 1: inspect the run under a short borrow, then release it.
        let first = {
            let Some(run) = self.runs.get_mut(session_id) else {
                return;
            };
            run.due_at = None;
            if run.cursor >= run.plan.len() {
                First::Done
            } else {
                let step = std::mem::replace(
                    &mut run.plan[run.cursor],
                    PlanStep::Line {
                        line: String::new(),
                        stream: "stdout".into(),
                        files: vec![],
                    },
                );
                run.cursor += 1;
                First::Step(step, run.cursor, run.plan.len())
            }
        };
        let (plan_step, cursor, plan_len) = match first {
            First::Done => {
                let session = self.find_session(session_id).cloned();
                if let Some(session) = session {
                    self.finish(&session, now);
                } else {
                    self.runs.remove(session_id);
                }
                return;
            }
            First::Step(step, cursor, len) => (step, cursor, len),
        };

        match plan_step {
            PlanStep::Ask(ask) => {
                let scope_key = format!("{session_id}::{}", ask.command);
                if self.grants.contains(&scope_key) {
                    self.push_output(
                        session_id,
                        &format!("Auto-approved for this session: {}", ask.command),
                        "system",
                    );
                    self.schedule(session_id, 300);
                    return;
                }
                let permission = self.issue_permission(session_id, &ask, now);
                if let Some(run) = self.runs.get_mut(session_id) {
                    run.awaiting = Some(permission.id.clone());
                }
                if let Some(s) = self.find_session_mut(session_id) {
                    s.status = "waiting".into();
                    s.last_activity_at = permission.requested_at;
                }
                self.broadcast(EventKind::SessionUpdated(
                    self.find_session(session_id).cloned().unwrap(),
                ));
                self.broadcast(EventKind::PermissionRequested(permission.clone()));
                let agent_name = self
                    .find_session(session_id)
                    .and_then(|s| self.find_agent(&s.agent_id).map(|a| a.name.clone()))
                    .unwrap_or_else(|| "Agent".into());
                self.push_activity(
                    "permission",
                    &format!("{agent_name} requested to run “{}”", ask.command),
                    Some(&permission.id),
                );
                if let Some(s) = self.find_session(session_id) {
                    let agent_id = s.agent_id.clone();
                    self.refresh_agent(&agent_id);
                }
            }
            PlanStep::Line {
                line,
                stream,
                files,
            } => {
                let output = self.push_output(session_id, &line, &stream);
                let progress = plan_progress(cursor, plan_len);
                let status = self
                    .find_session(session_id)
                    .map(|s| s.status.clone())
                    .unwrap_or_default();
                self.broadcast(EventKind::SessionOutput {
                    session_id: session_id.to_string(),
                    lines: vec![output],
                    progress,
                    status,
                });
                if !files.is_empty() {
                    if let Some(s) = self.find_session_mut(session_id) {
                        s.files.extend(files);
                    }
                    self.broadcast(EventKind::SessionUpdated(
                        self.find_session(session_id).cloned().unwrap(),
                    ));
                }
                self.schedule(session_id, STEP_MS);
            }
        }
    }

    fn schedule(&mut self, session_id: &str, delay: i64) {
        if let Some(run) = self.runs.get_mut(session_id) {
            run.due_at = Some(now_ms() + delay);
        }
    }

    fn finish(&mut self, session: &Session, now: i64) {
        let run = self.runs.remove(&session.id);
        if let Some(s) = self.find_session_mut(&session.id) {
            s.status = "completed".into();
            s.progress = 100;
            s.last_activity_at = now;
            for t in &mut s.timeline {
                t.status = "done".into();
            }
        }
        self.push_output(&session.id, "Session finished", "system");
        self.broadcast(EventKind::SessionUpdated(
            self.find_session(&session.id).cloned().unwrap(),
        ));
        self.push_activity(
            "session",
            &format!("Finished “{}”", session.title),
            Some(&session.id),
        );
        self.refresh_agent(&session.agent_id);
        // the prompt command that started this run is done (G1: one terminal)
        if let Some(run) = run
            && let Some(cmd_id) = run.command_id
        {
            self.result(&cmd_id, "completed", None, Some(&session.id), None);
        }
    }

    /// Stops a run and emits the terminal outcome of the client command that
    /// started it (G1). `expired` covers decisions that lapsed mid-run.
    fn stop_run(&mut self, session_id: &str, terminal: &str) {
        let Some(run) = self.runs.remove(session_id) else {
            return;
        };
        if let Some(cmd_id) = run.command_id {
            self.result(&cmd_id, terminal, None, Some(session_id), None);
        }
    }

    /* ---------------------------------------------------------------- */
    /* Permission lifecycle                                              */
    /* ---------------------------------------------------------------- */

    fn issue_permission(
        &mut self,
        session_id: &str,
        spec: &AskSpec,
        now: i64,
    ) -> PermissionRequest {
        let session = self.find_session(session_id).cloned().unwrap();
        let id = next_id("p");
        let permission = PermissionRequest {
            id: id.clone(),
            // the mock runtime is its own native authority: its internal id IS
            // the native request id; supported decisions are what it offers
            native_request_id: id,
            session_id: session.id,
            agent_id: session.agent_id,
            host_id: session.host_id,
            action: spec.action.clone(),
            title: spec.title.clone(),
            command: spec.command.clone(),
            args: spec.args.clone(),
            directory: spec.directory.clone(),
            risk: spec.risk.clone(),
            status: "pending".into(),
            supported_decisions: vec![
                "approve-once".into(),
                "approve-session".into(),
                "reject".into(),
            ],
            revision: 1,
            fingerprint: Some(format!("mock:{}:{}", spec.action, spec.command)),
            requested_at: now,
            expires_at: now + crate::protocol::PERMISSION_TTL_MS,
            context: spec.context.clone(),
            decided_at: None,
            decision: None,
            tool_item_id: None,
            options: vec![],
            detail: None,
        };
        self.state.permissions.insert(0, permission.clone());
        permission
    }

    /// Expiry sweep (G1/G2) — parity with TS tick().
    fn tick(&mut self, now: i64) {
        let expired: Vec<String> = self
            .state
            .permissions
            .iter()
            .filter(|p| p.status == "pending" && now >= p.expires_at)
            .map(|p| p.id.clone())
            .collect();
        for id in expired {
            if let Some(p) = self.find_permission_mut(&id) {
                let mut clone = p.clone();
                self.expire(&mut clone);
                if let Some(target) = self.find_permission_mut(&id) {
                    *target = clone;
                }
            }
        }
    }

    /// Public sweep entry — parity with the TS engine's tick().
    pub fn tick_public(&mut self) {
        self.tick(now_ms());
    }

    fn expire(&mut self, p: &mut PermissionRequest) {
        if p.status != "pending" {
            return;
        }
        let now = now_ms();
        p.status = "stale".into();
        p.decided_at = Some(now);
        p.revision += 1; // expiry is a resolution: older decisions die with it (G2)
        self.broadcast(EventKind::PermissionResolved(p.clone()));
        self.push_activity(
            "permission",
            &format!("“{}” expired without a decision", p.command),
            Some(&p.id),
        );
        let session_id = p.session_id.clone();
        let waiting = self
            .find_session(&session_id)
            .map(|s| s.status == "waiting")
            .unwrap_or(false);
        if waiting {
            // the blocked prompt's command gets its `expired` terminal (G1)
            self.stop_run(&session_id, "expired");
            if let Some(s) = self.find_session_mut(&session_id) {
                s.status = "idle".into();
                s.last_activity_at = now;
            }
            self.push_output(
                &session_id,
                "Permission request expired — task paused",
                "system",
            );
            self.broadcast(EventKind::SessionUpdated(
                self.find_session(&session_id).cloned().unwrap(),
            ));
            if let Some(s) = self.find_session(&session_id) {
                let agent_id = s.agent_id.clone();
                self.refresh_agent(&agent_id);
            }
        }
    }

    /* ---------------------------------------------------------------- */
    /* Helpers                                                           */
    /* ---------------------------------------------------------------- */

    fn find_session(&self, id: &str) -> Option<&Session> {
        self.state.sessions.iter().find(|s| s.id == id)
    }

    fn find_session_mut(&mut self, id: &str) -> Option<&mut Session> {
        self.state.sessions.iter_mut().find(|s| s.id == id)
    }

    fn find_agent(&self, id: &str) -> Option<&crate::protocol::Agent> {
        self.state.agents.iter().find(|a| a.id == id)
    }

    fn find_permission(&self, id: &str) -> Option<&PermissionRequest> {
        self.state.permissions.iter().find(|p| p.id == id)
    }

    fn find_permission_mut(&mut self, id: &str) -> Option<&mut PermissionRequest> {
        self.state.permissions.iter_mut().find(|p| p.id == id)
    }

    fn refresh_agent(&mut self, agent_id: &str) {
        let Some(agent) = self.find_agent(agent_id) else {
            return;
        };
        if agent.state == "offline" {
            return;
        }
        let active =
            self.state.sessions.iter().any(|s| {
                s.agent_id == agent_id && (s.status == "running" || s.status == "waiting")
            });
        let accepts = agent.accepts_new_session;
        let current = agent.state.clone();
        let next = if active {
            "busy"
        } else if accepts {
            "online"
        } else {
            "idle"
        };
        if next != current {
            if let Some(a) = self.state.agents.iter_mut().find(|a| a.id == agent_id) {
                a.state = next.to_string();
            }
            self.broadcast(EventKind::AgentUpdated(
                self.find_agent(agent_id).cloned().unwrap(),
            ));
        }
    }

    fn push_output(&mut self, session_id: &str, text: &str, stream: &str) -> OutputLine {
        let line = OutputLine {
            id: next_id("o"),
            ts: now_ms(),
            stream: stream.to_string(),
            text: text.to_string(),
        };
        if let Some(s) = self.find_session_mut(session_id) {
            s.output.push(line.clone());
            if s.output.len() > MAX_OUTPUT_LINES {
                s.output.remove(0);
            }
            s.last_activity_at = line.ts;
        }
        line
    }

    fn push_activity(&mut self, kind: &str, text: &str, ref_id: Option<&str>) {
        let event = ActivityEvent {
            id: next_id("ev"),
            ts: now_ms(),
            kind: kind.to_string(),
            text: text.to_string(),
            ref_id: ref_id.map(str::to_string),
        };
        self.state.activity.insert(0, event.clone());
        if self.state.activity.len() > MAX_ACTIVITY {
            self.state.activity.pop();
        }
        self.broadcast(EventKind::Activity(event));
    }
}

fn step(id: &str, label: &str, status: &str) -> SessionStep {
    SessionStep {
        id: id.into(),
        label: label.into(),
        status: status.into(),
    }
}

fn file(path: &str, additions: i64, deletions: i64, status: &str) -> FileChange {
    FileChange {
        path: path.into(),
        additions,
        deletions,
        status: status.into(),
    }
}

fn build_plan(session: &Session) -> Vec<PlanStep> {
    vec![
        PlanStep::Line {
            line: format!("Working on “{}”", session.title),
            stream: "stdout".into(),
            files: vec![],
        },
        PlanStep::Line {
            line: "Reading context and gathering files...".into(),
            stream: "stdout".into(),
            files: vec![],
        },
        PlanStep::Ask(AskSpec {
            action: "shell".into(),
            title: "Run shell command".into(),
            command: "npm run check".into(),
            args: vec![],
            directory: Some(session.project.clone()),
            risk: "medium".into(),
            context:
                "Verification step: type check and unit tests before the task is reported done."
                    .into(),
        }),
        PlanStep::Line {
            line: "Applying changes to 3 files".into(),
            stream: "stdout".into(),
            files: vec![
                file("tests/orders.api.test.ts", 24, 11, "modified"),
                file("tests/helpers/clock.ts", 38, 0, "added"),
                file("src/orders/checkout.ts", 6, 4, "modified"),
            ],
        },
        PlanStep::Line {
            line: "npm run check → 0 errors".into(),
            stream: "stdout".into(),
            files: vec![],
        },
        PlanStep::Line {
            line: "Done. Verification clean.".into(),
            stream: "stdout".into(),
            files: vec![],
        },
    ]
}

fn plan_progress(cursor: usize, plan_len: usize) -> i64 {
    if plan_len == 0 {
        return 0;
    }
    let raw = (cursor as f64 / plan_len as f64) * 100.0;
    99.min(raw.round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn make_engine() -> (Engine, Arc<Mutex<Vec<ServerMsg>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let engine = Engine::new(0, Box::new(move |m| sink.lock().unwrap().push(m)));
        (engine, events)
    }

    #[allow(clippy::type_complexity)] // test helper: (commandId, ok, sessionId, duplicate, error)
    fn ack_of(
        msg: Option<ServerMsg>,
    ) -> Option<(String, bool, Option<String>, bool, Option<String>)> {
        match msg {
            Some(ServerMsg::Ack {
                command_id,
                ok,
                error,
                duplicate,
                session_id,
                ..
            }) => Some((command_id, ok, session_id, duplicate, error)),
            _ => None,
        }
    }

    fn results(events: &Arc<Mutex<Vec<ServerMsg>>>, command_id: &str) -> Vec<String> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                ServerMsg::Event {
                    kind:
                        EventKind::CommandResult {
                            command_id: cid,
                            status,
                            ..
                        },
                    ..
                } if cid == command_id => Some(status.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    #[ignore = "micro-benchmark — run: cargo test --release microbench -- --ignored --nocapture"]
    fn microbench_engine_stale_decide() {
        const N: usize = 1_000_000;
        let (mut engine, events) = make_engine();
        let permission = engine
            .snapshot()
            .permissions
            .iter()
            .find(|p| p.status == "pending")
            .cloned()
            .expect("seeded pending permission");
        let first = ack_of(engine.handle(ClientMsg::PermissionDecision {
            command_id: "mb-0".into(),
            permission_id: permission.id.clone(),
            decision: "reject".into(),
            revision: (permission.revision + 1) as f64,
            option_id: None,
            message: None,
            answers: vec![],
        }));
        assert_eq!(
            first.as_ref().map(|a| a.1),
            Some(false),
            "stale must refuse"
        );
        let t0 = std::time::Instant::now();
        for i in 1..N {
            let _ = engine.handle(ClientMsg::PermissionDecision {
                command_id: format!("mb-{i}"),
                permission_id: permission.id.clone(),
                decision: "reject".into(),
                revision: (permission.revision + 1) as f64,
                option_id: None,
                message: None,
                answers: vec![],
            });
        }
        let dt = t0.elapsed();
        // stale refusals never emit events, so the sink stays empty
        assert_eq!(events.lock().unwrap().len(), 0);
        println!(
            "engine stale decide (validate+find+refuse+ack-cache, unique ids): {} ns/op ({} ops in {:?})",
            dt.as_nanos() / N as u128,
            N,
            dt
        );
    }

    #[test]
    #[ignore = "micro-benchmark — run: cargo test --release microbench -- --ignored --nocapture"]
    fn microbench_parse_validate_handle_encode() {
        // the whole in-process command path minus sockets and storage — the
        // symmetric counterpart of `engine-bench.mjs full` on the TS side
        const N: usize = 1_000_000;
        let (mut engine, events) = make_engine();
        let permission = engine
            .snapshot()
            .permissions
            .iter()
            .find(|p| p.status == "pending")
            .cloned()
            .expect("seeded pending permission");
        let pid = permission.id.clone();
        let rev = permission.revision + 1; // stale → refused every time
        // prebuild raw frames so the timed loop measures the server path only
        let raws: Vec<String> = (0..N)
            .map(|i| {
                format!(
                    r#"{{"type":"permission_decision","commandId":"mb-{i}","permissionId":"{pid}","decision":"reject","revision":{rev}}}"#
                )
            })
            .collect();
        let first_msg = crate::protocol::parse_client_message(&raws[0]).expect("parses+validates");
        let first_ack = engine.handle(first_msg).expect("ack");
        assert!(crate::json::stringify(&first_ack.to_json()).contains("\"ok\":false"));
        let t0 = std::time::Instant::now();
        for raw in &raws {
            let Some(msg) = crate::protocol::parse_client_message(raw) else {
                panic!("frame must parse+validate");
            };
            let Some(reply) = engine.handle(msg) else {
                panic!("stale decision must ack");
            };
            let _encoded = crate::json::stringify(&reply.to_json());
        }
        let dt = t0.elapsed();
        assert_eq!(events.lock().unwrap().len(), 0);
        println!(
            "rust in-process full path (parse+validate+handle+encode, no socket/storage): {} ns/op ({} ops in {:?})",
            dt.as_nanos() / N as u128,
            N,
            dt
        );
    }

    #[test]
    fn duplicate_command_ids_do_not_create_duplicate_sessions() {
        let (mut engine, _events) = make_engine();
        let before = engine.snapshot().sessions.len();
        let cmd = ClientMsg::CreateSession {
            command_id: "cmd-1".into(),
            agent_id: "a4".into(),
            title: "Duplicate test".into(),
            prompt: "Say hello".into(),
            project: None,
        };
        let first = ack_of(engine.handle(cmd.clone()));
        let second = ack_of(engine.handle(cmd));
        assert_eq!(first.as_ref().map(|a| a.1), Some(true));
        assert_eq!(second.as_ref().map(|a| a.1), Some(true));
        assert_eq!(second.as_ref().map(|a| a.3), Some(true));
        assert_eq!(first.unwrap().2, second.unwrap().2);
        assert_eq!(engine.snapshot().sessions.len(), before + 1);
    }

    #[test]
    fn approving_a_request_resolves_it_and_resumes_the_session() {
        let (mut engine, events) = make_engine();
        let permission = engine
            .snapshot()
            .permissions
            .iter()
            .find(|p| p.status == "pending")
            .cloned()
            .expect("seeded pending permission");
        let reply = ack_of(engine.handle(ClientMsg::PermissionDecision {
            command_id: "d-1".into(),
            permission_id: permission.id.clone(),
            decision: "approve-once".into(),
            revision: permission.revision as f64,
            option_id: None,
            message: None,
            answers: vec![],
        }));
        assert_eq!(reply.as_ref().map(|a| a.1), Some(true));
        let p = engine
            .snapshot()
            .permissions
            .iter()
            .find(|p| p.id == permission.id)
            .unwrap();
        assert_eq!(p.status, "approved");
        assert_eq!(p.decision.as_deref(), Some("approve-once"));
        let session = engine
            .snapshot()
            .sessions
            .iter()
            .find(|s| s.id == permission.session_id)
            .unwrap();
        assert_eq!(session.status, "running");
        assert!(events.lock().unwrap().iter().any(|e| matches!(
            e,
            ServerMsg::Event {
                kind: EventKind::PermissionResolved(_),
                ..
            }
        )));
    }

    #[test]
    fn a_stale_permission_request_can_no_longer_be_decided() {
        let (mut engine, _events) = make_engine();
        let permission = engine
            .snapshot()
            .permissions
            .iter()
            .find(|p| p.status == "pending")
            .cloned()
            .unwrap();
        // fast-forward past its expiry, then sweep
        let future = permission.expires_at + 1;
        engine.pump(future);
        let p = engine
            .snapshot()
            .permissions
            .iter()
            .find(|p| p.id == permission.id)
            .unwrap();
        assert_eq!(p.status, "stale");
        let revision = p.revision;
        let reply = ack_of(engine.handle(ClientMsg::PermissionDecision {
            command_id: "d-2".into(),
            permission_id: permission.id.clone(),
            decision: "approve-once".into(),
            revision: revision as f64,
            option_id: None,
            message: None,
            answers: vec![],
        }));
        assert_eq!(reply.map(|a| a.1), Some(false));
        assert_eq!(
            engine
                .snapshot()
                .permissions
                .iter()
                .find(|p| p.id == permission.id)
                .unwrap()
                .status,
            "stale"
        );
    }

    #[test]
    fn rejecting_a_request_interrupts_its_session() {
        let (mut engine, _events) = make_engine();
        let permission = engine
            .snapshot()
            .permissions
            .iter()
            .find(|p| p.status == "pending" && p.session_id == "s1")
            .cloned()
            .unwrap();
        let reply = ack_of(engine.handle(ClientMsg::PermissionDecision {
            command_id: "d-3".into(),
            permission_id: permission.id.clone(),
            decision: "reject".into(),
            revision: permission.revision as f64,
            option_id: None,
            message: None,
            answers: vec![],
        }));
        assert_eq!(reply.as_ref().map(|a| a.1), Some(true));
        let p = engine
            .snapshot()
            .permissions
            .iter()
            .find(|p| p.id == permission.id)
            .unwrap();
        assert_eq!(p.status, "rejected");
        let session = engine
            .snapshot()
            .sessions
            .iter()
            .find(|s| s.id == "s1")
            .unwrap();
        assert_eq!(session.status, "interrupted");
        assert!(
            session
                .output
                .iter()
                .any(|l| l.stream == "system" && l.text.contains("Rejected"))
        );
    }

    #[test]
    fn a_freshly_created_session_starts_running_its_plan_immediately() {
        let (mut engine, _events) = make_engine();
        let ack = ack_of(engine.handle(ClientMsg::CreateSession {
            command_id: "cmd-run".into(),
            agent_id: "a4".into(),
            title: "Run test".into(),
            prompt: "Do the thing".into(),
            project: None,
        }));
        assert_eq!(ack.as_ref().map(|a| a.1), Some(true));
        let session_id = ack.unwrap().2.unwrap();
        let session = engine
            .snapshot()
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .unwrap();
        assert_eq!(session.status, "running");
        // the plan is already executing — a second prompt is refused, not queued
        let second = ack_of(engine.handle(ClientMsg::SubmitPrompt {
            command_id: "cmd-run-2".into(),
            session_id: session_id.clone(),
            text: "again".into(),
        }));
        assert_eq!(second.as_ref().map(|a| a.1), Some(false));
        assert!(second.unwrap().4.unwrap().contains("already running"));
    }

    #[test]
    fn interrupting_a_waiting_session_expires_its_pending_ask() {
        let (mut engine, _events) = make_engine();
        let waiting = engine
            .snapshot()
            .sessions
            .iter()
            .find(|s| s.status == "waiting")
            .cloned()
            .unwrap();
        let ask = engine
            .snapshot()
            .permissions
            .iter()
            .find(|p| p.session_id == waiting.id && p.status == "pending")
            .cloned()
            .unwrap();
        let ack = ack_of(engine.handle(ClientMsg::Interrupt {
            command_id: "cmd-int".into(),
            session_id: waiting.id.clone(),
        }));
        assert_eq!(ack.as_ref().map(|a| a.1), Some(true));
        assert_eq!(
            engine
                .snapshot()
                .sessions
                .iter()
                .find(|s| s.id == waiting.id)
                .unwrap()
                .status,
            "interrupted"
        );
        assert_eq!(
            engine
                .snapshot()
                .permissions
                .iter()
                .find(|p| p.id == ask.id)
                .unwrap()
                .status,
            "stale"
        );
    }

    #[test]
    fn g1_interrupted_prompt_command_gets_exactly_one_cancelled_terminal() {
        let (mut engine, events) = make_engine();
        let ack = ack_of(engine.handle(ClientMsg::SubmitPrompt {
            command_id: "sub-1".into(),
            session_id: "s4".into(),
            text: "go".into(),
        }));
        assert_eq!(ack.as_ref().map(|a| a.1), Some(true));
        assert!(events.lock().unwrap().iter().any(|e| matches!(
            e,
            ServerMsg::Event {
                kind: EventKind::CommandProgress {
                    command_id,
                    state,
                    ..
                },
                ..
            } if command_id == "sub-1" && state == "running"
        )));
        let int = ack_of(engine.handle(ClientMsg::Interrupt {
            command_id: "int-1".into(),
            session_id: "s4".into(),
        }));
        assert_eq!(int.as_ref().map(|a| a.1), Some(true));
        assert_eq!(results(&events, "sub-1"), vec!["cancelled"]);
        assert_eq!(results(&events, "int-1"), vec!["completed"]);
    }

    #[test]
    fn g1_permission_lapsing_mid_run_yields_an_expired_terminal_not_silence() {
        let (mut engine, events) = make_engine();
        engine.handle(ClientMsg::SubmitPrompt {
            command_id: "sub-2".into(),
            session_id: "s4".into(),
            text: "run".into(),
        });
        // fast-forward: plan reaches the ask at step 3 (~1.35s of steps)
        let deadline = now_ms() + 10_000;
        for _ in 0..200 {
            engine.pump(now_ms() + 5_000);
            let asked = events.lock().unwrap().iter().any(|e| {
                matches!(
                    e,
                    ServerMsg::Event {
                        kind: EventKind::PermissionRequested(_),
                        ..
                    }
                )
            });
            if asked {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            assert!(
                now_ms() < deadline,
                "run never reached its permission checkpoint"
            );
        }
        let permission = {
            let ev = events.lock().unwrap();
            match ev.iter().find_map(|e| match e {
                ServerMsg::Event {
                    kind: EventKind::PermissionRequested(p),
                    ..
                } => Some(p.clone()),
                _ => None,
            }) {
                Some(p) => p,
                None => panic!("no permission_requested event"),
            }
        };
        // lapse it and sweep
        let past = permission.expires_at + 1;
        engine.pump(past);
        assert_eq!(results(&events, "sub-2"), vec!["expired"]);
        assert_eq!(
            engine
                .snapshot()
                .permissions
                .iter()
                .find(|p| p.id == permission.id)
                .unwrap()
                .status,
            "stale"
        );
    }

    #[test]
    fn g1_create_session_completes_at_acceptance() {
        let (mut engine, events) = make_engine();
        engine.handle(ClientMsg::CreateSession {
            command_id: "cc-1".into(),
            agent_id: "a4".into(),
            title: "Lifecycle".into(),
            prompt: "x".into(),
            project: None,
        });
        assert_eq!(results(&events, "cc-1"), vec!["completed"]);
    }

    #[test]
    fn g2_decisions_fail_closed_on_revision_and_unsupported_decisions() {
        let (mut engine, _events) = make_engine();
        let permission = engine
            .snapshot()
            .permissions
            .iter()
            .find(|p| p.status == "pending")
            .cloned()
            .unwrap();
        // wrong revision → refused, request untouched
        let stale = ack_of(engine.handle(ClientMsg::PermissionDecision {
            command_id: "d-rev".into(),
            permission_id: permission.id.clone(),
            decision: "approve-once".into(),
            revision: (permission.revision + 1) as f64,
            option_id: None,
            message: None,
            answers: vec![],
        }));
        assert_eq!(stale.map(|a| a.1), Some(false));
        assert_eq!(
            engine
                .snapshot()
                .permissions
                .iter()
                .find(|p| p.id == permission.id)
                .unwrap()
                .status,
            "pending"
        );
        // ADR-015: an option the request never offered, or a message on an
        // approval, is refused the same way (mock requests carry no options)
        for (option_id, message) in [(Some("allow"), None), (None, Some("why"))] {
            let refused = ack_of(engine.handle(ClientMsg::PermissionDecision {
                command_id: format!("d-opt-{}", option_id.is_some()),
                permission_id: permission.id.clone(),
                decision: "approve-once".into(),
                revision: permission.revision as f64,
                option_id: option_id.map(str::to_string),
                message: message.map(str::to_string),
                answers: vec![],
            }));
            assert_eq!(
                refused.and_then(|a| a.4).as_deref(),
                Some("decision is not supported by this runtime")
            );
        }
        // a decision the runtime does not offer → refused, never translated
        if let Some(p) = engine
            .state
            .permissions
            .iter_mut()
            .find(|p| p.id == permission.id)
        {
            p.supported_decisions = vec!["reject".into()];
        }
        let unsupported = ack_of(engine.handle(ClientMsg::PermissionDecision {
            command_id: "d-sup".into(),
            permission_id: permission.id.clone(),
            decision: "approve-once".into(),
            revision: permission.revision as f64,
            option_id: None,
            message: None,
            answers: vec![],
        }));
        assert_eq!(unsupported.map(|a| a.1), Some(false));
        assert_eq!(
            engine
                .snapshot()
                .permissions
                .iter()
                .find(|p| p.id == permission.id)
                .unwrap()
                .status,
            "pending"
        );
        // the supported decision at the fresh revision applies and bumps it
        let before = engine
            .snapshot()
            .permissions
            .iter()
            .find(|p| p.id == permission.id)
            .unwrap()
            .revision;
        let ok = ack_of(engine.handle(ClientMsg::PermissionDecision {
            command_id: "d-ok".into(),
            permission_id: permission.id.clone(),
            decision: "reject".into(),
            revision: permission.revision as f64,
            option_id: None,
            message: None,
            answers: vec![],
        }));
        assert_eq!(ok.as_ref().map(|a| a.1), Some(true));
        let p = engine
            .snapshot()
            .permissions
            .iter()
            .find(|p| p.id == permission.id)
            .unwrap();
        assert_eq!(p.status, "rejected");
        assert!(p.revision > before);
    }

    #[test]
    fn plan_streams_to_completion_and_auto_approves_with_grant() {
        let (mut engine, events) = make_engine();
        // create session, decide approve-session at the ask, let it finish
        let ack = ack_of(engine.handle(ClientMsg::CreateSession {
            command_id: "run-full".into(),
            agent_id: "a4".into(),
            title: "Full run".into(),
            prompt: "p".into(),
            project: None,
        }));
        let session_id = ack.unwrap().2.unwrap();
        // fast-forward until the ask appears
        for _ in 0..200 {
            engine.pump(now_ms() + 5_000);
            let asked = events.lock().unwrap().iter().any(|e| {
                matches!(
                    e,
                    ServerMsg::Event {
                        kind: EventKind::PermissionRequested(_),
                        ..
                    }
                )
            });
            if asked {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let ask = events
            .lock()
            .unwrap()
            .iter()
            .find_map(|e| match e {
                ServerMsg::Event {
                    kind: EventKind::PermissionRequested(p),
                    ..
                } => Some(p.clone()),
                _ => None,
            })
            .unwrap();
        engine.handle(ClientMsg::PermissionDecision {
            command_id: "dec".into(),
            permission_id: ask.id.clone(),
            decision: "approve-session".into(),
            revision: ask.revision as f64,
            option_id: None,
            message: None,
            answers: vec![],
        });
        // fast-forward to plan completion
        for _ in 0..200 {
            engine.pump(now_ms() + 5_000);
        }
        let session = engine
            .snapshot()
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .unwrap();
        assert_eq!(session.status, "completed");
        assert_eq!(session.progress, 100);
        assert!(session.timeline.iter().all(|t| t.status == "done"));
    }

    /* ---------------- ADR-015 runtime permissions ---------------- */

    fn opt(id: &str, decision: &str, scope: &'static str) -> PermissionOption {
        PermissionOption {
            id: id.into(),
            decision: decision.into(),
            label: id.into(),
            rule: None,
            scope,
            destination: None,
            takes_message: false,
            stops_turn: false,
        }
    }

    /// A runtime request as an adapter builds it (120 s expiry, every
    /// decision claimed); ingest applies the engine's own policy.
    fn runtime_permission(id: &str, options: Vec<PermissionOption>) -> PermissionRequest {
        let now = now_ms();
        PermissionRequest {
            id: id.into(),
            native_request_id: id.into(),
            session_id: "s-live".into(),
            agent_id: "a-live".into(),
            host_id: "rt-1".into(),
            action: "shell".into(),
            title: "Run shell command".into(),
            command: "npm test".into(),
            args: vec![],
            directory: None,
            risk: "high".into(),
            status: "pending".into(),
            supported_decisions: vec![
                "approve-once".into(),
                "approve-session".into(),
                "reject".into(),
            ],
            revision: 1,
            fingerprint: None,
            requested_at: now,
            expires_at: now + 120_000,
            context: String::new(),
            decided_at: None,
            decision: None,
            tool_item_id: Some("tool-1".into()),
            options,
            detail: None,
        }
    }

    fn stored(engine: &Engine, id: &str) -> PermissionRequest {
        engine
            .snapshot()
            .permissions
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .unwrap()
    }

    fn resolved(events: &Arc<Mutex<Vec<ServerMsg>>>, id: &str) -> Vec<PermissionRequest> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                ServerMsg::Event {
                    kind: EventKind::PermissionResolved(p),
                    ..
                } if p.id == id => Some(p.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn supported_decisions_are_the_legacy_buckets() {
        let (mut engine, events) = make_engine();
        engine.ingest_permission(runtime_permission(
            "rt:1",
            vec![
                opt("allow", "approve-once", "once"),
                opt("always", "approve-session", "persistent"),
                opt("deny", "reject", "once"),
            ],
        ));
        let p = stored(&engine, "rt:1");
        assert_eq!(p.supported_decisions, ["approve-once", "reject"]);
        let requested = events.lock().unwrap().iter().find_map(|e| match e {
            ServerMsg::Event {
                kind: EventKind::PermissionRequested(p),
                ..
            } => Some(p.clone()),
            _ => None,
        });
        assert_eq!(requested.as_ref(), Some(&p), "the broadcast record agrees");
        // a legacy approve-session has no bucket; naming the option works,
        // and each path hands back the option the adapter must answer with
        let mut check = |decision: &str, option_id: Option<&str>| {
            engine
                .validate_native_permission_decision("rt:1", decision, option_id, None, &[], 1.0)
                .map(|o| o.map(|o| o.id))
        };
        assert_eq!(
            check("approve-session", None),
            Err("decision is not supported by this runtime".into())
        );
        assert_eq!(
            check("approve-session", Some("always")),
            Ok(Some("always".into()))
        );
        assert_eq!(check("approve-once", None), Ok(Some("allow".into())));
        // without options the adapter's decisions stand
        engine.ingest_permission(runtime_permission("rt:2", vec![]));
        assert_eq!(stored(&engine, "rt:2").supported_decisions.len(), 3);
    }

    #[test]
    fn resolve_native_bumps_revision_and_refuses_late_decisions() {
        let (mut engine, events) = make_engine();
        engine.ingest_permission(runtime_permission("rt:1", vec![]));
        engine.resolve_native("rt:1", "stale", None);
        let p = stored(&engine, "rt:1");
        assert_eq!(
            (p.status.as_str(), p.revision, p.decision.as_deref()),
            ("stale", 2, None)
        );
        assert!(p.decided_at.is_some());
        assert_eq!(resolved(&events, "rt:1"), vec![p.clone()]);
        // idempotent: a second native resolution changes nothing
        engine.resolve_native("rt:1", "approved", Some("approve-once"));
        assert_eq!(stored(&engine, "rt:1"), p);
        assert_eq!(resolved(&events, "rt:1").len(), 1);
        // the phone's late tap fails closed, whatever revision it holds
        for revision in [1.0, 2.0] {
            assert_eq!(
                engine.validate_native_permission_decision(
                    "rt:1",
                    "approve-once",
                    None,
                    None,
                    &[],
                    revision
                ),
                Err("request is no longer pending".into())
            );
        }
        // a well-formed native outcome is kept; anything else is stale
        engine.ingest_permission(runtime_permission("rt:2", vec![]));
        engine.resolve_native("rt:2", "approved", Some("approve-session"));
        let p2 = stored(&engine, "rt:2");
        assert_eq!(
            (p2.status.as_str(), p2.decision.as_deref()),
            ("approved", Some("approve-session"))
        );
        engine.ingest_permission(runtime_permission("rt:3", vec![]));
        engine.resolve_native("rt:3", "approved", None);
        assert_eq!(stored(&engine, "rt:3").status, "stale");
    }

    #[test]
    fn native_resolved_event_resolves_the_engine_record() {
        // answered in the OpenCode TUI: the adapter publishes its own copy
        // (revision 1); the engine record must resolve, not stay pending
        let (mut engine, events) = make_engine();
        let pr = runtime_permission("per_tui", vec![]);
        engine.ingest_permission(pr.clone());
        engine.publish_external(EventKind::PermissionResolved(PermissionRequest {
            status: "approved".into(),
            decision: Some("approve-once".into()),
            ..pr
        }));
        let p = stored(&engine, "per_tui");
        assert_eq!((p.status.as_str(), p.revision), ("approved", 2));
        assert_eq!(resolved(&events, "per_tui"), vec![p]);
    }

    #[test]
    fn runtime_permissions_use_the_long_ttl() {
        let (mut engine, _events) = make_engine();
        let before = now_ms();
        engine.ingest_permission(runtime_permission("rt:1", vec![]));
        let p = stored(&engine, "rt:1");
        assert!(p.expires_at >= before + LIVE_PERMISSION_TTL_MS);
        assert!(p.expires_at <= now_ms() + LIVE_PERMISSION_TTL_MS);
        // well past the adapter's own 120 s it is still pending...
        engine.pump(before + 60 * 60_000);
        assert_eq!(stored(&engine, "rt:1").status, "pending");
        // ...and the sweep still expires it at 24 h
        engine.pump(p.expires_at);
        assert_eq!(stored(&engine, "rt:1").status, "stale");
        // mock and demo requests keep the short TTL
        let ask = AskSpec {
            action: "shell".into(),
            title: "Run shell command".into(),
            command: "npm test".into(),
            args: vec![],
            directory: None,
            risk: "low".into(),
            context: String::new(),
        };
        let mock = engine.issue_permission("s1", &ask, before);
        assert_eq!(
            mock.expires_at - mock.requested_at,
            crate::protocol::PERMISSION_TTL_MS
        );
    }
}
