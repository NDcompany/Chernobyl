# ADR-014: Revocation semantics for live sessions (D4) — decision memo

**Status:** Accepted 2026-10-25 (owner delegated ratification)
**Date:** 2026-10-23 (ratified 2026-10-25)
**Relates to:** threat model §3.6 (exposure), §5 D4, gate S8, README:42

## Current behavior (pre-D4 implementation)

- `companion revoke <device-id>` (`main.rs:374-399`) opens the journal from a
  **separate process**, marks the device `revoked`, persists. A *running* server
  holds its own in-memory store loaded at boot and never re-reads the journal.
- Consequence (proven by interop scenario 7, `tools/e2e-interop.mjs:312-335`):
  revocation took effect **at the next server restart**, not at the next handshake.
  A live session of a revoked device was never terminated — there was no
  registry→connection watch. Evidence: `revoking_a_live_device_blocks_reconnect`
  (`server.rs:1534-1551`) mutated the in-memory store directly (same-process path);
  the cross-process CLI path only applied at restart.

## Exposure (addressed by D4 ratification)

A lost/stolen phone whose session is still open keeps command access until:

1. the session drops naturally (heartbeat: server pings every `HEARTBEAT_MS` = 15 s,
   drops after `HEARTBEAT_MISS_LIMIT` = 3 misses ≈ 45 s of a silent peer), **or**
2. the operator restarts the companion, **or**
3. the operator revokes *and* the session dies for an unrelated reason.

An **active** attacker holding the unlocked phone could keep the session alive
indefinitely by answering heartbeats — revocation did nothing to it until restart.
This was the real gap: revocation is the canonical lost-phone control, and it was
inert against a live session.

## Resolution: terminate-on-revoke via in-process diagnostics route

The recommendation was ratified by the product owner on 2026-10-25. The implemented
design adheres strictly to the constraints outlined below:

- **No journal tail-watcher:** The companion remains the single journal writer while running.
- **In-process route on loopback diagnostics listener only:**
  `POST /devices/revoke` with JSON body `{"deviceId":"<id>"}` (reuse of capped-body
  POST handling used by Claude-hook ingress).
  - Any request carrying an `Origin` header is rejected with **403 Forbidden** (CSWSH
    protection, same posture as D1).
  - The route is absent on the main listener (**404 Not Found**).
  - Responses: `200 {"revoked":true,"terminated":N}`, `404` unknown device, `400` malformed body.
- **Terminate live sessions with WebSocket close code 4003:**
  The running companion marks the device `revoked` in its own store (persisted through its
  journal), appends an audit record (`append_audit("revoke", ...)`), and terminates every
  live connection whose E2E session belongs to that device:
  - Stores `device_id` alongside the session in `LiveSession` when the handshake completes.
  - Sends a WebSocket close frame with code **4003** and reason `"device revoked"`.
  - Tears down the connection through the existing RAII guard (`ConnGuard` from audit P1),
    ensuring idempotent writer shutdown and registry cleanup.
- **Future handshakes:** Refused closed as today (`hs_err device revoked`).
- **CLI `--revoke <id>` routing:**
  Tries the running companion first by POSTing to `http://127.0.0.1:<diagnostics_port>/devices/revoke`
  using the crate's std-only HTTP client (`src/client.rs`). If connection is refused/unreachable,
  it falls back to the offline journal write (restart-scoped, unchanged) and reports which path
  was used. If the companion returns 404, it reports unknown device and exits 1.

## Evidence

- `server::e2e_tests::revocation_route_terminates_live_session_leaves_others_intact_and_blocks_reconnect`:
  Paired device with live session revoked via route receives close 4003, removed from registry,
  other devices' sessions untouched, reconnect refused.
- `server::e2e_tests::revocation_route_rejects_origin_header_with_403`:
  Origin header triggers 403 refusal without mutating device state.
- `server::e2e_tests::revocation_route_is_absent_on_main_listener`:
  POST /devices/revoke on main listener returns 404.
- `server::e2e_tests::revocation_route_returns_404_for_unknown_device_and_400_for_malformed_body`:
  404 on unknown device id, 400 on malformed/missing body.
- `main::tests::cli_revoke_offline_falls_back_and_online_routes_to_running_companion`:
  CLI `--revoke` exercises both running-companion POST and offline-journal fallback.
