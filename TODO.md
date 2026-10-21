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
- [x] Gates (all measured today, all green):
  - `cargo test`: **57 passed, 0 failed**
  - `cargo clippy --all-targets -- -D warnings`: clean
  - `cargo fmt --check`: clean
  - `--self-test` (9 end-to-end checks over real sockets, debug **and** release binary):
    **ALL CHECKS PASSED** — hello/4001/4002/create+replay-dedupe//health/garbage-frame/
    permission-decision/fan-out/seq-durability
  - boot RSS via `/health rssBytes`: **3.83 MB** (4 018 176 bytes); release exe **407 KB**
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
- [ ] Differential conformance: run the same client script against the Node host and the Rust
      binary and diff the wire traffic (`--self-test` is that script for the Rust side; pointing
      it at the Node host is the delta check). Parity is currently proven by ported unit tests,
      not by a live A/B.
- [ ] Soak test: 24 h idle RSS stability + a multi-connection burst test (not run).
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
- No measured numbers for loaded/burst memory — only boot Rust RSS (3.83 MB).
- `services/host` is exercised only as a test suite (its 21 tests ran green in today's
  `npm run verify`); the Node host server itself was not run as a runtime this checkpoint.
