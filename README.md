# CHERNOBYL

One interface. Every agent. A mobile-first control app for AI agents: monitor hosts and
agents, watch live session output, and approve or reject permission requests from your
phone — with the host service enforcing every decision.

Monorepo layout:

```
packages/protocol   Shared TypeScript protocol: types, message envelopes,
                    runtime validators, seed data and the MockEngine.
services/host       Node.js WebSocket host service: snapshot-on-hello resume,
                    server-side pings, command dedupe, permission TTL,
                    optional token auth + protocol-version gate, /health.
apps/mobile         Expo (React Native) app — Expo Router, TanStack Query,
                    Zustand, React Hook Form + Zod, SQLite, SecureStore.
```

## Quick start

```powershell
npm install          # workspaces
npm run verify       # typecheck + lint + tests + builds + android export
```

Run the pieces:

```powershell
npm run dev:host     # host service on ws://localhost:8787 (+ /health)
npm run dev:mobile   # Expo dev server (press a / w for Android / web)
```

The app runs against the **offline mock by default** (no host needed). Set
`EXPO_PUBLIC_HOST_WS_URL` (e.g. `ws://10.0.4.21:8787`) in `apps/mobile/.env.local`
to connect the real host instead. If the host was started with `HOST_TOKEN`, save
the same token via the in-app "Connect host" modal — the app appends it to the
WebSocket URL from the device keystore.

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

## Host service

`services/host` is the enforcement point and is fully wired to the shared protocol:

- **Auth** — start with `HOST_TOKEN=<secret>`; WS clients must present it as a
  `?token=` query param or `Authorization: Bearer` header, else the socket is
  closed with `4001`.
- **Version gate** — a `hello` with a mismatched `PROTOCOL_VERSION` is rejected
  and closed with `4002`.
- **Resume** — every `hello` returns a full snapshot, so a reconnecting client
  never has to guess what it missed.
- **Liveness** — the server sends WS pings; idle-but-alive clients stay connected,
  dead peers are terminated after `HEARTBEAT_MISS_LIMIT` misses.
- **Dedupe** — a replayed `commandId` returns `duplicate: true` instead of running
  the command twice.

## Honest limitations (prototype)

- Mock by default: no shell is ever executed and sign-in checks no password. Point
  `EXPO_PUBLIC_HOST_WS_URL` at the host service to drive real state over the wire;
  the host still runs the `MockEngine` (no real agent integrations wired yet).
- Permission decisions are enforced by the host service — the app itself never grants or
  changes OS-level permissions, and the UI says so.
- Notifications are local-only while the app is open; background push needs FCM/APNs
  config (`googleServicesFile` / APNs key) which is deliberately not set up.
- Host token is stored in the device keystore (expo-secure-store), never in the DB;
  on web it stays in memory for the session.
- Audit trail is device-local SQLite.

## Verification status

- `npm run verify` passes: TypeScript (strict) across all three workspaces, ESLint
  (0 errors; 2 known warnings from React Compiler skipping `react-hook-form.watch`),
  **14/14 host tests** (7 engine + 7 real-socket integration covering auth, version
  gate, snapshot resume, broadcast, and duplicate suppression), and
  `expo export --platform android` → `dist-android` (Hermes bundle, 1443 modules).
- The WebSocket layer is exercised against a live host instance end-to-end
  (handshake, snapshot, `create_session` → `session_created` + ack, and token
  rejection with close `4001`).
- No Android SDK on this machine, so there is no `expo run:android` device build here;
  the export bundle is the build check. Web dev-server click-through is best-effort.

## Remaining steps for a real rollout

1. Wire real agent integrations behind the host (the MockEngine is the contract).
2. FCM/APNs push configuration for backgrounded notifications.
3. Account backend if cross-device sign-out/revocation is required.
4. EAS build + Play/App Store distribution (`eas build -p android`).
