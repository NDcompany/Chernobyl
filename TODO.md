# CHERNOBYL — TODO

Updated: 2026-11-04 (ADR-015 live agent sessions implemented on main: Claude Code driven sessions live-verified against Haiku, OpenCode rich parts live-verified on 1.19.4 + local Ollama, Codex `limited` against a fake app-server only; pairing QR replay fixed by device key rotation, emulator row 2.2 passes; checklist 30 of 31 rows run on the emulator, all pass; soak built into the binary; the owner's logo everywhere; emulator rows 9.1-9.13 not run yet). Before that, 2026-10-25 night (emulator validation of the release APK: 14 checklist rows pass; 95-agent independent security review, 22 findings fixed — `docs/security/INDEPENDENT-REVIEW-2026-10-25.md`; Android foreground service built and device-tested; simulated-data provenance; D1 app-Origin allow-list; cleartext ws:// in release). Before that, 2026-10-25 late (reliability mandate closed: audit P1–P10 + extra findings, D4 live revoke, D9 option B, pairing-while-running fix, Phase 6 camera QR / host records / outcome honesty, Phase 9 gate assessed in `docs/validation/RELEASE-GATE.md`). Earlier: 24 h soak for S11 **PASSED 2026-10-24** — all 10 criteria, `pass: true`; Phase 8 complete: TUI + pairing overlay + windowless autostart + packaging ADR-007; client read-timeout cross-handle fix). Status of the Rust companion rewrite + everything left.

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

### Phase 4′ — pairing + E2E session layer + revocation (committed `3dc75be`, `63b206f`, `4a6e42f`, `6c55420`, this checkpoint)

- [x] **Crypto primitives** (`3dc75be`, `crypto.rs`): std-only SHA-256, HMAC-SHA-256, HKDF,
      ChaCha20, Poly1305, ChaCha20-Poly1305 AEAD, constant-time eq — verified against official
      known-answer tests (RFC 4231/5869/8439), not remembered vectors. OS CSPRNG via
      `BCryptGenRandom` (fails closed).
- [x] **QR encoder** (`63b206f`, `qr.rs`): std-only Model-2 byte-mode encoder, EC level M,
      versions 1–10, terminal half-block ANSI rendering (4-module quiet zone). Frozen
      byte-mode reference matrices from the trusted `qrcode` lib (installed only in Temp,
      outside the repo) — a format-info placement bug (row/col swap) was found and fixed
      against them.
- [x] **Pairing + device registry + CLI** (`4a6e42f`): `PairingOffer` (single-use, 5-min TTL,
      rate-limited) issuing a per-device 256-bit PSK + challenge; `companion pair` persists the
      offer to the shared journal and prints its terminal QR; the PSK is DPAPI-protected at
      rest (`CryptProtectData`) so the journal never holds plaintext. Storage gains `pairing`
      (pending) and `device` (registry) record kinds with compaction round-trip.
- [x] **E2E session layer** (`6c55420`, `e2e.rs` + `server.rs`): `hs_hello → hs_challenge
      (fresh server nonce+salt) → hs_proof → hs_ok` handshake; the offer is redeemed on first
      connect (single-use consume + device registration) or a paired device reconnects; after
      `hs_ok` every frame is ChaCha20-Poly1305-sealed (HKDF directional keys, counter nonce) and
      replay / desync / tamper tears the connection down. Handshake messages are new
      `ClientMsg`/`ServerMsg` variants; the `?token=` loopback path is untouched (differential
      parity preserved).
- [x] **Revocation** (this commit, `main.rs`): `companion devices` lists the registry;
      `companion revoke <device-id>` flips the persisted `revoked` flag — the handshake refuses
      revoked devices (proved by the `revoking_a_live_device_blocks_reconnect` test).
- [x] **Security matrix** (ADR-011 §Future, all green, 129 `cargo test`): happy path seals/
      unseals, frame replay, tampered frame, bad proof (MITM without PSK), unknown device,
      expired QR, spent-QR reuse, revoked device, paired-device reconnect, live revocation —
      plus 8 `e2e.rs` unit tests (directional keys, proof binding, replay/desync/tamper,
      garbage envelopes). Full gates: `verify:rust` exit 0 (129 tests, clippy `-D warnings`,
      fmt, `--self-test`), `npm run verify` exit 0.
- [x] **Honest gaps kept**: no X25519 forward secrecy (PSK + fresh nonces; post-compromise
      secrecy explicitly not claimed — documented in `e2e.rs`); the manual phone-scan gate
      (physical QR scan with a real phone) is still owed at the end of the phase; no
      independent security audit.

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
- [x] Soak test harness — **built 2026-10-23** (`services/companion-rs/tools/soak.mjs`,
      committed `032257d`). Two modes: `npm run soak:smoke` (~80 s, 10 predefined pass/fail
      criteria) and `npm run soak:long -- --duration <s>` (explicit long/24-hour mode).
      2026-10-26: moved into the binary as `chernobyl-companion --soak smoke|<seconds>`
      (`src/soak.rs`); the npm scripts now run it.
      Emits JSONL telemetry + a machine-readable `summary.json`; threshold provenance is
      labelled (server-derived limits vs newly proposed RSS/timeout bounds); a fault-injection
      mode (`--inject server-kill`) proves the harness detects an unexpected exit (exit 1)
      instead of passing blindly. **Smoke: 10/10 PASS.** A **1-hour long run** on the current
      binary passed all 10 criteria (exit 0): 13 936 connects / 0 failures, 233 000 pongs in
      order (0 violations), D8 saturation teardowns ~517–559 ms across 130 cycles, dead-peer
      released at 60 s, RSS growth 5.1→9.9 MB (peak 10.5 MB) over the hour, graceful exit 0,
      restart 2/2, peers released to 0. The **genuine 24 h run for gate S11 PASSED
      2026-10-24** (`--duration 86400`, window 2026-10-23T15:06Z → 2026-10-24T15:07Z,
      `summary.json` `pass: true`, `failures: []`): 5 448 800/5 448 800 pongs in order
      (0 violations), 0 unexpected exits, 0 connection failures, 9 unauthorized attempts
      refused (8× 4001, 1× 429), D8 3 027 teardowns (min 513 / p50 533 / p95 1 073 /
      max 4 336 ms, all within bound), graceful exit 0, restart 2/2, RSS 5.14 → 8.12 MB
      (peak 11.18 MB). Artifacts `target/soak/2026-10-23T19-55Z-long-24h-restart/`.
      The soak is gate **evidence** only — resilience lives in the production code.

### Phase map (PRD §25; Phase 3 was in memory's gate list, approved this round)

- [x] Phase 3: OpenCode adapter — **done 2026-10-21** (Steps 1–3: `70e343f`, `ec86262`, live-e2e
      round): attach-only adapter, probe rules honored (temp-cwd + `XDG_DATA_HOME` isolation,
      never a second server on a shared DB), `session.error`/`MessageAbortedError` failure
      channels, content-type validation, G2 two-phase permission routing, merged snapshot,
      probe loop, restart recovery. Gated live e2e (`cargo test --test opencode_e2e --
      --ignored --nocapture`, real `opencode serve` 1.17.7): attach → agent/session lists →
      create → prompt 204 → live SSE normalization → interrupt **ack ok in 4.08 s** → diff →
      kill→unhealthy→restart→reconnect with re-listed session. Found+fixed: the native abort
      answer can arrive complete *without* the socket closing (old read-to-end client =
      false 15 s timeout; `client.rs` now frames by Content-Length/chunked, 3 regression
      tests, `docs/reverse-engineering/evidence/abort-no-fin-diagnostic.txt`). Honest gaps:
      "abort stops a healthy long generation" unproven (provider 426s every live turn,
      opencode.md §8); permission asked→reply live cycle unproven (fake-server tests prove
      contract wiring only).
- [x] Phase 4′: pairing + identity + **E2E session layer** (ADR-002/006/011) — **done
      2026-10-22** (5 commits `3dc75be`…this): `companion pair` prints a single-use, short-lived
      terminal QR (hostId + endpoint + per-device 256-bit PSK + challenge, hand-rolled QR
      encoder); PSK DPAPI-protected at rest; `hs_hello→hs_challenge→hs_proof→hs_ok` handshake
      (fresh server nonce, mutual HMAC proof); every frame thereafter ChaCha20-Poly1305 AEAD
      (HKDF-SHA-256 directional keys, monotonic counters; all crypto std-only with official
      RFC/NIST known-answer tests); `companion devices` / `companion revoke <id>` registry +
      revocation. **Security matrix green** (129 tests): expired/reused QR, revoked device, bad
      proof (MITM without PSK), frame replay, counter desync, tampered frame, paired-device
      reconnect, live revocation. Still owed: manual phone-scan gate with a real phone.
- [~] Phase 5′: **transport-agnostic remote access** (ADR-011) — **posture + multi-endpoint
      done** (decisions D1/D2/D6 implemented + tested, `d12bf9e`; D8 outbound-queue bound,
      `24f8020`; soak harness, `032257d`; **S10 multi-endpoint + ordered reconnect**,
      `c5e77c1` — repeatable `--endpoint` → QR `ep` in CLI order, client ordered failover,
      threat-model §3.9 review + S10 gate ✅ `74ce7f3`). Remaining transport decisions:
      **D3 re-scheduled** with interim statement (ADR-013, S7 ✅ — token = connection-admission
      only post-ADR-011; ticket flow deferred to the web-app milestone); **D4
      recommendation only** (ADR-014, S8 ⛔ — terminate-on-revoke via in-process revoke, needs
      product-owner ratification; not implemented, current restart-scoped semantics + exposure
      documented). Optional-transport how-to: `docs/REMOTE-ACCESS.md` (2026-10-25); D4 implemented (`c1c98c7`).
- [~] Phase 6: mobile integration — **client + wiring done 2026-10-23** (ADR-012 `e7eec9d`,
      QR `dev` contract fix `4e9ff07`, `@chernobyl/e2e` + interop `e6f68b4`, app wiring
      `d5f828e`): crypto decision (pure-JS `@noble/*`, no build-model change), the real
      client (strict pairing parse → `hs_*` mutual handshake → sealed frames → reconnecting
      transport with visible handshaking/denied states), keystore pairing storage,
      pairing-first transport boot + epoch rebuild, QR paste-pairing UI with honest failure
      states, badge/caption reactivity. Verified in layers: 27 unit tests (RFC KATs exact vs
      the Rust suite) + **13/13 interop vs the real Rust binary** (`npm run verify:e2e`:
      dev redemption, wrong-PSK denial, sealed hello_ack, heartbeat, replay/tamper teardown,
      two devices, restart recovery, revocation-at-restart semantics). **2026-10-25:** camera QR scanning (`bdfe6e5`), paired-host record with endpoint list + honest
      uncertain-outcome states (`37f6056`), pair-or-demo entry replacing the fake sign-in (C14), unpair
      in Settings, foreground re-dial. Still owed: on-device rows (physical device —
      `docs/validation/PHONE-VALIDATION-CHECKLIST.md`) and a native build of the Android foreground
      service (`apps/mobile/modules/connection-service`, written 2026-10-25; Kotlin compiles, no
      APK yet; checklist rows 8.5 and 8.6).
- [x] Phase 7: adapters — **done 2026-10-23**. Ollama (`04117ee`) + Claude Code (`b0bf067`) as Rust
      modules; **Hermes deferred** per ADR-008 (not installed — no pretend capability claims; the
      adapter SDK keeps unsupported adapters invisible rather than broken). Ollama is a model-runtime
      provider (attach-only HTTP: version/tags/ps; session ops honestly `unavailable` — PRD §7.7).
      Claude Code is a hook **ingress** (loopback diagnostics POST, GET-only upgrades preserved;
      explicit install via `companion claude-hooks`, never an auto-write to `settings.json`); only
      `SessionStart` live-verified (real 2.1.309 capture), other shapes parse defensively and report
      `limited`. PRD §7.5 "do not assume" list honored: session.list/create/prompt/interrupt +
      permission.review reported `unavailable`.
- [x] Phase 8: **TUI** + windowless autostart host + packaging (ADR-007) + backup/restore,
      log redaction, local metrics — **complete 2026-10-24.**
      (FCM push removed — replaced by Phase 6 persistent-WS notifications.)
      **Ops core (`2dccf42`):** `/metrics` on the loopback diagnostics listener
      only (Prometheus text; never on the main listener); endpoint-credential redaction at the
      root (`split_url` strips `user:pass@` from the authority so it never reaches the `Host:`
      header or error text); and a real transport fix — the main-listener POST reject path now
      drains the (capped) body before closing (Windows RST-on-unread-body race that made the
      Claude-hook test intermittently fail). `companion claude-hooks` prints the settings.json
      bridge snippet (explicit install — never auto-writes the user's settings).
      **Backup/restore + windowless autostart (`95d084f`, `39ce2ed`), packaging (`0fe365e`,
      ADR-007: published SHA-256 checksums + SmartScreen guidance).**
      **Terminal TUI (this checkpoint):** `src/tui.rs` — pure/testable core (state model,
      ANSI render, key decode, keybinds) + thin interactive loop on the in-repo WS client;
      `--tui` boots the daemon in-process and opens a loopback operator console (sessions,
      pending approvals with risk colors, selection, approve-once/reject, refresh, help,
      pairing overlay with payload + QR via the shared `pairing_block`, inert keys while the
      overlay is open). Raw console via new FFI `enable_raw_console_input` (VT input,
      restore-on-drop; falls back when stdin is not a console). While wiring it, fixed a
      real client bug: on Windows `TcpStream::try_clone` duplicates do NOT inherit socket
      options, so the read timeout set on the writer never bounded reads through the
      `BufReader` handle (dead 10 s bound in `connect`; `next_text_matching` could hang
      forever mid-match) — timeouts now set on the reading handle at all three sites.
- [~] Phase 9: release gate — **assessed 2026-10-25** (`docs/validation/RELEASE-GATE.md`): code-side
      items ✅ (security suites, revocation incl. live 4003, privacy/retention doc, backup/restore,
      credential scan, integrations doc, no mock advertised as real). **Update 2026-10-25 night:**
      the release APK builds (`scripts/build-android.mjs`) and passes 14 checklist rows on an Android
      15 emulator; independent agent review done and fixed. Still owed (external): one physical-phone
      run incl. a real camera QR scan (S4), a human third-party audit (S9), and release signing.
      **Update 2026-11-04:** 30 of 31 checklist rows (sections 1-8) run on the emulator, all pass
      (row 2.2 after the device key rotation `9e460d7`; 6.3 needs a PC reboot). The live-session
      rows 9.1-9.13 are NOT RUN.
- [x] ADR-015 live agent sessions — **implemented on main 2026-11-04**
      (`docs/decisions/ADR-015-live-agent-sessions.md`; deviations in its §10; threats T1-T11 as
      built in threat model §3.12):
  - [x] Step 1: protocol types, validators, `applyTranscript`, golden fixtures; `hello_ack`
        features `transcript.v1` (`85c54db`).
  - [x] Step 2: transcript hub (memory-only), subscriptions, backpressure (`07ba769`).
  - [x] Step 3: engine permission options (G2), `resolve_native`, 24 h runtime TTL, TS parity
        (`0a173b5`).
  - [x] Step 4: `LiveRuntime` trait and router, OpenCode behind it (`f03ef05`).
  - [x] Step 5: OpenCode rich parts (`955d865`); live tests pass on an isolated `opencode serve`
        1.19.4 with `qwen3.5:4b` at 24k context on local Ollama (`62c0aba`).
  - [x] Step 6: Claude Code driver and live runtime (`9cf8b3d`, `cf8dff9`); live round trip
        against Haiku, about $0.01.
  - [x] Step 7: Codex app-server adapter and runtime (`c83ab60`, `410d168`, `05b3481`); every
        capability `limited`, no live run.
  - [x] Step 8: MCP plumbing and journal redaction (`85c54db`, `f4e5c35`, `cf8dff9`).
  - [x] Step 9: phone conversation UI, transcript store, subscribe lifecycle, workspace picker,
        approval deep links, simulated demo transcript (`ca5ed96`, `43714df`).
  - [x] Step 10: interop scenario 11 against the fake Codex app-server (`05b3481`); no fake Claude in interop.
  - [ ] Step 11: emulator rows 9.1-9.13 (`docs/validation/PHONE-VALIDATION-CHECKLIST.md`
        section 9), NOT RUN.
  - [x] Step 12: docs (INTEGRATIONS, PRIVACY, threat model §3.12, README, this file, release
        gate, checklist section 9).

### Open items (2026-11-04)

- [ ] Emulator rows 9.1-9.13 (live sessions: Claude, OpenCode, an old APK against the new
      companion).
- [ ] Codex: `thread/resume` after the app-server exits, the 30 s interrupt-unknown timer and
      the 10-min `mcp_auth` expiry (none built; ADR-015 §10).
- [ ] A live Codex run, when someone has Codex installed and signed in; capabilities stay
      `limited` until then.
- [ ] Rare server-test flake `control_routes_refuse_a_missing_or_wrong_control_key`
      (os error 10054, connection reset); not reproducible on demand.
- [ ] One physical-phone run of the checklist (S4), incl. a real camera QR scan.
- [ ] A human third-party security audit (S9).
- [ ] Release signing: a release keystore for the APK; Windows code signing stays an ADR-007
      decision.

## Explicitly not done (honesty)

- No external/independent security audit; hardening evidence is the hostile-input test corpus
  (frames, handshake, JSON, client messages) plus the constant-time/limit checks above.
- No measured numbers for loaded/burst memory — **now measured** for all three shapes: RSS
  under sustained load (10.99 MB after 517 k persisted commands in 30 s, post-cap), a
  multi-connection burst (3.8 → 10.4 MB across 400 simultaneous connections), and the soak
  trend (**1-hour run: 5.1 → 9.9 MB, peak 10.5 MB**, 13 936 connects — and the full
  **24 h window PASSED 2026-10-24**: 5.14 → 8.12 MB, peak 11.18 MB over 24 h).
- ~~Command records have no retention cap~~ — **fixed 2026-10-21** (runtime caps + batched
  eviction + in-process compaction trigger; re-measured footprint above). Residual honest
  costs: stress throughput 17.2k/s vs 19.2k uncapped (retention selection + compaction
  rewrites), and `unresolved`-heavy states may legally sit above the cap until boot
  reconciliation marks them `unknown` (documented on `enforce_command_cap`).
- `services/host` runs as the differential peer **and** as a test suite (its 21 tests ran
  green in today's `npm run verify`); it is not a supported runtime — the Rust binary is.
