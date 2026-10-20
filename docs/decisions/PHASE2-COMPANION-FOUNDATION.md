# Phase 2 — Companion foundation (gate record)

Source: PRD §25 "Phase 2 — Companion foundation". This document maps every
deliverable and exit criterion to its implementation and test evidence, in
the same style as `PHASE1-GATE-REQUIREMENTS.md`.

Component: `services/companion` (`@chernobyl/companion`), reusing
`@chernobyl/adapter-sdk`, `@chernobyl/domain`, `@chernobyl/protocol`.
Persistence uses Node's built-in `node:sqlite` — **zero new dependencies**
(no native builds, no server installs; ADR/disk-space constraints hold).

## Deliverables → evidence

| Deliverable | Where | Evidence |
|---|---|---|
| Background process entry point | `src/main.ts` (`chernobyl-companion`) | `main.test.ts` spawns the built process, proves it serves `/health` with no mobile app attached and exits 0 through the graceful-shutdown routine |
| Configuration management | `src/config.ts` | `config.test.ts`: defaults created on first run; invalid values refused with actionable messages; a config written by a **newer** companion is refused (never guessed at); `CHERNOBYL_HOME` relocation |
| SQLite persistence | `src/storage.ts` | `storage.test.ts`: all 12 PRD §16.2 tables, WAL, idempotent commands by commandId, event retention (hard row cap on append + age sweep at open) |
| Adapter lifecycle manager | `src/adapters.ts` | `companion.test.ts`: isolated per-adapter handlers, `degraded` state with bounded jittered backoff probe, recovery to `healthy` |
| Discovery interface | `discoverAdapters(config)` | factory map (builtin `mock`); an unsupported type is **recorded** as `discovery-failed`, never silently ignored; disabled entries skipped |
| Health and diagnostics | `src/diagnostics.ts`, `health()` | loopback-only (`127.0.0.1`, PRD §15.2), `GET /health /version /adapters`; resource usage measured |
| Graceful shutdown + restart recovery | `src/main.ts`, `storage.ts` | audit `companion started/stopped` survives shutdown; boot reconciliation runs **before** new work is accepted |

## Exit criteria

- [x] **Companion runs without the mobile app** — `main.test.ts`: spawned
      `node dist/main.js` standalone, fetched `/health`, no phone/WS client
      involved anywhere in its lifecycle.
- [x] **Adapter failure does not crash the companion** — isolation test: an
      adapter whose `health()/send()/dispose()` all throw is reported
      `degraded`; the healthy sibling keeps executing commands; explicit
      commands to the dead adapter refuse fail-closed.
- [x] **Commands and event cursors survive restarts** — restart test: same
      data dir, new process → duplicate commandId returns stored state and
      the adapter is **not** re-executed; sequence continues from the
      persisted cursor (`event_cursors.host_seq`, appended transactionally
      with the `events` rows — PRD §14.3, conflict **C2** remainder).
- [x] **Resource usage is measured** — `health().resources` reports RSS,
      heap, CPU ticks and uptime. Measured at boot with the mock adapter on
      this machine: **RSS 56.9 MB, heap 5.9 MB, CPU 78 ms** to first health.

## Reliability rules implemented here (PRD §19)

- **§19.1 restart:** unresolved commands (`created/queued/accepted/running`)
  reconcile to `unknown` with an explicit error — never blindly repeated (G1).
- **§19.2 approvals:** pending approvals past their native deadline
  reconcile to `stale` — never auto-applied.
- **§19.4 fail-closed:** a latched durable-write failure refuses every
  subsequent consequential command *before* routing or execution; health
  reports it. (`When durable state cannot be written, fail closed.`)
- **§10.4 persist-before-execute:** commands are inserted before the adapter
  runs them; duplicates return the stored record.

## Carried forward (later phases, not gaps in this gate)

- **Session ↔ adapter routing:** `routeAdapter()` only auto-routes when
  exactly one healthy adapter exists; routing tables land with real
  adapters (Phase 3).
- **Replay full loop (`connection.resync_required`, PRD §14.4):** the
  storage primitive (`readEventsAfter(cursor)`) and persisted cursor exist
  now; the wire flow needs the gateway/relay (Phase 4′/5′).
- **Diagnostics are read-only;** config changes go through the config file +
  restart until the local API phase (PRD §15.2 permits, does not require).
- `adapter_instances` rows are written on status transitions;
  `sessions/native_entity_mappings/workspace_registry/notification_outbox`
  tables are created at v1 migration but populated by their own phases.
