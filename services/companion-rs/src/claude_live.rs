//! Claude Code driver core (ADR-015 §4.2, step 6 part 1): the pure protocol
//! half of a companion-driven `claude.exe`. The CLI speaks the Agent SDK's
//! stream-json control protocol on stdio (NDJSON both ways); this module turns
//! its stdout frames into normalized transcript ops and permission requests,
//! and builds every frame the companion writes to its stdin. Process plumbing
//! is `runtime_io::ChildIo`; wiring into the companion (the runtime trait, the
//! transcript hub, the engine) is part 2.
//!
//! - [`Driver::on_line`] decodes one stdout line (every Claude row of §3.6).
//!   Item ids: text/thinking `<message.id>#<block index>`, tools `tool_use.id`.
//! - [`permission_request`] maps `can_use_tool` to a [`PermissionRequest`]
//!   whose options are the CLI's own suggestions after the VS Code
//!   extension's filter (§4.5).
//! - [`Driver::decide`] writes the byte-exact §4.5 answer. Answers and
//!   `control_cancel_request` are linearized on the driver's pending map, so
//!   exactly one response goes out per CLI request and none after a cancel.
//!   Every stdin write goes through a `send` callback and state is committed
//!   only when it succeeds.
//! - [`spawn_args`], [`spawn_env`], [`resolve_binary`], [`validate_workspace`]
//!   are the launch and workspace policy (§4.2.2, §4.2.3, T1, T6, T10, T11).
//!
//! One `Driver` per session for its whole life, respawns included: the CLI
//! restores its cumulative cost on `--resume`, and synthesized ids continue.
//! Frames carry OAuth callback codes and tool input: never log them.
//!
//! Sources: `docs/reverse-engineering/claude-code-streaming.md`,
//! `claude-agent-sdk.md`, Agent SDK 0.3.310 `sdk.mjs` (control request
//! shapes), and the sanitized captures in `tests/fixtures/claude/`.

use crate::crypto::sha256;
use crate::json::{self, Json};
use crate::protocol::{
    DecisionError, LiveWorkspace, PermissionOption, PermissionRequest, PromptImage, is_claude_model,
};
use crate::runtime_io::{child_env, redact, show_format_chars};
use crate::transcript::{
    Field, FileDiff, Item, ItemKind, LiveState, McpAuthState, McpServer, Op, PlanStep, ToolCall,
    ToolStatus, TurnState, Usage,
};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/* ------------------------------------------------------------------ */
/* Constants                                                           */
/* ------------------------------------------------------------------ */

/// Runtime requests never expire natively (§4.1 "Engine (G2)").
pub const LIVE_PERMISSION_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// What the model reads when the phone denies without a message (§4.5).
pub const DENIED_FROM_PHONE: &str = "Denied from phone";
/// The answer to every control request this host does not serve (§3.6).
pub const UNSUPPORTED_BY_HOST: &str = "unsupported by host";
/// The modes the phone may pick and the config may set (§3.4); never
/// `bypassPermissions`, `dontAsk` or `auto`.
pub const PERMISSION_MODES: [&str; 3] = ["default", "acceptEdits", "plan"];
/// Where claude.ai connectors are signed in (§4.7); the companion is not involved.
pub const CLAUDE_AI_CONNECTORS: &str = "https://claude.ai/customize/connectors";
/// ADR-017: slash commands the phone never offers. `/clear` starts a new
/// session id the companion would lose track of (verified on 2.1.322);
/// `/heapdump` writes to the PC's desktop; the rest are internal.
const HIDDEN_COMMANDS: [&str; 5] = ["clear", "reset", "new", "heapdump", "workflow-launch-exec"];

/// §4.2.4: an unparseable line starting with this is answered with an error.
const CONTROL_REQUEST_PREFIX: &str = "{\"type\":\"control_request\"";
// §3.5 caps, applied to authoritative items (deltas are capped by the hub)
const TEXT_MAX: usize = 256 << 10;
const THINKING_MAX: usize = 64 << 10;
const INPUT_MAX: usize = 16 << 10;
const OUTPUT_MAX: usize = 32 << 10;
const DIFF_MAX: usize = 64 << 10;
const NOTICE_MAX: usize = 2 << 10;
const CONTEXT_CHARS: usize = 500;
const TITLE_CHARS: usize = 200;
/// Unfinished tool items kept for status updates; a turn rarely has more.
const TOOLS_MAX: usize = 256;

/* ------------------------------------------------------------------ */
/* Launch and workspace policy (§4.2.2, §4.2.3)                        */
/* ------------------------------------------------------------------ */

/// The SDK flag set (§4.2.2) as an argument vector: no shell ever parses
/// it, and prompts never travel in argv. `resume` picks `--resume` for a
/// session this companion created and holds no live process for (T10).
pub fn spawn_args(
    session_id: &str,
    resume: bool,
    permission_mode: Option<&str>,
    model: Option<&str>,
) -> Result<Vec<String>, &'static str> {
    if !is_uuid(session_id) {
        return Err("the Claude session id must be a UUID");
    }
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--input-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
        "--setting-sources",
        "user,project,local",
        "--thinking-display",
        "summarized",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if let Some(mode) = permission_mode {
        if !PERMISSION_MODES.contains(&mode) {
            return Err("permission mode must be default, acceptEdits or plan");
        }
        args.extend(["--permission-mode".into(), mode.into()]);
    }
    // ADR-017: "default" is the CLI's own choice, so no flag at all
    if let Some(model) = model.filter(|m| *m != "default") {
        if !is_claude_model(model) {
            return Err("model must be a Claude Code alias or a claude-… id");
        }
        args.extend(["--model".into(), model.into()]);
    }
    let flag = if resume { "--resume" } else { "--session-id" };
    args.extend([flag.into(), session_id.into()]);
    Ok(args)
}

/// The child environment (§4.2.2, T6): the scrubbed companion environment
/// plus the official SDK entrypoint (replacing an inherited `claude-vscode`)
/// and `system/session_state_changed` frames.
pub fn spawn_env(
    base: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    child_env(
        base,
        &[
            ("CLAUDE_CODE_ENTRYPOINT", "sdk-ts"),
            ("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1"),
        ],
    )
}

/// Binary resolution (§4.2.2): the configured `binary`, else the native
/// installer's `%USERPROFILE%\.local\bin\claude.exe`, else `claude.exe` in an
/// absolute PATH directory. Never a `.cmd`/`.bat` shim: Windows runs those
/// through cmd.exe.
pub fn resolve_binary(
    configured: Option<&Path>,
    userprofile: Option<&OsStr>,
    path_var: Option<&OsStr>,
) -> Result<PathBuf, String> {
    if let Some(p) = configured {
        let lower = p.to_string_lossy().to_ascii_lowercase();
        if lower.ends_with(".cmd") || lower.ends_with(".bat") {
            return Err(format!(
                "the Claude Code binary must be an executable, not a shell script: {}",
                p.display()
            ));
        }
        return if p.is_absolute() && p.is_file() {
            Ok(p.to_path_buf())
        } else {
            Err(format!("Claude Code binary not found: {}", p.display()))
        };
    }
    let installer = userprofile.map(|h| Path::new(h).join(".local").join("bin").join("claude.exe"));
    let on_path = path_var
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("claude.exe"));
    installer
        .into_iter()
        .chain(on_path)
        .find(|p| p.is_file())
        .ok_or_else(|| "claude.exe not found in %USERPROFILE%\\.local\\bin or on PATH".into())
}

/// Checks one configured workspace (§4.2.3, T1, T11): an absolute, existing
/// directory, canonicalized; never a drive root, the user-profile folder, or
/// a folder that contains the CHERNOBYL data directory (an agent reads its
/// cwd without asking). The id is the first 12 hex chars of
/// sha256(canonical path), stable across config spellings; the label is the
/// folder name.
pub fn validate_workspace(
    raw: &str,
    data_dir: &Path,
    profile: Option<&Path>,
) -> Result<LiveWorkspace, String> {
    if !Path::new(raw).is_absolute() {
        return Err(format!("workspace must be an absolute path: {raw}"));
    }
    let canon = std::fs::canonicalize(raw)
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| format!("workspace is not an existing directory: {raw}"))?;
    let path = plain(&canon);
    let dir = PathBuf::from(&path);
    if dir.parent().is_none() {
        return Err(format!("a drive root cannot be a workspace: {path}"));
    }
    if profile
        .and_then(|h| std::fs::canonicalize(h).ok())
        .is_some_and(|h| PathBuf::from(plain(&h)).starts_with(&dir))
    {
        return Err(format!(
            "the user profile folder cannot be a workspace: {path}"
        ));
    }
    let data = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    if PathBuf::from(plain(&data)).starts_with(&dir) {
        return Err(format!(
            "workspace contains the CHERNOBYL data directory: {path}"
        ));
    }
    let label = dir
        .file_name()
        .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned());
    Ok(LiveWorkspace {
        id: workspace_id(&path),
        label,
        path,
    })
}

/// First 12 hex chars of sha256 of the canonical path (§4.2.3).
pub fn workspace_id(canonical: &str) -> String {
    hex(&sha256(canonical.as_bytes()))[..12].to_string()
}

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\srv\share` → `\\srv\share` (what
/// `canonicalize` returns on Windows, minus the verbatim prefix).
pub fn plain(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
    }
}

/// A random v4 UUID: new session ids (`--session-id`) and prompt uuids.
pub fn new_uuid() -> Result<String, &'static str> {
    let mut b = [0u8; 16];
    crate::ffi::csprng_fill(&mut b).map_err(|_| "could not generate an id")?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

pub(crate) fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/* ------------------------------------------------------------------ */
/* Driver: decoder, pending maps, encoders                             */
/* ------------------------------------------------------------------ */

/// What one stdout line (or a phone action) means for the companion.
// ops dominate (one per token delta): boxing them would allocate per delta
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A transcript op: `Op::Append` goes to `Hub::append`, the rest to `Hub::push`.
    Op(Op),
    /// A new pending request for `engine.ingest_permission`.
    Permission(Box<PermissionRequest>),
    /// The CLI withdrew a request (or the process ended): resolve it `stale`.
    Withdrawn(String),
    /// A frame the CLI is blocked on (an error or decline answer): write it now.
    Write(String),
    /// A phone prompt's G1 lifecycle (§4.6): the command named `command_id`.
    Prompt {
        command_id: String,
        state: PromptState,
    },
    /// The CLI answered one of our control requests.
    Reply {
        command_id: Option<String>,
        subtype: &'static str,
        result: Result<(), String>,
    },
    /// The answer to `mcp_authenticate` (§4.7).
    AuthStarted {
        command_id: Option<String>,
        server: String,
        result: Result<AuthStart, String>,
    },
    /// `Session.status` changed (journaled): running | waiting | idle.
    Status(&'static str),
    /// `Session.title` changed (journaled).
    Title(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PromptState {
    /// Folded into the running turn; it starts later.
    Queued,
    /// Its turn started (`command_progress running`).
    Started,
    /// Its turn ended: completed | cancelled | failed.
    Done {
        status: &'static str,
        error: Option<String>,
    },
}

/// A phone decision as the driver needs it (from `permission_decision`).
pub struct Decision<'a> {
    pub permission_id: &'a str,
    pub decision: &'a str,
    pub option_id: Option<&'a str>,
    pub message: Option<&'a str>,
    pub answers: &'a [(String, Vec<String>)],
}

/// What `mcp_authenticate` returned.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthStart {
    /// Opened on the desktop (`flow: desktop`) or handed to the phone.
    pub auth_url: String,
    /// Present when the provider redirects to a localhost callback the phone
    /// can paste back (`flow: phone-callback`).
    pub callback: Option<Callback>,
}

/// Feed for `runtime_io::check_callback_url` and `PendingAuth`.
#[derive(Debug, Clone, PartialEq)]
pub struct Callback {
    pub port: u16,
    pub path: String,
    pub state: String,
    /// `auth.callbackPrefix`, e.g. `http://localhost:44818/callback`.
    pub prefix: String,
}

/// A mapped `can_use_tool` plus what the answer needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Asked {
    pub request: PermissionRequest,
    /// The request's `input`, exactly as the CLI sent it (§4.5 echo).
    input_raw: String,
    input: Json,
    /// Option id → that suggestion's exact text (§4.5 byte-for-byte echo).
    suggestions: Vec<(String, String)>,
    /// AskUserQuestion: question id → question text.
    questions: Vec<(String, String)>,
}

struct Sent {
    subtype: &'static str,
    command_id: Option<String>,
    /// The MCP server name, or the mode or model asked for.
    server: Option<String>,
}

struct Prompt {
    uuid: String,
    command_id: String,
    text: String,
    images: u32,
    ts: i64,
    started: bool,
    queued: bool,
}

struct Turn {
    id: String,
    started_at: i64,
    usage: Usage,
}

/// Protocol state of one driven Claude session. Not thread-safe by itself:
/// `claude_runtime` holds it under one mutex, which is what linearizes `decide` against
/// `control_cancel_request` (§4.2.4).
pub struct Driver {
    adapter_id: String,
    session_id: String,
    workspace: String,
    /// Our `chb_<n>` control requests awaiting a `control_response`.
    sent: Vec<(String, Sent)>,
    next_request: u64,
    /// The CLI's `can_use_tool` requests (keyed by its UUID request ids).
    asks: Vec<(String, Asked)>,
    prompts: Vec<Prompt>,
    /// The CLI reports `command_lifecycle` per prompt uuid (2.1.310 does,
    /// verified live), so prompts start and end by uuid, not by guesswork.
    lifecycle: bool,
    turn: Option<Turn>,
    status: &'static str,
    permission_mode: Option<String>,
    model: Option<String>,
    /// ADR-017: `initialize`'s models (value, label) and the slash command
    /// names (`initialize`, `system/init`, `commands_changed`).
    models: Vec<(String, String)>,
    commands: Vec<String>,
    /// ADR-017: the last `get_context_usage`, (used, max) tokens.
    context: Option<(i64, i64)>,
    /// Current streamed message and its open blocks: index → item id (None
    /// for blocks that are never shown).
    stream_msg: String,
    open: Vec<(i64, Option<String>)>,
    /// `assistant` frames seen per message id: frame k carries block k.
    blocks: Vec<(String, usize)>,
    /// Unfinished tool items, with when they started running.
    tools: Vec<(Item, i64)>,
    /// TodoWrite calls: shown as the plan, never as tool cards.
    hidden: Vec<String>,
    /// Tools whose permission request the CLI withdrew.
    cancelled: Vec<String>,
    /// Server name → `config.type`, from the last `mcp_status`.
    mcp_kinds: Vec<(String, String)>,
    /// Servers whose sign-in returned no localhost callback.
    desktop_only: Vec<String>,
    rate_windows: Vec<String>,
    seq: u64,
    cost_total: f64,
    malformed: u64,
}

impl Driver {
    pub fn new(adapter_id: &str, session_id: &str, workspace: &str) -> Driver {
        Driver {
            adapter_id: adapter_id.into(),
            session_id: session_id.into(),
            workspace: workspace.into(),
            sent: Vec::new(),
            next_request: 0,
            asks: Vec::new(),
            prompts: Vec::new(),
            lifecycle: false,
            turn: None,
            status: "starting",
            permission_mode: None,
            model: None,
            models: Vec::new(),
            commands: Vec::new(),
            context: None,
            stream_msg: String::new(),
            open: Vec::new(),
            blocks: Vec::new(),
            tools: Vec::new(),
            hidden: Vec::new(),
            cancelled: Vec::new(),
            mcp_kinds: Vec::new(),
            desktop_only: Vec::new(),
            rate_windows: Vec::new(),
            seq: 0,
            cost_total: 0.0,
            malformed: 0,
        }
    }

    /// `state {status: starting}` (§3.6 "(spawned)"): at every spawn.
    pub fn starting(&mut self) -> Op {
        self.status = "starting";
        Op::State(self.live_state())
    }

    /// Lines that failed to parse (the `/health` `runtimeMalformed` counter).
    pub fn malformed(&self) -> u64 {
        self.malformed
    }

    pub fn turn_running(&self) -> bool {
        self.turn.is_some()
    }

    /// The CLI's current permission mode (kept for a `--resume` respawn).
    pub fn permission_mode(&self) -> Option<&str> {
        self.permission_mode.as_deref()
    }

    /// The CLI's current model (kept for a `--resume` respawn).
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The cumulative cost a `--resume` restores, for a session this driver
    /// did not start (its turns report the difference).
    pub fn restore_cost(&mut self, total: f64) {
        self.cost_total = total;
    }

    /// No turn running, no prompt in flight and no request waiting: what the
    /// reaper needs before it closes stdin.
    pub fn is_quiet(&self) -> bool {
        self.turn.is_none() && self.prompts.is_empty() && self.asks.is_empty()
    }

    /// Whether `permission_id` is a request this process still waits on.
    pub fn knows_permission(&self, permission_id: &str) -> bool {
        self.request_id_of(permission_id)
            .is_some_and(|r| self.asks.iter().any(|(id, _)| id == r))
    }

    fn request_id_of<'a>(&self, permission_id: &'a str) -> Option<&'a str> {
        permission_id
            .strip_prefix(self.adapter_id.as_str())?
            .strip_prefix(':')
    }

    /* ---------------------------- stdin ---------------------------- */

    /// A phone prompt (§4.2.5): the user frame carries a fresh v4 uuid mapped
    /// to `command_id`, so the CLI's `command_lifecycle` frames find the
    /// command. Returns the user item to push once the frame is queued.
    /// ADR-017: `images` go first, as Messages API image blocks; the item
    /// and everything kept here hold only their count.
    pub fn user_prompt(
        &mut self,
        command_id: &str,
        text: &str,
        images: &[PromptImage],
        now: i64,
        send: impl FnOnce(String) -> Result<(), &'static str>,
    ) -> Result<Op, &'static str> {
        let uuid = new_uuid()?;
        let mut content = String::new();
        for im in images {
            content.push_str(&format!(
                "{{\"type\":\"image\",\"source\":{{\"type\":\"base64\",\"media_type\":{},\"data\":{}}}}},",
                q(&im.media_type),
                q(&im.data)
            ));
        }
        send(format!(
            "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{content}{{\"type\":\"text\",\"text\":{}}}]}},\"parent_tool_use_id\":null,\"session_id\":\"\",\"uuid\":\"{uuid}\"}}",
            q(text)
        ))?;
        let prompt = Prompt {
            uuid,
            command_id: command_id.into(),
            text: text.into(),
            images: images.len() as u32,
            ts: now,
            started: false,
            queued: false,
        };
        let op = Op::Item(user_item(&prompt));
        self.prompts.push(prompt);
        Ok(op)
    }

    /// `initialize` (verified live; SDK `subtype:"initialize",hooks:…`).
    pub fn initialize(
        &mut self,
        send: impl FnOnce(String) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        let body = r#"{"subtype":"initialize","hooks":null}"#.to_string();
        self.request(body, "initialize", None, None, send)
    }

    /// `interrupt` (verified live; the reply is `{"still_queued":[]}`).
    pub fn interrupt(
        &mut self,
        command_id: &str,
        send: impl FnOnce(String) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        let body = r#"{"subtype":"interrupt"}"#.to_string();
        self.request(body, "interrupt", Some(command_id), None, send)
    }

    /// `set_permission_mode {mode}` (verified live), whitelisted (§3.4).
    pub fn set_permission_mode(
        &mut self,
        command_id: &str,
        mode: &str,
        send: impl FnOnce(String) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        if !PERMISSION_MODES.contains(&mode) {
            return Err("permission mode must be default, acceptEdits or plan");
        }
        let body = format!(r#"{{"subtype":"set_permission_mode","mode":{}}}"#, q(mode));
        self.request(
            body,
            "set_permission_mode",
            Some(command_id),
            Some(mode),
            send,
        )
    }

    /// ADR-017 `set_model {model}` (verified on 2.1.322: the reply is
    /// `{model: <resolved id>}`, an unknown one is an error).
    pub fn set_model(
        &mut self,
        command_id: &str,
        model: &str,
        send: impl FnOnce(String) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        if !is_claude_model(model) {
            return Err("model must be a Claude Code alias or a claude-… id");
        }
        let body = format!(r#"{{"subtype":"set_model","model":{}}}"#, q(model));
        self.request(body, "set_model", Some(command_id), Some(model), send)
    }

    /// ADR-017 `rename_session` (the SDK's shape; verified on 2.1.322: the
    /// CLI answers `{}` and emits `system/session_title_changed`).
    pub fn rename(
        &mut self,
        command_id: &str,
        title: &str,
        send: impl FnOnce(String) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        let body = format!(
            r#"{{"subtype":"rename_session","title":{},"source":"host","session_id":{}}}"#,
            q(title),
            q(&self.session_id)
        );
        self.request(body, "rename_session", Some(command_id), None, send)
    }

    /// ADR-017 `get_context_usage` (verified live; no API call): the reply
    /// becomes `state.context`.
    pub fn context_usage(
        &mut self,
        send: impl FnOnce(String) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        self.request(
            r#"{"subtype":"get_context_usage"}"#.into(),
            "get_context_usage",
            None,
            None,
            send,
        )
    }

    /// `mcp_status` (verified live; the reply becomes an `Op::Mcp`).
    pub fn mcp_status(
        &mut self,
        send: impl FnOnce(String) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        self.request(
            r#"{"subtype":"mcp_status"}"#.into(),
            "mcp_status",
            None,
            None,
            send,
        )
    }

    /// `mcp_authenticate {serverName}` (verified live against a mock OAuth server).
    pub fn mcp_authenticate(
        &mut self,
        command_id: &str,
        server: &str,
        send: impl FnOnce(String) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        let body = format!(
            r#"{{"subtype":"mcp_authenticate","serverName":{}}}"#,
            q(server)
        );
        self.request(
            body,
            "mcp_authenticate",
            Some(command_id),
            Some(server),
            send,
        )
    }

    /// `mcp_oauth_callback_url {serverName, callbackUrl}` (verified live). The
    /// URL must already have passed `runtime_io::check_callback_url`; the
    /// frame carries a one-time code (T5).
    pub fn mcp_oauth_callback_url(
        &mut self,
        command_id: &str,
        server: &str,
        callback_url: &str,
        send: impl FnOnce(String) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        let body = format!(
            r#"{{"subtype":"mcp_oauth_callback_url","serverName":{},"callbackUrl":{}}}"#,
            q(server),
            q(callback_url)
        );
        self.request(
            body,
            "mcp_oauth_callback_url",
            Some(command_id),
            Some(server),
            send,
        )
    }

    /// `mcp_reconnect {serverName}` (verified live).
    pub fn mcp_reconnect(
        &mut self,
        command_id: Option<&str>,
        server: &str,
        send: impl FnOnce(String) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        let body = format!(
            r#"{{"subtype":"mcp_reconnect","serverName":{}}}"#,
            q(server)
        );
        self.request(body, "mcp_reconnect", command_id, Some(server), send)
    }

    fn request(
        &mut self,
        body: String,
        subtype: &'static str,
        command_id: Option<&str>,
        server: Option<&str>,
        send: impl FnOnce(String) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        let id = format!("chb_{}", self.next_request + 1);
        send(format!(
            "{{\"type\":\"control_request\",\"request_id\":\"{id}\",\"request\":{body}}}"
        ))?;
        self.next_request += 1;
        let sent = Sent {
            subtype,
            command_id: command_id.map(String::from),
            server: server.map(String::from),
        };
        self.sent.push((id, sent));
        Ok(())
    }

    /// Answers a pending `can_use_tool` (§4.5). Under the same lock as
    /// [`Driver::on_line`], so a request the CLI withdrew (or one already
    /// answered) fails with "request is no longer pending" and nothing is
    /// written. The request stays pending if `send` fails.
    pub fn decide(
        &mut self,
        d: &Decision,
        send: impl FnOnce(String) -> Result<(), &'static str>,
    ) -> Result<Vec<Event>, &'static str> {
        let not_pending = DecisionError::NotPending.message();
        let unsupported = DecisionError::UnsupportedDecision.message();
        let rid = self.request_id_of(d.permission_id).ok_or(not_pending)?;
        let at = self
            .asks
            .iter()
            .position(|(id, _)| id == rid)
            .ok_or(not_pending)?;
        let asked = &self.asks[at].1;
        let options = &asked.request.options;
        let option = match d.option_id {
            Some(id) => options.iter().find(|o| o.id == id),
            // a legacy decision selects only a non-persistent option (§4.1)
            None => options
                .iter()
                .find(|o| o.decision == d.decision && o.scope != "persistent"),
        }
        .filter(|o| o.decision == d.decision)
        .ok_or(unsupported)?;
        let allow = option.decision != "reject";
        send(success_response(rid, &answer_body(asked, option, d)?))?;
        let (_, asked) = self.asks.remove(at);
        let mut out = Vec::new();
        if allow && let Some(tid) = &asked.request.tool_item_id {
            self.update_tool(tid, &mut out, |t| t.status = ToolStatus::Running);
        }
        if self.asks.is_empty() && self.turn.is_some() {
            self.set_status("running", &mut out);
        }
        Ok(out)
    }

    /* ---------------------------- stdout --------------------------- */

    /// Decodes one stdout line (§3.6, §4.2.4). Depth ≤ 32 comes from
    /// `json::parse`; the 16 MiB cap from `ChildIo` ([`Driver::on_skipped`]).
    pub fn on_line(&mut self, line: &str, now: i64) -> Vec<Event> {
        let mut out = Vec::new();
        let Ok(v) = json::parse(line) else {
            self.malformed += 1;
            self.salvage(line, now, &mut out);
            return out;
        };
        match json::get_str(&v, "type").unwrap_or("") {
            "control_request" => self.control_request(&v, line, now, &mut out),
            "control_response" => self.control_response(&v, line, now, &mut out),
            "control_cancel_request" => self.cancel(&v, &mut out),
            "system" => self.system(&v, now, &mut out),
            "stream_event" => self.stream_event(&v, now, &mut out),
            "assistant" => self.assistant(&v, now, &mut out),
            "user" => self.user(&v, now, &mut out),
            "result" => self.result(&v, now, &mut out),
            "rate_limit_event" => self.rate_limit(&v, now, &mut out),
            "command_lifecycle" => self.lifecycle(&v, &mut out),
            // keep_alive, transcript_mirror, prompt_suggestion, anything newer
            _ => {}
        }
        out
    }

    /// A stdout line over 16 MiB was skipped (`ChildEvent::Skipped`).
    pub fn on_skipped(&mut self, count: u64, now: i64) -> Vec<Event> {
        let mut out = Vec::new();
        let text =
            format!("Skipped a message from Claude Code larger than 16 MiB ({count} so far)");
        self.notice(&mut out, now, None, "warning", &text);
        out
    }

    /// The process exited (§4.2.4): pending requests go stale, open prompts
    /// fail, our requests fail, the turn fails and the state ends. The
    /// cumulative cost is kept for a `--resume` respawn.
    pub fn on_exit(&mut self, code: Option<i32>, now: i64) -> Vec<Event> {
        let why = match code {
            Some(c) => format!("Claude Code exited with code {c}"),
            None => "Claude Code exited".to_string(),
        };
        let mut out = Vec::new();
        for (_, asked) in std::mem::take(&mut self.asks) {
            out.push(Event::Withdrawn(asked.request.id));
        }
        for (_, sent) in std::mem::take(&mut self.sent) {
            match sent.subtype {
                "mcp_status" | "get_context_usage" => {}
                "mcp_authenticate" => out.push(Event::AuthStarted {
                    command_id: sent.command_id,
                    server: sent.server.unwrap_or_default(),
                    result: Err(why.clone()),
                }),
                subtype => out.push(Event::Reply {
                    command_id: sent.command_id,
                    subtype,
                    result: Err(why.clone()),
                }),
            }
        }
        for p in std::mem::take(&mut self.prompts) {
            out.push(Event::Prompt {
                command_id: p.command_id,
                state: PromptState::Done {
                    status: "failed",
                    error: Some(why.clone()),
                },
            });
        }
        self.finish_tools(ToolStatus::Failed, Some(&why), now, &mut out);
        if let Some(turn) = self.turn.take() {
            out.push(Event::Op(Op::Turn(TurnState {
                id: turn.id,
                status: "failed",
                started_at: turn.started_at,
                ended_at: Some(now),
                usage: Some(turn.usage),
                cost_usd: None,
                error: Some(why),
                diffs: Vec::new(),
            })));
        }
        self.set_status("ended", &mut out);
        self.open.clear();
        self.blocks.clear();
        self.hidden.clear();
        self.cancelled.clear();
        out
    }

    /// §4.2.4: an unparseable `control_request` is refused by request id, so
    /// the CLI never blocks on it (fail-closed).
    fn salvage(&mut self, line: &str, now: i64, out: &mut Vec<Event>) {
        if !line.starts_with(CONTROL_REQUEST_PREFIX) {
            return; // a lost delta heals at the authoritative `assistant` frame
        }
        let Some(id) = shallow_request_id(line) else {
            return;
        };
        out.push(Event::Write(error_response(
            id,
            "unparseable control request",
        )));
        self.notice(
            out,
            now,
            None,
            "warning",
            "Claude Code sent a request the companion could not read; it was refused",
        );
    }

    fn control_request(&mut self, v: &Json, line: &str, now: i64, out: &mut Vec<Event>) {
        let Some(id) = json::get_str(v, "request_id").filter(|s| !s.is_empty()) else {
            return;
        };
        let req = json::get(v, "request").unwrap_or(&Json::Null);
        match json::get_str(req, "subtype") {
            Some("can_use_tool") => {
                let raw = raw_member(line, "request").unwrap_or("");
                if let Err(e) = self.ask(id, req, raw, now, out) {
                    out.push(Event::Write(error_response(id, e)));
                }
            }
            Some("elicitation") => {
                out.push(Event::Write(success_response(
                    id,
                    r#"{"action":"decline"}"#,
                )));
                let server = json::get_str(req, "mcp_server_name").unwrap_or("An MCP server");
                let text = format!(
                    "{server} asked for input, which the phone cannot answer yet; it was declined"
                );
                self.notice(out, now, None, "warning", &text);
            }
            // hook_callback, mcp_message, request_user_dialog, anything newer:
            // an unanswered request blocks the CLI
            _ => out.push(Event::Write(error_response(id, UNSUPPORTED_BY_HOST))),
        }
    }

    fn ask(
        &mut self,
        request_id: &str,
        req: &Json,
        raw: &str,
        now: i64,
        out: &mut Vec<Event>,
    ) -> Result<(), &'static str> {
        if self.asks.iter().any(|(id, _)| id == request_id) {
            return Ok(()); // redelivered while still pending
        }
        let asked = permission_request(
            &self.adapter_id,
            &self.session_id,
            &self.workspace,
            request_id,
            req,
            raw,
            now,
        )?;
        let pid = asked.request.id.clone();
        if let Some(tid) = asked.request.tool_item_id.clone() {
            self.update_tool(&tid, out, |t| {
                t.status = ToolStatus::AwaitingApproval;
                t.permission_id = Some(pid);
            });
        }
        self.set_status("waiting", out);
        // last: once the engine has it a decision may land, and nothing from
        // this batch may follow that decision's ops
        out.push(Event::Permission(Box::new(asked.request.clone())));
        self.asks.push((request_id.into(), asked));
        Ok(())
    }

    fn cancel(&mut self, v: &Json, out: &mut Vec<Event>) {
        let Some(id) = json::get_str(v, "request_id") else {
            return;
        };
        let Some(at) = self.asks.iter().position(|(r, _)| r == id) else {
            return; // already answered: never answered twice
        };
        let (_, asked) = self.asks.remove(at);
        if let Some(tid) = asked.request.tool_item_id {
            push_capped(&mut self.cancelled, tid, 64);
        }
        out.push(Event::Withdrawn(asked.request.id));
        if self.asks.is_empty() && self.turn.is_some() {
            self.set_status("running", out);
        }
    }

    fn control_response(&mut self, v: &Json, line: &str, now: i64, out: &mut Vec<Event>) {
        let Some(resp) = json::get(v, "response") else {
            return;
        };
        let Some(id) = json::get_str(resp, "request_id") else {
            return;
        };
        // only our own ids: the CLI's echoes and unknown ids are ignored
        let Some(at) = self.sent.iter().position(|(s, _)| s == id) else {
            return;
        };
        let (_, sent) = self.sent.remove(at);
        let body = json::get(resp, "response").unwrap_or(&Json::Null);
        let result = if json::get_str(resp, "subtype") == Some("success") {
            Ok(())
        } else {
            let e = json::get_str(resp, "error").unwrap_or("Claude Code refused the request");
            Err(cap(&redact(e), NOTICE_MAX).0)
        };
        match (sent.subtype, result) {
            ("mcp_status", Ok(())) => self.mcp_reply(body, out),
            ("mcp_status" | "get_context_usage", Err(_)) => {}
            ("get_context_usage", Ok(())) => {
                let n = |k: &str| json::get(body, k).and_then(json::as_f64).map(|x| x as i64);
                if let (Some(used), Some(max)) = (n("totalTokens"), n("maxTokens"))
                    && max > 0
                    && self.context != Some((used, max))
                {
                    self.context = Some((used, max));
                    out.push(Event::Op(Op::State(self.live_state())));
                }
            }
            ("mcp_authenticate", result) => {
                let server = sent.server.unwrap_or_default();
                let result = result.and_then(|()| auth_start(body).map_err(String::from));
                if let Ok(start) = &result {
                    self.desktop_only.retain(|s| *s != server);
                    if start.callback.is_none() {
                        self.desktop_only.push(server.clone());
                    }
                }
                out.push(Event::AuthStarted {
                    command_id: sent.command_id,
                    server,
                    result,
                });
            }
            ("initialize", Ok(())) => {
                // `account` (email, organization) is never read, so never forwarded
                if let Some(mode) = json::get_str(body, "current_permission_mode") {
                    self.permission_mode = Some(mode.to_string());
                }
                if let Some(list) = json::get(body, "models").and_then(json::as_arr) {
                    self.models = list
                        .iter()
                        .filter_map(|m| {
                            let value = json::get_str(m, "value").filter(|v| is_claude_model(v))?;
                            let label = json::get_str(m, "displayName").unwrap_or(value);
                            Some((value.to_string(), plain_text(label)))
                        })
                        .collect();
                }
                self.set_commands(json::get(body, "commands"));
                if self.turn.is_none() && self.status != "idle" {
                    self.set_status("idle", out);
                } else {
                    out.push(Event::Op(Op::State(self.live_state())));
                }
                self.reingest(body, line, now, out);
                out.push(Event::Reply {
                    command_id: sent.command_id,
                    subtype: "initialize",
                    result: Ok(()),
                });
            }
            (subtype, result) => {
                // the CLI sends no `system/status` for a mode it was told
                if subtype == "set_permission_mode"
                    && result.is_ok()
                    && let Some(mode) = sent.server.clone()
                    && self.permission_mode.as_deref() != Some(&mode)
                {
                    self.permission_mode = Some(mode);
                    out.push(Event::Op(Op::State(self.live_state())));
                }
                // nor for a model: its reply names the resolved id
                if subtype == "set_model" && result.is_ok() {
                    let model = json::get_str(body, "model")
                        .map(String::from)
                        .or(sent.server);
                    if model.is_some() && self.model != model {
                        self.model = model;
                        out.push(Event::Op(Op::State(self.live_state())));
                    }
                }
                out.push(Event::Reply {
                    command_id: sent.command_id,
                    subtype,
                    result,
                })
            }
        }
    }

    /// `initialize` → `pending_permission_requests[]` re-ingested (§3.6).
    fn reingest(&mut self, body: &Json, line: &str, now: i64, out: &mut Vec<Event>) {
        let Some(list) = json::get(body, "pending_permission_requests").and_then(json::as_arr)
        else {
            return;
        };
        let raws = raw_member(line, "response")
            .and_then(|r| raw_member(r, "response"))
            .and_then(|r| raw_member(r, "pending_permission_requests"))
            .map(raw_elements)
            .unwrap_or_default();
        for (el, raw) in list.iter().zip(raws) {
            let req = json::get(el, "request").unwrap_or(&Json::Null);
            let (Some(id), Some("can_use_tool")) = (
                json::get_str(el, "request_id"),
                json::get_str(req, "subtype"),
            ) else {
                continue;
            };
            let raw_req = raw_member(raw, "request").unwrap_or("");
            if let Err(e) = self.ask(id, req, raw_req, now, out) {
                out.push(Event::Write(error_response(id, e)));
            }
        }
    }

    fn system(&mut self, v: &Json, now: i64, out: &mut Vec<Event>) {
        match json::get_str(v, "subtype").unwrap_or("") {
            "init" => {
                self.ensure_turn(v, now, out);
                let mut changed = false;
                for (slot, key) in [
                    (&mut self.model, "model"),
                    (&mut self.permission_mode, "permissionMode"),
                ] {
                    if let Some(x) = json::get_str(v, key)
                        && slot.as_deref() != Some(x)
                    {
                        *slot = Some(x.to_string());
                        changed = true;
                    }
                }
                changed |= self.set_commands(json::get(v, "slash_commands"));
                if changed {
                    out.push(Event::Op(Op::State(self.live_state())));
                }
                if let Some(list) = json::get(v, "mcp_servers").and_then(json::as_arr) {
                    let servers = list
                        .iter()
                        .filter_map(|s| {
                            let name = json::get_str(s, "name")?;
                            let kind = self
                                .mcp_kinds
                                .iter()
                                .find(|(n, _)| n == name)
                                .map_or("", |(_, k)| k.as_str());
                            let status = json::get_str(s, "status").unwrap_or("");
                            Some(mcp_entry(
                                name,
                                status,
                                kind,
                                None,
                                self.phone_callback(name),
                            ))
                        })
                        .collect();
                    out.push(Event::Op(Op::Mcp(servers)));
                }
                // without lifecycle frames, the oldest waiting prompt started
                if !self.lifecycle
                    && !self.prompts.iter().any(|p| p.started)
                    && let Some(i) = self.prompts.iter().position(|p| !p.started)
                {
                    self.start_prompt(i, out);
                }
            }
            "status" => {
                if let Some(mode) = json::get_str(v, "permissionMode")
                    && self.permission_mode.as_deref() != Some(mode)
                {
                    self.permission_mode = Some(mode.to_string());
                    out.push(Event::Op(Op::State(self.live_state())));
                }
            }
            "session_state_changed" => match json::get_str(v, "state") {
                Some("running") => {
                    self.ensure_turn(v, now, out);
                    self.set_status("running", out);
                }
                Some("requires_action") => self.set_status("waiting", out),
                Some("idle") => self.set_status("idle", out),
                _ => {}
            },
            "api_retry" => {
                let mut text = "Retrying…".to_string();
                if let (Some(n), Some(max)) =
                    (json::get_i64(v, "attempt"), json::get_i64(v, "max_retries"))
                {
                    text.push_str(&format!(" (attempt {n} of {max})"));
                }
                self.notice(out, now, json::get_str(v, "uuid"), "warning", &text);
            }
            "compact_boundary" => {
                let id = json::get_str(v, "uuid");
                self.notice(out, now, id, "info", "Context compacted");
            }
            "session_title_changed" => {
                if let Some(t) = json::get_str(v, "title").filter(|t| !t.is_empty()) {
                    out.push(Event::Title(t.to_string()));
                }
            }
            "commands_changed" if self.set_commands(json::get(v, "commands")) => {
                out.push(Event::Op(Op::State(self.live_state())));
            }
            _ => {} // thinking_tokens, hook_*, task_*, …
        }
    }

    fn stream_event(&mut self, v: &Json, now: i64, out: &mut Vec<Event>) {
        // stream events are main-thread only; subagent work arrives whole
        if json::get_str(v, "parent_tool_use_id").is_some() {
            return;
        }
        let Some(ev) = json::get(v, "event") else {
            return;
        };
        let index = json::get_i64(ev, "index").unwrap_or(0);
        match json::get_str(ev, "type").unwrap_or("") {
            "message_start" => {
                self.ensure_turn(v, now, out);
                self.stream_msg = json::get_str(v, "api_message_id")
                    .or_else(|| json::get(ev, "message").and_then(|m| json::get_str(m, "id")))
                    .unwrap_or("")
                    .to_string();
                self.open.clear();
            }
            "content_block_start" => {
                let block = json::get(ev, "content_block").unwrap_or(&Json::Null);
                let msg = json::get_str(v, "api_message_id")
                    .map_or_else(|| self.stream_msg.clone(), String::from);
                let shown = match json::get_str(block, "type").unwrap_or("") {
                    kind @ ("text" | "thinking") => {
                        let id = format!("{msg}#{index}");
                        let k = if kind == "text" {
                            ItemKind::Text
                        } else {
                            ItemKind::Thinking
                        };
                        out.push(Event::Op(Op::Item(self.item(&id, k, now))));
                        Some(id)
                    }
                    "tool_use" | "server_tool_use" => {
                        let id = json::get_str(block, "id").unwrap_or("").to_string();
                        let name = json::get_str(block, "name").unwrap_or("tool");
                        if id.is_empty() {
                            None
                        } else if name == "TodoWrite" {
                            self.hide(id);
                            None
                        } else {
                            let mut item = self.item(&id, ItemKind::Tool, now);
                            item.tool = Some(tool_call(name));
                            out.push(Event::Op(Op::Item(item.clone())));
                            self.track_tool(item, now);
                            Some(id)
                        }
                    }
                    _ => None, // redacted_thinking, server tool results, …
                };
                self.open.retain(|(i, _)| *i != index);
                self.open.push((index, shown));
            }
            "content_block_delta" => {
                let Some(id) = self
                    .open
                    .iter()
                    .find(|(i, _)| *i == index)
                    .and_then(|(_, id)| id.clone())
                else {
                    return;
                };
                let delta = json::get(ev, "delta").unwrap_or(&Json::Null);
                let (field, key) = match json::get_str(delta, "type").unwrap_or("") {
                    "text_delta" => (Field::Text, "text"),
                    "thinking_delta" => (Field::Text, "thinking"),
                    "input_json_delta" => (Field::Input, "partial_json"),
                    _ => return, // signature_delta, citations_delta: never displayed
                };
                if let Some(text) = json::get_str(delta, key).filter(|t| !t.is_empty()) {
                    out.push(Event::Op(Op::Append {
                        id,
                        field,
                        text: text.to_string(),
                    }));
                }
            }
            "message_delta" => {
                if let (Some(turn), Some(u)) = (self.turn.as_mut(), json::get(ev, "usage")) {
                    let n = |k: &str| json::get_i64(u, k).unwrap_or(0);
                    let cache_read = n("cache_read_input_tokens");
                    let thinking = json::get(u, "output_tokens_details")
                        .and_then(|d| json::get_i64(d, "thinking_tokens"))
                        .unwrap_or(0);
                    let t = &mut turn.usage;
                    t.input += n("input_tokens") + n("cache_creation_input_tokens") + cache_read;
                    t.output += n("output_tokens");
                    t.cache_read = Some(t.cache_read.unwrap_or(0) + cache_read);
                    if thinking > 0 {
                        t.reasoning = Some(t.reasoning.unwrap_or(0) + thinking);
                    }
                }
            }
            _ => {} // content_block_stop, message_stop
        }
    }

    fn assistant(&mut self, v: &Json, now: i64, out: &mut Vec<Event>) {
        let Some(msg) = json::get(v, "message") else {
            return;
        };
        let parent = json::get_str(v, "parent_tool_use_id").map(String::from);
        if parent.is_none() {
            self.ensure_turn(v, now, out);
        }
        if let Some(e) = json::get_str(v, "error") {
            let text = match e {
                "authentication_failed" | "oauth_org_not_allowed" => {
                    "Claude Code is not signed in on the desktop".to_string()
                }
                other => format!("Claude Code reported an error: {other}"),
            };
            self.notice(out, now, None, "error", &text);
        }
        let mid = json::get_str(msg, "id").unwrap_or("");
        for block in json::get(msg, "content")
            .and_then(json::as_arr)
            .unwrap_or(&[])
        {
            let index = self.next_block(mid);
            let kind = json::get_str(block, "type").unwrap_or("");
            match kind {
                "text" | "thinking" => {
                    let (k, key, max) = if kind == "text" {
                        (ItemKind::Text, "text", TEXT_MAX)
                    } else {
                        (ItemKind::Thinking, "thinking", THINKING_MAX)
                    };
                    let mut item = self.item(&format!("{mid}#{index}"), k, now);
                    let body = redact(json::get_str(block, key).unwrap_or(""));
                    (item.text, item.truncated) = cap(&body, max);
                    item.done = true;
                    item.parent_id = parent.clone();
                    out.push(Event::Op(Op::Item(item)));
                }
                "tool_use" | "server_tool_use" => self.tool_use(block, parent.clone(), now, out),
                k if k.ends_with("_tool_result") => {
                    // a server tool (web search) finishes inside the message
                    if let Some(id) = json::get_str(block, "tool_use_id") {
                        let id = id.to_string();
                        self.finish_tool(&id, now, out, |t| t.status = ToolStatus::Completed);
                    }
                }
                _ => {}
            }
        }
    }

    fn tool_use(&mut self, block: &Json, parent: Option<String>, now: i64, out: &mut Vec<Event>) {
        let Some(id) = json::get_str(block, "id").filter(|i| !i.is_empty()) else {
            return;
        };
        let name = json::get_str(block, "name").unwrap_or("tool");
        let input = json::get(block, "input").unwrap_or(&Json::Null);
        if name == "TodoWrite" {
            self.hide(id.to_string());
            let mut item = self.item("plan", ItemKind::Plan, now);
            item.steps = plan_steps(input);
            out.push(Event::Op(Op::Item(item)));
            return;
        }
        let at = self.tools.iter().position(|(i, _)| i.id == id);
        let mut item = match at {
            Some(at) => self.tools.remove(at).0,
            None => {
                let mut item = self.item(id, ItemKind::Tool, now);
                item.tool = Some(tool_call(name));
                item
            }
        };
        item.parent_id = parent;
        if let Some(t) = item.tool.as_mut() {
            t.title = tool_title(name, input, &self.workspace);
            t.input = tool_input(name, input);
            if t.status == ToolStatus::Pending {
                t.status = ToolStatus::Running;
            }
        }
        out.push(Event::Op(Op::Item(item.clone())));
        self.track_tool(item, now);
    }

    fn user(&mut self, v: &Json, now: i64, out: &mut Vec<Event>) {
        if json::get_bool(v, "isReplay") == Some(true) {
            return;
        }
        let content = json::get(v, "message").and_then(|m| json::get(m, "content"));
        let stopped = |t: &str| t.starts_with("[Request interrupted by user");
        match content {
            Some(Json::Arr(blocks)) => {
                for b in blocks {
                    match json::get_str(b, "type") {
                        Some("tool_result") => self.tool_result(b, v, now, out),
                        Some("text") if json::get_str(b, "text").is_some_and(stopped) => {
                            self.notice(out, now, json::get_str(v, "uuid"), "info", "Stopped");
                        }
                        _ => {}
                    }
                }
            }
            Some(Json::Str(t)) if stopped(t) => {
                self.notice(out, now, json::get_str(v, "uuid"), "info", "Stopped");
            }
            _ => {}
        }
    }

    fn tool_result(&mut self, block: &Json, frame: &Json, now: i64, out: &mut Vec<Event>) {
        let Some(id) = json::get_str(block, "tool_use_id") else {
            return;
        };
        if let Some(at) = self.hidden.iter().position(|h| h == id) {
            self.hidden.remove(at); // TodoWrite: the plan item already shows it
            return;
        }
        let is_error = json::get_bool(block, "is_error") == Some(true);
        let content = content_text(json::get(block, "content"));
        let decision = json::get(frame, "tool_result_meta")
            .and_then(json::as_arr)
            .and_then(|m| m.iter().find(|e| json::get_str(e, "id") == Some(id)))
            .and_then(|e| json::get(e, "permission_decision"));
        let verdict = decision.and_then(|d| json::get_str(d, "decision"));
        let source = decision.and_then(|d| json::get_str(d, "source"));
        let result = json::get(frame, "tool_use_result").unwrap_or(&Json::Null);
        let cancelled = self.cancelled.iter().any(|c| c == id);
        let status = if cancelled && is_error {
            ToolStatus::Cancelled
        } else if verdict == Some("reject") {
            ToolStatus::Denied
        } else if is_error {
            ToolStatus::Failed
        } else {
            ToolStatus::Completed
        };
        let output = match json::get_str(result, "stdout") {
            Some(stdout) => join_streams(stdout, json::get_str(result, "stderr").unwrap_or("")),
            None if !is_error => content.clone(),
            None => String::new(),
        };
        let (output, output_truncated) = head_tail(&redact(&output), OUTPUT_MAX);
        let diffs = result_diffs(result, &self.workspace);
        let auto = (verdict == Some("accept"))
            .then_some(source)
            .flatten()
            .filter(|s| !s.starts_with("user"))
            .map(String::from);
        let id = id.to_string();
        self.finish_tool(&id, now, out, |t| {
            t.status = status;
            t.output = output;
            t.output_truncated = output_truncated;
            t.error = is_error.then(|| cap(&redact(&content), NOTICE_MAX).0);
            t.diffs = diffs;
            t.auto_approved = auto;
        });
    }

    fn result(&mut self, v: &Json, now: i64, out: &mut Vec<Event>) {
        let subtype = json::get_str(v, "subtype").unwrap_or("");
        let is_error = json::get_bool(v, "is_error") == Some(true);
        let aborted =
            json::get_str(v, "terminal_reason").is_some_and(|r| r.starts_with("aborted_"));
        let (status, prompt_status) = if subtype == "success" && !is_error {
            ("completed", "completed")
        } else if aborted {
            ("interrupted", "cancelled")
        } else {
            ("failed", "failed")
        };
        let error = (status == "failed").then(|| result_error(v, subtype));
        // total_cost_usd is cumulative (and /clear resets it)
        let cost = json::get(v, "total_cost_usd")
            .and_then(json::as_f64)
            .map(|total| {
                let delta = if total >= self.cost_total {
                    total - self.cost_total
                } else {
                    total
                };
                self.cost_total = total;
                delta
            });
        let tool_end = if aborted {
            ToolStatus::Cancelled
        } else {
            ToolStatus::Failed
        };
        self.finish_tools(tool_end, None, now, out);
        let turn = match self.turn.take() {
            Some(t) => t,
            None => Turn {
                id: self.synth_id("turn"),
                started_at: now,
                usage: zero_usage(),
            },
        };
        out.push(Event::Op(Op::Turn(TurnState {
            id: turn.id,
            status,
            started_at: turn.started_at,
            ended_at: Some(now),
            usage: Some(turn.usage),
            cost_usd: cost,
            error: error.clone(),
            diffs: Vec::new(),
        })));
        // started prompts end with their turn. Without lifecycle frames a
        // prompt folded into the turn never reports `started`: with no sends
        // left queued (`queued_turn_count`) it ended here too.
        let folded = !self.lifecycle && json::get_i64(v, "queued_turn_count").unwrap_or(0) == 0;
        for p in std::mem::take(&mut self.prompts) {
            if !(p.started || folded) {
                self.prompts.push(p);
                continue;
            }
            if p.queued {
                let mut item = user_item(&p);
                item.queued = false;
                out.push(Event::Op(Op::Item(item)));
            }
            out.push(Event::Prompt {
                command_id: p.command_id,
                state: PromptState::Done {
                    status: prompt_status,
                    error: error.clone(),
                },
            });
        }
        self.blocks.clear();
        self.open.clear();
        self.cancelled.clear();
        self.set_status("idle", out);
    }

    fn rate_limit(&mut self, v: &Json, now: i64, out: &mut Vec<Event>) {
        let Some(info) = json::get(v, "rate_limit_info") else {
            return;
        };
        let status = json::get_str(info, "status").unwrap_or("allowed");
        if status == "allowed" {
            return;
        }
        let kind = json::get_str(info, "rateLimitType").unwrap_or("usage");
        let window = format!(
            "{kind}:{status}:{}",
            json::get(info, "resetsAt")
                .map(json::stringify)
                .unwrap_or_default()
        );
        if self.rate_windows.contains(&window) {
            return; // once per window
        }
        push_capped(&mut self.rate_windows, window, 16);
        let limit = match kind {
            "five_hour" => "5-hour",
            "seven_day" => "7-day",
            "seven_day_opus" => "7-day Opus",
            "seven_day_sonnet" => "7-day Sonnet",
            "overage" | "seven_day_overage_included" => "extra usage",
            other => other,
        };
        let text = match (
            status,
            json::get(info, "utilization").and_then(json::as_f64),
        ) {
            ("allowed_warning", Some(u)) => {
                format!("Claude usage is at {:.0}% of the {limit} limit", u * 100.0)
            }
            ("allowed_warning", None) => format!("Claude usage is close to the {limit} limit"),
            _ => format!("Claude usage limit reached ({limit})"),
        };
        self.notice(out, now, json::get_str(v, "uuid"), "warning", &text);
    }

    fn lifecycle(&mut self, v: &Json, out: &mut Vec<Event>) {
        let (Some(uuid), Some(state)) =
            (json::get_str(v, "command_uuid"), json::get_str(v, "state"))
        else {
            return;
        };
        self.lifecycle = true;
        let Some(i) = self.prompts.iter().position(|p| p.uuid == uuid) else {
            return;
        };
        match state {
            "queued" if !self.prompts[i].queued => {
                self.prompts[i].queued = true;
                out.push(Event::Op(Op::Item(user_item(&self.prompts[i]))));
                out.push(Event::Prompt {
                    command_id: self.prompts[i].command_id.clone(),
                    state: PromptState::Queued,
                });
            }
            "started" => self.start_prompt(i, out),
            // it ran: its turn's result closes it, or already did unless it
            // never said `started` (then it ends here)
            "completed" => {
                self.start_prompt(i, out);
                if self.turn.is_none() {
                    let p = self.prompts.remove(i);
                    let state = PromptState::Done {
                        status: "completed",
                        error: None,
                    };
                    out.push(Event::Prompt {
                        command_id: p.command_id,
                        state,
                    });
                }
            }
            "cancelled" | "discarded" | "refused" => {
                let p = self.prompts.remove(i);
                let (status, error) = if state == "refused" {
                    ("failed", Some("Claude Code refused the prompt".to_string()))
                } else {
                    ("cancelled", None)
                };
                out.push(Event::Prompt {
                    command_id: p.command_id,
                    state: PromptState::Done { status, error },
                });
            }
            _ => {}
        }
    }

    fn start_prompt(&mut self, i: usize, out: &mut Vec<Event>) {
        let p = &mut self.prompts[i];
        if p.started {
            return;
        }
        p.started = true;
        if std::mem::take(&mut p.queued) {
            out.push(Event::Op(Op::Item(user_item(p))));
        }
        out.push(Event::Prompt {
            command_id: p.command_id.clone(),
            state: PromptState::Started,
        });
    }

    /* --------------------------- helpers --------------------------- */

    fn live_state(&self) -> LiveState {
        LiveState {
            status: self.status,
            permission_mode: self.permission_mode.clone(),
            model: self.model.clone(),
            models: self.models.clone(),
            commands: self.commands.clone(),
            context: self.context,
        }
    }

    /// Slash command names from a list of names (`system/init`) or of
    /// `{name}` objects (`initialize`, `commands_changed`), minus the hidden
    /// ones; true when they changed. Not a list: nothing changes.
    fn set_commands(&mut self, list: Option<&Json>) -> bool {
        let Some(list) = list.and_then(json::as_arr) else {
            return false;
        };
        let names: Vec<String> = list
            .iter()
            .filter_map(|c| json::as_str(c).or_else(|| json::get_str(c, "name")))
            .filter(|n| !n.is_empty() && !n.starts_with('_') && !HIDDEN_COMMANDS.contains(n))
            .map(plain_text)
            .collect();
        if names == self.commands {
            return false;
        }
        self.commands = names;
        true
    }

    fn set_status(&mut self, status: &'static str, out: &mut Vec<Event>) {
        if self.status == status {
            return;
        }
        self.status = status;
        out.push(Event::Op(Op::State(self.live_state())));
        if matches!(status, "running" | "waiting" | "idle") {
            out.push(Event::Status(status));
        }
    }

    /// Opens a turn on its first frame (`system/init`, `session_state_changed
    /// running`, or the first content); `system/init` repeats every turn and
    /// never resets anything.
    fn ensure_turn(&mut self, v: &Json, now: i64, out: &mut Vec<Event>) {
        if self.turn.is_some() {
            return;
        }
        let id = match json::get_str(v, "uuid") {
            Some(u) => u.to_string(),
            None => self.synth_id("turn"),
        };
        self.turn = Some(Turn {
            id: id.clone(),
            started_at: now,
            usage: zero_usage(),
        });
        out.push(Event::Op(Op::Turn(TurnState {
            id,
            status: "running",
            started_at: now,
            ended_at: None,
            usage: None,
            cost_usd: None,
            error: None,
            diffs: Vec::new(),
        })));
        self.set_status("running", out);
    }

    fn item(&self, id: &str, kind: ItemKind, ts: i64) -> Item {
        Item {
            id: id.to_string(),
            kind,
            ts,
            turn_id: self.turn.as_ref().map(|t| t.id.clone()),
            ..Default::default()
        }
    }

    fn notice(
        &mut self,
        out: &mut Vec<Event>,
        now: i64,
        id: Option<&str>,
        level: &'static str,
        text: &str,
    ) {
        let id = match id {
            Some(id) => id.to_string(),
            None => self.synth_id("notice"),
        };
        let mut item = self.item(&id, ItemKind::Notice, now);
        item.text = cap(&redact(text), NOTICE_MAX).0;
        item.level = Some(level);
        item.done = true;
        out.push(Event::Op(Op::Item(item)));
    }

    fn synth_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}-{}", self.seq)
    }

    /// Frame k of message `mid` carries its block k (one `assistant` frame
    /// per content block, in order), with or without stream events.
    fn next_block(&mut self, mid: &str) -> usize {
        if let Some((_, n)) = self.blocks.iter_mut().find(|(m, _)| m == mid) {
            *n += 1;
            return *n - 1;
        }
        if self.blocks.len() >= 32 {
            self.blocks.remove(0);
        }
        self.blocks.push((mid.to_string(), 1));
        0
    }

    fn hide(&mut self, id: String) {
        if !self.hidden.contains(&id) {
            push_capped(&mut self.hidden, id, 64);
        }
    }

    fn track_tool(&mut self, item: Item, now: i64) {
        if self.tools.len() >= TOOLS_MAX {
            self.tools.remove(0);
        }
        self.tools.push((item, now));
    }

    fn update_tool(&mut self, id: &str, out: &mut Vec<Event>, f: impl FnOnce(&mut ToolCall)) {
        if let Some((item, _)) = self.tools.iter_mut().find(|(i, _)| i.id == id)
            && let Some(t) = item.tool.as_mut()
        {
            f(t);
            out.push(Event::Op(Op::Item(item.clone())));
        }
    }

    fn finish_tool(
        &mut self,
        id: &str,
        now: i64,
        out: &mut Vec<Event>,
        f: impl FnOnce(&mut ToolCall),
    ) {
        let Some(at) = self.tools.iter().position(|(i, _)| i.id == id) else {
            return;
        };
        let (mut item, since) = self.tools.remove(at);
        if let Some(t) = item.tool.as_mut() {
            f(t);
            t.duration_ms = Some((now - since).max(0));
        }
        out.push(Event::Op(Op::Item(item)));
    }

    /// Tools that never got a result when their turn (or process) ended.
    fn finish_tools(
        &mut self,
        status: ToolStatus,
        why: Option<&str>,
        now: i64,
        out: &mut Vec<Event>,
    ) {
        for (mut item, since) in std::mem::take(&mut self.tools) {
            if let Some(t) = item.tool.as_mut() {
                t.status = status;
                t.error = why.map(String::from);
                t.duration_ms = Some((now - since).max(0));
            }
            out.push(Event::Op(Op::Item(item)));
        }
    }

    fn phone_callback(&self, server: &str) -> bool {
        !self.desktop_only.iter().any(|s| s == server)
    }

    fn mcp_reply(&mut self, body: &Json, out: &mut Vec<Event>) {
        let list = json::get(body, "mcpServers")
            .and_then(json::as_arr)
            .unwrap_or(&[]);
        let kind = |s: &Json| {
            json::get(s, "config")
                .and_then(|c| json::get_str(c, "type"))
                .unwrap_or("")
                .to_string()
        };
        self.mcp_kinds = list
            .iter()
            .filter_map(|s| Some((json::get_str(s, "name")?.to_string(), kind(s))))
            .collect();
        let servers = list
            .iter()
            .filter_map(|s| {
                let name = json::get_str(s, "name")?;
                Some(mcp_entry(
                    name,
                    json::get_str(s, "status").unwrap_or(""),
                    &kind(s),
                    json::get_str(s, "error"),
                    self.phone_callback(name),
                ))
            })
            .collect();
        out.push(Event::Op(Op::Mcp(servers)));
    }
}

fn user_item(p: &Prompt) -> Item {
    Item {
        id: p.uuid.clone(),
        kind: ItemKind::User,
        ts: p.ts,
        text: p.text.clone(),
        done: true,
        queued: p.queued,
        images: p.images,
        ..Default::default()
    }
}

fn tool_call(name: &str) -> ToolCall {
    ToolCall {
        name: name.to_string(),
        title: None,
        status: ToolStatus::Pending,
        input: String::new(),
        output: String::new(),
        output_truncated: false,
        error: None,
        diffs: Vec::new(),
        permission_id: None,
        auto_approved: None,
        duration_ms: None,
    }
}

fn zero_usage() -> Usage {
    Usage {
        input: 0,
        output: 0,
        cache_read: None,
        reasoning: None,
    }
}

fn push_capped(list: &mut Vec<String>, value: String, max: usize) {
    if list.len() >= max {
        list.remove(0);
    }
    list.push(value);
}

/* ------------------------------------------------------------------ */
/* Permission mapping (§4.5)                                           */
/* ------------------------------------------------------------------ */

/// Maps one `can_use_tool` request (`req` parsed, `raw` its exact text) to a
/// [`PermissionRequest`]: id `<adapterId>:<request_id>`, the CLI's
/// suggestions as options after the extension's filter (no `setMode
/// bypassPermissions`, nothing under `suppress_always_allow_rule`, nothing
/// unoffered), reject first under `default_to_no`, a per-tool `detail`, the
/// legacy v2 fields, and redaction over detail and context.
pub fn permission_request(
    adapter_id: &str,
    session_id: &str,
    workspace: &str,
    request_id: &str,
    req: &Json,
    raw: &str,
    now: i64,
) -> Result<Asked, &'static str> {
    let malformed = "malformed permission request";
    let tool = json::get_str(req, "tool_name")
        .filter(|t| !t.is_empty())
        .ok_or(malformed)?;
    let input = json::get(req, "input")
        .filter(|i| matches!(i, Json::Obj(_)))
        .ok_or(malformed)?;
    let input_raw = raw_member(raw, "input").ok_or(malformed)?;
    let display = json::get_str(req, "display_name")
        .filter(|d| !d.is_empty())
        .unwrap_or(tool);
    let described = describe(tool, input, req, workspace);
    let question = tool == "AskUserQuestion";
    let plan = tool == "ExitPlanMode";
    let (allow_label, deny_label) = if question {
        ("Submit answers", "Decline")
    } else if plan {
        ("Approve plan", "Keep planning")
    } else {
        ("Allow", "Deny")
    };
    let allow = PermissionOption {
        id: "allow".into(),
        decision: "approve-once".into(),
        label: allow_label.into(),
        rule: None,
        scope: "once",
        destination: None,
        takes_message: false,
        stops_turn: false,
    };
    let deny = PermissionOption {
        id: "deny".into(),
        decision: "reject".into(),
        label: deny_label.into(),
        takes_message: true,
        ..allow.clone()
    };
    let mut offered = Vec::new();
    let mut suggestions = Vec::new();
    if json::get_bool(req, "suppress_always_allow_rule") != Some(true) {
        let parsed = json::get(req, "permission_suggestions")
            .and_then(json::as_arr)
            .unwrap_or(&[]);
        let raws = raw_member(raw, "permission_suggestions")
            .map(raw_elements)
            .unwrap_or_default();
        for (i, (s, raw_s)) in parsed.iter().zip(raws).enumerate() {
            if let Some(o) = suggestion_option(i, s, plan) {
                suggestions.push((o.id.clone(), raw_s.to_string()));
                offered.push(o);
            }
        }
    }
    // default_to_no (and an ask that wants the session opened) makes Deny
    // the primary button, which the phone takes from options[0]
    let deny_first = json::get_bool(req, "default_to_no") == Some(true)
        || (!question && json::get_bool(req, "requires_user_interaction") == Some(true));
    let options: Vec<PermissionOption> = if deny_first {
        [deny, allow].into_iter().chain(offered).collect()
    } else {
        std::iter::once(allow)
            .chain(offered)
            .chain(std::iter::once(deny))
            .collect()
    };
    let supported = ["approve-once", "approve-session", "reject"]
        .into_iter()
        .filter(|d| {
            options
                .iter()
                .any(|o| o.decision == *d && o.scope != "persistent")
        })
        .map(String::from)
        .collect();
    let title = json::get_str(req, "title")
        .map(plain_text)
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| format!("Claude wants to use {display}"));
    let context = json::get_str(req, "description")
        .filter(|d| !d.trim().is_empty())
        .or_else(|| json::get_str(req, "decision_reason"))
        .map(|c| first_chars(&redact(&plain_text(c)), CONTEXT_CHARS))
        .unwrap_or_default();
    let request = PermissionRequest {
        id: format!("{adapter_id}:{request_id}"),
        native_request_id: request_id.into(),
        session_id: session_id.into(),
        agent_id: adapter_id.into(),
        host_id: adapter_id.into(),
        action: described.action.into(),
        title: first_chars(&redact(&title), CONTEXT_CHARS),
        command: redact(&described.command),
        args: Vec::new(),
        directory: (!workspace.is_empty()).then(|| workspace.to_string()),
        risk: described.risk.into(),
        status: "pending".into(),
        supported_decisions: supported,
        revision: 1,
        fingerprint: None,
        requested_at: now,
        expires_at: now + LIVE_PERMISSION_TTL_MS,
        context,
        decided_at: None,
        decision: None,
        from_pc: false,
        tool_item_id: json::get_str(req, "tool_use_id")
            .filter(|t| !t.is_empty())
            .map(String::from),
        options,
        detail: Some(described.detail),
    };
    Ok(Asked {
        request,
        input_raw: input_raw.to_string(),
        input: input.clone(),
        suggestions,
        questions: described.questions,
    })
}

/// One suggestion as an option (labels after the VS Code extension's
/// wording), or `None` when it must not be offered.
fn suggestion_option(i: usize, s: &Json, plan: bool) -> Option<PermissionOption> {
    let dest = json::get_str(s, "destination")?;
    let (scope, file, dest_label) = match dest {
        "session" => ("session", None, "this session"),
        "cliArg" => ("session", None, "startup options"),
        "localSettings" => (
            "persistent",
            Some(".claude/settings.local.json"),
            "this project (just you)",
        ),
        "projectSettings" => (
            "persistent",
            Some(".claude/settings.json"),
            "this project (shared)",
        ),
        "userSettings" => (
            "persistent",
            Some("~/.claude/settings.json"),
            "all projects",
        ),
        _ => return None,
    };
    let (label, rule) = match json::get_str(s, "type")? {
        "setMode" => {
            let mode = json::get_str(s, "mode")?;
            // the same whitelist as set_permission_mode: bypassPermissions,
            // dontAsk and auto are never reachable from the phone (T4)
            if !matches!(mode, "default" | "acceptEdits" | "plan") {
                return None;
            }
            let label = match (mode, dest) {
                ("acceptEdits", _) if plan => "Approve and auto-accept edits".to_string(),
                ("acceptEdits", "session") => "Allow all edits this session".to_string(),
                ("default", "session") => "Allow and return to normal mode".to_string(),
                _ => format!("Allow and switch to {} for {dest_label}", mode_name(mode)),
            };
            (label, None)
        }
        "addRules" => {
            if json::get_str(s, "behavior") != Some("allow") {
                return None;
            }
            let rules = json::get(s, "rules")
                .and_then(json::as_arr)
                .filter(|r| !r.is_empty())?;
            let mut shown = Vec::new();
            let mut text = Vec::new();
            for r in rules {
                let tool = json::get_str(r, "toolName")?;
                match json::get_str(r, "ruleContent") {
                    Some(c) => {
                        shown.push(format!("`{c}`"));
                        text.push(format!("{tool}({c})"));
                    }
                    None => {
                        shown.push(format!("`{tool}`"));
                        text.push(tool.to_string());
                    }
                }
            }
            let label = format!("Allow {} for {dest_label}", and_list(&shown));
            (label, Some(text.join(", ")))
        }
        "addDirectories" => {
            let dirs: Vec<&str> = json::get(s, "directories")
                .and_then(json::as_arr)?
                .iter()
                .filter_map(json::as_str)
                .collect();
            if dirs.is_empty() {
                return None;
            }
            let shown: Vec<String> = dirs.iter().map(|d| format!("`{d}`")).collect();
            let label = format!("Allow access to {} for {dest_label}", and_list(&shown));
            (label, Some(dirs.join(", ")))
        }
        _ => return None, // replaceRules, removeRules, …: not offered
    };
    Some(PermissionOption {
        id: format!("suggestion-{i}"),
        decision: "approve-session".into(),
        label: redact(&label),
        rule: rule.map(|r| redact(&r)),
        scope,
        destination: file.map(String::from),
        takes_message: false,
        stops_turn: false,
    })
}

fn mode_name(mode: &str) -> String {
    match mode {
        "acceptEdits" => "accept-edits mode".into(),
        "default" => "normal mode".into(),
        "plan" => "plan mode".into(),
        "dontAsk" => "don't-ask mode".into(),
        other => format!("{other} mode"),
    }
}

fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

struct Described {
    detail: Json,
    action: &'static str,
    risk: &'static str,
    command: String,
    questions: Vec<(String, String)>,
}

/// `detail` by tool (§4.5 / `PermissionDetail`) plus the legacy action,
/// risk and command.
fn describe(tool: &str, input: &Json, req: &Json, workspace: &str) -> Described {
    let get = |k: &str| json::get_str(input, k).unwrap_or("").to_string();
    let mut questions = Vec::new();
    let (detail, action, risk, command) = match tool {
        "Bash" => {
            // capped like every other input; the full text stays in `input_raw`
            let command = cap_marked(&get("command"), INPUT_MAX);
            let mut f = vec![("kind", json::s("shell")), ("command", json::s(&command))];
            if !workspace.is_empty() {
                f.push(("cwd", json::s(workspace)));
            }
            if let Some(d) = json::get_str(input, "description").filter(|d| !d.is_empty()) {
                f.push(("reason", json::s(d)));
            }
            (json::obj(f), "shell", "high", command)
        }
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            let path = json::get_str(input, "file_path")
                .or_else(|| json::get_str(input, "notebook_path"))
                .unwrap_or("")
                .to_string();
            let mut f = vec![("kind", json::s("edit")), ("path", json::s(&path))];
            let patch = edit_patch(tool, input);
            if !patch.is_empty() {
                f.push(("patch", json::s(&cap(&patch, DIFF_MAX).0)));
            }
            (json::obj(f), "file-write", "medium", path)
        }
        "Read" => {
            let path = get("file_path");
            let d = json::obj(vec![("kind", json::s("read")), ("path", json::s(&path))]);
            (d, "agent-control", "low", path)
        }
        "WebFetch" | "WebSearch" => {
            let url = json::get_str(input, "url")
                .or_else(|| json::get_str(input, "query"))
                .unwrap_or("")
                .to_string();
            let d = json::obj(vec![("kind", json::s("fetch")), ("url", json::s(&url))]);
            (d, "network", "medium", url)
        }
        "AskUserQuestion" => {
            let mut list = Vec::new();
            let qs = json::get(input, "questions")
                .and_then(json::as_arr)
                .unwrap_or(&[]);
            for (i, qn) in qs.iter().enumerate() {
                let id = format!("q{i}");
                let text = json::get_str(qn, "question").unwrap_or("").to_string();
                let options = json::get(qn, "options")
                    .and_then(json::as_arr)
                    .unwrap_or(&[])
                    .iter()
                    .map(|o| {
                        let mut f =
                            vec![("label", json::s(json::get_str(o, "label").unwrap_or("")))];
                        if let Some(d) = json::get_str(o, "description") {
                            f.push(("description", json::s(d)));
                        }
                        json::obj(f)
                    })
                    .collect();
                let mut f = vec![("id", json::s(&id)), ("text", json::s(&text))];
                if let Some(h) = json::get_str(qn, "header") {
                    f.push(("header", json::s(h)));
                }
                f.push(("options", Json::Arr(options)));
                let multi = json::get_bool(qn, "multiSelect") == Some(true);
                f.push(("multiSelect", Json::Bool(multi)));
                f.push(("allowFreeText", Json::Bool(true)));
                list.push(json::obj(f));
                questions.push((id, text));
            }
            let first = questions
                .first()
                .map(|(_, t)| t.clone())
                .unwrap_or_default();
            let d = json::obj(vec![
                ("kind", json::s("question")),
                ("questions", Json::Arr(list)),
            ]);
            (d, "agent-control", "low", first)
        }
        "ExitPlanMode" => {
            let d = json::obj(vec![
                ("kind", json::s("plan")),
                ("plan", json::s(&get("plan"))),
            ]);
            (d, "agent-control", "low", tool.to_string())
        }
        t if t.starts_with("mcp__") => {
            let rest = &t["mcp__".len()..];
            let (server, name) = rest.split_once("__").unwrap_or((rest, ""));
            // the provenance name reads better than the sanitized one in the tool name
            let server = json::get(req, "mcp_server")
                .and_then(|m| json::get_str(m, "name"))
                .unwrap_or(server);
            let d = json::obj(vec![
                ("kind", json::s("mcp")),
                ("server", json::s(server)),
                ("tool", json::s(name)),
                ("input", json::s(&cap_marked(&pretty(input), INPUT_MAX))),
            ]);
            (d, "agent-control", "medium", t.to_string())
        }
        _ => {
            let d = json::obj(vec![
                ("kind", json::s("generic")),
                ("tool", json::s(tool)),
                ("input", json::s(&cap_marked(&pretty(input), INPUT_MAX))),
            ]);
            let display = json::get_str(req, "display_name").unwrap_or(tool);
            (d, "agent-control", "low", display.to_string())
        }
    };
    Described {
        detail: redact_json(detail),
        action,
        risk,
        command,
        questions,
    }
}

/// The §4.5 answer body for `option`.
fn answer_body(a: &Asked, option: &PermissionOption, d: &Decision) -> Result<String, &'static str> {
    if option.decision == "reject" {
        let message = d
            .message
            .filter(|m| !m.trim().is_empty())
            .unwrap_or(DENIED_FROM_PHONE);
        return Ok(format!(
            "{{\"behavior\":\"deny\",\"message\":{}}}",
            q(message)
        ));
    }
    let input = if a.questions.is_empty() {
        a.input_raw.clone()
    } else {
        answered_input(a, d.answers)?
    };
    let mut body = format!("{{\"behavior\":\"allow\",\"updatedInput\":{input}");
    if let Some((_, raw)) = a.suggestions.iter().find(|(id, _)| *id == option.id) {
        body.push_str(&format!(",\"updatedPermissions\":[{raw}]"));
    }
    body.push('}');
    Ok(body)
}

/// AskUserQuestion: the original input plus `answers` keyed by question
/// text; several picks join with ", " like the extension does.
fn answered_input(a: &Asked, answers: &[(String, Vec<String>)]) -> Result<String, &'static str> {
    let mut picked = Vec::new();
    for (qid, values) in answers {
        let (_, text) = a
            .questions
            .iter()
            .find(|(id, _)| id == qid)
            .ok_or("the answers name a question that was not asked")?;
        let chosen: Vec<&str> = values
            .iter()
            .map(String::as_str)
            .filter(|v| !v.is_empty())
            .collect();
        if !chosen.is_empty() {
            picked.push((text.clone(), Json::Str(chosen.join(", "))));
        }
    }
    if picked.is_empty() {
        return Err("answer at least one question");
    }
    let Json::Obj(pairs) = &a.input else {
        return Err("malformed permission request");
    };
    let mut pairs: Vec<(String, Json)> = pairs
        .iter()
        .filter(|(k, _)| k != "answers")
        .cloned()
        .collect();
    pairs.push(("answers".into(), Json::Obj(picked)));
    Ok(json::stringify(&Json::Obj(pairs)))
}

fn success_response(request_id: &str, body: &str) -> String {
    format!(
        "{{\"type\":\"control_response\",\"response\":{{\"subtype\":\"success\",\"request_id\":{},\"response\":{body}}}}}",
        q(request_id)
    )
}

fn error_response(request_id: &str, error: &str) -> String {
    format!(
        "{{\"type\":\"control_response\",\"response\":{{\"subtype\":\"error\",\"request_id\":{},\"error\":{}}}}}",
        q(request_id),
        q(error)
    )
}

/// The `request_id` of a line that does not parse, by a shallow scan: the
/// CLI writes `type`, then `request_id`, then `request`.
fn shallow_request_id(line: &str) -> Option<&str> {
    let key = "\"request_id\":\"";
    let start = line.find(key)? + key.len();
    let len = line[start..].find('"')?;
    let id = &line[start..start + len];
    (!id.is_empty() && id.len() <= 128 && !id.contains('\\')).then_some(id)
}

/* ------------------------------------------------------------------ */
/* MCP (§4.7)                                                          */
/* ------------------------------------------------------------------ */

/// One server as the phone sees it. `authFlows`: claude.ai connectors open
/// claude.ai; remote (http/sse) servers sign in on the desktop, and from the
/// phone unless their last sign-in had no localhost callback; stdio servers
/// have nothing to do.
fn mcp_entry(
    name: &str,
    status: &str,
    kind: &str,
    error: Option<&str>,
    phone_callback: bool,
) -> McpServer {
    let mapped = match status {
        "connected" => "connected",
        "pending" => "pending",
        "needs-auth" => "needs-auth",
        "disabled" => "disabled",
        _ => "failed",
    };
    let error = error
        .map(str::to_string)
        .or_else(|| (mapped == "failed" && status != "failed").then(|| format!("status: {status}")))
        .map(|e| cap(&redact(&e), NOTICE_MAX).0);
    let (auth_flows, auth) = match (mapped, kind) {
        ("needs-auth", "claudeai-proxy") => (
            vec!["claude-ai"],
            Some(McpAuthState {
                state: "awaiting-browser",
                url: Some(CLAUDE_AI_CONNECTORS.into()),
                callback_prefix: None,
                error: None,
            }),
        ),
        ("needs-auth", "http" | "sse") if phone_callback => {
            (vec!["desktop", "phone-callback"], None)
        }
        ("needs-auth", "http" | "sse") => (vec!["desktop"], None),
        _ => (Vec::new(), None),
    };
    McpServer {
        name: first_chars(name, 256),
        status: mapped,
        error,
        auth_flows,
        auth,
    }
}

/// Reads the `mcp_authenticate` answer: `authUrl`, and a phone-pasteable
/// callback when the redirect is `http://localhost:<callbackPort><path>`
/// (or 127.0.0.1) with a `state` to check (§4.7, T5).
pub fn auth_start(body: &Json) -> Result<AuthStart, &'static str> {
    let auth_url = json::get_str(body, "authUrl")
        .filter(|u| !u.is_empty())
        .ok_or("Claude Code returned no sign-in URL")?
        .to_string();
    let port = json::get_i64(body, "callbackPort").and_then(|p| u16::try_from(p).ok());
    let state = json::get_str(body, "state").unwrap_or("");
    let localhost = json::get_str(body, "redirectScheme") == Some("localhost")
        && json::get_bool(body, "callbackExpected") != Some(false);
    let callback = port
        .filter(|_| localhost && !state.is_empty())
        .and_then(|port| {
            let redirect = query_param(&auth_url, "redirect_uri")?;
            let rest = redirect.strip_prefix("http://")?;
            let slash = rest.find('/')?;
            let (authority, path) = rest.split_at(slash);
            let (host, p) = authority.rsplit_once(':')?;
            let ok = matches!(host, "localhost" | "127.0.0.1")
                && p == port.to_string()
                && !path.contains(['?', '#']);
            ok.then(|| Callback {
                port,
                path: path.to_string(),
                state: state.to_string(),
                prefix: format!("http://{authority}{path}"),
            })
        });
    Ok(AuthStart { auth_url, callback })
}

/// A query parameter of `url`, percent- and `+`-decoded.
fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1.split('#').next()?;
    let raw = query
        .split('&')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))?;
    let b = raw.as_bytes();
    let digit = |k: usize| b.get(k).and_then(|&c| (c as char).to_digit(16));
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                out.push((digit(i + 1)? * 16 + digit(i + 2)?) as u8);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

/* ------------------------------------------------------------------ */
/* Tool display: titles, inputs, outputs, diffs, plans                 */
/* ------------------------------------------------------------------ */

fn tool_title(name: &str, input: &Json, workspace: &str) -> Option<String> {
    let get = |k: &str| json::get_str(input, k);
    let title = match name {
        "Bash" => get("command").map(|c| c.lines().next().unwrap_or("").to_string()),
        "Edit" | "Write" | "MultiEdit" | "Read" | "NotebookEdit" => get("file_path")
            .or_else(|| get("notebook_path"))
            .map(|p| display_path(p, workspace)),
        "Glob" | "Grep" => get("pattern").map(String::from),
        "WebFetch" => get("url").map(String::from),
        "WebSearch" => get("query").map(String::from),
        "Task" | "Agent" => get("description").map(String::from),
        "AskUserQuestion" => json::get(input, "questions")
            .and_then(json::as_arr)
            .and_then(|q| q.first())
            .and_then(|q| json::get_str(q, "question"))
            .map(String::from),
        _ => None,
    }?;
    let title = redact(&first_chars(title.trim(), TITLE_CHARS));
    (!title.is_empty()).then_some(title)
}

/// Pretty JSON, or the raw command for Bash (`ToolCall.input`).
fn tool_input(name: &str, input: &Json) -> String {
    let text = match (name, json::get_str(input, "command")) {
        ("Bash", Some(c)) => c.to_string(),
        _ => pretty(input),
    };
    cap_marked(&redact(&text), INPUT_MAX)
}

/// Inside the workspace a path shows relative to it (the card's title and
/// the diff header); anything else stays absolute.
fn display_path(path: &str, workspace: &str) -> String {
    let ws = workspace.trim_end_matches(['\\', '/']);
    match path.get(..ws.len()) {
        Some(head)
            if !ws.is_empty()
                && head.eq_ignore_ascii_case(ws)
                && matches!(path.as_bytes().get(ws.len()), Some(b'\\' | b'/'))
                && path.len() > ws.len() + 1 =>
        {
            path[ws.len() + 1..].to_string()
        }
        _ => path.to_string(),
    }
}

fn join_streams(stdout: &str, stderr: &str) -> String {
    let mut out = stdout.to_string();
    if !stderr.trim().is_empty() {
        if !out.is_empty() && !out.ends_with('\n') && !stderr.starts_with('\n') {
            out.push('\n');
        }
        out.push_str(stderr);
    }
    out
}

/// `tool_result.content` as text: a string, or its text blocks.
fn content_text(content: Option<&Json>) -> String {
    match content {
        Some(Json::Str(s)) => s.clone(),
        Some(Json::Arr(blocks)) => blocks
            .iter()
            .filter_map(|b| json::get_str(b, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Diffs from `tool_use_result`: Write `create` is all added, Edit and Write
/// `update` render their `structuredPatch` hunks.
fn result_diffs(result: &Json, workspace: &str) -> Vec<FileDiff> {
    let Some(path) = json::get_str(result, "filePath") else {
        return Vec::new();
    };
    let (patch, status) = if json::get_str(result, "type") == Some("create") {
        (
            added_patch(json::get_str(result, "content").unwrap_or("")),
            "added",
        )
    } else {
        let hunks = json::get(result, "structuredPatch")
            .and_then(json::as_arr)
            .unwrap_or(&[]);
        if hunks.is_empty() {
            return Vec::new();
        }
        (structured_patch(hunks), "modified")
    };
    vec![file_diff(display_path(path, workspace), status, &patch)]
}

fn file_diff(path: String, status: &'static str, patch: &str) -> FileDiff {
    let count = |c: char| patch.lines().filter(|l| l.starts_with(c)).count() as i64;
    let (patch, truncated) = cap(&redact(patch), DIFF_MAX);
    FileDiff {
        path,
        status,
        additions: count('+'),
        deletions: count('-'),
        patch,
        truncated,
    }
}

/// `@@ -a,b +c,d @@`, omitting a count of 1 like diff(1).
fn hunk_header(old_start: i64, old_lines: i64, new_start: i64, new_lines: i64) -> String {
    let range = |start: i64, n: i64| {
        if n == 1 {
            start.to_string()
        } else {
            format!("{start},{n}")
        }
    };
    format!(
        "@@ -{} +{} @@",
        range(old_start, old_lines),
        range(new_start, new_lines)
    )
}

fn structured_patch(hunks: &[Json]) -> String {
    let mut out = Vec::new();
    for h in hunks {
        let n = |k: &str| json::get_i64(h, k).unwrap_or(0);
        out.push(hunk_header(
            n("oldStart"),
            n("oldLines"),
            n("newStart"),
            n("newLines"),
        ));
        let lines = json::get(h, "lines").and_then(json::as_arr).unwrap_or(&[]);
        out.extend(lines.iter().filter_map(json::as_str).map(String::from));
    }
    out.join("\n")
}

fn split_lines(s: &str) -> Vec<&str> {
    let s = s.strip_suffix('\n').unwrap_or(s);
    if s.is_empty() {
        return Vec::new();
    }
    s.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

pub(crate) fn added_patch(content: &str) -> String {
    let lines = split_lines(content);
    if lines.is_empty() {
        return String::new();
    }
    let mut out = vec![hunk_header(0, 0, 1, lines.len() as i64)];
    out.extend(lines.iter().map(|l| format!("+{l}")));
    out.join("\n")
}

/// The unified diff of `old` against `new` as two texts (an Edit before it
/// runs, so file line numbers are unknown): common leading and trailing
/// lines are context, the rest is replaced.
pub(crate) fn replace_patch(old: &str, new: &str) -> String {
    let (a, b) = (split_lines(old), split_lines(new));
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let start = |n: usize| i64::from(n > 0);
    let mut out = vec![hunk_header(
        start(a.len()),
        a.len() as i64,
        start(b.len()),
        b.len() as i64,
    )];
    out.extend(a[..pre].iter().map(|l| format!(" {l}")));
    out.extend(a[pre..a.len() - suf].iter().map(|l| format!("-{l}")));
    out.extend(b[pre..b.len() - suf].iter().map(|l| format!("+{l}")));
    out.extend(a[a.len() - suf..].iter().map(|l| format!(" {l}")));
    out.join("\n")
}

/// The patch a permission prompt shows: from old/new strings or content.
fn edit_patch(tool: &str, input: &Json) -> String {
    let get = |v: &Json, k: &str| json::get_str(v, k).unwrap_or("").to_string();
    match tool {
        "Edit" => replace_patch(&get(input, "old_string"), &get(input, "new_string")),
        "MultiEdit" => json::get(input, "edits")
            .and_then(json::as_arr)
            .unwrap_or(&[])
            .iter()
            .map(|e| replace_patch(&get(e, "old_string"), &get(e, "new_string")))
            .collect::<Vec<_>>()
            .join("\n"),
        "Write" => added_patch(&get(input, "content")),
        _ => added_patch(&get(input, "new_source")), // NotebookEdit
    }
}

/// TodoWrite `todos[]` → plan steps (`content`/`status` map directly).
fn plan_steps(input: &Json) -> Vec<PlanStep> {
    json::get(input, "todos")
        .and_then(json::as_arr)
        .unwrap_or(&[])
        .iter()
        .map(|t| PlanStep {
            text: redact(json::get_str(t, "content").unwrap_or("")),
            status: match json::get_str(t, "status") {
                Some("in_progress") => "in_progress",
                Some("completed") => "completed",
                Some("cancelled") => "cancelled",
                _ => "pending",
            },
        })
        .collect()
}

fn result_error(v: &Json, subtype: &str) -> String {
    let text = json::get_str(v, "result")
        .filter(|r| !r.trim().is_empty())
        .map(String::from)
        .or_else(|| {
            let errors: Vec<&str> = json::get(v, "errors")
                .and_then(json::as_arr)
                .unwrap_or(&[])
                .iter()
                .filter_map(json::as_str)
                .collect();
            (!errors.is_empty()).then(|| errors.join("; "))
        })
        .unwrap_or_else(|| match subtype {
            "error_max_turns" => "Claude Code stopped after the maximum number of turns".into(),
            "error_max_budget_usd" => "Claude Code stopped at its budget limit".into(),
            other => format!("Claude Code turn failed ({other})"),
        });
    cap(&redact(&plain_text(&text)), NOTICE_MAX).0
}

/* ------------------------------------------------------------------ */
/* Text and JSON helpers                                               */
/* ------------------------------------------------------------------ */

fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    json::write_str(&mut out, s);
    out
}

fn cap(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

fn cap_marked(s: &str, max: usize) -> String {
    match cap(s, max) {
        (s, false) => s,
        (s, true) => format!("{s}\n…"),
    }
}

/// Head + tail within `max` bytes (`ToolCall.output`, §3.5).
fn head_tail(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut head = max / 2;
    while !s.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = s.len() - max / 2;
    while !s.is_char_boundary(tail) {
        tail += 1;
    }
    (format!("{}\n…\n{}", &s[..head], &s[tail..]), true)
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Drops ANSI escape sequences and control characters (the CLI says
/// `decision_reason` may carry them) but keeps newlines and tabs; invisible
/// format characters (bidi, zero-width) become visible `<U+XXXX>` markers.
fn plain_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        if !c.is_control() || c == '\n' || c == '\t' {
            out.push(c);
        }
    }
    show_format_chars(&out)
}

fn redact_json(v: Json) -> Json {
    match v {
        Json::Str(s) => Json::Str(redact(&s)),
        Json::Arr(a) => Json::Arr(a.into_iter().map(redact_json).collect()),
        Json::Obj(o) => Json::Obj(o.into_iter().map(|(k, v)| (k, redact_json(v))).collect()),
        other => other,
    }
}

/// Two-space pretty JSON for tool inputs.
fn pretty(v: &Json) -> String {
    fn go(out: &mut String, v: &Json, depth: usize) {
        let indent = |out: &mut String, d: usize| {
            out.push('\n');
            out.push_str(&"  ".repeat(d));
        };
        match v {
            Json::Arr(items) if !items.is_empty() => {
                out.push('[');
                for (i, x) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    indent(out, depth + 1);
                    go(out, x, depth + 1);
                }
                indent(out, depth);
                out.push(']');
            }
            Json::Obj(pairs) if !pairs.is_empty() => {
                out.push('{');
                for (i, (k, x)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    indent(out, depth + 1);
                    json::write_str(out, k);
                    out.push_str(": ");
                    go(out, x, depth + 1);
                }
                indent(out, depth);
                out.push('}');
            }
            other => json::write(out, other),
        }
    }
    let mut out = String::new();
    go(&mut out, v, 0);
    out
}

/// The exact source text of `key`'s value in the JSON object `obj`, which
/// `json::parse` already accepted: echoes go back byte for byte (§4.5).
pub(crate) fn raw_member<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let b = obj.as_bytes();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    loop {
        i = skip_ws(b, i);
        if b.get(i) != Some(&b'"') {
            return None;
        }
        let key_end = value_end(b, i);
        let name = json::parse(obj.get(i..key_end)?).ok()?;
        i = skip_ws(b, key_end);
        if b.get(i) != Some(&b':') {
            return None;
        }
        let start = skip_ws(b, i + 1);
        let end = value_end(b, start);
        if json::as_str(&name) == Some(key) {
            return obj.get(start..end);
        }
        i = skip_ws(b, end);
        if b.get(i) != Some(&b',') {
            return None;
        }
        i += 1;
    }
}

/// The exact source text of each element of the JSON array `arr`.
fn raw_elements(arr: &str) -> Vec<&str> {
    let b = arr.as_bytes();
    let mut out = Vec::new();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'[') {
        return out;
    }
    i = skip_ws(b, i + 1);
    if b.get(i) == Some(&b']') {
        return out;
    }
    while i < b.len() {
        let end = value_end(b, i);
        match arr.get(i..end) {
            Some(el) => out.push(el),
            None => break,
        }
        i = skip_ws(b, end);
        if b.get(i) != Some(&b',') {
            break;
        }
        i = skip_ws(b, i + 1);
    }
    out
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while matches!(b.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        i += 1;
    }
    i
}

/// End of the JSON value starting at `i` (input already validated).
fn value_end(b: &[u8], i: usize) -> usize {
    match b.get(i) {
        Some(b'"') => {
            let mut k = i + 1;
            while k < b.len() {
                match b[k] {
                    b'\\' => k += 2,
                    b'"' => return k + 1,
                    _ => k += 1,
                }
            }
            b.len()
        }
        Some(b'{' | b'[') => {
            let (mut depth, mut k) = (0usize, i);
            while k < b.len() {
                match b[k] {
                    b'"' => {
                        k = value_end(b, k);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return k + 1;
                        }
                    }
                    _ => {}
                }
                k += 1;
            }
            b.len()
        }
        _ => {
            let mut k = i;
            while k < b.len() && !matches!(b[k], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                k += 1;
            }
            k
        }
    }
}

/* ------------------------------------------------------------------ */
/* Tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    /// Security pass 2026-11-15 (M3): text from the CLI shown on an approval
    /// card has controls dropped and format characters made visible.
    #[test]
    fn plain_text_marks_bidi_and_zero_width_characters() {
        assert_eq!(
            plain_text("ok \u{1b}[31mred\u{1b}[0m\u{7} \u{202E}fdp.exe"),
            "ok red <U+202E>fdp.exe"
        );
        assert_eq!(plain_text("a\u{200B}b\nc\td"), "a<U+200B>b\nc\td");
    }
    use crate::runtime_io::{ChildEvent, ChildIo, PendingAuth, check_callback_url};
    use std::cell::RefCell;
    use std::time::Duration;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!("../tests/fixtures/claude/", $name))
        };
    }

    /// The captures' workspaces after sanitizing (`<tmp>` = the research dir).
    const RUN1: &str = "<tmp>\\run";

    /// One capture line: direction, ms since spawn, and the frame's exact text.
    fn frames(capture: &str) -> Vec<(String, i64, &str)> {
        capture
            .lines()
            .map(|l| {
                let v = json::parse(l).unwrap();
                let dir = json::get_str(&v, "dir").unwrap_or("").to_string();
                let t = json::get_i64(&v, "t").unwrap_or(0);
                (dir, t, raw_member(l, "msg").unwrap())
            })
            .collect()
    }

    /// The exact text of the first frame in `dir` matching `pred`.
    fn find<'a>(capture: &'a str, dir: &str, pred: impl Fn(&Json) -> bool) -> &'a str {
        frames(capture)
            .into_iter()
            .find(|(d, _, m)| d == dir && pred(&json::parse(m).unwrap()))
            .map(|(_, _, m)| m)
            .expect("frame in the capture")
    }

    fn subtype(v: &Json) -> &str {
        json::get(v, "request")
            .and_then(|r| json::get_str(r, "subtype"))
            .unwrap_or("")
    }

    fn tool_named(v: &Json, name: &str) -> bool {
        json::get(v, "request").and_then(|r| json::get_str(r, "tool_name")) == Some(name)
    }

    struct Answer {
        decision: &'static str,
        option: Option<&'static str>,
        message: Option<&'static str>,
        answers: Vec<(String, Vec<String>)>,
    }

    fn pick(decision: &'static str, option: &'static str) -> Answer {
        Answer {
            decision,
            option: Some(option),
            message: None,
            answers: Vec::new(),
        }
    }

    struct Replay {
        d: Driver,
        events: Vec<Event>,
        /// Every frame our driver wrote, in order.
        stdin: Vec<String>,
    }

    /// Replays a research capture with this driver as the host: our encoders
    /// stand in for the research driver's requests and prompts (and must
    /// write its frames, request ids aside), `answer` decides each
    /// permission, and stdout frames are decoded with the research request
    /// ids mapped to ours.
    fn replay(
        capture: &str,
        ws: &str,
        mut answer: impl FnMut(&PermissionRequest) -> Option<Answer>,
    ) -> Replay {
        let mut r = Replay {
            d: Driver::new("claude-1", "ses-1", ws),
            events: Vec::new(),
            stdin: Vec::new(),
        };
        let mut ids: Vec<(String, String)> = Vec::new();
        let mut prompts = 0;
        for (dir, t, msg) in frames(capture) {
            let v = json::parse(msg).unwrap_or(Json::Null);
            if dir == "in" {
                let log = RefCell::new(Vec::new());
                let send = |f: String| {
                    log.borrow_mut().push(f);
                    Ok(())
                };
                match json::get_str(&v, "type") {
                    Some("control_request") => {
                        let req = json::get(&v, "request").unwrap();
                        let arg = |k: &str| json::get_str(req, k).unwrap();
                        let res = match subtype(&v) {
                            "initialize" => r.d.initialize(send),
                            "mcp_status" => r.d.mcp_status(send),
                            "interrupt" => r.d.interrupt("cmd-interrupt", send),
                            "set_permission_mode" => {
                                r.d.set_permission_mode("cmd-mode", arg("mode"), send)
                            }
                            "mcp_authenticate" => {
                                r.d.mcp_authenticate("cmd-auth", arg("serverName"), send)
                            }
                            "mcp_oauth_callback_url" => r.d.mcp_oauth_callback_url(
                                "cmd-callback",
                                arg("serverName"),
                                arg("callbackUrl"),
                                send,
                            ),
                            "mcp_reconnect" => {
                                r.d.mcp_reconnect(Some("cmd-reconnect"), arg("serverName"), send)
                            }
                            _ => continue, // get_context_usage, mcp_clear_auth: not companion requests
                        };
                        res.unwrap();
                        let ours = log.borrow_mut().pop().unwrap();
                        let theirs = json::get_str(&v, "request_id").unwrap().to_string();
                        let chb = format!("chb_{}", ids.len() + 1);
                        // same subtype and field names as the live research frames
                        let want = msg.replace(&format!("\"{theirs}\""), &format!("\"{chb}\""));
                        let mut got = ours.clone();
                        if !want.contains("\"hooks\"") {
                            got = got.replace(",\"hooks\":null", ""); // run 1-3 sent no hooks
                        }
                        assert_eq!(got, want, "request frame for {}", subtype(&v));
                        ids.push((theirs, chb));
                        r.stdin.push(ours);
                    }
                    Some("user") => {
                        let message = json::get(&v, "message").unwrap();
                        let text = match json::get(message, "content") {
                            Some(Json::Str(s)) => s.clone(),
                            other => content_text(other),
                        };
                        prompts += 1;
                        let command = format!("cmd-{prompts}");
                        let op = r.d.user_prompt(&command, &text, &[], t, send).unwrap();
                        r.events.push(Event::Op(op));
                        r.stdin.extend(log.into_inner());
                    }
                    _ => {} // the research driver's answers: ours come from `answer`
                }
            } else if dir == "out" {
                let mut line = msg.to_string();
                if line.starts_with("{\"type\":\"control_response\"") {
                    for (theirs, ours) in &ids {
                        line = line.replace(
                            &format!("\"request_id\":\"{theirs}\""),
                            &format!("\"request_id\":\"{ours}\""),
                        );
                    }
                }
                let batch = r.d.on_line(&line, t);
                // like production: the phone decides after the batch is out
                for e in &batch {
                    if let Event::Write(f) = e {
                        r.stdin.push(f.clone());
                    }
                }
                r.events.extend(batch.iter().cloned());
                for p in permissions(&batch) {
                    let Some(a) = answer(p) else { continue };
                    let dec = Decision {
                        permission_id: &p.id,
                        decision: a.decision,
                        option_id: a.option,
                        message: a.message,
                        answers: &a.answers,
                    };
                    let log = RefCell::new(Vec::new());
                    let after =
                        r.d.decide(&dec, |f| {
                            log.borrow_mut().push(f);
                            Ok(())
                        })
                        .unwrap();
                    r.stdin.extend(log.into_inner());
                    r.events.extend(after);
                }
            }
        }
        r
    }

    /// A readable line per event.
    fn summary(e: &Event) -> String {
        match e {
            Event::Op(Op::Item(i)) => match &i.tool {
                Some(t) => {
                    let mut s = format!("tool {} {} {:?}", i.id, t.name, t.status);
                    if let Some(x) = &t.title {
                        s += &format!(" title={x:?}");
                    }
                    if let Some(x) = &t.auto_approved {
                        s += &format!(" auto={x}");
                    }
                    if let Some(x) = &t.permission_id {
                        s += &format!(" perm={x}");
                    }
                    if let Some(x) = t.output.lines().next() {
                        s += &format!(" out={x:?}");
                    }
                    if let Some(x) = &t.error {
                        s += &format!(" error={x:?}");
                    }
                    for d in &t.diffs {
                        s += &format!(
                            " diff={} {} +{} -{}",
                            d.path, d.status, d.additions, d.deletions
                        );
                    }
                    s
                }
                None => {
                    let done = if i.done { " done" } else { "" };
                    let queued = if i.queued { " queued" } else { "" };
                    format!("{:?}{done}{queued} {:?}", i.kind, first_chars(&i.text, 40))
                }
            },
            Event::Op(Op::Append { field, .. }) => format!("append {field:?}"),
            Event::Op(Op::Turn(t)) => format!(
                "turn {} cost={:?} usage={:?} error={:?}",
                t.status,
                t.cost_usd.map(|c| (c * 1e8).round() / 1e8),
                t.usage
                    .as_ref()
                    .map(|u| (u.input, u.output, u.cache_read, u.reasoning)),
                t.error.as_deref().map(|e| first_chars(e, 40))
            ),
            Event::Op(Op::State(s)) => format!(
                "state {} mode={} model={}",
                s.status,
                s.permission_mode.as_deref().unwrap_or("-"),
                s.model.as_deref().unwrap_or("-")
            ),
            Event::Op(Op::Mcp(list)) => list
                .iter()
                .map(|m| format!("mcp {} {} {:?}", m.name, m.status, m.auth_flows))
                .collect::<Vec<_>>()
                .join("; "),
            Event::Permission(p) => format!("permission {} {:?}", p.id, p.title),
            Event::Withdrawn(p) => format!("withdrawn {p}"),
            Event::Write(f) => format!("write {f}"),
            Event::Prompt { command_id, state } => format!("prompt {command_id} {state:?}"),
            Event::Reply {
                subtype, result, ..
            } => format!("reply {subtype} {result:?}"),
            Event::AuthStarted { server, result, .. } => format!(
                "auth {server} {:?}",
                result
                    .as_ref()
                    .map(|a| a.callback.as_ref().map(|c| c.prefix.clone()))
            ),
            Event::Status(s) => format!("status {s}"),
            Event::Title(t) => format!("title {t}"),
        }
    }

    /// Summaries with runs of appends folded into one line.
    fn summaries(events: &[Event]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for e in events {
            let s = summary(e);
            if s.starts_with("append") && out.last() == Some(&s) {
                continue;
            }
            out.push(s);
        }
        out
    }

    fn permissions(events: &[Event]) -> Vec<&PermissionRequest> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Permission(p) => Some(p.as_ref()),
                _ => None,
            })
            .collect()
    }

    fn ended_turns(events: &[Event]) -> Vec<&TurnState> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Op(Op::Turn(t)) if t.ended_at.is_some() => Some(t),
                _ => None,
            })
            .collect()
    }

    fn last_tool<'a>(events: &'a [Event], id: &str) -> &'a ToolCall {
        events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Op(Op::Item(i)) if i.id == id => i.tool.as_ref(),
                _ => None,
            })
            .expect("tool item")
    }

    fn decision<'a>(id: &'a str, decision: &'a str, option: Option<&'a str>) -> Decision<'a> {
        Decision {
            permission_id: id,
            decision,
            option_id: option,
            message: None,
            answers: &[],
        }
    }

    /// `decide` that records what it would write.
    fn decide(d: &mut Driver, dec: &Decision) -> Result<String, &'static str> {
        let mut written = None;
        d.decide(dec, |f| {
            written = Some(f);
            Ok(())
        })?;
        Ok(written.expect("one frame"))
    }

    /// Decodes one `can_use_tool` frame and returns its permission.
    fn ask(d: &mut Driver, frame: &str) -> PermissionRequest {
        let events = d.on_line(frame, 1_000);
        permissions(&events)
            .first()
            .map(|p| (*p).clone())
            .expect("a permission")
    }

    fn opt<'a>(p: &'a PermissionRequest, id: &str) -> &'a PermissionOption {
        p.options.iter().find(|o| o.id == id).expect("option")
    }

    /* ------------------------- transcript ------------------------- */

    #[test]
    fn capture_run1_produces_expected_ops() {
        // run 1: initialize, mcp_status, an auto-approved Bash, a Write
        // approved inline, an interrupt mid-stream, a follow-up turn and an
        // mcp_authenticate that yields a localhost callback
        let r = replay(fixture!("capture-1.ndjson"), RUN1, |p| {
            (p.tool_item_id.as_deref() == Some("toolu_019VpuxowumMgKBRTvDc2fq8"))
                .then(|| pick("approve-once", "allow"))
        });
        let want = [
            "state idle mode=default model=-",
            "status idle",
            "reply initialize Ok(())",
            "User done \"Use the Bash tool to run exactly: echo h\"",
            "mcp sentry-remote pending []",
            // turn 1: system/init opens it; Bash is auto-approved, Write asks
            "turn running cost=None usage=None error=None",
            "state running mode=default model=-",
            "status running",
            "state running mode=default model=claude-haiku-5-5",
            "mcp sentry-remote needs-auth [\"desktop\", \"phone-callback\"]",
            "prompt cmd-1 Started",
            "Notice done \"Claude usage is at 79% of the 7-day limi\"",
            "tool toolu_01L7vYbuwodCNuW8ywvyYMuv Bash Pending",
            "append Input",
            "tool toolu_01L7vYbuwodCNuW8ywvyYMuv Bash Running title=\"echo hi\"",
            "tool toolu_019VpuxowumMgKBRTvDc2fq8 Write Pending",
            "append Input",
            "tool toolu_019VpuxowumMgKBRTvDc2fq8 Write Running title=\"note.txt\"",
            "tool toolu_01L7vYbuwodCNuW8ywvyYMuv Bash Completed title=\"echo hi\" auto=config out=\"hi\"",
            "tool toolu_019VpuxowumMgKBRTvDc2fq8 Write AwaitingApproval title=\"note.txt\" perm=claude-1:74f9687e-65cf-4bc2-9b4d-721d032e2845",
            "state waiting mode=default model=claude-haiku-5-5",
            "status waiting",
            "permission claude-1:74f9687e-65cf-4bc2-9b4d-721d032e2845 \"Claude wants to use Write\"",
            "tool toolu_019VpuxowumMgKBRTvDc2fq8 Write Running title=\"note.txt\" perm=claude-1:74f9687e-65cf-4bc2-9b4d-721d032e2845",
            "state running mode=default model=claude-haiku-5-5",
            "status running",
            "tool toolu_019VpuxowumMgKBRTvDc2fq8 Write Completed title=\"note.txt\" perm=claude-1:74f9687e-65cf-4bc2-9b4d-721d032e2845 out=\"File created successfully at: <tmp>\\\\run\\\\note.txt (file state is current in your context — no need to Read it back)\" diff=note.txt added +1 -0",
            "Text \"\"",
            "append Text",
            "Text done \"done\"",
            "turn completed cost=Some(0.00886069) usage=Some((81691, 215, Some(40609), None)) error=None",
            "prompt cmd-1 Done { status: \"completed\", error: None }",
            "state idle mode=default model=claude-haiku-5-5",
            "status idle",
            // turn 2: interrupted while text streams
            "User done \"Count from 1 to 40, one number per line,\"",
            "turn running cost=None usage=None error=None",
            "state running mode=default model=claude-haiku-5-5",
            "status running",
            "mcp sentry-remote needs-auth [\"desktop\", \"phone-callback\"]",
            "prompt cmd-2 Started",
            "auth sentry-remote Ok(Some(\"http://localhost:44818/callback\"))",
            "Text \"\"",
            "append Text",
            "reply interrupt Ok(())",
            "Text done \"1\\n2\\n3\\n4\\n5\\n6\\n7\\n8\\n9\\n10\\n11\\n12\\n13\\n14\\n15\\n16\\n1\"",
            "Notice done \"Stopped\"",
            "turn interrupted cost=Some(0.0) usage=Some((41130, 8, Some(41078), None)) error=None",
            "prompt cmd-2 Done { status: \"cancelled\", error: None }",
            "state idle mode=default model=claude-haiku-5-5",
            "status idle",
            // turn 3: a follow-up in the same process
            "User done \"Use the Bash tool to run: echo second\"",
            "turn running cost=None usage=None error=None",
            "state running mode=default model=claude-haiku-5-5",
            "status running",
            "mcp sentry-remote needs-auth [\"desktop\", \"phone-callback\"]",
            "prompt cmd-3 Started",
            "tool toolu_01WVr6cqPJx9TW2rEFAS8xtc Bash Pending",
            "append Input",
            "tool toolu_01WVr6cqPJx9TW2rEFAS8xtc Bash Running title=\"echo second\"",
            "tool toolu_01WVr6cqPJx9TW2rEFAS8xtc Bash Completed title=\"echo second\" auto=config out=\"second\"",
            "Text \"\"",
            "append Text",
            "Text done \"The command printed `second`. I stopped \"",
            "turn completed cost=Some(0.00094327) usage=Some((82673, 110, Some(82347), None)) error=None",
            "prompt cmd-3 Done { status: \"completed\", error: None }",
            "state idle mode=default model=claude-haiku-5-5",
            "status idle",
            "mcp sentry-remote needs-auth [\"desktop\", \"phone-callback\"]",
        ];
        assert_eq!(summaries(&r.events), want);
        // item ids: `<message.id>#<block index>`, tools by tool_use.id
        let ids: Vec<&str> = r
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Op(Op::Item(i)) if i.kind == ItemKind::Text => Some(i.id.as_str()),
                _ => None,
            })
            .collect();
        assert!(ids.contains(&"msg_011CgPXrUK7C4jN5nyZsRPpc#0"), "{ids:?}");
        let appended = r.events.iter().any(|e| {
            matches!(e, Event::Op(Op::Append { id, field: Field::Input, .. })
                if id == "toolu_019VpuxowumMgKBRTvDc2fq8")
        });
        assert!(
            appended,
            "streaming tool input is appended to the tool item"
        );
        // the Write card's final input and its all-added diff
        let write = last_tool(&r.events, "toolu_019VpuxowumMgKBRTvDc2fq8");
        assert_eq!(
            write.input,
            "{\n  \"file_path\": \"<tmp>\\\\run\\\\note.txt\",\n  \"content\": \"x\"\n}"
        );
        assert_eq!(write.diffs[0].patch, "@@ -0,0 +1 @@\n+x");
        assert_eq!(r.d.malformed(), 0);
    }

    #[test]
    fn prompt_uuid_maps_lifecycle_to_the_command() {
        let mut d = Driver::new("claude-1", "ses-1", RUN1);
        let mut frame = String::new();
        let op = d
            .user_prompt("cmd-7", "and then say ok", &[], 5, |f| {
                frame = f;
                Ok(())
            })
            .unwrap();
        let uuid = json::get_str(&json::parse(&frame).unwrap(), "uuid")
            .unwrap()
            .to_string();
        assert!(is_uuid(&uuid) && uuid.as_bytes()[14] == b'4', "v4: {uuid}");
        let want = format!(
            "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"and then say ok\"}}]}},\"parent_tool_use_id\":null,\"session_id\":\"\",\"uuid\":\"{uuid}\"}}"
        );
        assert_eq!(frame, want);
        assert!(matches!(&op, Op::Item(i) if i.id == uuid && i.kind == ItemKind::User));
        let life = |state: &str| {
            format!(
                "{{\"type\":\"command_lifecycle\",\"command_uuid\":\"{uuid}\",\"state\":\"{state}\"}}"
            )
        };
        let mut got = summaries(&d.on_line(&life("queued"), 6));
        got.extend(summaries(&d.on_line(&life("started"), 7)));
        got.extend(summaries(&d.on_line(&life("started"), 8))); // once only
        let result = r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.01,"queued_turn_count":0}"#;
        got.extend(summaries(&d.on_line(result, 9)));
        got.extend(summaries(&d.on_line(&life("completed"), 10))); // after result: nothing left
        assert_eq!(
            got,
            [
                "User done queued \"and then say ok\"",
                "prompt cmd-7 Queued",
                "User done \"and then say ok\"",
                "prompt cmd-7 Started",
                "turn completed cost=Some(0.01) usage=Some((0, 0, None, None)) error=None",
                "prompt cmd-7 Done { status: \"completed\", error: None }",
                "state idle mode=- model=-",
                "status idle",
            ]
        );
        // a prompt the CLI cancels closes at once
        let mut prompt = |d: &mut Driver, command: &str| {
            d.user_prompt(command, "x", &[], 11, |f| {
                frame = f;
                Ok(())
            })
            .unwrap();
            json::get_str(&json::parse(&frame).unwrap(), "uuid")
                .unwrap()
                .to_string()
        };
        let lifecycle = |uuid: &str, state: &str| {
            format!(
                "{{\"type\":\"command_lifecycle\",\"command_uuid\":\"{uuid}\",\"state\":\"{state}\"}}"
            )
        };
        let uuid = prompt(&mut d, "cmd-8");
        assert_eq!(
            summaries(&d.on_line(&lifecycle(&uuid, "cancelled"), 12)),
            ["prompt cmd-8 Done { status: \"cancelled\", error: None }"]
        );
        // a prompt written as the turn ends is not closed by that turn's
        // result: it ends by its own uuid (here a late `completed`)
        let (first, late) = (prompt(&mut d, "cmd-10"), prompt(&mut d, "cmd-11"));
        d.on_line(&lifecycle(&first, "started"), 12);
        let ended = summaries(&d.on_line(result, 13));
        assert!(
            ended.iter().any(|s| s.starts_with("prompt cmd-10 Done")),
            "{ended:?}"
        );
        assert!(!ended.iter().any(|s| s.contains("cmd-11")), "{ended:?}");
        assert_eq!(
            summaries(&d.on_line(&lifecycle(&late, "completed"), 14)),
            [
                "prompt cmd-11 Started",
                "prompt cmd-11 Done { status: \"completed\", error: None }"
            ]
        );
        // a failed write records nothing
        let refused = d.user_prompt("cmd-9", "y", &[], 13, |_| {
            Err("runtime is not reading input")
        });
        assert_eq!(refused.err(), Some("runtime is not reading input"));
        assert!(
            summaries(&d.on_exit(Some(0), 14))
                .iter()
                .all(|s| !s.contains("cmd-9"))
        );
    }

    /* ------------------------- permissions ------------------------ */

    fn can_use_tool(id: &str, tool: &str, input: &str, extra: &str) -> String {
        format!(
            "{{\"type\":\"control_request\",\"request_id\":\"{id}\",\"request\":{{\"subtype\":\"can_use_tool\",\"tool_name\":\"{tool}\",\"display_name\":\"{tool}\",\"input\":{input}{extra},\"tool_use_id\":\"toolu_{id}\"}}}}"
        )
    }

    #[test]
    fn can_use_tool_becomes_permission_with_filtered_options() {
        let frame = find(fixture!("sdk-run2.ndjson"), "out", |v| {
            tool_named(v, "Bash")
        });
        let ws = "<tmp>\\work2";
        let mut d = Driver::new("claude-1", "ses-1", ws);
        let p = ask(&mut d, frame);
        assert_eq!(p.id, "claude-1:7fa94ef9-93ff-4469-8e00-4fc6167401d4");
        assert_eq!(p.native_request_id, "7fa94ef9-93ff-4469-8e00-4fc6167401d4");
        assert_eq!(
            p.tool_item_id.as_deref(),
            Some("toolu_01ChGDoeLpapysjVoBqVMKhX")
        );
        assert_eq!(
            (p.session_id.as_str(), p.agent_id.as_str()),
            ("ses-1", "claude-1")
        );
        assert_eq!((p.action.as_str(), p.risk.as_str()), ("shell", "high"));
        assert_eq!(p.title, "Claude wants to use Bash");
        assert_eq!(p.command, "mkdir sub");
        assert_eq!(p.context, "Create sub directory");
        assert_eq!(p.directory.as_deref(), Some(ws));
        assert_eq!((p.status.as_str(), p.revision), ("pending", 1));
        assert_eq!(p.expires_at - p.requested_at, LIVE_PERMISSION_TTL_MS);
        assert_eq!(
            json::stringify(p.detail.as_ref().unwrap()),
            r#"{"kind":"shell","command":"mkdir sub","cwd":"<tmp>\\work2","reason":"Create sub directory"}"#
        );
        let options: Vec<_> = p
            .options
            .iter()
            .map(|o| {
                (
                    o.id.as_str(),
                    o.decision.as_str(),
                    o.scope,
                    o.label.as_str(),
                    o.rule.as_deref(),
                    o.destination.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            options,
            [
                ("allow", "approve-once", "once", "Allow", None, None),
                (
                    "suggestion-0",
                    "approve-session",
                    "persistent",
                    "Allow `mkdir sub *` for this project (just you)",
                    Some("Bash(mkdir sub *)"),
                    Some(".claude/settings.local.json"),
                ),
                (
                    "suggestion-1",
                    "approve-session",
                    "session",
                    "Allow access to `<tmp>\\work2` for this session",
                    Some("<tmp>\\work2"),
                    None,
                ),
                (
                    "suggestion-2",
                    "approve-session",
                    "session",
                    "Allow all edits this session",
                    None,
                    None,
                ),
                ("deny", "reject", "once", "Deny", None, None),
            ]
        );
        assert!(opt(&p, "deny").takes_message);
        assert_eq!(
            p.supported_decisions,
            ["approve-once", "approve-session", "reject"]
        );
        let wire = json::stringify(&p.to_json());
        assert!(
            wire.contains("\"toolItemId\":\"toolu_01ChGDoeLpapysjVoBqVMKhX\""),
            "{wire}"
        );
        assert!(d.knows_permission(&p.id) && !d.knows_permission("claude-2:7fa94ef9"));
        // default_to_no makes the reject option options[0], the primary Deny
        let strict = frame.replace(
            "\"tool_use_id\":",
            "\"default_to_no\":true,\"tool_use_id\":",
        );
        let p = ask(&mut Driver::new("claude-1", "ses-1", ws), &strict);
        let ids: Vec<&str> = p.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "deny",
                "allow",
                "suggestion-0",
                "suggestion-1",
                "suggestion-2"
            ]
        );
    }

    #[test]
    fn permission_detail_and_legacy_fields_by_tool() {
        let ws = "C:\\w";
        let mut d = Driver::new("claude-1", "ses-1", ws);
        let edit = r#"{"file_path":"C:\\w\\a.txt","old_string":"a\nb\nc","new_string":"a\nB\nc"}"#;
        let mcp = r#"{"query":"crash"}"#;
        let plan = r#"{"plan":"1. Do it"}"#;
        let cases = [
            (
                "Edit",
                edit,
                "file-write",
                "medium",
                "C:\\w\\a.txt",
                r#"{"kind":"edit","path":"C:\\w\\a.txt","patch":"@@ -1,3 +1,3 @@\n a\n-b\n+B\n c"}"#,
            ),
            (
                "Write",
                r#"{"file_path":"C:\\w\\b.txt","content":"one\ntwo\n"}"#,
                "file-write",
                "medium",
                "C:\\w\\b.txt",
                r#"{"kind":"edit","path":"C:\\w\\b.txt","patch":"@@ -0,0 +1,2 @@\n+one\n+two"}"#,
            ),
            (
                "Read",
                r#"{"file_path":"C:\\w\\c.txt"}"#,
                "agent-control",
                "low",
                "C:\\w\\c.txt",
                r#"{"kind":"read","path":"C:\\w\\c.txt"}"#,
            ),
            (
                "WebFetch",
                r#"{"url":"https://example.com","prompt":"p"}"#,
                "network",
                "medium",
                "https://example.com",
                r#"{"kind":"fetch","url":"https://example.com"}"#,
            ),
            (
                "WebSearch",
                r#"{"query":"rust std"}"#,
                "network",
                "medium",
                "rust std",
                r#"{"kind":"fetch","url":"rust std"}"#,
            ),
            (
                "mcp__plugin_sentry_sentry__find_issues",
                mcp,
                "agent-control",
                "medium",
                "mcp__plugin_sentry_sentry__find_issues",
                r#"{"kind":"mcp","server":"plugin_sentry_sentry","tool":"find_issues","input":"{\n  \"query\": \"crash\"\n}"}"#,
            ),
            (
                "ExitPlanMode",
                plan,
                "agent-control",
                "low",
                "ExitPlanMode",
                r#"{"kind":"plan","plan":"1. Do it"}"#,
            ),
            (
                "Glob",
                r#"{"pattern":"*.rs"}"#,
                "agent-control",
                "low",
                "Glob",
                r#"{"kind":"generic","tool":"Glob","input":"{\n  \"pattern\": \"*.rs\"\n}"}"#,
            ),
        ];
        for (i, (tool, input, action, risk, command, detail)) in cases.into_iter().enumerate() {
            let p = ask(&mut d, &can_use_tool(&format!("r{i}"), tool, input, ""));
            assert_eq!(
                (p.action.as_str(), p.risk.as_str()),
                (action, risk),
                "{tool}"
            );
            assert_eq!(p.command, command, "{tool}");
            assert_eq!(
                json::stringify(p.detail.as_ref().unwrap()),
                detail,
                "{tool}"
            );
        }
        // the CLI's own title wins; secrets and ANSI never reach the phone
        let key = format!("sk-ant-api03-{}", "aB3dE5fG7h".repeat(4));
        let extra = format!(
            ",\"title\":\"Claude wants to run a script\",\"description\":\"\\u001b[31mexport TOKEN={key}\\u001b[0m\""
        );
        let input = format!("{{\"command\":\"curl -H 'x-api-key: {key}' x\"}}");
        let p = ask(&mut d, &can_use_tool("s1", "Bash", &input, &extra));
        assert_eq!(p.title, "Claude wants to run a script");
        assert_eq!(p.context, "export TOKEN=«redacted:secret»");
        let wire = json::stringify(&p.to_json());
        assert!(!wire.contains(&key), "{wire}");
        // ExitPlanMode speaks plan: approve, auto-accept edits, keep planning
        let offer = r#","permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}]"#;
        let p = ask(&mut d, &can_use_tool("plan2", "ExitPlanMode", plan, offer));
        let labels: Vec<&str> = p.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Approve plan",
                "Approve and auto-accept edits",
                "Keep planning"
            ]
        );
        // a malformed request is refused, never left unanswered
        let bad = r#"{"type":"control_request","request_id":"m1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":"x"}}"#;
        assert_eq!(
            d.on_line(bad, 0),
            [Event::Write(error_response(
                "m1",
                "malformed permission request"
            ))]
        );
    }

    #[test]
    fn allow_once_writes_byte_exact_control_response() {
        let cap = fixture!("capture-1.ndjson");
        let frame = find(cap, "out", |v| tool_named(v, "Write"));
        let want = find(cap, "in", |v| {
            json::get_str(v, "type") == Some("control_response")
        });
        let mut d = Driver::new("claude-1", "ses-1", RUN1);
        let p = ask(&mut d, frame);
        assert_eq!(
            decide(&mut d, &decision(&p.id, "approve-once", Some("allow"))),
            Ok(want.into())
        );
        // a legacy decision (no optionId) selects the same option
        let p = ask(&mut d, frame);
        assert_eq!(
            decide(&mut d, &decision(&p.id, "approve-once", None)),
            Ok(want.into())
        );
        // the input goes back as the CLI wrote it, even where a re-serializer
        // would change the bytes (escapes, exponents, negative zero)
        let input = r#"{"command":"echo \u00e9","timeout":1.0e3,"n":-0.0}"#;
        let p = ask(&mut d, &can_use_tool("b1", "Bash", input, ""));
        assert_eq!(
            decide(&mut d, &decision(&p.id, "approve-once", Some("allow"))),
            Ok(format!(
                r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"b1","response":{{"behavior":"allow","updatedInput":{input}}}}}}}"#
            ))
        );
    }

    #[test]
    fn suggestion_filter_drops_bypass_and_unoffered_rules() {
        let offered = r#","permission_suggestions":[{"type":"setMode","mode":"dontAsk","destination":"session"},{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"npm test:*"}],"behavior":"allow","destination":"projectSettings"},{"type":"replaceRules","rules":[{"toolName":"Bash"}],"behavior":"allow","destination":"userSettings"},{"type":"addRules","rules":[{"toolName":"Read"}],"behavior":"deny","destination":"session"},{ "type" : "setMode", "mode":"acceptEdits", "destination":"userSettings" }]"#;
        let frame = can_use_tool("f1", "Bash", r#"{"command":"npm test"}"#, offered);
        let mut d = Driver::new("claude-1", "ses-1", RUN1);
        let p = ask(&mut d, &frame);
        let options: Vec<_> = p
            .options
            .iter()
            .map(|o| {
                (
                    o.id.as_str(),
                    o.scope,
                    o.label.as_str(),
                    o.destination.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            options,
            [
                ("allow", "once", "Allow", None),
                (
                    "suggestion-1",
                    "persistent",
                    "Allow `npm test:*` for this project (shared)",
                    Some(".claude/settings.json"),
                ),
                (
                    "suggestion-4",
                    "persistent",
                    "Allow and switch to accept-edits mode for all projects",
                    Some("~/.claude/settings.json"),
                ),
                ("deny", "once", "Deny", None),
            ]
        );
        // legacy decisions never select a persistent rule
        assert_eq!(p.supported_decisions, ["approve-once", "reject"]);
        let mut wrote = false;
        let mut refuse = |option: &str, decision: &str| {
            let res = d.decide(
                &Decision {
                    permission_id: "claude-1:f1",
                    decision,
                    option_id: (!option.is_empty()).then_some(option),
                    message: None,
                    answers: &[],
                },
                |_| {
                    wrote = true;
                    Ok(())
                },
            );
            assert_eq!(
                res.err(),
                Some("decision is not supported by this runtime"),
                "{option}"
            );
        };
        refuse("suggestion-0", "approve-session"); // dontAsk (like bypassPermissions/auto): never offered
        refuse("suggestion-2", "approve-session"); // not a type the phone may grant
        refuse("suggestion-9", "approve-session"); // never offered at all
        refuse("suggestion-1", "approve-once"); // the option's bucket is fixed
        refuse("", "approve-session"); // no legacy-selectable session option
        assert!(!wrote, "nothing reaches stdin");
        // the chosen suggestion is echoed byte for byte, spacing and all
        let echoed = decide(
            &mut d,
            &decision("claude-1:f1", "approve-session", Some("suggestion-4")),
        )
        .unwrap();
        assert!(
            echoed.ends_with(r#""updatedPermissions":[{ "type" : "setMode", "mode":"acceptEdits", "destination":"userSettings" }]}}}"#),
            "{echoed}"
        );
        // suppress_always_allow_rule: only allow once and deny
        let quiet = frame.replace(
            "\"tool_use_id\":",
            "\"suppress_always_allow_rule\":true,\"tool_use_id\":",
        );
        let p = ask(&mut Driver::new("claude-1", "ses-1", RUN1), &quiet);
        let ids: Vec<&str> = p.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["allow", "deny"]);
        // nor is bypass reachable as a mode
        let refused = d.set_permission_mode("cmd-1", "bypassPermissions", |_| panic!("written"));
        assert!(refused.is_err());
        // run 2: Edit approved with the offered setMode, AskUserQuestion
        // answered; both answers equal the research driver's live frames
        let cap = fixture!("capture-2.ndjson");
        let r = replay(cap, "<tmp>\\run2", |p| match p.title.as_str() {
            "Claude wants to use Edit" => Some(pick("approve-session", "suggestion-0")),
            _ => Some(Answer {
                answers: vec![("q0".into(), vec!["red".into()])],
                ..pick("approve-once", "allow")
            }),
        });
        let answers: Vec<&str> = frames(cap)
            .into_iter()
            .filter(|(dir, _, m)| dir == "in" && m.starts_with("{\"type\":\"control_response\""))
            .map(|(_, _, m)| m)
            .collect();
        assert_eq!(answers.len(), 2);
        for want in answers {
            assert!(r.stdin.iter().any(|f| f == want), "missing {want}");
        }
        let edit = last_tool(&r.events, "toolu_01E7AzpNYui5YtxM5vDsv2SM");
        assert_eq!(edit.status, ToolStatus::Completed);
        assert_eq!(edit.diffs[0].patch, "@@ -1,2 +1,2 @@\n one\n-two\n+three");
        assert_eq!((edit.diffs[0].additions, edit.diffs[0].deletions), (1, 1));
        assert_eq!(edit.diffs[0].path, "a.txt");
        let got = summaries(&r.events);
        assert!(got.contains(&"state running mode=acceptEdits model=claude-haiku-5-5".to_string()));
        assert!(got.contains(&"reply set_permission_mode Ok(())".to_string()));
        assert!(
            got.iter().any(|s| s.starts_with("Thinking done \"")),
            "summarized thinking"
        );
    }

    #[test]
    fn deny_with_message_reaches_stdin() {
        let cap = fixture!("capture-3.ndjson");
        let r = replay(cap, "<tmp>\\run3", |_| {
            Some(Answer {
                message: Some("Denied from phone: not now"),
                ..pick("reject", "deny")
            })
        });
        let want = find(cap, "in", |v| {
            json::get_str(v, "type") == Some("control_response")
        });
        assert!(r.stdin.iter().any(|f| f == want), "{:#?}", r.stdin);
        let tool = last_tool(&r.events, "toolu_01DFzqjTXrsfuaqM8pAbvk2A");
        assert_eq!(tool.status, ToolStatus::Denied);
        assert_eq!(tool.error.as_deref(), Some("Denied from phone: not now"));
        assert_eq!(ended_turns(&r.events)[0].status, "completed");
        // no message: the model reads the default
        let frame = find(cap, "out", |v| tool_named(v, "Bash"));
        let mut d = Driver::new("claude-1", "ses-1", "<tmp>\\run3");
        let p = ask(&mut d, frame);
        let dec = Decision {
            message: Some("   "),
            ..decision(&p.id, "reject", Some("deny"))
        };
        assert_eq!(
            decide(&mut d, &dec),
            Ok(success_response(
                &p.native_request_id,
                r#"{"behavior":"deny","message":"Denied from phone"}"#
            ))
        );
    }

    #[test]
    fn cancel_request_withdraws_and_late_decision_fails() {
        let cap = fixture!("sdk-run5-cancel.ndjson");
        let asked = find(cap, "out", |v| tool_named(v, "Bash"));
        let cancel = find(cap, "out", |v| {
            json::get_str(v, "type") == Some("control_cancel_request")
        });
        let ws = "<tmp>\\work4";
        // withdrawn first: the late decision fails and nothing is written
        let mut d = Driver::new("claude-1", "ses-1", ws);
        let p = ask(&mut d, asked);
        assert!(d.knows_permission(&p.id));
        assert!(
            d.on_line(cancel, 2)
                .contains(&Event::Withdrawn(p.id.clone()))
        );
        assert!(!d.knows_permission(&p.id));
        let mut wrote = false;
        let late = d.decide(&decision(&p.id, "approve-once", Some("allow")), |_| {
            wrote = true;
            Ok(())
        });
        assert_eq!(late.err(), Some("request is no longer pending"));
        assert!(!wrote);
        // answered first: the cancel is a no-op and a second answer fails
        let mut d = Driver::new("claude-1", "ses-1", ws);
        let p = ask(&mut d, asked);
        assert!(decide(&mut d, &decision(&p.id, "reject", Some("deny"))).is_ok());
        assert!(d.on_line(cancel, 3).is_empty());
        let again = decide(&mut d, &decision(&p.id, "reject", Some("deny")));
        assert_eq!(again.err(), Some("request is no longer pending"));
        // a write the child refuses keeps the request pending for a retry
        let mut d = Driver::new("claude-1", "ses-1", ws);
        let p = ask(&mut d, asked);
        let stuck = d.decide(&decision(&p.id, "reject", Some("deny")), |_| {
            Err("runtime is not reading input")
        });
        assert_eq!(stuck.err(), Some("runtime is not reading input"));
        assert!(d.knows_permission(&p.id));
        // process exit stales whatever is still pending
        assert!(
            d.on_exit(Some(1), 4)
                .contains(&Event::Withdrawn(p.id.clone()))
        );
        // the replayed run: the card ends cancelled and the turn interrupted
        let r = replay(cap, ws, |_| None);
        let tool = last_tool(&r.events, "toolu_01Rmrkch8RpHsaCt1HpDN92F");
        assert_eq!(tool.status, ToolStatus::Cancelled);
        let ends: Vec<&str> = ended_turns(&r.events).iter().map(|t| t.status).collect();
        assert_eq!(ends, ["interrupted", "completed"]);
        let got = summaries(&r.events);
        for s in [
            "status waiting",
            "withdrawn claude-1:7f10d860-cf55-4a3f-93ab-5ac4cbbb14ec",
            "reply interrupt Ok(())",
        ] {
            assert!(got.contains(&s.to_string()), "{s}: {got:#?}");
        }
    }

    #[test]
    fn ask_user_question_maps_ids_to_question_text() {
        let cap = fixture!("capture-2.ndjson");
        let frame = find(cap, "out", |v| tool_named(v, "AskUserQuestion"));
        let want = find(cap, "in", |v| {
            let body = json::get(v, "response").and_then(|r| json::get(r, "response"));
            body.and_then(|b| json::get(b, "updatedInput"))
                .and_then(|i| json::get(i, "answers"))
                .is_some()
        });
        let mut d = Driver::new("claude-1", "ses-1", "<tmp>\\run2");
        let p = ask(&mut d, frame);
        assert_eq!(
            json::stringify(p.detail.as_ref().unwrap()),
            r#"{"kind":"question","questions":[{"id":"q0","text":"Pick a color","header":"Color","options":[{"label":"red","description":"Choose red"},{"label":"blue","description":"Choose blue"}],"multiSelect":false,"allowFreeText":true}]}"#
        );
        let labels: Vec<&str> = p.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["Submit answers", "Decline"]);
        let with = |answers: &[(String, Vec<String>)], d: &mut Driver| {
            let dec = Decision {
                answers,
                ..decision(&p.id, "approve-once", Some("allow"))
            };
            decide(d, &dec)
        };
        let unknown = [("q9".to_string(), vec!["red".to_string()])];
        assert_eq!(
            with(&unknown, &mut d).err(),
            Some("the answers name a question that was not asked")
        );
        assert_eq!(
            with(&[], &mut d).err(),
            Some("answer at least one question")
        );
        let red = [("q0".to_string(), vec!["red".to_string()])];
        assert_eq!(with(&red, &mut d), Ok(want.to_string()));
        // several picks join like the extension's question card
        let mut d = Driver::new("claude-1", "ses-1", "<tmp>\\run2");
        ask(&mut d, frame);
        let both = [(
            "q0".to_string(),
            vec!["red".to_string(), "Teal, please".to_string()],
        )];
        let frame = with(&both, &mut d).unwrap();
        assert!(
            frame.contains(r#""answers":{"Pick a color":"red, Teal, please"}"#),
            "{frame}"
        );
    }

    #[test]
    fn initialize_account_is_never_forwarded() {
        let cap = fixture!("capture-1.ndjson");
        let init = find(cap, "out", |v| {
            let body = json::get(v, "response").and_then(|r| json::get(r, "response"));
            body.and_then(|b| json::get_str(b, "claude_code_version"))
                .is_some()
        });
        // the fixture's account block was removed when it was sanitized: put a
        // synthetic one back, plus a request that was open before (re-ingested)
        let account = r#""account":{"email":"someone@example.invalid","organization":"Example Org","subscriptionType":"Claude Team","apiProvider":"firstParty"},"#;
        let open = can_use_tool("p-1", "Read", r#"{"file_path":"<tmp>\\run\\x.txt"}"#, "");
        let line = init
            .replace("\"request_id\":\"req_1\"", "\"request_id\":\"chb_1\"")
            .replacen(
                "\"pid\":",
                &format!("{account}\"pending_permission_requests\":[{open}],\"pid\":"),
                1,
            );
        assert!(line.contains("someone@example.invalid"));
        let mut d = Driver::new("claude-1", "ses-1", RUN1);
        d.initialize(|_| Ok(())).unwrap();
        let events = d.on_line(&line, 10);
        let all = format!("{events:?}");
        for leak in [
            "someone@example.invalid",
            "Example Org",
            "Claude Team",
            "firstParty",
        ] {
            assert!(!all.contains(leak), "{leak} leaked: {all}");
        }
        assert_eq!(
            summaries(&events),
            [
                "state idle mode=default model=-",
                "status idle",
                "state waiting mode=default model=-",
                "status waiting",
                "permission claude-1:p-1 \"Claude wants to use Read\"",
                "reply initialize Ok(())",
            ]
        );
    }

    #[test]
    fn cumulative_cost_becomes_per_turn_delta() {
        let r = replay(fixture!("capture-1.ndjson"), RUN1, |_| {
            Some(pick("approve-once", "allow"))
        });
        let costs: Vec<f64> = ended_turns(&r.events)
            .iter()
            .map(|t| t.cost_usd.unwrap())
            .collect();
        // totals 0.00886069, 0.00886069 (the interrupt), 0.00980396
        assert_eq!(costs.len(), 3);
        assert!((costs[0] - 0.008_860_69).abs() < 1e-12, "{costs:?}");
        assert_eq!(costs[1], 0.0);
        assert!((costs[2] - 0.000_943_27).abs() < 1e-12, "{costs:?}");
        // a /clear restarts the running total: then the total is the turn's cost
        let mut d = Driver::new("claude-1", "ses-1", RUN1);
        for (total, want) in [(0.5, 0.5), (0.75, 0.25), (0.125, 0.125)] {
            let line = format!(
                r#"{{"type":"result","subtype":"success","is_error":false,"total_cost_usd":{total}}}"#
            );
            let cost = ended_turns(&d.on_line(&line, 0))[0].cost_usd;
            assert_eq!(cost, Some(want), "{total}");
        }
        // other errors fail the turn with the CLI's own words
        let failed = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"errors":["Reached maximum number of turns (6)"]}"#;
        let ended = d.on_line(failed, 0);
        let turn = ended_turns(&ended)[0];
        assert_eq!(
            (turn.status, turn.error.as_deref()),
            ("failed", Some("Reached maximum number of turns (6)"))
        );
    }

    /* -------------------------- robustness ------------------------ */

    #[test]
    fn unknown_control_request_gets_error_response() {
        let hook = find(fixture!("sdk-run6-hooks.ndjson"), "out", |v| {
            subtype(v) == "hook_callback"
        });
        let mut d = Driver::new("claude-1", "ses-1", "<tmp>\\work5");
        assert_eq!(
            d.on_line(hook, 0),
            [Event::Write(
                r#"{"type":"control_response","response":{"subtype":"error","request_id":"e1ed17b6-c779-4d68-91ae-f799e03a661e","error":"unsupported by host"}}"#.into()
            )]
        );
        for sub in ["mcp_message", "request_user_dialog", "something_new"] {
            let frame = format!(
                r#"{{"type":"control_request","request_id":"r-{sub}","request":{{"subtype":"{sub}"}}}}"#
            );
            let id = format!("r-{sub}");
            assert_eq!(
                d.on_line(&frame, 0),
                [Event::Write(error_response(&id, UNSUPPORTED_BY_HOST))]
            );
        }
        // MCP elicitation forms are a later step: declined, and the user is told
        let elicit = r#"{"type":"control_request","request_id":"e-1","request":{"subtype":"elicitation","mcp_server_name":"github","message":"Pick a repo","mode":"form"}}"#;
        let events = d.on_line(elicit, 0);
        assert_eq!(
            events[0],
            Event::Write(
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"e-1","response":{"action":"decline"}}}"#.into()
            )
        );
        assert!(
            matches!(&events[1], Event::Op(Op::Item(i)) if i.kind == ItemKind::Notice && i.text.starts_with("github asked for input")),
            "{events:?}"
        );
    }

    #[test]
    fn unparseable_control_request_is_salvaged_with_error() {
        let mut d = Driver::new("claude-1", "ses-1", RUN1);
        let deep = format!(
            r#"{{"type":"control_request","request_id":"5f0c-2","request":{{"subtype":"can_use_tool","tool_name":"x","input":{{"a":{}1{}}}}}}}"#,
            "[".repeat(40),
            "]".repeat(40)
        );
        let cases = [
            // a lone surrogate: an emoji split across deltas
            r#"{"type":"control_request","request_id":"5f0c-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"echo \ud83d"}}}"#.to_string(),
            deep, // nested past depth 32
            r#"{"type":"control_request","request_id":"5f0c-3","request":{"subtype":"can_use_tool","tool_na"#.to_string(),
        ];
        for (i, line) in cases.iter().enumerate() {
            let events = d.on_line(line, 0);
            let id = format!("5f0c-{}", i + 1);
            assert_eq!(
                events[0],
                Event::Write(error_response(&id, "unparseable control request"))
            );
            assert!(matches!(&events[1], Event::Op(Op::Item(n)) if n.level == Some("warning")));
        }
        // anything else that does not parse is counted only (a lost delta
        // heals at the authoritative assistant frame), as is an id-less request
        let delta = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"\ud83d"}}}"#;
        assert!(d.on_line(delta, 0).is_empty());
        assert!(
            d.on_line(r#"{"type":"control_request","request":{"sub"#, 0)
                .is_empty()
        );
        assert_eq!(d.malformed(), 5);
    }

    #[test]
    fn oversize_line_is_skipped_with_notice() {
        use std::io::Write as _;
        let (out_r, mut out_w) = std::io::pipe().unwrap();
        let (_in_r, in_w) = std::io::pipe().unwrap();
        let (_child, rx) = ChildIo::from_pipes(out_r, in_w).unwrap();
        let writer = std::thread::spawn(move || {
            let pad = "x".repeat(crate::runtime_io::MAX_LINE_BYTES);
            let big = format!("{{\"type\":\"assistant\",\"pad\":\"{pad}\"}}\n");
            out_w.write_all(big.as_bytes()).unwrap();
            out_w
                .write_all(
                    b"{\"type\":\"system\",\"subtype\":\"status\",\"permissionMode\":\"plan\"}\n",
                )
                .unwrap();
        });
        let mut d = Driver::new("claude-1", "ses-1", RUN1);
        let mut events = Vec::new();
        loop {
            match rx
                .recv_timeout(Duration::from_secs(30))
                .expect("child event")
            {
                ChildEvent::Line(l) => events.extend(d.on_line(&l, 0)),
                ChildEvent::Skipped(n) => events.extend(d.on_skipped(n, 0)),
                ChildEvent::Exit(_) => break,
            }
        }
        writer.join().unwrap();
        assert_eq!(
            summaries(&events),
            [
                "Notice done \"Skipped a message from Claude Code large\"",
                "state starting mode=plan model=-",
            ]
        );
    }

    /* ------------------------ launch policy ------------------------ */

    /// A fresh directory under the system temp dir (removed by the caller).
    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chb-claude-live-{}", new_uuid().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn spawn_args_match_the_sdk_set_without_shell() {
        let id = "0b6c1f5e-8f2a-4c3d-9e7b-123456789abc";
        assert_eq!(
            spawn_args(id, false, None, None).unwrap(),
            [
                "-p",
                "--output-format",
                "stream-json",
                "--input-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--permission-prompt-tool",
                "stdio",
                "--setting-sources",
                "user,project,local",
                "--thinking-display",
                "summarized",
                "--session-id",
                id,
            ]
        );
        let resumed = spawn_args(id, true, Some("acceptEdits"), None).unwrap();
        assert_eq!(
            &resumed[13..],
            ["--permission-mode", "acceptEdits", "--resume", id]
        );
        for mode in ["bypassPermissions", "dontAsk", "auto", ""] {
            assert!(spawn_args(id, false, Some(mode), None).is_err(), "{mode}");
        }
        for bad in [
            "",
            "abc",
            "--resume",
            "0b6c1f5e-8f2a-4c3d-9e7b-123456789abc --x",
        ] {
            assert!(spawn_args(bad, false, None, None).is_err(), "{bad}");
        }
        let all = spawn_args(id, false, Some("plan"), None).unwrap().join(" ");
        for absent in [
            "--replay-user-messages",
            "bypass",
            "dangerously",
            "cmd",
            "/c",
        ] {
            assert!(!all.contains(absent), "{absent}");
        }
        // binary: the configured one, then the native installer, then PATH;
        // never a .cmd shim, never a relative PATH entry
        let root = scratch();
        let home = root.join("home");
        let installer = home.join(".local").join("bin");
        let (shim, real) = (root.join("shim"), root.join("real"));
        for dir in [&installer, &shim, &real] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(installer.join("claude.exe"), b"").unwrap();
        std::fs::write(shim.join("claude.cmd"), b"").unwrap();
        std::fs::write(real.join("claude.exe"), b"").unwrap();
        let path = std::env::join_paths([&shim, &real]).unwrap();
        let found = resolve_binary(None, Some(home.as_os_str()), Some(&path)).unwrap();
        assert_eq!(found, installer.join("claude.exe"));
        let nowhere = root.join("nobody");
        let found = resolve_binary(None, Some(nowhere.as_os_str()), Some(&path)).unwrap();
        assert_eq!(found, real.join("claude.exe"));
        assert!(resolve_binary(Some(&shim.join("claude.cmd")), None, None).is_err());
        assert!(resolve_binary(Some(Path::new("claude.exe")), None, None).is_err());
        let configured = real.join("claude.exe");
        assert_eq!(
            resolve_binary(Some(&configured), None, None),
            Ok(configured)
        );
        let relative = std::env::join_paths([".", "shim"]).unwrap();
        assert!(resolve_binary(None, None, Some(&relative)).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn child_env_drops_host_token_and_sets_entrypoint() {
        let base = [
            ("PATH", "C:\\bin"),
            ("HOST_TOKEN", "t"),
            ("CHERNOBYL_HOME", "h"),
            ("NODE_OPTIONS", "--require x"),
            ("CLAUDE_CODE_ENTRYPOINT", "claude-vscode"),
            ("ANTHROPIC_MODEL", "keep"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = spawn_env(base);
        let pairs: Vec<(&str, &str)> = env
            .iter()
            .map(|(k, v)| (k.to_str().unwrap(), v.to_str().unwrap()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("PATH", "C:\\bin"),
                ("ANTHROPIC_MODEL", "keep"),
                ("CLAUDE_CODE_ENTRYPOINT", "sdk-ts"),
                ("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1"),
            ]
        );
    }

    #[test]
    fn workspace_allow_list_refuses_drive_root_profile_and_data_dir() {
        let root = scratch();
        let (ws, profile, other) = (root.join("ws"), root.join("profile"), root.join("other"));
        let data = ws.join("data");
        for dir in [&data, &profile, &other] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(root.join("file.txt"), b"").unwrap();
        let check = |p: &Path| validate_workspace(&p.to_string_lossy(), &data, Some(&profile));
        let refused = |p: &Path, why: &str| {
            let err = check(p).unwrap_err();
            assert!(err.contains(why), "{}: {err}", p.display());
        };
        refused(&ws, "contains the CHERNOBYL data directory");
        refused(&data, "contains the CHERNOBYL data directory");
        // root holds both the data dir and the profile; either refusal will do
        assert!(check(&root).is_err());
        refused(&profile, "user profile folder");
        // a folder that contains the profile (the Users folder) is refused too
        let users = root.join("users");
        let me = users.join("me");
        std::fs::create_dir_all(&me).unwrap();
        let err = validate_workspace(&users.to_string_lossy(), &data, Some(&me)).unwrap_err();
        assert!(err.contains("user profile folder"), "{err}");
        assert!(validate_workspace(&me.to_string_lossy(), &data, Some(&users)).is_ok());
        let canon = std::fs::canonicalize(&root).unwrap();
        let drive = PathBuf::from(plain(&canon));
        refused(drive.ancestors().last().unwrap(), "drive root");
        refused(Path::new("relative\\dir"), "absolute");
        refused(&root.join("missing"), "existing directory");
        refused(&root.join("file.txt"), "existing directory");
        let ok = check(&other).unwrap();
        assert_eq!(ok.label, "other");
        assert_eq!(ok.path, plain(&std::fs::canonicalize(&other).unwrap()));
        assert!(!ok.path.starts_with(r"\\?\"), "{}", ok.path);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn workspace_ids_are_stable_hashes() {
        let root = scratch();
        let (ws, data, other) = (root.join("Proj"), root.join("data"), root.join("Other"));
        for dir in [&ws, &data, &other] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let canon = plain(&std::fs::canonicalize(&ws).unwrap());
        let id = |p: &str| validate_workspace(p, &data, None).unwrap().id;
        let first = id(&canon);
        assert_eq!(first, hex(&sha256(canon.as_bytes()))[..12]);
        assert!(
            first.len() == 12
                && first
                    .bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        for spelling in [
            format!("{canon}\\"),
            canon.replace('\\', "/"),
            canon.to_lowercase(),
            format!("{canon}\\..\\Proj"),
        ] {
            assert_eq!(id(&spelling), first, "{spelling}");
        }
        assert_ne!(id(&other.to_string_lossy()), first);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /* ------------------------------ MCP ---------------------------- */

    #[test]
    fn mcp_callback_checks_host_port_path_state() {
        // run 4 replayed through our encoders: needs-auth → mcp_authenticate
        // → mcp_oauth_callback_url → mcp_reconnect → connected
        let r = replay(
            fixture!("sdk-run4-mcp-oauth.ndjson"),
            "<tmp>\\work3",
            |_| None,
        );
        let got = summaries(&r.events);
        let server = "chernobyl-mock-oauth";
        assert_eq!(
            got[3],
            format!("mcp {server} needs-auth [\"desktop\", \"phone-callback\"]")
        );
        for s in [
            "reply mcp_oauth_callback_url Ok(())",
            "reply mcp_reconnect Ok(())",
            "mcp chernobyl-mock-oauth connected []",
        ] {
            assert!(got.contains(&s.to_string()), "{s}: {got:#?}");
        }
        let start = r
            .events
            .iter()
            .find_map(|e| match e {
                Event::AuthStarted { result: Ok(s), .. } => Some(s.clone()),
                _ => None,
            })
            .unwrap();
        assert!(
            start.auth_url.starts_with("http://127.0.0.1:"),
            "{}",
            start.auth_url
        );
        let cb = start.callback.unwrap();
        assert_eq!(
            (
                cb.port,
                cb.path.as_str(),
                cb.state.as_str(),
                cb.prefix.as_str()
            ),
            (
                48807,
                "/callback",
                "redacted-state",
                "http://localhost:48807/callback"
            )
        );
        // one pending sign-in per server, single use, checked before relaying
        let mut pending = PendingAuth::default();
        pending.insert("ses-1", server, cb.clone(), 0);
        let taken = pending.take("ses-1", server, 1_000).unwrap();
        let forged = "http://localhost:48807/callback?code=redacted-code&state=forged";
        assert_eq!(
            check_callback_url(forged, taken.port, &taken.path, &taken.state).err(),
            Some("callback state does not match the sign-in")
        );
        assert!(pending.take("ses-1", server, 1_000).is_none(), "single use");
        let q = "?code=redacted-code&state=redacted-state";
        for (url, why) in [
            (
                format!("http://evil.test:48807/callback{q}"),
                "callback host must be localhost or 127.0.0.1",
            ),
            (
                format!("http://localhost:48808/callback{q}"),
                "callback port does not match the sign-in",
            ),
            (
                format!("http://localhost:48807/cb{q}"),
                "callback path does not match the sign-in",
            ),
        ] {
            assert_eq!(
                check_callback_url(&url, cb.port, &cb.path, &cb.state).err(),
                Some(why)
            );
        }
        let pasted = format!("http://localhost:48807/callback{q}");
        let ok = check_callback_url(&pasted, cb.port, &cb.path, &cb.state).unwrap();
        let mut d = Driver::new("claude-1", "ses-1", "<tmp>\\work3");
        let mut written = Vec::new();
        d.mcp_oauth_callback_url("cmd-cb", server, &ok.url, |f| {
            written.push(f);
            Ok(())
        })
        .unwrap();
        d.mcp_reconnect(Some("cmd-cb"), server, |f| {
            written.push(f);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            written,
            [
                format!(
                    r#"{{"type":"control_request","request_id":"chb_1","request":{{"subtype":"mcp_oauth_callback_url","serverName":"{server}","callbackUrl":"{pasted}"}}}}"#
                ),
                format!(
                    r#"{{"type":"control_request","request_id":"chb_2","request":{{"subtype":"mcp_reconnect","serverName":"{server}"}}}}"#
                ),
            ]
        );
        // a sign-in with no localhost callback is desktop-only from then on;
        // claude.ai connectors open claude.ai; stdio servers have nothing to do
        let mut d = Driver::new("claude-1", "ses-1", "<tmp>\\work3");
        d.mcp_authenticate("cmd-a", "remote", |_| Ok(())).unwrap();
        d.mcp_status(|_| Ok(())).unwrap();
        let reply = r#"{"type":"control_response","response":{"subtype":"success","request_id":"chb_1","response":{"authUrl":"https://auth.example/authorize?x=1","requiresUserAction":true,"callbackExpected":false}}}"#;
        let started = d.on_line(reply, 0);
        assert!(
            matches!(&started[0], Event::AuthStarted { result: Ok(s), .. } if s.callback.is_none())
        );
        let status = r#"{"type":"control_response","response":{"subtype":"success","request_id":"chb_2","response":{"mcpServers":[{"name":"remote","status":"needs-auth","config":{"type":"http","url":"https://mcp.example/mcp"}},{"name":"claude.ai Gmail","status":"needs-auth","config":{"type":"claudeai-proxy","url":"https://mcp-proxy.example/x"}},{"name":"local","status":"needs-auth","config":{"type":"stdio","command":"x"}},{"name":"odd","status":"exploded","config":{"type":"http"}}]}}}"#;
        let events = d.on_line(status, 0);
        let Event::Op(Op::Mcp(list)) = &events[0] else {
            panic!("{events:?}")
        };
        let flows: Vec<(&str, &str, Vec<&str>)> = list
            .iter()
            .map(|m| (m.name.as_str(), m.status, m.auth_flows.clone()))
            .collect();
        assert_eq!(
            flows,
            [
                ("remote", "needs-auth", vec!["desktop"]),
                ("claude.ai Gmail", "needs-auth", vec!["claude-ai"]),
                ("local", "needs-auth", vec![]),
                ("odd", "failed", vec![]),
            ]
        );
        let auth = list[1].auth.as_ref().unwrap();
        assert_eq!(auth.url.as_deref(), Some(CLAUDE_AI_CONNECTORS));
        assert_eq!(list[3].error.as_deref(), Some("status: exploded"));
    }

    /* ---------------------------- live ---------------------------- */

    /// Drives the real CLI (Haiku, about $0.01) through this driver's
    /// encoders and decoder: `CHERNOBYL_LIVE_CLAUDE=1 cargo test --lib
    /// live_claude -- --ignored --nocapture`. `CHERNOBYL_LIVE_CLAUDE_DIR` is
    /// the parent of the temp workspace; `CHERNOBYL_LIVE_CLAUDE_CAPTURE`
    /// names a file for the raw frames, which are unsanitized (the
    /// initialize response carries the account): keep it out of the repo.
    #[test]
    #[ignore = "spawns the real Claude Code CLI"]
    fn live_claude_haiku_protocol_round_trip() {
        if std::env::var_os("CHERNOBYL_LIVE_CLAUDE").is_none() {
            return;
        }
        let env = |k: &str| std::env::var_os(k);
        let bin =
            resolve_binary(None, env("USERPROFILE").as_deref(), env("PATH").as_deref()).unwrap();
        let session = new_uuid().unwrap();
        let parent =
            env("CHERNOBYL_LIVE_CLAUDE_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let cwd = parent.join(format!("chb-live-{}", &session[..8]));
        std::fs::create_dir_all(&cwd).unwrap();
        let ws = plain(&std::fs::canonicalize(&cwd).unwrap());
        let mut args = spawn_args(&session, false, Some("default"), None).unwrap();
        args.extend(["--model".to_string(), "haiku".to_string()]);
        let (io, rx) = ChildIo::spawn(&bin, &args, &cwd, &spawn_env(std::env::vars_os())).unwrap();
        let t0 = crate::protocol::now_ms();
        let now = || crate::protocol::now_ms() - t0;
        let log = RefCell::new(Vec::<String>::new());
        let record = |dir: &str, frame: &str| {
            let frame = if json::parse(frame).is_ok() {
                frame.to_string()
            } else {
                q(frame)
            };
            log.borrow_mut().push(format!(
                "{{\"dir\":\"{dir}\",\"t\":{},\"msg\":{frame}}}",
                now()
            ));
        };
        let send = |f: String| {
            record("in", &f);
            io.try_send(f)
        };
        let mut d = Driver::new("claude-1", &session, &ws);
        d.initialize(send).unwrap();
        let prompt = "Create a.txt containing hi, then edit it to say hello";
        d.user_prompt("cmd-1", prompt, &[], now(), send).unwrap();
        let (mut events, mut code, mut edits) = (Vec::new(), None, 0);
        let mut closing = false;
        while let Ok(ev) = rx.recv_timeout(Duration::from_secs(240)) {
            let batch = match ev {
                ChildEvent::Line(l) => {
                    record("out", &l);
                    d.on_line(&l, now())
                }
                ChildEvent::Skipped(n) => d.on_skipped(n, now()),
                ChildEvent::Exit(c) => {
                    code = c;
                    events.extend(d.on_exit(c, now()));
                    break;
                }
            };
            for e in &batch {
                match e {
                    Event::Write(f) => send(f.clone()).unwrap(),
                    Event::Op(Op::Turn(t)) if t.ended_at.is_some() && !closing => {
                        closing = true;
                        d.mcp_status(send).unwrap();
                    }
                    Event::Op(Op::Mcp(_)) if closing => io.close_stdin(),
                    _ => {}
                }
            }
            events.extend(batch.iter().cloned());
            for p in permissions(&batch) {
                // Write once, then the Edit through the offered acceptEdits
                // suggestion (the byte-for-byte echo); nothing outside the
                // workspace, nothing else
                let inside = p
                    .command
                    .to_ascii_lowercase()
                    .starts_with(&ws.to_ascii_lowercase());
                let (choice, option) = if p.action == "file-write" && inside {
                    edits += 1;
                    let suggestion = p
                        .options
                        .iter()
                        .find(|o| o.label == "Allow all edits this session");
                    match suggestion {
                        Some(o) if edits > 1 => ("approve-session", o.id.clone()),
                        _ => ("approve-once", "allow".to_string()),
                    }
                } else {
                    ("reject", "deny".to_string())
                };
                let dec = Decision {
                    permission_id: &p.id,
                    decision: choice,
                    option_id: Some(&option),
                    message: Some("not needed for this test"),
                    answers: &[],
                };
                events.extend(d.decide(&dec, send).unwrap());
            }
        }
        if let Some(path) = env("CHERNOBYL_LIVE_CLAUDE_CAPTURE") {
            std::fs::write(path, log.borrow().join("\n") + "\n").unwrap();
        }
        let got = summaries(&events);
        println!("{got:#?}");
        assert_eq!(code, Some(0), "stderr: {}", io.stderr_tail());
        assert!(got.iter().any(|s| s == "append Text"), "streaming text");
        assert!(edits >= 1, "a Write permission was answered");
        assert!(
            got.iter().any(|s| s.starts_with("turn completed")),
            "{got:#?}"
        );
        let text = std::fs::read_to_string(cwd.join("a.txt")).unwrap();
        assert_eq!(text.trim(), "hello");
    }
}
