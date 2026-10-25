# CHERNOBYL — Reliability & Resilience Audit (mandate §1)

**Date:** 2026-10-25 · **Tree:** `e683fb3` (24 h soak evidence committed) · **Method:** direct
end-to-end reading of the production tree — `server.rs`, `companion.rs`, `storage.rs`,
`engine.rs`, `opencode.rs`, `ollama.rs`, `claude.rs`, `tui.rs`, `main.rs`, `client.rs`,
`packages/e2e/src/transport.ts`. Subagents were unavailable (usage limit), so every finding
below was read in place, not inferred. Line cites are to this tree.

**Scope rule held throughout:** the soak harness (`tools/soak.mjs`) is a *verification
tool*. Nothing in this audit adds soak code, soak UI, or a production dependency on a soak
run; passing a soak is never equated with being resilient. Every finding is about the
shipped companion/protocol/client code.

**Disposition convention:** each finding states the failure scenario and the required
production behavior. Status is updated once the fix lands (§12 verification per commit).

---

## 1. Findings, prioritized by production impact

### P1 — Connection teardown is not unwind-safe (§2 lifecycle) · Status: fix committed (`6dc66bd`)

- **Where:** `server.rs:716-991`. Teardown is inline code after the read loop
  (`server.rs:967-975`): unregister from `clients`, `writer_stop.store(true)`, join the
  `ws-writer` thread. Registration happens earlier at `server.rs:752-757`.
- **Failure scenario:** any panic on this thread *after* registration — a poisoned
  `clients`/`session`/`engine` mutex (`.lock().unwrap()` at `server.rs:756`, `:971`, or any
  lock inside `handle_client_msg`) or an internal bug — unwinds straight past the inline
  teardown. Result: the registry entry stays forever (`clients` grows on every subsequent
  panic, visible only as memory), the `ws-writer` thread blocks on an empty channel for the
  life of the process, and the peer's socket is left half-open with no close frame. The
  comment at `server.rs:722-725` shows the author guarded *`?`-failures* explicitly but not
  unwinds.
- **Required behavior:** teardown must run exactly once on *every* exit path — normal EOF,
  protocol violation, saturation, `?` failure, and unwind — i.e. an RAII guard whose `Drop`
  unregisters, stops and joins the writer. A panic must degrade to "this one connection
  died", never to a leaked registry entry or an orphaned writer.
- **Regression layer:** `server.rs` test — drive a connection whose handler panics, then
  assert `clients.len()` returns to baseline and no `ws-writer` thread survives.

### P2 — One global outbound bound, no per-message-class policy, no saturation visibility (§4) · Status: fix committed (`5050b1c`)

- **Where:** `server.rs:39-40` — `OUTBOUND_QUEUE_MAX_MESSAGES = 256`,
  `OUTBOUND_QUEUE_MAX_BYTES = 4 MiB`, applied as a single FIFO in `Outbound::enqueue_sealed`
  (`server.rs:628-660`). `/health` exposes `events` and `malformed` only
  (`companion.rs:474-503`); `/metrics` likewise (`companion.rs:510+`). There is **no
  saturation or shed counter anywhere in the binary** — the 24 h soak had to infer D8
  teardown behavior externally from client-side timing.
- **Failure scenario:** a burst of bulk traffic (session output lines) fills the single
  queue and the reply to a `ping` or a permission answer — the control messages that keep
  the operator's session honest — is refused at `server.rs:953` with "outbound queue
  saturated", tearing down the connection. One class starves another even though control
  traffic is tiny. When it happens in production, nothing in `/health` says *why*; the
  operator sees only a dropped client.
- **Required behavior:** per-class admission thresholds (bulk ≈ 192 msgs / 3 MiB, control
  256 msgs / 4 MiB — control keeps reserved headroom), admission evaluated **before** the
  AEAD seal so a shed frame never consumes a counter (counter order must equal wire order),
  overflow policy unchanged (teardown, never silent drop), and saturation/shed counters
  surfaced on both `/health` and `/metrics`.
- **Regression layer:** `server.rs` unit tests: control still admitted when bulk is full;
  shed frame consumes no counter; counters increment and appear in `/health`.

### P3 — Mid-file journal corruption is truncated silently (§5) · Status: fix committed (`755f264`)

- **Where:** `storage.rs:261-290` — an unparseable line causes `break` with no log, no
  counter, no audit record; `storage.rs:167-180` then truncates the file at `valid_bytes`.
- **Failure scenario:** a torn *tail* after a power cut is expected and by design
  (ADR-010), but the same code path handles mid-file corruption — including a crash in the
  middle of a compaction rename or disk rot — with byte-identical silence, and discards
  every record *after* the corrupt line. The operator is told nothing: boot looks healthy,
  `commands`/`events` counts just differ from what was written, and §5's "no silent
  corruption" is violated.
- **Required behavior:** distinguish "corrupt line at EOF (torn tail)" from "corrupt line
  with valid data after it (mid-file loss)": log the byte offset and line number to stderr,
  keep a latched counter exposed in `/health`, and record the loss in the audit log. The
  truncate-and-continue *policy* itself is unchanged (it is the documented ADR-010 policy);
  the defect is the silence, not the truncation.
- **Regression layer:** `storage.rs` test — journal with garbage in the middle, valid lines
  after it → boot succeeds, counter set, health field present, tail preserved policy
  unchanged.

### P4 — C15 durability claim does not match the code (§5 documentation) · Status: fixed (C15 wording corrected; fsync-per-append deliberately not adopted)

- **Where:** `CONFLICT_REGISTER.md` C15 says "fsync-on-append"; `storage.rs:398-416` only
  does `write_all` + `flush`, and `File::flush` is a no-op — the bytes reach the OS page
  cache, not the disk. `sync_all()` is called only inside `compact()` (`storage.rs:715`).
- **Failure scenario:** a power loss can lose recently appended records, so a command
  acknowledged as persisted may not survive; anyone reasoning from C15 would over-trust
  durability.
- **Required behavior:** correct the document to the real policy — OS-buffered append with
  fsync only at compaction, and state the resulting window. (Changing the *code* to
  fsync-per-append is a deliberate performance decision, not an audit fix — flagged for the
  owner, not made silently.)

### P5 — Reconnect loops have no jitter and never reset (§2) · Status: fixed (`46bfa9f` TS, Rust next commit)

- **Fix:** TS `retryDelayMs` (equal jitter, `[cap/2, cap]`) shared by the E2E and plaintext transports, backoff reset only after a session stayed up 30 s, and `connect()` now cancels a scheduled retry (it used to open a duplicate socket). Rust `reconnect_delay_ms`: uniform in `[250, ceiling]`, ceiling 500 → 1000 → 1250 ms (pre-P5 bounds kept), CSPRNG-seeded, attempt reset after a stream stayed up 30 s, wait sliced to the exact delay. Tests: `transport.test.ts` (3 new), `reconnect_delay_is_jittered_growing_and_bounded`.

- **Where:** `opencode.rs:896-916` — `backoff = min(backoff+1, 5); step = 250 * backoff`
  → 250…1250 ms, deterministic, and `backoff` never returns to 0 after a *healthy*
  session; `packages/e2e/src/transport.ts:173-181` — `min(15000, 1000 * 2**attempts)`,
  also deterministic.
- **Failure scenario:** after an adapter outage, every companion instance (and every e2e
  client) retries in lockstep at the same millisecond offsets — the classic thundering-herd
  pattern against a recovering `opencode serve`. Deterministic backoff also makes
  "reconnect storms" indistinguishable from ordinary load in logs.
- **Required behavior:** bounded exponential backoff **with jitter**, and the attempt
  counter reset after a connection that stayed up (a healthy session must not inherit its
  old penalty). Bounds stay exactly as they are — no unbounded growth.
- **Regression layer:** `transport.test.ts` — delay falls inside `[base, jittered cap]`,
  attempts reset after a stable connection; Rust side asserted by a pure backoff helper test.

### P6 — Shutdown never joins connection threads; the TUI's disconnect tears down the daemon (§8) · Status: fixed (`74fd902`)

- **Fix:** `ServerHandle` tracks connection threads; `stop()` shuts every live socket down and joins within 5 s, returning whether it finished; `companion.shutdown(graceful)` writes "shut down gracefully" only then, otherwise "N connection(s) still active". Connections see shutdown through a 250 ms idle `peek` poll (Windows does not wake a blocked `recv` when a cloned handle is shut down); `FrameReader` retries timeouts mid-frame so a slow frame is never split. Tests: `ordered_shutdown_joins_live_ws_and_graceful_line_is_last`, `a_frame_slower_than_the_idle_poll_is_not_split`.

- **Where:** `server.rs:194-199` collects only the accept and scheduler threads, so
  `ServerHandle::stop()` (`server.rs:82-87`) returns while `conn`/`ws-writer` threads may
  still be mid-frame; `main.rs:240-253` then calls `companion.shutdown()`, which appends
  `"companion shut down gracefully"` (`companion.rs:726-734`) and runs `compact()` while
  those threads may still append. In the `--tui` path a socket error sets
  `running = false` (`tui.rs:611-616`) and that same block stops the daemon — a transient
  read error ends the whole process.
- **Failure scenario:** a connection thread writing during `compact()` serializes correctly
  (both take the storage mutex), but a thread writing *after* `shutdown()` returns and
  during process exit can leave a torn final line (loader tolerates it) while the audit log
  already claims a graceful shutdown — an untrue record. The TUI coupling means one flaky
  socket ends a long-running companion with exit 0 and no reconnect attempt.
- **Required behavior:** ordered shutdown — stop accepting, close/drain live connections
  (bounded by the existing read/write timeouts), *then* append the audit record and
  compact; the "gracefully" audit line must only be written when that ordering actually
  completed. The TUI must show the disconnect truthfully (it already does) and must not be
  the reason the daemon dies: separate "TUI quit" from "connection lost".
- **Regression layer:** shutdown test asserting no post-shutdown appends (audit line order);
  TUI-level test that a disconnect does not stop the server.

### P7 — No bound on concurrent connections (§2) · Status: fixed (`74fd902`; numbers chosen under the owner's delegation 2026-10-25)

- **Fix:** caps per listener, main 1024 (above the 400-connection burst test) and diagnostics 64; excess accepts are closed immediately; `connectionsActive` / `connectionsRejected` on `/health` and `/metrics`; spawn failures count as rejections.

- **Where:** `server.rs:213-221` — one thread per accepted connection, no cap, and spawn
  failures are dropped with `let _ =`.
- **Failure scenario:** a local process (the default posture is loopback, but the
  non-loopback posture exists) can open thousands of idle sockets; each costs a thread plus
  a 15 s heartbeat read timeout before it is reaped — memory and fd exhaustion by an
  unauthenticated peer, i.e. an availability attack with no mitigation.
- **Required behavior:** a max-connection bound with an immediate close (and a counter) when
  exceeded. **Not implemented in this mandate without the owner's number**: the bound is a
  capacity decision, and the thread-per-connection model is ADR-010's. Recorded here as an
  accepted, documented risk instead of a silent change.

### P8 — TUI state is truthful but availability-coupled (§9) · Status: fixed (`74fd902`)

- **Fix:** a TUI socket error shows "disconnected from companion — reconnecting (attempt N)" and retries with 250 ms → 5 s backoff; only quit or Ctrl-C returns, so a flaky socket no longer ends the daemon.

- **Where:** `tui.rs:611-616` sets `state.msg = "disconnected from companion"` then ends
  the loop; `main.rs:240-253` shuts the daemon down. **No fake success exists** (§9 is
  met): the disconnect is shown verbatim. The remaining gap is that the TUI has no
  reconnect and its exit is indistinguishable from an operator quit (both exit 0).
- **Required behavior:** truthful state (present) + no daemon teardown caused solely by a
  client-side read error (fixed with P6).

### P9 — Storage outcome errors are discarded at one call site (§5) · Status: fixed (`74fd902`)

- **Fix:** the failed append is logged with the commandId and counted (`outcomeWriteFailures` on `/health` and `/metrics`); test `outcome_write_failure_increments_counter_and_surfaces_in_health_and_metrics` injects the failure.

- **Where:** `companion.rs:394` — `let _ = store.update_command_outcome(...)`.
- **Failure scenario:** if that append fails, `append` latches `failed = true`
  (`storage.rs:411-413`) so the store does fail closed afterwards — but the *in-memory*
  record keeps `status: "unresolved"`, and nothing logs that the terminal outcome was not
  recorded. The next boot reconciles it to `unknown`, which is honest but differs from the
  outcome the operator already saw.
- **Required behavior:** log the failed outcome write and expose it (the storage-failed
  latch already surfaces in `/health`); never pretend the outcome was recorded.

### P10 — Auth-failure counter is global, not per-origin (§7 observability) · Status: documented

- **Where:** `server.rs:301-321`, `companion.rs:39` — a single `auth_failures: Mutex<u32>`.
- **Assessment (not a security defect):** the 429 cooldown applies *only* to wrong-token
  attempts, and one successful auth resets it (`server.rs:321`), so a legitimate client can
  never be locked out by an attacker. The gap is attribution only: `/metrics`
  (`chernobyl_auth_failures`) cannot say *who* is failing. Per-IP tracking is a small
  change but adds a map with its own eviction problem on a loopback-by-default server;
  recorded as accepted.

---

## 2. Lifecycle coverage matrix (§1 required list)

| Lifecycle | Owner (file:lines) | Verdict |
|---|---|---|
| Process | `main.rs:120-273`, `ffi` shutdown handler | Defect P6 (shutdown ordering), config load already fail-closed |
| Client / connection | `server.rs:202-325`, `:716-991` | Defects P1 (unwind-safety), P7 (no cap); teardown correct on all *non-panicking* paths |
| Session | `engine.rs` (plan pump, TTL, ack cache 500) | No defect found: state machine driven by one scheduler, TTL expiry bounded |
| Adapter | `opencode.rs:336-359`, `:896-916`; `ollama.rs`, `claude.rs` | Defect P5 (jitter/reset); `dispose()` idempotent (`:353`), probe loop bounded, attach-only, no tight restart loop (250–1250 ms floor) |
| Command / ack | `companion.rs:243-410`, `storage.rs:452-495`, `:600-620` | **No defect** — see §3 |
| Subscription / fan-out | `server.rs:628-660` + per-conn `clients` registry | Defect P2 (bounds/visibility); seal-before-write contract holds (post-`854775f`/`477fefc`) |
| Record (persistence) | `storage.rs:141-290`, `:398-416`, `:643-724` | Defects P3 (silent truncation), P4 (false fsync claim), P9 (discarded outcome error) |
| Mobile reconnect | `packages/e2e/src/transport.ts:150-260` | Defect P5 (jitter/reset); ordered endpoint failover + terminal deny on `server-proof-mismatch` verified by interop 17/17 |

## 3. Confirmed non-defects (checked, not assumed)

1. **No auto-replay of unknown-outcome commands.** Boot reconciliation writes
   `status:"unknown"`, `ok:Some(false)`, `error:"reconciled at boot"`
   (`storage.rs:600-620`), so a restart never re-executes and never claims success; replays
   of a *completed* command return the original ack verbatim (`companion.rs:257+`).
2. **Bounded command timeouts + idempotency.** Persist-before-execute keyed by `commandId`
   (`companion.rs:246-257`), engine-side ack cache, storage-side dedupe surviving restart.
3. **Heartbeat dead-peer release.** 15 s beat × 3 misses (`protocol.rs:11-12`) reaps a
   stalled peer independently of client behavior — confirmed live by the 24 h soak
   (dead-peer released within bound; D8 teardowns p50 533 ms).
4. **Fail-closed storage.** Any journal write failure latches `failed`
   (`storage.rs:399-414`) and new consequential commands are refused
   (`companion.rs:250-255`).
5. **Adapter fault-injection vs live tests are distinct.** The fake HTTP/Ollama process
   lives under `#[cfg(test)]` (`ollama.rs:368+`); live adapter claims are gated behind
   `--ignored` tests that need a real `opencode serve`. No fake process is ever presented
   as live evidence.
6. **No new exposure.** Nothing in this audit binds a new socket, changes the `/health`
   posture, or adds a non-loopback listener.

## 4. What was deliberately *not* decided here

- **Ticket migration, crypto policy** — owner decisions, untouched. (D4 and D9 were decided
  2026-10-25 under the owner's delegation — see ADR-014 and threat model §5.)
- **P4 code change (fsync-per-append)** — a durability/throughput trade-off, deliberately not
  made. (P7's cap was set 2026-10-25 under the owner's delegation.)
- **Harness changes** — none; the soak stays a dev/CI tool (`tools/soak.mjs` untouched).

## 5. Additional findings during the fix round (2026-10-25)

- **E2E nonce layout mismatch (critical, fixed `986f4e9`).** The TS client put the counter in nonce bytes 4–11, the Rust server in bytes 0–7. They agree only at counter 0, so every paired phone session died on its second sealed frame, and the 1 s reconnect plus a single-sample interop check hid it. Golden frames at n=1 are now pinned in both suites, and the interop churn check counts every status change.
- **Duplicate socket on `connect()` during backoff (fixed `46bfa9f`).** The superseded retry timer opened a second socket and orphaned the first.
- **Close 4001/4002 not terminal (fixed `3f0968d`).** The transport retried a bad token or protocol mismatch forever.
- **Unbounded audit trails (fixed).** Desktop journal audit records and the phone's decision table now keep the newest 1000.
