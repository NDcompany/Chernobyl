//! ADR-015 §3.3/§3.5: the transcript hub. Live runtimes push per-session
//! item logs; connections that subscribed receive them. Memory only: never
//! journaled, never persisted (threat T2).
//!
//! Each session transcript keeps a ring of ops serialized once (replay), the
//! compacted items, turns, state and MCP list as of the last op (snapshots),
//! and streaming deltas coalesced until `pump` flushes them, every second
//! scheduler tick (100 ms). A subscription is (connection, session, cursor):
//! `pump` sends what lies after the cursor, or a snapshot when the cursor is
//! not in the ring, and skips a connection whose bulk queue is over half
//! full, so transcripts alone never cause a D8 teardown.
//!
//! Every text that enters is redacted (`runtime_io::redact`) and capped by
//! its serialized length (the §3.5 table), so control characters cannot
//! inflate a capped field six-fold on the wire, no op outgrows the snapshot
//! cap and every frame fits half the bulk budget.
//!
//! The hub is a leaf lock: while it is held, no other companion lock is
//! taken except `Outbound`'s session lock inside `enqueue_sealed` (a leaf
//! too). Runtimes reach it only through [`HubHandle`], whose calls lock,
//! work and return.

use crate::json::{self, Json};
use crate::protocol::{ServerMsg, now_ms};
use crate::runtime_io::redact;
use crate::server::{BULK_QUEUE_MAX_BYTES, BULK_QUEUE_MAX_MESSAGES, MsgClass, Outbound};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

/* ---------------- bounds (§3.5; lengths are serialized bytes) ---------------- */

const RING_OPS: usize = 2048;
const RING_BYTES: usize = 1 << 20;
const ITEMS_MAX: usize = 400;
const ITEMS_BYTES: usize = 2 << 20;
const SNAPSHOT_BYTES: usize = 512 << 10;
const MESSAGE_BYTES: usize = 64 << 10;
const MESSAGE_OPS: usize = 256;
const APPEND_BYTES: usize = 8 << 10;
const SESSIONS_MAX: usize = 8;
const ENDED_TTL_MS: i64 = 60 * 60 * 1000;
const SUBS_PER_CONN: usize = 4;
const SNAPSHOT_EVERY_MS: i64 = 1000;
const TEXT_MAX: usize = 256 << 10;
const THINKING_MAX: usize = 64 << 10;
const NOTICE_MAX: usize = 2 << 10;
const INPUT_MAX: usize = 16 << 10;
/// Tool output keeps this much head and this much tail.
const OUTPUT_HALF: usize = 16 << 10;
const DIFF_MAX: usize = 64 << 10;
const DIFF_FILES: usize = 16;
// Not in the §3.5 table: they keep every op inside the snapshot cap.
/// All diffs of one tool item or turn together (16 × 64 KiB would be 1 MiB).
const DIFFS_TOTAL: usize = 256 << 10;
/// Ids (item, turn, parent, permission, session); a longer one is malformed.
const ID_MAX: usize = 256;
/// Names, titles, labels, plan steps, paths, model and mode.
const LABEL_MAX: usize = 1 << 10;
const ERROR_MAX: usize = 1 << 10;
const URL_MAX: usize = 4 << 10;
const PLAN_STEPS: usize = 100;
const MCP_SERVERS: usize = 32;

const OUTPUT_MARKER: &str = "\n… output truncated …\n";
/// A diff "meta" line (git's `\ No newline…` shape), so a viewer shows it as
/// a note, not as patch content.
const DIFF_MARKER: &str = "\\ diff truncated by the companion";

/* ---------------- normalized model (ADR-015 §3.2) ---------------- */

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ItemKind {
    User,
    #[default]
    Text,
    Thinking,
    Tool,
    Plan,
    Notice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolStatus {
    /// Announced; the input may still be streaming.
    #[default]
    Pending,
    Running,
    /// A pending PermissionRequest names this item (`toolItemId`).
    AwaitingApproval,
    Completed,
    Failed,
    /// The user or a rule refused it.
    Denied,
    /// Interrupted or withdrawn.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FileDiff {
    pub path: String,
    /// added | modified | deleted | renamed
    pub status: &'static str,
    pub additions: i64,
    pub deletions: i64,
    /// Unified diff text; redacted and capped by the hub.
    pub patch: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanStep {
    pub text: String,
    /// pending | in_progress | completed | cancelled
    pub status: &'static str,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolCall {
    /// Runtime tool name, shown verbatim ("Bash", "apply_patch", …).
    pub name: String,
    /// One-line summary: the command, path or URL.
    pub title: Option<String>,
    pub status: ToolStatus,
    /// Final input (pretty JSON or the raw command), or raw partial JSON
    /// while streaming. Empty = not serialized.
    pub input: String,
    /// Empty = not serialized. The hub keeps head + tail.
    pub output: String,
    pub output_truncated: bool,
    pub error: Option<String>,
    pub diffs: Vec<FileDiff>,
    /// PermissionRequest.id while or after it was asked.
    pub permission_id: Option<String>,
    /// Why no prompt appeared: rule | mode | read-only | hook.
    pub auto_approved: Option<String>,
    pub duration_ms: Option<i64>,
}

/// One transcript entry. Ids are stable per session (§3.6). Only the fields
/// of its `kind` are serialized: `text` (user, text, thinking, notice),
/// `done`/`truncated` (text, thinking), `queued` (user), `level` (notice),
/// `tool` (tool), `steps` (plan). Build with struct update syntax:
/// `Item { id, kind: ItemKind::Text, text, ..Default::default() }`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Item {
    pub id: String,
    pub kind: ItemKind,
    pub ts: i64,
    pub turn_id: Option<String>,
    /// Nested under a subagent / Task tool item.
    pub parent_id: Option<String>,
    pub text: String,
    pub done: bool,
    pub truncated: bool,
    pub queued: bool,
    /// info | warning | error (notice; default info)
    pub level: Option<&'static str>,
    pub tool: Option<ToolCall>,
    pub steps: Vec<PlanStep>,
}

/// Which item field an `append` grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// user, text, thinking, notice
    Text,
    /// tool input
    Input,
    /// tool output
    Output,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: Option<i64>,
    pub reasoning: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TurnState {
    pub id: String,
    /// running | completed | interrupted | failed
    pub status: &'static str,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub usage: Option<Usage>,
    /// Claude only: this turn's share of the cumulative cost.
    pub cost_usd: Option<f64>,
    pub error: Option<String>,
    pub diffs: Vec<FileDiff>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiveState {
    /// starting | idle | running | waiting | ended
    pub status: &'static str,
    pub permission_mode: Option<String>,
    pub model: Option<String>,
}

impl Default for LiveState {
    fn default() -> LiveState {
        LiveState {
            status: "idle",
            permission_mode: None,
            model: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct McpServer {
    /// Untrusted display text (user or plugin config).
    pub name: String,
    /// connected | pending | failed | needs-auth | disabled
    pub status: &'static str,
    pub error: Option<String>,
    /// desktop | phone-callback | claude-ai; empty = nothing to do here.
    pub auth_flows: Vec<&'static str>,
    pub auth: Option<McpAuthState>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct McpAuthState {
    /// starting | awaiting-browser | awaiting-callback | completing | failed
    pub state: &'static str,
    pub url: Option<String>,
    pub callback_prefix: Option<String>,
    pub error: Option<String>,
}

/// One transcript op. `Item` inserts or replaces by id (always the whole
/// item), `Turn` upserts by id, `State` and `Mcp` replace. `Append` is what
/// the hub's flush emits for coalesced deltas; pushing one is the same as
/// `append`.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Item(Item),
    Append {
        id: String,
        field: Field,
        text: String,
    },
    Turn(TurnState),
    State(LiveState),
    Mcp(Vec<McpServer>),
}

/* ---------------- serialization (the TS shapes; golden-tested) ---------------- */

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::User => "user",
            ItemKind::Text => "text",
            ItemKind::Thinking => "thinking",
            ItemKind::Tool => "tool",
            ItemKind::Plan => "plan",
            ItemKind::Notice => "notice",
        }
    }
}

impl ToolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolStatus::Pending => "pending",
            ToolStatus::Running => "running",
            ToolStatus::AwaitingApproval => "awaiting-approval",
            ToolStatus::Completed => "completed",
            ToolStatus::Failed => "failed",
            ToolStatus::Denied => "denied",
            ToolStatus::Cancelled => "cancelled",
        }
    }
}

impl Field {
    pub fn as_str(self) -> &'static str {
        match self {
            Field::Text => "text",
            Field::Input => "input",
            Field::Output => "output",
        }
    }
}

type Fields = Vec<(&'static str, Json)>;

fn opt_str(f: &mut Fields, key: &'static str, v: Option<&str>) {
    if let Some(v) = v {
        f.push((key, json::s(v)));
    }
}

fn flag(f: &mut Fields, key: &'static str, on: bool) {
    if on {
        f.push((key, Json::Bool(true)));
    }
}

fn diffs_json(f: &mut Fields, diffs: &[FileDiff]) {
    if !diffs.is_empty() {
        f.push((
            "diffs",
            Json::Arr(diffs.iter().map(FileDiff::to_json).collect()),
        ));
    }
}

impl FileDiff {
    pub fn to_json(&self) -> Json {
        let mut f = vec![
            ("path", json::s(&self.path)),
            ("status", json::s(self.status)),
            ("additions", Json::Int(self.additions)),
            ("deletions", Json::Int(self.deletions)),
            ("patch", json::s(&self.patch)),
        ];
        flag(&mut f, "truncated", self.truncated);
        json::obj(f)
    }
}

impl ToolCall {
    pub fn to_json(&self) -> Json {
        let mut f = vec![("name", json::s(&self.name))];
        opt_str(&mut f, "title", self.title.as_deref());
        f.push(("status", json::s(self.status.as_str())));
        if !self.input.is_empty() {
            f.push(("input", json::s(&self.input)));
        }
        if !self.output.is_empty() {
            f.push(("output", json::s(&self.output)));
        }
        flag(&mut f, "outputTruncated", self.output_truncated);
        opt_str(&mut f, "error", self.error.as_deref());
        diffs_json(&mut f, &self.diffs);
        opt_str(&mut f, "permissionId", self.permission_id.as_deref());
        opt_str(&mut f, "autoApproved", self.auto_approved.as_deref());
        if let Some(d) = self.duration_ms {
            f.push(("durationMs", Json::Int(d)));
        }
        json::obj(f)
    }
}

impl Item {
    pub fn to_json(&self) -> Json {
        let mut f = vec![("id", json::s(&self.id)), ("ts", Json::Int(self.ts))];
        opt_str(&mut f, "turnId", self.turn_id.as_deref());
        opt_str(&mut f, "parentId", self.parent_id.as_deref());
        f.push(("kind", json::s(self.kind.as_str())));
        match self.kind {
            ItemKind::User => {
                f.push(("text", json::s(&self.text)));
                flag(&mut f, "queued", self.queued);
            }
            ItemKind::Text | ItemKind::Thinking => {
                f.push(("text", json::s(&self.text)));
                f.push(("done", Json::Bool(self.done)));
                flag(&mut f, "truncated", self.truncated);
            }
            ItemKind::Notice => {
                f.push(("level", json::s(self.level.unwrap_or("info"))));
                f.push(("text", json::s(&self.text)));
            }
            ItemKind::Tool => {
                let tool = self.tool.clone().unwrap_or_default();
                f.push(("tool", tool.to_json()));
            }
            ItemKind::Plan => {
                let steps = self.steps.iter().map(|s| {
                    json::obj(vec![
                        ("text", json::s(&s.text)),
                        ("status", json::s(s.status)),
                    ])
                });
                f.push(("steps", Json::Arr(steps.collect())));
            }
        }
        json::obj(f)
    }
}

impl TurnState {
    pub fn to_json(&self) -> Json {
        let mut f = vec![
            ("id", json::s(&self.id)),
            ("status", json::s(self.status)),
            ("startedAt", Json::Int(self.started_at)),
        ];
        if let Some(t) = self.ended_at {
            f.push(("endedAt", Json::Int(t)));
        }
        if let Some(u) = &self.usage {
            let mut uf = vec![
                ("inputTokens", Json::Int(u.input)),
                ("outputTokens", Json::Int(u.output)),
            ];
            if let Some(n) = u.cache_read {
                uf.push(("cacheReadTokens", Json::Int(n)));
            }
            if let Some(n) = u.reasoning {
                uf.push(("reasoningTokens", Json::Int(n)));
            }
            f.push(("usage", json::obj(uf)));
        }
        if let Some(c) = self.cost_usd {
            f.push(("costUsd", Json::Float(c)));
        }
        opt_str(&mut f, "error", self.error.as_deref());
        diffs_json(&mut f, &self.diffs);
        json::obj(f)
    }
}

impl LiveState {
    pub fn to_json(&self) -> Json {
        let mut f = vec![("status", json::s(self.status))];
        opt_str(&mut f, "permissionMode", self.permission_mode.as_deref());
        opt_str(&mut f, "model", self.model.as_deref());
        json::obj(f)
    }
}

impl McpServer {
    pub fn to_json(&self) -> Json {
        let mut f = vec![
            ("name", json::s(&self.name)),
            ("status", json::s(self.status)),
        ];
        opt_str(&mut f, "error", self.error.as_deref());
        let flows = self.auth_flows.iter().map(|a| json::s(a)).collect();
        f.push(("authFlows", Json::Arr(flows)));
        if let Some(a) = &self.auth {
            let mut af = vec![("state", json::s(a.state))];
            opt_str(&mut af, "url", a.url.as_deref());
            opt_str(&mut af, "callbackPrefix", a.callback_prefix.as_deref());
            opt_str(&mut af, "error", a.error.as_deref());
            f.push(("auth", json::obj(af)));
        }
        json::obj(f)
    }
}

fn op_text(seq: u64, op: &Op) -> String {
    let seq = ("seq", Json::Int(seq as i64));
    let v = match op {
        Op::Item(item) => json::obj(vec![seq, ("op", json::s("item")), ("item", item.to_json())]),
        Op::Append { id, field, text } => json::obj(vec![
            seq,
            ("op", json::s("append")),
            ("id", json::s(id)),
            ("field", json::s(field.as_str())),
            ("text", json::s(text)),
        ]),
        Op::Turn(turn) => json::obj(vec![seq, ("op", json::s("turn")), ("turn", turn.to_json())]),
        Op::State(state) => json::obj(vec![
            seq,
            ("op", json::s("state")),
            ("state", state.to_json()),
        ]),
        Op::Mcp(servers) => {
            let servers = servers.iter().map(McpServer::to_json).collect();
            json::obj(vec![
                seq,
                ("op", json::s("mcp")),
                ("servers", Json::Arr(servers)),
            ])
        }
    };
    json::stringify(&v)
}

/* ---------------- caps and redaction ---------------- */

/// Bytes `c` takes inside a JSON string (the escapes `json::write_str` uses).
fn char_wire_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 || c == '\u{7f}' => 6,
        c => c.len_utf8(),
    }
}

fn wire_len(s: &str) -> usize {
    s.chars().map(char_wire_len).sum()
}

/// Byte index of the longest prefix of `s` that serializes within `max`.
fn cut_at(s: &str, max: usize) -> usize {
    let mut n = 0;
    for (i, c) in s.char_indices() {
        n += char_wire_len(c);
        if n > max {
            return i;
        }
    }
    s.len()
}

/// Cuts `s` to at most `max` serialized bytes at a char boundary; true when
/// it cut.
fn clip(s: &mut String, max: usize) -> bool {
    let at = cut_at(s, max);
    let cut = at < s.len();
    s.truncate(at);
    cut
}

fn clip_opt(s: &mut Option<String>, max: usize) {
    if let Some(s) = s {
        clip(s, max);
    }
}

/// Redacts, then caps: a cap must never cut a secret in two and leave half
/// of it unrecognised.
fn scrub(s: &mut String, max: usize) -> bool {
    *s = redact(s);
    clip(s, max)
}

/// Keeps the first and last `half` serialized bytes of `s` around a marker;
/// true when it cut.
fn head_tail(s: &mut String, half: usize) -> bool {
    if wire_len(s) <= 2 * half {
        return false;
    }
    let head = cut_at(s, half);
    let (mut n, mut tail) = (0, s.len());
    for (i, c) in s.char_indices().rev() {
        n += char_wire_len(c);
        if n > half {
            break;
        }
        tail = i;
    }
    *s = format!("{}{OUTPUT_MARKER}{}", &s[..head], &s[tail..]);
    true
}

/// Caps a patch at `max` on a line boundary and ends it with the marker
/// line; true when it cut.
fn cap_patch(patch: &mut String, max: usize) -> bool {
    if wire_len(patch) <= max {
        return false;
    }
    clip(patch, max.saturating_sub(wire_len(DIFF_MARKER) + 2));
    match patch.rfind('\n') {
        Some(nl) => patch.truncate(nl + 1),
        None => patch.clear(),
    }
    patch.push_str(DIFF_MARKER);
    true
}

/// 16 files, 64 KiB each, 256 KiB together; past the total a file keeps its
/// counts but only the marker.
fn bound_diffs(diffs: &mut Vec<FileDiff>) {
    diffs.truncate(DIFF_FILES);
    let mut left = DIFFS_TOTAL;
    for d in diffs {
        clip(&mut d.path, LABEL_MAX);
        d.patch = redact(&d.patch);
        if cap_patch(&mut d.patch, DIFF_MAX.min(left)) {
            d.truncated = true;
        }
        left = left.saturating_sub(wire_len(&d.patch));
    }
}

fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= ID_MAX
}

fn opt_id_ok(id: &Option<String>) -> bool {
    id.as_deref().is_none_or(id_ok)
}

/// The cap of the field an append grows; `None` for a field the item kind
/// does not take (dropped, as the phone's reducer would).
fn field_cap(kind: ItemKind, field: Field) -> Option<usize> {
    match (kind, field) {
        (ItemKind::User | ItemKind::Text, Field::Text) => Some(TEXT_MAX),
        (ItemKind::Thinking, Field::Text) => Some(THINKING_MAX),
        (ItemKind::Notice, Field::Text) => Some(NOTICE_MAX),
        (ItemKind::Tool, Field::Input) => Some(INPUT_MAX),
        // streamed output stops at 32 KiB; the final replace keeps head + tail
        (ItemKind::Tool, Field::Output) => Some(2 * OUTPUT_HALF),
        _ => None,
    }
}

/// Applies the §3.5 caps and redaction to one op in place. False: an id is
/// empty or over 256 bytes (the op is dropped and counted as malformed).
fn bound(op: &mut Op) -> bool {
    match op {
        Op::Item(item) => {
            if !id_ok(&item.id) || !opt_id_ok(&item.turn_id) || !opt_id_ok(&item.parent_id) {
                return false;
            }
            let cap = match item.kind {
                ItemKind::User | ItemKind::Text => TEXT_MAX,
                ItemKind::Thinking => THINKING_MAX,
                ItemKind::Notice => NOTICE_MAX,
                ItemKind::Tool | ItemKind::Plan => 0,
            };
            if scrub(&mut item.text, cap) {
                item.truncated = true;
            }
            item.steps.truncate(PLAN_STEPS);
            for step in &mut item.steps {
                scrub(&mut step.text, LABEL_MAX);
            }
            if item.kind != ItemKind::Tool {
                return true;
            }
            let tool = item.tool.get_or_insert_with(ToolCall::default);
            if !opt_id_ok(&tool.permission_id) {
                return false;
            }
            clip(&mut tool.name, LABEL_MAX);
            if let Some(title) = &mut tool.title {
                scrub(title, LABEL_MAX);
            }
            scrub(&mut tool.input, INPUT_MAX);
            tool.output = redact(&tool.output);
            if head_tail(&mut tool.output, OUTPUT_HALF) {
                tool.output_truncated = true;
            }
            if let Some(e) = &mut tool.error {
                scrub(e, ERROR_MAX);
            }
            clip_opt(&mut tool.auto_approved, LABEL_MAX);
            bound_diffs(&mut tool.diffs);
            true
        }
        Op::Turn(turn) => {
            if !id_ok(&turn.id) {
                return false;
            }
            if let Some(e) = &mut turn.error {
                scrub(e, ERROR_MAX);
            }
            bound_diffs(&mut turn.diffs);
            true
        }
        Op::State(state) => {
            clip_opt(&mut state.permission_mode, LABEL_MAX);
            clip_opt(&mut state.model, LABEL_MAX);
            true
        }
        // runtime-produced auth URLs must keep working: capped, not redacted
        Op::Mcp(servers) => {
            servers.truncate(MCP_SERVERS);
            for s in servers {
                clip(&mut s.name, LABEL_MAX);
                clip_opt(&mut s.error, ERROR_MAX);
                if let Some(a) = &mut s.auth {
                    clip_opt(&mut a.url, URL_MAX);
                    clip_opt(&mut a.callback_prefix, URL_MAX);
                    clip_opt(&mut a.error, ERROR_MAX);
                }
            }
            true
        }
        Op::Append { .. } => true,
    }
}

/* ---------------- one session's transcript ---------------- */

/// A compacted item with the serialized length of its text, tool input and
/// tool output (so caps cost O(delta)) and its size for the budgets.
struct Entry {
    item: Item,
    lens: [usize; 3],
    bytes: usize,
}

struct Transcript {
    /// 16 hex chars from the CSPRNG, minted per transcript: a restart or a
    /// re-creation never silently resets `seq` (PRD §14.3).
    epoch: String,
    /// Last assigned op seq (0 = none yet; the first op is 1).
    seq: u64,
    ring: VecDeque<(u64, String)>,
    ring_bytes: usize,
    /// Compacted state as of `seq`.
    items: Vec<Entry>,
    items_bytes: usize,
    turns: Vec<(TurnState, usize)>,
    turns_bytes: usize,
    state: LiveState,
    mcp: Vec<McpServer>,
    truncated_before: bool,
    /// Coalesced deltas not sequenced yet: (item id, field, text, its
    /// serialized length).
    pending: Vec<(String, Field, String, usize)>,
    last_activity: i64,
    ended_at: Option<i64>,
}

fn mint_epoch() -> String {
    let bytes = crate::ffi::csprng_bytes(8).unwrap_or_else(|_| {
        // never panic under the hub lock: a clock-derived epoch still changes
        (now_ms() as u64).to_le_bytes().to_vec()
    });
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Transcript {
    fn new(now: i64, truncated_before: bool) -> Transcript {
        Transcript {
            epoch: mint_epoch(),
            seq: 0,
            ring: VecDeque::new(),
            ring_bytes: 0,
            items: Vec::new(),
            items_bytes: 0,
            turns: Vec::new(),
            turns_bytes: 0,
            state: LiveState::default(),
            mcp: Vec::new(),
            truncated_before,
            pending: Vec::new(),
            last_activity: now,
            ended_at: None,
        }
    }

    fn find(&self, id: &str) -> Option<usize> {
        self.items.iter().rposition(|e| e.item.id == id)
    }

    /// Seq of the oldest op still replayable.
    fn ring_start(&self) -> u64 {
        self.ring.front().map_or(self.seq + 1, |(s, _)| *s)
    }

    /// Assigns the next seq and keeps the op serialized in the ring (oldest
    /// out past 2,048 ops or 1 MiB; the newest always stays). Returns its
    /// size.
    fn record(&mut self, op: &Op) -> usize {
        self.seq += 1;
        let text = op_text(self.seq, op);
        let len = text.len();
        self.ring_bytes += len;
        self.ring.push_back((self.seq, text));
        while self.ring.len() > 1 && (self.ring.len() > RING_OPS || self.ring_bytes > RING_BYTES) {
            if let Some((_, old)) = self.ring.pop_front() {
                self.ring_bytes -= old.len();
            }
        }
        len
    }

    /// Coalesces one redacted delta, capped by what its field still has room
    /// for (the item's text plus what is already pending).
    fn append(&mut self, id: &str, field: Field, text: &str) {
        let Some(at) = self.find(id) else {
            return; // unknown or evicted item: the phone would ignore it too
        };
        let Some(cap) = field_cap(self.items[at].item.kind, field) else {
            return;
        };
        let pending = self.pending.iter_mut().find(|p| p.0 == id && p.1 == field);
        let have = self.items[at].lens[field as usize] + pending.as_ref().map_or(0, |p| p.3);
        let mut text = text.to_string();
        clip(&mut text, cap.saturating_sub(have));
        if text.is_empty() {
            return;
        }
        let len = wire_len(&text);
        match pending {
            Some(p) => {
                p.2.push_str(&text);
                p.3 += len;
            }
            None => self.pending.push((id.to_string(), field, text, len)),
        }
    }

    /// Sequences the coalesced deltas in arrival order: each is redacted
    /// again as one string (a secret split across chunks of this window is
    /// caught), capped, folded into its item, and sent as append ops of at
    /// most 8 KiB.
    fn flush(&mut self) {
        for (id, field, text, _) in std::mem::take(&mut self.pending) {
            let Some(at) = self.find(&id) else { continue };
            let Some(cap) = field_cap(self.items[at].item.kind, field) else {
                continue;
            };
            let mut text = redact(&text);
            let room = cap.saturating_sub(self.items[at].lens[field as usize]);
            clip(&mut text, room);
            if text.is_empty() {
                continue;
            }
            let len = wire_len(&text);
            let entry = &mut self.items[at];
            let full = entry.lens[field as usize] + len >= cap;
            match field {
                Field::Text => {
                    entry.item.text.push_str(&text);
                    entry.item.truncated |= full;
                }
                Field::Input | Field::Output => {
                    let tool = entry.item.tool.get_or_insert_with(ToolCall::default);
                    if field == Field::Input {
                        tool.input.push_str(&text);
                    } else {
                        tool.output.push_str(&text);
                        tool.output_truncated |= full;
                    }
                }
            }
            entry.lens[field as usize] += len;
            entry.bytes += len;
            self.items_bytes += len;
            let mut rest = text.as_str();
            while !rest.is_empty() {
                let cut = cut_at(rest, APPEND_BYTES);
                let op = Op::Append {
                    id: id.clone(),
                    field,
                    text: rest[..cut].to_string(),
                };
                self.record(&op);
                rest = &rest[cut..];
            }
        }
        self.compact();
    }

    /// Sequences one bounded op and folds it into the compacted state.
    fn apply(&mut self, op: Op) {
        let bytes = self.record(&op);
        match op {
            Op::Item(item) => {
                let tool = item.tool.as_ref();
                let lens = [
                    wire_len(&item.text),
                    tool.map_or(0, |t| wire_len(&t.input)),
                    tool.map_or(0, |t| wire_len(&t.output)),
                ];
                let entry = Entry { item, lens, bytes };
                match self.find(&entry.item.id) {
                    Some(at) => {
                        self.items_bytes = self.items_bytes - self.items[at].bytes + bytes;
                        self.items[at] = entry;
                    }
                    None => {
                        self.items_bytes += bytes;
                        self.items.push(entry);
                    }
                }
            }
            Op::Turn(turn) => match self.turns.iter().position(|t| t.0.id == turn.id) {
                Some(i) => {
                    self.turns_bytes = self.turns_bytes - self.turns[i].1 + bytes;
                    self.turns[i] = (turn, bytes);
                }
                None => {
                    self.turns_bytes += bytes;
                    self.turns.push((turn, bytes));
                }
            },
            Op::State(state) => self.state = state,
            Op::Mcp(servers) => self.mcp = servers,
            Op::Append { .. } => {} // deltas take the flush path
        }
        self.compact();
    }

    fn over_budget(&self) -> bool {
        self.items.len() > ITEMS_MAX || self.items_bytes + self.turns_bytes > ITEMS_BYTES
    }

    /// 400 items or 2 MiB (turns included): turns no item references go
    /// first (never the newest: a turn can start before its items), then the
    /// oldest items, each taking along a turn left without items.
    fn compact(&mut self) {
        if !self.over_budget() {
            return;
        }
        let refs: HashSet<&str> = self
            .items
            .iter()
            .filter_map(|e| e.item.turn_id.as_deref())
            .collect();
        let newest = self.turns.len().saturating_sub(1);
        let mut i = 0;
        self.turns.retain(|t| {
            let keep = i == newest || refs.contains(t.0.id.as_str());
            i += 1;
            keep
        });
        self.turns_bytes = self.turns.iter().map(|t| t.1).sum();
        while self.over_budget() && !self.items.is_empty() {
            let gone = self.items.remove(0);
            self.items_bytes -= gone.bytes;
            self.truncated_before = true;
            let Some(turn) = gone.item.turn_id else {
                continue;
            };
            let orphan = !self
                .items
                .iter()
                .any(|e| e.item.turn_id.as_deref() == Some(turn.as_str()));
            let last = self.turns.len().saturating_sub(1);
            if let Some(i) = self.turns.iter().position(|t| t.0.id == turn)
                && orphan
                && i != last
            {
                self.turns_bytes -= self.turns.remove(i).1;
            }
        }
    }

    /// Newest items first until 512 KiB (each with its turn), then in render
    /// order; `truncatedBefore` when older items were left out.
    fn snapshot(&self, session: &str) -> ServerMsg {
        let state = self.state.to_json();
        let mcp = Json::Arr(self.mcp.iter().map(McpServer::to_json).collect());
        let fixed =
            256 + session.len() + json::stringify(&state).len() + json::stringify(&mcp).len();
        let mut room = SNAPSHOT_BYTES.saturating_sub(fixed);
        let mut truncated = self.truncated_before;
        let (mut items, mut turns, mut taken) = (Vec::new(), Vec::new(), Vec::<&str>::new());
        for e in self.items.iter().rev() {
            let turn = e
                .item
                .turn_id
                .as_deref()
                .filter(|id| !taken.contains(id))
                .and_then(|id| self.turns.iter().find(|t| t.0.id == id));
            let cost = e.bytes + turn.map_or(0, |t| t.1) + 2;
            if cost > room {
                truncated = true;
                break;
            }
            room -= cost;
            items.push(e.item.to_json());
            if let Some((t, _)) = turn {
                taken.push(&t.id);
                turns.push(t.to_json());
            }
        }
        items.reverse();
        turns.reverse();
        ServerMsg::TranscriptSnapshot {
            session_id: session.to_string(),
            epoch: self.epoch.clone(),
            seq: self.seq,
            body: json::obj(vec![
                ("items", Json::Arr(items)),
                ("truncatedBefore", Json::Bool(truncated)),
                ("turns", Json::Arr(turns)),
                ("state", state),
                ("mcp", mcp),
            ]),
        }
    }
}

/* ---------------- subscriptions and the pump ---------------- */

struct Sub {
    conn: u64,
    out: Arc<Outbound>,
    session: String,
    /// The phone's view: replay continues it while it still fits the ring.
    epoch: String,
    cursor: u64,
}

/// (connection, session, when) of the snapshots sent within the last second:
/// the rate limit outlives an unsubscribe/resubscribe or eviction churn.
type Recent = Vec<(u64, String, i64)>;

/// Queued size of `plain` bytes once sealed (base64 of ciphertext + tag in
/// `{"n":…,"d":"…"}`); also an upper bound for a loopback plaintext frame.
fn sealed_len(plain: usize) -> usize {
    (plain + 16).div_ceil(3) * 4 + 32
}

/// §3.5 backpressure: bulk depth at most half its cap, and the frame fits in
/// half the bulk byte budget.
fn headroom(out: &Outbound, frame_len: usize) -> bool {
    out.bulk_depth() <= BULK_QUEUE_MAX_MESSAGES / 2
        && out.bulk_bytes() + sealed_len(frame_len) <= BULK_QUEUE_MAX_BYTES / 2
}

/// Sends one subscriber what lies after its cursor, or a snapshot (at most
/// one per second per connection and session) when the cursor is not in the
/// ring: another epoch, fallen behind the ring start, or a fresh view. A
/// connection without headroom is skipped this tick and its cursor waits.
/// False: the connection refused a frame (it is being torn down).
fn serve(sub: &mut Sub, t: &Transcript, recent: &mut Recent, now: i64) -> bool {
    if sub.out.bulk_depth() > BULK_QUEUE_MAX_MESSAGES / 2 {
        return true;
    }
    let start = t.ring_start();
    let replayable = sub.epoch == t.epoch && sub.cursor <= t.seq && sub.cursor + 1 >= start;
    if !replayable {
        if recent
            .iter()
            .any(|(conn, session, _)| *conn == sub.conn && *session == sub.session)
        {
            return true;
        }
        let frame = t.snapshot(&sub.session).to_text();
        if !headroom(&sub.out, frame.len()) {
            return true;
        }
        if sub
            .out
            .enqueue_sealed(frame.into_bytes(), MsgClass::Bulk)
            .is_err()
        {
            return false;
        }
        recent.push((sub.conn, sub.session.clone(), now));
        (sub.epoch, sub.cursor) = (t.epoch.clone(), t.seq);
        return true;
    }
    while sub.cursor < t.seq {
        let (mut batch, mut bytes) = (Vec::new(), 0);
        for (_, op) in t.ring.range((sub.cursor + 1 - start) as usize..) {
            if !batch.is_empty() && (batch.len() == MESSAGE_OPS || bytes + op.len() > MESSAGE_BYTES)
            {
                break;
            }
            bytes += op.len() + 1;
            batch.push(op);
        }
        if !headroom(&sub.out, bytes + 64 + sub.session.len() + t.epoch.len()) {
            return true;
        }
        let sent = batch.len() as u64;
        let frame = ServerMsg::Transcript {
            session_id: sub.session.clone(),
            epoch: t.epoch.clone(),
            ops: batch.into_iter().cloned().collect(),
        };
        if sub
            .out
            .enqueue_sealed(frame.to_text().into_bytes(), MsgClass::Bulk)
            .is_err()
        {
            return false;
        }
        sub.cursor += sent;
    }
    true
}

/// Every session transcript and subscription (ADR-015 §3.3). One instance
/// lives in `Shared::hub`; tests drive it directly with explicit `now`.
#[derive(Default)]
pub struct Hub {
    sessions: HashMap<String, Transcript>,
    subs: Vec<Sub>,
    recent: Recent,
    tick: u64,
    malformed: u64,
}

impl Hub {
    /// Item, turn, state or mcp: flushes the session's pending deltas first
    /// (so a replace is never followed by a stale delta), then sequences the
    /// bounded op. Creates the transcript on its first op (8 at most, the
    /// least recently active goes). A malformed op is dropped and counted.
    pub fn push(&mut self, session: &str, mut op: Op, now: i64) {
        if let Op::Append { id, field, text } = &op {
            return self.append(session, id, *field, text, now);
        }
        if !id_ok(session) || !bound(&mut op) {
            self.malformed += 1;
            return;
        }
        let t = self.open(session, now);
        t.flush();
        t.apply(op);
    }

    /// A streaming delta for an item already pushed: redacted on its own,
    /// capped, coalesced, sent by the next flush.
    pub fn append(&mut self, session: &str, id: &str, field: Field, text: &str, now: i64) {
        if let Some(t) = self.sessions.get_mut(session) {
            t.append(id, field, &redact(text));
            t.last_activity = now;
        }
    }

    /// Read-modify-push of one item (a status change, a permission link):
    /// `f` edits a copy of the compacted item, deltas included, and the copy
    /// is pushed as an authoritative replace. False when the item is unknown
    /// or was evicted. `f` runs under the hub lock: it must not lock.
    pub fn update(&mut self, session: &str, id: &str, f: impl FnOnce(&mut Item), now: i64) -> bool {
        let Some(t) = self.sessions.get_mut(session) else {
            return false;
        };
        t.flush();
        let Some(at) = t.find(id) else { return false };
        let mut item = t.items[at].item.clone();
        f(&mut item);
        self.push(session, Op::Item(item), now);
        true
    }

    /// Marks the transcript ended (state `ended` unless already) and starts
    /// its one-hour collection clock.
    pub fn end_session(&mut self, session: &str, now: i64) {
        let Some(t) = self.sessions.get_mut(session) else {
            return;
        };
        t.flush();
        if t.state.status != "ended" {
            let state = LiveState {
                status: "ended",
                ..t.state.clone()
            };
            t.apply(Op::State(state));
        }
        t.ended_at = Some(now);
        t.last_activity = now;
    }

    /// Creates an empty transcript for a session a runtime owns, so its first
    /// subscriber can attach before any op (the runtime hydrates it after).
    pub fn ensure(&mut self, session: &str, now: i64) {
        if id_ok(session) {
            self.open(session, now);
        }
    }

    pub fn has(&self, session: &str) -> bool {
        self.sessions.contains_key(session)
    }

    pub fn is_subscribed(&self, session: &str) -> bool {
        self.subs.iter().any(|s| s.session == session)
    }

    /// Adds this connection's subscription, or only moves its cursor when it
    /// already has one for the session (a replayed or repeated subscribe
    /// never doubles the stream). `resume` is the phone's (epoch, afterSeq):
    /// `pump` replays after it while it fits the ring, else sends a snapshot.
    /// A fifth subscription on one connection evicts its oldest. Ok(true):
    /// the session's first subscriber.
    pub fn subscribe(
        &mut self,
        conn: u64,
        out: Arc<Outbound>,
        session: &str,
        resume: Option<(String, u64)>,
    ) -> Result<bool, &'static str> {
        if !self.has(session) {
            return Err("unknown session");
        }
        let (epoch, cursor) = resume.unwrap_or_default();
        if let Some(sub) = self
            .subs
            .iter_mut()
            .find(|s| s.conn == conn && s.session == session)
        {
            (sub.epoch, sub.cursor) = (epoch, cursor);
            return Ok(false);
        }
        let first = !self.is_subscribed(session);
        if self.subs.iter().filter(|s| s.conn == conn).count() >= SUBS_PER_CONN
            && let Some(oldest) = self.subs.iter().position(|s| s.conn == conn)
        {
            self.subs.remove(oldest);
        }
        self.subs.push(Sub {
            conn,
            out,
            session: session.to_string(),
            epoch,
            cursor,
        });
        Ok(first)
    }

    /// Drops this connection's subscription to `session`, or all of them
    /// (`None`: the connection is gone or revoked).
    pub fn unsubscribe(&mut self, conn: u64, session: Option<&str>) {
        self.subs
            .retain(|s| s.conn != conn || session.is_some_and(|x| x != s.session));
        if session.is_none() {
            self.recent.retain(|r| r.0 != conn);
        }
    }

    /// One scheduler tick (50 ms): flushes the deltas every second tick,
    /// collects transcripts ended an hour ago (and their subscriptions), then
    /// serves every subscriber with headroom. Subscriptions of a torn-down
    /// connection go too.
    pub fn pump(&mut self, now: i64) {
        self.tick += 1;
        if self.tick.is_multiple_of(2) {
            for t in self.sessions.values_mut() {
                t.flush();
            }
        }
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, t)| t.ended_at.is_some_and(|at| now - at >= ENDED_TTL_MS))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            self.sessions.remove(id);
        }
        self.recent.retain(|r| now - r.2 < SNAPSHOT_EVERY_MS);
        let Hub {
            sessions,
            subs,
            recent,
            ..
        } = self;
        subs.retain_mut(|sub| {
            if sub.out.is_terminated() || expired.contains(&sub.session) {
                return false;
            }
            sessions
                .get(&sub.session)
                .is_none_or(|t| serve(sub, t, recent, now))
        });
    }

    /// (transcripts, subscriptions, malformed runtime input) for `/health`.
    pub fn stats(&self) -> (usize, usize, u64) {
        (self.sessions.len(), self.subs.len(), self.malformed)
    }

    /// Counts one malformed runtime frame (`runtimeMalformed`).
    pub fn malformed(&mut self) {
        self.malformed += 1;
    }

    /// The session's transcript, created when missing.
    fn open(&mut self, session: &str, now: i64) -> &mut Transcript {
        if !self.sessions.contains_key(session) {
            if self.sessions.len() >= SESSIONS_MAX
                && let Some(lru) = self
                    .sessions
                    .iter()
                    .min_by_key(|(_, t)| t.last_activity)
                    .map(|(id, _)| id.clone())
            {
                self.sessions.remove(&lru);
            }
            // subscribers saw history this new transcript does not have
            let watched = self.is_subscribed(session);
            self.sessions
                .insert(session.to_string(), Transcript::new(now, watched));
        }
        let t = self.sessions.get_mut(session).expect("inserted above");
        t.last_activity = now;
        t
    }
}

/// What a live runtime holds to stream its sessions (ADR-015 §3.3); the
/// companion hands it over at construction (`Companion::hub`). Clone it
/// freely: every call locks the hub, works and returns, so a runtime can
/// never hold the hub lock across its own work. Never call it while holding
/// the engine lock or a lock the runtime's other paths wait on with the hub
/// held (there are none: the hub calls nothing).
///
/// `HubHandle::default()` is a detached hub nobody subscribes to, for unit
/// tests and runtimes built without a companion.
#[derive(Clone, Default)]
pub struct HubHandle(Arc<Mutex<Hub>>);

impl HubHandle {
    pub fn new(hub: Arc<Mutex<Hub>>) -> HubHandle {
        HubHandle(hub)
    }

    fn hub(&self) -> std::sync::MutexGuard<'_, Hub> {
        self.0.lock().unwrap()
    }

    /// Inserts or replaces an item (always the whole item), upserts a turn,
    /// replaces the live state or the MCP list. See [`Hub::push`].
    pub fn push(&self, session: &str, op: Op) {
        self.hub().push(session, op, now_ms());
    }

    /// Streams a delta into an item already pushed. See [`Hub::append`].
    pub fn append(&self, session: &str, id: &str, field: Field, text: &str) {
        self.hub().append(session, id, field, text, now_ms());
    }

    /// Changes fields of a pushed item without keeping a copy of it. See
    /// [`Hub::update`].
    pub fn update(&self, session: &str, id: &str, f: impl FnOnce(&mut Item)) -> bool {
        self.hub().update(session, id, f, now_ms())
    }

    /// The session is over: state `ended`, transcript collected after 1 h.
    pub fn end_session(&self, session: &str) {
        self.hub().end_session(session, now_ms());
    }

    /// True while a connection watches the session (MCP polling, reaping).
    pub fn is_subscribed(&self, session: &str) -> bool {
        self.hub().is_subscribed(session)
    }

    /// True while the session has a transcript or a subscriber. A runtime
    /// that streams on demand (OpenCode sees every session of its server)
    /// pushes only then, so unwatched sessions cost no hub memory.
    pub fn streams(&self, session: &str) -> bool {
        let hub = self.hub();
        hub.has(session) || hub.is_subscribed(session)
    }

    /// Counts a malformed runtime frame on `/health` (`runtimeMalformed`).
    pub fn malformed(&self) {
        self.hub().malformed();
    }

    /// True when the session's transcript holds at least one item.
    pub fn has_items(&self, session: &str) -> bool {
        self.hub()
            .sessions
            .get(session)
            .is_some_and(|t| !t.items.is_empty())
    }

    /// Test read-back for runtime tests: flushes the deltas and returns the
    /// session's ops as serialized (oldest first).
    #[cfg(test)]
    pub(crate) fn ops(&self, session: &str) -> Vec<String> {
        let mut hub = self.hub();
        let Some(t) = hub.sessions.get_mut(session) else {
            return Vec::new();
        };
        t.flush();
        t.ring.iter().map(|(_, op)| op.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::Saturation;
    use std::sync::mpsc::Receiver;

    const S: &str = "ses_live_1";

    macro_rules! fixture {
        ($name:literal) => {
            json::parse(include_str!(concat!(
                "../../../packages/protocol/fixtures/live/",
                $name
            )))
            .unwrap()
        };
    }

    /// Key order is not part of the wire contract: compare objects sorted.
    fn canon(v: &Json) -> Json {
        match v {
            Json::Obj(pairs) => {
                let mut sorted: Vec<(String, Json)> =
                    pairs.iter().map(|(k, v)| (k.clone(), canon(v))).collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                Json::Obj(sorted)
            }
            Json::Arr(items) => Json::Arr(items.iter().map(canon).collect()),
            other => other.clone(),
        }
    }

    fn outbound() -> (Arc<Outbound>, Receiver<(MsgClass, Vec<u8>)>) {
        Outbound::new(Arc::new(Saturation::default()), false)
    }

    /// Every frame queued for `out`, parsed, with the writer's accounting.
    fn drain(out: &Outbound, rx: &Receiver<(MsgClass, Vec<u8>)>) -> Vec<Json> {
        let mut frames = Vec::new();
        while let Some((_, payload)) = out.test_dequeue(rx) {
            frames.push(json::parse(std::str::from_utf8(&payload).unwrap()).unwrap());
        }
        frames
    }

    fn text_item(id: &str, text: &str) -> Item {
        Item {
            id: id.into(),
            kind: ItemKind::Text,
            text: text.into(),
            ..Default::default()
        }
    }

    fn state(status: &'static str) -> Op {
        Op::State(LiveState {
            status,
            ..Default::default()
        })
    }

    fn ring(hub: &Hub, session: &str) -> Vec<Json> {
        let t = &hub.sessions[session];
        t.ring
            .iter()
            .map(|(_, op)| json::parse(op).unwrap())
            .collect()
    }

    fn seqs(frame: &Json) -> Vec<i64> {
        json::as_arr(json::get(frame, "ops").unwrap())
            .unwrap()
            .iter()
            .map(|op| json::get_i64(op, "seq").unwrap())
            .collect()
    }

    fn kind(frame: &Json) -> &str {
        json::get_str(frame, "type").unwrap()
    }

    /// Two pumps: the second tick flushes the deltas.
    fn flush(hub: &mut Hub, now: i64) {
        hub.pump(now);
        hub.pump(now);
    }

    #[test]
    fn ring_evicts_by_ops_and_bytes() {
        let mut hub = Hub::default();
        for _ in 0..RING_OPS + 10 {
            hub.push(S, state("running"), 0);
        }
        let t = &hub.sessions[S];
        assert_eq!((t.ring.len(), t.seq), (RING_OPS, (RING_OPS + 10) as u64));
        assert_eq!(t.ring_start(), 11, "the oldest ops went first");
        // ~200 KiB ops: the byte budget holds five of them, nothing older
        for i in 0..8 {
            let big = text_item(&format!("big{i}"), &"x".repeat(200 << 10));
            hub.push(S, Op::Item(big), 0);
        }
        let t = &hub.sessions[S];
        assert!(t.ring_bytes <= RING_BYTES);
        assert_eq!(t.ring_bytes, t.ring.iter().map(|(_, op)| op.len()).sum());
        let ids: Vec<String> = ring(&hub, S)
            .iter()
            .map(|op| {
                json::get_str(json::get(op, "item").unwrap(), "id")
                    .unwrap()
                    .into()
            })
            .collect();
        assert_eq!(ids, ["big3", "big4", "big5", "big6", "big7"]);
    }

    #[test]
    fn deltas_coalesce_within_a_flush() {
        let mut hub = Hub::default();
        hub.push(S, Op::Item(text_item("t1", "")), 0);
        hub.append(S, "t1", Field::Text, "Hel", 0);
        hub.append(S, "t1", Field::Text, "lo", 0);
        hub.pump(0); // first tick: no flush yet
        assert_eq!(hub.sessions[S].seq, 1);
        hub.append(S, "t1", Field::Text, " world", 0);
        hub.pump(0);
        let ops = ring(&hub, S);
        assert_eq!(ops.len(), 2, "three deltas, one op");
        assert_eq!(json::get_str(&ops[1], "op"), Some("append"));
        assert_eq!(json::get_str(&ops[1], "text"), Some("Hello world"));
        assert_eq!(hub.sessions[S].items[0].item.text, "Hello world");
        // a delta longer than 8 KiB becomes several ops of at most 8 KiB
        hub.append(S, "t1", Field::Text, &"y".repeat(20 << 10), 0);
        flush(&mut hub, 0);
        let ops = ring(&hub, S);
        let sizes: Vec<usize> = ops[2..]
            .iter()
            .map(|op| json::get_str(op, "text").unwrap().len())
            .collect();
        assert_eq!(sizes, [8 << 10, 8 << 10, 4 << 10]);
        // a field the item kind does not take is dropped
        hub.append(S, "t1", Field::Output, "nope", 0);
        hub.append(S, "missing", Field::Text, "nope", 0);
        flush(&mut hub, 0);
        assert_eq!(ring(&hub, S).len(), 5);
    }

    #[test]
    fn item_replace_flushes_pending_deltas_first() {
        let mut hub = Hub::default();
        hub.push(S, Op::Item(text_item("t1", "")), 0);
        hub.append(S, "t1", Field::Text, "abc", 0);
        let done = Item {
            done: true,
            ..text_item("t1", "abcdef")
        };
        hub.push(S, Op::Item(done), 0);
        let ops = ring(&hub, S);
        let shape: Vec<(i64, &str)> = ops
            .iter()
            .map(|op| {
                (
                    json::get_i64(op, "seq").unwrap(),
                    json::get_str(op, "op").unwrap(),
                )
            })
            .collect();
        assert_eq!(shape, [(1, "item"), (2, "append"), (3, "item")]);
        flush(&mut hub, 0);
        assert_eq!(hub.sessions[S].seq, 3, "nothing stale follows the replace");
        let t = &hub.sessions[S];
        assert_eq!(
            (t.items[0].item.text.as_str(), t.items[0].item.done),
            ("abcdef", true)
        );
        // update: read-modify-push without the runtime keeping a copy
        assert!(hub.update(S, "t1", |item| item.text.push('!'), 0));
        assert!(!hub.update(S, "gone", |_| {}, 0));
        let ops = ring(&hub, S);
        let last = json::get(&ops[3], "item").unwrap();
        assert_eq!(json::get_str(last, "text"), Some("abcdef!"));
    }

    #[test]
    fn subscribe_with_current_epoch_replays_after_cursor() {
        let mut hub = Hub::default();
        for _ in 0..10 {
            hub.push(S, state("running"), 0);
        }
        let epoch = hub.sessions[S].epoch.clone();
        assert_eq!(epoch.len(), 16);
        assert!(epoch.bytes().all(|b| b.is_ascii_hexdigit()));
        let (out, rx) = outbound();
        assert_eq!(
            hub.subscribe(1, out.clone(), S, Some((epoch.clone(), 6))),
            Ok(true)
        );
        hub.pump(0);
        let frames = drain(&out, &rx);
        assert_eq!(frames.len(), 1);
        assert_eq!(kind(&frames[0]), "transcript");
        assert_eq!(json::get_str(&frames[0], "epoch"), Some(epoch.as_str()));
        assert_eq!(seqs(&frames[0]), [7, 8, 9, 10]);
        // live: the next op follows on the next tick, nothing is repeated
        hub.push(S, state("idle"), 0);
        hub.pump(0);
        let frames = drain(&out, &rx);
        assert_eq!(frames.len(), 1);
        assert_eq!(seqs(&frames[0]), [11]);
        hub.pump(0);
        assert!(drain(&out, &rx).is_empty());
        // a second subscriber is not the first; an unknown session is refused
        let (other, _rx2) = outbound();
        assert_eq!(hub.subscribe(2, other, S, None), Ok(false));
        let (stray, _rx3) = outbound();
        assert_eq!(
            hub.subscribe(3, stray, "nope", None),
            Err("unknown session")
        );
    }

    #[test]
    fn stale_cursor_or_new_epoch_gets_snapshot() {
        let mut hub = Hub::default();
        for _ in 0..3000 {
            hub.push(S, state("running"), 0);
        }
        let epoch = hub.sessions[S].epoch.clone();
        let cases = [
            Some((epoch.clone(), 10)),       // behind the ring start
            Some(("0123abcd".into(), 2999)), // another epoch
            None,                            // a fresh view
        ];
        for (conn, resume) in cases.into_iter().enumerate() {
            let (out, rx) = outbound();
            hub.subscribe(conn as u64, out.clone(), S, resume).unwrap();
            hub.pump(0);
            let frames = drain(&out, &rx);
            assert_eq!(frames.len(), 1, "conn {conn}");
            assert_eq!(kind(&frames[0]), "transcript_snapshot");
            assert_eq!(json::get_i64(&frames[0], "seq"), Some(3000));
            // re-subscribing only moves the cursor: a valid one replays…
            hub.subscribe(conn as u64, out.clone(), S, Some((epoch.clone(), 2995)))
                .unwrap();
            hub.pump(500);
            let frames = drain(&out, &rx);
            assert_eq!(seqs(&frames[0]), [2996, 2997, 2998, 2999, 3000]);
            // …a stale one waits for the 1 s snapshot rate limit
            hub.subscribe(conn as u64, out.clone(), S, None).unwrap();
            hub.pump(999);
            assert!(drain(&out, &rx).is_empty(), "rate limited");
            hub.pump(1000);
            assert_eq!(kind(&drain(&out, &rx)[0]), "transcript_snapshot");
            // the limit is per (connection, session): resubscribing keeps it
            hub.unsubscribe(conn as u64, Some(S));
            hub.subscribe(conn as u64, out.clone(), S, None).unwrap();
            hub.pump(1500);
            assert!(drain(&out, &rx).is_empty(), "still rate limited");
            hub.pump(2000);
            assert_eq!(kind(&drain(&out, &rx)[0]), "transcript_snapshot");
            hub.unsubscribe(conn as u64, None);
        }
        assert_eq!(hub.stats().1, 0);
    }

    #[test]
    fn snapshot_is_bounded_and_marks_truncation() {
        let mut hub = Hub::default();
        for i in 0..3 {
            hub.push(
                S,
                Op::Item(text_item(&format!("big{i}"), &"x".repeat(200 << 10))),
                0,
            );
        }
        hub.push(S, Op::Item(text_item("small", "hi")), 0);
        let snap = hub.sessions[S].snapshot(S).to_text();
        assert!(snap.len() <= SNAPSHOT_BYTES, "{}", snap.len());
        let snap = json::parse(&snap).unwrap();
        let ids: Vec<&str> = json::as_arr(json::get(&snap, "items").unwrap())
            .unwrap()
            .iter()
            .map(|i| json::get_str(i, "id").unwrap())
            .collect();
        assert_eq!(
            ids,
            ["big1", "big2", "small"],
            "newest that fit, in render order"
        );
        assert_eq!(json::get(&snap, "truncatedBefore"), Some(&Json::Bool(true)));
        // the compacted state itself keeps 400 items
        let mut hub = Hub::default();
        for i in 0..ITEMS_MAX + 5 {
            hub.push(S, Op::Item(text_item(&format!("i{i}"), "x")), 0);
        }
        let t = &hub.sessions[S];
        assert_eq!(t.items.len(), ITEMS_MAX);
        assert_eq!(t.items[0].item.id, "i5");
        assert!(t.truncated_before);
        let snap = json::parse(&t.snapshot(S).to_text()).unwrap();
        assert_eq!(json::get(&snap, "truncatedBefore"), Some(&Json::Bool(true)));
        assert_eq!(
            json::as_arr(json::get(&snap, "items").unwrap())
                .unwrap()
                .len(),
            ITEMS_MAX
        );
    }

    #[test]
    fn pump_skips_connections_above_half_bulk_depth() {
        let mut hub = Hub::default();
        hub.push(S, state("running"), 0);
        let (busy, busy_rx) = outbound();
        let (idle, idle_rx) = outbound();
        for _ in 0..=BULK_QUEUE_MAX_MESSAGES / 2 {
            busy.enqueue(b"{}".to_vec(), MsgClass::Bulk).unwrap();
        }
        assert_eq!(busy.bulk_depth(), 97);
        hub.subscribe(1, busy.clone(), S, None).unwrap();
        hub.subscribe(2, idle.clone(), S, None).unwrap();
        hub.pump(0);
        assert_eq!(kind(&drain(&idle, &idle_rx)[0]), "transcript_snapshot");
        assert_eq!(
            drain(&busy, &busy_rx).len(),
            97,
            "only the filler, nothing added"
        );
        assert!(!busy.is_terminated(), "skipped, never torn down");
        // once drained it is served like any other connection
        hub.pump(0);
        assert_eq!(kind(&drain(&busy, &busy_rx)[0]), "transcript_snapshot");
    }

    #[test]
    fn lagging_subscriber_recovers_with_snapshot() {
        let mut hub = Hub::default();
        hub.push(S, state("running"), 0);
        let (out, rx) = outbound();
        hub.subscribe(1, out.clone(), S, None).unwrap();
        hub.pump(0);
        assert_eq!(kind(&drain(&out, &rx)[0]), "transcript_snapshot");
        // the phone stalls while the ring moves past its cursor
        for _ in 0..=BULK_QUEUE_MAX_MESSAGES / 2 {
            out.enqueue(b"{}".to_vec(), MsgClass::Bulk).unwrap();
        }
        for _ in 0..RING_OPS + 50 {
            hub.push(S, state("running"), 0);
        }
        hub.pump(2000);
        assert_eq!(drain(&out, &rx).len(), 97, "nothing sent while lagging");
        hub.pump(2000);
        let frames = drain(&out, &rx);
        assert_eq!(frames.len(), 1);
        assert_eq!(kind(&frames[0]), "transcript_snapshot");
        assert_eq!(
            json::get_i64(&frames[0], "seq"),
            Some((RING_OPS + 51) as i64)
        );
        // and live ops continue from the snapshot's seq
        hub.push(S, state("idle"), 2000);
        hub.pump(2000);
        assert_eq!(seqs(&drain(&out, &rx)[0]), [(RING_OPS + 52) as i64]);
    }

    #[test]
    fn fifth_subscription_evicts_oldest() {
        let mut hub = Hub::default();
        for i in 1..=5 {
            hub.push(&format!("s{i}"), state("idle"), i);
        }
        let (out, _rx) = outbound();
        let (other, _rx2) = outbound();
        hub.subscribe(9, other, "s1", None).unwrap();
        for i in 1..=5 {
            hub.subscribe(1, out.clone(), &format!("s{i}"), None)
                .unwrap();
        }
        let mine: Vec<&str> = hub
            .subs
            .iter()
            .filter(|s| s.conn == 1)
            .map(|s| s.session.as_str())
            .collect();
        assert_eq!(mine, ["s2", "s3", "s4", "s5"]);
        assert!(
            hub.is_subscribed("s1"),
            "another connection's subscription stays"
        );
        hub.unsubscribe(1, Some("s3"));
        assert_eq!(hub.stats().1, 4);
    }

    #[test]
    fn ended_transcript_is_collected_after_ttl() {
        let mut hub = Hub::default();
        hub.push(S, state("running"), 0);
        let (out, rx) = outbound();
        hub.subscribe(1, out.clone(), S, None).unwrap();
        hub.end_session(S, 1000);
        let last = ring(&hub, S).pop().unwrap();
        assert_eq!(
            json::get_str(json::get(&last, "state").unwrap(), "status"),
            Some("ended")
        );
        hub.pump(1000 + ENDED_TTL_MS - 1);
        assert_eq!(hub.stats(), (1, 1, 0));
        assert!(!drain(&out, &rx).is_empty());
        hub.pump(1000 + ENDED_TTL_MS);
        assert_eq!(
            hub.stats(),
            (0, 0, 0),
            "transcript and its subscription gone"
        );
        // and the session limit: the least recently active of 9 goes
        for i in 0..=SESSIONS_MAX {
            hub.push(&format!("s{i}"), state("idle"), i as i64);
        }
        assert_eq!(hub.stats().0, SESSIONS_MAX);
        assert!(!hub.has("s0") && hub.has("s8"));
    }

    #[test]
    fn tool_output_keeps_head_and_tail() {
        let mut hub = Hub::default();
        let output = format!(
            "{}{}{}",
            "A".repeat(20 << 10),
            "B".repeat(30 << 10),
            "C".repeat(20 << 10)
        );
        let tool = Item {
            id: "tool-1".into(),
            kind: ItemKind::Tool,
            tool: Some(ToolCall {
                name: "Bash".into(),
                status: ToolStatus::Completed,
                output,
                ..Default::default()
            }),
            ..Default::default()
        };
        hub.push(S, Op::Item(tool), 0);
        let op = ring(&hub, S).pop().unwrap();
        let tool = json::get(json::get(&op, "item").unwrap(), "tool").unwrap();
        let kept = json::get_str(tool, "output").unwrap();
        let want = format!(
            "{}{OUTPUT_MARKER}{}",
            "A".repeat(16 << 10),
            "C".repeat(16 << 10)
        );
        assert_eq!(kept, want);
        assert_eq!(json::get(tool, "outputTruncated"), Some(&Json::Bool(true)));
        // streamed output stops at 32 KiB and is marked
        let streaming = Item {
            id: "tool-2".into(),
            kind: ItemKind::Tool,
            ..Default::default()
        };
        hub.push(S, Op::Item(streaming), 0);
        for _ in 0..40 {
            hub.append(S, "tool-2", Field::Output, &"o".repeat(1 << 10), 0);
        }
        flush(&mut hub, 0);
        let t = &hub.sessions[S];
        let streamed = t.items[1].item.tool.as_ref().unwrap();
        assert_eq!(streamed.output.len(), 2 * OUTPUT_HALF);
        assert!(streamed.output_truncated);
    }

    #[test]
    fn diff_is_capped_with_marker() {
        let mut hub = Hub::default();
        let patch = format!("@@ -1 +1 @@\n{}", "+a line of code\n".repeat(8000));
        let diff = |i: usize| FileDiff {
            path: format!("src/f{i}.rs"),
            status: "modified",
            additions: 8000,
            deletions: 0,
            patch: patch.clone(),
            truncated: false,
        };
        let tool = Item {
            id: "edit-1".into(),
            kind: ItemKind::Tool,
            tool: Some(ToolCall {
                name: "Edit".into(),
                diffs: (0..20).map(diff).collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        hub.push(S, Op::Item(tool), 0);
        let t = &hub.sessions[S];
        let diffs = &t.items[0].item.tool.as_ref().unwrap().diffs;
        assert_eq!(diffs.len(), DIFF_FILES);
        let first = &diffs[0];
        assert!(first.truncated && first.patch.ends_with(&format!("\n{DIFF_MARKER}")));
        assert!(wire_len(&first.patch) <= DIFF_MAX);
        assert!(
            first.patch.lines().rev().nth(1).unwrap().starts_with('+'),
            "whole lines kept"
        );
        assert_eq!(first.additions, 8000, "counts stay the runtime's");
        let total: usize = diffs.iter().map(|d| wire_len(&d.patch)).sum();
        assert!(
            total <= DIFFS_TOTAL + DIFF_FILES * wire_len(DIFF_MARKER),
            "{total}"
        );
        assert_eq!(
            diffs[DIFF_FILES - 1].patch,
            DIFF_MARKER,
            "past the total: marker only"
        );
        // turns get the same caps
        let turn = TurnState {
            id: "turn-1".into(),
            status: "completed",
            diffs: vec![diff(0)],
            ..Default::default()
        };
        hub.push(S, Op::Turn(turn), 0);
        assert!(hub.sessions[S].turns[0].0.diffs[0].truncated);
    }

    #[test]
    fn hub_redacts_secrets_in_items_tools_diffs_notices_and_deltas() {
        // built at run time, so no key-shaped literal sits in the source
        let key = format!("sk-ant-api03-{}", "aB3dE5fG7h".repeat(4));
        let body = "aB3dE5fG7haB3dE5fG7h";
        let mut hub = Hub::default();
        hub.push(S, Op::Item(text_item("final", &format!("done: {key}"))), 0);
        let notice = Item {
            id: "n".into(),
            kind: ItemKind::Notice,
            text: format!("retrying with {key}"),
            ..Default::default()
        };
        hub.push(S, Op::Item(notice), 0);
        let tool = Item {
            id: "bash".into(),
            kind: ItemKind::Tool,
            tool: Some(ToolCall {
                name: "Bash".into(),
                title: Some(format!("echo {key}")),
                input: format!("{{\"command\":\"echo {key}\"}}"),
                output: format!("{key}\n"),
                error: Some(format!("failed: {key}")),
                diffs: vec![FileDiff {
                    path: ".env".into(),
                    status: "modified",
                    patch: format!("@@ -1 +1 @@\n+ANTHROPIC={key}\n"),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        hub.push(S, Op::Item(tool), 0);
        // a delta chunk is redacted on its own…
        hub.push(S, Op::Item(text_item("stream", "")), 0);
        hub.append(S, "stream", Field::Text, &format!("chunk {key} "), 0);
        flush(&mut hub, 0);
        // …and a secret split across two chunks of one flush is caught there
        hub.append(S, "stream", Field::Text, &key[..10], 0);
        hub.append(S, "stream", Field::Text, &key[10..], 0);
        flush(&mut hub, 0);
        let all: String = hub.sessions[S]
            .ring
            .iter()
            .map(|(_, op)| op.as_str())
            .collect();
        assert!(!all.contains(body), "a secret reached the ring: {all}");
        assert_eq!(all.matches("«redacted:anthropic-key»").count(), 9, "{all}");
        let snap = hub.sessions[S].snapshot(S).to_text();
        assert!(!snap.contains(body));
    }

    #[test]
    fn malformed_ops_are_dropped_and_counted() {
        let mut hub = Hub::default();
        hub.push(S, Op::Item(text_item("", "no id")), 0);
        hub.push(
            S,
            Op::Item(text_item(&"i".repeat(ID_MAX + 1), "long id")),
            0,
        );
        hub.push(&"s".repeat(ID_MAX + 1), state("idle"), 0);
        assert_eq!(hub.stats(), (0, 0, 3));
        // runtimes count what they could not parse through the handle
        let handle = HubHandle::new(Arc::new(Mutex::new(hub)));
        handle.malformed();
        assert_eq!(handle.hub().stats(), (0, 0, 4));
    }

    #[test]
    fn ops_and_snapshot_match_the_golden_fixtures() {
        const T0: i64 = 1_761_436_800_000;
        let turn_id = Some("turn_1".to_string());
        let item = |id: &str, ts: i64, kind: ItemKind| Item {
            id: id.into(),
            ts: T0 + ts,
            turn_id: turn_id.clone(),
            kind,
            ..Default::default()
        };
        let live = |status| LiveState {
            status,
            permission_mode: Some("default".into()),
            model: Some("claude-haiku-4-5".into()),
        };
        // transcript-snapshot.json: what a subscriber sees for this state
        let hub = HubHandle::default();
        hub.push(
            S,
            Op::Item(Item {
                text: "Create a.txt containing hi".into(),
                ..item("u_1", 0, ItemKind::User)
            }),
        );
        hub.push(
            S,
            Op::Item(Item {
                text: "The user wants one small file.".into(),
                done: true,
                ..item("msg_00#0", 50, ItemKind::Thinking)
            }),
        );
        let steps = vec![
            PlanStep {
                text: "Write a.txt".into(),
                status: "in_progress",
            },
            PlanStep {
                text: "Confirm the content".into(),
                status: "pending",
            },
        ];
        hub.push(
            S,
            Op::Item(Item {
                steps,
                ..item("plan", 60, ItemKind::Plan)
            }),
        );
        hub.push(
            S,
            Op::Item(Item {
                level: Some("warning"),
                text: "Retrying: overloaded".into(),
                ..item("n_1", 70, ItemKind::Notice)
            }),
        );
        hub.push(
            S,
            Op::Turn(TurnState {
                id: "turn_1".into(),
                status: "running",
                started_at: T0,
                ..Default::default()
            }),
        );
        hub.push(S, Op::State(live("running")));
        let golden = fixture!("transcript-snapshot.json");
        let snap = json::parse(&hub.0.lock().unwrap().sessions[S].snapshot(S).to_text()).unwrap();
        for key in [
            "type",
            "sessionId",
            "items",
            "truncatedBefore",
            "turns",
            "state",
            "mcp",
        ] {
            let (got, want) = (
                json::get(&snap, key).unwrap(),
                json::get(&golden, key).unwrap(),
            );
            assert_eq!(canon(got), canon(want), "{key}");
        }
        assert_eq!(json::get_i64(&snap, "seq"), Some(6));

        // transcript.json: ops 4..10 of a session, as the ring keeps them
        let hub = HubHandle::default();
        for _ in 0..3 {
            hub.push(S, Op::State(live("running")));
        }
        hub.push(
            S,
            Op::Item(Item {
                text: "Creating".into(),
                ..item("msg_01#0", 100, ItemKind::Text)
            }),
        );
        hub.append(S, "msg_01#0", Field::Text, " a.txt now.");
        let tool = ToolCall {
            name: "Write".into(),
            title: Some("a.txt".into()),
            status: ToolStatus::AwaitingApproval,
            input: r#"{"file_path":"a.txt","content":"hi"}"#.into(),
            permission_id: Some("claude-1:toolu_01".into()),
            ..Default::default()
        };
        hub.push(
            S,
            Op::Item(Item {
                tool: Some(tool),
                ..item("toolu_01", 200, ItemKind::Tool)
            }),
        );
        hub.append(S, "toolu_01", Field::Output, "File created successfully");
        let diff = FileDiff {
            path: "a.txt".into(),
            status: "added",
            additions: 1,
            deletions: 0,
            patch: "@@ -0,0 +1 @@\n+hi".into(),
            truncated: false,
        };
        hub.push(
            S,
            Op::Turn(TurnState {
                id: "turn_1".into(),
                status: "completed",
                started_at: T0,
                ended_at: Some(T0 + 12_400),
                usage: Some(Usage {
                    input: 4210,
                    output: 96,
                    cache_read: Some(3900),
                    reasoning: None,
                }),
                cost_usd: Some(0.0123),
                error: None,
                diffs: vec![diff],
            }),
        );
        hub.push(
            S,
            Op::State(LiveState {
                status: "idle",
                ..live("idle")
            }),
        );
        hub.push(
            S,
            Op::Mcp(vec![
                McpServer {
                    name: "sentry".into(),
                    status: "needs-auth",
                    auth_flows: vec!["desktop", "phone-callback"],
                    ..Default::default()
                },
                McpServer {
                    name: "linear".into(),
                    status: "pending",
                    auth_flows: vec!["phone-callback"],
                    auth: Some(McpAuthState {
                        state: "awaiting-callback",
                        url: Some("https://linear.app/oauth/authorize?client_id=fixture".into()),
                        callback_prefix: Some("http://localhost:44818/callback".into()),
                        error: None,
                    }),
                    ..Default::default()
                },
                McpServer {
                    name: "files".into(),
                    status: "connected",
                    ..Default::default()
                },
            ]),
        );
        let ops: Vec<Json> = hub.ops(S)[3..]
            .iter()
            .map(|op| json::parse(op).unwrap())
            .collect();
        let golden = fixture!("transcript.json");
        assert_eq!(
            canon(&Json::Arr(ops.clone())),
            canon(json::get(&golden, "ops").unwrap())
        );
        // the envelope splices the ring's text verbatim
        let frame = ServerMsg::Transcript {
            session_id: S.into(),
            epoch: "9f86d081884c7d65".into(),
            ops: ops.iter().map(json::stringify).collect(),
        };
        assert_eq!(
            canon(&json::parse(&frame.to_text()).unwrap()),
            canon(&golden)
        );
    }
}
