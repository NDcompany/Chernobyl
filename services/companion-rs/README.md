# chernobyl-companion — the CHERNOBYL local backend (Rust)

One standalone, zero-dependency binary (ADR-010): the protocol-v2 WebSocket server the mobile
app already speaks + the Phase-2 companion foundation + loopback diagnostics. `[dependencies]`
is intentionally empty — std only.

## Build and run

```powershell
cargo build --release                 # target\release\chernobyl-companion.exe
.\target\release\chernobyl-companion.exe            # default: 127.0.0.1:8787
.\target\release\chernobyl-companion.exe --self-test # 7-check end-to-end probe
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
