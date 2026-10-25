/**
 * CHERNOBYL shared protocol (v2).
 *
 * Single source of truth for hosts, agents, sessions, activity events and
 * permission requests, plus the WebSocket client/server message envelope.
 * The only dependency is @chernobyl/domain (the lifecycle FSM authority);
 * otherwise payload-light on purpose — the host service and the Expo app
 * both consume it.
 *
 * v2 changes (see docs/decisions/PHASE1-GATE-REQUIREMENTS.md):
 *  - G1: every mutating command has a three-phase lifecycle — acceptance
 *    (`ack`), execution (`command_progress`), exactly one terminal outcome
 *    (`command_result`). An ack is NEVER an execution outcome.
 *  - G2: PermissionRequest carries the native request id, the decisions the
 *    runtime actually supports, and a revision that invalidates stale
 *    decisions. Decisions are never synthesized or translated.
 *  - Every event message carries an EventEnvelope (per-host monotonic
 *    `seq`, `eventId`, `occurredAt`); snapshot replies carry `lastSeq` so
 *    clients can detect gaps.
 *
 * All timestamps are epoch milliseconds (UTC). All ids are opaque strings.
 */

import type { CommandState } from '@chernobyl/domain';

/* Constants and helpers live in ./util and are re-exported at the bottom. */

/* ------------------------------------------------------------------ */
/* Enumerations                                                        */
/* ------------------------------------------------------------------ */

export type HostKind = 'local' | 'remote';
export type HostStatus = 'connected' | 'connecting' | 'offline';

export type AgentKind =
  | 'claude-code'
  | 'opencode'
  | 'codex'
  | 'browser'
  | 'local'
  | 'custom';

export type AgentState = 'online' | 'busy' | 'idle' | 'offline';

export type SessionStatus =
  | 'running'
  | 'waiting'
  | 'completed'
  | 'interrupted'
  | 'idle';

export type PermissionAction =
  | 'shell'
  | 'file-write'
  | 'network'
  | 'agent-control';

export type PermissionRisk = 'low' | 'medium' | 'high';

/**
 * `pending`  – awaiting a decision, not expired.
 * `stale`    – expired without a decision; may still be shown but never decided.
 * `expired`  – alias kept explicit for audit rows.
 * `approved` / `rejected` – decided (approved carries scope).
 */
export type PermissionStatus =
  | 'pending'
  | 'stale'
  | 'expired'
  | 'approved'
  | 'rejected';

export type PermissionDecision =
  | 'approve-once'
  | 'approve-session'
  | 'reject';

export type ActivityKind = 'session' | 'permission' | 'host' | 'agent';

export type OutputStream = 'stdout' | 'stderr' | 'system';

/** Terminal command outcome; `unknown` = safe reconciliation impossible (PRD §12.4). */
export type CommandOutcome = Extract<
  CommandState,
  'completed' | 'failed' | 'cancelled' | 'expired' | 'unknown'
>;

/** Execution phase reported before an outcome; never terminal by itself. */
export type CommandProgressState = 'accepted' | 'running';

/**
 * Envelope stamped on every event message by the event authority (host).
 * `seq` is monotonic per host; a restart without persisted sequences is
 * detectable because it resets (persistence lands with the companion DB).
 */
export interface EventEnvelope {
  seq: number;
  eventId: string;
  occurredAt: number;
}

/* ------------------------------------------------------------------ */
/* Entities                                                            */
/* ------------------------------------------------------------------ */

export interface Host {
  id: string;
  name: string;
  kind: HostKind;
  platform: string;
  /** Display address only — never a credential. */
  address: string;
  status: HostStatus;
  lastSeenAt: number;
  /**
   * ADR-009 provenance: demo data from a mock seed/engine, never a live
   * runtime. Present only when true (real adapters omit it).
   */
  simulated?: boolean;
}

export interface Agent {
  id: string;
  hostId: string;
  kind: AgentKind;
  name: string;
  state: AgentState;
  /** Human-readable capability labels shown on the agent detail screen. */
  capabilities: string[];
  /** True when the agent accepts new prompts right now. */
  acceptsNewSession: boolean;
  /** ADR-009 provenance, same rule as `Host.simulated`. */
  simulated?: boolean;
}

export interface OutputLine {
  id: string;
  ts: number;
  stream: OutputStream;
  text: string;
}

export interface SessionStep {
  id: string;
  label: string;
  status: 'done' | 'active' | 'pending';
}

export interface FileChange {
  path: string;
  additions: number;
  deletions: number;
  status: 'modified' | 'added' | 'removed';
}

export interface Session {
  id: string;
  agentId: string;
  hostId: string;
  title: string;
  project: string;
  status: SessionStatus;
  /** 0–100 */
  progress: number;
  createdAt: number;
  lastActivityAt: number;
  /** Last prompt submitted by the user (display only). */
  prompt?: string;
  timeline: SessionStep[];
  output: OutputLine[];
  files: FileChange[];
}

export interface PermissionRequest {
  id: string;
  /**
   * Opaque id the native runtime uses for this request (G2). Decisions are
   * addressed by this id — never by a synthesized one.
   */
  nativeRequestId: string;
  sessionId: string;
  agentId: string;
  hostId: string;
  action: PermissionAction;
  /** Short action label, e.g. "Run shell command". */
  title: string;
  /** Exact command / path / URL the agent wants to use. */
  command: string;
  args: string[];
  directory?: string;
  risk: PermissionRisk;
  status: PermissionStatus;
  /** Decisions the native runtime actually offers; never assumed (G2). */
  supportedDecisions: PermissionDecision[];
  /** Bumps on every resolution/expiry; a decision must match to apply (G2). */
  revision: number;
  /** Content fingerprint for display/audit; optional, never authoritative. */
  fingerprint?: string;
  requestedAt: number;
  expiresAt: number;
  /** Free-form context from the host (what the agent is doing and why). */
  context: string;
  decidedAt?: number;
  decision?: PermissionDecision;
}

export interface ActivityEvent {
  id: string;
  ts: number;
  kind: ActivityKind;
  text: string;
  /** Id of the session/permission/host the event refers to, if any. */
  refId?: string;
}

/** Full state snapshot delivered on connect (or resume after reconnect). */
export interface Snapshot {
  hosts: Host[];
  agents: Agent[];
  sessions: Session[];
  permissions: PermissionRequest[];
  activity: ActivityEvent[];
}

/* ------------------------------------------------------------------ */
/* Client → server messages                                            */
/* ------------------------------------------------------------------ */

export type ClientMessage =
  | { type: 'hello'; clientId: string; protocol: number }
  | { type: 'ping'; commandId: string }
  | {
      type: 'create_session';
      commandId: string;
      agentId: string;
      title: string;
      prompt: string;
      /** Working directory / project label for the new session. */
      project?: string;
    }
  | { type: 'submit_prompt'; commandId: string; sessionId: string; text: string }
  | { type: 'interrupt'; commandId: string; sessionId: string }
  | {
      type: 'permission_decision';
      commandId: string;
      permissionId: string;
      decision: PermissionDecision;
      /** Revision the user decided against; mismatch fails closed (G2). */
      revision: number;
    }
  | { type: 'list_state'; commandId: string };

export type ClientMessageType = ClientMessage['type'];

/* ------------------------------------------------------------------ */
/* Server → client messages                                            */
/* ------------------------------------------------------------------ */

export type ServerMessage =
  | {
      type: 'hello_ack';
      protocol: number;
      serverTime: number;
      snapshot: Snapshot;
      /** Highest envelope seq at reply time; clients use it to detect gaps. */
      lastSeq: number;
    }
  | { type: 'pong'; commandId: string }
  /**
   * ACCEPTANCE ONLY: the command was persisted and routed (duplicate flag
   * marks replays). Never an execution outcome — outcomes arrive as
   * exactly one `command_result` per command (G1).
   */
  | {
      type: 'ack';
      commandId: string;
      ok: boolean;
      error?: string;
      /** True when the command was already processed (duplicate suppression). */
      duplicate?: boolean;
      sessionId?: string;
      permissionId?: string;
    }
  | { type: 'state'; snapshot: Snapshot; lastSeq: number }
  | ({ type: 'host_updated'; host: Host } & EventEnvelope)
  | ({ type: 'agent_updated'; agent: Agent } & EventEnvelope)
  | ({ type: 'session_created'; session: Session } & EventEnvelope)
  | ({
      type: 'session_updated';
      session: Session;
    } & EventEnvelope)
  | ({
      type: 'session_output';
      sessionId: string;
      lines: OutputLine[];
      progress: number;
      status: SessionStatus;
    } & EventEnvelope)
  | ({
      type: 'permission_requested';
      permission: PermissionRequest;
    } & EventEnvelope)
  | ({
      type: 'permission_resolved';
      permission: PermissionRequest;
    } & EventEnvelope)
  | ({ type: 'activity'; event: ActivityEvent } & EventEnvelope)
  /** Execution phase of a mutating command (accepted → running). */
  | ({
      type: 'command_progress';
      commandId: string;
      state: CommandProgressState;
      sessionId?: string;
    } & EventEnvelope)
  /** Exactly one terminal outcome per mutating command (G1). */
  | ({
      type: 'command_result';
      commandId: string;
      status: CommandOutcome;
      error?: string;
      sessionId?: string;
      permissionId?: string;
    } & EventEnvelope);

export type ServerMessageType = ServerMessage['type'];

/* ------------------------------------------------------------------ */
/* Runtime validation (trust boundary — never assume the peer's shape) */
/* ------------------------------------------------------------------ */

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null;
}

function isStr(v: unknown): v is string {
  return typeof v === 'string' && v.length > 0;
}

function isNum(v: unknown): v is number {
  return typeof v === 'number' && Number.isFinite(v);
}

const CLIENT_TYPES: ReadonlySet<string> = new Set([
  'hello',
  'ping',
  'create_session',
  'submit_prompt',
  'interrupt',
  'permission_decision',
  'list_state',
]);

const SERVER_TYPES: ReadonlySet<string> = new Set([
  'hello_ack',
  'pong',
  'ack',
  'state',
  'host_updated',
  'agent_updated',
  'session_created',
  'session_updated',
  'session_output',
  'permission_requested',
  'permission_resolved',
  'activity',
  'command_progress',
  'command_result',
]);

function hasCommandId(v: Record<string, unknown>): boolean {
  return isStr(v.commandId);
}

/** Every event message must carry a well-formed envelope. */
function hasEnvelope(v: Record<string, unknown>): boolean {
  return isNum(v.seq) && isStr(v.eventId) && isNum(v.occurredAt);
}

/** Validates an untrusted payload as a ClientMessage. */
export function isClientMessage(v: unknown): v is ClientMessage {
  if (!isRecord(v) || !isStr(v.type) || !CLIENT_TYPES.has(v.type)) return false;
  switch (v.type) {
    case 'hello':
      return isStr(v.clientId) && isNum(v.protocol);
    case 'create_session':
      return hasCommandId(v) && isStr(v.agentId) && isStr(v.title) && isStr(v.prompt);
    case 'submit_prompt':
      return hasCommandId(v) && isStr(v.sessionId) && isStr(v.text);
    case 'interrupt':
      return hasCommandId(v) && isStr(v.sessionId);
    case 'permission_decision':
      return (
        hasCommandId(v) &&
        isStr(v.permissionId) &&
        isNum(v.revision) &&
        (v.decision === 'approve-once' ||
          v.decision === 'approve-session' ||
          v.decision === 'reject')
      );
    case 'list_state':
    case 'ping':
      return hasCommandId(v);
    default:
      return false;
  }
}

function hasSnapshot(v: Record<string, unknown>): boolean {
  const s = v.snapshot;
  return (
    isRecord(s) &&
    Array.isArray(s.hosts) &&
    Array.isArray(s.agents) &&
    Array.isArray(s.sessions) &&
    Array.isArray(s.permissions) &&
    Array.isArray(s.activity)
  );
}

/** Validates an untrusted payload as a ServerMessage. */
export function isServerMessage(v: unknown): v is ServerMessage {
  if (!isRecord(v) || !isStr(v.type) || !SERVER_TYPES.has(v.type)) return false;
  switch (v.type) {
    case 'hello_ack':
      return (
        isNum(v.protocol) && isNum(v.serverTime) && isNum(v.lastSeq) && hasSnapshot(v)
      );
    case 'pong':
      return isStr(v.commandId);
    case 'ack':
      return isStr(v.commandId) && typeof v.ok === 'boolean';
    case 'state':
      return isNum(v.lastSeq) && hasSnapshot(v);
    case 'host_updated':
      return isRecord(v.host) && isStr(v.host.id) && hasEnvelope(v);
    case 'agent_updated':
      return isRecord(v.agent) && isStr(v.agent.id) && hasEnvelope(v);
    case 'session_created':
    case 'session_updated':
      return isRecord(v.session) && isStr(v.session.id) && hasEnvelope(v);
    case 'session_output':
      return (
        isStr(v.sessionId) &&
        Array.isArray(v.lines) &&
        isNum(v.progress) &&
        isStr(v.status) &&
        hasEnvelope(v)
      );
    case 'permission_requested':
    case 'permission_resolved':
      return isRecord(v.permission) && isStr(v.permission.id) && hasEnvelope(v);
    case 'activity':
      return isRecord(v.event) && isStr(v.event.id) && hasEnvelope(v);
    case 'command_progress':
      return (
        hasCommandId(v) &&
        (v.state === 'accepted' || v.state === 'running') &&
        hasEnvelope(v)
      );
    case 'command_result':
      return (
        hasCommandId(v) &&
        (v.status === 'completed' ||
          v.status === 'failed' ||
          v.status === 'cancelled' ||
          v.status === 'expired' ||
          v.status === 'unknown') &&
        hasEnvelope(v)
      );
    default:
      return false;
  }
}

/** Parse a raw WS frame; returns null instead of throwing. */
export function parseServerMessage(raw: string): ServerMessage | null {
  try {
    const parsed: unknown = JSON.parse(raw);
    return isServerMessage(parsed) ? parsed : null;
  } catch {
    return null;
  }
}

export function parseClientMessage(raw: string): ClientMessage | null {
  try {
    const parsed: unknown = JSON.parse(raw);
    return isClientMessage(parsed) ? parsed : null;
  } catch {
    return null;
  }
}

/* ------------------------------------------------------------------ */
/* Small helpers shared by both sides (see ./util for the rest)        */
/* ------------------------------------------------------------------ */

export function isPermissionAction(v: unknown): v is PermissionAction {
  return (
    v === 'shell' ||
    v === 'file-write' ||
    v === 'network' ||
    v === 'agent-control'
  );
}

export * from './util';
export { createSeedSnapshot } from './seed';
export { MockEngine } from './engine';
