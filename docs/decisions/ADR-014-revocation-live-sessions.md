# ADR-014: Revocation semantics for live sessions (D4) — decision memo

**Status:** Proposed recommendation (agent-authored; **product owner must ratify before implementation**)
**Date:** 2026-10-23
**Relates to:** threat model §3.6 (exposure), §5 D4, gate S8, README:42

## Current behavior (verified, not changed by this memo)

- `companion revoke <device-id>` (`main.rs:374-399`) opens the journal from a
  **separate process**, marks the device `revoked`, persists. A *running* server
  holds its own in-memory store loaded at boot and never re-reads the journal.
- Consequence (proven by interop scenario 7, `tools/e2e-interop.mjs:312-335`):
  revocation takes effect **at the next server restart**, not at the next handshake.
  A live session of a revoked device is never terminated — there is no
  registry→connection watch. Evidence: `revoking_a_live_device_blocks_reconnect`
  (`server.rs:1534-1551`) mutates the in-memory store directly (same-process path);
  the cross-process CLI path only applies at restart.

## Exposure (honest statement)

A lost/stolen phone whose session is still open keeps command access until:

1. the session drops naturally (heartbeat: server pings every `HEARTBEAT_MS` = 15 s,
   drops after `HEARTBEAT_MISS_LIMIT` = 3 misses ≈ 45 s of a silent peer), **or**
2. the operator restarts the companion, **or**
3. the operator revokes *and* the session dies for an unrelated reason.

An **active** attacker holding the unlocked phone can keep the session alive
indefinitely by answering heartbeats — revocation does nothing to it until restart.
This is the real gap: revocation is the canonical lost-phone control, and it is
inert against a live session.

## Recommendation: terminate-on-revoke, implemented via an in-process revoke path

Prefer **terminate**, with these design constraints (to avoid the hazards of the
naive alternative):

- **Do not** add a journal tail-watcher that re-reads a file another process
  appends to. Two processes sharing one append-mode journal already exists as a
  latent hazard (CLI `Storage::open` may truncate/compact a torn tail while the
  server holds the file open); a watcher would add concurrent-readers-on-compaction
  races on Windows (rename-over-open-handle semantics) for no security win.
- Instead: route `revoke` to the **running companion** when one is reachable (the
  CLI already binds a known port; an authenticated in-process revoke command or
  local signal). The companion marks the device revoked in its *own* store and
  enqueues a close frame (a dedicated code, e.g. 4003, documented + mirrored in
  `@chernobyl/e2e` as a terminal deny alongside 4001/4002) on the matching live
  connection, then drops the session. Fall back to the current offline journal
  write when no server is running (restart-scoped, unchanged).
- The interop assertion at scenario 7b ("revoke takes effect at restart (still
  connects live)") would be **flipped** to "revoked device's live session is
  terminated", and the frozen-contract semantics documented in ADR-011 / threat
  model §3.6 updated in the same change.

## Why this is not implemented yet (keeps S8 ⛔)

1. The threat model reserves D4 for the product owner; the standing mandate says
   never to silently change authorization/session semantics. This memo is the
   documented recommendation, not ratification.
2. It changes a frozen-contract wire semantic (adds a close code + flips an
   interop-asserted behavior) and touches the revoke CLI's process model — a
   coordinated change needing owner sign-off, not an autonomous edit.
3. The 24 h soak completed **2026-10-24** on the frozen revision `f47c4f6`
   (S11 ✅); landing a transport semantic change afterwards puts the new tree on
   an un-soaked revision, so S11's evidence would have to be re-established for
   any transport-changing build.

## If the owner instead prefers "keep next-handshake"

Document the exposure exactly as stated above (§3.6 already does) and close S8
with "decided = keep, exposure accepted" — no code change. The exposure is bounded
for a *silent* peer (≤ ~45 s) but unbounded for an *active* attacker, which is why
terminate is recommended over keep.
