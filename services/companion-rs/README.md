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
`--diagnostics-port <n>` (default 9487) · `--exit-after <ms>` · `--self-test`.
`CHERNOBYL_HOME` selects the data directory (config, journal).

**Secure-by-default:** a non-loopback bind without `--token`/`HOST_TOKEN` is refused.
Loopback with no token keeps Node-parity behavior (dev/demo).

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
numbers and the caveats are in `TODO.md`). `tools/engine-bench.mjs` microbenches the TS
engine alone (pair of the in-crate `microbench_engine_stale_decide` `#[ignore]` test; run
`cargo test --release microbench -- --ignored --nocapture` for the Rust side).
`--churn --light --minutes N` runs connect/hello/close cycles for soak load.

## Layout

| Module | Role |
|---|---|
| `json` / `sha1` / `base64` | strict parsers (depth 32, dup keys, lone surrogates rejected; RFC vectors) |
| `protocol` | ClientMessage validation, ServerMsg wire format, permission authority (fail-closed) |
| `ws` | RFC 6455 handshake + framing, 256 000-byte cap, hostile-input tests |
| `seed` / `engine` | deterministic seed state + the MockEngine port (plans, G1/G2, ack cache) |
| `storage` | append-only JSONL journal: torn-tail recovery, atomic compaction, retention |
| `config` / `adapters` / `companion` | Node-parity config, adapter discovery, boot/reconcile/idempotency |
| `server` | accept loops, auth (4001/429), version gate (4002), heartbeat, diagnostics HTTP |
| `client` | test WS client + loopback `http_get` (used by `--self-test`) |
| `ffi` | `SetConsoleCtrlHandler` (graceful shutdown), `GetProcessMemoryInfo` (rssBytes) |
| `main` | CLI/env wiring, scheduler pump (50 ms), `--self-test`, graceful shutdown |
