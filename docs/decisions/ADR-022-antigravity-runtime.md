# ADR-022: Google Antigravity as a live runtime (`agy acp`)

**Status:** Accepted 2026-11-12 (delegated by the owner). Implemented on branch `feat/antigravity`.
**Relates to:** ADR-015 (live sessions, LiveRuntime, T1-T11), ADR-016 (workspace files and git), ADR-017 (composer), ADR-018 (sessions on the PC), ADR-020 (terminal), threat model `PHASE5-THREAT-MODEL.md` §3.12, `docs/reverse-engineering/codex-and-antigravity.md` Topic B (B9 for 1.5.6).

## 1. Context

The owner uses Google Antigravity's CLI, `agy`, next to Claude Code and Codex, and wants it on the phone as a live runtime with the same experience as far as agy allows.

The plan written against agy 1.4.7 assumed the headless `--output-format stream-json` mode: it streams text and tool steps but cannot ask for a permission. A tool that needs one is auto-denied and listed in `result.denied_actions`. That plan was **approve-then-retry**: raise an approval after the denial, write a narrow allow-rule into a per-session `--settings` file, respawn with `--conversation <id>` and tell the model to try again. Interrupt meant killing the process.

Re-verifying on the installed **agy 1.5.6** (B9) changed the picture. Since 1.5.0 agy ships `agy acp`, an [Agent Client Protocol](https://agentclientprotocol.com) server over stdio. Verified live: it asks for permissions inline (`session/request_permission`), stops a turn with `session/cancel`, merges a prompt sent mid-turn, carries real command output and edit diffs, takes images, lists models, modes and slash commands, and lists, loads and resumes past conversations.

## 2. Decision

**Drive `agy acp`, not stream-json.** One `agy acp` child per adapter, spawned on the first command that needs it; every phone session is an ACP session inside it. Code: `services/companion-rs/src/agy.rs` (protocol core: JSON-RPC client, `session/update` decoder, approvals, launch config) and `agy_runtime.rs` (the `LiveRuntime`). It is the Codex shape (one child, sessions inside, ADR-015 §4.3) with Claude's composer.

- **Config.** `{"id": "agy-1", "type": "antigravity", "workspaces": [...], "binary"?, "model"?, "maxSessions"?}`. Workspaces use Claude's policy (`claude_live::validate_workspace`): absolute, existing, canonical; never a drive root, the profile folder or one holding the data folder; checked at config load (a bad folder stops the companion). The phone names a workspace by id and never sends a folder. `binary` defaults to `%LOCALAPPDATA%\agy\bin\agy.exe`, then `agy.exe` on PATH; `.cmd`/`.bat` are refused. `model` is the default model for new sessions; `maxSessions` (1-16, default 3) caps sessions that are running or waiting. One create per 5 s. No `args` key: nothing in the file or from the phone reaches agy's command line, which is always `agy acp`.
- **Spawn.** `ChildIo::spawn` with no shell, `child_env` (T6) and the first workspace as cwd; each session passes its own `cwd` to `session/new` with `mcpServers: []`. Never `--dangerously-skip-permissions`, and never `--mode auto`: the only modes the phone can set are `default`, `acceptEdits` and `plan`, sent as `session/set_mode` `default | accept-edits | plan`.
- **Transcript.** `agent_message_chunk` and `agent_thought_chunk` stream into text and thinking items. `tool_call` and `tool_call_update` are merged per `toolCallId` into one card: the command line as title and input, the output from `content` or `rawOutput.output` (head and tail, redacted), and diffs from ACP `diff` blocks (`oldText` → `newText`, or all-added). Before the edit runs, `write_file {Path, Content}` and `edit_file {Path, Target, Replacement}` already show their proposed change. Also mapped: `plan` to a plan item, `available_commands_update` to the slash menu, `current_mode_update` to the mode chip. Each `session/prompt` is one turn, and its reply carries the footer: duration, and input, output, cache-read and thought tokens. agy reports no context window, so there is no context meter.
- **Approvals (inline, G2).** `session/request_permission` becomes an engine `PermissionRequest` (`<adapter>:<process generation>-<agy id>`) attached to the tool card, with agy's own labels. The title is "Antigravity wants to run a command", "change a file", "fetch a URL" or "use `<tool>`"; there is a shell or edit detail, and the risk is high, medium or low. Only `allow_once` and `reject_once` are offered. `allow_always` and `reject_always` are dropped because their scope is agy's own settings files and the phone grants nothing persistent; if no deny remains, the phone offers "Deny and stop", which answers `cancelled`. The engine validates the decision before the one answer `{"outcome":{"outcome":"selected","optionId":…}}` is written. An approval left open when its turn ends, on interrupt or on exit goes `stale`.
- **Follow-ups.** `session/prompt` at any time; agy merges a mid-turn prompt (`promptWhileBusy: "merge"`): the running prompt replies `end_turn` and the new one carries on as the next turn.
- **Interrupt.** `session/cancel` (a notification). Open approvals are answered `cancelled` (ACP requires it) and withdrawn. The command completes when the turns end (`stopReason: "cancelled"`), or becomes `unknown` after 30 s.
- **Resume, exit and idle.** When agy exits, its sessions go idle with a notice, and the next prompt respawns agy and `session/resume`s the session, then sends the prompt (not a create, so no cap or rate slot). A resume agy refuses ends the session. If nothing is in flight and no phone is subscribed for 10 min, agy's stdin closes (quiet: no error notice) and the next prompt resumes.
- **History.** `session/list {cwd}` for every workspace, once at boot (this spawns agy; the idle close stops it again) and then at most once a minute on the probe cadence while agy runs. Listed conversations show as idle sessions. The first subscriber gets one replayed by `session/load` (no model call), and a prompt adopts it. This replaces reading `~/.gemini/antigravity-cli/conversations/<id>.db` (SQLite with protobuf payloads; no crates).
- **Composer.** Model picker from `session/new`'s `models.availableModels` (advertised once known, also on `Agent.live.models` for the new-task screen), images (ACP `image` blocks, verified live), slash commands, rename (the companion's title; agy keeps its own) and end (`session/close`). There is no context meter, no MCP and no fork.
- **Protocol (additive, protocol stays 2).** `Agent.kind` gains `antigravity`; `Agent.live.models?: {value, label}[]`; the wire model check (`isModelName` / `is_model_name`) accepts a Claude model, an OpenCode `provider/model` id, or any runtime model name (lowercase letters, digits, `.`, `_`, `-`, starting and ending with a letter or digit, at most 64 characters; merged with the Codex rule on 2026-11-15), and each runtime checks the id against its own list. Claude keeps its narrower `is_claude_model`.
- **Phone.** An original 8×8 mark (a little flying saucer on its beam, violet `#8B5CF6`, never Google's logo), always with the name "Antigravity" beside it, in the app and in the KitKat client. Approvals look like every other live approval: the card, the Approvals tab and the notification. The MCP chip stays hidden because agy reports no servers. Files, Changes, Pull and Terminal work on Antigravity workspaces like any other (ADR-016, ADR-020).

## 3. Why not approve-then-retry

It was the only option under stream-json, and it is worse on every axis that matters. It writes allow-rules, even per session, and someone has to validate their breadth. It loses the turn: the model was told "the user denied" and must be talked into retrying. It replays the conversation through a respawn for every approval. It cannot interrupt without killing the process, shows no command output (only a summary) and no diffs. ACP removes all of that. The companion never writes an agy settings file, so the threat-model item "per-session settings file, narrow rules, never global" reduces to "no settings are written at all". What remains of the old plan: if a future agy drops `acp`, stream-json with approve-then-retry is the fallback (B2-B4 stay documented).

## 4. Limits versus Claude Code

| | Claude Code (ADR-015/017/018) | Antigravity (this ADR) |
|---|---|---|
| Transport | `claude.exe` stream-json, one process per session | `agy acp`, one process per adapter |
| Text / thinking | token deltas / summarized thinking | chunk deltas / no thinking text (agy streams none) |
| Tool cards | input, output, diffs | input, output (agy's text), diffs from ACP `diff` blocks |
| Approvals | inline; once, session, persistent rule (confirmed) | inline; once only (`allow_always` dropped) |
| Follow-up while busy | queued as its own turn | merged into the running turn |
| Interrupt | control request | `session/cancel` |
| Permission modes | default, acceptEdits, plan | the same three (`accept-edits` on the wire) |
| Model / images / commands / rename / end | yes / yes / yes / CLI rename / yes | yes / yes / yes / companion title / `session/close` |
| Context meter | yes | no (agy reports no window) |
| MCP status, sign-in | yes | no (agy acp exposes none; chip hidden) |
| History | 50 newest on disk, replay, resume, fork | `session/list` per workspace, `session/load` replay, `session/resume`; no fork |
| Sessions on the PC (ADR-018) | live mirror and hook approvals | no (agy has no hooks to the phone; its own Remote Control is Google's) |

## 5. Threats

ADR-015 T1-T11 apply unchanged (§3.12 records the Antigravity row). Specific to agy:

| # | Threat | Mitigation | Tests |
|---|---|---|---|
| G1 | The phone widens agy's permissions | Only once-only options are offered; `allow_always` and `reject_always` never reach the phone; no settings file is written; modes are limited to `default`, `acceptEdits` and `plan`; `--dangerously-skip-permissions` and `--mode auto` have no path (no `args` key, a fixed argv) | `approvals_offer_once_only_options_and_answer_inline`, `model_mode_close_and_errors`, `launch_config_applies_the_workspace_policy` |
| G2 | A forged or late answer | The engine validates every decision (pending, revision, option bucket); one answer per agy request id; ids are scoped per process generation, so a respawned agy reusing `"1"` never matches an old record; withdrawn on turn end, interrupt and exit | `antigravity_boots_routes_approvals_and_serves_its_workspace`, `interrupt_cancels_asks_and_completes_with_the_turn`, `exit_fails_commands_and_idles_sessions` |
| G3 | agy asks the client to read or write files, or to run terminals | The client offers no `fs` or `terminal` capability; any other request gets `Method not found` | `approvals_offer_once_only_options_and_answer_inline` |
| G4 | History of other folders | `session/list` is asked per allow-listed workspace and each result is kept only if its cwd canonicalizes inside one (`claude_history::inside`) | `history_is_listed_loaded_and_adopted` |
| G5 | A model id as a flag | The wire check refuses anything not starting with a lowercase letter or digit (so never `-`/`--flag`); the id travels only inside JSON-RPC params, never argv | protocol `is_model_name` tests, `composer-invalid.json` |

**Residuals.** agy loads the user's own MCP servers, rules, skills and allow-lists, as it does in the terminal. Its auto-allowed operations (workspace writes in `default` mode, anything the user's allow-list covers) run without a phone prompt. Paired-phone holders get what the agy login can do, inside those rules. The boot history listing starts agy once (about 200 MB) for up to 10 minutes.

## 6. Verification

Unit tests against a pipe peer (`agy::tests`, `agy_runtime::tests`) and a fake `agy acp` (`tools/fake-agy-acp.mjs`, the real spawn path, also through `Companion::boot`). Live: the `#[ignore]` test `agy_live_create_approve_follow_up_interrupt_resume` passed on 2026-11-12 against agy 1.5.6 with `gemini-3.8-flash-low`, in 41 s and about 75k tokens (input plus output; quota on the owner's agy login). It covered create (8 s to the first answer), an inline approval for `hostname`, a follow-up, a `ping` interrupted in 1.9 s (`cancelled`, then `completed` for the stop), and a resume in a new process that remembered the first answer. That run found two things. First, item and turn ids restarted after a respawn and overwrote the earlier ones in the hub; they are now scoped by process generation (asserted in `deny_interrupt_exit_and_resume`). Second, agy runs commands through PowerShell from `PATH`, so a companion started with a PATH that lacks `WindowsPowerShell\v1.0` fails every approved command (documented in `INTEGRATIONS.md`). An earlier attempt that answered only the first approval waited on a second one the model asked for; a phone answers each as it comes. Emulator rows are not run yet.

## 7. Alternatives rejected

- stream-json with approve-then-retry (§3).
- Reading agy's conversation SQLite files directly: no crates allowed, protobuf payloads, and the format is private. `session/list`/`load` give the same data.
- agy's own `--remote-control`: Google's cloud relay and browser UI, outside CHERNOBYL's channel.
- The desktop app's language-server Connect API (B5): internal and undocumented, with a per-process CSRF token.
