# CHERNOBYL — Conflict Register (current implementation vs backend PRD)

**Origin:** Phase 0 planning review, 2026-10-21. Migrated from the planning conversation into durable records as required by PRD §27-7.
**Status legend:** `decided` = resolved by an ADR, implementation timed; `open` = must be fixed in the listed phase.

| # | Conflict | Resolution | Status |
|---|---|---|---|
| C1 | WS auth uses a **long-lived token in the query string** (`?token=`) — PRD §11.4 forbids this for production | Short-lived single-use connection tickets in Phase 4′ — ADR-006 | decided (impl Phase 4′) |
| C2 | State is a **single snapshot with no event sequences, cursors or replay** — PRD §14 envelope required | Protocol v2: `HostEvent` envelope + per-host sequence + replay/resync, mock engine migrated in same change — Phase 1 | open (Phase 1 landed `EventEnvelope {seq,eventId,occurredAt}` + `lastSeq` gap detection on every event; **Phase 2 landed persistence**: `event_cursors.host_seq` advances transactionally with `events` rows, so a restart never resets the sequence — `readEventsAfter(cursor)` is the replay primitive; the full `connection.resync_required` wire flow lands with the gateway/relay) |
| C3 | **Mock permissions/results could be mistaken for real** once companion data exists — PRD §13.1, Phase 9 | Permanent simulation badge, one mode active at a time, streams never merged — ADR-009; UI work Phase 6 | decided (UI Phase 6) |
| C4 | "Connect host" modal **simulates enrollment** by injecting a host into the dashboard | Replaced by real pairing-challenge entry in Phase 6; simulation survives only in demo mode — ADR-002/ADR-009 | open (Phase 6) |
| C5 | **Audit history is device-local SQLite only** — PRD wants durable host-side records | Companion SQLite `audit_entries` becomes source of truth (Phase 2); phone keeps a synced view; cloud audit table voided by ADR-001 | open (Phase 2) |
| C6 | **Notifications are local-only** while app is open — PRD §17 requires remote push | FCM push sent **directly from the companion** (no cloud worker) — ADR-001; needs a Firebase project from the product owner — Phase 8 | decided (impl Phase 8, external dependency) |
| C7 | PRD §5.2 tree would create ~10 packages/services **up front**, most empty | Create each workspace only when its phase needs it — ADR-004 | decided |
| C8 | `services/host` is a **standalone server**; PRD §5.1 wants it as a reusable core under the companion | Extract connection/auth/dedupe/heartbeat core in Phase 2 behind the existing 14 tests; companion wraps it; gateway = that core | open (Phase 2) |
| C9 | PRD schedules a **Hermes adapter**, but Hermes is not installed on this machine | Deferred until installed; Phase 7 order Claude Code → Ollama → Hermes — ADR-008 | decided (revisit when installed) |
| C10 | PRD mandates **PostgreSQL** for cloud metadata; no Docker/psql on this machine and ADR-001 removes the cloud | SQLite everywhere — ADR-003 (PRD §16.1 superseded) | decided |
| C11 | **No deployment target** exists for the PRD's "different networks" proof | Outbound cloudflared tunnel (≈50MB, no account) for the Phase 5′ cross-network gate — ADR-001; production hosting deferred | decided (impl Phase 5′) |
| C12 | PRD wants **signed installers**; no code-signing certificate exists | Unsigned installer + published SHA-256 checksums + docs — ADR-007 | decided |
| C13 | PRD §22.6 E2E implies **device testing**; no Android SDK on this machine | Playwright-driven E2E on the mobile web build + explicitly documented manual on-device checks — ADR-005 | decided |
| C14 | Mobile **sign-in is fake** (mock auth, no credential check) and transport selection is the env var `EXPO_PUBLIC_HOST_WS_URL` | Phase 6: pairing-based device credential replaces fake sign-in; transport = paired host list (URL + credential); demo mode remains an explicit choice — ADR-002/ADR-009 | open (Phase 6) |

## Rules carried forward from the register

- `npm run verify` must stay green after every phase (baseline: `docs/architecture/PHASE0_BASELINE.md`).
- Mock mode keeps working through all fixes (ADR-009).
- No phase may claim a capability it has not demonstrated with real evidence (PRD §27).
