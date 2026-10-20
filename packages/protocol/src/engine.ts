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

import { createSeedSnapshot } from './seed';
import { PERMISSION_TTL_MS, isStalePermission, nextId, unrefTimer } from './util';
import type {
  ActivityEvent,
  Agent,
  AgentState,
  ClientMessage,
  FileChange,
  OutputStream,
  PermissionAction,
  PermissionRequest,
  PermissionRisk,
  ServerMessage,
  Session,
  Snapshot,
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
}

/** Mutations that must be idempotent across reconnect retries. */
const MUTATING: ReadonlySet<ClientMessage['type']> = new Set([
  'create_session',
  'submit_prompt',
  'interrupt',
  'permission_decision',
]);

const TICK_MS = 1_000;
const STEP_MS = 450;
const MAX_ACK_CACHE = 500;
const MAX_OUTPUT_LINES = 300;
const MAX_ACTIVITY = 60;

export class MockEngine {
  private state: Snapshot = createSeedSnapshot();
  private runs = new Map<string, RunState>();
  private acks = new Map<string, ServerMessage>();
  /** `${sessionId}::${command}` grants created by "approve for this session". */
  private grants = new Set<string>();
  private ticker: ReturnType<typeof setInterval>;

  constructor(private emit: (msg: ServerMessage) => void) {
    this.ticker = setInterval(() => this.tick(), TICK_MS);
    unrefTimer(this.ticker);
  }

  snapshot(): Snapshot {
    return this.state;
  }

  dispose(): void {
    clearInterval(this.ticker);
    for (const run of this.runs.values()) {
      if (run.timer) clearTimeout(run.timer);
    }
    this.runs.clear();
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
        reply = { type: 'state', snapshot: this.state };
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
        };
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
    const agent = this.findAgent(msg.agentId);
    if (!agent) return ackErr(msg.commandId, 'unknown agent');
    if (!agent.acceptsNewSession) {
      return ackErr(msg.commandId, `${agent.name} is not accepting new sessions`);
    }
    const now = Date.now();
    const session: Session = {
      id: nextId('s'),
      agentId: agent.id,
      hostId: agent.hostId,
      title: msg.title.trim().slice(0, 90) || 'Untitled task',
      project: msg.project?.trim() || 'workspace',
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
    this.emit({ type: 'session_created', session });
    this.pushActivity('session', `${agent.name} started “${session.title}”`, session.id);
    this.refreshAgent(agent.id);
    // no run state exists yet, so schedule() alone would no-op — start the plan
    this.startRun(session);
    return { type: 'ack', commandId: msg.commandId, ok: true, sessionId: session.id };
  }

  private submitPrompt(msg: Extract<ClientMessage, { type: 'submit_prompt' }>): ServerMessage {
    const session = this.findSession(msg.sessionId);
    if (!session) return ackErr(msg.commandId, 'unknown session');
    const run = this.runs.get(session.id);
    if (run && !run.awaiting) return ackErr(msg.commandId, 'session is already running');
    if (session.status === 'waiting') {
      return ackErr(msg.commandId, 'session is waiting for a permission decision');
    }
    session.prompt = msg.text;
    session.status = 'running';
    session.lastActivityAt = Date.now();
    this.pushOutput(session, `» ${msg.text}`, 'stdout');
    this.emit({ type: 'session_updated', session });
    this.startRun(session);
    return { type: 'ack', commandId: msg.commandId, ok: true, sessionId: session.id };
  }

  private interrupt(msg: Extract<ClientMessage, { type: 'interrupt' }>): ServerMessage {
    const session = this.findSession(msg.sessionId);
    if (!session) return ackErr(msg.commandId, 'unknown session');
    const active = session.status === 'running' || session.status === 'waiting';
    if (!active) {
      // idempotent: interrupting an idle session is not an error
      return { type: 'ack', commandId: msg.commandId, ok: true };
    }
    this.stopRun(session.id);
    session.status = 'interrupted';
    session.lastActivityAt = Date.now();
    // an unanswered ask is moot once the user interrupts the session
    const pending = this.state.permissions.find(
      (p) => p.sessionId === session.id && p.status === 'pending',
    );
    if (pending) this.expire(pending);
    this.pushOutput(session, 'Interrupted by user', 'system');
    this.emit({ type: 'session_updated', session });
    this.pushActivity('session', `Interrupted “${session.title}”`, session.id);
    this.refreshAgent(session.agentId);
    return { type: 'ack', commandId: msg.commandId, ok: true };
  }

  private decide(msg: Extract<ClientMessage, { type: 'permission_decision' }>): ServerMessage {
    const p = this.findPermission(msg.permissionId);
    if (!p) return ackErr(msg.commandId, 'unknown permission request');
    if (p.status !== 'pending') return ackErr(msg.commandId, 'request is no longer pending');
    if (isStalePermission(p)) {
      this.expire(p);
      return ackErr(msg.commandId, 'request expired before a decision was made');
    }

    const now = Date.now();
    const session = this.findSession(p.sessionId);
    const agent = this.findAgent(p.agentId);

    if (msg.decision === 'reject') {
      p.status = 'rejected';
      p.decidedAt = now;
      p.decision = 'reject';
      this.emit({ type: 'permission_resolved', permission: p });
      this.pushActivity(
        'permission',
        `${agent?.name ?? 'Agent'}: rejected “${p.command}”`,
        p.id,
      );
      if (session) {
        this.stopRun(session.id);
        session.status = 'interrupted';
        session.lastActivityAt = now;
        this.pushOutput(session, `Rejected: ${p.title} — ${p.command} was not executed`, 'system');
        this.emit({ type: 'session_updated', session });
        this.refreshAgent(session.agentId);
      }
    } else {
      p.status = 'approved';
      p.decidedAt = now;
      p.decision = msg.decision;
      if (msg.decision === 'approve-session') {
        this.grants.add(`${p.sessionId}::${p.command}`);
      }
      this.emit({ type: 'permission_resolved', permission: p });
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
        this.emit({ type: 'session_updated', session });
        this.resume(session.id);
      }
    }
    return { type: 'ack', commandId: msg.commandId, ok: true, permissionId: p.id };
  }

  /* ---------------------------------------------------------------- */
  /* Run loop                                                          */
  /* ---------------------------------------------------------------- */

  private startRun(session: Session): void {
    const run: RunState = {
      sessionId: session.id,
      plan: this.buildPlan(session),
      cursor: 0,
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
      this.emit({ type: 'session_updated', session });
      this.emit({ type: 'permission_requested', permission });
      this.pushActivity('permission', `${this.findAgent(session.agentId)?.name ?? 'Agent'} requested to run “${step.ask.command}”`, permission.id);
      this.refreshAgent(session.agentId);
      return;
    }

    const line = this.pushOutput(session, step.line, step.stream ?? 'stdout');
    const progress = planProgress(run);
    this.emit({ type: 'session_output', sessionId: session.id, lines: [line], progress, status: session.status });
    if (step.files) {
      session.files = [...session.files, ...step.files];
      this.emit({ type: 'session_updated', session });
    }
    this.schedule(sessionId, STEP_MS);
  }

  private finish(session: Session): void {
    this.runs.delete(session.id);
    session.status = 'completed';
    session.progress = 100;
    session.lastActivityAt = Date.now();
    session.timeline = session.timeline.map((s) => ({ ...s, status: 'done' as const }));
    this.pushOutput(session, 'Session finished', 'system');
    this.emit({ type: 'session_updated', session });
    this.pushActivity('session', `Finished “${session.title}”`, session.id);
    this.refreshAgent(session.agentId);
  }

  private stopRun(sessionId: string): void {
    const run = this.runs.get(sessionId);
    if (!run) return;
    if (run.timer) clearTimeout(run.timer);
    this.runs.delete(sessionId);
  }

  /* ---------------------------------------------------------------- */
  /* Permission lifecycle                                              */
  /* ---------------------------------------------------------------- */

  private issuePermission(session: Session, spec: AskSpec): PermissionRequest {
    const now = Date.now();
    const permission: PermissionRequest = {
      id: nextId('p'),
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
    this.emit({ type: 'permission_resolved', permission: p });
    this.pushActivity('permission', `“${p.command}” expired without a decision`, p.id);

    const session = this.findSession(p.sessionId);
    if (session && session.status === 'waiting') {
      this.stopRun(session.id);
      session.status = 'idle';
      session.lastActivityAt = p.decidedAt;
      this.pushOutput(session, 'Permission request expired — task paused', 'system');
      this.emit({ type: 'session_updated', session });
      this.refreshAgent(session.agentId);
    }
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
      this.emit({ type: 'agent_updated', agent });
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
    this.emit({ type: 'activity', event });
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
