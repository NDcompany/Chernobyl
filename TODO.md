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
- [x] CPU/throughput benchmark vs Node host — **done 2026-10-21** (`tools/differential.mjs
      --bench`, closed-loop saturation, 4 conns, same generator both sides, server CPU
      measured per-process; 16 logical cores; soak running in background during all runs):
  - **malformed (transport hot path, 60 s, zero state growth):** Rust **36 303/s**, 0.033
    CPU-s/1k msgs, p50 0.10 ms — Node **15 319/s**, 0.066 CPU-s/1k, p50 0.24 ms →
    **Rust 2.4× throughput at ½ the CPU per message** (Node pays V8 exception cost on
    every unparseable frame; Rust's non-throwing parser doesn't).
  - **command (validate → engine stale-refusal → ack; Rust additionally persists
    persist-before-execute, 30 s):** Rust **18 532/s**, 0.091 CPU-s/1k, p50 0.19 ms, journal
    364 MB, RSS 421 MB — Node **32 169/s**, 0.030 CPU-s/1k, p50 0.11 ms, RSS 181 MB →
    **Node 1.7× faster here because it persists nothing**: Rust's 3× CPU/msg is engine +
    two journal appends + record map per command (the §19.4/C2 durability design).
  - Lesson (kept in the tool's comments): the first command run used `decision: "deny"`,
    which fails the protocol enum (`approve-once|approve-session|reject`) on BOTH sides —
    it measured schema-rejection, not the engine (Rust's `malformed` counter exposed it:
    counter == exact sum of both phases, `commands: 0`). Re-run with `reject` verified
    `commands == sent`, `malformed: 0`.
  - Honesty: single run per scenario, no repeats/variance; CPU of the shared load generator
    not subtracted (identical for both sides); Rust's engine-only cost not separately
    isolated (every protocol command is mutating → always persisted on Rust).
- [ ] Soak test: **started 2026-10-21, in progress** — release binary on :18792 with a fresh
      temp home and `--exit-after 86400000` (24 h), boot RSS 4 030 464 B (3.84 MB), plus a
      detached 10-minute RSS sampler (`Temp\opencode\chernobyl-soak-rss.tsv`) and a detached
      *light* connection churn (connect+hello+close every 5 s, `--churn --light`) to exercise
      accept/teardown. Survives until reboot or 24 h; numbers land in the sampler file.
      Still owed after it finishes: multi-connection **burst** test (many clients at once).
- [ ] Windows autostart/service wrapper (current build is a console daemon with
      Ctrl+C/CTRL_CLOSE graceful shutdown).

### Phase map (unchanged, PRD §25)

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
- No measured numbers for loaded/burst memory — **superseded for the two bench scenarios
  above** (RSS under sustained load is now measured: Rust 4.4 MB transport-only, 421 MB
  after 556 k persisted commands in 30 s); the 24 h soak trend and a multi-connection
  burst are still pending.
- **Command records have no retention cap** (events cap at 5 000; commands don't) — found
  by the benchmark: an authorized client sustaining mutating commands grows memory/journal
  unbounded (421 MB RSS + 364 MB journal in 30 s at 18.5 k cmd/s). Irrelevant at real
  rates (a few commands/minute) but a hardening item before Phase 9.
- `services/host` runs as the differential peer **and** as a test suite (its 21 tests ran
  green in today's `npm run verify`); it is not a supported runtime — the Rust binary is.
