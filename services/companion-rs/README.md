# chernobyl-companion — the CHERNOBYL local backend (Rust)

One standalone, zero-dependency binary (ADR-010): the protocol-v2 WebSocket server the mobile
app already speaks + the Phase-2 companion foundation + loopback diagnostics. `[dependencies]`
is intentionally empty — std only.

## Build and run

```powershell
cargo build --release                 # target\release\chernobyl-companion.exe
.\target\release\chernobyl-companion.exe            # default: 127.0.0.1:8787
.\target\release\chernobyl-companion.exe --self-test # 9-check end-to-end probe
```

Flags (env in parentheses): `--config <path>` · `--port <n>` (`PORT`, 0 = ephemeral,
default 8787) · `--bind <addr>` (default 127.0.0.1) · `--token <t>` (`HOST_TOKEN`) ·
`--diagnostics-port <n>` (default 9487) · `--exit-after <ms>` · `--self-test` ·
`--install-autostart` / `--uninstall-autostart` (HKCU Run start-on-login, user level,
idempotent; carries `--config` when given, console window at login — packaging caveat in
`TODO.md`) · `--pair` (create a pairing offer, print its terminal QR, exit) ·
`--endpoint <ws-url>` (address embedded in the QR, default `ws://bind:port`) ·
`--devices` (list the paired-device registry) · `--revoke <device-id>` (revoke a paired
device). `CHERNOBYL_HOME` selects the data directory (config, journal).

**Secure-by-default:** a non-loopback bind without `--token`/`HOST_TOKEN` is refused.
Loopback with no token keeps Node-parity behavior (dev/demo). Three posture policies layer on
top (resolved D1/D2/D6, threat model §3.4/§3.1.3/§3.7): a WS upgrade carrying a browser
`Origin` header is refused **403** (browsers unsupported until the web app, ADR-011 §5);
**beyond loopback** every command requires a live E2E session — a stolen token alone gets an
explicit `ack_err("e2e session required")`, never execution; and `/health` on the main
listener **beyond loopback** requires the token (401 otherwise). Loopback keeps the open
counters + plaintext dev parity path.

## Pairing + E2E session (ADR-002/006/011)

`companion pair` creates a single-use, 5-minute pairing offer, persists it to the shared
journal (the per-device 256-bit PSK is DPAPI-protected at rest — never plaintext on disk),
and prints a terminal QR the phone scans at close physical range. The QR payload carries the
host id, an endpoint to dial, the PSK and a challenge.

After the phone connects, a handshake (`hs_hello → hs_challenge → hs_proof → hs_ok`) proves
mutual PSK knowledge — the server answers with a fresh nonce+salt and both sides HMAC it, so
a captured proof cannot be replayed and a MITM without the PSK is rejected. A pending offer is
redeemed on first connect (single-use consume + device registration); a paired device
reconnects with its stored PSK. From `hs_ok` on, every frame is ChaCha20-Poly1305-sealed with
HKDF-SHA-256 directional keys and a monotonic counter nonce, so replay, desync and tampering
all tear the connection down. Revocation (`companion revoke <device-id>`) blocks a device at
the next handshake. All crypto is std-only with official RFC/NIST known-answer tests; no
X25519 forward secrecy yet (PSK + fresh nonces — post-compromise secrecy is not claimed).

The `?token=` loopback path is unchanged and stays for dev/differential parity. Beyond
loopback the token alone is **not** enough to run commands (D2): complete the `hs_*`
handshake first.

### E2E wire format (the contract a phone client must implement)

All frames are WebSocket **text** frames carrying JSON. Handshake first (plaintext), then a
sealed envelope per frame.

Handshake (PSK = the 32 bytes from the QR, base64):

1. C→S `{"type":"hs_hello","deviceId":"<hex>"}`
2. S→C `{"type":"hs_challenge","nonce":"<b64 16B>","salt":"<b64 16B>"}` — fresh per connection
3. C→S `{"type":"hs_proof","proof":"<b64 32B>"}`
   `proof = HMAC-SHA256(psk, "client" ‖ 0x00 ‖ deviceId ‖ 0x00 ‖ nonce ‖ 0x00 ‖ salt)`
4. S→C `{"type":"hs_ok","serverProof":"<b64 32B>"}` (mutual auth; same HMAC with role
   `"server"`) — or `{"type":"hs_err","reason":"…"}` on rejection.

After `hs_ok` both sides switch to sealed frames. Each frame payload is:

```
{"n": <counter>, "d": "<base64 ChaCha20-Poly1305(ciphertext ‖ 16B tag)>"}
```

- **Keys** (per connection, from PSK + the `salt` from step 2): client→host =
  `HKDF-SHA256(PRK = HMAC(salt, psk), info = "chernobyl-e2e-c2h", 32B)`; host→client uses
  `"chernobyl-e2e-h2c"`.
- **AEAD nonce** = bytes 0–7 big-endian `n`, bytes 8–11 zero (12 bytes); no AAD. Golden frames at n=0 and n=1 are pinned in both suites (`e2e.rs` `golden_sealed_frames_match_the_ts_client`, `session.test.ts`).
- **Counter** `n` starts at 0 per direction and increments by exactly 1 per frame. The server
  requires strict sequence: a lower `n` is a replay, a higher one is desync — both close the
  connection. A bad tag (tamper) also closes it.
- The **inner plaintext** is the normal protocol JSON (`hello`, `ping`, `create_session`, …).

## Verify

```powershell
npm run verify:rust   # from repo root: cargo test + clippy -D warnings + fmt --check + self-test
```

`npm run verify` (the Node/workspace gate) is unchanged and must stay green — see `TODO.md`
for current evidence and remaining work.

## Differential conformance (A/B vs the Node host)

`tools/differential.mjs` (Node, uses the workspace `ws`) runs one scripted 12-observation
protocol scenario and diffs normalized transcripts between two servers — timestamps and
generated ids are stripped, everything else (acks, error strings, close codes, event shapes,
seed snapshot) must match exactly:

```powershell
# terminal 1 — reference (stateless, in-memory): rebuild first so dist matches src
$env:PORT="18790"; $env:HOST_TOKEN="diff-secret"; node services\host\dist\server.js
# terminal 2 — candidate: always give it a FRESH home, else its journal replays old commands
$env:CHERNOBYL_HOME="$env:TEMP\ab-fresh"; .\services\companion-rs\target\release\chernobyl-companion.exe --port 18791 --token diff-secret
# terminal 3 — probe both, then compare (exit 0 + IDENTICAL = parity)
node services\companion-rs\tools\differential.mjs --url ws://127.0.0.1:18790 --token diff-secret --out host.json
node services\companion-rs\tools\differential.mjs --url ws://127.0.0.1:18791 --token diff-secret --out rust.json
node services\companion-rs\tools\differential.mjs --compare host.json rust.json
```

Gotchas (both found the hard way): **restart both servers between runs** — both sides cache
replies by `commandId`, so a second probe run replays run #1's outcomes and masks real diffs;
and never reuse a dirty `CHERNOBYL_HOME` for an A/B run (boot reconciliation answers every
old command honestly, which is correct behavior but not what you're diffing).
A second instance on one machine needs its own `--diagnostics-port` (default 9487 is held
by any already-running instance — the new one refuses to start, by design).
`--bench --kind malformed|command --duration 60 --conns 4` runs the closed-loop saturation
benchmark (throughput + p50/p95/p99; measure server CPU externally per process — measured
numbers and the caveats are in `TODO.md`). `--burst --conns 200` opens that many connections
**simultaneously** (connect+hello+one persisted command+close) and reports wall, per-phase
p50/p95, errors and the /health RSS delta — the reconnect-storm shape the staggered bench
never exercises (it found the ~128-handshake std backlog ceiling; the binary now binds with
`listen(SOMAXCONN)`). `tools/engine-bench.mjs [engine|full] [iters]` microbenches the TS side
(engine alone, or the full in-process path `parse+validate+handle+encode` for the `full`
mode), paired with the in-crate `#[ignore]` tests — run each Rust bench with a single
filter, e.g. `cargo test --release microbench_engine -- --ignored --nocapture`, because
`cargo test -- --ignored` runs the benches in **parallel threads** and contaminates them.
`--churn --light --minutes N` runs connect/hello/close cycles for soak load.

## Soak testing (repeatability over time)

`tools/soak.mjs` (Node, workspace `ws`, no new deps) spawns the **real** binary on loopback
with an isolated temp home (`CHERNOBYL_HOME` under the run dir) and an `--exit-after`
failsafe, drives it through lifecycle stress, samples `/health` resources every 30 s, and
writes timestamped JSONL events plus a machine-readable `summary.json` (threshold provenance
included: existing source limits vs newly proposed soak limits).

```powershell
npm run soak:smoke                                       # ~2-3 min, CI-suitable
npm run soak:long -- --duration 86400                   # explicit 24 h run (default 86400)
node services\companion-rs\tools\soak.mjs --inject server-kill   # failure-detection demo
```

Phases: token auth failure/429/recovery · pipelined-ping FIFO ordering · churn with
registry-leak checks · reconnect recovery · stopped-consumer saturation (count- and
byte-bound paths, chosen adaptively from measured reply size vs the 4 MiB/256 = 16 KiB
threshold) · heartbeat dead-peer release · abrupt client death · graceful `--exit-after`
shutdown with live clients + queued work · restart on the same data dir. Pass/fail criteria
(10, listed in the tool header and evaluated from observations — never process-liveness
alone): unexpected exits, ordering violations, unauthorized data, peer leaks, missing D8
teardowns, dead-peer survival, graceful-shutdown exit code, restart recovery, RSS growth +
absolute caps, connection failures. Any failure ⇒ exit 1.

Gotchas: the machine must **stay awake** for a long run (a prior 24 h attempt died at ~4.7 h
on PC sleep); a short long-mode run is labeled by its measured duration in `summary.json`
and does **not** close soak gate S11 — only a genuine 24 h window does. S11 closed on
2026-10-24 with `target/soak/2026-10-23T19-55Z-long-24h-restart/summary.json`
(`pass: true`, all 10 criteria, 86 429 s window). The standard
`verify` scripts never depend on a soak run. The harness is loopback-only by design (no LAN
exposure); it does not simulate a phone and does not exercise E2E-sealed sessions (the Rust
matrix covers those).

## Layout

| Module | Role |
|---|---|
| `json` / `sha1` / `base64` | strict parsers (depth 32, dup keys, lone surrogates rejected; RFC vectors) |
| `protocol` | ClientMessage validation, ServerMsg wire format, permission authority (fail-closed) |
| `ws` | RFC 6455 handshake + framing, 256 000-byte cap, hostile-input tests |
| `seed` / `engine` | deterministic seed state + the MockEngine port (plans, G1/G2, ack cache) |
| `storage` | append-only JSONL journal: torn-tail recovery, atomic compaction, retention |
| `config` / `adapters` / `companion` | Node-parity config, adapter discovery, boot/reconcile/idempotency |
| `server` | accept loops, auth (4001/429), origin refusal (403), posture gates (D1/D2/D6), version gate (4002), heartbeat, diagnostics HTTP, E2E handshake + sealed-frame dispatch |
| `client` | test WS client + loopback `http_get` (used by `--self-test`) |
| `crypto` | std-only SHA-256, HMAC, HKDF, ChaCha20, Poly1305, AEAD (official RFC/NIST KATs) |
| `e2e` | PSK handshake proofs, HKDF directional keys, sealed frames with counter nonces |
| `qr` / `pairing` | std-only QR Model-2 encoder + single-use, TTL/rate-limited pairing offers |
| `ffi` | `SetConsoleCtrlHandler` (graceful shutdown), `GetProcessMemoryInfo` (rssBytes), `BCryptGenRandom`, DPAPI, large-backlog listener |
| `main` | CLI/env wiring, scheduler pump (50 ms), `--self-test`, pairing/revoke, graceful shutdown |
