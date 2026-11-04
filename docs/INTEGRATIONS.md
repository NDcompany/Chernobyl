# Supported integrations

The companion reports every adapter's live capabilities on the loopback diagnostics route
`GET /adapters`. Each capability is `available`, `limited` (with the reason) or `unavailable`.
That route and the `capabilities()` list in each adapter's source are the authority. This
page records **which versions were verified** and the limits that matter in practice.

| Integration | Kind | Verified against | Source | How it connects |
|---|---|---|---|---|
| OpenCode | agent runtime | `opencode` **1.19.4** (live e2e PASS 2026-10-24), 1.17.7 (Phase 3) | `services/companion-rs/src/opencode.rs` | attach-only HTTP + SSE to a running `opencode serve`; never spawns a second server |
| Ollama | model runtime | Ollama **0.40.9** | `services/companion-rs/src/ollama.rs` | attach-only HTTP (`/api/version`, `/api/tags`, `/api/ps`) |
| Claude Code | hook ingress + driven sessions | Claude Code **2.1.309** (`SessionStart` captured live); driven sessions **2.1.310** (live Haiku run through the companion server) | `services/companion-rs/src/claude.rs`, `claude_live.rs`, `claude_runtime.rs` | hooks POST to the loopback diagnostics listener (install the snippet from `chernobyl-companion --claude-hooks` yourself, never auto-written); with `workspaces`, the phone also starts `claude.exe` sessions (stream-json on stdio) in allow-listed folders |
| Codex | agent runtime (live sessions) | **no live run**: openai/codex `970111f` schema, fake app-server processes only | `services/companion-rs/src/codex.rs`, `codex_runtime.rs` | spawns one `codex app-server` (stdio JSON-RPC) per adapter on the first session, in an allow-listed workspace; each phone session is a thread |
| Hermes | — | not installed, deferred | ADR-008 | not offered; unsupported adapters stay invisible rather than half-working |
| Mock | simulation | — | `services/companion-rs/src/seed.rs` | offline demo data; the phone labels it **Simulation** (ADR-009) |

## Limits that matter

- **OpenCode.** Sessions can be listed, created and prompted. These are `limited`:
  interrupt (stopping a healthy long generation is not yet re-verified), diff (no populated
  capture yet), permission reply (the contract is verified from OpenAPI, but no live
  asked/reply exchange yet) and output (text parts only). There is no SSE replay: after a
  reconnect, state is re-listed.
- **Ollama.** It is a model provider, not an agent. Session operations report
  `unavailable` (PRD §7.7).
- **Claude Code.** Without `workspaces` it is ingress only: the companion observes hook
  events from sessions started in VS Code or a terminal (only `SessionStart` was captured
  live; other hook shapes are parsed defensively and reported `limited`). With
  `workspaces` the phone drives its own sessions: create, prompt (follow-ups join the
  running turn), interrupt, inline approvals with Claude's options, permission mode
  (default, accept edits, plan), end, a streaming transcript, and MCP status, sign-in and
  reconnect. History of sessions it did not start is `unavailable`; desktop MCP sign-in
  is `limited` (whether the CLI's own listener reconnects by itself is unverified, so the
  companion polls and reconnects). **Adding a folder to `workspaces` is trusting it:** the
  CLI runs in print mode, which skips Claude's folder-trust dialog, so the folder's
  project hooks and MCP servers load on spawn. Hooks of a driven session are ignored, so it
  is not shown twice. Set `binary` only to a real `claude.exe` (no `.cmd` shim).
- **Codex.** Every capability is `limited`: "implemented from openai/codex@970111f schema;
  verified against fake app-server processes only; no live run". Sessions stream a live
  transcript; approvals carry Codex's own options (allow, allow for this session, the
  proposed exec-policy amendment as a persistent option, deny, deny and stop); follow-ups
  steer the running turn; interrupt, MCP status, desktop MCP sign-in and reconnect (a
  config reload) work. Not offered: ending a session, permission modes, phone-side MCP
  sign-in. A session dies with its app-server process; the next new session respawns it.
  **Adding a folder to `workspaces` is the folder-trust decision:** Codex runs with
  `approvalPolicy: on-request` and `sandbox: workspace-write` there. Set `binary` to the real
  `codex.exe`; npm's `codex.cmd` shim is refused (no shell is ever used).

## Updating this page

When an adapter is verified against a new runtime version, update the version column and the
adapter's capability notes in the same commit. Do not mark a capability `available` from a
fake-server test alone. Fault-injection tests prove the wiring; only a live run proves
support.
