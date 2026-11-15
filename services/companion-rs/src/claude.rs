//! Claude Code adapter (Phase 7 — PRD §7.5): a hook **ingress**, not an
//! attach-client. Claude Code fires hooks by invoking a small local command
//! with a JSON payload on stdin; that command (the "explicitly installed hook
//! bridge") POSTs the payload to this companion's loopback diagnostics
//! listener, where it is normalized into the SAME seq/persist/fan-out event
//! pipeline as engine and OpenCode events (one sequence authority, C2).
//!
//! Honesty rules carried from PRD §7.5:
//!  * Hooks are one-way and session-scoped. They do **not** provide arbitrary
//!    session attachment, historical-session enumeration, or universal remote
//!    message injection — those capabilities are reported `unavailable`, never
//!    faked. Session *control* (create/prompt/interrupt) stays unavailable.
//!  * Claude Code's own permission checks are **never disabled** to simplify
//!    integration; the engine remains the G2 authority and a live
//!    asked→decision round-trip through the bridge is reported `unavailable`
//!    until release-gate verified.
//!  * Only `SessionStart` is live-verified in this build (real payload captured
//!    against Claude Code 2.1.309 on 2026-10-23). Other hook shapes are parsed
//!    **defensively** from their documented field names and reported `limited`
//!    (documented, not yet live-captured) — never as verified.
//!
//! The companion never writes the user's `settings.json` on its own.
//! `bridge_config()` prints the exact snippet the operator pastes in
//! themselves (explicit install). Ingestion is attach-only in reverse: a
//! dead/absent bridge simply means no events arrive — never a spawn or a
//! config rewrite.
//!
//! ADR-018 adds one round-trip, and only that one: the `PermissionRequest`
//! command hook (`companion --hook permission-request`, [`permission_hook`])
//! posts the hook's stdin to the loopback, control-key route
//! `/hooks/claude/permission` and waits for the phone's allow or deny, at
//! most its `--timeout`. Every failure (no companion, no phone, timeout,
//! anything else) prints nothing and exits 0, so Claude Code asks on the PC
//! as usual. [`install_hook`] / [`uninstall_hook`] add or remove exactly
//! that entry in `settings.json`, only when the operator runs
//! `--install-hooks` / `--uninstall-hooks`, after a backup.

use crate::adapters::Capability;
use crate::json::{self, Json};
use crate::opencode::AdapterError;
use crate::protocol::{ActivityEvent, EventKind, next_id, now_ms};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/* ------------------------------------------------------------------ */
/* State                                                               */
/* ------------------------------------------------------------------ */

/// One normalized observation from the most recent hook, for the status line.
#[derive(Debug, Clone)]
pub struct LastHook {
    pub event: String,
    pub session_id: Option<String>,
    pub ts: i64,
}

pub struct ClaudeAdapter {
    pub id: String,
    /// Total hooks accepted since boot (proof a bridge is actually installed
    /// and delivering — the honest signal, not a claimed connection).
    received: AtomicU64,
    last: Mutex<Option<LastHook>>,
}

impl ClaudeAdapter {
    pub fn new(id: &str) -> ClaudeAdapter {
        ClaudeAdapter {
            id: id.to_string(),
            received: AtomicU64::new(0),
            last: Mutex::new(None),
        }
    }

    /// PRD §7.3 honest capability declaration. Structural rules (tested):
    /// unique ids, every non-`available` entry carries a `detail`, and at
    /// least one entry is not `available`. Maps directly to PRD §7.5's
    /// explicit "do not assume" list.
    pub fn capabilities(&self) -> Vec<Capability> {
        vec![
            Capability::new("server.discovery", "available", None),
            Capability::new("server.health", "available", None),
            Capability::new(
                "session.lifecycle",
                "limited",
                Some(
                    "SessionStart live-verified against Claude Code 2.1.309; Stop/SessionEnd \
                     are parsed from their documented hook shapes and pending release-gate \
                     live capture",
                ),
            ),
            Capability::new(
                "tool.activity",
                "limited",
                Some(
                    "Pre/PostToolUse hooks carry tool name/input; payload shape is documented \
                     but not yet live-captured in this build",
                ),
            ),
            Capability::new(
                "transcript.read",
                "limited",
                Some(
                    "each hook carries transcript_path; complete transcript history is NOT \
                     guaranteed (PRD §7.5) and is not claimed",
                ),
            ),
            Capability::new(
                "permission.review",
                "unavailable",
                Some(
                    "PreToolUse can gate, but a live asked→decision round-trip through the \
                     bridge is unverified; the engine stays G2 authority and Claude Code's \
                     own permission checks are never disabled (PRD §7.5)",
                ),
            ),
            Capability::new(
                "session.list",
                "unavailable",
                Some(
                    "hooks expose only the active session; no supported API enumerates \
                     historical Claude Code sessions",
                ),
            ),
            Capability::new(
                "session.create",
                "unavailable",
                Some("no supported hook creates a session"),
            ),
            Capability::new(
                "session.prompt",
                "unavailable",
                Some(
                    "no supported remote message injection via hooks (PRD §7.5 explicitly \
                     forbids assuming it)",
                ),
            ),
            Capability::new(
                "session.interrupt",
                "unavailable",
                Some("no supported hook interrupts a running turn"),
            ),
            Capability::new(
                "server.spawn",
                "unavailable",
                Some(
                    "attach-only by design (G4.3): Claude Code is the user's own process; \
                     never spawned or reconfigured by the companion",
                ),
            ),
        ]
    }

    pub fn is_healthy(&self) -> bool {
        true // the ingress itself is always ready on the loopback listener
    }

    pub fn received(&self) -> u64 {
        self.received.load(Ordering::SeqCst)
    }

    pub fn last_hook(&self) -> Option<LastHook> {
        self.last.lock().unwrap().clone()
    }

    /// Human status for the adapter list: distinguishes "ingress ready, no
    /// bridge installed yet" from "bridge active (N events)" — never claims a
    /// bridge that isn't delivering.
    pub fn detail(&self) -> String {
        let n = self.received();
        if n == 0 {
            "ingress ready on loopback diagnostics listener; no hook bridge has delivered \
             an event yet (run `companion claude-hooks` to print the install snippet)"
                .into()
        } else {
            let last = self
                .last_hook()
                .map(|l| format!(", last: {} @ {}", l.event, l.ts))
                .unwrap_or_default();
            format!("bridge active — {n} hook(s) received{last}")
        }
    }

    /// Hard cap on ingested hook payloads (audit §6 bounded output). Oversize
    /// payloads are rejected immediately to protect against unbounded memory growth.
    pub const MAX_HOOK_PAYLOAD_BYTES: usize = 1024 * 1024; // 1 MiB

    /// Accept one hook payload (already read off the wire). Returns the
    /// normalized event to publish, or `None` for an ignorable/unknown hook
    /// (counted but never surfaced as a fake). Errors are malformed payloads.
    pub fn ingest(&self, payload: &str) -> Result<Option<EventKind>, AdapterError> {
        if payload.len() > Self::MAX_HOOK_PAYLOAD_BYTES {
            return Err(AdapterError::InvalidResponse(format!(
                "hook payload exceeds {} byte limit (was {})",
                Self::MAX_HOOK_PAYLOAD_BYTES,
                payload.len()
            )));
        }
        let v = json::parse(payload).map_err(|e| {
            AdapterError::InvalidResponse(format!("hook body is not valid JSON: {e:?}"))
        })?;
        let event_name = json::get_str(&v, "hook_event_name").ok_or_else(|| {
            AdapterError::InvalidResponse("hook payload has no `hook_event_name`".into())
        })?;
        let session_id = json::get_str(&v, "session_id").map(str::to_string);
        let kind = normalize_hook(&v, event_name);
        self.received.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().unwrap() = Some(LastHook {
            event: event_name.to_string(),
            session_id,
            ts: now_ms(),
        });
        Ok(kind)
    }
}

/* ------------------------------------------------------------------ */
/* Normalization (defensive — documented shapes, only SessionStart is  */
/* live-verified; unknown hooks are ignored, never fabricated)          */
/* ------------------------------------------------------------------ */

/// Longest hook-supplied text surfaced in an activity line (E2: a hook body
/// may be 64 KB and 60 activity lines ride in every hello).
const HOOK_TEXT_MAX: usize = 500;

/// Hook-supplied text as shown to the phone: secrets redacted, then capped.
fn hook_text(s: &str) -> String {
    crate::runtime_io::redact(s)
        .chars()
        .take(HOOK_TEXT_MAX)
        .collect()
}

fn normalize_hook(v: &Json, event_name: &str) -> Option<EventKind> {
    let session_id = hook_text(json::get_str(v, "session_id").unwrap_or("(unknown session)"));
    let session_id = session_id.as_str();
    let cwd = hook_text(json::get_str(v, "cwd").unwrap_or(""));
    let activity = |kind: &str, text: String| {
        Some(EventKind::Activity(ActivityEvent {
            id: next_id("evt"),
            ts: now_ms(),
            kind: kind.to_string(),
            text,
            ref_id: json::get_str(v, "session_id").map(str::to_string),
        }))
    };
    match event_name {
        "SessionStart" => {
            let source = hook_text(json::get_str(v, "source").unwrap_or("unknown"));
            activity(
                "session",
                format!("Claude Code session started ({source}) in {cwd}"),
            )
        }
        "SessionEnd" => activity(
            "session",
            format!("Claude Code session ended ({session_id})"),
        ),
        "Stop" => activity(
            "session",
            format!("Claude Code turn complete ({session_id})"),
        ),
        "SubagentStop" => activity(
            "session",
            format!("Claude Code subagent turn complete ({session_id})"),
        ),
        "Notification" => {
            let msg = hook_text(json::get_str(v, "message").unwrap_or("(no message)"));
            activity("session", format!("Claude Code notification: {msg}"))
        }
        "PreToolUse" => {
            let tool = hook_text(json::get_str(v, "tool_name").unwrap_or("(unknown tool)"));
            activity("agent", format!("Claude Code about to use tool {tool}"))
        }
        "PostToolUse" => {
            let tool = hook_text(json::get_str(v, "tool_name").unwrap_or("(unknown tool)"));
            activity("agent", format!("Claude Code used tool {tool}"))
        }
        // UserPromptSubmit, PreCompact, and anything newer: counted by the
        // adapter but not surfaced — an unknown hook is never faked into a
        // meaningful-looking event.
        _ => None,
    }
}

/* ------------------------------------------------------------------ */
/* Bridge config generator (explicit install — the companion never     */
/* writes the user's settings.json)                                     */
/* ------------------------------------------------------------------ */

/// Print the `settings.json` hook snippet that forwards every Claude Code
/// hook to this companion's loopback ingress. The operator pastes it in
/// themselves. `ingress_url` is the companion's loopback diagnostics
/// `/hooks/claude` endpoint.
pub fn bridge_config(ingress_url: &str, control_key: &str) -> String {
    // The bridge is a one-line curl that streams stdin to the ingress, with
    // the control key the route requires (security pass 2026-11-15, E2; the
    // key is as private as this settings file). It fails closed: if the
    // companion is down, curl exits non-zero and Claude Code continues (hooks
    // are advisory — a dead bridge never blocks a turn).
    let cmd = format!(
        "curl -sS -X POST -H \"content-type: application/json\" -H \"{}: {control_key}\" --data-binary @- \"{ingress_url}\"",
        crate::server::CONTROL_KEY_HEADER
    );
    let events = [
        "SessionStart",
        "SessionEnd",
        "Stop",
        "SubagentStop",
        "Notification",
        "PreToolUse",
        "PostToolUse",
    ];
    let mut hooks = Vec::new();
    for ev in events {
        hooks.push(format!(
            "    \"{ev}\": [ {{ \"hooks\": [ {{ \"type\": \"command\", \"command\": \"{}\" }} ] }} ],",
            cmd.replace('\\', "\\\\").replace('"', "\\\"")
        ));
    }
    format!(
        "// Paste into ~/.claude/settings.json (explicit install — CHERNOBYL never writes it).\n\
         // Every hook above is forwarded to the companion ingress; a dead companion means\n\
         // the hook exits non-zero and Claude Code simply continues (hooks are advisory).\n\
         {{\n  \"hooks\": {{\n{}\n  }}\n}}\n",
        hooks.join("\n")
    )
}

/* ------------------------------------------------------------------ */
/* ADR-018: the PermissionRequest hook (approvals from the phone)       */
/* ------------------------------------------------------------------ */

/// What identifies our hook command in `settings.json`.
pub const HOOK_ARGS: &str = "--hook permission-request";
/// `--install-hooks` writes the old file here first.
pub const SETTINGS_BACKUP: &str = "settings.json.chernobyl-backup";
/// The hook's own wait for the phone by default (seconds).
pub const HOOK_TIMEOUT_S: u64 = 120;
/// Claude Code's timeout for the hook is ours plus this margin, so the
/// hook always gives up (printing nothing) before Claude Code kills it.
const HOOK_MARGIN_S: u64 = 30;

/// The hook's stdout for the companion's answer `{"behavior":"allow"}` or
/// `{"behavior":"deny","message":…}`; `None` prints nothing.
pub fn hook_output(answer: &str) -> Option<String> {
    let v = json::parse(answer).ok()?;
    let decision = match json::get_str(&v, "behavior")? {
        "allow" => json::obj(vec![("behavior", json::s("allow"))]),
        "deny" => json::obj(vec![
            ("behavior", json::s("deny")),
            (
                "message",
                json::s(json::get_str(&v, "message").unwrap_or("Denied from the phone")),
            ),
        ]),
        _ => return None,
    };
    Some(json::stringify(&json::obj(vec![(
        "hookSpecificOutput",
        json::obj(vec![
            ("hookEventName", json::s("PermissionRequest")),
            ("decision", decision),
        ]),
    )])))
}

/// `companion --hook permission-request`: the hook's stdin goes to the
/// running companion on `127.0.0.1:<diag_port>`, which answers once the
/// phone decides or after `wait`. Returns what to print; `None` (print
/// nothing, exit 0, the PC asks as usual) on every failure.
pub fn permission_hook(input: &str, diag_port: u16, key: &str, wait: Duration) -> Option<String> {
    if key.is_empty() || input.len() > ClaudeAdapter::MAX_HOOK_PAYLOAD_BYTES {
        return None;
    }
    let v = json::parse(input).ok()?;
    if json::get_str(&v, "hook_event_name") != Some("PermissionRequest") {
        return None;
    }
    let url = format!(
        "http://127.0.0.1:{diag_port}/hooks/claude/permission?wait={}",
        wait.as_millis()
    );
    let read = wait + Duration::from_secs(2);
    let resp = crate::server::control_post_within(&url, key, input, read).ok()?;
    if resp.status != 200 {
        return None;
    }
    hook_output(&resp.body)
}

/// The command line `settings.json` runs: forward slashes and double
/// quotes work in Git Bash and cmd alike. Refuses a path a shell would
/// expand.
pub fn hook_command(exe: &Path, config: &Path, timeout_s: u64) -> Result<String, String> {
    let quote = |p: &Path| -> Result<String, String> {
        let s = p.to_string_lossy().replace('\\', "/");
        if s.chars()
            .any(|c| matches!(c, '"' | '$' | '`' | '%' | '!') || c.is_control())
        {
            return Err(format!(
                "{s}: the path has a character a shell would expand"
            ));
        }
        Ok(format!("\"{s}\""))
    };
    Ok(format!(
        "{} {HOOK_ARGS} --timeout {timeout_s} --config {}",
        quote(exe)?,
        quote(config)?
    ))
}

fn is_ours(hook: &Json) -> bool {
    json::get_str(hook, "command").is_some_and(|c| c.contains(HOOK_ARGS))
}

/// Reads `settings.json` as an object (absent: `{}`), keeping a BOM.
fn read_settings(path: &Path) -> Result<(Json, bool), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Json::Obj(vec![]), false));
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let bom = text.starts_with('\u{feff}');
    let body = text.trim_start_matches('\u{feff}');
    if body.trim().is_empty() {
        return Ok((Json::Obj(vec![]), bom));
    }
    match json::parse(body) {
        Ok(v @ Json::Obj(_)) => Ok((v, bom)),
        Ok(_) => Err(format!(
            "{} is not a JSON object; not touching it",
            path.display()
        )),
        Err(e) => Err(format!(
            "{} is not valid JSON ({e:?}); fix it first, not touching it",
            path.display()
        )),
    }
}

/// Backs the file up (when it exists), then replaces it whole.
fn write_settings(path: &Path, v: &Json, bom: bool) -> Result<(), String> {
    if path.exists() {
        let backup = path.with_file_name(SETTINGS_BACKUP);
        std::fs::copy(path, &backup)
            .map_err(|e| format!("cannot back up to {}: {e}", backup.display()))?;
    } else if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let mut text = if bom {
        "\u{feff}".to_string()
    } else {
        String::new()
    };
    text.push_str(&json::pretty(v));
    text.push('\n');
    let tmp = path.with_file_name("settings.json.chernobyl-tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

/// The value at `key` of an object, inserted as `default` when absent.
fn entry<'a>(obj: &'a mut Json, key: &str, default: Json) -> Option<&'a mut Json> {
    let Json::Obj(pairs) = obj else { return None };
    let at = match pairs.iter().position(|(k, _)| k == key) {
        Some(at) => at,
        None => {
            pairs.push((key.to_string(), default));
            pairs.len() - 1
        }
    };
    Some(&mut pairs[at].1)
}

/// Removes our hook from every `PermissionRequest` group, then groups left
/// empty. Returns whether anything was removed.
fn remove_ours(groups: &mut Vec<Json>) -> bool {
    let before = groups.clone();
    for g in groups.iter_mut() {
        if let Some(Json::Arr(hooks)) = entry(g, "hooks", Json::Arr(vec![])) {
            hooks.retain(|h| !is_ours(h));
        }
    }
    groups.retain(|g| !matches!(json::get(g, "hooks"), Some(Json::Arr(h)) if h.is_empty()));
    *groups != before
}

/// `--install-hooks`: puts exactly one `PermissionRequest` command hook
/// (`command`, Claude Code timeout `timeout_s` + 30 s) into `settings`,
/// keeping every other key and hook. Idempotent: an identical entry
/// leaves the file untouched. Refuses invalid JSON or an unexpected shape.
pub fn install_hook(settings: &Path, command: &str, timeout_s: u64) -> Result<String, String> {
    let (mut root, bom) = read_settings(settings)?;
    let wanted = json::obj(vec![(
        "hooks",
        Json::Arr(vec![json::obj(vec![
            ("type", json::s("command")),
            ("command", json::s(command)),
            ("timeout", Json::Int((timeout_s + HOOK_MARGIN_S) as i64)),
        ])]),
    )]);
    let shape = || {
        format!(
            "{}: \"hooks\" is not the shape Claude Code uses; not touching it",
            settings.display()
        )
    };
    let hooks = entry(&mut root, "hooks", Json::Obj(vec![])).ok_or_else(shape)?;
    if !matches!(hooks, Json::Obj(_)) {
        return Err(shape());
    }
    let Some(Json::Arr(groups)) = entry(hooks, "PermissionRequest", Json::Arr(vec![])) else {
        return Err(shape());
    };
    let ours: Vec<&Json> = groups
        .iter()
        .filter(|g| matches!(json::get(g, "hooks"), Some(Json::Arr(h)) if h.iter().any(is_ours)))
        .collect();
    if ours.len() == 1 && *ours[0] == wanted {
        return Ok(format!(
            "the CHERNOBYL approval hook is already in {}",
            settings.display()
        ));
    }
    remove_ours(groups);
    groups.push(wanted);
    write_settings(settings, &root, bom)?;
    Ok(format!(
        "added the CHERNOBYL approval hook to {} (backup: {})",
        settings.display(),
        SETTINGS_BACKUP
    ))
}

/// `--uninstall-hooks`: removes exactly our hook, and the `PermissionRequest`
/// list and `hooks` object only when that leaves them empty. Idempotent.
pub fn uninstall_hook(settings: &Path) -> Result<String, String> {
    if !settings.exists() {
        return Ok(format!(
            "{} does not exist; nothing to remove",
            settings.display()
        ));
    }
    let (mut root, bom) = read_settings(settings)?;
    let Json::Obj(pairs) = &mut root else {
        unreachable!("read_settings returns objects")
    };
    let Some(hi) = pairs.iter().position(|(k, _)| k == "hooks") else {
        return Ok("the CHERNOBYL approval hook is not installed".into());
    };
    let Json::Obj(hooks) = &mut pairs[hi].1 else {
        return Ok("the CHERNOBYL approval hook is not installed".into());
    };
    let Some(pi) = hooks.iter().position(|(k, _)| k == "PermissionRequest") else {
        return Ok("the CHERNOBYL approval hook is not installed".into());
    };
    let Json::Arr(groups) = &mut hooks[pi].1 else {
        return Ok("the CHERNOBYL approval hook is not installed".into());
    };
    if !remove_ours(groups) {
        return Ok("the CHERNOBYL approval hook is not installed".into());
    }
    if groups.is_empty() {
        hooks.remove(pi);
    }
    if hooks.is_empty() {
        pairs.remove(hi);
    }
    write_settings(settings, &root, bom)?;
    Ok(format!(
        "removed the CHERNOBYL approval hook from {} (backup: {})",
        settings.display(),
        SETTINGS_BACKUP
    ))
}

/// `CLAUDE_CONFIG_DIR`, else `%USERPROFILE%\.claude`, plus `settings.json`.
pub fn settings_path(config_dir: Option<&OsStr>, profile: Option<&OsStr>) -> Option<PathBuf> {
    crate::claude_history::projects_dir(config_dir, profile)
        .and_then(|p| p.parent().map(|d| d.join("settings.json")))
}

/* ------------------------------------------------------------------ */
/* Tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    /// The REAL `SessionStart` payload captured against Claude Code 2.1.309 on
    /// 2026-10-23 (isolated `CLAUDE_CONFIG_DIR`, so the user's real settings
    /// and credentials were never touched). Field names are grounded in this
    /// capture, not guessed.
    const SESSION_START: &str = r#"{
      "session_id": "24e5d08e-906e-47bf-b902-4fc7d50acdec",
      "transcript_path": "C:\\Users\\you\\AppData\\Local\\Temp\\opencode\\claude-hook-probe\\projects\\G--Chernobyl\\24e5d08e.jsonl",
      "cwd": "G:\\Chernobyl",
      "hook_event_name": "SessionStart",
      "source": "startup"
    }"#;

    #[test]
    fn session_start_normalizes_to_activity() {
        let ad = ClaudeAdapter::new("cc-1");
        let kind = ad
            .ingest(SESSION_START)
            .expect("well-formed hook")
            .expect("SessionStart surfaces");
        match kind {
            EventKind::Activity(a) => {
                assert_eq!(a.kind, "session");
                assert!(a.text.contains("started"), "text was: {}", a.text);
                assert_eq!(
                    a.ref_id.as_deref(),
                    Some("24e5d08e-906e-47bf-b902-4fc7d50acdec")
                );
            }
            other => panic!("expected Activity, got {other:?}"),
        }
        assert_eq!(ad.received(), 1);
        let last = ad.last_hook().expect("last hook recorded");
        assert_eq!(last.event, "SessionStart");
    }

    #[test]
    fn pre_tool_use_surfaces_tool_activity() {
        let ad = ClaudeAdapter::new("cc-1");
        let payload =
            r#"{"session_id":"s1","hook_event_name":"PreToolUse","tool_name":"Bash","cwd":"/w"}"#;
        let kind = ad.ingest(payload).unwrap().unwrap();
        match kind {
            EventKind::Activity(a) => {
                assert_eq!(a.kind, "agent");
                assert!(a.text.contains("Bash"), "text was: {}", a.text);
            }
            other => panic!("expected Activity, got {other:?}"),
        }
    }

    #[test]
    fn unknown_hook_is_counted_but_not_fabricated() {
        let ad = ClaudeAdapter::new("cc-1");
        // well-formed but an event we don't normalize → counted, returns None
        let out = ad
            .ingest(r#"{"session_id":"s1","hook_event_name":"PreCompact"}"#)
            .expect("well-formed");
        assert!(out.is_none(), "unknown hook must not surface a fake event");
        assert_eq!(ad.received(), 1); // still counted (proof a bridge is live)
    }

    #[test]
    fn malformed_payload_is_rejected() {
        let ad = ClaudeAdapter::new("cc-1");
        assert!(ad.ingest("{not json").is_err());
        // missing hook_event_name
        assert!(ad.ingest(r#"{"session_id":"s1"}"#).is_err());
        assert_eq!(ad.received(), 0); // nothing accepted
    }

    #[test]
    fn oversize_payload_is_rejected() {
        let ad = ClaudeAdapter::new("cc-1");
        let payload = format!(
            r#"{{"session_id":"s1","hook_event_name":"SessionStart","pad":"{}"}}"#,
            "x".repeat(ClaudeAdapter::MAX_HOOK_PAYLOAD_BYTES + 10)
        );
        let err = ad.ingest(&payload).unwrap_err();
        assert_eq!(err.code(), "invalid-response");
        assert!(err.to_string().contains("byte limit"));
        assert_eq!(ad.received(), 0);
    }

    #[test]
    fn capability_declaration_is_structurally_honest() {
        let caps = ClaudeAdapter::new("cc-1").capabilities();
        let mut ids: Vec<&str> = caps.iter().map(|c| c.id.as_str()).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "capability ids must be unique");
        for c in &caps {
            if c.status != "available" {
                assert!(
                    c.detail.as_deref().unwrap_or("").len() > 10,
                    "capability '{}' is '{}' but has no explanatory detail",
                    c.id,
                    c.status
                );
            }
        }
        assert!(caps.iter().any(|c| c.status != "available"));
        // PRD §7.5: hooks must NOT be faked into session control / injection
        for sid in [
            "session.list",
            "session.create",
            "session.prompt",
            "session.interrupt",
        ] {
            let c = caps.iter().find(|c| c.id == sid).expect(sid);
            assert_eq!(c.status, "unavailable", "{sid} must not be faked available");
        }
        // permission checks stay Claude Code's own; never claimed by us
        let p = caps.iter().find(|c| c.id == "permission.review").unwrap();
        assert_eq!(p.status, "unavailable");
    }

    /// Security pass 2026-11-15 (E2): hook text is redacted and capped, so a
    /// 60 KB hook can neither leak a secret nor bloat the snapshot.
    #[test]
    fn hook_text_is_redacted_and_capped() {
        let big = "x".repeat(60_000);
        let secret = format!("ghp_{}", "x".repeat(36));
        let hook = format!(
            r#"{{"hook_event_name":"Notification","session_id":"s1","cwd":"{big}","message":"token {secret} {big}"}}"#
        );
        let v = json::parse(&hook).unwrap();
        let Some(EventKind::Activity(a)) = normalize_hook(&v, "Notification") else {
            panic!("a Notification surfaces as activity");
        };
        assert!(
            a.text.chars().count() <= HOOK_TEXT_MAX + 40,
            "{}",
            a.text.len()
        );
        assert!(!a.text.contains(&secret), "{}", a.text);
        let start = format!(
            r#"{{"hook_event_name":"SessionStart","session_id":"s1","source":"{big}","cwd":"{big}"}}"#
        );
        let v = json::parse(&start).unwrap();
        let Some(EventKind::Activity(a)) = normalize_hook(&v, "SessionStart") else {
            panic!("a SessionStart surfaces as activity");
        };
        assert!(
            a.text.chars().count() <= 2 * HOOK_TEXT_MAX + 80,
            "{}",
            a.text.len()
        );
    }

    #[test]
    fn bridge_config_targets_ingress_and_is_explicit() {
        let cfg = bridge_config("http://127.0.0.1:9191/hooks/claude", "k3y");
        assert!(cfg.contains("http://127.0.0.1:9191/hooks/claude"), "{cfg}");
        // E2: the ingress route needs the control key
        assert!(
            cfg.contains(&format!("{}: k3y", crate::server::CONTROL_KEY_HEADER)),
            "{cfg}"
        );
        assert!(
            cfg.contains("settings.json"),
            "must name the file the user edits"
        );
        // it is a snippet for the user to paste, never written by the companion
        assert!(cfg.contains("CHERNOBYL never writes it"));
        assert!(cfg.contains("PreToolUse"));
        assert!(cfg.contains("SessionStart"));
    }

    /* ---- ADR-018: the PermissionRequest hook ---- */

    /// The documented stdin shape (code.claude.com/docs/en/hooks).
    const ASK: &str = r#"{"session_id":"11111111-1111-4111-8111-111111111111","transcript_path":"C:\\t.jsonl","cwd":"C:\\w","permission_mode":"default","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"rm -rf node_modules","description":"Remove node_modules"}}"#;

    /// A one-shot loopback server: records the request, then answers
    /// `reply` (None: holds the connection open without answering).
    fn fake(reply: Option<&'static str>) -> (u16, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_millis(300)))
                .unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = s.read(&mut buf) {
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
            }
            match reply {
                Some(r) => {
                    let _ = s.write_all(r.as_bytes());
                }
                None => std::thread::sleep(Duration::from_secs(4)),
            }
            String::from_utf8_lossy(&req).into_owned()
        });
        (port, t)
    }

    fn ok(body: &str) -> &'static str {
        Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )
    }

    #[test]
    fn hook_prints_the_documented_decision_shape() {
        let (port, t) = fake(Some(ok(r#"{"behavior":"allow"}"#)));
        let out = permission_hook(ASK, port, "k3y", Duration::from_secs(5)).unwrap();
        assert_eq!(
            out,
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
        );
        let req = t.join().unwrap();
        assert!(
            req.starts_with("POST /hooks/claude/permission?wait=5000 "),
            "{req}"
        );
        assert!(req.contains(&format!("{}: k3y\r\n", crate::server::CONTROL_KEY_HEADER)));
        assert!(req.ends_with(ASK), "the hook's stdin is the body");

        let (port, t) = fake(Some(ok(r#"{"behavior":"deny","message":"not on main"}"#)));
        let out = permission_hook(ASK, port, "k", Duration::from_secs(5)).unwrap();
        t.join().unwrap();
        let v = json::parse(&out).unwrap();
        let d = json::get(json::get(&v, "hookSpecificOutput").unwrap(), "decision").unwrap();
        assert_eq!(json::get_str(d, "behavior"), Some("deny"));
        assert_eq!(json::get_str(d, "message"), Some("not on main"));
    }

    /// Every failure prints nothing: no decision, so the PC asks as usual.
    #[test]
    fn hook_prints_nothing_on_any_failure() {
        // 204: not the phone's, or the wait ran out in the companion
        let (port, t) = fake(Some("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n"));
        assert_eq!(
            permission_hook(ASK, port, "k", Duration::from_secs(5)),
            None
        );
        t.join().unwrap();
        // the companion never answers: our own read timeout ends it
        let (port, t) = fake(None);
        let start = std::time::Instant::now();
        assert_eq!(
            permission_hook(ASK, port, "k", Duration::from_millis(100)),
            None
        );
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "{:?}",
            start.elapsed()
        );
        drop(t);
        // no companion: nothing listens there
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        assert_eq!(
            permission_hook(ASK, port, "k", Duration::from_secs(1)),
            None
        );
        // not a PermissionRequest, not JSON, no key, a garbage answer
        let other = ASK.replace("PermissionRequest", "PreToolUse");
        assert_eq!(
            permission_hook(&other, port, "k", Duration::from_secs(1)),
            None
        );
        assert_eq!(
            permission_hook("{oops", port, "k", Duration::from_secs(1)),
            None
        );
        assert_eq!(permission_hook(ASK, port, "", Duration::from_secs(1)), None);
        assert_eq!(hook_output(r#"{"behavior":"maybe"}"#), None);
        assert_eq!(hook_output("<html>"), None);
    }

    fn scratch() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "chb-hooks-{}",
            crate::claude_live::new_uuid().unwrap()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn install_merges_is_idempotent_and_uninstall_restores() {
        let dir = scratch();
        let path = dir.join("settings.json");
        let original = r#"{
  "model": "opus",
  "hooks": {
    "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "lint.sh"}]}],
    "PermissionRequest": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "mine.sh", "timeout": 5}]}]
  },
  "permissions": {"allow": ["Bash(ls:*)"], "x": 1.5}
}"#;
        std::fs::write(&path, original).unwrap();
        let cmd = hook_command(
            Path::new("C:\\Programs\\CHERNOBYL\\chernobyl-companion.exe"),
            Path::new("C:\\Users\\u\\AppData\\Roaming\\CHERNOBYL\\companion.json"),
            120,
        )
        .unwrap();
        assert_eq!(
            cmd,
            r#""C:/Programs/CHERNOBYL/chernobyl-companion.exe" --hook permission-request --timeout 120 --config "C:/Users/u/AppData/Roaming/CHERNOBYL/companion.json""#
        );
        install_hook(&path, &cmd, 120).unwrap();
        // the backup is the file as it was
        assert_eq!(
            std::fs::read_to_string(dir.join(SETTINGS_BACKUP)).unwrap(),
            original
        );
        let v = json::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let before = json::parse(original).unwrap();
        for key in ["model", "permissions"] {
            assert_eq!(json::get(&v, key), json::get(&before, key), "{key}");
        }
        let hooks = json::get(&v, "hooks").unwrap();
        assert_eq!(
            json::get(hooks, "PreToolUse"),
            json::get(json::get(&before, "hooks").unwrap(), "PreToolUse")
        );
        let groups = json::as_arr(json::get(hooks, "PermissionRequest").unwrap()).unwrap();
        assert_eq!(groups.len(), 2, "the user's own hook stays");
        let ours = json::as_arr(json::get(&groups[1], "hooks").unwrap()).unwrap();
        assert_eq!(json::get_str(&ours[0], "command"), Some(cmd.as_str()));
        assert_eq!(json::get_i64(&ours[0], "timeout"), Some(150));
        assert!(json::get(&groups[1], "matcher").is_none(), "every tool");

        // idempotent: the same install does not write (or back up) again
        let installed = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(dir.join(SETTINGS_BACKUP)).unwrap();
        let msg = install_hook(&path, &cmd, 120).unwrap();
        assert!(msg.contains("already"), "{msg}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), installed);
        assert!(!dir.join(SETTINGS_BACKUP).exists());
        // a new timeout replaces our entry, never adds a second one
        install_hook(&path, &cmd.replace("120", "60"), 60).unwrap();
        let v = json::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let groups = json::get(json::get(&v, "hooks").unwrap(), "PermissionRequest").unwrap();
        assert_eq!(json::as_arr(groups).unwrap().len(), 2);

        // uninstall: back to the user's content
        uninstall_hook(&path).unwrap();
        let v = json::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v, before);
        assert!(uninstall_hook(&path).unwrap().contains("not installed"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn install_creates_a_missing_file_and_uninstall_leaves_no_trace() {
        let dir = scratch();
        let path = dir.join("sub").join("settings.json");
        install_hook(&path, "\"c:/x.exe\" --hook permission-request", 120).unwrap();
        assert!(
            !dir.join("sub").join(SETTINGS_BACKUP).exists(),
            "nothing to back up"
        );
        uninstall_hook(&path).unwrap();
        assert_eq!(
            json::parse(&std::fs::read_to_string(&path).unwrap()).unwrap(),
            Json::Obj(vec![])
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn install_refuses_invalid_json_and_odd_shapes() {
        let dir = scratch();
        let path = dir.join("settings.json");
        for bad in [
            "{\"model\": ",
            "[1,2]",
            r#"{"hooks": []}"#,
            r#"{"hooks": {"PermissionRequest": {}}}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(
                install_hook(&path, "x --hook permission-request", 120).is_err(),
                "{bad}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bad, "untouched");
            assert!(!dir.join(SETTINGS_BACKUP).exists());
        }
        std::fs::write(&path, "{\"model\": ").unwrap();
        assert!(uninstall_hook(&path).is_err());
        // a path a shell would expand is refused
        assert!(hook_command(Path::new("C:\\a$b\\x.exe"), Path::new("c.json"), 120).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
