# Phase 3 live OpenCode end-to-end — validation evidence

**Status:** ✅ PASS (2026-10-24). Opt-in integration test `services/companion-rs/tests/opencode_e2e.rs::live_opencode_end_to_end` run manually against a **real `opencode` CLI** (v1.19.4) on an isolated `XDG_DATA_HOME`. This was a blocked item ("needs a real `opencode` CLI on PATH") — the CLI became available on PATH (npm shim target `node_modules\opencode-ai\bin\opencode.exe`) and the run below is the resulting evidence. Test command:

```text
cargo test --test opencode_e2e -- --ignored --nocapture
```

**Isolation (ADR-010 / opencode.md §9):** own `XDG_DATA_HOME` (never the shared `~/.local/share/opencode`); attach-only (G4.3 — the harness owns the server lifecycle, the adapter only attaches); Windows harness (`taskkill /T` tree-kill for restart recovery).

**Result:** `test result: ok. 1 passed; 0 failed` in 44.72 s.

## Evidence captured (verbatim from the run)

| Step | Claim | Evidence line |
|---|---|---|
| 1. Attach readiness | Adapter probe reached a healthy real runtime | `attached to real opencode 1.19.4 at http://127.0.0.1:50858 (isolated XDG_DATA_HOME)` |
| 2. Agent/session map | Real agent list mapped, version read | `agents=["build","compaction","explore","general","plan","summary","title"] sessions=0 version=1.19.4` |
| 3. Create session | Real `ses_` session created on the runtime | `created real session ses_e93dc261affejMuawBFB3Txds1` |
| 4. Prompt acceptance | `prompt_async` accepted (HTTP 204), outcome deferred to SSE (G1) | `prompt accepted (204) — outcome follows on SSE` |
| 5. SSE normalization | User-message text normalized out of the LIVE stream into storage | `message.part.updated normalized into storage events` |
| 6. Terminal outcome | Real turn reached a terminal state (not mocked) | `terminal=true turn_status=Some("executed") turn_error=None messages=2` |
| 7. Interrupt/abort | Native abort dispatched and acked | `abort → ack ok in 5.2656ms` (dispatched at `unix_ms=1792815689310`) |
| 8. Diff endpoint | Real validated JSON returned (empty diff is honest) | `diff = []` |
| 9. Restart recovery | Kill → probe latches unhealthy → restart (same data dir) → adapter reconnects and re-lists the session | `dead runtime latched unhealthy (… os error 10061)` → `reconnected after restart; session ses_e93dc261affejMuawBFB3Txds1 re-listed from persisted state` |

## What this proves (and what it does not)

- **Proves (PRD §25 Phase-3 exit):** a real OpenCode session controlled end to end through the Rust companion — attach, agent map, session create, prompt round-trip, SSE→storage normalization, terminal outcome (`executed`, not a mocked ack), native abort (~5 ms idle/mid-turn), diff endpoint, and restart recovery (kill → unhealthy latch → reconnect → re-list from persisted state). No mock success responses anywhere in the path.
- **Does not prove:** a *healthy long turn* abort and a *populated* diff (the run returned an empty diff and aborted an already-terminal turn) — these remain the two cheap, bounded ⚠️ checks recorded in `docs/reverse-engineering/opencode.md` §11, re-runnable when a healthy long turn and an asking policy are available. They are release-gate items, not design unknowns.
- **Provider caveat:** the terminal outcome was a healthy `executed` turn; the correlated failure channel (provider HTTP 426 / FK-skew, G4.1) is covered separately by the adapter fault-injection conformance suite, not by this healthy-run e2e.

**Relation:** complements `docs/reverse-engineering/opencode.md` §11 (Phase-0 contract evidence) with the Phase-3 companion-driven live run; adapter safety rules from `PHASE1-GATE-REQUIREMENTS.md` G4.
