# CHERNOBYL — privacy and data retention

**Applies to:** the desktop companion (`chernobyl-companion.exe`) and the CHERNOBYL phone app.
**Last reviewed:** 2026-11-12 against ADR-017 (Claude composer: photos, model, rename on the phone).

## The short version

CHERNOBYL is local-only. There is no CHERNOBYL cloud, account, relay, hosted database,
analytics, crash reporting or telemetry. Data moves only between your desktop and your
paired phone, over a connection that is end-to-end encrypted after pairing (ADR-011). Nothing
is sent to us, because there is no "us" server to send it to.

The agent runtimes you connect (OpenCode, Ollama, Claude Code, Codex) keep their own data under
their own policies. CHERNOBYL reads from them on your machine. It does not copy their history
anywhere else.

## What the desktop companion stores

Everything lives in one folder: `%APPDATA%\CHERNOBYL` by default, or `CHERNOBYL_HOME` if set.

| Data | Where | Why | Retention |
|---|---|---|---|
| Config (`companion.json`): host name, adapter list and endpoints, ports, retention limits, and the OpenCode `basicAuth` (`user:pass`) if you set one, **in plaintext** | config file | needed to run | until you edit or delete it |
| Control key (`control.key`): 64 random hex characters, **in plaintext** | data folder | the CLI presents it to the running companion for `--pair` and `--revoke` | until you delete it; a missing key is recreated at the next start |
| Commands received from the phone (the raw command message, status, outcome) | journal `companion.journal` | idempotency: a replayed command returns its original result instead of running twice | newest **5 000** (`retention.commands`); records whose outcome is still unknown are kept until boot reconciliation marks them `unknown` |
| Events sent to clients (session/permission/activity updates) | journal | reconnecting clients resume without gaps | newest **5 000** (`retention.events`) and nothing older than **7 days** (`retention.eventsMaxAgeMs`) |
| Permission approvals (ids, status, revision, timestamps) | journal | fail-closed approval checks | same as events |
| Pairing offers | journal | single-use QR redemption | 5-minute lifetime; the per-device key is stored **DPAPI-protected**, never in plaintext |
| Paired devices (device id, DPAPI-protected key, revoked flag) | journal | authenticate your phone | until revoked and removed |
| Audit log (start/stop, pairing, corruption notices) | journal | an honest operational trail | newest **1 000** entries |
| Backups you create (`--backup`) | the folder you choose: keep it one only you can write, because the `.sha256` sidecar catches corruption, not deliberate edits | restore | until you delete them |

What it does **not** store: the transport token (environment variable only), your agents'
conversation history, or file contents. Live transcripts (prompts, replies, reasoning, tool
input and output, diffs) that a subscribed phone watches are held **in memory only**: redacted
for known secret shapes, size-capped, and dropped an hour after a session ends. They are never
journaled. With Claude Code workspaces configured, the phone can also read past Claude Code
conversations: every session whose folder is inside an allow-listed workspace, including ones
started in a terminal or VS Code. The companion reads them from Claude Code's own files
(`%USERPROFILE%\.claude\projects`) on your PC, only the last 4 MiB of one session at a time and
only when your phone opens it, and redacts and caps them before they leave the PC, the same as
live transcripts. Each listed session's title (at most 80 characters, redacted) is part of the
session events the journal keeps. OpenCode conversation text used to be republished as journaled activity events;
since ADR-015 step 5 it is not. What you type on the phone is different: a prompt
(`create_session`, `submit_prompt`) and a deny reason (`permission_decision.message`) are part
of the command record, journaled as sent and kept with the newest 5 000 commands. Permission
requests are journaled as before, with their command or patterns and, for an edit, the diff
the runtime shows, redacted and capped. An `mcp_auth_callback` command is journaled with its
callback URL replaced by `<redacted>`, because it carries a one-time authorization code. The
answers to an agent's question (`permission_decision.answers`) are journaled with each value
replaced by `<redacted>`. Photos you attach to a Claude Code prompt (ADR-017; at most 4, about
130 KiB together) travel inside that prompt through the encrypted channel to Claude Code on your
PC, which sends them to Anthropic like any image you paste into Claude Code. The companion does
not store them: the journal keeps only each photo's type with the data replaced by `<redacted>`,
and the live transcript and history show only how many images a prompt had. The phone keeps the
preview only until the prompt is sent. Claude Code itself saves the prompt, images included, in
its own session file under `%USERPROFILE%\.claude\projects`, as it does for terminal sessions.
A model choice and a session name you set on the phone are journaled as sent.
Workspace files and git (ADR-016): folder listings, file content,
git status and diffs the phone opens are read on demand, sent only to the asking phone and
never journaled; file content and diffs are redacted for known secret shapes, and `.env*`
files reach the phone with every value masked. The git commands themselves (`git_stage`,
`git_unstage`, `git_commit` with its message, `git_push`) are journaled like any command, and
a commit or push adds an audit record with the workspace id, the message's first line and the
outcome. Two credentials are stored in plaintext: the OpenCode
`basicAuth` in `companion.json` and the control key. Only the folder's permissions protect
them. The default `%APPDATA%\CHERNOBYL` is readable by your Windows account and administrators
only, but the files go wherever a copy of the folder goes: a roaming profile, a support bundle,
or a `CHERNOBYL_HOME` on a drive without permissions. Adapter endpoint credentials
(`user:pass@` in a URL) are stripped from logs and error text.

The journal is append-only and is compacted to the live set when it grows past 64 MB and at
shutdown. Appends go to the OS file cache. A crash of the companion loses nothing, but a power
loss can lose the last few records (CONFLICT_REGISTER C15). While it runs, the companion holds
`companion.lock` (an empty file) open so CLI commands never write the journal under it.

**Diagnostics** (`/health`, `/metrics`) are counters and states only, never command payloads
or secrets. They are served on loopback; beyond loopback `/health` requires the token.

## What the phone app stores

| Data | Where | Retention |
|---|---|---|
| Pairing payload (host id, endpoints, device id, per-device key) and the optional token | OS keystore (`expo-secure-store`, unlocked-device only) | until you unpair the host |
| App settings, onboarding flag, display name | app SQLite database | until the app is uninstalled or data cleared |
| Your approval decisions (permission id, agent, command text, decision, risk, time) | app SQLite database | newest **1 000** |
| Live state (sessions, approvals, live transcripts, opened files, git status and diffs) | memory, refreshed from the companion | not persisted |

The phone shows local notifications for approval requests and finished sessions. There is no
push service (no FCM/APNs), so notification content never leaves the device.

## Deleting your data

- **Revoke a phone:** `chernobyl-companion --revoke <device-id>` (list devices with `--devices`).
  The device can no longer handshake, and a later `--restore` of an older backup keeps the
  revocation.
- **Unpair on the phone:** Settings, then Security, then *Unpair this host*. This deletes the
  pairing key and token from the keystore.
- **Erase everything on the desktop:** stop the companion, run `--uninstall-autostart`, and
  delete the CHERNOBYL folder.
- **Erase everything on the phone:** uninstall the app, or clear its storage.

## Changes

This file changes in the same commit as any change to what is stored or for how long.
