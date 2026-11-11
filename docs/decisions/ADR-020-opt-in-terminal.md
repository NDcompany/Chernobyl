# ADR-020: Opt-in one-shot terminal from the phone

**Status:** Accepted 2026-11-12 (delegated by the owner). Implemented on branch `feat/extras`.
**Relates to:** ADR-011 (E2E), ADR-015 (T1, T2, T6, T7; live messages are E2E-only), ADR-016 (workspace allow-list, command lifecycle, audit), threat model `PHASE5-THREAT-MODEL.md` §3.12.

## 1. Context

ADR-016 deliberately left out "a terminal or arbitrary command execution". From the phone the owner can steer an agent, review its work and ship it, but cannot run `npm test`, look at a log or restart a dev server without asking an agent to do it (and approving its tool call). Comparable phone clients offer a shell. The owner asked for one, opt-in.

## 2. Decision

A **one-shot command runner**, not an interactive PTY: the phone sends one command line, the PC runs it to completion, the output streams back.

- **Off by default.** `companion.json` `"terminal": {"enabled": false}` per companion; only `true` turns it on (`scripts/setup-windows.ps1 -EnableTerminal`). Only then does `hello_ack.features` carry `terminal.v1` and the phone show a Terminal action per workspace.
- **Messages** (additive, protocol stays 2; journaled with the ADR-016 command lifecycle):

| Message | Answer |
|---|---|
| `terminal_run {commandId, workspaceId, command}` (1-4,096 UTF-16 units) | `ack` once the shell is running; `terminal_output {commandId, workspaceId, data}` chunks to the starting connection only; `terminal_exit {commandId, workspaceId, exitCode\|null, reason: exit\|timeout\|stopped, truncated}`; then one `command_result` (`completed` for exit code 0, otherwise `failed` with "exited with code N", "timed out after 120 s" or "stopped from the phone") |
| `terminal_stop {commandId, workspaceId}` | `ack` (or "nothing is running in this workspace"); kills the run's process tree. Not journaled: the run's own result records the stop |

- **Execution.** `cmd.exe /d /s /c "<line>"` on Windows (`%SystemRoot%\System32\cmd.exe`, no AutoRun, the line passed verbatim with `raw_arg`), `/bin/sh -c` elsewhere. This IS a shell, by design and by explicit opt-in. cwd = the canonical allow-listed workspace; `child_env` (no `HOST_TOKEN`, `CHERNOBYL_*`, `NODE_OPTIONS`); stdin closed; `CREATE_NO_WINDOW`. A blank line, a NUL or a line break is refused.
- **Bounds.** 120 s, then the tree is killed (`taskkill /T /F`, or the process group); Stop from the phone does the same. Output: stdout and stderr merged, line-buffered (a partial line goes out after 250 ms or 8 KiB), ANSI/VT escapes and other control characters stripped, `runtime_io::redact`ed, batched into frames of about 16 KiB or 100 ms, at most 1 MiB per run (`truncated`, the rest is drained and dropped). One run per workspace; one start per 2 s per companion.
- **Record.** Every run is a journaled command whose payload carries the command line redacted, plus an audit record `terminal-run` (`<workspace id>: <redacted line>`) written before the process starts. The output is never journaled. The phone keeps the last 20 command lines per workspace in its local storage; output lives in screen state only.
- **Code.** `services/companion-rs/src/terminal.rs` (runner, `Terminals` busy/rate state, ANSI strip), `companion.rs` (`terminal_run`, `terminal_stop`), `server.rs` (sink and E2E gate), `protocol.rs` / `packages/protocol/src/index.ts` (types, validators), golden fixtures `packages/protocol/fixtures/live/terminal.json`, `client-valid.json`, `client-invalid.json`. Phone: `app/terminal/[workspace].tsx`, `src/data/terminal.ts`, `src/utils/terminal.ts`.

## 3. What it bypasses

The agents' approval flow. A command an agent would have to ask for (`rm -rf`, `git reset --hard`, `curl … | sh`, reading `~/.ssh`) runs immediately, with the companion user's full rights, anywhere that user can reach: the workspace is the starting folder, **not a sandbox** (`cd ..` works). The ADR-016 containment (W1, W2) and the git rules (W6: no force, no reset) do not apply inside a terminal command. Whoever holds an unrevoked paired phone with the terminal on holds a shell on the PC.

## 4. Threats and mitigations

| # | Threat | Mitigation | Tests |
|---|---|---|---|
| X1 | Arbitrary execution from a stolen phone | Off by default, operator opt-in in a file on the PC; a run needs a live E2E session from a paired device on every posture, loopback included (the server hands the companion an output sink only when `outbound.has_session()`; without one: "the terminal needs a paired device with a live E2E session"); revocation (ADR-014) cuts the device off | `terminal_is_advertised_when_enabled_and_needs_a_session`, `terminal_is_refused_unless_enabled_and_sealed` |
| X2 | Unnoticed use | Audit record before the process starts, the journaled command and its outcome; the phone shows a banner "Runs on your PC without approval prompts" | `terminal_runs_streams_audits_and_stops` |
| X3 | Secrets in output or in the typed line | Output redacted per line (known shapes); the journal and the audit keep the line redacted; output never journaled and never persisted on the phone | `output_is_capped_redacted_and_batched`, `git_pull_and_terminal_messages_parse_within_bounds` |
| X4 | The token leaks to the command | `child_env` (T6) | `the_scrubbed_env_hides_the_token` |
| X5 | Runaway or hostile output | 120 s tree kill, Stop, 1 MiB cap, batching, control characters and escape sequences stripped (no terminal-escape injection into the phone UI), one run per workspace, one start per 2 s | `a_timeout_kills_the_process_tree`, `a_stop_kills_the_run`, `strips_ansi_and_controls`, `one_run_per_workspace_and_a_start_interval` |
| X6 | Another client sees the output | `terminal_output`/`terminal_exit` go to the starting connection only, sealed | interop check "terminal: … observers see no output" |

**Residuals.** Everything in §3. Redaction recognises known shapes only, and a partial line flushed by time can split a secret. A command that detaches a grandchild (`start notepad`) outlives the run once the shell exits; a timeout or Stop kills the whole tree only while the shell is alive. cmd.exe writes non-UTF-8 code-page text for some built-ins; it shows as replacement characters. If the starting connection drops, the run continues to its end or timeout and its output is lost; the phone shows the outcome from `command_result` after reconnecting, and Stop still works from any connection. The command history on the phone is in the app's SQLite storage, unencrypted like the rest of its settings.

## 5. Alternatives rejected

An interactive PTY (ConPTY FFI, a VT renderer on the phone, keystroke streaming: much more surface for little gain over one-shot runs); an allow-list of commands (the owner wants a terminal; a list is a weaker agent); routing commands through an agent's approval (that is what agents already do).
