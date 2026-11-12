# ADR-021: OpenCode at the same level as Claude Code on the phone

**Status:** Accepted 2026-11-12 (delegated by the owner). Implemented on branch `feat/opencode-parity`.
**Relates to:** ADR-015 (live sessions, §4.4 OpenCode), ADR-016 (workspace files and git), ADR-017 (composer), ADR-018 (fork), ADR-020 (terminal), threat model `PHASE5-THREAT-MODEL.md` §3.12, `docs/reverse-engineering/opencode-live-events.md`, `apps/mobile/DESIGN.md`.

## 1. Context

Until now OpenCode advertised `composer: []` and `workspaces: []`. The phone hid the model chip, photos, slash commands, the context meter and rename for OpenCode sessions, and Files, Changes and Terminal were unreachable when OpenCode was the only agent. Every operation below was checked against the live OpenAPI document (`GET /doc` on `opencode serve` 1.19.4, 190 operations) before it was used.

## 2. Decision

| Feature | OpenCode route (1.19.4) | Companion |
|---|---|---|
| Workspace | `GET /path` → `directory` | Validated with `claude_live::validate_workspace` (absolute, existing, canonical; not a drive root, not the profile folder or a folder containing it, not a folder containing the CHERNOBYL data dir). On success it is the agent's single `Agent.live.workspaces` entry, so ADR-016 Files/Changes/Pull and ADR-020 Terminal work unchanged (they look workspaces up across every runtime). Sessions whose `directory` is that folder (case-insensitive, either slash) carry the workspace's own path as `Session.project`, so the conversation's Files / Changes chips find it. `create_session.project` is accepted only when it is that workspace id; any other folder is refused as before. Re-read on every probe (15 s); an adapter built without the data dir (probes) advertises none. |
| Models | `GET /config/providers` (providers → models with `name`, `limit.context`, `status`), `GET /config` → `model` | `provider/model` values, the configured model first, deprecated ones and ids that fail the wire rule left out, at most 32; on `Agent.live.models` (new optional field, for New task) and `LiveState.models`. `create_session.model` and `set_model` take only a listed id. The choice is kept per session and sent as `model: {providerID, modelID}` with every `prompt_async` (and as `model: "provider/model"` with `/command`): OpenCode has no per-session model setting (`PATCH /session/{id}` takes `title`, `metadata`, `permission`, `time` only), so `set_model` completes at once and applies from the next prompt. `LiveState.model` is the chosen model, else the model of the session's last answer, else the configured one. |
| Photos | `prompt_async` `parts[]` `{type: "file", mime, filename, url: "data:<mime>;base64,…"}` | After the text part, same ADR-017 bounds (4 images, 180,000 base64 characters). The user item shows the count (from the user message's image `file` parts, live and on hydration). A model that cannot read images gets OpenCode's own substitute; the phone still sends. |
| Slash commands | `GET /command` (names); `POST /session/{id}/command {command, arguments, model?, parts?}` | Names on `LiveState.slashCommands`. A prompt `/name args` whose name OpenCode listed goes to `/command` instead of `prompt_async`. That call answers only when the turn ends, so it runs on a thread; the turn's outcome closes the prompt from the event stream like any prompt; a refusal before the turn started (4xx, connection error) fails the prompt. Unknown `/x` is plain text. |
| Context | assistant `message.updated` `info.tokens` `{input, output, reasoning, cache: {read, write}}`, `providerID`, `modelID` | `usedTokens` = the sum of the last assistant message's tokens; `maxTokens` = that model's `limit.context`; no state when the limit is unknown (the chip hides). Pushed when it changes and on hydration. |
| Rename | `PATCH /session/{id} {title}` → the session | `rename_session` completes on the 2xx answer; the session is re-mapped and `session_updated` goes out. |
| Fork | `POST /session/{id}/fork {}` (`messageID` optional) → the new session, with all messages | `submit_prompt {fork: true}` forks, then prompts the fork; the ack names the fork, `forkedFrom` = the original title (kept across OpenCode's own updates), the chosen model carries over. The original is not written. The phone offers fork only for `livePc` sessions, which OpenCode never has (every OpenCode session is promptable directly), so this is reachable from the protocol, not from a button. |

`Agent.live.composer` for OpenCode: `model` (when the provider list loaded), `images`, `commands` (when `GET /command` listed any), `context`, `rename`. A server that answers none of the new routes still probes healthy with `images`, `context`, `rename` only.

**Wire rule for models (both stacks, golden fixtures `composer-valid.json` / `composer-invalid.json`).** `isModelName` / `is_model` now also take `provider/model`: at most 64 characters, provider `[A-Za-z0-9._-]`, model `[A-Za-z0-9._:/@+-]`, neither part empty or starting with `-` (or `/` for the model). The Claude runtime checks `is_claude_model` (the old rule) itself, so an OpenCode id never reaches `claude.exe --model`; OpenCode receives the id as JSON, never as an argument.

**Phone.** Unchanged screens; the controls appear because the agent now advertises them. New task: the model row lists `live.models` when present (`newTaskModels`: Default first, then the list), else Claude Code's aliases; switching agents resets it to Default. The model and rename sheets name the agent ("OpenCode on your PC decides which models…").

## 3. Security

- **T1/T11 (folder trust).** The serve's folder becomes browsable from the phone without an explicit `workspaces` entry. Attaching an OpenCode endpoint already lets the phone drive an agent that reads and writes there, so the companion treats the serve's folder as trusted the same way, with the same exclusions as a configured workspace (never a drive root, the profile folder or a folder holding the companion's data). Git actions keep ADR-016 W1-W9; the terminal stays off unless `terminal.enabled`.
- Model ids are allow-listed against OpenCode's own list before they are stored or sent. Rename is the ADR-017 title rule. Images keep the ADR-017 bounds and are never journaled or put in the transcript (count only).

## 4. Not possible with OpenCode 1.19.4

- **Ending a session and permission modes:** no route (unchanged from ADR-015 §4.4).
- **A per-session model setting:** none; the model rides each prompt.
- **Persistent "always" rules:** OpenCode's `always` reply lasts until the server restarts (unchanged).
- **Live mirror / hook approvals for other processes (ADR-018 §2.1-2.2):** not needed: every session on the server, including those a TUI attached to it creates, streams on the SSE bus. A separate `opencode run` (its own in-process server on the same data directory) does not reach the attached serve's event bus: its session appears on the next probe and replays from `GET /session/{id}/message`, but is not streamed while that process runs.

## 5. Verification

- Rust unit tests against the fake server (`opencode.rs`): `serve_folder_is_the_workspace_and_the_composer_is_advertised`, `create_takes_the_workspace_a_listed_model_and_photos`, `set_model_rename_and_slash_commands`, `fork_creates_the_fork_and_prompts_it`, `photos_count_on_the_user_item_and_the_context_meter_fills`, `slash_and_model_helpers`; `protocol.rs` model rule and `Agent.live.models` serialization; TS `composer.test.ts` (wire rule), mobile `composer.test.ts` (`newTaskModels`, labels).
- Live (`tests/opencode_e2e.rs` `live_opencode_parity`, ignored; isolated `opencode serve` 1.19.4, Ollama `qwen3.5:4b` with a 24k-context alias, free): see §6.

## 6. Live run

`live_opencode_parity`, 2026-11-12, opencode 1.19.4 + Ollama `qwen3.5:4b` (24k alias), about 29 min on CPU, free. Run 2 passed every step through fork: workspace advertised (14 models, all five composer features), a phone-chosen folder refused, an `opencode run` session in the same folder listed on the next probe and replayed (stdout "RUNOK"), create with the chosen model (the assistant messages report `modelID` = the alias; context reported, 8,109 of 24,576 tokens later on), bash asked with Allow once / Always allow `echo *` / Deny and ran after Allow once, a queued follow-up completed after the first turn, Stop cancelled the prompt and interrupted the turn, rename, `set_model`, slash commands listed (`init`, `review`, `hello`, `customize-opencode`) and `/hello` ran through `/command` (SLASHOK), a photo prompt completed with the count on the user item, fork created a new session and its turn completed. It stopped at `fs_read note.txt`: the companion honestly answered "binary file", because OpenCode's shell was PowerShell 5.1 (no `SHELL` set in the test's environment) and its `>` writes UTF-16. The test now reads `README.md`; run 3 (with that change) was in progress when this was committed.
