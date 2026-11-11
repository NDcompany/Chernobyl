/**
 * Mock agent engine — the execution + permission authority of the host
 * service. Everything here is simulated: no shell is ever spawned.
 *
 * Responsibilities:
 *  - own the authoritative Snapshot (hosts, agents, sessions, permissions)
 *  - run scripted session plans and stream output
 *  - pause plans at permission checkpoints and enforce decisions
 *  - expire stale permission requests
 *  - suppress duplicate commands by commandId
 */

import { evaluatePermissionDecision, PERMISSION_DECISION_ERRORS } from '@chernobyl/domain';
import { createSeedSnapshot } from './seed';
import { applyTranscript, emptyTranscriptView, type TranscriptView } from './transcript';
import {
  FEATURE_TRANSCRIPT,
  PERMISSION_TTL_MS,
  isStalePermission,
  nextId,
  unrefTimer,
} from './util';
import type {
  ActivityEvent,
  Agent,
  AgentState,
  ClientMessage,
  CommandOutcome,
  CommandProgressState,
  EventEnvelope,
  FileChange,
  FileDiff,
  LiveStatus,
  McpServerState,
  OutputStream,
  PermissionAction,
  PermissionDetail,
  PermissionOption,
  PermissionRequest,
  PermissionRisk,
  PlanStep as TodoStep,
  ServerMessage,
  Session,
  SessionStatus,
  Snapshot,
  TranscriptItem,
  TranscriptOp,
  TurnState,
} from './index';

interface AskSpec {
  action: PermissionAction;
  title: string;
  command: string;
  args: string[];
  directory?: string;
  risk: PermissionRisk;
  context: string;
}

type PlanStep = { line: string; stream?: OutputStream; files?: FileChange[] } | { ask: AskSpec };

interface RunState {
  sessionId: string;
  plan: PlanStep[];
  cursor: number;
  timer?: ReturnType<typeof setTimeout>;
  awaiting?: string;
  /**
   * Command that started this run, for the exactly-once terminal outcome
   * (G1). Absent for runs not started by a client command (session created
   * by create_session completes that command immediately; seeded sessions
   * resume with no client command to report to).
   */
  commandId?: string;
}

/** An event message with its envelope fields removed (stamped by broadcast). */
type Unenveloped<T> = T extends EventEnvelope ? Omit<T, keyof EventEnvelope> : T;

/* ADR-015 demo transcript (see `MockEngineOptions.live`) */

type DistOmit<T, K extends PropertyKey> = T extends unknown ? Omit<T, K> : never;
/** A transcript op before the engine stamps its seq, item/turn times and the merged state. */
type OpBody = DistOmit<TranscriptOp, 'seq'>;

/** A scripted approval: legacy fields for v2 clients, plus the runtime's own options and detail. */
interface LiveAsk {
  toolItemId: string;
  spec: AskSpec;
  options: PermissionOption[];
  detail: PermissionDetail;
  /** the script after the decision */
  then: (approved: boolean, permissionId: string, message?: string) => LiveStep[];
}

/** `ms` is the pause after the op. */
type LiveStep = { op: OpBody; ms?: number } | { ask: LiveAsk };

interface LiveRun {
  epoch: string;
  /** folded through the phone's own reducer: the snapshot source */
  view: TranscriptView;
  /** every op since seq 1, for replay (a demo transcript stays small) */
  log: TranscriptOp[];
  subscribed: boolean;
  steps: LiveStep[];
  timer?: ReturnType<typeof setTimeout>;
  /** the scripted approval waiting for a decision */
  ask?: { permissionId: string; then: LiveAsk['then'] };
  /** the running turn */
  turn?: TurnState;
  /** submit_prompt commands that end with the running turn (Claude merges mid-turn prompts) */
  commands: string[];
}

export interface MockEngineOptions {
  /**
   * ADR-015 demo (the app's offline mock): Claude Code turns live and its
   * "Refactor billing module" session plays one scripted transcript, with
   * follow-ups, stop, mode and end. Off for the reference host, whose
   * parity twin (the Rust companion's mock) streams no transcript.
   */
  live?: boolean;
}

/** Mutations that must be idempotent across reconnect retries. */
const MUTATING: ReadonlySet<ClientMessage['type']> = new Set([
  'create_session',
  'submit_prompt',
  'interrupt',
  'permission_decision',
  'session_end',
  'set_permission_mode',
  'mcp_auth',
  'mcp_auth_callback',
  'mcp_reconnect',
  'set_model',
  'rename_session',
]);

const TICK_MS = 1_000;
const STEP_MS = 450;
const MAX_ACK_CACHE = 500;
const MAX_OUTPUT_LINES = 300;
const MAX_ACTIVITY = 60;
const STREAM_MS = 90;
/** A runtime's own request waits a day, not the mock's two minutes (ADR-015 §4.5). */
const LIVE_PERMISSION_TTL_MS = 24 * 60 * 60_000;
const LIVE_SESSION_STATUS: Record<LiveStatus, SessionStatus> = {
  starting: 'running',
  running: 'running',
  waiting: 'waiting',
  idle: 'idle',
  ended: 'completed',
};
const OPEN_TOOL: ReadonlySet<string> = new Set(['pending', 'running', 'awaiting-approval']);

export class MockEngine {
  private state: Snapshot = createSeedSnapshot();
  private runs = new Map<string, RunState>();
  /** ADR-015 transcripts by session id (live mode only). */
  private live = new Map<string, LiveRun>();
  private acks = new Map<string, ServerMessage>();
  /** `${sessionId}::${command}` grants created by "approve for this session". */
  private grants = new Set<string>();
  private ticker: ReturnType<typeof setInterval>;
  /** Per-host monotonic event sequence (persisted later with the companion DB). */
  private seq = 0;

  constructor(
    private emit: (msg: ServerMessage) => void,
    opts: MockEngineOptions = {},
  ) {
    this.ticker = setInterval(() => this.tick(), TICK_MS);
    unrefTimer(this.ticker);
    if (opts.live) this.startLiveDemo();
  }

  snapshot(): Snapshot {
    return this.state;
  }

  /** Highest envelope seq issued so far (goes into hello_ack/state as lastSeq). */
  lastSeq(): number {
    return this.seq;
  }

  /** Stamps the event envelope; the engine is the event authority. */
  private broadcast(msg: Unenveloped<Extract<ServerMessage, EventEnvelope>>): void {
    this.seq += 1;
    this.emit({
      ...msg,
      seq: this.seq,
      eventId: nextId('evt'),
      occurredAt: Date.now(),
    } as ServerMessage);
  }

  /** G1: acceptance phase event for a mutating command. */
  private progress(commandId: string, state: CommandProgressState, sessionId?: string): void {
    this.broadcast({ type: 'command_progress', commandId, state, sessionId });
  }

  /** G1: the single terminal outcome for a mutating command. */
  private result(
    commandId: string,
    status: CommandOutcome,
    extra?: { error?: string; sessionId?: string; permissionId?: string },
  ): void {
    this.broadcast({ type: 'command_result', commandId, status, ...extra });
  }

  dispose(): void {
    clearInterval(this.ticker);
    for (const run of [...this.runs.values(), ...this.live.values()]) {
      if (run.timer) clearTimeout(run.timer);
    }
    this.runs.clear();
    this.live.clear();
  }

  /* ---------------------------------------------------------------- */
  /* Message handling                                                  */
  /* ---------------------------------------------------------------- */

  handle(msg: ClientMessage): ServerMessage | undefined {
    const cmdId = 'commandId' in msg ? msg.commandId : undefined;
    if (cmdId && MUTATING.has(msg.type)) {
      const cached = this.acks.get(cmdId);
      if (cached) return duplicateOf(cached);
    }

    let reply: ServerMessage | undefined;
    switch (msg.type) {
      case 'ping':
        reply = { type: 'pong', commandId: msg.commandId };
        break;
      case 'list_state':
        reply = { type: 'state', snapshot: this.state, lastSeq: this.seq };
        break;
      case 'create_session':
        reply = this.createSession(msg);
        break;
      case 'submit_prompt':
        reply = this.submitPrompt(msg);
        break;
      case 'interrupt':
        reply = this.interrupt(msg);
        break;
      case 'permission_decision':
        reply = this.decide(msg);
        break;
      case 'hello':
        reply = {
          type: 'hello_ack',
          protocol: msg.protocol,
          serverTime: Date.now(),
          snapshot: this.state,
          lastSeq: this.seq,
          features: [FEATURE_TRANSCRIPT],
        };
        break;
      // ADR-015: only a live-mode transcript session answers these; every
      // other session refuses them honestly (parity with the Rust engine)
      case 'session_subscribe':
        reply = this.subscribeLive(msg);
        break;
      case 'session_unsubscribe':
      case 'session_end':
      case 'set_permission_mode': {
        const run = this.live.get(msg.sessionId);
        reply = run
          ? this.liveCommand(msg, run)
          : ackErr(msg.commandId, 'not supported by this agent');
        break;
      }
      case 'mcp_auth':
      case 'mcp_auth_callback':
      case 'mcp_reconnect':
      // ADR-017: the composer extras are the live Claude runtime's
      case 'set_model':
      case 'rename_session':
        reply = ackErr(msg.commandId, 'not supported by this agent');
        break;
      default:
        reply = undefined;
    }

    if (reply && reply.type === 'ack' && cmdId && MUTATING.has(msg.type)) {
      this.acks.set(cmdId, reply);
      if (this.acks.size > MAX_ACK_CACHE) {
        const oldest = this.acks.keys().next().value;
        if (oldest) this.acks.delete(oldest);
      }
    }
    return reply;
  }

  /* ---------------------------------------------------------------- */
  /* Commands                                                          */
  /* ---------------------------------------------------------------- */

  private createSession(msg: Extract<ClientMessage, { type: 'create_session' }>): ServerMessage {
    // ADR-017: images and a model are the live Claude runtime's (parity with the Rust engine)
    if (msg.images?.length || msg.model !== undefined) return ackErr(msg.commandId, 'not supported by this agent');
    const agent = this.findAgent(msg.agentId);
    if (!agent) return ackErr(msg.commandId, 'unknown agent');
    if (!agent.acceptsNewSession) {
      return ackErr(msg.commandId, `${agent.name} is not accepting new sessions`);
    }
    const now = Date.now();
    // a spawning runtime names its allow-listed workspace by id (ADR-015 §4.2.3)
    const workspace = agent.live?.workspaces?.find((w) => w.id === msg.project);
    const session: Session = {
      id: nextId('s'),
      agentId: agent.id,
      hostId: agent.hostId,
      title: msg.title.trim().slice(0, 90) || 'Untitled task',
      project: workspace?.path ?? (msg.project?.trim() || 'workspace'),
      ...(agent.live ? { transcript: true as const } : {}),
      status: 'running',
      progress: 0,
      createdAt: now,
      lastActivityAt: now,
      prompt: msg.prompt,
      timeline: [
        { id: 't1', label: 'Accept task', status: 'done' },
        { id: 't2', label: 'Plan steps', status: 'active' },
        { id: 't3', label: 'Apply changes', status: 'pending' },
        { id: 't4', label: 'Verify', status: 'pending' },
      ],
      output: [],
      files: [],
    };
    this.state.sessions.unshift(session);
    this.broadcast({ type: 'session_created', session });
    this.pushActivity('session', `${agent.name} started “${session.title}”`, session.id);
    this.refreshAgent(agent.id);
    // no run state exists yet, so schedule() alone would no-op — start the plan.
    // The create command's outcome is the session's existence: accepted and
    // completed immediately; the run itself is session-level (no commandId).
    if (agent.live) {
      this.openLive(session, [
        { op: { op: 'state', state: { status: 'starting', permissionMode: 'default' } }, ms: 1500 },
        { op: { op: 'mcp', servers: DEMO_MCP }, ms: 0 },
        ...replyTurn(msg.prompt, agent.name),
      ]);
    } else {
      this.startRun(session);
    }
    this.progress(msg.commandId, 'accepted', session.id);
    this.result(msg.commandId, 'completed', { sessionId: session.id });
    return { type: 'ack', commandId: msg.commandId, ok: true, sessionId: session.id };
  }

  private submitPrompt(msg: Extract<ClientMessage, { type: 'submit_prompt' }>): ServerMessage {
    if (msg.images?.length) return ackErr(msg.commandId, 'not supported by this agent');
    const session = this.findSession(msg.sessionId);
    if (!session) return ackErr(msg.commandId, 'unknown session');
    const live = this.live.get(session.id);
    if (live) return this.promptLive(msg, session, live);
    const run = this.runs.get(session.id);
    if (run && !run.awaiting) return ackErr(msg.commandId, 'session is already running');
    if (session.status === 'waiting') {
      return ackErr(msg.commandId, 'session is waiting for a permission decision');
    }
    session.prompt = msg.text;
    session.status = 'running';
    session.lastActivityAt = Date.now();
    this.pushOutput(session, `» ${msg.text}`, 'stdout');
    this.broadcast({ type: 'session_updated', session });
    this.startRun(session, msg.commandId);
    this.progress(msg.commandId, 'accepted', session.id);
    this.progress(msg.commandId, 'running', session.id);
    return { type: 'ack', commandId: msg.commandId, ok: true, sessionId: session.id };
  }

  private interrupt(msg: Extract<ClientMessage, { type: 'interrupt' }>): ServerMessage {
    const session = this.findSession(msg.sessionId);
    if (!session) return ackErr(msg.commandId, 'unknown session');
    this.progress(msg.commandId, 'accepted', session.id);
    const live = this.live.get(session.id);
    if (live) {
      // the process survives a stop: the turn ends interrupted, the session idles
      if (live.view.state.status !== 'ended') this.stopLive(session.id, live, 'idle');
      this.result(msg.commandId, 'completed', { sessionId: session.id });
      return { type: 'ack', commandId: msg.commandId, ok: true };
    }
    const active = session.status === 'running' || session.status === 'waiting';
    if (!active) {
      // idempotent: interrupting an idle session is not an error
      this.result(msg.commandId, 'completed', { sessionId: session.id });
      return { type: 'ack', commandId: msg.commandId, ok: true };
    }
    // cancels the run and emits the interrupted prompt's own terminal (G1)
    this.stopRun(session.id, 'cancelled');
    session.status = 'interrupted';
    session.lastActivityAt = Date.now();
    // an unanswered ask is moot once the user interrupts the session
    const pending = this.state.permissions.find(
      (p) => p.sessionId === session.id && p.status === 'pending',
    );
    if (pending) this.expire(pending);
    this.pushOutput(session, 'Interrupted by user', 'system');
    this.broadcast({ type: 'session_updated', session });
    this.pushActivity('session', `Interrupted “${session.title}”`, session.id);
    this.refreshAgent(session.agentId);
    this.result(msg.commandId, 'completed', { sessionId: session.id });
    return { type: 'ack', commandId: msg.commandId, ok: true };
  }

  private decide(msg: Extract<ClientMessage, { type: 'permission_decision' }>): ServerMessage {
    const p = this.findPermission(msg.permissionId);
    if (!p) return ackErr(msg.commandId, 'unknown permission request');
    // Fail-closed authority: native metadata, pending status, expiry,
    // revision match, supported decision / option bucket, message and
    // answers (G2, ADR-015). Never translated.
    const verdict = evaluatePermissionDecision(p, msg);
    if (!verdict.ok) {
      if (verdict.code === 'expired') this.expire(p);
      return ackErr(msg.commandId, PERMISSION_DECISION_ERRORS[verdict.code]);
    }
    const option = verdict.option;

    const now = Date.now();
    const session = this.findSession(p.sessionId);
    const agent = this.findAgent(p.agentId);
    const live = this.live.get(p.sessionId);
    this.progress(msg.commandId, 'accepted', p.sessionId);
    p.revision += 1; // resolution invalidates any older decision (G2)

    if (live?.ask?.permissionId === p.id) {
      p.status = msg.decision === 'reject' ? 'rejected' : 'approved';
      p.decidedAt = now;
      p.decision = msg.decision;
      this.broadcast({ type: 'permission_resolved', permission: p });
      const verb =
        msg.decision === 'reject' ? 'rejected' : option?.scope === 'persistent' ? 'always allowed' : 'approved';
      this.pushActivity('permission', `${agent?.name ?? 'Agent'}: ${verb} “${p.command}”`, p.id);
      // the script goes on down the branch the user picked
      live.steps = [...live.ask.then(msg.decision !== 'reject', p.id, msg.message), ...live.steps];
      live.ask = undefined;
      this.playLive(p.sessionId);
    } else if (msg.decision === 'reject') {
      p.status = 'rejected';
      p.decidedAt = now;
      p.decision = 'reject';
      this.broadcast({ type: 'permission_resolved', permission: p });
      this.pushActivity(
        'permission',
        `${agent?.name ?? 'Agent'}: rejected “${p.command}”`,
        p.id,
      );
      if (session) {
        this.stopRun(session.id, 'cancelled');
        session.status = 'interrupted';
        session.lastActivityAt = now;
        this.pushOutput(session, `Rejected: ${p.title} — ${p.command} was not executed`, 'system');
        this.broadcast({ type: 'session_updated', session });
        this.refreshAgent(session.agentId);
      }
    } else {
      p.status = 'approved';
      p.decidedAt = now;
      p.decision = msg.decision;
      if (msg.decision === 'approve-session') {
        this.grants.add(`${p.sessionId}::${p.command}`);
      }
      this.broadcast({ type: 'permission_resolved', permission: p });
      this.pushActivity(
        'permission',
        msg.decision === 'approve-once'
          ? `${agent?.name ?? 'Agent'}: approved “${p.command}” once`
          : `${agent?.name ?? 'Agent'}: approved “${p.command}” for this session`,
        p.id,
      );
      if (session) {
        session.status = 'running';
        session.lastActivityAt = now;
        this.pushOutput(session, `Approved (${msg.decision}): ${p.command}`, 'system');
        this.broadcast({ type: 'session_updated', session });
        this.resume(session.id);
      }
    }
    this.result(msg.commandId, 'completed', { sessionId: p.sessionId, permissionId: p.id });
    return { type: 'ack', commandId: msg.commandId, ok: true, permissionId: p.id };
  }

  /* ---------------------------------------------------------------- */
  /* Run loop                                                          */
  /* ---------------------------------------------------------------- */

  private startRun(session: Session, commandId?: string): void {
    const run: RunState = {
      sessionId: session.id,
      plan: this.buildPlan(session),
      cursor: 0,
      commandId,
    };
    this.runs.set(session.id, run);
    this.schedule(session.id, STEP_MS);
  }

  /** Continue a run after a permission decision (recreates it if needed). */
  private resume(sessionId: string): void {
    const existing = this.runs.get(sessionId);
    if (existing) {
      existing.awaiting = undefined;
      this.schedule(sessionId, 350);
      return;
    }
    const session = this.findSession(sessionId);
    if (!session) return;
    this.startRun(session);
  }

  private buildPlan(session: Session): PlanStep[] {
    return [
      { line: `Working on “${session.title}”` },
      { line: 'Reading context and gathering files...' },
      {
        ask: {
          action: 'shell',
          title: 'Run shell command',
          command: 'npm run check',
          args: [],
          directory: session.project,
          risk: 'medium',
          context: 'Verification step: type check and unit tests before the task is reported done.',
        },
      },
      { line: 'Applying changes to 3 files', files: [
        { path: 'tests/orders.api.test.ts', additions: 24, deletions: 11, status: 'modified' },
        { path: 'tests/helpers/clock.ts', additions: 38, deletions: 0, status: 'added' },
        { path: 'src/orders/checkout.ts', additions: 6, deletions: 4, status: 'modified' },
      ] },
      { line: 'npm run check → 0 errors', stream: 'stdout' },
      { line: 'Done. Verification clean.', stream: 'stdout' },
    ];
  }

  private schedule(sessionId: string, delay: number): void {
    const run = this.runs.get(sessionId);
    if (!run) return;
    if (run.timer) clearTimeout(run.timer);
    run.timer = setTimeout(() => this.step(sessionId), delay);
    unrefTimer(run.timer);
  }

  private step(sessionId: string): void {
    const run = this.runs.get(sessionId);
    const session = this.findSession(sessionId);
    if (!run || !session) {
      this.runs.delete(sessionId);
      return;
    }
    run.timer = undefined;

    if (run.cursor >= run.plan.length) {
      this.finish(session);
      return;
    }

    const step = run.plan[run.cursor++];
    if ('ask' in step) {
      const scopeKey = `${session.id}::${step.ask.command}`;
      if (this.grants.has(scopeKey)) {
        this.pushOutput(session, `Auto-approved for this session: ${step.ask.command}`, 'system');
        this.schedule(sessionId, 300);
        return;
      }
      const permission = this.issuePermission(session, step.ask);
      run.awaiting = permission.id;
      session.status = 'waiting';
      session.lastActivityAt = permission.requestedAt;
      this.broadcast({ type: 'session_updated', session });
      this.broadcast({ type: 'permission_requested', permission });
      this.pushActivity('permission', `${this.findAgent(session.agentId)?.name ?? 'Agent'} requested to run “${step.ask.command}”`, permission.id);
      this.refreshAgent(session.agentId);
      return;
    }

    const line = this.pushOutput(session, step.line, step.stream ?? 'stdout');
    const progress = planProgress(run);
    this.broadcast({ type: 'session_output', sessionId: session.id, lines: [line], progress, status: session.status });
    if (step.files) {
      session.files = [...session.files, ...step.files];
      this.broadcast({ type: 'session_updated', session });
    }
    this.schedule(sessionId, STEP_MS);
  }

  private finish(session: Session): void {
    const run = this.runs.get(session.id);
    this.runs.delete(session.id);
    session.status = 'completed';
    session.progress = 100;
    session.lastActivityAt = Date.now();
    session.timeline = session.timeline.map((s) => ({ ...s, status: 'done' as const }));
    this.pushOutput(session, 'Session finished', 'system');
    this.broadcast({ type: 'session_updated', session });
    this.pushActivity('session', `Finished “${session.title}”`, session.id);
    this.refreshAgent(session.agentId);
    // the prompt command that started this run is done (G1: one terminal)
    if (run?.commandId) {
      this.result(run.commandId, 'completed', { sessionId: session.id });
    }
  }

  /**
   * Stops a run and emits the terminal outcome of the client command that
   * started it (G1). `expired` covers decisions that lapsed mid-run.
   */
  private stopRun(sessionId: string, terminal: 'cancelled' | 'expired'): void {
    const run = this.runs.get(sessionId);
    if (!run) return;
    if (run.timer) clearTimeout(run.timer);
    this.runs.delete(sessionId);
    if (run.commandId) {
      this.result(run.commandId, terminal, { sessionId });
    }
  }

  /* ---------------------------------------------------------------- */
  /* Permission lifecycle                                              */
  /* ---------------------------------------------------------------- */

  private issuePermission(session: Session, spec: AskSpec): PermissionRequest {
    const now = Date.now();
    const id = nextId('p');
    const permission: PermissionRequest = {
      id,
      // the mock runtime is its own native authority: its internal id IS the
      // native request id; supported decisions are the ones it actually offers
      nativeRequestId: id,
      sessionId: session.id,
      agentId: session.agentId,
      hostId: session.hostId,
      action: spec.action,
      title: spec.title,
      command: spec.command,
      args: spec.args,
      directory: spec.directory,
      risk: spec.risk,
      status: 'pending',
      supportedDecisions: ['approve-once', 'approve-session', 'reject'],
      revision: 1,
      fingerprint: `mock:${spec.action}:${spec.command}`,
      requestedAt: now,
      expiresAt: now + PERMISSION_TTL_MS,
      context: spec.context,
    };
    this.state.permissions.unshift(permission);
    return permission;
  }

  /** Expiry sweep — runs on every tick and before every decision. */
  tick(): void {
    const now = Date.now();
    for (const p of this.state.permissions) {
      if (isStalePermission(p, now)) this.expire(p);
    }
  }

  private expire(p: PermissionRequest): void {
    if (p.status !== 'pending') return;
    p.status = 'stale';
    p.decidedAt = Date.now();
    p.revision += 1; // expiry is a resolution: older decisions die with it (G2)
    this.broadcast({ type: 'permission_resolved', permission: p });
    this.pushActivity('permission', `“${p.command}” expired without a decision`, p.id);

    const session = this.findSession(p.sessionId);
    if (session && session.status === 'waiting') {
      // the blocked prompt's command gets its `expired` terminal (G1)
      this.stopRun(session.id, 'expired');
      session.status = 'idle';
      session.lastActivityAt = p.decidedAt;
      this.pushOutput(session, 'Permission request expired — task paused', 'system');
      this.broadcast({ type: 'session_updated', session });
      this.refreshAgent(session.agentId);
    }
  }

  /* ---------------------------------------------------------------- */
  /* ADR-015 live transcripts (live mode only)                         */
  /* ---------------------------------------------------------------- */

  /** Claude Code turns live and its billing session plays the demo script. */
  private startLiveDemo(): void {
    const agent = this.findAgent('a1');
    const session = this.findSession('s1');
    if (!agent || !session) return;
    agent.live = DEMO_LIVE;
    // its legacy write request belonged to the classic run the script replaces
    this.state.permissions = this.state.permissions.filter((p) => p.sessionId !== session.id);
    this.openLive(session, demoScript(session.prompt ?? 'Refactor the billing module.', session.project));
  }

  private openLive(session: Session, steps: LiveStep[]): void {
    const epoch = nextId('ep');
    session.transcript = true;
    this.live.set(session.id, {
      epoch,
      view: { ...emptyTranscriptView(), epoch },
      log: [],
      subscribed: false,
      steps,
      commands: [],
    });
    this.playLive(session.id);
  }

  /** Plays the next step; a scripted approval pauses the script until it is decided. */
  private playLive(sessionId: string): void {
    const run = this.live.get(sessionId);
    if (!run) return;
    if (run.timer) clearTimeout(run.timer);
    run.timer = undefined;
    const step = run.steps.shift();
    if (!step) return;
    if ('ask' in step) {
      this.askLive(sessionId, run, step.ask);
      return;
    }
    this.pushOp(sessionId, step.op);
    run.timer = setTimeout(() => this.playLive(sessionId), step.ms ?? STREAM_MS);
    unrefTimer(run.timer);
  }

  /**
   * Stamps the seq, item and turn times and the merged state, folds the op
   * into the view, logs it for replay and streams it to a subscriber.
   */
  private pushOp(sessionId: string, body: OpBody): void {
    const run = this.live.get(sessionId);
    const session = this.findSession(sessionId);
    if (!run || !session) return;
    const now = Date.now();
    let stamped = body;
    if (body.op === 'item') {
      stamped = { ...body, item: { ...body.item, ts: now } };
    } else if (body.op === 'state') {
      stamped = { ...body, state: { ...run.view.state, ...body.state } };
    } else if (body.op === 'turn') {
      const running = body.turn.status === 'running';
      const turn = running
        ? { ...body.turn, startedAt: now }
        : { ...body.turn, startedAt: run.turn?.startedAt ?? now, endedAt: now };
      run.turn = running ? turn : undefined;
      stamped = { ...body, turn };
    }
    const op = { ...stamped, seq: run.view.seq + 1 } as TranscriptOp;
    const msg = { type: 'transcript' as const, sessionId, epoch: run.epoch, ops: [op] };
    run.view = applyTranscript(run.view, msg).view;
    run.log.push(op);
    if (run.subscribed) this.emit(msg);
    if (op.op === 'state') this.liveStatus(session, op.state.status);
    if (op.op === 'turn' && op.turn.status !== 'running') {
      const outcome: CommandOutcome =
        op.turn.status === 'completed' ? 'completed' : op.turn.status === 'interrupted' ? 'cancelled' : 'failed';
      for (const id of run.commands.splice(0)) this.result(id, outcome, { sessionId });
    }
  }

  /** The session record follows the runtime (the journaled path: lists, badges, alerts). */
  private liveStatus(session: Session, status: LiveStatus): void {
    const next = LIVE_SESSION_STATUS[status];
    if (!next || next === session.status) return;
    session.status = next;
    session.lastActivityAt = Date.now();
    this.broadcast({ type: 'session_updated', session });
    this.refreshAgent(session.agentId);
  }

  /** The engine record first (every client, the inbox, alerts), then the card it belongs to. */
  private askLive(sessionId: string, run: LiveRun, ask: LiveAsk): void {
    const session = this.findSession(sessionId);
    if (!session) return;
    const p = this.issuePermission(session, ask.spec);
    // legacy decisions reach only the non-persistent options (G2)
    p.supportedDecisions = [...new Set(ask.options.filter((o) => o.scope !== 'persistent').map((o) => o.decision))];
    p.expiresAt = p.requestedAt + LIVE_PERMISSION_TTL_MS;
    p.toolItemId = ask.toolItemId;
    p.options = ask.options;
    p.detail = ask.detail;
    run.ask = { permissionId: p.id, then: ask.then };
    this.broadcast({ type: 'permission_requested', permission: p });
    this.pushActivity('permission', `${this.findAgent(session.agentId)?.name ?? 'Agent'} requested to run “${p.command}”`, p.id);
    const at = run.view.index.get(ask.toolItemId);
    const item = at === undefined ? undefined : run.view.items[at];
    if (item?.kind === 'tool') {
      this.pushOp(sessionId, {
        op: 'item',
        item: { ...item, tool: { ...item.tool, status: 'awaiting-approval', permissionId: p.id } },
      });
    }
    this.pushOp(sessionId, { op: 'state', state: { status: 'waiting' } });
  }

  /** §3.5: the ops after the cursor while the epoch matches, else a snapshot; live ops follow. */
  private subscribeLive(msg: Extract<ClientMessage, { type: 'session_subscribe' }>): ServerMessage {
    const run = this.live.get(msg.sessionId);
    if (!run) return ackErr(msg.commandId, 'not supported by this agent');
    const { epoch, view } = run;
    if (msg.epoch === epoch && msg.afterSeq !== undefined && msg.afterSeq <= view.seq) {
      const ops = run.log.slice(msg.afterSeq);
      if (ops.length) this.emit({ type: 'transcript', sessionId: msg.sessionId, epoch, ops });
    } else {
      this.emit({
        type: 'transcript_snapshot',
        sessionId: msg.sessionId,
        epoch,
        seq: view.seq,
        items: view.items as TranscriptItem[],
        truncatedBefore: false,
        turns: [...view.turns.values()],
        state: view.state,
        mcp: view.mcp,
      });
    }
    run.subscribed = true;
    return { type: 'ack', commandId: msg.commandId, ok: true, sessionId: msg.sessionId };
  }

  private liveCommand(
    msg: Extract<ClientMessage, { type: 'session_unsubscribe' | 'session_end' | 'set_permission_mode' }>,
    run: LiveRun,
  ): ServerMessage {
    const ok: ServerMessage = { type: 'ack', commandId: msg.commandId, ok: true, sessionId: msg.sessionId };
    if (msg.type === 'session_unsubscribe') {
      run.subscribed = false;
      return ok;
    }
    if (msg.type === 'session_end') {
      this.progress(msg.commandId, 'running', msg.sessionId);
      if (run.view.state.status !== 'ended') this.stopLive(msg.sessionId, run, 'ended');
    } else {
      this.pushOp(msg.sessionId, { op: 'state', state: { status: run.view.state.status, permissionMode: msg.mode } });
    }
    this.result(msg.commandId, 'completed', { sessionId: msg.sessionId });
    return ok;
  }

  /** A follow-up joins the running turn (Claude's promptWhileBusy 'merge'), else starts one. */
  private promptLive(
    msg: Extract<ClientMessage, { type: 'submit_prompt' }>,
    session: Session,
    run: LiveRun,
  ): ServerMessage {
    if (run.view.state.status === 'ended') return ackErr(msg.commandId, 'session has ended');
    session.prompt = msg.text;
    run.commands.push(msg.commandId);
    if (run.turn) {
      this.pushOp(session.id, {
        op: 'item',
        item: { id: nextId('itm'), ts: 0, turnId: run.turn.id, kind: 'user', text: msg.text, queued: true },
      });
    } else {
      run.steps.push(...replyTurn(msg.text, this.findAgent(session.agentId)?.name ?? 'The agent'));
      if (!run.timer && !run.ask) this.playLive(session.id);
    }
    this.progress(msg.commandId, 'accepted', session.id);
    this.progress(msg.commandId, 'running', session.id);
    return { type: 'ack', commandId: msg.commandId, ok: true, sessionId: session.id };
  }

  /** Stop or end: open tool cards cancel, the turn ends interrupted, a pending ask goes stale (late taps fail closed). */
  private stopLive(sessionId: string, run: LiveRun, status: 'idle' | 'ended'): void {
    if (run.timer) clearTimeout(run.timer);
    run.timer = undefined;
    run.steps = [];
    const pending = run.ask ? this.findPermission(run.ask.permissionId) : undefined;
    run.ask = undefined;
    if (pending) this.expire(pending);
    const turn = run.turn;
    if (turn) {
      for (const item of run.view.items) {
        if (item.kind === 'tool' && item.turnId === turn.id && OPEN_TOOL.has(item.tool.status)) {
          this.pushOp(sessionId, { op: 'item', item: { ...item, tool: { ...item.tool, status: 'cancelled' } } });
        }
      }
      this.pushOp(sessionId, { op: 'turn', turn: { ...turn, status: 'interrupted' } });
    }
    if (run.view.state.status !== status) this.pushOp(sessionId, { op: 'state', state: { status } });
  }

  /* ---------------------------------------------------------------- */
  /* Helpers                                                           */
  /* ---------------------------------------------------------------- */

  private findSession(id: string): Session | undefined {
    return this.state.sessions.find((s) => s.id === id);
  }

  private findAgent(id: string): Agent | undefined {
    return this.state.agents.find((a) => a.id === id);
  }

  private findPermission(id: string): PermissionRequest | undefined {
    return this.state.permissions.find((p) => p.id === id);
  }

  private refreshAgent(agentId: string): void {
    const agent = this.findAgent(agentId);
    if (!agent || agent.state === 'offline') return;
    const active = this.state.sessions.some(
      (s) => s.agentId === agent.id && (s.status === 'running' || s.status === 'waiting'),
    );
    const next: AgentState = active ? 'busy' : agent.acceptsNewSession ? 'online' : 'idle';
    if (next !== agent.state) {
      agent.state = next;
      this.broadcast({ type: 'agent_updated', agent });
    }
  }

  private pushOutput(session: Session, text: string, stream: OutputStream) {
    const line = { id: nextId('o'), ts: Date.now(), stream, text };
    session.output.push(line);
    if (session.output.length > MAX_OUTPUT_LINES) session.output.shift();
    session.lastActivityAt = line.ts;
    return line;
  }

  private pushActivity(kind: ActivityEvent['kind'], text: string, refId?: string): void {
    const event: ActivityEvent = { id: nextId('ev'), ts: Date.now(), kind, text, refId };
    this.state.activity.unshift(event);
    if (this.state.activity.length > MAX_ACTIVITY) this.state.activity.pop();
    this.broadcast({ type: 'activity', event });
  }
}

function ackErr(commandId: string, error: string): ServerMessage {
  return { type: 'ack', commandId, ok: false, error };
}

function duplicateOf(cached: ServerMessage): ServerMessage {
  if (cached.type === 'ack') return { ...cached, duplicate: true };
  return cached;
}

function planProgress(run: RunState): number {
  return Math.min(99, Math.round((run.cursor / run.plan.length) * 100));
}

/* ------------------------------------------------------------------ */
/* ADR-015 demo script — simulated like everything here (the agent and */
/* host carry `simulated`, so the phone badges the session)            */
/* ------------------------------------------------------------------ */

const DEMO_MCP: McpServerState[] = [{ name: 'github', status: 'connected', authFlows: [] }];

const DEMO_LIVE: NonNullable<Agent['live']> = {
  transcript: true,
  promptWhileBusy: 'merge',
  interrupt: true,
  end: true,
  permissionModes: ['default', 'acceptEdits', 'plan'],
  workspaces: [
    { id: 'ws-admin-dashboard', label: 'admin-dashboard', path: 'C:\\Projects\\admin-dashboard' },
    { id: 'ws-atlas', label: 'atlas', path: 'C:\\Projects\\atlas' },
  ],
};

const PLAN = [
  'Read the billing module',
  'Extract invoice and payment services',
  'Move the tax rules into TaxService',
  'Run the billing tests',
];
const THINKING =
  'The billing module still mixes three concerns. Invoices and payments are already extracted; the tax helpers remain in index.ts. Plan: move them into src/billing/tax.ts behind a TaxService, re-export everything from index.ts, then run the billing tests before reporting back.';
const INTRO =
  "Invoices and payments are already split out. I'll move the tax rules into **`TaxService`** (`src/billing/tax.ts`) and keep `index.ts` as a thin re-export.";
const EDIT_INPUT =
  '{\n  "file_path": "src/billing/index.ts",\n  "old_string": "export function computeTax(…",\n  "new_string": "export { InvoiceService, PaymentService, TaxService };"\n}';
const EDIT_DIFF: FileDiff = {
  path: 'src/billing/index.ts',
  status: 'modified',
  additions: 2,
  deletions: 6,
  patch: [
    '@@ -1,9 +1,5 @@',
    " import { InvoiceService } from './invoice';",
    " import { PaymentService } from './payment';",
    "-import { roundCents } from '../util/money';",
    "+import { TaxService } from './tax';",
    ' ',
    '-export function computeTax(amount: number, rate: number): number {',
    '-  return roundCents(amount * rate);',
    '-}',
    '-',
    '-export { InvoiceService, PaymentService };',
    '+export { InvoiceService, PaymentService, TaxService };',
  ].join('\n'),
};
const TEST_OUTPUT = [
  '> admin-dashboard@1.4.0 test\n> vitest run billing\n\n',
  ' ✓ src/billing/invoice.test.ts (14 tests) 211ms\n',
  ' ✓ src/billing/payment.test.ts (19 tests) 187ms\n',
  ' ✓ src/billing/tax.test.ts (9 tests) 64ms\n\n',
  ' Test Files  3 passed (3)\n      Tests  42 passed (42)\n',
];
const DONE =
  'All **42** billing tests pass. `index.ts` is now a thin re-export:\n\n- `InvoiceService` and `PaymentService` are unchanged\n- the tax rules live in `src/billing/tax.ts` behind `TaxService`\n\nShould I move `roundCents` next to it as well?';
const DENIED =
  "The tax rules are in `TaxService` now, but untested. Run `npm test -- billing` when you're ready, or tell me what to check instead.";

/** Two-word pieces that join back to `text` exactly. */
function chunks(text: string): string[] {
  return text.match(/\s*\S+\s*(?:\S+\s*)?/g) ?? [];
}

/** A text or thinking item streamed in: empty, one append per piece, then the authoritative replace. */
function streamed(kind: 'text' | 'thinking', id: string, turnId: string, text: string): LiveStep[] {
  return [
    { op: { op: 'item', item: { id, ts: 0, turnId, kind, text: '', done: false } } },
    ...chunks(text).map((t): LiveStep => ({ op: { op: 'append', id, field: 'text', text: t } })),
    { op: { op: 'item', item: { id, ts: 0, turnId, kind, text, done: true } }, ms: 400 },
  ];
}

/** A prompt and a short simulated reply, as one turn (new sessions and follow-ups). */
function replyTurn(prompt: string, agent: string): LiveStep[] {
  const turnId = nextId('turn');
  const gist = prompt.length > 80 ? `${prompt.slice(0, 79)}…` : prompt;
  return [
    { op: { op: 'item', item: { id: nextId('itm'), ts: 0, turnId, kind: 'user', text: prompt } }, ms: 300 },
    { op: { op: 'turn', turn: { id: turnId, status: 'running', startedAt: 0 } }, ms: 0 },
    { op: { op: 'state', state: { status: 'running' } }, ms: 500 },
    ...streamed(
      'text',
      nextId('itm'),
      turnId,
      `Simulated reply. In a live session ${agent} would now work on “${gist}” and stream every step here, with approvals inline. Nothing runs on a real machine in this demo.`,
    ),
    { op: { op: 'turn', turn: { id: turnId, status: 'completed', startedAt: 0, usage: { inputTokens: 2_480, outputTokens: 96 } } }, ms: 0 },
    { op: { op: 'state', state: { status: 'idle' } } },
  ];
}

/**
 * The billing session's turn: thinking, a plan, streamed text, an Edit with
 * its diff, then `npm test` behind an inline approval offering allow once,
 * a persistent rule and deny with a message.
 */
function demoScript(prompt: string, project: string): LiveStep[] {
  const turnId = 'demo-turn';
  const plan = (...status: TodoStep['status'][]): LiveStep => ({
    op: { op: 'item', item: { id: 'demo-plan', ts: 0, turnId, kind: 'plan', steps: PLAN.map((text, i) => ({ text, status: status[i] })) } },
    ms: 400,
  });
  const command = 'npm test -- billing';
  const edit = {
    id: 'demo-edit',
    ts: 0,
    turnId,
    kind: 'tool',
    tool: { name: 'Edit', title: 'src/billing/index.ts', status: 'pending' },
  } satisfies TranscriptItem;
  const bash = {
    id: 'demo-bash',
    ts: 0,
    turnId,
    kind: 'tool',
    tool: { name: 'Bash', title: command, status: 'pending', input: command },
  } satisfies TranscriptItem;
  return [
    { op: { op: 'mcp', servers: DEMO_MCP }, ms: 0 },
    { op: { op: 'state', state: { status: 'running', permissionMode: 'default' } }, ms: 300 },
    { op: { op: 'item', item: { id: 'demo-user', ts: 0, turnId, kind: 'user', text: prompt } }, ms: 300 },
    { op: { op: 'turn', turn: { id: turnId, status: 'running', startedAt: 0 } }, ms: 600 },
    ...streamed('thinking', 'demo-thinking', turnId, THINKING),
    plan('completed', 'completed', 'in_progress', 'pending'),
    ...streamed('text', 'demo-text', turnId, INTRO),
    { op: { op: 'item', item: edit }, ms: 200 },
    ...chunks(EDIT_INPUT).map((text): LiveStep => ({ op: { op: 'append', id: edit.id, field: 'input', text }, ms: 60 })),
    { op: { op: 'item', item: { ...edit, tool: { ...edit.tool, status: 'running', input: EDIT_INPUT } } }, ms: 600 },
    {
      op: {
        op: 'item',
        item: {
          ...edit,
          tool: { ...edit.tool, status: 'completed', input: EDIT_INPUT, diffs: [EDIT_DIFF], autoApproved: 'rule', durationMs: 412 },
        },
      },
      ms: 500,
    },
    plan('completed', 'completed', 'completed', 'in_progress'),
    { op: { op: 'item', item: bash }, ms: 500 },
    {
      ask: {
        toolItemId: bash.id,
        spec: {
          action: 'shell',
          title: 'Run shell command',
          command,
          args: [],
          directory: project,
          risk: 'high',
          context: 'Verifying the billing split with the test suite before reporting back.',
        },
        options: [
          { id: 'allow', decision: 'approve-once', label: 'Allow', scope: 'once' },
          {
            id: 'allow-always',
            decision: 'approve-session',
            label: 'Always allow `npm test` in this project',
            rule: 'Bash(npm test:*)',
            scope: 'persistent',
            destination: '.claude/settings.local.json',
          },
          { id: 'deny', decision: 'reject', label: 'Deny', scope: 'once', takesMessage: true },
        ],
        detail: { kind: 'shell', command, cwd: project, reason: 'Verify the billing split before reporting back' },
        then: (approved, permissionId, message) => {
          const tool: LiveStep[] = approved
            ? [
                { op: { op: 'item', item: { ...bash, tool: { ...bash.tool, status: 'running', permissionId } } }, ms: 500 },
                ...TEST_OUTPUT.map((text): LiveStep => ({ op: { op: 'append', id: bash.id, field: 'output', text }, ms: 400 })),
                {
                  op: {
                    op: 'item',
                    item: {
                      ...bash,
                      tool: { ...bash.tool, status: 'completed', permissionId, output: TEST_OUTPUT.join(''), durationMs: 1840 },
                    },
                  },
                  ms: 400,
                },
                plan('completed', 'completed', 'completed', 'completed'),
              ]
            : [
                { op: { op: 'item', item: { ...bash, tool: { ...bash.tool, status: 'denied', permissionId } } }, ms: 500 },
                plan('completed', 'completed', 'completed', 'cancelled'),
              ];
          const reply = approved
            ? DONE
            : message
              ? `Understood, I won't run the tests. You said: “${message}”\n\n${DENIED}`
              : DENIED;
          return [
            { op: { op: 'state', state: { status: 'running' } }, ms: 0 },
            ...tool,
            ...streamed('text', 'demo-done', turnId, reply),
            {
              op: {
                op: 'turn',
                turn: {
                  id: turnId,
                  status: 'completed',
                  startedAt: 0,
                  usage: { inputTokens: 18_240, outputTokens: 1_310, cacheReadTokens: 12_800 },
                  costUsd: 0.04,
                },
              },
              ms: 0,
            },
            { op: { op: 'state', state: { status: 'idle' } } },
          ];
        },
      },
    },
  ];
}
