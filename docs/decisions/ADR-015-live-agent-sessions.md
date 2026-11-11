# ADR-015: Live agent sessions on the phone (streaming transcript, inline approvals, follow-ups, interrupt, MCP)

**Status:** Accepted 2026-10-26 (ratification delegated by the owner). Implemented on main 2026-11-04 (steps 1-10 and 12; deviations in §10). Step 11, the emulator rows 9.1-9.13, is pending until they are run.
**Date:** 2026-10-26
**Relates to:** PRD §7.2-7.5, §7.8, §12.5, §13, §14.4-14.5; gates G1, G2, G4.3 (amended in §4.2.1); ADR-009 (mock honesty), ADR-010 (std-only Rust), ADR-011 (E2E), ADR-014 (revocation); threat model `PHASE5-THREAT-MODEL.md` §3.1, §3.8, §3.10, §3.12 (T1-T11 as built); `apps/mobile/DESIGN.md`.
**Research this builds on:** `docs/reverse-engineering/claude-code-streaming.md`, `claude-agent-sdk.md`, `codex-and-antigravity.md`, `opencode-live-events.md` (all 2026-10-26).

---

## 1. Context

**What the phone gets today.** Session status, `session_output` lines (the mock engine's lines, and OpenCode text parts republished as activity), and a separate approval screen. Claude Code is a hook ingress only (`claude.rs`): it observes and cannot create, prompt, interrupt or approve. OpenCode is text-only and never streams (it ignores `message.part.delta`, `opencode-live-events.md` §2.4). Codex has no adapter.

**What the owner wants.** The phone should feel like the Claude Code VS Code extension: a live conversation with streaming text and thinking, tool calls with their inputs, outputs and diffs as they happen, approvals inside the conversation (not a separate "approve permission" app), follow-up prompts, a stop button, and MCP server status and sign-in.

**What the research found.** All three runtimes expose the same shape over a local channel, so one normalized model covers them:

| | Claude Code | Codex | OpenCode |
|---|---|---|---|
| Channel | Spawned CLI, NDJSON on stdio (the Agent SDK protocol; the VS Code extension uses nothing else) | Spawned `codex app-server`, JSON-RPC lines on stdio | Running `opencode serve`, HTTP + SSE (attach, as today) |
| Text / thinking | Token deltas / ~5-word summarized thinking deltas | Token deltas / reasoning summary deltas | Token deltas for both |
| Tool calls | Streaming input JSON; whole result with `structuredPatch` diffs | Typed items, streaming command output, unified diffs | pending → running → completed/error; diff in the edit permission and the turn summary |
| Approval | `control_request can_use_tool` keyed by `tool_use_id`; answer `control_response` | Server→client request keyed by `itemId`; answer `{decision}` | `permission.asked` keyed by `tool.callID`; `POST /permission/{id}/reply` |
| Follow-up while busy | Usually its own next turn (merged only sometimes, at a tool boundary) | `turn/steer` (merged) | Queued as its own turn |
| Interrupt | `control_request interrupt`; process survives | `turn/interrupt` | `POST /session/{id}/abort` |
| MCP | `mcp_status` (poll), `mcp_authenticate` → URL, `mcp_oauth_callback_url` | `mcpServerStatus/list`, `mcpServer/oauth/login` → URL | `GET /mcp` (poll), `POST /mcp/{name}/auth`, `/auth/callback` |
| Verifiable here | Yes (CLI 2.1.310, Haiku) | No (not installed) | Yes (1.19.4 + local Ollama) |

Antigravity (`agy`) streams activity but cannot ask for approval in headless mode, and its desktop API is an internal, CSRF-guarded protobuf service (`codex-and-antigravity.md` §B3, §B5). It is deferred (§8).

**Constraints this design must respect.**

1. `PROTOCOL_VERSION` is strict-equality gated (D7). A bump to 3 would lock out every existing phone and companion, so the extension must be additive inside v2.
2. Engine events are journaled before broadcast, the journal keeps 5,000 events, and `PRIVACY.md` promises the companion does not store conversation history. Token deltas cannot go through that pipeline.
3. Each connection's bulk queue is 192 frames / 3 MiB, and overflow tears the connection down (D8). A raw token stream would trip it on a slow phone.
4. Inbound WS frames are capped at 256,000 bytes; sealed frames are base64 (×4/3).
5. Every frame after `hs_ok` goes through `Outbound::enqueue_sealed`; at remote posture a sessionless connection gets nothing (D2, D9).
6. G2: permissions stay native, revisioned and fail-closed; the engine is the single authority.
7. G4.3: adapters attach rather than spawn a second runtime on a shared store. Driving Claude and Codex needs a narrow amendment (§4.2.1).
8. Std-only Rust (ADR-010): child processes, pipes, threads and the in-crate JSON parser are all the tooling there is.

---

## 2. Decision summary

1. **Additive protocol, still v2.** Two new server messages (`transcript`, `transcript_snapshot`), seven new client messages, optional fields on `Agent`, `Session`, `PermissionRequest` and `permission_decision`, and `hello_ack.features: ["transcript.v1"]` so a new phone can tell whether the companion supports any of it. Old phones never receive transcript frames, because those go only to connections that subscribed.
2. **Transcript = per-session item log with five ops** (`item`, `append`, `turn`, `state`, `mcp`). It lives in companion memory only: a bounded ring for replay plus a compacted item list for snapshots, each session with its own `epoch` and `seq`. It is never journaled.
3. **One code path for live streaming and replay.** Each subscription is `(connection, session, cursor)`. The existing 50 ms scheduler flushes coalesced deltas and sends each subscriber what lies after its cursor. A reconnecting phone re-subscribes with its last `seq`; if that is still in the ring it gets the missing ops, otherwise a snapshot. The pump skips any connection whose bulk queue is over half full, so transcripts can never cause a D8 teardown by themselves.
4. **Permissions stay engine records (G2)** and gain `toolItemId`, `options` (the runtime's own choices, labelled verbatim) and `detail` (command, diff, questions, plan). A decision may name an `optionId`. Legacy decisions map only to non-persistent options. A request the runtime withdraws or answers elsewhere is resolved `stale` with a revision bump, so a late tap fails closed.
5. **Claude Code is driven by spawning the CLI** with the SDK's stream-json control protocol, std-only. Claude's own permission engine decides everything: `--permission-prompt-tool stdio`, never `bypassPermissions`, and the suggestion filter the VS Code extension applies.
6. **Codex through `codex app-server` over stdio**, implemented from the pinned schema and tested against fake processes. Its capabilities stay `limited` until a live run (`INTEGRATIONS.md` rule).
7. **OpenCode stays attach-only** and gains rich parts: deltas, reasoning, tool cards, permissions linked by `callID`, questions, todos, retry notices, interrupted (not failed) aborts, and a real `command_result` at turn end.
8. **MCP:** per-session status chip. Sign-in either opens the runtime's own authorization URL in the desktop browser (all three runtimes) or relays a pasted callback URL from the phone (Claude, OpenCode), which the companion validates and never journals. Adding servers, credentials, claude.ai connectors and account login stay on the desktop.
9. **Phone:** a conversation view (streaming markdown, collapsible tool cards with diffs, approve/deny inside the card, composer that keeps working while the agent runs, stop button, MCP chip). The Approvals tab becomes an inbox that deep-links into the conversation.
10. **Security:** no new listener, no protocol or crypto change, E2E only, memory-only transcripts, secret redaction, size caps, child-environment scrubbing, and an operator-set workspace allow-list that doubles as the folder-trust decision.

Alternatives rejected: journaling the transcript (write amplification, 5,000-event eviction, privacy promise); extending `session_output` lines (no structure for tool cards, diffs or inline approvals); bumping the protocol to 3 (D7 locks out old peers); Claude hooks for control (one-way, no prompt injection, no streaming); Claude Remote Control and `agy --remote-control` (cloud relays tied to vendor accounts, while CHERNOBYL is local, E2E and multi-runtime); Antigravity's language-server RPC (internal and unversioned).

---

## 3. Protocol extension (a)

### 3.1 Compatibility rules

- `PROTOCOL_VERSION` stays `2`; the D7 gate is untouched.
- New server message types are ignored by old phones (`parseServerMessage` returns null and both transports drop it: `packages/e2e/src/transport.ts` handleSealed, `wsTransport.ts`). They are also never sent to them, because only `session_subscribe` creates a transcript stream.
- New optional fields on existing entities are ignored by old validators (`isServerMessage` checks required fields only).
- New client message types sent to an old companion get the existing `malformed` ack. A new phone uses them only when `hello_ack.features` contains `"transcript.v1"`.
- `transcript` messages are deliberately **not** host events: they carry no `EventEnvelope`, have their own per-session `epoch`/`seq`, and are not journaled. Durable state (sessions, permissions, command lifecycle, activity) keeps flowing through the journaled host `seq` exactly as today.

### 3.2 TypeScript (`packages/protocol/src/index.ts`, additive)

```ts
/* ---------------- ADR-015 live sessions (additive; protocol stays 2) ---------------- */

export const FEATURE_TRANSCRIPT = 'transcript.v1';

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
  | (ItemBase & { kind: 'user'; text: string; queued?: true })
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

// PermissionRequest gains:
//   toolItemId?: string;            transcript tool item this request belongs to
//   options?: PermissionOption[];   never empty when present; always contains a 'reject'
//   detail?: PermissionDetail;

// Agent gains:
//   live?: {
//     transcript: true;
//     promptWhileBusy: 'merge' | 'queue' | 'none';
//     interrupt: boolean;
//     end: boolean;                         // session_end supported (Claude)
//     permissionModes?: string[];           // Claude: ['default', 'acceptEdits', 'plan']
//     workspaces?: Array<{ id: string; label: string; path: string }>; // spawning runtimes only
//   };

// Session gains:
//   transcript?: true;   present only when a transcript stream exists for it

/* Client → server additions */
export type LiveClientMessage =
  | { type: 'session_subscribe'; commandId: string; sessionId: string; epoch?: string; afterSeq?: number }
  | { type: 'session_unsubscribe'; commandId: string; sessionId: string }
  | { type: 'session_end'; commandId: string; sessionId: string }
  | { type: 'set_permission_mode'; commandId: string; sessionId: string; mode: 'default' | 'acceptEdits' | 'plan' }
  | { type: 'mcp_auth'; commandId: string; sessionId: string; server: string; flow: 'desktop' | 'phone-callback' }
  | { type: 'mcp_auth_callback'; commandId: string; sessionId: string; server: string; callbackUrl: string }
  | { type: 'mcp_reconnect'; commandId: string; sessionId: string; server: string }; // §4.7 "Reconnect a failed server" (added in step 1)
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
```

`session_subscribe` and `session_unsubscribe` are non-mutating: no journal row, answered with an `ack`. `session_end`, `set_permission_mode`, `mcp_auth`, `mcp_auth_callback` and `mcp_reconnect` are mutating: persisted before execution, deduplicated by `commandId`, and given the full G1 lifecycle (§4.6). `mcp_auth_callback` is persisted with `callbackUrl` replaced by `"<redacted>"`, because it carries a one-time authorization code.

The pure reducer `applyTranscript(view, msg) → { view, resubscribe }` lives in a new `packages/protocol/src/transcript.ts`, so the existing TS test runner covers it and the app imports it. Its rules: a snapshot replaces the view; a batch with a different `epoch` returns `resubscribe: true` and is not applied; ops with `seq <= view.seq` are skipped as duplicates, and the first remaining op must be `view.seq + 1`, otherwise `resubscribe: true`; `append` to an unknown item id is ignored (it was trimmed out of a snapshot); unknown item kinds are kept but not rendered.

### 3.3 Rust (`services/companion-rs`)

`src/protocol.rs`, additive:

```rust
// ClientMsg gains (validators mirror the TS ones field for field):
SessionSubscribe   { command_id: String, session_id: String, epoch: Option<String>, after_seq: Option<u64> },
SessionUnsubscribe { command_id: String, session_id: String },
SessionEnd         { command_id: String, session_id: String },
SetPermissionMode  { command_id: String, session_id: String, mode: String },   // whitelisted
McpAuth            { command_id: String, session_id: String, server: String, flow: String }, // whitelisted
McpAuthCallback    { command_id: String, session_id: String, server: String, callback_url: String },
McpReconnect       { command_id: String, session_id: String, server: String },
// PermissionDecision gains:
//   option_id: Option<String>, message: Option<String>, answers: Vec<(String, Vec<String>)>
// is_mutating(): SessionEnd | SetPermissionMode | McpAuth | McpAuthCallback | McpReconnect = true;
//                SessionSubscribe | SessionUnsubscribe = false.

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionOption {
    pub id: String,
    pub decision: String,           // approve-once | approve-session | reject
    pub label: String,
    pub rule: Option<String>,
    pub scope: &'static str,        // once | session | persistent
    pub destination: Option<String>,
    pub takes_message: bool,
    pub stops_turn: bool,
}

// PermissionRequest gains (serialized only when present, like `simulated`):
//   pub tool_item_id: Option<String>,
//   pub options: Vec<PermissionOption>,
//   pub detail: Option<Json>,      // built by detail_shell()/detail_edit()/... helpers in transcript.rs

// ServerMsg gains:
Transcript { session_id: String, epoch: String, ops: Vec<String> },  // ops pre-serialized once in the ring
TranscriptSnapshot { session_id: String, epoch: String, seq: u64, body: Json },
// HelloAck gains `features: &'static [&'static str]` = &["transcript.v1"].
```

`src/transcript.rs` (new):

```rust
pub enum ItemKind { User, Text, Thinking, Tool, Plan, Notice }

pub enum ToolStatus { Pending, Running, AwaitingApproval, Completed, Failed, Denied, Cancelled }

pub struct FileDiff { pub path: String, pub status: &'static str, pub additions: i64,
                      pub deletions: i64, pub patch: String, pub truncated: bool }

pub struct PlanStep { pub text: String, pub status: &'static str }

pub struct ToolCall {
    pub name: String, pub title: Option<String>, pub status: ToolStatus,
    pub input: String, pub output: String, pub output_truncated: bool,
    pub error: Option<String>, pub diffs: Vec<FileDiff>,
    pub permission_id: Option<String>, pub auto_approved: Option<String>,
    pub duration_ms: Option<i64>,
}

pub struct Item {
    pub id: String, pub kind: ItemKind, pub ts: i64,
    pub turn_id: Option<String>, pub parent_id: Option<String>,
    pub text: String,              // user / text / thinking / notice body
    pub done: bool, pub truncated: bool, pub queued: bool,
    pub level: Option<&'static str>,   // notice
    pub tool: Option<ToolCall>,        // kind == Tool
    pub steps: Vec<PlanStep>,          // kind == Plan
}

pub enum Field { Text, Input, Output }

pub struct TurnState { pub id: String, pub status: &'static str, pub started_at: i64,
                       pub ended_at: Option<i64>, pub usage: Option<Usage>,
                       pub cost_usd: Option<f64>, pub error: Option<String>, pub diffs: Vec<FileDiff> }
pub struct Usage { pub input: i64, pub output: i64, pub cache_read: Option<i64>, pub reasoning: Option<i64> }
pub struct LiveState { pub status: &'static str, pub permission_mode: Option<String>, pub model: Option<String> }
pub struct McpServer { pub name: String, pub status: &'static str, pub error: Option<String>,
                       pub auth_flows: Vec<&'static str>, pub auth: Option<McpAuthState> }
pub struct McpAuthState { pub state: &'static str, pub url: Option<String>,
                          pub callback_prefix: Option<String>, pub error: Option<String> }

pub enum Op { Item(Item), Append { id: String, field: Field, text: String },
              Turn(TurnState), State(LiveState), Mcp(Vec<McpServer>) }

struct Transcript {
    epoch: String,                     // 16 hex chars from ffi::csprng_bytes(8), minted per transcript
    seq: u64,                          // last assigned op seq (starts at 0; first op is 1)
    ring: VecDeque<(u64, String)>,     // (seq, op JSON); bounded by RING_OPS / RING_BYTES
    ring_bytes: usize,
    items: Vec<Item>,                  // compacted state, bounded by ITEMS_MAX / ITEMS_BYTES
    index: HashMap<String, usize>,
    pending: Vec<(String, Field, String)>, // coalesced deltas awaiting the next flush
    turns: Vec<TurnState>, state: LiveState, mcp: Vec<McpServer>,
    truncated_before: bool, last_activity: i64, ended_at: Option<i64>,
}

struct Sub { conn: u64, out: Arc<Outbound>, session: String,
             epoch: String, cursor: u64, last_snapshot_at: i64 }

pub struct Hub { sessions: HashMap<String, Transcript>, subs: Vec<Sub> }

impl Hub {
    /// item/turn/state/mcp: flush this item's pending deltas first, apply to state, append to ring.
    pub fn push(&mut self, session: &str, op: Op);
    /// Streaming delta: redacted and capped, merged into `pending`, flushed by `pump`.
    pub fn append(&mut self, session: &str, id: &str, field: Field, text: &str);
    pub fn subscribe(&mut self, conn: u64, out: Arc<Outbound>, session: &str,
                     resume: Option<(String, u64)>) -> Result<(), &'static str>;
    pub fn unsubscribe(&mut self, conn: u64, session: Option<&str>); // None = connection gone
    /// Every scheduler tick: flush pending deltas (every 2nd tick), then for each sub with
    /// bulk headroom send ops after its cursor (or a snapshot), GC ended transcripts.
    pub fn pump(&mut self, now: i64);
    pub fn end_session(&mut self, session: &str, now: i64);
}
```

The hub is a **leaf lock**: no other companion lock is taken while it is held. The one exception is `Outbound`'s internal session lock inside `enqueue_sealed`, which is itself a leaf. Adapters call the hub outside the engine lock. A shared `Arc<Mutex<Hub>>` lives in `Shared`.

`src/adapters.rs` gains the runtime trait. There are now three implementations, so the trait is justified:

```rust
pub trait LiveRuntime: Send + Sync {
    fn id(&self) -> &str;
    fn is_healthy(&self) -> bool;
    fn agents(&self) -> Vec<Agent>;
    fn sessions(&self) -> Vec<Session>;
    fn owns_session(&self, session_id: &str) -> bool;
    fn knows_permission(&self, permission_id: &str) -> bool;
    /// Acceptance only (G1/G4.1): create/prompt/interrupt/end/decision/mode/mcp.
    fn send(&self, msg: &ClientMsg) -> Result<ServerMsg, AdapterError>;
    /// First subscriber arrived: hydrate history (OpenCode), start MCP polling.
    fn on_subscribe(&self, _session_id: &str) {}
    fn capabilities(&self) -> Vec<Capability>;
    fn dispose(&self);
}
```

### 3.4 Validation at the trust boundary (TS `isClientMessage`, Rust `parse_client_message`)

| Field | Rule |
|---|---|
| `sessionId`, `server` | non-empty string; `server` ≤ 256 chars |
| `afterSeq` | finite, non-negative integer when present |
| `epoch` | ≤ 64 chars when present |
| `mode` | `default` \| `acceptEdits` \| `plan` (never `bypassPermissions`, `dontAsk`, `auto`) |
| `flow` | `desktop` \| `phone-callback` |
| `callbackUrl` | ≤ 4,096 chars, starts with `http://` (full check in the adapter, §4.7) |
| `optionId` | ≤ 128 chars |
| `message` | ≤ 4,096 chars |
| `answers` | object, ≤ 8 keys, each an array of ≤ 16 strings of ≤ 1,024 chars |
| `submit_prompt.text` | ≤ 32,000 characters (TS string length; Rust char count), so the companion is never stricter than the phone (the phone's zod limit rises from 2,000 to 32,000) |

Everything else follows the existing strictness: non-empty required strings, duplicate keys rejected, depth ≤ 32. Hostile-corpus tests extend `protocol.rs`.

### 3.5 Sequencing, replay and bounds

- **Epoch and seq.** Each session transcript gets a fresh random `epoch` when the companion creates its state (boot, first event, or re-creation after LRU eviction) and a `seq` that starts at 1. A restart therefore never silently resets a sequence (PRD §14.3): the epoch changes and the phone resynchronizes.
- **Subscribe.** If `epoch` matches and every op after `afterSeq` is still in the ring, the companion replays those ops. Otherwise it sends `transcript_snapshot` (newest items that fit the snapshot cap, `truncatedBefore` set when older items were dropped), then live ops after the snapshot's `seq`. Subscribing again to the same session on the same connection only replaces the cursor, so the transport's replay of an in-flight `session_subscribe` and the hook's own re-subscribe after a reconnect cannot double the stream.
- **Live.** Each `transcript` message holds contiguous ops. The phone checks that the first op it has not seen is `lastSeq + 1`, and on a gap or epoch change re-subscribes (the reducer returns `resubscribe`). FIFO per connection means gaps occur only after a lag snapshot.
- **Coalescing.** `append` deltas for the same `(item, field)` merge in `pending` and are flushed into the ring every 100 ms (every second tick of the existing 50 ms scheduler). Any `push` for an item flushes that item's pending deltas first, so an authoritative `item` replace can never be followed by a stale delta.
- **Backpressure.** The pump sends to a connection only while its bulk depth is ≤ 96 (half of 192) and the frame fits in half the bulk byte budget. Otherwise it skips that tick, and the cursor waits. A subscriber that falls behind the ring start gets a snapshot once it drains. `Outbound` gains `bulk_depth()` and `bulk_bytes()` accessors (the `Depth` counters already exist).
- **Reconnect.** The phone re-sends `session_subscribe` with its `epoch` and last `seq` whenever transport status returns to `connected`. Nothing new is needed in the transport.

| Bound | Value |
|---|---|
| Delta flush interval | 100 ms |
| Text per `append` op | ≤ 8 KiB (longer text is split) |
| One `transcript` message | ≤ 64 KiB serialized, ≤ 256 ops |
| Replay ring per session | 2,048 ops or 1 MiB, oldest evicted (about 3 minutes of streaming) |
| Compacted items per session | 400 items or 2 MiB, oldest evicted (`truncatedBefore`) |
| Snapshot | ≤ 512 KiB serialized, newest items first |
| Sessions with transcript state | 8 (LRU on last activity; ended sessions dropped after 1 h) |
| Subscriptions per connection | 4 (a fifth evicts the oldest) |
| Snapshots | ≤ 1 per second per (connection, session) |
| Assistant text item | 256 KiB, then `truncated` and further appends dropped |
| Thinking item | 64 KiB |
| Tool input | 16 KiB |
| Tool output | 32 KiB (16 KiB head + 16 KiB tail, `outputTruncated`) |
| Diff | 64 KiB per file, 16 files per tool item or turn |
| Notice | 2 KiB |
| Runtime NDJSON / JSON-RPC line | 16 MiB (§4.2.4) |

Worst-case transcript memory is 8 × (1 + 2) MiB = 24 MiB, against today's ~5 MB RSS. A typical phone session holds a few hundred KB.

### 3.6 Native → normalized mapping

Item ids: Claude text/thinking = `<message.id>#<block index>`, Claude tool = `tool_use.id`; Codex = `item.id`; OpenCode text/reasoning = `part.id`, OpenCode tool = `callID` (the key permissions refer to). The permission's `toolItemId` always equals the tool item's id.

**Claude Code** (stdout frames; sources: `claude-code-streaming.md` §2-6, `claude-agent-sdk.md` §2-5):

| CLI frame | Normalized | Notes |
|---|---|---|
| (spawned) | `state {status: starting}` | Until the `initialize` response |
| `initialize` response | `state {status: idle, permissionMode: current_permission_mode}` | `account` (email) dropped entirely; `pending_permission_requests[]` re-ingested as permissions |
| `system/init` (every turn) | `state {model, permissionMode}`; `mcp` from `mcp_servers` | Never a reset |
| `system/status {permissionMode}` | `state {permissionMode}` | |
| `system/session_state_changed` | `state {status}`: running → running, requires_action → waiting, idle → idle; `Session.status` journaled on change | Enabled by `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1` |
| `command_lifecycle {command_uuid}` | user item `queued` on/off; `command_progress running` at `started` | `command_uuid` = the uuid we sent (§4.6) |
| `stream_event content_block_start` | `item` text / thinking / tool (`status: pending`, `name`) | `server_tool_use` → tool |
| `text_delta`, `thinking_delta` | `append text` | Coalesced |
| `input_json_delta` | `append input` (raw fragment) | Replaced by the final input on `assistant` |
| `signature_delta`, `citations_delta` | dropped | Never displayed |
| `assistant` (one block) | `item` authoritative replace (text/thinking `done`, tool input pretty-printed + `title`) | `parent_tool_use_id` → `parentId`; `aborted` → done; `error: authentication_failed` etc. → `notice error` ("Claude Code is not signed in on the desktop") |
| `message_delta {usage}` | accumulated into the turn's usage | |
| `control_request can_use_tool` | PermissionRequest (§4.5) + tool `awaiting-approval`, `permissionId` | `AskUserQuestion` → `detail.question`; `ExitPlanMode` → `detail.plan` |
| `control_cancel_request` | engine `resolve_native(stale)`; the tool shows `cancelled` when the CLI's synthetic tool_result follows | Never answered afterwards |
| `user` tool_result | tool `completed` / `failed` / `denied` (`is_error` + `permission_decision.source = user_reject`); `output` from `tool_use_result.stdout/stderr` or the content text; `diffs` from `structuredPatch` (rendered to unified text) or Write `create` (all lines added); `autoApproved` from `tool_result_meta[].permission_decision.source` (`config`, `mode`, `hook`) | |
| `user` text `[Request interrupted by user…]` | `notice info "Stopped"` | |
| `result` | `turn` end: `success` → completed; `error_during_execution` + `terminal_reason aborted_*` → interrupted; other errors → failed. `costUsd` = `total_cost_usd` − previous total (it is cumulative) | Closes the turn's prompt commands (G1) |
| `TodoWrite` tool input `todos[]` | `item plan` (id `plan`) upserted; the TodoWrite tool card itself is not shown | `content`/`status` map directly |
| `system/api_retry` | `notice warning "Retrying…"` | |
| `system/compact_boundary` | `notice info "Context compacted"` | |
| `system/session_title_changed` | `Session.title` (journaled) | If present |
| `rate_limit_event` with `status ≠ allowed` | `notice warning`, once per window | |
| `control_request` `hook_callback`, `mcp_message`, `request_user_dialog`, unknown | `control_response {subtype: "error", error: "unsupported by host"}` | An unanswered request blocks the CLI |
| `control_request elicitation` | `{action: "decline"}` + `notice` | MCP forms are a later step |
| `keep_alive`, `transcript_mirror`, `prompt_suggestion`, other | ignored | |

**Codex app-server** (JSON-RPC; schema pin openai/codex `970111f`, `codex-rs/app-server-protocol/schema/typescript/`):

| Message | Normalized |
|---|---|
| `thread/start` result | Session (id = `thread.id`), `state idle` |
| `turn/started` | `turn running` (id = `turn.id`); Session running |
| `item/started` agentMessage + `item/agentMessage/delta` | `item text` + `append text` |
| `item/started` reasoning + `item/reasoning/summaryTextDelta` | `item thinking` + `append text` (raw `textDelta` ignored when summaries arrive) |
| `item/started` commandExecution + `…/outputDelta` | `item tool {name: shell, title: command, status: running}` + `append output` (line-buffered, redacted, forwarding stops at 32 KiB) |
| `item/started` / `completed` fileChange | `item tool {name: apply_patch, diffs: changes[].diff}` |
| `item/started` / `completed` mcpToolCall | `item tool {name: <server>/<tool>, input: arguments, output: result, error}` |
| other ThreadItems (webSearch, imageView, …) | `item tool` generic; `contextCompaction` → `notice` |
| `item/completed` (any) | authoritative `item` replace; status `completed` / `failed` / `declined` → denied; output = head + tail of `aggregatedOutput` |
| `turn/plan/updated` | `item plan` (id `plan:<turnId>`), `inProgress` → `in_progress` |
| `turn/diff/updated` | `turn.diffs` (split on `diff --git` headers) |
| `thread/tokenUsage/updated` | turn usage from `last` |
| `error {willRetry}` | `notice` warning (retrying) or error |
| `turn/completed` | `turn` end: completed / interrupted / failed (+ `turn.error.message`) |
| `item/commandExecution/requestApproval`, `item/fileChange/requestApproval` | PermissionRequest (`toolItemId = itemId`) |
| `item/tool/requestUserInput` | PermissionRequest `detail.question`; answer `{answers: {[id]: {answers: [...]}}}` |
| `serverRequest/resolved` | engine `resolve_native(stale)` if still pending (another client answered) |
| `mcpServer/startupStatus/updated`, `mcpServer/oauthLogin/completed` | re-list `mcpServerStatus/list` → `mcp` |
| `item/permissions/requestApproval` | Local intervention (PRD §13.5): answer `{permissions: {}, scope: "turn"}` (grant nothing) + `notice` "grant extra sandbox permissions on the desktop" |
| `mcpServer/elicitation/request` | `{action: "decline", content: null, _meta: null}` + `notice` |
| any other server request | JSON-RPC error `-32601` (an unanswered request blocks the turn) |

Status values: `notStarted`/`starting` → pending, `authenticationRequired` → needs-auth, `cancelled` → failed.

**OpenCode** (`/global/event` payloads; source `opencode-live-events.md` §2-5):

| Event | Normalized |
|---|---|
| `message.updated` | records `messageID → role`; a user message becomes `item user` (id = messageID) |
| `message.part.updated` text, role user | `item user` text. **Never republished as Activity** (fixes the echo, and stops journaling conversation text) |
| `message.part.updated` text / reasoning, role assistant | `item text` / `item thinking` (created empty, full text on the final update) |
| `message.part.delta {field: "text"}` | `append text` on `partID` (type from the part map) |
| `message.part.updated` tool | `item tool` (id = callID): pending → pending, running → running (repeated `metadata.output` → output), completed → completed, error → failed or denied (when it follows a `reject` for that callID); `diffs` from `state.metadata.diff` when present |
| `message.part.updated` step-finish | turn usage and cost accumulated |
| user `message.updated` with `summary.diffs`, or `GET /session/{id}/diff?messageID=` | `turn.diffs` and `Session.files` |
| `session.status busy` | `turn running` (id = the user messageID); Session running |
| `session.status retry` | `notice warning "Retrying: <message>"` (+ `action.label`/`link` as text) |
| `session.status idle` / `session.idle` (idempotent; arrives twice) | `turn` completed + the prompt's `command_result completed` (today no success outcome is ever emitted, a G1 gap) |
| `session.error MessageAbortedError` | `turn interrupted`; prompt command `cancelled`; interrupt command `completed` |
| `session.error` other | `turn failed`; prompt command `failed` (existing G4.1 path) |
| `permission.asked` | PermissionRequest (`toolItemId = tool.callID`, `detail` from `metadata.diff` / `metadata.command`) + tool `awaiting-approval` |
| `permission.replied` (`requestID`) | engine `resolve_native` when the request is still pending, i.e. it was answered in the OpenCode TUI (after our own reply it is already gone from the pending map) |
| `question.asked` / `replied` / `rejected` | PermissionRequest `detail.question` / `resolve_native` |
| `todo.updated` | `item plan` |
| `session.compacted` | `notice` |
| `tui.toast.show` titled "MCP…", `mcp.tools.changed` | poll `GET /mcp` → `mcp` |
| `mcp.browser.open.failed {url}` | `mcp` auth `url` (phone flow) |
| `sync` twins, heartbeats, everything else | ignored |

Status values: `needs_auth` → needs-auth, `needs_client_registration` → failed ("configure the client on the desktop").

---

## 4. Adapter work (b)

### 4.1 Shared pieces

- **Router.** `Companion` holds `runtimes: Vec<Arc<dyn LiveRuntime>>` in place of the single OpenCode slot. `create_session` routes by the agent's `hostId` (= adapter id, the OpenCode precedent); session-scoped commands route to `owns_session`; `permission_decision` to `knows_permission`; anything else stays with the engine. The snapshot drops engine demo sessions and agents when any live runtime is healthy (the existing ADR-009 rule, extended to all runtimes), then concatenates every runtime's lists. A session id claimed by two runtimes is refused and logged. Native ids are not assumed globally unique (PRD §8.3): Claude and Codex permission ids are `<adapterId>:<nativeId>`. OpenCode keeps `per_…` to avoid churn.
- **Engine (G2).** `evaluate_permission_decision` gains `option_id`, `message` and `answers`, and returns the selected option, which the adapter answers with. When `option_id` is given, the option must exist and its `decision` must equal the decision sent. Without it, a request that has options needs a **legacy-selectable** option for that decision: the first with `scope ≠ persistent` (`takesMessage` does not disqualify it, because the message is optional). `message` is accepted only when the selected option has `takesMessage`, or for a reject on a request without options. `answers` are accepted only for a `question` detail: known question ids, one answer unless `multiSelect`, labels from the options unless `allowFreeText`. Anything else is refused as unsupported. `supportedDecisions` is derived on ingest from exactly the legacy-selectable buckets, so an old phone never shows a button that would map to a persistent rule. New `resolve_native(permission_id, status, decision)` covers resolutions that happen natively (withdrawn, answered in another UI, session ended): it bumps the revision, updates the stored record and broadcasts once. It is idempotent, and records only a well-formed `approved` or `rejected` as such; anything else becomes `stale`. A `permission_resolved` that an adapter publishes takes the same path. This also fixes today's gap where an OpenCode request answered in the TUI stays `pending` in the engine. `LIVE_PERMISSION_TTL_MS = 24 h` for runtime requests: none of the three runtimes expires a request (PRD §13.4: presentation expiry is tracked separately). `@chernobyl/domain`'s `evaluatePermissionDecision` changes in parity.
  *Wire status (step 3):* native resolutions go out as `stale`, not as a new value. `stale` has been in the v2 `PermissionStatus` union since the first protocol commit, and both engines already send it for expiry. `isServerMessage` checks only `permission.id` and the envelope of a `permission_resolved`, so a pre-ADR-015 phone upserts the record and stops showing it as pending. No `resolution` field is added; TS and Rust match.
- **Child process helper** (`ChildIo`, in `adapters.rs`), used by Claude and Codex. It spawns an explicit binary with `Command` (no shell, `CREATE_NO_WINDOW` through `std::os::windows::process::CommandExt`) and starts three threads: a stdout line reader (§4.2.4 caps), a stderr drain that keeps the last 4 KiB for diagnostics, and a writer fed by a `sync_channel(64)`. A full channel refuses the command ("runtime is not reading input"), so a stuck child can never block the WS read loop. The child environment is the companion's minus `HOST_TOKEN` and every `CHERNOBYL_*` variable, and minus `NODE_OPTIONS`. Prompts travel only on stdin, never in argv. The protocol cores of both drivers take any `BufRead` + `Write` pair; production hands them the child's pipes and unit tests hand them `std::io::pipe()` ends with a scripted peer.
- **Desktop browser opener** (`open_url_on_desktop`): accepts only `http`/`https` URLs without spaces, quotes or control characters, and runs `rundll32.exe url.dll,FileProtocolHandler <url>` as an argument vector. `cmd /c start` is never used, because cmd parses `&` in query strings.
- **Redaction** (`transcript::redact`), applied to tool input, output, diffs, notices, permission detail and context, and to final text blocks: `sk-ant-…`, `sk-…`/`sk-proj-…`, `ghp_`/`gho_`/`ghs_`/`github_pat_`, `xox[abprs]-`, `AKIA` + 16 chars, `AIza` + 35 chars, JWTs (`eyJ….eyJ….…`), `-----BEGIN … PRIVATE KEY-----` blocks, `Authorization: Bearer <tok>`, and `KEY=value` / `"key": "value"` where the key contains `SECRET`, `TOKEN`, `PASSWORD`, `PASSWD`, `API_KEY`, `PRIVATE_KEY` or `CREDENTIAL`. Matches become `«redacted:<kind>»`. It is a hand-written scanner (no regex crate), UTF-8 safe and idempotent.

### 4.2 Claude Code driver (`src/claude_live.rs`)

#### 4.2.1 G4.3 amendment (spawned runtimes)

G4.3 forbade a second runtime on a shared store because two OpenCode generations corrupted one SQLite database. A companion-spawned runtime is permitted when all three of these hold:

(a) the runtime is designed for many concurrent processes on one data directory. Claude Code and Codex are both spawned as child processes by their own VS Code extensions, users run several Claude terminals side by side, and each Claude session writes its own transcript file;
(b) no two processes ever write the same session: new sessions get a fresh `--session-id <uuid>`, `--resume` is used only for a session this companion created and holds no live process for, and anything else needs `--fork-session`;
(c) the spawn uses an explicit binary, no shell, and prompts only on stdin.

OpenCode stays attach-only; its shared database is the original incident. The existing hook ingress stays as it is, observing sessions started in VS Code or a terminal. Hooks whose `session_id` belongs to a driven session are ignored, so those sessions do not show up twice.

#### 4.2.2 Launch

```text
<claude.exe> -p --output-format stream-json --input-format stream-json --verbose
             --include-partial-messages --permission-prompt-tool stdio
             --setting-sources user,project,local --thinking-display summarized
             [--permission-mode <configured default>]
             (--session-id <uuid> | --resume <uuid>)
cwd = the allow-listed workspace
env = scrubbed (§4.1) + CLAUDE_CODE_ENTRYPOINT=sdk-ts + CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1
```

- `-p` follows `claude --help` ("`--input-format` only works with `--print`") and was verified live in the first research's runs 1-3. The SDK omits it, which also worked (second research). `--thinking-display` is an SDK flag hidden from `--help`, verified in research run 2. `--replay-user-messages` is **not** passed: the companion creates user items itself, so there are no echoed `control_response` frames to filter.
- `CLAUDE_CODE_ENTRYPOINT=sdk-ts` replaces whatever value launched the companion (a child of VS Code inherits `claude-vscode`). It is the official SDK's value; step 6's live test confirms behaviour.
- Binary: the config `binary`, else `%USERPROFILE%\.local\bin\claude.exe`, else `claude.exe` on PATH (never the `.cmd` shim). Health is `claude --version` at boot and in the probe loop.
- Sequence: spawn → write `control_request initialize` → write the first `user` frame right away (the CLI queues it) → `state starting` until the initialize response (2-30 s depending on the user's plugins; the first turn takes 9-12 s).

#### 4.2.3 Configuration and workspace policy

The existing `claude` adapter entry gains optional keys. Without `workspaces` it stays an ingress only (today's behaviour):

```json
{ "id": "claude-1", "type": "claude", "enabled": true,
  "workspaces": ["G:\\Chernobyl"], "binary": null, "maxSessions": 3, "permissionMode": "default" }
```

- Each workspace must be an absolute, existing directory, canonicalized. Drive roots, the user-profile root, and any directory that contains the CHERNOBYL data directory are refused at config load. An agent reads files in its cwd without asking, so the data directory (control key, journal) must never be inside one.
- **Adding a folder to `workspaces` is the folder-trust decision.** Print mode does not show Claude's trust dialog, so project hooks and MCP servers in that folder load on spawn. `INTEGRATIONS.md` and the config documentation say so next to the `workspaces` setting.
- The phone sees `Agent.live.workspaces: [{id, label, path}]`, where `id` is the first 12 hex chars of sha256(canonical path) (stable across config edits) and `label` is the folder name. `create_session.project` must be one of those ids; anything else is refused.
- `maxSessions` (default 3) caps live child processes. `create_session` is rate-limited to one per 5 s per adapter. A process idle for 30 min with no subscriber and no pending engine permission gets its stdin closed (graceful exit). The next `submit_prompt` respawns it with `--resume <uuid>` behind `state starting`. Reaping also covers a request that passed its 24 h presentation expiry: closing stdin makes the CLI fail that request, so the tool is denied.
- The `Session` entity: `id` = the Claude session uuid (chosen up front, so the `create_session` ack carries it), `agentId`/`hostId` = adapter id, `project` = workspace path, `transcript: true`, and `files` aggregated from tool diffs, which keeps the old Files tab working.

#### 4.2.4 Reading frames safely

- Lines are read up to 16 MiB. A longer line is skipped to its newline, counted, and reported as a `notice`.
- A line that fails to parse (depth > 32, a lone surrogate from an emoji split across deltas, truncation) is counted. If it starts with `{"type":"control_request"`, the `request_id` is extracted by a shallow scan and answered with `control_response {subtype: "error"}`. That is fail-closed (the CLI treats it as a refusal) and keeps the CLI from blocking. A lost text delta heals itself, because the authoritative `assistant` frame replaces the item's text.
- Request id namespaces: companion-originated ids are `chb_<n>`, CLI-originated ids are UUIDs, kept in two maps.
- Exactly one `control_response` goes out per CLI request, except after a `control_cancel_request`. Answering and cancelling are linearized on the driver's pending-request map (§4.5).
- On child exit: pending permissions → `resolve_native(stale)`, an in-flight prompt → `command_result failed ("Claude Code exited with code N")`, a pending interrupt → `unknown`, `state ended` unless a respawn follows. If the companion itself dies, the children see stdin EOF and can no longer receive an answer, so an open approval can never be granted while the companion is down. The SDK sources say later control requests then fail with "Stream closed"; step 6's live test confirms the exact exit behaviour.

#### 4.2.5 Phone → CLI

| Phone message | Written to stdin |
|---|---|
| `create_session` | spawn (§4.2.2) + `initialize` + first `user` frame |
| `submit_prompt` | `{"type":"user","message":{"role":"user","content":[{"type":"text","text":…}]},"parent_tool_use_id":null,"session_id":"","uuid":"<v4 uuid mapped to commandId>"}` |
| `interrupt` | `{"type":"control_request","request_id":"chb_N","request":{"subtype":"interrupt"}}` |
| `permission_decision` | `{"type":"control_response","response":{"subtype":"success","request_id":<CLI id>,"response":{…}}}` (§4.5) |
| `set_permission_mode` | `control_request {subtype: "set_permission_mode", mode}` |
| `session_end` | `interrupt` if a turn is running, then close stdin (exit within ~4 s, then `kill` after a 5 s grace) |
| `mcp_auth` / `mcp_auth_callback` | §4.7 |

### 4.3 Codex app-server (`src/codex.rs`)

- One `codex app-server` child per adapter (stdio, `ChildIo`); each phone session is a thread inside it. Config: `{"type": "codex", "workspaces": [...], "binary": null, "args": []}` (`args` lets tests point `binary` at `node` and a fake script). The workspace policy is the same as Claude's.
- Handshake: `initialize {clientInfo: {name: "chernobyl", title: "CHERNOBYL", version}, capabilities: {experimentalApi: false}}` → `initialized` → per session `thread/start {cwd, approvalPolicy: "on-request", sandbox: "workspace-write"}` → `turn/start {threadId, input: [{type: "text", text}]}`. Enum spelling follows the generated TS types (`"on-request"`, `"workspace-write"`), not the docs prose (`codex-and-antigravity.md` §A7).
- Approval options: `accept` → approve-once "Allow"; `acceptForSession` → approve-session "Allow for this session"; `acceptWithExecpolicyAmendment` → approve-session, `scope: persistent`, `rule` = the proposed amendment, shown only when `proposedExecpolicyAmendment` is present; `decline` → reject "Deny"; `cancel` → reject "Deny and stop", `stopsTurn`. The answer is `{"id": <request id>, "result": {"decision": …}}`.
- Follow-ups: idle → `turn/start`; busy → `turn/steer {threadId, input, expectedTurnId}`, falling back to `turn/start` if Codex says no turn is active. Interrupt: `turn/interrupt {threadId, turnId}`.
- Parsing is tolerant: unknown notifications are ignored, unknown fields kept out. Every request id Codex sends gets exactly one response.
- Honesty: every Codex capability is `limited` with the detail "implemented from openai/codex@970111f schema; verified against fake app-server processes only; no live run".

### 4.4 OpenCode rich parts (`src/opencode.rs`, extended)

Implements the §3.6 table and `opencode-live-events.md` §9:

- keeps `partID → {type, messageID}` and `messageID → role` maps;
- on first subscribe and after every SSE reconnect, hydrates items from `GET /session/{id}/message` on a short-lived thread (deltas are not replayable);
- replies through `POST /permission/{id}/reply {reply, message?}` and `POST /question/{id}/reply {answers}` / `/reject`;
- maps `MessageAbortedError` to interrupted and closes prompt commands on idle;
- polls `GET /mcp` while a session is subscribed (30 s) and after auth calls or MCP toasts;
- never sends a `tools` map in prompt bodies (`opencode-live-events.md` §3.4).

Permission options: `once` → approve-once "Allow once"; `always` → approve-session "Always allow `<always[] joined>` until OpenCode restarts", `rule` = the patterns (the research found the rule held in memory, `GET /api/permission/saved` stayed empty); `reject` → reject "Deny" with `takesMessage` (rejecting with a message keeps the turn going). OpenCode has no permission-mode or end-session equivalent, so those capabilities are absent.

### 4.5 Permission round trip (fail-closed, G2)

```text
runtime ──request (native id R, tool T)──▶ adapter
adapter: PermissionRequest P {id, nativeRequestId: R, toolItemId: T, options, detail,
                              supportedDecisions = legacy buckets, revision: 1,
                              expiresAt: now + 24 h, legacy title/command/action/risk filled}
         adds R to its pending map, then:
         engine.ingest_permission(P) → permission_requested   (journaled, every client, notifications)
         hub.push(item T: awaiting-approval, permissionId P)  → transcript (subscribers only)
phone:   renders the approval inside tool card T, reading P (status, revision, options) from the snapshot
         ── permission_decision {permissionId, decision, optionId?, message?, answers?, revision}
companion: persist command → engine.validate_native_permission_decision (pending, expiry,
           revision, decision ∈ supportedDecisions or optionId's bucket) → adapter.decide()
adapter:   under its pending-map lock: R still present? → write the native answer, remove R
           (R absent → ack_err "request is no longer pending")
companion: engine.complete_native_decision → permission_resolved (revision 2) + command_result completed
withdrawal: control_cancel_request / serverRequest/resolved / permission.replied from elsewhere / process exit
           → adapter removes R → engine.resolve_native(P, stale | approved | rejected) → revision bump
           → a late phone decision fails with "request is no longer pending"
```

Claude answer bodies (verified live unless marked):

- allow once: `{"behavior":"allow","updatedInput":<original input>}`;
- a suggestion option: the same plus `"updatedPermissions":[<that suggestion, echoed byte-for-byte>]`;
- deny: `{"behavior":"deny","message":"<phone text or 'Denied from phone'>"}`;
- AskUserQuestion: allow with `updatedInput.answers = {<question text>: <label or labels or free text>}`; the phone sends `answers` keyed by question id and the driver maps ids back to the question texts;
- ExitPlanMode (docs only): "Approve plan" = allow; "Approve and auto-accept edits" = allow + the offered `setMode acceptEdits` suggestion; "Keep planning" = deny with the phone's message.

The suggestion filter mirrors the extension's `requestToolPermission`: drop any `setMode bypassPermissions`; drop everything when `suppress_always_allow_rule` is set; never send an `updatedPermissions` entry that was not offered. Destinations `localSettings`, `projectSettings` and `userSettings` become `scope: persistent` with `destination` = `.claude/settings.local.json`, `.claude/settings.json` or `~/.claude/settings.json`. Choosing one on the phone requires a confirm dialog that shows the exact rule and file, and appends a companion audit record (`permission-rule`). `default_to_no` makes Deny the primary button. Approve-with-edited-input is not offered in v1.

Legacy fields for v2 phones: `title` = the CLI's `title` sentence or "Claude wants to use <display_name>"; `command` = the command, path or URL; `action` = shell (Bash) / file-write (Edit, Write, MultiEdit, NotebookEdit) / network (WebFetch, WebSearch) / agent-control (everything else); `risk` = high for shell, medium for file-write, network and MCP, low otherwise; `context` = description or decision reason, redacted, ≤ 500 chars; `directory` = the workspace.

The companion TUI's approval keys go through the same `handle_client_msg` path, so a driven session can also be answered at the desktop. With two phones, the first decision wins and the second fails on the revision, as today.

### 4.6 Follow-ups, interrupts and command lifecycles (G1, PRD §12.5)

| Command | ack (acceptance only) | `command_progress` | `command_result` |
|---|---|---|---|
| `create_session` | process spawned / thread started / OpenCode session created | `accepted` on initialize; `running` when the first turn starts | when the first turn ends: completed / cancelled / failed; `failed` on spawn or initialize failure |
| `submit_prompt` | frame queued to the writer / HTTP 204 / JSON-RPC result | `running` when its turn starts (Claude `command_lifecycle started` or the next `system/init`; Codex `turn/started` or the steer result; OpenCode `session.status busy`) | at its turn's end: completed / cancelled (interrupted) / failed |
| `interrupt` | written | `accepted` on native confirmation (Claude `control_response`, abort `true`, `turn/interrupt` result) | `completed` when the turn ends interrupted, or at once if no turn was running; `unknown` after 30 s without a turn end |
| `permission_decision` | unchanged | unchanged | unchanged |
| `set_permission_mode` | written | — | completed on `control_response success`, failed on error |
| `session_end` | accepted | `running` while the process drains | completed on exit |
| `mcp_auth` | accepted | `running` while awaiting the browser | completed when the server reports `connected`; failed; `expired` after 10 min |
| `mcp_auth_callback` | validated and forwarded | — | completed on connected; failed otherwise |
| `mcp_reconnect` | forwarded | — | completed on connected; failed otherwise |

Follow-up behaviour, which the phone shows in the composer through `Agent.live.promptWhileBusy`:

- Claude `queue` (changed from `merge` after emulator row 9.5 on 2026-11-04): the CLI folds a mid-turn prompt into the running turn only sometimes, at a tool boundary; usually it runs as its own turn. The driver completes the command either way, so the phone promises only "Queued".
- Codex `merge` via `turn/steer`.
- OpenCode `queue`: the prompt runs as its own turn after the current one.

Closing the phone, dropping the socket or stopping the app never interrupts anything (PRD §12.5).

### 4.7 MCP status and sign-in

Status per session: Claude polls `mcp_status` after initialize, after each `system/init`, every 30 s while subscribed, and after auth actions (there is no push event; about 15 ms per call). Codex re-lists on its push notifications. OpenCode polls as in §4.4.

| Action | Phone can trigger | How the companion does it |
|---|---|---|
| See server status (connected / pending / failed / needs-auth / disabled) | yes | `mcp` op |
| Sign in on the PC (`flow: desktop`) | yes | Claude `mcp_authenticate` → open `authUrl` on the desktop (the CLI's own localhost listener completes it); OpenCode `POST /mcp/{name}/auth/authenticate` (opens the desktop browser itself, run on a thread); Codex `mcpServer/oauth/login` → open `authorizationUrl` → wait for `oauthLogin/completed`. Then poll until connected (Claude also sends `mcp_reconnect`, needed in research run 4) |
| Finish sign-in away from the PC (`flow: phone-callback`) | Claude, OpenCode | Start as above but return `auth.url` and `auth.callbackPrefix` to the phone. The user signs in in the phone browser, which lands on a dead `http://localhost:<port>/callback?code=…&state=…` page, and pastes that URL. The companion checks scheme `http`, host `localhost` or `127.0.0.1`, the exact port and path from the auth start, and that `state` equals the value it returned (Claude `state`, OpenCode `oauthState`). It then sends Claude `mcp_oauth_callback_url` + `mcp_reconnect`, or OpenCode `POST /mcp/{name}/auth/callback {code}`. One pending auth per server, 10-min lifetime, single use |
| claude.ai connectors (`config.type: claudeai-proxy`) | open only | `authFlows: ['claude-ai']`, `url: https://claude.ai/customize/connectors`, opened on the phone; the companion is not involved |
| Reconnect a failed server | yes (`mcp_reconnect {server}`, §3.2) | Claude `mcp_reconnect`; OpenCode `POST /mcp/{name}/connect`; Codex `config/mcpServer/reload` (its closest equivalent) |
| Add, remove or toggle servers; clear credentials; Claude or Codex account login; edit workspaces or binaries; bypass permissions | **no, desktop only** | not exposed |

The phone never supplies a URL that the desktop opens. It names a server, and the companion opens only the URL the runtime returned. stdio servers report `authFlows: []`.

---

## 5. Phone UX (c)

Follows `apps/mobile/DESIGN.md`: monochrome, hairlines, tokens only, state colors only for state. `session/[id].tsx` picks the conversation view when `session.transcript` is true. Sessions without a transcript (the companion's demo adapter, or any session on a companion that predates this ADR) keep today's screen.

**Header** (dark `PageHeader`): title, agent · host, `StatusPill`. On the right, an **MCP chip** ("MCP 4/5" with a warning dot when a server needs auth or failed; it opens an MCP sheet listing servers with status and the actions from §4.7) and a **mode chip** for Claude ("Ask before edits" / "Accept edits" / "Plan"; tapping opens a picker of `permissionModes`).

**Body** (`TranscriptList`, a FlatList keyed by item id; it sticks to the bottom while the user is at the bottom and shows a "Jump to latest" pill otherwise):

- **user** items: `Card` with `surface2` fill. A queued prompt shows "Queued" or "Joins the current turn".
- **text** items: `MessageText`, a small in-house markdown subset (paragraphs, headings, bold, italic, inline code, fenced code in `mono` on black, bullet and numbered lists). No HTML, no images, no remote loads. Links show their full URL and open only on an explicit tap, after a confirm. Streaming text renders as it arrives; nothing is announced per delta, and accessibility announces at turn end.
- **thinking**: a collapsed row "Thinking…" with a one-line preview, expandable to `text2` italic text.
- **tool** items: `ToolCard`, collapsible; header row with an icon, the tool name, the one-line `title` and a status mark (running indicator, done, failed, denied, stopped). Expanded: input in a `MonoBlock`, output in mono with a "truncated" marker, and `DiffView` (lines by first character, `success`/`error` tokens for +/-, hunk headers in `text3`). "Auto-approved (rule)" shows in `text3` when `autoApproved` is set. Subagent items nest inside their Task card, collapsed.
- **Inline approval** (`InlineApproval`, inside the tool card): when the snapshot holds a pending `PermissionRequest` whose `toolItemId` matches, the card gets a strong outline, opens itself, shows the detail (diff, command, questions or plan), and offers a primary "Allow" (or "Deny" when `default_to_no`), a secondary "Deny", and "More…" for session and persistent options and "Deny and tell Claude why" (`takesMessage`). Persistent options go through a `ConfirmModal` showing the rule and destination file. Questions render as option chips plus free text. The revision sent is the snapshot record's revision. A pending permission whose tool item is not in the view (trimmed, or not subscribed yet) is shown as a standalone card at the end of the conversation, so it is never invisible. If the tool item says `awaiting-approval` before the permission record arrives, the card shows "Waiting for approval…" with no buttons.
- **plan**: a checklist card updated in place.
- **notice**: centered `xs` text (retrying, stopped, compacted, errors).
- **turn footer**: after a turn's last item, "Done · 12.4 s · 4.2k tokens · $0.01" (cost only for Claude), "Stopped", or the error.

**Composer** (`PromptComposer`, extended): it stays enabled while the agent runs when `promptWhileBusy` is `merge` or `queue`, with the caption "Joins the current turn" or "Runs after this turn". A **Stop** button appears while a turn runs. The prompt limit is 32,000 chars. Starting a session shows "Starting Claude Code…" from `state starting`. An unknown command outcome keeps today's honest warning (mandate §9).

**Lifecycle**: `useLiveSession(sessionId)` sends `session_subscribe` on focus and whenever the transport returns to `connected` (with `epoch` and last `seq`), and `session_unsubscribe` on blur. `useTranscript(sessionId)` reads a zustand store fed through `applyTranscript`. That store is a second reducer on the **same** transport message stream, not a second connection or fetch path; `DESIGN.md`'s "never add a second data path" rule is amended to say so in the same commit.

**Elsewhere**: the Approvals tab stays as an inbox, and tapping a request with `toolItemId` on a transcript session opens the conversation scrolled to that card. A notification tap does the same. The new-task screen offers a workspace picker for agents with `live.workspaces`. Live commands that should not toast (`session_subscribe`, `session_unsubscribe`) go through `getTransport().send()` directly rather than `useCommands`.

**Mock (ADR-009)**: the app's TS `MockEngine` gains one scripted transcript for its demo run (streaming text, a tool card with a diff, an inline approval, a turn footer), badged Simulated. The companion's Rust mock gets no transcript; its sessions keep the classic screen.

**New primitives** (`src/ui/transcript.tsx`): `TranscriptList`, `MessageText`, `ThinkingRow`, `ToolCard`, `DiffView`, `InlineApproval`, `PlanCard`, `NoticeRow`, `TurnFooter`, `McpChip`, `McpSheet`, `ModeChip`. New icons are added to `PATHS` (thinking, plug, diff, stop if missing). `DESIGN.md` gets them in the same commit.

---

## 6. Security (d)

**Invariants kept.** No new listener; every byte to the phone goes through `Outbound::enqueue_sealed`. Every new client message needs a live E2E session beyond loopback: none of them is exempt, so the existing D2 gate applies, `session_subscribe` included because transcripts carry content. At remote posture a sessionless or handshaking connection receives no transcript frames (the existing D9 check in `enqueue_sealed`). Revocation (ADR-014) closes the connection, and `ConnGuard` teardown removes its subscriptions. There is no protocol version change and no crypto change.

| # | Change | New exposure | Control |
|---|---|---|---|
| T1 | The phone can spawn Claude and Codex processes | A stolen, unrevoked phone can start agents | Operator-set workspace allow-list (no drive roots, never containing the data dir); `maxSessions`; rate limit; the runtimes' own permission engines stay on (no bypass mode is reachable); revocation cuts the phone off. Already true for OpenCode prompts today |
| T2 | Transcripts (code, tool output) flow to the phone | More sensitive data on the phone and in companion RAM | E2E only; memory-only on both sides (never journaled, never in phone SQLite); redaction of secret shapes (§4.1); caps (§3.5); ended transcripts dropped after 1 h |
| T3 | Agent output is rendered on the phone (prompt-injection surface) | Spoofed UI, tracking pixels, malicious links | Markdown subset with no images, HTML or remote loads; links need a tap and a confirm showing the full URL; approval controls are drawn only from companion `PermissionRequest` records, never from transcript text; MCP server and tool names render as plain text |
| T4 | Persistent permission rules from the phone | Silent broad grants | Only runtime-offered suggestions, after the extension's filter (no `bypassPermissions`); persistent options need a confirm naming the rule and file; legacy decisions never select them; audit record per grant |
| T5 | MCP OAuth relay | Auth code leakage; arbitrary URL opening on the desktop | The desktop opens only runtime-produced http(s) URLs, without a shell; callback URL checked for host, port, path and state, single-use, 10 min; never journaled (payload redaction) or logged |
| T6 | Children inherit the companion's environment | Agent reads the transport token | `HOST_TOKEN` and `CHERNOBYL_*` removed from the child env; the control key's folder can never be a workspace |
| T7 | Runtime frames are new untrusted input | Parser DoS, memory growth, a stuck runtime | 16 MiB line cap, JSON depth 32, salvage-and-refuse for unparseable control requests, bounded writer queue, malformed counters on `/health` |
| T8 | Transcript fan-out load | Saturation teardowns | Coalescing, half-depth backpressure, 4 subscriptions per connection, snapshot rate limit |
| T9 | The spawned runtime opens a localhost OAuth callback listener during sign-in | Another local process could hit it | The runtime's own behaviour (identical under VS Code); PKCE and state are enforced by the runtime; not a CHERNOBYL listener and not reachable from the phone |
| T10 | Two writers on one Claude session | Transcript corruption | Fresh `--session-id`; `--resume` only for companion-owned sessions without a live process; `--fork-session` otherwise. To continue a phone session in VS Code, end it on the phone first or let the 30-min reaper close it |
| T11 | Print mode skips Claude's folder-trust dialog | Project hooks and MCP servers in a workspace run on spawn | Only operator-allow-listed workspaces can be spawned in; the docs state that allow-listing equals trusting the folder |

**Residuals stated honestly.** A secret split across two streaming text deltas can appear on screen until its block completes, when the authoritative replace re-redacts it. Tool output arrives whole (Claude, OpenCode) or line-buffered (Codex), so it is redacted before display. Redaction recognises known shapes only. The desktop JSONL transcripts keep everything; they belong to the runtimes, not to CHERNOBYL.

**Doc corrections in the same change.** `PRIVACY.md`: today OpenCode text parts are republished as journaled activity, which contradicts "does not store conversation history". The new adapter stops that, and the doc gains "live transcripts are held in memory only" plus the MCP callback redaction. The threat model gets T1-T11 as §3.12 (§3.11 went to the pairing-QR device key rotation, `9e460d7`). `INTEGRATIONS.md`: Claude becomes "ingress + driven sessions", Codex is added as `limited`.

---

## 7. Implementation and test plan (e)

Order matters: each step is one commit with both gates green (`npm run verify`, `npm run verify:rust`), plus `npm run verify:e2e` from step 2 on. Well-specified batches (steps 1, 3, 5 and the UI components in 9) suit delegation to Antigravity per the owner's preference, with every diff reviewed and the checks rerun before commit.

1. **Protocol types and validators (TS + Rust).** Shared golden fixtures in `packages/protocol/fixtures/live/*.json`, read by the Rust tests through `include_str!`. Tests:
   - Rust: `parses_live_client_messages_and_rejects_bad_fields`, `permission_decision_accepts_option_id_message_answers`, `hello_ack_advertises_transcript_feature`, `transcript_messages_match_golden_fixtures`, `permission_extras_serialize_only_when_present`, hostile-corpus additions (bad `afterSeq`, oversize `answers`, `mode: bypassPermissions`).
   - TS (`packages/protocol`): validator accept/reject cases, the same fixtures, `applyTranscript` (`appends`, `upserts`, `ignores append to unknown item`, `gap or new epoch asks to resubscribe`, `snapshot replaces`, `duplicate seq skipped`).
2. **Transcript hub and subscription plumbing** (`transcript.rs`, `server.rs` routing of subscribe/unsubscribe with the connection's `Outbound`, `ConnGuard` cleanup, `Companion::pump` integration, `/health` counters `transcriptSessions`, `transcriptSubs`, `runtimeMalformed`). Tests:
   - `ring_evicts_by_ops_and_bytes`, `deltas_coalesce_within_a_flush`, `item_replace_flushes_pending_deltas_first`, `subscribe_with_current_epoch_replays_after_cursor`, `stale_cursor_or_new_epoch_gets_snapshot`, `snapshot_is_bounded_and_marks_truncation`, `pump_skips_connections_above_half_bulk_depth`, `lagging_subscriber_recovers_with_snapshot`, `fifth_subscription_evicts_oldest`, `ended_transcript_is_collected_after_ttl`, `tool_output_keeps_head_and_tail`, `diff_is_capped_with_marker`;
   - `redacts_known_secret_shapes`, `redaction_is_idempotent_and_utf8_safe`;
   - e2e: `transcript_frames_are_sealed_for_session_clients`, `sessionless_remote_connection_gets_no_transcript`, `subscribe_requires_session_at_remote_posture`, `teardown_drops_subscriptions`, `revoked_device_loses_its_subscriptions`.
3. **Engine G2 extensions + domain parity.** Rust: `option_decision_must_match_its_bucket`, `unknown_option_is_unsupported`, `legacy_decision_never_selects_a_persistent_option`, `supported_decisions_are_the_legacy_buckets`, `resolve_native_bumps_revision_and_refuses_late_decisions`, `runtime_permissions_use_the_long_ttl`. TS (`packages/domain`): the same cases for `evaluatePermissionDecision`.
4. **`LiveRuntime` trait and router.** OpenCode moves behind it with behaviour unchanged; all existing OpenCode and wiring tests pass untouched. New: `create_session_routes_by_agent_host`, `session_commands_route_to_owner`, `duplicate_session_id_across_runtimes_is_refused`, `demo_data_hidden_when_any_live_runtime_is_healthy`.
5. **OpenCode rich parts.** Fake-server SSE scripts from the 1.19.4 captures, sanitized and committed under `services/companion-rs/tests/fixtures/opencode/`. Tests:
   - `text_and_reasoning_deltas_become_appends`, `user_text_is_not_echoed_or_journaled`, `tool_part_lifecycle_maps_to_tool_item`, `permission_links_to_tool_by_call_id_with_diff`, `reject_with_message_reaches_reply_route`, `question_round_trip`, `todo_updated_becomes_plan`;
   - `abort_is_interrupted_not_failed`, `idle_completes_the_prompt_command`, `double_idle_is_idempotent`, `retry_status_becomes_notice`;
   - `mcp_needs_auth_is_polled_and_auth_start_returns_url`, `hydrates_items_from_messages_on_subscribe_and_reconnect`.
   - Live (`#[ignore]`, extends `tests/opencode_e2e.rs`): isolated `opencode serve` (`HOME`, `USERPROFILE` and `XDG_*` overridden, temp cwd) on local Ollama with a 24k-context model alias, covering streaming deltas, a bash permission approved inline, a reject with message, an abort mid-stream, and `GET /mcp`.
6. **Claude driver.** In-memory fake peer over `std::io::pipe()` replaying sanitized captures (research runs 1, 2, 5, 6, MCP run 4; paths → `<tmp>`, account, PKCE and state values removed) from `tests/fixtures/claude/`. Tests:
   - launch and config: `spawn_args_match_the_sdk_set_without_shell`, `child_env_drops_host_token_and_sets_entrypoint`, `workspace_allow_list_refuses_drive_root_profile_and_data_dir`, `workspace_ids_are_stable_hashes`;
   - transcript: `capture_run1_produces_expected_ops`;
   - permissions: `can_use_tool_becomes_permission_with_filtered_options`, `allow_once_writes_byte_exact_control_response`, `suggestion_filter_drops_bypass_and_unoffered_rules`, `deny_with_message_reaches_stdin`, `cancel_request_withdraws_and_late_decision_fails`, `ask_user_question_maps_ids_to_question_text`, `initialize_account_is_never_forwarded`;
   - lifecycle: `interrupt_progress_then_cancelled_turn`, `mid_turn_prompt_lifecycle_queued_then_started`, `cumulative_cost_becomes_per_turn_delta`, `process_exit_fails_prompt_and_stales_permissions`, `idle_process_is_reaped_and_resumed`, `max_sessions_and_rate_limit_refuse`;
   - robustness: `unknown_control_request_gets_error_response`, `unparseable_control_request_is_salvaged_with_error`, `oversize_line_is_skipped_with_notice`;
   - MCP: `mcp_desktop_flow_opens_runtime_url_only`, `mcp_callback_checks_host_port_path_state`.
   - Live (`#[ignore]`, `CHERNOBYL_LIVE_CLAUDE=1`): `live_claude_haiku_inline_permission_round_trip`, with `--model haiku` in a temp workspace and the prompt "Create a.txt containing hi, then edit it to say hello". It asserts streaming text, a Write permission answered with an option, the Edit diff, a follow-up mid-turn and an interrupt that leaves the process alive. Cost about $0.01 per run.
7. **Codex adapter.** In-memory fake peer scripted from schema shapes, plus `services/companion-rs/tools/fake-codex-app-server.mjs` (a node script speaking JSON-RPC on stdio) driven through the real spawn path in the interop suite. Tests:
   - `handshake_initialize_initialized_thread_start`, `agent_message_deltas_become_text_appends`, `command_output_deltas_are_line_buffered_and_redacted`, `file_change_item_carries_unified_diff`, `turn_diff_splits_per_file`;
   - `approval_options_map_the_decision_enum`, `server_request_resolved_withdraws`, `unknown_server_request_gets_method_not_found`, `permissions_request_gets_empty_grant_and_notice`;
   - `steer_when_busy_start_when_idle`, `interrupt_uses_the_active_turn_id`;
   - `mcp_status_and_startup_updates_map`, `oauth_login_completed_marks_connected`.
8. **MCP plumbing and journal redaction.** `opener_args_reject_non_http_and_shell_metacharacters`, `mcp_auth_callback_is_journaled_redacted` (asserts the journal file never contains the code), `pending_auth_expires_after_ten_minutes`.
9. **Phone.** `useLiveSession`, the transcript store, the `src/ui/transcript.tsx` components, the session-screen branch, the new-task workspace picker, Approvals deep links, the `MockEngine` scripted transcript (TS test: `mock transcript ops are valid and contiguous`), the `DESIGN.md` update. Gates: typecheck, lint, `npm run verify`.
10. **Interop** (`tools/e2e-interop.mjs`, the real companion binary with the fake Claude and Codex scripts configured in a temp `companion.json`):
    - subscribe → snapshot → live ops, all sealed;
    - reconnect with a cursor → replay with no gap or duplicate;
    - a stale cursor → snapshot;
    - an inline decision by `optionId` with revision, plus a stale revision refused;
    - a v2 client that never subscribes receives zero transcript frames;
    - `mcp_auth_callback` with a wrong `state` refused.
11. **Emulator rows** (new section 9 in `PHONE-VALIDATION-CHECKLIST.md`, run with `scripts/device/validate.mjs` against the real companion with Claude and OpenCode configured):

    | Row | Check |
    |---|---|
    | 9.1 | Claude session from the phone: "Starting Claude Code…", then streaming text grows in place |
    | 9.2 | Tool card shows streaming input, then output; the Edit card shows the diff |
    | 9.3 | Inline approval inside the card → Allow → the tool completes; the Approvals tab entry clears |
    | 9.4 | "Deny and tell Claude why" → Claude continues and mentions the reason |
    | 9.5 | Follow-up sent mid-turn shows "Queued" and is answered (in that turn or the next) |
    | 9.6 | Stop mid-stream → "Stopped"; the next prompt works in the same session |
    | 9.7 | App in the background, an approval arrives as a notification; tapping it opens the conversation at the card |
    | 9.8 | Airplane mode on and off mid-stream → the transcript resumes with no gap or duplicate |
    | 9.9 | MCP chip shows needs-auth → "Sign in on PC" opens the desktop browser → the chip turns connected |
    | 9.10 | A persistent "always allow" option shows the confirm with the rule and `.claude/settings.local.json` |
    | 9.11 | `echo` of a fake `sk-ant-` key appears as `«redacted:anthropic-key»` |
    | 9.12 | OpenCode session (Ollama): streaming, inline bash approval, abort shown as Stopped |
    | 9.13 | An old (pre-ADR-015) APK against the new companion: dashboard, approvals and sessions behave as before |

12. **Docs.** `INTEGRATIONS.md`, `PRIVACY.md`, threat model §3.12, `services/companion-rs/README.md` wire contract, `README.md`, `TODO.md`, and the memory project note.

**Live verification here:** Claude Code yes (CLI 2.1.310, Haiku, temp workspace, about $0.01 per run); OpenCode yes (1.19.4 on local Ollama, since the free models are exhausted; isolated env); Codex no (not installed, no credentials; fake processes only, capabilities stay `limited`); Antigravity deferred.

---

## 8. Consequences

- The companion grows three modules (`transcript.rs`, `claude_live.rs`, `codex.rs`) and a trait. It stays std-only, a single binary, with no new listener.
- `claude.rs`'s "server.spawn unavailable" capability becomes accurate only for the ingress. The driver declares its own honest list: `session.create`, `session.prompt`, `session.interrupt`, `permission.review` and `session.activity.stream` are `available` once step 6's live test passes, and `session.history.read` stays `unavailable` in v1.
- G4.3 is amended for spawned runtimes (§4.2.1). PRD §7.5's "only through a verified supported mechanism" is met by the documented headless and Agent SDK interface, and Claude's permission checks are never disabled.
- Codex appears in the app as `limited` until someone runs it live.

**Not in v1 (each is a later, separately reviewed step):**

- Claude session history: listing `~/.claude/projects/*` JSONL for allow-listed workspaces, read-only history hydration after a companion restart, showing hook-observed desktop sessions as read-only sessions, and "continue on phone" through `--resume <id> --fork-session` (the SDK's `listSessions`/`getSessionMessages` read the same files). Until then a companion restart drops driven Claude sessions from the phone's list; their transcripts stay in `~/.claude/projects` and `claude --resume` on the desktop still opens them.
- Approve with edited input; image prompts; model picker; context meter (`get_context_usage`); MCP elicitation forms; notification action buttons; a Rust-mock transcript; a soak phase that floods transcripts.
- **Antigravity:** an activity-only adapter over `agy --input-format stream-json --output-format stream-json` is feasible, but headless mode auto-denies instead of asking (`denied_actions`), and interrupt means killing the process. It waits until `agy` gains a headless approval channel. Its language-server Connect RPC is rejected (internal, CSRF per process, protobuf, unversioned).
- **Remote Control** (Claude and `agy`) stays the vendors' own cloud-relayed option. Nothing here depends on it.

## 9. Sources

- Claude Code / Agent SDK: `docs/reverse-engineering/claude-code-streaming.md`, `claude-agent-sdk.md`; extension `anthropic.claude-code-2.1.310` `extension.js` (SDK transport and query code, unaffected by the local ad-injection patch both researchers found); <https://code.claude.com/docs/en/agent-sdk/streaming-output.md>, …/agent-sdk/permissions, …/agent-sdk/user-input.md, …/agent-sdk/mcp.md, …/headless.md, …/cli-reference.md.
- Codex: `docs/reverse-engineering/codex-and-antigravity.md` Topic A; openai/codex `970111f` `codex-rs/app-server-protocol/schema/typescript/` (`ServerRequest.ts`, `v2/ThreadItem.ts`, `v2/CommandExecutionApprovalDecision.ts`, `v2/McpServerStatus.ts`, `v2/McpServerOauthLogin*.ts`), `codex-rs/app-server/README.md`; <https://developers.openai.com/codex/app-server>.
- OpenCode: `docs/reverse-engineering/opencode-live-events.md`, `opencode.md`; <https://opencode.ai/docs/server/>, <https://opencode.ai/docs/mcp-servers/>.
- Antigravity: `docs/reverse-engineering/codex-and-antigravity.md` Topic B.
- CHERNOBYL code read for this design: `packages/protocol/src/index.ts`, `services/companion-rs/src/{protocol,adapters,companion,engine,server,opencode,claude,config,e2e,ws}.rs`, `packages/e2e/src/transport.ts`, `apps/mobile/app/session/[id].tsx`, `apps/mobile/src/data/{reducer,state,commands,transport}.ts`, `apps/mobile/src/ui/lists.tsx`, `apps/mobile/DESIGN.md`.

## 10. Implementation notes (2026-11-04)

Where the code on main differs from the text above, as recorded in the commit messages (`85c54db` to `62c0aba`). The code is the authority.

- **Codex.** The app-server starts on the first session, not at boot. When it exits (crash, or codex shutting itself down), its sessions go to `idle` with a notice instead of ending; the next `submit_prompt` to one respawns the app-server and sends `thread/resume {threadId, cwd, approvalPolicy: "on-request", sandbox: "workspace-write"}`, then the prompt as a new `turn/start` (`97bb31a`). A resume is not a create: it takes no T1 rate-limit or session-cap slot. One resume per thread at a time; a second prompt meanwhile is refused. If Codex refuses the resume, or exits during it, the command fails ("Codex could not resume this session: …") and the session ends. Assumption: the request shape comes from `docs/reverse-engineering/codex-and-antigravity.md` A2 (`thread/resume {threadId, ...same overrides, excludeTurns?}`), and the result is taken to look like `thread/start`'s (only `model` is read). `excludeTurns` is not sent, in case it is experimental. The fake app-server implements the same shape and answers an unknown id with "no rollout found for thread id …"; neither is checked against a real codex. The §4.6 timers are built: an `interrupt` with no turn end in 30 s is `unknown`, and a desktop `mcp_auth` not completed in 10 min is `expired`; a late answer after either closes nothing. Phone-callback sign-in is refused ("Codex signs MCP servers in on the desktop only"). No live run (§7).
- **Claude session history (2026-11-12, `bd999f6`).** Part of §8's "not in v1" list is built, without `--fork-session`. On the probe cadence (15 s) the runtime scans `<CLAUDE_CONFIG_DIR or %USERPROFILE%\.claude>\projects` (`claude_history.rs`) and lists every session whose recorded `cwd` lies inside an allow-listed workspace, whether it was started from the phone, a terminal or the VS Code extension. Phone sessions therefore come back after a companion restart. Folder names only preselect: the `cwd` in the entries decides, canonicalized with the same rules as `validate_workspace` (T11). Bounds: the 50 most recently written sessions out of at most 500 candidate files; per file a 64 KiB head and a 256 KiB tail, cached by path, mtime and size; `<uuid>.jsonl` only (subagent files and `isSidechain` entries are skipped). The title is `custom-title`, else `ai-title`, else `last-prompt`, else the first user text, cut to 80 characters after redaction. A listed session is `idle` with `transcript: true`; a live session with the same id wins, so nothing is listed twice. New or renamed sessions after the first scan go out as `session_updated`. **History:** the first subscriber of a listed session gets the last 4 MiB of its JSONL, decoded by the live `Driver` (user and assistant entries; `toolUseResult` is passed under its stream-json name), at most 300 items and turns, with a notice when older ones are left out. Tool calls end `completed`, each user prompt closes the turn before it, and history never raises a permission request. It is replayed again only when the file changed since. **Continue:** a `submit_prompt` (or mode or MCP command) adopts the session. Its process starts with `--resume <id>` in the recorded cwd, which is re-checked against the allow-list at that moment, and takes a `maxSessions` slot like any live process. `interrupt` and `session_end` on a session the companion never ran complete as no-ops. The driver starts from the last `cost-state` total, so the first resumed turn reports only its own cost. **In use on the PC (T10):** a transcript written in the last 2 minutes (`claude_history::IN_USE_MS`) belongs to a process the companion does not own, so the prompt is refused with "This session is active on the PC right now; continue it there or wait a moment". Hooks of listed but not driven sessions still reach the activity feed (`drives_session`). **Privacy:** history is read from the user's local files, then redacted and capped (titles here, every op by the hub) before anything leaves the PC. It stays memory-only like live transcripts. Titles of listed sessions are journaled as part of `session_updated` events. Known limits: a session ended on the phone shows `completed` until the companion restarts; a reaped or adopted session is resumed without the 2-minute check, so T10's advice still applies to one also opened on the desktop. Live test `live_claude_haiku_disk_session_is_found_replayed_and_resumed`: `claude -p "say hi"` on Haiku, then found, refused while fresh, replayed and resumed with one prompt; $0.0076 in total.
- **PC sessions live on the phone (2026-11-12, ADR-018).** A listed session that another process is writing is now mirrored while a phone watches it (`claude_history::Tail`, a 750 ms poll through the same `Driver`, bounded per poll) and flagged `Session.livePc`. A plain prompt to it is still refused (T10), but `submit_prompt {fork: true}` continues it as a new session with `--resume <id> --fork-session --session-id <new>`, leaving the original file untouched. With the operator's explicit `--install-hooks`, Claude Code's `PermissionRequest` command hook (`--hook permission-request`) asks a connected phone (allow once or deny, `fromPc`) and falls back to the PC's own prompt on timeout or any failure. See ADR-018.
- **Workspace policy timing.** Claude workspaces are checked at boot, not at config load: a bad folder keeps the `claude` entry ingress-only with `lastError` set, and the companion still starts. Codex workspaces are checked at config load, and a bad folder fails the load.
- **Claude config bounds.** `maxSessions` accepts 1-16 (default 3); `permissionMode` accepts `default`, `acceptEdits` or `plan`.
- **Claude capabilities.** As §8, plus `session.list` and `server.spawn` `limited`, and `mcp.auth` `limited` (phone-callback verified against a mock OAuth server; whether the CLI's own desktop listener reconnects by itself is unverified, so the companion polls and sends `mcp_reconnect`).
- **`list_state`** returns the merged snapshot, the same one `hello_ack` carries, instead of the engine's alone.
- **Journal redaction.** Besides `mcp_auth_callback.callbackUrl` (§6 T5), each value in `permission_decision.answers` is journaled as `<redacted>`; the in-memory message is what executes. `permission_decision.message` (a deny reason) and prompt text are journaled as sent, as every command record is.
- **Module layout.** `ChildIo`, `child_env`, `redact`, the desktop opener and the callback check live in `src/runtime_io.rs`, not `adapters.rs` / `transcript.rs`. `ChildIo` also refuses `.cmd` and `.bat` binaries. The runtimes are `claude_runtime.rs` and `codex_runtime.rs` over the protocol cores `claude_live.rs` and `codex.rs`.
- **Transcript caps beyond §3.5:** 256 KiB of diffs per item or turn, 256-byte ids (longer ops are dropped and counted), 1 KiB labels and errors, 100 plan steps, 32 MCP servers, so every op fits the 512 KiB snapshot cap.
- **Subscriptions** (`session_subscribe`, `session_unsubscribe`) are handled in `server.rs` next to `hello`, acked and never journaled.
- **Storage.** An ack recorded after its command already finished keeps the recorded outcome (an interrupt with no running turn completes before its ack).
- **Interop (step 10).** Scenario 11 of `tools/e2e-interop.mjs` runs the real companion against the fake Codex app-server only; there is no fake Claude process in the interop suite. It covers subscribe → snapshot → contiguous sealed ops, cursor replay, stale cursor → snapshot, an inline decision by `optionId` with a stale revision refused, and a client that never subscribes getting zero transcript frames. The wrong-`state` `mcp_auth_callback` refusal is covered by unit tests (`mcp_callback_checks_host_port_path_state`), not by interop.
- **Phone demo.** The scripted transcript runs only on the app's own mock transport (`MockEngine({ live: true })`). The Node reference host keeps the classic engine, matching the Rust mock.
- **Live tests.** Claude: `live_claude_haiku_protocol_round_trip` (`claude_live.rs`) and `live_claude_haiku_inline_permission_round_trip` (`claude_runtime.rs`), CLI 2.1.310 with Haiku, about $0.01 per run. OpenCode: `live_opencode_rich_parts` and `live_opencode_end_to_end` (`tests/opencode_e2e.rs`) passed on an isolated `opencode serve` 1.19.4 with `qwen3.5:4b` at 24k context on local Ollama; a 15-min watchdog now fails a hung run instead of waiting. Codex: none.
