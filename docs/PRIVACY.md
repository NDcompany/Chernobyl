# CHERNOBYL — privacy and data retention

**Applies to:** the desktop companion (`chernobyl-companion.exe`) and the CHERNOBYL phone app.
**Last reviewed:** 2026-10-25 against the code at the commit that adds this file.

## The short version

CHERNOBYL is local-only. There is no CHERNOBYL cloud, account, relay, hosted database,
analytics, crash reporting or telemetry. Data moves only between your desktop and your
paired phone, over a connection that is end-to-end encrypted after pairing (ADR-011). Nothing
is sent to us, because there is no "us" server to send it to.

The agent runtimes you connect (OpenCode, Ollama, Claude Code) keep their own data under
their own policies. CHERNOBYL reads from them on your machine. It does not copy their history
anywhere else.

## What the desktop companion stores

Everything lives in one folder: `%APPDATA%\CHERNOBYL` by default, or `CHERNOBYL_HOME` if set.

| Data | Where | Why | Retention |
|---|---|---|---|
| Config (`companion.json`): host name, adapter list and endpoints, ports, retention limits | config file | needed to run | until you edit or delete it |
| Commands received from the phone (the raw command message, status, outcome) | journal `companion.jsonl` | idempotency: a replayed command returns its original result instead of running twice | newest **5 000** (`retention.commands`); records whose outcome is still unknown are kept until boot reconciliation marks them `unknown` |
| Events sent to clients (session/permission/activity updates) | journal | reconnecting clients resume without gaps | newest **5 000** (`retention.events`) and nothing older than **7 days** (`retention.eventsMaxAgeMs`) |
| Permission approvals (ids, status, revision, timestamps) | journal | fail-closed approval checks | same as events |
| Pairing offers | journal | single-use QR redemption | 5-minute lifetime; the per-device key is stored **DPAPI-protected**, never in plaintext |
| Paired devices (device id, DPAPI-protected key, revoked flag) | journal | authenticate your phone | until revoked and removed |
| Audit log (start/stop, pairing, corruption notices) | journal | an honest operational trail | newest **1 000** entries |
| Backups you create (`--backup`) | the folder you choose | restore | until you delete them |

What it does **not** store: the transport token (environment variable only), your agents'
conversation history, file contents, or any credential in plaintext. Adapter endpoint
credentials (`user:pass@` in a URL) are stripped from logs and error text.

The journal is append-only and is compacted to the live set when it grows past 64 MB and at
shutdown. Appends go to the OS file cache. A crash of the companion loses nothing, but a power
loss can lose the last few records (CONFLICT_REGISTER C15).

**Diagnostics** (`/health`, `/metrics`) are counters and states only, never command payloads
or secrets. They are served on loopback; beyond loopback `/health` requires the token.

## What the phone app stores

| Data | Where | Retention |
|---|---|---|
| Pairing payload (host id, endpoints, device id, per-device key) and the optional token | OS keystore (`expo-secure-store`, unlocked-device only) | until you unpair the host |
| App settings, onboarding flag, display name | app SQLite database | until the app is uninstalled or data cleared |
| Your approval decisions (permission id, agent, command text, decision, risk, time) | app SQLite database | newest **1 000** |
| Live state (sessions, approvals) | memory, refreshed from the companion | not persisted |

The phone shows local notifications for approval requests and finished sessions. There is no
push service (no FCM/APNs), so notification content never leaves the device.

## Deleting your data

- **Revoke a phone:** `chernobyl-companion --revoke <device-id>` (list devices with `--devices`).
  The device can no longer handshake.
- **Unpair on the phone:** Settings, then Security, then *Unpair this host*. This deletes the
  pairing key and token from the keystore.
- **Erase everything on the desktop:** stop the companion, run `--uninstall-autostart`, and
  delete the CHERNOBYL folder.
- **Erase everything on the phone:** uninstall the app, or clear its storage.

## Changes

This file changes in the same commit as any change to what is stored or for how long.
