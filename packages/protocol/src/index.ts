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
 *  - ADR-015 (additive, still v2): live transcript messages, advertised by
 *    `hello_ack.features` (FEATURE_TRANSCRIPT); the reducer is ./transcript.
 *  - ADR-016 (additive, still v2): workspace files and git, advertised by
 *    FEATURE_WORKSPACE.
 *  - ADR-020 (additive, still v2): the opt-in one-shot terminal, advertised by
 *    FEATURE_TERMINAL only when the PC turned it on.
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
  /** ADR-015: present only for runtimes that stream a live transcript. */
  live?: {
    transcript: true;
    promptWhileBusy: 'merge' | 'queue' | 'none';
    interrupt: boolean;
    /** session_end supported (Claude). */
    end: boolean;
    /** Claude: ['default', 'acceptEdits', 'plan']. */
    permissionModes?: string[];
    /** Spawning runtimes only. */
    workspaces?: Array<{ id: string; label: string; path: string }>;
    /** ADR-017 composer extras this runtime takes (Claude); the phone hides the rest. */
    composer?: ComposerFeature[];
  };
}

/** ADR-017: `Agent.live.composer` entries. */
export type ComposerFeature = 'model' | 'images' | 'commands' | 'context' | 'rename';

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
  /** ADR-015: present only when a transcript stream exists for this session. */
  transcript?: true;
  /** ADR-018: a Claude Code session another process on the PC is writing right now. */
  livePc?: true;
  /** ADR-018: the title of the session this one was forked from. */
  forkedFrom?: string;
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
  /** ADR-015: transcript tool item this request belongs to. */
  toolItemId?: string;
  /** ADR-015: the runtime's own choices; never empty when present, always holds a 'reject'. */
  options?: PermissionOption[];
  /** ADR-015: what is being asked (command, diff, questions, plan). */
  detail?: PermissionDetail;
  /** ADR-018: asked by a Claude Code session on the PC through its hook. */
  fromPc?: true;
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

/* ---------------- ADR-015 live sessions (additive; protocol stays 2) ---------------- */

// FEATURE_TRANSCRIPT = 'transcript.v1' lives in ./util with the other
// constants (the engine needs it without importing this module back).

export type ToolStatus =
  | 'pending'            // announced, input may still be streaming
  | 'running'
  | 'awaiting-approval'  // a PermissionRequest with toolItemId === this item is pending
  | 'completed'
  | 'failed'
  | 'denied'             // the user or a rule refused it
  | 'cancelled';         // interrupted / withdrawn

export interface FileDiff {
  path: string;
  status: 'added' | 'modified' | 'deleted' | 'renamed';
  additions: number;
  deletions: number;
  /** Unified diff text (lines starting ' ', '+', '-', '@@'), redacted, at most 64 KiB. */
  patch: string;
  truncated?: true;
}

export interface PlanStep {
  text: string;
  status: 'pending' | 'in_progress' | 'completed' | 'cancelled';
}

export interface ToolCall {
  /** Runtime tool name, shown verbatim: "Bash", "Edit", "shell", "apply_patch", "sentry_find_issues". */
  name: string;
  /** One-line summary: the command, path or URL. */
  title?: string;
  status: ToolStatus;
  /** Final input as pretty JSON or the raw command; while streaming, raw partial JSON. Redacted, at most 16 KiB. */
  input?: string;
  /** Redacted; head + tail, at most 32 KiB. */
  output?: string;
  outputTruncated?: true;
  error?: string;
  diffs?: FileDiff[];
  /** PermissionRequest.id while or after it was asked. */
  permissionId?: string;
  /** Why no prompt appeared: 'rule' | 'mode' | 'read-only' | 'hook' (runtime-reported). */
  autoApproved?: string;
  durationMs?: number;
}

interface ItemBase {
  id: string;          // stable per session; adapter-defined (§3.6)
  ts: number;
  turnId?: string;
  parentId?: string;   // nested under a subagent / Task tool item
}

export type TranscriptItem =
  | (ItemBase & { kind: 'user'; text: string; queued?: true; /** ADR-017: how many images it carried */ images?: number })
  | (ItemBase & { kind: 'text'; text: string; done: boolean; truncated?: true })
  | (ItemBase & { kind: 'thinking'; text: string; done: boolean; truncated?: true })
  | (ItemBase & { kind: 'tool'; tool: ToolCall })
  | (ItemBase & { kind: 'plan'; steps: PlanStep[] })
  | (ItemBase & { kind: 'notice'; level: 'info' | 'warning' | 'error'; text: string });

export interface Usage {
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens?: number;
  reasoningTokens?: number;
}

export interface TurnState {
  id: string;
  status: 'running' | 'completed' | 'interrupted' | 'failed';
  startedAt: number;
  endedAt?: number;
  usage?: Usage;
  /** Claude only: this turn's share of the CLI's cumulative total_cost_usd. */
  costUsd?: number;
  error?: string;
  /** Per-turn file changes when the runtime reports them (OpenCode summary.diffs, Codex turn diff). */
  diffs?: FileDiff[];
}

export type LiveStatus = 'starting' | 'idle' | 'running' | 'waiting' | 'ended';

export interface LiveState {
  status: LiveStatus;
  permissionMode?: string;   // Claude: default | acceptEdits | plan | ...
  model?: string;
  /** ADR-017 (Claude): the models the CLI offers. */
  models?: Array<{ value: string; label: string }>;
  /** ADR-017 (Claude): slash command names, without the slash. */
  slashCommands?: string[];
  /** ADR-017 (Claude): context window use after the last turn. */
  context?: { usedTokens: number; maxTokens: number };
}

export type McpStatus = 'connected' | 'pending' | 'failed' | 'needs-auth' | 'disabled';

export interface McpServerState {
  /** Untrusted display text (comes from user/plugin config). */
  name: string;
  status: McpStatus;
  error?: string;
  /** Sign-in flows this server supports from the phone; [] = nothing to do or desktop only. */
  authFlows: Array<'desktop' | 'phone-callback' | 'claude-ai'>;
  auth?: {
    state: 'starting' | 'awaiting-browser' | 'awaiting-callback' | 'completing' | 'failed';
    /** Only for the phone-callback and claude-ai flows: the URL the phone opens. */
    url?: string;
    /** Phone-callback flow: the pasted URL must start with this, e.g. "http://localhost:44818/callback". */
    callbackPrefix?: string;
    error?: string;
  };
}

export type TranscriptOp =
  | { seq: number; op: 'item'; item: TranscriptItem }             // insert or replace by item.id
  | { seq: number; op: 'append'; id: string; field: 'text' | 'input' | 'output'; text: string }
  | { seq: number; op: 'turn'; turn: TurnState }                    // upsert by turn.id
  | { seq: number; op: 'state'; state: LiveState }
  | { seq: number; op: 'mcp'; servers: McpServerState[] };         // full replace

/* Permission additions (all optional; legacy fields stay filled for v2 clients) */

export interface PermissionOption {
  /** Adapter-defined, stable for this request. */
  id: string;
  /** G2 bucket; also what legacy clients send. */
  decision: PermissionDecision;
  /** Exact user-facing text built from native data, e.g. "Always allow `npm test` in this project". */
  label: string;
  /** Rule text a session or persistent grant adds, shown verbatim. */
  rule?: string;
  scope: 'once' | 'session' | 'persistent';
  /** Where a persistent grant is written, e.g. ".claude/settings.local.json". */
  destination?: string;
  /** The phone may attach a free-text reason the model sees. */
  takesMessage?: true;
  /** Choosing it also ends the current turn. */
  stopsTurn?: true;
}

export type PermissionDetail =
  | { kind: 'shell'; command: string; cwd?: string; reason?: string }
  | { kind: 'edit'; path: string; patch?: string }
  | { kind: 'read'; path: string }
  | { kind: 'fetch'; url: string }
  | { kind: 'mcp'; server: string; tool: string; input?: string }
  | {
      kind: 'question';
      questions: Array<{
        id: string;
        text: string;
        header?: string;
        options: Array<{ label: string; description?: string }>;
        multiSelect: boolean;
        allowFreeText: boolean;
      }>;
    }
  | { kind: 'plan'; plan: string }
  | { kind: 'generic'; tool: string; input?: string };

// PermissionRequest gains toolItemId / options / detail, Agent gains `live`
// and Session gains `transcript` (declared on those interfaces above).

/* Client → server additions */
export type LiveClientMessage =
  | { type: 'session_subscribe'; commandId: string; sessionId: string; epoch?: string; afterSeq?: number }
  | { type: 'session_unsubscribe'; commandId: string; sessionId: string }
  | { type: 'session_end'; commandId: string; sessionId: string }
  | { type: 'set_permission_mode'; commandId: string; sessionId: string; mode: 'default' | 'acceptEdits' | 'plan' }
  | { type: 'mcp_auth'; commandId: string; sessionId: string; server: string; flow: 'desktop' | 'phone-callback' }
  | { type: 'mcp_auth_callback'; commandId: string; sessionId: string; server: string; callbackUrl: string }
  | { type: 'mcp_reconnect'; commandId: string; sessionId: string; server: string } // §4.7 "Reconnect a failed server"
  // ADR-017 composer (Claude): model must pass isModelName, title isSessionTitle
  | { type: 'set_model'; commandId: string; sessionId: string; model: string }
  | { type: 'rename_session'; commandId: string; sessionId: string; title: string };
// ClientMessage = existing union | LiveClientMessage.
// permission_decision gains: optionId?: string; message?: string; answers?: Record<string, string[]>
//   (answers keyed by PermissionDetail.question.questions[].id)

/* Server → client additions */
export type LiveServerMessage =
  | { type: 'transcript'; sessionId: string; epoch: string; ops: TranscriptOp[] }
  | {
      type: 'transcript_snapshot';
      sessionId: string;
      epoch: string;
      seq: number;               // ops after this seq follow as `transcript`
      items: TranscriptItem[];
      truncatedBefore: boolean;  // older items exist only on the desktop
      turns: TurnState[];        // turns referenced by `items`
      state: LiveState;
      mcp: McpServerState[];
    };
// ServerMessage = existing union | LiveServerMessage.
// hello_ack gains: features?: string[]

/* ---------------- ADR-016 workspace files and git (additive; protocol stays 2) ---------------- */

/** Bounds shared with the Rust validator. */
export const WORKSPACE_PATH_MAX = 1024;
export const WORKSPACE_PATHS_MAX = 256;
export const WORKSPACE_MESSAGE_MAX = 4096;
export const WORKSPACE_ID_MAX = 64;
/** ADR-020: one terminal command line, in UTF-16 units. */
export const TERMINAL_COMMAND_MAX = 4096;

/**
 * `workspaceId` is an `Agent.live.workspaces[].id`; paths are relative to that
 * workspace (`/`-separated; '' = its root). Reads (fs_list, fs_read,
 * git_status, git_diff) are answered with a `workspace_result` followed by an
 * ack; never journaled. Mutations (git_stage, git_unstage, git_commit,
 * git_push, git_pull, terminal_run) get the full command lifecycle: ack on acceptance, then one
 * `command_result` (its `error` carries git's own summary).
 */
export type WorkspaceClientMessage =
  | { type: 'fs_list'; commandId: string; workspaceId: string; path?: string }
  | { type: 'fs_read'; commandId: string; workspaceId: string; path: string }
  | { type: 'git_status'; commandId: string; workspaceId: string }
  | { type: 'git_diff'; commandId: string; workspaceId: string; path?: string; staged?: boolean }
  | { type: 'git_stage'; commandId: string; workspaceId: string; paths: string[] }
  | { type: 'git_unstage'; commandId: string; workspaceId: string; paths: string[] }
  | { type: 'git_commit'; commandId: string; workspaceId: string; message: string }
  | { type: 'git_push'; commandId: string; workspaceId: string }
  /** ADR-016 addendum: fast-forward the current branch from its upstream (`--ff-only`). */
  | { type: 'git_pull'; commandId: string; workspaceId: string }
  /**
   * ADR-020 (only when `hello_ack.features` has FEATURE_TERMINAL): one
   * command line through the PC's shell, cwd = the workspace. Acked once it
   * runs; output arrives as `terminal_output` to this connection, then one
   * `terminal_exit`, then its `command_result`.
   */
  | { type: 'terminal_run'; commandId: string; workspaceId: string; command: string }
  /** ADR-020: kill the workspace's running command; ack only, never journaled. */
  | { type: 'terminal_stop'; commandId: string; workspaceId: string };

export interface FsEntry {
  name: string;
  kind: 'dir' | 'file';
  /** Bytes; 0 for folders. */
  size: number;
  mtime: number;
}

export interface GitFile {
  path: string;
  /** Porcelain letter: M A D R C T U. */
  status: string;
  /** Renames and copies. */
  origPath?: string;
}

export type WorkspaceResult =
  | { kind: 'fs_list'; path: string; entries: FsEntry[]; truncated: boolean }
  | {
      kind: 'fs_read';
      path: string;
      /** Size on disk; `content` holds at most 256 KiB of it. */
      size: number;
      truncated: boolean;
      /** Secret shapes were masked. */
      redacted: boolean;
      /** A `.env*` file: every value is masked. */
      sensitive?: boolean;
      content: string;
    }
  | {
      kind: 'git_status';
      /** null when HEAD is detached. */
      branch: string | null;
      upstream?: string;
      ahead: number;
      behind: number;
      staged: GitFile[];
      unstaged: GitFile[];
      untracked: string[];
      conflicted: string[];
      truncated: boolean;
    }
  | { kind: 'git_diff'; path?: string; staged: boolean; truncated: boolean; patch: string };

export type WorkspaceServerMessage =
  | {
      type: 'workspace_result';
      commandId: string;
      workspaceId: string;
      result: WorkspaceResult;
    }
  /** ADR-020: output (stdout+stderr, ANSI stripped, redacted); never journaled. */
  | { type: 'terminal_output'; commandId: string; workspaceId: string; data: string }
  | {
      type: 'terminal_exit';
      commandId: string;
      workspaceId: string;
      /** Set when `reason` is `exit` (null if the process had none). */
      exitCode: number | null;
      reason: 'exit' | 'timeout' | 'stopped';
      /** Output passed the 1 MiB cap; the rest was dropped. */
      truncated: boolean;
    };

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
      /** ADR-017 (Claude): images before the prompt text. */
      images?: PromptImage[];
      /** ADR-017 (Claude): `--model` at spawn; must pass isModelName. */
      model?: string;
    }
  | {
      type: 'submit_prompt';
      commandId: string;
      sessionId: string;
      text: string;
      images?: PromptImage[];
      /**
       * ADR-018 (Claude): continue a session from the PC as a NEW session
       * (`--resume <id> --fork-session`); the ack's sessionId names it.
       */
      fork?: boolean;
    }
  | { type: 'interrupt'; commandId: string; sessionId: string }
  | {
      type: 'permission_decision';
      commandId: string;
      permissionId: string;
      decision: PermissionDecision;
      /** Revision the user decided against; mismatch fails closed (G2). */
      revision: number;
      /** ADR-015: a PermissionOption.id; its `decision` must equal `decision`. */
      optionId?: string;
      /** ADR-015: free-text reason the model sees (options with `takesMessage`). */
      message?: string;
      /** ADR-015: keyed by PermissionDetail.question.questions[].id. */
      answers?: Record<string, string[]>;
    }
  | { type: 'list_state'; commandId: string }
  | LiveClientMessage
  | WorkspaceClientMessage;

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
      /** ADR-015: protocol extensions this server understands, e.g. FEATURE_TRANSCRIPT. */
      features?: string[];
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
    } & EventEnvelope)
  /** ADR-015: transcript frames reach only connections that subscribed; never journaled. */
  | LiveServerMessage
  /** ADR-016: a workspace read's answer, to the asking connection only, before its ack. */
  | WorkspaceServerMessage;

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

/** At most `max` UTF-16 units (JS length; the Rust validator counts the same units). */
function fits(v: unknown, max: number): v is string {
  return typeof v === 'string' && v.length <= max;
}

/** Non-negative safe integer (transcript seq, afterSeq). */
function isSeq(v: unknown): v is number {
  return Number.isSafeInteger(v) && (v as number) >= 0;
}

/** ADR-015 §3.4: an object of ≤ 8 question ids, each ≤ 16 answers of ≤ 1,024 chars. */
function isAnswers(v: unknown): boolean {
  if (!isRecord(v) || Array.isArray(v)) return false;
  const lists = Object.values(v);
  return (
    lists.length <= 8 &&
    lists.every((a) => Array.isArray(a) && a.length <= 16 && a.every((s) => fits(s, 1024)))
  );
}

/* ADR-017 composer bounds (Rust `protocol.rs` has the same constants). */

/** Images per prompt. */
export const PROMPT_IMAGES_MAX = 4;
/**
 * Base64 characters of all a prompt's images together (about 131 KiB of
 * JPEG): the whole prompt travels in one WebSocket frame of at most
 * 256,000 bytes, sealed as base64, so the phone also checks the frame.
 */
export const PROMPT_IMAGES_DATA_MAX = 180_000;
/** Session titles (UTF-16 units). */
export const SESSION_TITLE_MAX = 80;
export const PROMPT_IMAGE_TYPES = ['image/png', 'image/jpeg', 'image/webp', 'image/gif'] as const;
export type PromptImageType = (typeof PROMPT_IMAGE_TYPES)[number];
export interface PromptImage {
  mediaType: PromptImageType;
  /** Base64 (no data: prefix) that starts with the format's signature. */
  data: string;
}

/** Each format's first bytes as base64 (PNG, JPEG, GIF, RIFF for WebP). */
const IMAGE_MAGIC: Record<PromptImageType, string> = {
  'image/png': 'iVBORw0KGgo',
  'image/jpeg': '/9j/',
  'image/gif': 'R0lGOD',
  'image/webp': 'UklGR',
};
const MODEL_ALIASES: ReadonlySet<string> = new Set(['default', 'opus', 'sonnet', 'haiku', 'fable', 'opusplan']);

/** ADR-017: a Claude Code alias or a full `claude-…` id (lowercase, digits, dashes; ≤ 64). */
export function isModelName(v: unknown): v is string {
  return (
    typeof v === 'string' &&
    (MODEL_ALIASES.has(v) || (v.length <= 64 && /^claude-[a-z0-9-]+$/.test(v)))
  );
}

/** ADR-017: 1-80 UTF-16 units, not blank, no control characters. */
export function isSessionTitle(v: unknown): v is string {
  // eslint-disable-next-line no-control-regex
  return fits(v, SESSION_TITLE_MAX) && v.trim().length > 0 && !/[\u0000-\u001f\u007f-\u009f]/.test(v);
}

/** ADR-017: one image: a whitelisted type and strict base64 with that format's signature. */
export function isPromptImage(v: unknown): v is PromptImage {
  if (!isRecord(v) || typeof v.mediaType !== 'string' || typeof v.data !== 'string') return false;
  const magic = (IMAGE_MAGIC as Record<string, string | undefined>)[v.mediaType];
  const data = v.data;
  const body = data.replace(/=+$/, '');
  return (
    magic !== undefined &&
    data.length <= PROMPT_IMAGES_DATA_MAX &&
    data.length % 4 === 0 &&
    data.length - body.length <= 2 &&
    data.startsWith(magic) &&
    /^[A-Za-z0-9+/]*$/.test(body)
  );
}

/** ADR-017: at most 4 images whose data totals at most PROMPT_IMAGES_DATA_MAX characters. */
export function isPromptImages(v: unknown): v is PromptImage[] {
  return (
    Array.isArray(v) &&
    v.length <= PROMPT_IMAGES_MAX &&
    v.every(isPromptImage) &&
    v.reduce((n: number, im: PromptImage) => n + im.data.length, 0) <= PROMPT_IMAGES_DATA_MAX
  );
}

const CLIENT_TYPES: ReadonlySet<string> = new Set([
  'hello',
  'ping',
  'create_session',
  'submit_prompt',
  'interrupt',
  'permission_decision',
  'list_state',
  'session_subscribe',
  'session_unsubscribe',
  'session_end',
  'set_permission_mode',
  'mcp_auth',
  'mcp_auth_callback',
  'mcp_reconnect',
  'set_model',
  'rename_session',
  'fs_list',
  'fs_read',
  'git_status',
  'git_diff',
  'git_stage',
  'git_unstage',
  'git_commit',
  'git_push',
  'git_pull',
  'terminal_run',
  'terminal_stop',
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
  'transcript',
  'transcript_snapshot',
  'workspace_result',
  'terminal_output',
  'terminal_exit',
]);

function hasCommandId(v: Record<string, unknown>): boolean {
  return isStr(v.commandId);
}

/** Every event message must carry a well-formed envelope. */
function hasEnvelope(v: Record<string, unknown>): boolean {
  return isNum(v.seq) && isStr(v.eventId) && isNum(v.occurredAt);
}

/** ADR-016: a non-empty workspace path within bounds. */
function isPath(v: unknown): boolean {
  return isStr(v) && fits(v, WORKSPACE_PATH_MAX);
}

/** ADR-016: 1..256 non-empty paths. */
function isPaths(v: unknown): boolean {
  return Array.isArray(v) && v.length > 0 && v.length <= WORKSPACE_PATHS_MAX && v.every(isPath);
}

/** Validates an untrusted payload as a ClientMessage. */
export function isClientMessage(v: unknown): v is ClientMessage {
  if (!isRecord(v) || !isStr(v.type) || !CLIENT_TYPES.has(v.type)) return false;
  if (v.type.startsWith('fs_') || v.type.startsWith('git_') || v.type.startsWith('terminal_')) {
    // ADR-016: shape only; containment is the companion's job
    if (!hasCommandId(v) || !isStr(v.workspaceId) || !fits(v.workspaceId, WORKSPACE_ID_MAX)) return false;
  }
  switch (v.type) {
    case 'hello':
      return isStr(v.clientId) && isNum(v.protocol);
    case 'create_session':
      return (
        hasCommandId(v) &&
        isStr(v.agentId) &&
        isStr(v.title) &&
        isStr(v.prompt) &&
        (v.images === undefined || isPromptImages(v.images)) &&
        (v.model === undefined || isModelName(v.model))
      );
    case 'submit_prompt':
      // ADR-015 §3.4: 32,000 characters as the phone's zod counts them (UTF-16
      // units); the Rust companion counts chars, so it is never stricter
      return (
        hasCommandId(v) &&
        isStr(v.sessionId) &&
        isStr(v.text) &&
        fits(v.text, 32_000) &&
        (v.images === undefined || isPromptImages(v.images)) &&
        (v.fork === undefined || typeof v.fork === 'boolean')
      );
    case 'set_model':
      return hasCommandId(v) && isStr(v.sessionId) && isModelName(v.model);
    case 'rename_session':
      return hasCommandId(v) && isStr(v.sessionId) && isSessionTitle(v.title);
    case 'interrupt':
    case 'session_unsubscribe':
    case 'session_end':
      return hasCommandId(v) && isStr(v.sessionId);
    case 'permission_decision':
      return (
        hasCommandId(v) &&
        isStr(v.permissionId) &&
        isNum(v.revision) &&
        (v.decision === 'approve-once' ||
          v.decision === 'approve-session' ||
          v.decision === 'reject') &&
        (v.optionId === undefined || fits(v.optionId, 128)) &&
        (v.message === undefined || fits(v.message, 4096)) &&
        (v.answers === undefined || isAnswers(v.answers))
      );
    case 'list_state':
    case 'ping':
      return hasCommandId(v);
    case 'session_subscribe':
      return (
        hasCommandId(v) &&
        isStr(v.sessionId) &&
        // a fresh view's epoch '' is fine: it never matches, so a snapshot follows
        (v.epoch === undefined || fits(v.epoch, 64)) &&
        (v.afterSeq === undefined || isSeq(v.afterSeq))
      );
    case 'set_permission_mode':
      // whitelist: bypassPermissions / dontAsk / auto are never reachable from a phone
      return (
        hasCommandId(v) &&
        isStr(v.sessionId) &&
        (v.mode === 'default' || v.mode === 'acceptEdits' || v.mode === 'plan')
      );
    case 'mcp_auth':
      return (
        hasCommandId(v) &&
        isStr(v.sessionId) &&
        isStr(v.server) &&
        fits(v.server, 256) &&
        (v.flow === 'desktop' || v.flow === 'phone-callback')
      );
    case 'mcp_auth_callback':
      // shape only; host, port, path and state are checked by the adapter (§4.7)
      return (
        hasCommandId(v) &&
        isStr(v.sessionId) &&
        isStr(v.server) &&
        fits(v.server, 256) &&
        fits(v.callbackUrl, 4096) &&
        v.callbackUrl.startsWith('http://')
      );
    case 'mcp_reconnect':
      return hasCommandId(v) && isStr(v.sessionId) && isStr(v.server) && fits(v.server, 256);
    case 'fs_list':
      return v.path === undefined || fits(v.path, WORKSPACE_PATH_MAX);
    case 'fs_read':
      return isPath(v.path);
    case 'git_status':
    case 'git_push':
    case 'git_pull':
    case 'terminal_stop':
      return true;
    case 'terminal_run':
      // shape only; blank lines and line breaks are refused by the companion
      return isStr(v.command) && fits(v.command, TERMINAL_COMMAND_MAX);
    case 'git_diff':
      return (
        (v.path === undefined || fits(v.path, WORKSPACE_PATH_MAX)) &&
        (v.staged === undefined || typeof v.staged === 'boolean')
      );
    case 'git_stage':
    case 'git_unstage':
      return isPaths(v.paths);
    case 'git_commit':
      return isStr(v.message) && fits(v.message, WORKSPACE_MESSAGE_MAX);
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
        isNum(v.protocol) &&
        isNum(v.serverTime) &&
        isNum(v.lastSeq) &&
        hasSnapshot(v) &&
        (v.features === undefined ||
          (Array.isArray(v.features) && v.features.every((f) => typeof f === 'string')))
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
    case 'transcript':
      // op kinds stay open (a newer companion may add some); applyTranscript
      // skips what it does not know but still needs every seq
      return (
        isStr(v.sessionId) &&
        isStr(v.epoch) &&
        Array.isArray(v.ops) &&
        v.ops.every((o) => isRecord(o) && isSeq(o.seq) && isStr(o.op))
      );
    case 'transcript_snapshot':
      return (
        isStr(v.sessionId) &&
        isStr(v.epoch) &&
        isSeq(v.seq) &&
        Array.isArray(v.items) &&
        typeof v.truncatedBefore === 'boolean' &&
        Array.isArray(v.turns) &&
        isRecord(v.state) &&
        Array.isArray(v.mcp)
      );
    case 'workspace_result':
      // result kinds stay open (a newer companion may add some)
      return hasCommandId(v) && isStr(v.workspaceId) && isRecord(v.result) && isStr(v.result.kind);
    case 'terminal_output':
      return hasCommandId(v) && isStr(v.workspaceId) && typeof v.data === 'string';
    case 'terminal_exit':
      return (
        hasCommandId(v) &&
        isStr(v.workspaceId) &&
        (v.exitCode === null || isNum(v.exitCode)) &&
        (v.reason === 'exit' || v.reason === 'timeout' || v.reason === 'stopped') &&
        typeof v.truncated === 'boolean'
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
export * from './transcript';
export { createSeedSnapshot } from './seed';
export { MockEngine } from './engine';
