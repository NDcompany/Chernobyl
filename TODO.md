# CHERNOBYL — TODO

Updated: 2026-10-21 (Phase R checkpoint). Status of the Rust companion rewrite + everything left.

## Done

### Phases 0-2 (committed `9cf3c6a`, `c853cdb`, `286ac97`, `5789e4b`)

- Phase 0: backend PRD, ADR-001…009, conflict register, baseline.
- Phase 1: gates G1-G4 (`docs/decisions/PHASE1-GATE-REQUIREMENTS.md`).
- Phase 2: companion foundation — config, storage (Node), adapters, diagnostics, health.

### Phase R — Rust companion rewrite (`services/companion-rs/`, this checkpoint)

- [x] One std-only binary: WS protocol-v2 server + companion foundation + diagnostics.
      Empty `[dependencies]`; hand-rolled JSON (depth/dup-key/surrogate/control-char strictness),
      WS framing, SHA-1, base64, Windows FFI.
- [x] Engine ported test-for-test from `engine.ts` (plans, G1/G2, TTL 120 s, ack cache 500,
      idempotency by commandId, boot reconciliation, seq continuity C2).
- [x] Server parity with `host.ts`: 4001 post-upgrade auth (`?token=`/Bearer, constant-time,
      5-failure → 429), 4002 version gate, `/health` + `/version` + `//adapters` diagnostics,
      15 s heartbeat × 3 misses, 256 000-byte payload cap, malformed → ack "malformed message".
- [x] Storage: append-only JSONL journal, torn-tail truncation, atomic compaction, retention
      caps, latched fail-closed on write (§19.4) — logged as conflict **C15** vs PRD §16.2.
- [x] Hardening beyond parity: non-loopback bind refuses without a token (secure-by-default,
      documented deviation in ADR-010).
- [x] Gates (all measured today, all green — final numbers after the A/B fixes):
  - `cargo test`: **59 passed, 0 failed**
  - `cargo clippy --all-targets -- -D warnings`: clean
  - `cargo fmt --check`: clean
  - `--self-test` (9 end-to-end checks over real sockets, debug **and** release binary):
    **ALL CHECKS PASSED** — hello/4001/4002/create+replay-dedupe//health/garbage-frame/
    permission-decision/fan-out/seq-durability
  - live A/B vs Node host (`tools/differential.mjs --compare`): **IDENTICAL** (12/12
    observations, normalized)
  - boot RSS via `/health rssBytes`: **3.83-3.84 MB**; release exe **407 KB**
  - `npm run verify`: exit 0 (unchanged, still the repo-wide gate)
- [x] Docs: `docs/decisions/ADR-010-RUST-COMPANION.md`, conflict register C15,
      `services/companion-rs/README.md`, this TODO, `npm run verify:rust` script.
- [x] Self-test extended to **9/9 checks** (debug + release): added the two wire flows the app
      depends on — permission decision (stale revision refused fail-closed, fresh revision
      resolves + `permission_resolved` broadcast) and multi-client fan-out (a second connected
      client sees `session_created` it did not cause).
- [x] **`services/companion` (Node) removed** — the Rust binary is the only desktop runtime.
      `npm run verify` re-run green after workspace rewiring (typecheck/test/build chains no
      longer reference it; lockfile refreshed).

### Hardening round — skipped items closed (committed `9f148b6`, `1805318`, `d46e334`)

- [x] **Retention caps at runtime** (the benchmark's unbounded-growth finding, fixed):
      commands capped at `retention.commands` (default 5 000, optional field — old configs
      parse unchanged) + events now pruned **during** the run (the old cap applied only at
      boot). `unresolved` records are never evicted (replay/boot reconciliation must find
      them); eviction is batched — one O(cap) selection per ~78 commands instead of a scan
      per eviction (the first per-eviction version cost 19.2k → 6.5k cmd/s at cap; batching
      restored **17.2k/s**, p50 0.19 ms). Journal also bounded **within** one process now:
      past 64 MB of appends since last compaction it rewrites to the live set (failure is
      ignored on purpose — the append already succeeded).
  - Footprint re-measured (30 s saturated command bench, fresh home): RSS **421 MB →
    10.99 MB**, journal **364 MB → 20.34 MB**, cap held (`commandsTotal` 4 966),
    `storageFailed: false`, 517 114 commands at 17 237/s. Storage microbench with caps on:
    **18 630 ns/op** (was 17 378 uncapped — the caps cost ~1.2 µs/op).
- [x] **Multi-connection burst test** (was owed): new `differential.mjs --burst --conns N`
      opens N simultaneous connections (connect+hello+one persisted command+close), reports
      wall, per-phase p50/p95, errors, and /health RSS delta.
  - **Found a real bug on first run:** Rust **181/200** (19 refused at the TCP layer) vs
    Node 200/200 — std's `TcpListener::bind` exposes no backlog knob and queues only ~128
    handshakes; a reconnect storm (app + relay flap) is exactly that shape. Bisect: 150/150,
    100/100 clean → confirmed backlog. **Fixed** with one winsock call std doesn't provide
    (`ffi::bind_listener_backlog`: raw socket + `listen(SOMAXCONN)`, adopted via
    `from_raw_socket` so accept/shutdown behavior is unchanged; IPv6 binds fall back to std).
  - After fix: Rust **200/200 (0 errs, 205 ms)**, **400/400 (0 errs, 338 ms)**, working set
    3.8 → 10.4 MB after a 400-conn burst. Node 400/400 in 333 ms (53.5 → 65.4 MB).
- [x] **Full-path microbench pair** (the previously skipped TS-side mirror): `engine-bench.mjs
      full` = JSON.parse + validate + handle + stringify vs in-crate
      `microbench_parse_validate_handle_encode` — **Rust 5.93-6.06 µs/op, TS 2.15-2.20
      µs/op** (solo release reps). Engine-only solo: Rust 2.54-2.56 µs/op, TS 0.542-0.545
      µs/op. Storage has no TS counterpart (the Node side persists nothing — verified: the
      host holds only a client Map, no record store).
  - Updated budget: Rust wire 89 µs ≈ 6 in-process + 18.6 storage + ~64 socket/queue/lock;
    Node 32 µs ≈ 2.2 in-process + ~30 socket — the gap remains durability + transport.
  - Gotcha (now in the tool comments): `cargo test -- --ignored` runs the microbenches in
    **parallel threads** — one combined run measured the engine at 6.6 µs vs 2.55 solo.
    Run each bench with a single filter (or `--test-threads=1`).
- [x] **Old Node companion resurrected for the memory baseline** (was optional): restored
      `services/companion` from git history (`7331bac`), built, booted idle → **53.6 MB
    working set vs Rust 3.8 MB = 14.1×** (the earlier `services/host` proxy number, 53.8 MB,
    was coincidentally accurate). Removed again; `npm run verify` re-run green.
- [x] **Windows autostart** (PRD §6.3 start-on-login): `--install-autostart` /
      `--uninstall-autostart` write/remove the HKCU Run entry via `reg.exe` (std-only, user
      level, no elevation, idempotent uninstall; `--config` is carried into the entry when
      installed from a non-default config). **Live round-trip verified**: install → `reg
      query` shows the quoted path → uninstall → re-uninstall reports "not installed", exit 0
      both times. Known limitation: the console daemon appears as a console window at
      login; a windowless host is packaging work (Phase 8/ADR-007). Token stays env-only —
      the autostarted instance serves loopback without one (Node parity mode) until the
      relay phase.

## Remaining

### Phase R follow-ups

- [x] Decommission `services/companion` (Node) — **done 2026-10-21**. `services/host` remains,
      not as runtime but as the TS conformance test suite (21 tests in `npm run verify`); move
      its engine tests into `packages/protocol` and drop the host service when convenient.
- [x] Differential conformance vs the Node host — **done 2026-10-21, IDENTICAL**. New
      `services/companion-rs/tools/differential.mjs` runs a fixed 12-observation scenario
      (auth 4001, hello + full seed snapshot, malformed ack, create + event fan-out, replay
      dedupe, submit refusal text, permission stale/fresh, interrupts, 4002 version gate,
      /health) against both servers and diffs the normalized transcripts (volatile timestamps
      and generated ids stripped). First run found **three real Rust-side parity bugs**, all
      fixed at one root cause + one satellite:
      1. store-replayed acks were *fabricated* from the record (`ok = status != "failed"`),
         so refused commands replayed as success and permission/interrupt acks carried the
         wrong id fields — now the original ack outcome (ok/error/sessionId/permissionId) is
         persisted and replayed **verbatim**, incl. across restart;
      2. journal **compaction had a second, older command serializer** that silently dropped
         `ok`/`permissionId` (found while testing fix 1; shared `command_body()` now, with a
         compact-preserves-outcome test);
      3. (symptom of 1) boot reconciliation of in-flight commands now answers
         `ok:false, "reconciled at boot"` on replay instead of success.
      Re-run after fixes on fresh cold servers (fresh `CHERNOBYL_HOME` dir + restarted host —
      both sides cache by commandId): **IDENTICAL**, compare exit 0. Gates re-run green
      (`cargo test` **59**, clippy, fmt, `--self-test` 9/9, `npm run verify` exit 0).
- [x] CPU/throughput benchmark vs Node host — **done 2026-10-21, repeated (3 reps/scenario,
      fresh server per rep) + engine-only isolated on both sides** (`tools/differential.mjs
      --bench`, closed-loop saturation, 4 conns, same generator both sides, server CPU
      measured per-process; 16 logical cores; soak in background for all runs):
  - **malformed (transport hot path, 60 s, zero state):** Rust **31 742/s** (31 380–32 360),
    **0.034 CPU-s/1k**, p50 0.11 ms — Node **13 864/s** (13 747–13 970), 0.0717 CPU-s/1k,
    p50 0.25 ms → **Rust 2.3× throughput at 2.1× less CPU per message.** Rep spread ±2 %;
    initial single runs came in ~10 % higher on both sides (background drift).
  - **command (validate → engine stale-refusal → ack; Rust also persists, 30 s):**
    Node **29 803/s** (27 659–31 082), **0.032 CPU-s/1k**, p50 0.11 ms — Rust **19 194/s**
    (18 912–19 543), 0.089 CPU-s/1k, p50 0.18 ms → **Node 1.55× throughput; Rust 2.8× CPU
    per message = engine + two journal appends + record insert per command (§19.4/C2
    durability design).** Rep spread: Rust ±1.6 %, Node ±6 %.
  - **Engine-only isolation (release, 1 M unique stale decisions):** TS `MockEngine`
    **596 ns/op** (`tools/engine-bench.mjs`) vs Rust `microbench_engine_stale_decide`
    (in-crate `#[ignore]` test) **2 957 ns/op** → the TS engine is 5× faster in isolation,
    but the 2.4 µs absolute gap is ~3 % of Rust's 89 µs wire cost per command; Rust
    `microbench_storage_put_update_pair` (same harness) = **17 378 ns/op** for the two
    journal appends *(both Rust figures predate the retention-cap round — solo re-runs
    with caps on: engine 2.54-2.56 µs, storage 18.6 µs, see the hardening section)*. Rough
    budget (subtractions across runs, approximate): Rust 89 µs/msg
    ≈ 38 transport+validate + 3 engine + 17 storage + ~30 queueing/lock contention;
    Node 32 µs/msg ≈ 27 transport+validate + 0.6 engine + misc → **the gap is durability
    plus transport layer, not the engine port.**
  - Lesson (kept in the tool's comments): the first command run used `decision: "deny"`,
    which fails the protocol enum (`approve-once|approve-session|reject`) on BOTH sides —
    it measured schema-rejection, not the engine (Rust's `malformed` counter exposed it:
    counter == exact sum of both phases, `commands: 0`). Re-run with `reject` verified
    `commands == sent`, `malformed: 0`.
  - Caveats kept: one machine, soak in background during all runs (equal for both sides);
    load-generator CPU not subtracted (identical both sides); decomposition subtracts
    measurements from different runs.
- [ ] Soak test: **in progress, restarted on the current binary** — release binary on :18792
      with a fresh temp home and `--exit-after 86400000` (24 h), plus a detached 10-minute
      RSS sampler (`Temp\opencode\chernobyl-soak-rss.tsv`) and a detached *light* connection
      churn (connect+hello+close every 5 s, `--churn --light --minutes 1440`). **Honest
      caveat:** the soak was killed twice earlier (both times the running exe locked the
      file against rebuild, and both restarts postdate material changes), so the TSV holds
      partial runs with visible `uptimeMs` resets — it restarts cleanly and the final
      24 h-uninterrupted window starts whenever the binary stops changing. The churn's
      default 30-minute lifetime also expired unnoticed mid-run; it now runs the full 24 h.

### Phase map (PRD §25; Phase 3 was in memory's gate list, approved this round)

- [ ] Phase 3: OpenCode adapter — probe rules (temp-cwd + `XDG_DATA_HOME` isolation, never a
      second server on a shared DB), `session.error` as the failure channel, HTML-200
      content-type validation per the adapter-sdk contract.
- [ ] Phase 4′: pairing challenge (ADR-002), short-lived connection tickets (ADR-006).
- [ ] Phase 5′: outbound cloudflared tunnel cross-network gate (ADR-001).
- [ ] Phase 6: mobile integration — simulation badge (ADR-009), paired transport replaces
      `EXPO_PUBLIC_HOST_WS_URL` + fake sign-in.
- [ ] Phase 7: adapters in order Claude Code → Ollama → Hermes (ADR-008) — as Rust modules.
- [ ] Phase 8: FCM push from the companion (needs Firebase project) + packaging (ADR-007).
- [ ] Phase 9: release gate — no mock advertised as real (PRD §13.1).

## Explicitly not done (honesty)

- No external/independent security audit; hardening evidence is the hostile-input test corpus
  (frames, handshake, JSON, client messages) plus the constant-time/limit checks above.
- No measured numbers for loaded/burst memory — **now measured** for all three shapes: RSS
  under sustained load (10.99 MB after 517 k persisted commands in 30 s, post-cap), a
  multi-connection burst (3.8 → 10.4 MB across 400 simultaneous connections), and the 24 h
  soak trend (**pending** — see the soak item above for why the window restarts).
- ~~Command records have no retention cap~~ — **fixed 2026-10-21** (runtime caps + batched
  eviction + in-process compaction trigger; re-measured footprint above). Residual honest
  costs: stress throughput 17.2k/s vs 19.2k uncapped (retention selection + compaction
  rewrites), and `unresolved`-heavy states may legally sit above the cap until boot
  reconciliation marks them `unknown` (documented on `enforce_command_cap`).
- `services/host` runs as the differential peer **and** as a test suite (its 21 tests ran
  green in today's `npm run verify`); it is not a supported runtime — the Rust binary is.
