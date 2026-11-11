# ADR-017: Claude composer extras on the phone (model, photos, slash commands, context, rename)

**Status:** Accepted 2026-11-12 (owner-delegated). Built on main.
**Relates to:** ADR-015 (live sessions: transcript protocol, composer, T1-T11), ADR-011 (sealed frames), ADR-009 (mock honesty), threat model `PHASE5-THREAT-MODEL.md` §3.12, `docs/PRIVACY.md`, `apps/mobile/DESIGN.md`.

## 1. Context

ADR-015 gave the phone a Claude Code conversation: prompts, Stop, inline approvals, permission mode, MCP. Hosted phone clients for Claude Code also let the user pick the model, attach a screenshot or photo, run slash commands, watch the context window fill up and rename the session. The CLI's stream-json control protocol already has all of it. Verified on `claude` 2.1.322 with no API call (`docs/reverse-engineering/claude-code-streaming.md` §4.4):

- `initialize` answers with `models: [{value, displayName, …}]` and `commands: [{name, description, argumentHint, aliases?}]`; `system/init` repeats the names as `slash_commands`, and `system/commands_changed` sends a new list.
- `set_model {model}` answers `{model: <resolved id>}` (`default`, `sonnet`, `haiku` and full ids all work) or an error for an unknown model.
- `get_context_usage` answers `{totalTokens, maxTokens, percentage, categories, …}` without calling the API.
- `rename_session {title, source: "host", session_id}` answers `{}` and emits `system/session_title_changed` (the title lands in the session's JSONL as `custom-title`, which `claude_history` already reads).
- A `user` frame may carry Messages API image blocks before the text (`{type:"image", source:{type:"base64", media_type, data}}`): a 2x2 red PNG was answered "Red".
- Slash commands sent as plain prompt text run in stream-json mode and answer with text (`/context`, `/model`, `/usage`, `/mcp`, `/config`, `/rename x`, `/effort` all tried, cost 0). `/clear` starts a **new session id**, which the companion would lose track of.

## 2. Decision

**Protocol (additive; old phones and companions ignore the new optional fields).**

| Where | Addition |
|---|---|
| `Agent.live.composer` | `Array<'model' \| 'images' \| 'commands' \| 'context' \| 'rename'>`: what this runtime takes. Claude sends all five; Codex, OpenCode and the mock send none, so the phone hides the controls. |
| `submit_prompt`, `create_session` | optional `images: [{mediaType, data}]` |
| `create_session` | optional `model` (passed as `--model` at spawn; `default` passes nothing) |
| client messages | `set_model {commandId, sessionId, model}`, `rename_session {commandId, sessionId, title}` (mutating: journaled, deduplicated, G1 lifecycle like `set_permission_mode`) |
| `LiveState` (`state` op) | optional `models: [{value, label}]`, `slashCommands: string[]`, `context: {usedTokens, maxTokens}` |
| user `TranscriptItem` | optional `images: number` (a count; never the bytes) |

**Validation at the trust boundary** (TS `isClientMessage` and Rust `parse_client_message`, same golden fixtures `packages/protocol/fixtures/live/composer-*.json`):

- *Model:* an alias (`default`, `opus`, `sonnet`, `haiku`, `fable`, `opusplan`) or `claude-[a-z0-9-]+`, at most 64 characters. Nothing a flag parser could read as a flag; spawn takes it as one argv element, never through a shell.
- *Title:* 1-80 UTF-16 units, not blank, no control characters.
- *Images:* at most 4; media type `image/png`, `image/jpeg`, `image/webp` or `image/gif`; strict base64 (alphabet, length a multiple of 4, at most 2 padding characters) that starts with that format's signature (`iVBORw0KGgo`, `/9j/`, `R0lGOD`, `UklGR`); **180,000 base64 characters for all images together** (about 131 KiB of JPEG).

**Why 180,000 and not 1.5 MB per image.** The companion reads one WebSocket frame of at most 256,000 bytes (`ws::MAX_PAYLOAD_BYTES`) and does not reassemble fragments; a sealed frame is the JSON plus a 16-byte tag, base64-encoded (ADR-011), so a message can be about 191,900 bytes of JSON. Raising the frame cap or chunking images over several messages is out of scope. 180,000 leaves room for the envelope and a few thousand characters of prompt; the phone computes the sealed size of the real message before it sends and refuses with "Too large to send in one message" when a long prompt plus the photos would not fit.

**Claude runtime.** `Driver::user_prompt` writes the image blocks before the text; the user item and the prompt record keep only the count. `Driver::set_model`, `rename`, `context_usage` are control requests like `set_permission_mode`: `set_model` and `rename_session` complete or fail on the CLI's `control_response`; a successful `set_model` puts the resolved id on `state.model` (the CLI sends no status frame for it). `get_context_usage` is sent after `initialize` and after every turn end (no API call), and its reply becomes `state.context`. The model list comes from `initialize` (only values that pass the model rule); slash command names from `initialize`, `system/init` and `commands_changed`, minus `clear`/`reset`/`new` (new session id), `heapdump` (writes to the PC's desktop), `workflow-launch-exec` and names starting with `_` (internal). The hub caps the lists (32 models, 200 commands, 64 characters each). A respawn after the idle reap passes the current model with `--model`. History replay (`claude_history`) shows a prompt's images as a count too.

**Phone.** Session header: a model chip next to the permission-mode chip (sheet: the CLI's models, else `default`/`sonnet`/`opus`/`haiku`), "Context N%" (warning dot from 80%), and a pencil chip that renames (sheet with a text field, 80 characters). Composer: typing `/` at the start lists the matching slash commands (prefix matches first, at most 8); picking one inserts `/<name> `; commands are sent as ordinary prompt text. A camera button takes up to 4 photos with `expo-camera` (already in the app for the pairing QR): the picture size is the largest the camera offers with a long edge of at most 1280 px, and the JPEG is re-encoded at quality 0.7, 0.5, 0.35, 0.2, 0.1 until it fits what is left of the budget. Thumbnails stay in the composer until sent; the conversation shows "1 image" / "N images" on the user bubble. New task: a model row (aliases) for agents with `composer` `model`.

## 3. Security (threat model §3.12 bullet)

- *T1:* `--model` and `set_model` take only allow-listed names; no bypass mode or other flag becomes reachable. Rename and model changes are mutating commands with the full G1 lifecycle.
- *T2:* image bytes go from the phone through the sealed channel to `claude.exe`'s stdin and nowhere else: not into the transcript hub, not into the journal (the command record keeps `{mediaType, data: "<redacted>"}`), not into history replay. The companion does not store them.
- Image size bounds stop a phone from making the companion hold or forward more than one frame's worth per prompt.

## 4. Consequences and limits

- **No gallery.** Picking an existing screenshot needs `expo-image-picker`, which the app does not ship; adding it would need a native rebuild and a new dependency, so only the camera is offered. If the owner wants gallery attachments, add `expo-image-picker` (an official Expo module) and feed its base64 into the same composer list.
- **Small photos.** 180,000 base64 characters across all photos means about 1280 px at low JPEG quality for one photo, less for four; enough for text on a screen, not for fine detail. Chunking would lift it.
- Thumbnails are not kept: after sending, the bubble shows only the count, also on other phones and after a reconnect.
- The camera path is typechecked and the helpers are unit-tested, but the camera itself was not run on a device in this change.
- The slash-command menu offers what the CLI reports; it does not know which commands are useful from a phone, only which ones are harmful to the companion.

## 5. Verification

- Rust: `parses_composer_messages_and_rejects_bad_fields` (fixtures, size edges, model rule), `composer_ops_match_the_golden_fixture` (state and item serialization, hub caps), `composer_model_images_commands_context_and_rename` (fake CLI: `--model`, image blocks before the text, nothing of the image in the hub, hidden commands, models filtered, context, `set_model` resolved id and failure, rename via `session_title_changed`), `prompt_images_are_journaled_redacted`, `replay_counts_prompt_images_without_their_bytes`.
- TS: `packages/protocol/src/composer.test.ts` (same fixtures, size edges, reducer, mock refusals), `apps/mobile/src/utils/composer.test.ts` (slash filter, frame and image budgets, picture size, context, model labels).
- Live (`live_claude_haiku_composer`, Haiku, `ANTHROPIC_MODEL=haiku`): `set_model haiku`, a 2x2 red PNG answered "Red", rename, context 27,709 of 1,000,000 tokens, 194 slash commands and 14 models listed; **$0.0061**.
