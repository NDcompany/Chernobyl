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
//! The companion never writes the user's `settings.json`. `bridge_config()`
//! prints the exact snippet the operator pastes in themselves (explicit
//! install). Ingestion is attach-only in reverse: a dead/absent bridge simply
//! means no events arrive — never a spawn or a config rewrite.

use crate::adapters::Capability;
use crate::json::{self, Json};
use crate::opencode::AdapterError;
use crate::protocol::{ActivityEvent, EventKind, next_id, now_ms};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

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

    /// Accept one hook payload (already read off the wire). Returns the
    /// normalized event to publish, or `None` for an ignorable/unknown hook
    /// (counted but never surfaced as a fake). Errors are malformed payloads.
    pub fn ingest(&self, payload: &str) -> Result<Option<EventKind>, AdapterError> {
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

fn normalize_hook(v: &Json, event_name: &str) -> Option<EventKind> {
    let session_id = json::get_str(v, "session_id").unwrap_or("(unknown session)");
    let cwd = json::get_str(v, "cwd").unwrap_or("");
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
            let source = json::get_str(v, "source").unwrap_or("unknown");
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
            let msg = json::get_str(v, "message").unwrap_or("(no message)");
            activity("session", format!("Claude Code notification: {msg}"))
        }
        "PreToolUse" => {
            let tool = json::get_str(v, "tool_name").unwrap_or("(unknown tool)");
            activity("agent", format!("Claude Code about to use tool {tool}"))
        }
        "PostToolUse" => {
            let tool = json::get_str(v, "tool_name").unwrap_or("(unknown tool)");
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
pub fn bridge_config(ingress_url: &str) -> String {
    // The bridge is a one-line curl that streams stdin to the ingress. It
    // fails closed: if the companion is down, curl exits non-zero and Claude
    // Code continues (hooks are advisory — a dead bridge never blocks a turn).
    let cmd = format!(
        "curl -sS -X POST -H \"content-type: application/json\" --data-binary @- \"{ingress_url}\""
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

    #[test]
    fn bridge_config_targets_ingress_and_is_explicit() {
        let cfg = bridge_config("http://127.0.0.1:9191/hooks/claude");
        assert!(cfg.contains("http://127.0.0.1:9191/hooks/claude"), "{cfg}");
        assert!(
            cfg.contains("settings.json"),
            "must name the file the user edits"
        );
        // it is a snippet for the user to paste, never written by the companion
        assert!(cfg.contains("CHERNOBYL never writes it"));
        assert!(cfg.contains("PreToolUse"));
        assert!(cfg.contains("SessionStart"));
    }
}
