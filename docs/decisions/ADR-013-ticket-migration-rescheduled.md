# ADR-013: Re-schedule ADR-006 ticket migration (D3) with interim statement

**Status:** Proposed (agent-authored recommendation; product owner ratifies)
**Date:** 2026-10-23
**Relates to:** ADR-006 (supersedes its "scheduled Phase 4′" line), ADR-011, PRD §11.4, conflict C1, threat model §5 D3

## Context

ADR-006 accepted migrating `?token=` query-string auth to short-lived single-use
connection tickets, scheduled for Phase 4′. It was never implemented (zero `ticket`
matches in `*.rs`). At the time ADR-006 was written the long-lived query token *was*
the command credential, so its URL leakage (proxies, history, logs) was a direct
command-execution risk.

ADR-011 + D2 changed that: beyond loopback, a stolen token no longer buys command
execution. Commands are refused until the PSK-derived E2E session exists
(`server.rs:754-772`, test `remote_posture_gates_commands_until_the_session_is_handshaken`
`server.rs:1208-1251`), and `/health` joins the token gate when bound beyond loopback
with a token configured (D6, `server.rs:251-261`). The token is now a
*connection-admission* credential, not an authorization credential.

## Decision (recommendation)

**Formally re-schedule ADR-006 to the web-app milestone (ADR-011 §5) with the interim
statement below.** Rationale:

- The core risk ADR-006 addressed is largely neutralized by the E2E session gate.
  A leaked query token now admits a socket and counters-only `/health`; every command
  and state mutation still requires the PSK handshake.
- Ticket issuance needs an authenticated HTTP round trip bound to (device, host,
  audience, expiry) and a single-use store — that machinery is best built together
  with the web app, which is the first client that genuinely needs a browser-safe
  credential flow (browsers cannot set WS headers, so a ticket is the natural fit).
- Deferring is the conservative direction: no code change, no wire change, fully
  reversible by implementing ADR-006 unchanged.

## Interim statement (in force until the web-app milestone)

The long-lived query token remains the production connection-admission credential
(PRD §11.4, C1 open). Posture that makes this acceptable:

- Beyond loopback the token gates WS admission + `/health` only; command execution
  and state mutation require the PSK-derived E2E session (D2, ADR-011).
- The PSK itself never travels in a URL — it is derived into AEAD frames only.
- Residual accepted risk: a leaked token permits connection-spam / DoS-adjacent
  pressure against the WS listener and counters-only `/health` reads, but not
  command execution or state reads, on a remote-posture bind.

## Consequences

- S7 flips to ✅ on this memo (gate wording: "implemented **or** re-scheduled with
  interim statement"), documentation-only.
- ADR-006 stays `Accepted` for its decision; its "scheduled Phase 4′" timing is
  superseded by this re-scheduling.
- When the web-app milestone lands, ADR-006's ticket flow is implemented there and
  this interim statement is retired.
