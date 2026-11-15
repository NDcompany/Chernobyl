# Supported integrations

The companion reports every adapter's live capabilities on the loopback diagnostics route
`GET /adapters`. Each capability is `available`, `limited` (with the reason) or `unavailable`.
That route and the `capabilities()` list in each adapter's source are the authority. This
page records **which versions were verified** and the limits that matter in practice.

| Integration | Kind | Verified against | Source | How it connects |
|---|---|---|---|---|
| OpenCode | agent runtime (live sessions) | `opencode` **1.19.4** (rich parts live run PASS 2026-11-04 on local Ollama; live e2e PASS 2026-10-24), 1.17.7 (Phase 3) | `services/companion-rs/src/opencode.rs` | attach-only HTTP + SSE to a running `opencode serve`; never spawns a second server |
| Ollama | model runtime | Ollama **0.40.9** | `services/companion-rs/src/ollama.rs` | attach-only HTTP (`/api/version`, `/api/tags`, `/api/ps`) |
| Claude Code | hook ingress + driven sessions | Claude Code **2.1.309** (`SessionStart` captured live); driven sessions **2.1.310** (live Haiku run through the companion server) | `services/companion-rs/src/claude.rs`, `claude_live.rs`, `claude_runtime.rs` | hooks POST to the loopback diagnostics listener (install the snippet from `chernobyl-companion --claude-hooks` yourself, never auto-written; it runs `chernobyl-companion --hook activity`, which reads the control key itself, so the snippet holds no secret); with `workspaces`, the phone also starts `claude.exe` sessions (stream-json on stdio) in allow-listed folders, and lists, replays and continues (`--resume`) the 50 newest Claude sessions on disk in those folders, refusing one written in the last 2 minutes (ADR-015 §10); the phone also picks the model, attaches camera photos, runs slash commands, shows context use and renames (ADR-017); a session running in a terminal or VS Code streams live to the phone ("Live on PC"), can be forked to continue on the phone (`--resume --fork-session`), and with `--install-hooks` its permission prompts reach the phone (allow once or deny; the PC asks as usual after 120 s, or at once when no phone is connected); the hook round trip is verified live with `claude -p` 2.1.322 (ADR-018) |
| Antigravity | agent runtime (live sessions) | `agy` **1.5.6** (`agy acp` live run through the runtime 2026-11-12, `gemini-3.8-flash-low`: create, approve a command, follow-up, interrupt, resume in a new process; about 75k tokens) | `services/companion-rs/src/agy.rs`, `agy_runtime.rs` | spawns one `agy acp` (Agent Client Protocol, JSON-RPC on stdio) per adapter; each phone session is an agy session in an allow-listed workspace; history from agy's own `session/list` (ADR-022) |
| Codex | agent runtime (live sessions) | **codex-cli 0.167.0-alpha.4.1** (inside the Codex desktop app 26.1106; live runs PASS 2026-11-12, 11 short turns; schema from its `generate-ts`) | `services/companion-rs/src/codex.rs`, `codex_runtime.rs` | spawns one `codex app-server` (stdio JSON-RPC) per adapter, found on PATH or in the Codex app's install at every start; each phone session is a thread in an allow-listed workspace; lists, replays, resumes and forks the Codex threads (the Codex app's included) in those folders; model picker, images, context use, rename (ADR-015 §10) |
| Hermes | — | not installed, deferred | ADR-008 | not offered; unsupported adapters stay invisible rather than half-working |
| Mock | simulation | — | `services/companion-rs/src/seed.rs` | offline demo data; the phone labels it **Simulation** (ADR-009) |

## Limits that matter

- **OpenCode.** Sessions stream a live transcript: text and reasoning as they are
  generated, tool cards keyed by OpenCode's `callID`, approvals inline on their card (allow
  once; always allow the offered pattern until OpenCode restarts; deny, optionally with a
  reason the model reads), questions from OpenCode's `question` tool, the todo list as a plan,
  retries as notices, per-turn usage. A prompt sent mid-turn runs as its own turn afterwards.
  Stop shows the turn as interrupted, not failed. MCP status is polled every 30 s while a
  phone watches; sign-in works on the desktop or by pasting the dead callback page from the
  phone browser, and failed servers can be reconnected. `limited`: diff (summary diffs are
  mapped, but no populated capture yet), MCP sign-in (completing a real OAuth login is not
  verified live). End and permission modes are ADR-021's (below). There is no SSE replay:
  after a reconnect, state is re-listed and watched transcripts are rebuilt from
  `GET /session/{id}/message`. OpenCode's conversation text is not journaled (prompts sent
  from the phone are, as part of their command records: `PRIVACY.md`).
  Since ADR-021 (2026-11-12) OpenCode is at Claude's level on the phone: the folder `opencode
  serve` runs in (`GET /path`, checked like a Claude workspace) is the agent's workspace, so
  Files, Changes, Pull and the opt-in Terminal work and New task picks it; the model picker
  lists `GET /config/providers` and sends the chosen `provider/model` with each prompt
  (OpenCode has no per-session model setting); photos go as `data:` file parts; slash commands
  come from `GET /command` and run through `POST /session/{id}/command`; the context meter
  uses the last answer's tokens against the model's limit; rename is `PATCH /session/{id}`;
  `submit_prompt {fork}` forks natively. End aborts a running turn and ends the session on
  the companion's side (OpenCode has no end route; `DELETE /session` would destroy the history,
  so it is never called); later prompts are refused until the companion restarts. The
  permission-mode chip picks OpenCode's built-in agents: Build (`default`) and Plan (`plan`),
  sent as `agent` with every prompt; user-defined agents are not offered, since one may allow
  what Build asks for. Terminal runs: `opencode run --attach http://127.0.0.1:4096 "..."` goes
  through the watched server and streams live; a plain `opencode run` uses its own private
  server, so its session lists on the next probe (with its history) but does not stream.
- **Ollama.** It is a model provider, not an agent. Session operations report
  `unavailable` (PRD §7.7).
- **Claude Code.** Without `workspaces` it is ingress only: the companion observes hook
  events from sessions started in VS Code or a terminal (only `SessionStart` was captured
  live; other hook shapes are parsed defensively and reported `limited`). With
  `workspaces` the phone drives its own sessions: create, prompt (follow-ups join the
  running turn), interrupt, inline approvals with Claude's options, permission mode
  (default, accept edits, plan), end, a streaming transcript, and MCP status, sign-in and
  reconnect. Sessions running elsewhere on the PC are mirrored, forkable and, with the
  explicitly installed `PermissionRequest` hook (`chernobyl-companion --install-hooks`, undo
  with `--uninstall-hooks`, or `setup-windows.ps1 -PhoneApprovals`), approvable from the
  phone (ADR-018). Assumed, not verifiable headless: an interactive Claude Code shows its own
  dialog while the hook waits and cancels the hook when you answer on the PC. Questions and
  plan approvals of PC sessions stay on the PC. Desktop MCP sign-in
  is `limited` (whether the CLI's own listener reconnects by itself is unverified, so the
  companion polls and reconnects). **Adding a folder to `workspaces` is trusting it:** the
  CLI runs in print mode, which skips Claude's folder-trust dialog, so the folder's
  project hooks and MCP servers load on spawn. Hooks of a driven session are ignored, so it
  is not shown twice. Set `binary` only to a real `claude.exe` (no `.cmd` shim).
  Workspaces are checked when the companion starts: a bad folder leaves the entry
  ingress-only with `lastError` on `GET /adapters`. `maxSessions` (1-16, default 3) caps live
  processes; one new session per 5 s. A process idle for 30 min with no phone watching is
  closed and resumed on the next prompt. A companion restart drops driven sessions from the
  phone (their transcripts stay in Claude's own files).
- **Antigravity (ADR-022).** Driven through `agy acp`, not the headless `-p` stream-json mode,
  which cannot ask for permissions. Sessions stream a live transcript: text as it is generated
  (agy streams no thinking text), tool cards with the command, its output and edit diffs, a
  plan, and a turn footer with duration and tokens. Approvals are inline on the card with agy's
  own labels, **once only**: agy's "always" options are never offered, and the companion never
  writes agy's settings. Follow-ups steer the running turn. Stop is `session/cancel` (the turn
  ends interrupted; `unknown` after 30 s). The phone also picks the model (agy's own list),
  attaches photos, runs agy's slash commands (its built-ins and your skills), sets the mode
  (default, accept edits, plan), renames (the companion's title) and ends (`session/close`).
  Not offered: MCP status (agy reports none, so the chip is hidden), a context meter (agy
  reports no window size), forks, and sessions running on the PC. History: the conversations agy
  lists for each workspace (`session/list`, once at boot, then once a minute while agy runs),
  replayed by `session/load` when the phone opens one and continued by `session/resume`. When
  agy exits or closes after 10 idle minutes with no phone watching, the next prompt respawns it
  and resumes. Sessions are capped by `maxSessions` (1-16, default 3, running or waiting) and
  one new session per 5 s; workspaces are checked when the config loads (Claude's policy). agy
  runs commands through PowerShell found on PATH: a companion started with a PATH that lacks
  `WindowsPowerShell\v1.0` makes every approved command fail (seen in the live run with a
  trimmed PATH). agy's own rules still apply: what your agy allow-list or `default` mode
  auto-allows (writes inside the workspace) runs without a phone prompt.
- **Codex** (live-verified 2026-11-12, codex-cli 0.167.0-alpha.4.1, ADR-015 §10). Sessions,
  prompts, interrupt, the streamed transcript, approvals and diffs are `available`; the session
  list, history, spawn and MCP capabilities are `limited` with their reasons. Approvals carry
  exactly the options codex lists (live: allow, the proposed exec-policy amendment as a
  persistent option, deny and stop; file changes also allow for this session and deny);
  follow-ups steer the running turn; the composer has codex's own models (`model/list`),
  camera photos, the context meter and rename. The 50 newest Codex threads whose folder is a
  workspace, the Codex app's included, are listed, replayed on open and resumed by a prompt;
  one the Codex app wrote in the last 2 minutes is "Live on PC" (re-read at the probe cadence,
  continued on the phone only as a fork). Not offered: ending a session, permission modes,
  slash commands, phone-side MCP sign-in, a live token stream of a thread running in the
  Codex app (the app's app-server is a private child; nothing to attach to). When the
  app-server exits, its sessions idle; the next prompt respawns it and resumes the thread. A
  turn the account refuses (a usage limit) fails with the notice and the session goes idle. An
  interrupt with no turn end in 30 s is `unknown`; a desktop sign-in expires after 10 min.
  Sessions are capped at 8 open per adapter and one new session per 5 s. **Adding a folder to
  `workspaces` is trusting it:** Codex runs there with `approvalPolicy: untrusted` (it asks
  the phone before every file change and every command it does not know as read-only) and
  without its sandbox (`danger-full-access`; the Windows sandbox misbehaved in the live run),
  and codex marks the folder trusted in its own `config.toml`. Workspaces are checked when the
  config loads, and a bad folder stops the companion from starting. Leave `binary` out: the
  companion finds `codex.exe` on PATH or in the Codex app's install at every start (never a
  `WindowsApps` alias); npm's `codex.cmd` shim is refused (no shell is ever used). The
  companion runs one `codex app-server` from boot (about 200 MB) to list threads.
- **Workspace files and git (ADR-016, `workspace.v1`).** Any Claude, Codex or Antigravity workspace on the
  allow-list can be browsed read-only from the phone and committed and pushed from it. Git is
  the desktop's own `git.exe` (found on PATH, else `C:\Program Files\Git\cmd\git.exe`; any
  version with `git restore`, 2.23+), run with no shell and no prompts: a push that needs a
  credential prompt fails instead of opening a window, so pushes rely on a credential
  helper or SSH key that already works without asking. Git actions need the workspace to
  be the repository root. Push sends the checked-out branch to its configured upstream only,
  never forced. `git pull` is fast-forward only from the upstream (never a merge commit or rebase; ADR-016 §7). Verified against Git for Windows **2.55.0** with temporary
  repositories and a local bare remote (`workspace::tests`); no hosted remote run yet.

## Updating this page

When an adapter is verified against a new runtime version, update the version column and the
adapter's capability notes in the same commit. Do not mark a capability `available` from a
fake-server test alone. Fault-injection tests prove the wiring; only a live run proves
support.
