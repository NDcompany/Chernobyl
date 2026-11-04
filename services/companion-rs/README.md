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

The exe carries the CHERNOBYL logo as its icon: `build.rs` turns `assets/chernobyl.ico`
(generated from `design/Chernobyl Logo.png` by `scripts/brand/make-logo-assets.mjs`) into a
`.res` file for the MSVC linker, std-only like the rest of the crate.

Flags (env in parentheses): `--config <path>` · `--port <n>` (`PORT`, 0 = ephemeral,
default 8787) · `--bind <addr>` (default 127.0.0.1) · `--token <t>` (`HOST_TOKEN`) ·
`--diagnostics-port <n>` (default 9487) · `--exit-after <ms>` · `--self-test` ·
`--install-autostart` / `--uninstall-autostart` (HKCU Run start-on-login, user level,
idempotent; carries `--config` when given, console window at login — packaging caveat in
`TODO.md`) · `--pair` (create a pairing offer, print its terminal QR, exit) ·
`--endpoint <ws-url>` (address embedded in the QR, default `ws://bind:port`) ·
`--devices` (list the paired-device registry) · `--revoke <device-id>` (revoke a paired
device) · `--require-e2e` (force remote E2E session posture on any bind, e.g. for tunnels) ·
`--soak smoke|<seconds>` (built-in soak harness, see [Soak testing](#soak-testing-repeatability-over-time)). `CHERNOBYL_HOME` selects the data directory (config, journal).

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
mutual PSK knowledge — the phone sends a fresh client nonce, the server answers with a fresh
nonce+salt and both sides HMAC all three, so neither a captured proof nor a recorded server
transcript can be replayed and a MITM without the PSK is rejected. A pending offer is
redeemed on first connect (single-use consume + device registration), and that first session
hands the phone a fresh device key that replaces the QR's PSK once the phone confirms it
stored it, so a re-used QR is refused from then on (see "Device key rotation" below); a paired
device reconnects with its stored key. From `hs_ok` on, every frame is ChaCha20-Poly1305-sealed with
HKDF-SHA-256 directional keys and a monotonic counter nonce, so replay, desync and tampering
all tear the connection down. Revocation (`companion revoke <device-id>`) terminates any
live session with close code 4003 ("device revoked") via an in-process diagnostics route
and blocks future handshakes (D4). All crypto is std-only with official RFC/NIST known-answer tests; no
X25519 forward secrecy yet (PSK + fresh nonces — post-compromise secrecy is not claimed).

The `?token=` loopback path is unchanged and stays for dev/differential parity. Beyond
loopback the token alone is **not** enough to run commands (D2): complete the `hs_*`
handshake first.

### E2E wire format (the contract a phone client must implement)

All frames are WebSocket **text** frames carrying JSON. Handshake first (plaintext), then a
sealed envelope per frame.

Handshake (PSK = the 32 bytes from the QR, base64):

1. C→S `{"type":"hs_hello","deviceId":"<hex>","cnonce":"<b64 16B>"}` — `cnonce` fresh per
   attempt from the OS CSPRNG; a missing or malformed one gets `hs_err`
2. S→C `{"type":"hs_challenge","nonce":"<b64 16B>","salt":"<b64 16B>"}` — fresh per connection
3. C→S `{"type":"hs_proof","proof":"<b64 32B>"}`
   `proof = HMAC-SHA256(psk, "client" ‖ 0x00 ‖ deviceId ‖ 0x00 ‖ nonce ‖ 0x00 ‖ salt ‖ 0x00 ‖ cnonce)`
4. S→C `{"type":"hs_ok","serverProof":"<b64 32B>"}` (mutual auth; same HMAC with role
   `"server"`) — or `{"type":"hs_err","reason":"…"}` on rejection.

Both sides contribute freshness (security review 2026-10-25): the server's nonce+salt stop a
replayed client proof, and the client's `cnonce` makes a recorded server transcript fail the
client's `serverProof` check and never re-derive an old session's keys. `hs_ok` is the last
plaintext frame: the server sends no event while a handshake is in flight.

After `hs_ok` both sides switch to sealed frames. Each frame payload is:

```
{"n": <counter>, "d": "<base64 ChaCha20-Poly1305(ciphertext ‖ 16B tag)>"}
```

- **Keys** (per connection, from PSK + the `salt` from step 2 + the `cnonce` from step 1):
  client→host = `HKDF-SHA256(PRK = HMAC(salt ‖ cnonce, psk), info = "chernobyl-e2e-c2h", 32B)`;
  host→client uses `"chernobyl-e2e-h2c"`.
- **AEAD nonce** = bytes 0–7 big-endian `n`, bytes 8–11 zero (12 bytes); no AAD. Golden frames at n=0 and n=1 are pinned in both suites (`e2e.rs` `golden_sealed_frames_match_the_ts_client`, `session.test.ts`).
- **Counter** `n` starts at 0 per direction and increments by exactly 1 per frame. The server
  requires strict sequence: a lower `n` is a replay, a higher one is desync — both close the
  connection. A bad tag (tamper) also closes it.
- The **inner plaintext** is the normal protocol JSON (`hello`, `ping`, `create_session`, …),
  plus the two transport-level rotation frames below.

**Device key rotation** (ADR-011 addendum). These frames are transport-level like the `hs_*`
frames, always sealed, and not protocol messages:

- S→C `{"type":"hs_rekey","psk":"<b64 32B>"}` comes right after `hs_ok` while the device's
  rotated key is pending, i.e. from its first redemption until the phone confirms the key. Every
  connection gets the same key until then.
- C→S `{"type":"hs_rekey_ack"}` is sent only after the phone persisted the key, and from its
  next handshake on the phone proves that key. The server accepts it any time after `hs_ok`
  (also before `hello`).

The ack makes the new key the device's only key, and the QR's PSK then gets `hs_err`. A proof
under the pending key (the phone stored it but the ack was lost) promotes it the same way. A
client that ignores `hs_rekey` keeps working on the QR's PSK and is offered the same key on
each connection.

### Live transcripts (ADR-015, feature `transcript.v1`)

`hello_ack.features` carries `"transcript.v1"`. A phone that sees it may subscribe to a live
session's transcript; older phones never receive transcript frames.

- C→S `{"type":"session_subscribe","commandId":"…","sessionId":"…","epoch":"…","afterSeq":N}`
  (`epoch`/`afterSeq` optional, both needed to resume) → `ack` with `ok:true` and `sessionId`,
  or `ok:false` `"unknown session"`. Non-mutating: no journal row. Subscribing again on the
  same connection only moves the cursor; a fifth subscription on one connection drops its oldest.
- C→S `{"type":"session_unsubscribe","commandId":"…","sessionId":"…"}` → `ack` (idempotent).
- S→C `{"type":"transcript_snapshot","sessionId","epoch","seq","items","truncatedBefore","turns","state","mcp"}`
  when the phone's `epoch`/`afterSeq` cannot be replayed (another epoch, behind the replay
  ring, or none sent); at most one per second per connection and session, at most 512 KiB,
  newest items kept (`truncatedBefore` when older ones were left out).
- S→C `{"type":"transcript","sessionId","epoch","ops":[…]}`: contiguous ops after the cursor
  (`seq` +1 each), at most 256 ops and about 64 KiB per message (a larger single op travels
  alone). Ops: `item` (insert or replace by id), `append` (`text`/`input`/`output` delta),
  `turn` (upsert by id), `state`, `mcp` (full list). The phone re-subscribes with its
  `epoch` and last `seq` on a gap, an epoch change, or a reconnect.

Beyond loopback `session_subscribe` needs a live E2E session (the D2 gate) and a sessionless
connection receives no transcript frame (D9). Transcript frames are sealed Bulk-class frames,
sent only while the connection's bulk queue is at most half full, so they never cause a D8
teardown; a lagging phone gets a snapshot once it drains. Subscriptions end with the
connection (teardown, revocation 4003). Transcripts are memory only: 8 sessions (least
recently active evicted), ended ones dropped after 1 h, replay ring 2,048 ops or 1 MiB, 400
items or 2 MiB per session; text is redacted and capped as ADR-015 §3.5 lists (deltas every
100 ms, at most 8 KiB per append). `/health` reports `transcriptSessions`, `transcriptSubs`
and `runtimeMalformed`.

### Claude Code adapter (ADR-015 §4.2)

The `"type": "claude"` entry is always the hook ingress (Claude Code hooks POST to the
diagnostics listener). With `workspaces` it also drives Claude Code sessions from the phone:

```json
{ "id": "claude-1", "type": "claude", "enabled": true,
  "workspaces": ["G:\\Chernobyl"], "binary": null, "maxSessions": 3, "permissionMode": "default" }
```

- `workspaces` (optional): absolute, existing folders, checked at boot. A drive root, the
  user-profile folder and any folder containing the CHERNOBYL data directory are refused; a bad
  entry is logged by name, shows as `lastError` on `/adapters`, and the entry stays an ingress
  only. Without `workspaces` nothing is spawned (today's behaviour). The phone sees them as
  `Agent.live.workspaces` and `create_session.project` must be one of those ids.
  **Adding a folder to `workspaces` is trusting it:** the CLI runs in print mode, which skips
  Claude's folder-trust dialog, so that folder's project hooks, settings and MCP servers
  load on spawn.
- `binary`: `null` = `%USERPROFILE%\.local\bin\claude.exe`, else `claude.exe` on `PATH`
  (never the npm `.cmd` shim: no shell is ever used). Health is `claude --version` at boot
  and in the probe loop.
- `maxSessions` (1-16, default 3): live `claude.exe` processes at once. `create_session`
  is also limited to one per 5 s.
- `permissionMode` (`default` | `acceptEdits` | `plan`, default the CLI's): the starting
  mode; the phone can switch among the same three. Bypass modes are never reachable.

Each session is one `claude -p --input-format stream-json …` child with `--session-id
<uuid>` (the session id) in its workspace; prompts travel on stdin only and the environment
drops `HOST_TOKEN`, `CHERNOBYL_*` and `NODE_OPTIONS`. Prompts sent mid-turn join the
running turn (`promptWhileBusy: merge`); approvals are answered inline with Claude's own
options; `interrupt`, `set_permission_mode`, `session_end` (interrupt, close stdin, kill
after 5 s) and MCP status, sign-in (desktop or phone-callback) and reconnect are supported. A
process idle for 30 min with no subscriber and nothing pending has its stdin closed; the next
command respawns it with `--resume`. If a process dies, open approvals turn `stale`, its
prompts fail ("Claude Code exited with code N") and the session ends. Hooks fired by a driven
session are ignored, so it never shows twice.

### Codex adapter (ADR-015 §4.3)

An adapter entry in `companion.json` with `"type": "codex"` drives `codex app-server`:

```json
{ "id": "codex-1", "type": "codex", "enabled": true,
  "workspaces": ["G:\\Chernobyl"], "binary": null, "args": [] }
```

- `workspaces` (required, at least one): absolute, existing folders, checked at config load.
  A drive root, the user-profile folder (or a folder containing it) and any folder containing
  the CHERNOBYL data directory are refused. The phone sees them as
  `Agent.live.workspaces` (`id` = first 12 hex chars of sha256 of the canonical path) and
  `create_session.project` must be one of those ids. **Listing a folder trusts it**: Codex
  runs there with `approvalPolicy: on-request`, `sandbox: workspace-write`.
- `binary`: `null` = `codex.exe` on `PATH`. It is spawned directly, never through a shell,
  so npm's `codex.cmd` shim is refused: point `binary` at the real `codex.exe` (inside the
  npm package's `vendor` folder). Health (`/adapters`) is whether the binary resolves.
- `args`: put before `app-server`. Tests use `"binary": "node"` (or node's full path) with
  `"args": ["<repo>/services/companion-rs/tools/fake-codex-app-server.mjs"]`.

One app-server child per adapter, spawned on the first `create_session` (acked once Codex
names the thread, which is the session id) with the companion's environment minus
`HOST_TOKEN`, `CHERNOBYL_*` and `NODE_OPTIONS`. Prompts travel on stdin only. If the child
exits, open approvals turn `stale`, running prompts fail, a pending interrupt is `unknown`
and its sessions end; the next `create_session` respawns it. Capabilities are all `limited`
(no live run; see `docs/INTEGRATIONS.md`).

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

The soak harness is built into the binary (`src/soak.rs`, std only, no Node needed):
`--soak` spawns **this same executable** as the server under test, as child processes on
loopback with a token, ephemeral ports, an isolated home (`CHERNOBYL_HOME` under the run dir)
and an `--exit-after` failsafe, so crash, graceful shutdown and restart are judged across a
real process boundary. It drives the server with the in-repo client through lifecycle
stress, samples `/health` resources every 30 s, prints one PASS/FAIL line per criterion, and
writes timestamped `events.jsonl` plus a machine-readable `summary.json` (criteria, threshold
provenance: existing source limits vs newly proposed soak limits, failures, `pass`) to
`target/soak/<timestamp>-<mode>` or `--out <dir>`.

```powershell
npm run soak:smoke                                         # one pass, ~80 s, CI-suitable
npm run soak:long                                          # explicit 24 h run (--soak 86400)
cargo run --release -- --soak 3600                         # any other duration, in seconds
cargo run --release -- --soak smoke --inject server-kill   # failure-detection demo (exit 1)
```

Long mode runs the same pass with a load loop (churn + FIFO every cycle, saturation every
3rd, dead peer every 10th) until the duration has really elapsed, before the closing phases,
so the quiesced RSS is measured after the load; a progress line every 30 s replaces
per-phase output while the loop runs. `tools/soak.mjs` (the Node original, same phases and
criteria) is superseded and kept for one release.

Phases: token auth failure/429/recovery · pipelined-ping FIFO ordering · churn with
registry-leak checks · reconnect recovery · stopped-consumer saturation (count- and
byte-bound paths, chosen adaptively from measured reply size vs the 4 MiB/256 = 16 KiB
threshold) · heartbeat dead-peer release · abrupt client death · graceful `--exit-after`
shutdown with live clients + queued work · restart on the same data dir. Pass/fail criteria
(10, listed in the `src/soak.rs` header and evaluated from observations — never process-liveness
alone): unexpected exits, ordering violations, unauthorized data, peer leaks, missing D8
teardowns, dead-peer survival, graceful-shutdown exit code, restart recovery, RSS growth +
absolute caps, connection failures. Exit 0 only when all ten pass; any failure ⇒ exit 1.

Gotchas: the machine must **stay awake** for a long run (a prior 24 h attempt died at ~4.7 h
on PC sleep); a short long-mode run is labeled by its measured duration in `summary.json`
and does **not** close soak gate S11 — only a genuine 24 h window does. S11 closed on
2026-10-24 with `target/soak/2026-10-23T19-55Z-long-24h-restart/summary.json`
(`pass: true`, all 10 criteria, 86 429 s window, run by the Node harness). The standard
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
| `config` / `adapters` / `companion` | Node-parity config, adapter discovery, boot/reconcile/idempotency; the `LiveRuntime` trait and router (ADR-015 §4.1): `create_session` by the agent's `hostId`, session commands to the owning runtime, `permission_decision` to the runtime that surfaced it, the rest to the engine; demo data hidden while any runtime is healthy |
| `server` | accept loops, auth (4001/429), origin refusal (403), posture gates (D1/D2/D6), version gate (4002), live session revocation (4003, D4), heartbeat, diagnostics HTTP, E2E handshake + sealed-frame dispatch, `session_subscribe`/`session_unsubscribe` per connection |
| `transcript` | ADR-015 transcript hub: per-session op ring + compacted items, coalesced deltas, subscriptions, half-bulk backpressure, snapshots, redaction and caps; `HubHandle` is what live runtimes push through |
| `claude_live` / `claude_runtime` | ADR-015 Claude Code: stream-json driver core (decoder, permission options, encoders, workspace policy) and its `LiveRuntime` (a process per session, reaper and `--resume`, G1 lifecycles, MCP sign-in) |
| `codex` / `codex_runtime` | ADR-015 Codex app-server: JSON-RPC core (decoder, approvals, MCP) and its `LiveRuntime` (spawn, sessions as threads, hub/engine wiring) |
| `runtime_io` | ADR-015 child process IO, env scrub, secret redaction, desktop URL opener, OAuth callback check |
| `client` | test WS client + loopback `http_get` (used by `--self-test`) |
| `crypto` | std-only SHA-256, HMAC, HKDF, ChaCha20, Poly1305, AEAD (official RFC/NIST KATs) |
| `e2e` | PSK handshake proofs, HKDF directional keys, sealed frames with counter nonces |
| `qr` / `pairing` | std-only QR Model-2 encoder + single-use, TTL/rate-limited pairing offers |
| `ffi` | `SetConsoleCtrlHandler` (graceful shutdown), `GetProcessMemoryInfo` (rssBytes), `BCryptGenRandom`, DPAPI, large-backlog listener |
| `main` | CLI/env wiring, scheduler pump (50 ms), `--self-test`, pairing/revoke, graceful shutdown |
| `soak` | `--soak` harness: spawns this exe as the server under test, 10 PASS/FAIL criteria, summary.json |
