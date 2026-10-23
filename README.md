# CHERNOBYL

One interface. Every agent. A mobile-first control app for AI agents: monitor hosts and
agents, watch live session output, and approve or reject permission requests from your
phone — with the companion backend enforcing every decision.

Monorepo layout:

```
packages/protocol   Shared TypeScript protocol: types, message envelopes,
                    runtime validators, seed data and the MockEngine.
services/companion-rs
                    The real backend (Rust, std-only, one binary): protocol-v2
                    WebSocket server + pairing/QR + E2E session layer +
                    OpenCode adapter + loopback diagnostics. This is the
                    production runtime the phone talks to.
services/host       TypeScript reference host (Node + ws): MockEngine + the
                    protocol conformance/differential suite the Rust engine was
                    ported test-for-test from. Still built and tested by `verify`;
                    not the production enforcement point.
apps/mobile         Expo (React Native) app — Expo Router, TanStack Query,
                    Zustand, React Hook Form + Zod, SQLite, SecureStore.
```

## Quick start

```powershell
npm install          # workspaces
npm run verify       # TS: typecheck + lint + tests + builds + android export
npm run verify:rust  # Rust runtime: tests + clippy + fmt + self-test
```

Run the pieces:

```powershell
# real backend (Rust companion) — the runtime the phone talks to
cargo run --release --manifest-path services/companion-rs/Cargo.toml
#   default ws://127.0.0.1:8787 (+ loopback diagnostics on 9487)
#   add --token <t> (or HOST_TOKEN=...) to require auth; --pair to print a pairing QR

npm run dev:mobile   # Expo dev server (press a / w for Android / web)
npm run dev:host     # OPTIONAL: Node reference host (MockEngine) for differential testing
```

The app runs against the **offline mock by default** (no backend needed). Set
`EXPO_PUBLIC_HOST_WS_URL` (e.g. `ws://10.0.4.21:8787`) in `apps/mobile/.env.local`
to connect the real companion instead. If the companion was started with a token
(`HOST_TOKEN`), save the same token via the in-app "Connect host" modal — the app
holds it in the device keystore. Beyond loopback a token alone is not enough to run
commands (decision D2): the E2E session handshake must complete first (see
`services/companion-rs/README.md`, "E2E wire format").

## Screens

| Screen | Route | Notes |
| --- | --- | --- |
| Onboarding | `/onboarding` | 3 slides, contour graphics, skip |
| Sign in | `/sign-in` | mock auth, no credentials checked |
| Home dashboard | `/` (tabs) | stats, active agents, pending approvals, activity, search |
| Agents | `/agents` | search + full list |
| Agent details | `/agent/[id]` | capabilities, sessions, activity, start session |
| Sessions | `/sessions` | All / Running / Waiting / Completed filters + search |
| Session workspace | `/session/[id]` | Output / Files / Activity, send, interrupt |
| Approvals | `/approvals` | pending (live countdowns) + decided history |
| Permission request | `/permission/[id]` | exact command, risk, Approve once / Reject, separate "approve for session" |
| Profile | `/profile` | settings entries, sign out (confirmed) |
| Settings | `/settings/[section]` | hosts, integrations, notifications, appearance, security, audit |
| New task | `/new-task` (modal) | pick agent, describe the job |
| Connect host | `/connect-host` (modal) | token → SecureStore keystore |

## How state works

- **Server state** (hosts, agents, sessions, permissions, activity) lives in one TanStack
  Query snapshot. Transport events merge through `applyServerMessage`; the offline mock
  and the real WebSocket transport emit the identical `ServerMessage` stream.
- **Client state** (auth, onboarding, settings, search/filter, toasts) is Zustand,
  persisted to SQLite (`src/platform/storage.ts`).
- **Commands** (`src/data/commands.ts`) attach fresh `commandId`s, suppress identical
  concurrent sends, and the WS transport replays unresolved commands with the *same*
  `commandId` on reconnect — the host answers duplicates with `duplicate: true` acks
  instead of executing twice.
- **Permission requests** expire after 2 minutes (protocol `PERMISSION_TTL_MS`); stale
  requests are never decidable, on either side.

## Backend runtime

### `services/companion-rs` — the real runtime (Rust, std-only)

One zero-dependency binary (ADR-010). It is the enforcement point and speaks the same
protocol-v2 the mobile app already emits:

- **Auth** — start with `--token` / `HOST_TOKEN`; WS clients present it as a `?token=`
  query param or `Authorization: Bearer` header, else the socket is closed `4001`. A
  non-loopback bind without a token is refused outright.
- **Version gate** — a `hello` with a mismatched `PROTOCOL_VERSION` is rejected and
  closed `4002`.
- **Resume** — every `hello` returns a full snapshot, so a reconnecting client never
  has to guess what it missed.
- **Liveness** — server-side heartbeats; dead peers are torn down after 3 missed beats.
- **Dedupe** — a replayed `commandId` returns `duplicate: true` instead of running twice.
- **Pairing + E2E session (ADR-002/006/011)** — `--pair` prints a single-use, 5-minute
  QR carrying a per-device 256-bit PSK (DPAPI-protected at rest). A handshake
  (`hs_hello → hs_challenge → hs_proof → hs_ok`) proves mutual PSK knowledge; from
  `hs_ok` on every frame is ChaCha20-Poly1305-sealed with HKDF directional keys and a
  monotonic counter nonce (replay/tamper-proof). Revocation blocks a device at its next
  handshake. See `services/companion-rs/README.md` for the exact wire format.
- **Posture policies (decisions D1/D2/D6)** — a WS upgrade carrying a browser `Origin`
  is refused `403` (no browsers until the web app); beyond loopback every command
  requires a live E2E session (a stolen token alone gets an explicit error, never
  execution); and `/health` on the main listener beyond loopback requires the token.
- **Adapters** — an attach-only OpenCode adapter (HTTP/SSE to an existing
  `opencode serve`) plus the in-process `mock` engine. Claude Code / Ollama / Hermes are
  not wired yet (Phase 7).

### `services/host` — TypeScript reference host (Node + ws)

The MockEngine + protocol conformance/differential suite the Rust engine was ported
test-for-test from. It still builds and runs under `verify`, but it is not the
production backend — use the companion for real connections.

## Honest limitations

- **Mock by default**: no shell is ever executed and sign-in checks no password in the
  offline demo. Point `EXPO_PUBLIC_HOST_WS_URL` at the companion to drive real state.
  ADR-009: mock mode is a permanent, first-class dev/test mode; the mobile app shows a
  persistent **"Simulation"** badge while the mock is active so demo data is never
  mistaken for a real agent.
- **Real agent integration**: the OpenCode adapter is wired end-to-end (attach-only
  HTTP/SSE to an existing `opencode serve`, no spawned server). Claude Code / Ollama /
  Hermes adapters are not implemented yet (Phase 7). When no healthy real runtime is
  attached, sessions/agents fall back to the mock seed — the UI must not present that
  as live activity.
- **E2E pairing is complete on the backend but the phone client is not built yet**
  (Phase 6). The wire format the phone must implement is documented and frozen in
  `services/companion-rs/README.md`; the server side is implemented and covered by an
  adversarial test matrix.
- **Notifications are local-only while the app is open.** Backgrounded delivery is
  designed as a persistent WebSocket + Android foreground service posting local
  notifications (ADR-011 — FCM/APNs deliberately dropped). No delivery while the socket
  is down; iOS background push (APNs) is out of scope.
- **Transport encryption is app-layer E2E**, not TLS — the std-only Rust companion has
  no TLS crate, so security lives in the per-device PSK session (ADR-011). The
  companion's own socket is plain `ws://`; a middleman sees ciphertext only.
- Host/paired-device token lives in the device keystore (expo-secure-store), never in
  the DB; on web it stays in memory for the session. Audit trail is device-local SQLite.

## Verification status

Two suites, both green on the current tree:

- `npm run verify` (TypeScript): strict typecheck across the TS workspaces, ESLint
  (0 errors; 2 known warnings from React Compiler skipping `react-hook-form.watch`),
  **44 tests** (13 domain + 22 host + 9 adapter-sdk via `node --test`), workspace builds,
  and `expo export --platform android` → `dist-android` (Hermes bundle).
- `npm run verify:rust` (the real runtime): **160 passed / 0 failed / 3 ignored** Rust
  tests (incl. the crypto known-answer tests and the D1/D2/D6 posture + E2E adversarial
  matrix), `cargo clippy -D warnings`, `cargo fmt --check`, and the binary's `--self-test`
  end-to-end probe.
- **Soak harness** (`services/companion-rs/tools/soak.mjs`): `npm run soak:smoke` (~80 s,
  10 predefined pass/fail criteria) and `npm run soak:long` (explicit long/24-hour mode).
  Emits JSONL telemetry + a machine-readable `summary.json`; a fault-injection mode proves
  the harness detects an unexpected server exit (exit 1) rather than passing blindly.
  Thresholds are labelled by provenance — server-derived existing limits (queue bounds,
  heartbeat, auth-failure cap) vs newly proposed ones (RSS growth, release timeouts).

The WebSocket/E2E layer is exercised against a live companion end-to-end (handshake,
snapshot, command → ack, token rejection `4001`, and the pairing → handshake → sealed-frame
path) in the Rust integration tests. There is no Android SDK on this machine, so there is
no `expo run:android` device build here; the export bundle is the build check, and
physical phone validation is tracked in `docs/validation/PHONE-VALIDATION-CHECKLIST.md`
(all rows currently NOT RUN / BLOCKED).

## Remaining work (honest status)

The Rust backend, protocol, crypto, pairing and the OpenCode adapter are implemented and
tested (Phases 0–5′ complete). What is left:

1. **Phase 6 — mobile E2E/pairing client.** Scan the pairing QR, run the `hs_*` handshake,
   seal/open frames with the documented wire format, store the per-device PSK in the
   keystore, reconnect by trying the advertised endpoint list (ADR-011 §2), and add the
   Android foreground service + persistent WS + local notifications. Blocked on a
   mobile crypto-stack + native-module decision (pure-JS vs native crypto/camera) and on
   a physical Android device for validation.
2. **Phase 7 — additional adapters** (Claude Code, Ollama, Hermes) as Rust modules.
3. **Phase 8 — terminal TUI + windowless autostart host** (ADR-011 §5).
4. **Physical-device validation** — `docs/validation/PHONE-VALIDATION-CHECKLIST.md`, all
   rows NOT RUN/BLOCKED (needs a real Android phone + the Phase 6 client).
5. **24-hour soak** for the S11 release gate (`npm run soak:long -- --duration 86400`;
   the machine must stay awake — an earlier attempt died ~4.7 h on PC sleep).
6. **Independent security assessment** (S9) — automated tests + review are not a
   substitute for it.

See `docs/decisions/PHASE5-THREAT-MODEL.md` §6 for the per-gate status and the exact
evidence each gate is waiting on.
