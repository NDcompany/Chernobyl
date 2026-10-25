# ADR-011: E2E session encryption + transport-agnostic remote access

**Status:** Accepted
**Date:** 2026-10-21
**Decided by:** Product owner (connectivity + security requirements review, post-Phase 3)
**Supersedes:** ADR-001's fixed outbound-tunnel choice ("outbound tunnel URL (cloudflared,
Phase 5′)" and "push (FCM) is sent directly from the companion machine"), PRD §25 Phase 8
"Push registration / Notification worker"

## Context

Product owner requirements (verbatim, condensed): *hassle-free — the user scans a QR code
once with the phone app and stays connected for a long time; works "no matter if the user
is on the same internet or not" (only requirement: companion online); multiple mobile
clients per companion; the connection must be secure and end-to-end encrypted — no
man-in-the-middle, no cookie hijacking; the companion is a CLI with a terminal UI (like
`claude` / `opencode` CLIs), no web UI — the phone app is the only UI; a web app may come
later as a feature; background notifications without FCM (persistent WS chosen).*

Constraints that shape the design:

- The project is **local-only / no cloud**: no hosted relay, account server, or database
  (ADR-001). No transport may be *mandatory* third-party infrastructure.
- Reaching a NAT'd desktop from another network requires *some* path across the internet;
  every path has a flaw — direct needs a public IP (CGNAT), free tunnels change URLs
  (violates scan-once), named tunnels need accounts, VPN meshes need coordination.
- **std-only Rust has no TLS crate**; hand-rolling TLS is off the table. Transport-level
  encryption is therefore not ours to guarantee.

## Decision

### 1. Security lives in the pairing, not the transport (E2E session layer)

Every pairing generates a fresh random **256-bit per-device PSK** (OS CSPRNG,
`BCryptGenRandom` FFI). The single-use QR (ADR-002) carries it. From then on **every
WebSocket frame is encrypted app-layer with ChaCha20-Poly1305 AEAD**: per-connection
directional keys derived from the PSK via HKDF-SHA-256 (fresh random salt per
connection), monotonic per-direction counter as nonce (replay-proof), device credential
transmitted only *inside* AEAD frames — there is no bearer token or cookie on any wire.

- Consequence: **every transport is equally trustworthy** — LAN `ws://`, VPN, port-forward,
  or tunnel; a middleman (edge, relay, router) sees ciphertext only. MITM without having
  seen the QR (screen/physical access at pairing time) cannot derive keys; frame replay
  fails on counters; "cookie hijacking" has no cookie to steal.
- Consequence: all crypto is hand-rolled std-only (SHA-256, HMAC, HKDF-lite, ChaCha20,
  Poly1305, CSPRNG FFI) — precedent: hand-rolled WS framing/SHA-1/base64. **Every
  primitive must ship with official known-answer tests** (RFC 4231, RFC 8439, NIST CAVP);
  Poly1305 uses fixed-limb constant-time-style arithmetic. This is the single biggest
  security-sensitive work item in the remaining phases; it is bounded (~1k lines) and
  fully vector-testable.

### 2. Transport-agnostic remote access (replaces the fixed cloudflared phase)

No mandatory third-party transport. The companion advertises **all reachable endpoints**
(current LAN address always; plus whatever the user optionally enables: tailnet address,
DDNS + port-forward, cloudflared URL). The QR and the app's stored host record carry the
endpoint **list**; the app reconnects by trying them in order. Remote access becomes a
deployment detail with documented how-tos (incl. a CGNAT check), not an architecture
pillar. The companion's own server stays plain `ws://` — protection is layer 1, not the
pipe.

### 3. QR pairing is per-device, single-use, long-lived session

Each *new* device scans its own QR (printed by `companion pair` in the terminal, via a
hand-rolled terminal-QR encoder): hostId + endpoint list + companion key fingerprint +
that device's PSK + challenge. Short-lived, single-use (ADR-002). One scan per device;
the device stays paired until revoked — per-device credential + PSK means per-device
revocation. Existing devices are unaffected by new pairings.

### 4. Push: persistent WebSocket, no FCM (as chosen)

App-open approvals stream live over the existing fan-out. App in the background (not swiped
away from Recents): Android foreground service keeps the WS alive and posts **local
notifications** on approval events. Honest limitation: no delivery while the socket is down.
iOS background push (APNs) is out of scope and would revisit this decision.

**Implementation (2026-10-25).** The local Expo module `apps/mobile/modules/connection-service`
(Android only) runs `ConnectionForegroundService`, a `specialUse` foreground service with a
low-importance ongoing notification on channel `chernobyl-connection`. The service exists only
to deliver alerts, so `TransportProvider` runs it only for a stored pairing (the real E2E
transport, never the mock or the plaintext dev transport), while the user is signed in, push and
at least one alert type are on, and the notification permission is granted. It starts only while
the app is in the foreground, because Android 12+ refuses background starts; each return to the
foreground retries a start that was missed or refused. It updates the notification text from the
transport status. It stops on `denied` (posting a "Disconnected" alert when the app is in the
background), on sign-out or alerts off, on unpair or re-pair, and on unmount. A stop goes through
the service's own start queue (`ACTION_STOP`, then `stopSelf(startId)`), never `stopService()`:
a `stopService()` that lands before `startForeground()` crashes the app. The service starts the
headless JS task `ChernobylConnection`, a promise that never settles, because React Native pauses JS timers
when the activity pauses unless a headless task is active. It does not extend
`HeadlessJsTaskService`, which holds a partial wake lock for its whole lifetime. Approval
notifications are still posted from JS (`apps/mobile/src/platform/notify.ts`).

Limits:

- It keeps the process, the socket and JS timers alive while the app is in the background.
  It does nothing else.
- It holds no wake lock. The foreground service keeps network access in Doze, but JS runs only
  while the CPU is awake. The companion's 15 s WebSocket ping wakes the device and is answered
  natively (OkHttp), so the socket survives sleep, at the battery cost of a wake about every 15 s
  (significant on cellular).
- It is `START_NOT_STICKY`: after the process dies (force-stop, the OS reclaiming memory) nothing
  restarts it until the app is opened again. Swiping the app away from Recents destroys the
  activity; React Native's `ReactDelegate.onHostDestroy()` then stops the React surface, which
  unmounts the provider and stops the service. Delivery after a swipe-away would need the
  transport to live outside the React tree.
- While the task runs, React Native's timer loop ticks on display frames, as in the foreground.
- Notifications stay local: no FCM, and nothing is delivered while the socket is down.
- Google Play requires a declaration for the `specialUse` type at release; the manifest property
  `PROPERTY_SPECIAL_USE_FGS_SUBTYPE` states the reason.
- Its Kotlin compiles (checked 2026-10-25 with the Kotlin 2.2.10 compiler against android-36,
  outside Gradle), but it has not been built into an APK or run on a device yet.

### 5. Companion UI = CLI + TUI only

No web UI. A terminal UI (status, sessions, approvals with keybinds, pairing flow)
lands in Phase 8; `companion pair` prints the QR as plain console output from Phase 4′.
Diagnostics endpoints (`/health`, `/version`) remain — they are not a UI. A future web
app reuses this same E2E session layer.

## Consequences

- Phase map changes (TODO.md updated same day): Phase 4′ gains the E2E session layer +
  terminal QR encoder; Phase 5′ becomes endpoint advertisement + reconnect logic +
  optional-transport how-tos; Phase 6 gains persistent-WS local notifications; Phase 8
  gains TUI + windowless autostart host and loses FCM entirely.
- Security test matrix grows: MITM without PSK, frame replay, QR reuse/expiry, revoked
  device reuse, ticket replay, counter desync — plus known-answer tests for every crypto
  primitive.
- iOS: foreground works over the same WS; background push deferred (APNs is Apple
  infrastructure — a future decision if iOS ships).
- The web app (future feature) inherits E2E for free — same frames, same keys.

## Addendum 2026-10-26: device key rotation at first use

**Problem.** §3 promises a single-use QR, but redemption registered the device under the PSK
the QR carries, so the QR stayed a credential for as long as the device stayed paired. Found on
the emulator (checklist row 2.2): a wiped phone given the same payload again connected as the
device. A photo or screenshot of the QR taken during its 5-minute display, or the payload found
later, gave persistent access.

**Decision.** The QR's PSK only bootstraps the first session. At first redemption the companion
mints a fresh 256-bit device key (CSPRNG FFI) and hands it to the phone inside that sealed
session. Once the phone confirms it stored the key, the device key is the only key and the
QR's PSK fails the handshake.

Wire: two transport-level frames, sealed like everything after `hs_ok`. Like the `hs_*` frames
they are not protocol messages, so they are not in `ClientMessage`/`ServerMessage`.

- S→C `{"type":"hs_rekey","psk":"<b64 32B>"}`, right after `hs_ok` while the device's key
  rotation is pending. Every connection gets the same key until it is promoted; the companion
  never mints a second one.
- C→S `{"type":"hs_rekey_ack"}`, sent only after the phone persisted the key. It is accepted
  any time after `hs_ok`, including before `hello`.

State per device (journal `device` record, both keys DPAPI-protected): `psk` (current) and an
optional `pskNext` (pending).

| State | Handshake accepts | After `hs_ok` | Leaves by |
|---|---|---|---|
| Offer (unredeemed) | the QR's PSK | — | first handshake: device registered with `psk` = QR PSK, `pskNext` = new key |
| Pending | the QR's PSK, or the new key | `hs_rekey` (when proved with the QR's PSK) | `hs_rekey_ack`, or a proof under the new key (its ack was lost): promote |
| Rotated | the device key only | — | (the QR's PSK gets `hs_err`, the app's terminal deny) |

A revoked device is refused in every state, under either key. Promotion (`psk` = `pskNext`,
`pskNext` cleared, persisted) runs under the storage lock, and the handshake keeps verifying
unlocked and deciding again under the lock.

The commit is two-phase, so no single lost frame or failed write locks the phone out. A lost
`hs_rekey` means the phone keeps the QR's PSK and is offered the same key next time. When the
keystore write fails, the app sends no ack and keeps the old key. A lost ack is harmless: the
phone has switched to the new key, and its next handshake proves that key and promotes it.

**Compatibility.** An app without rotation support drops the unknown sealed frame
(`parseServerMessage` returns null) and keeps working on the QR's PSK; it never acks, so for it
the QR stays a credential until the phone is re-paired with a rotating app. Devices paired
before this change have no pending key and are not rotated. `pskNext` is optional in the
journal, so older journals load unchanged.

**Residuals** (threat model §3.11): until the paired phone's first ack, anyone holding the QR
connects as the device and receives the same key; an attacker who redeems and acks first leaves
the real phone with a visible terminal deny. With no forward secrecy (§1), a passive observer
who also holds the QR and records the first session can derive the device key.

**Evidence:** `server.rs` `replayed_qr_after_pairing_is_refused`,
`lost_rekey_ack_promotes_on_next_handshake_with_new_key`,
`client_ignoring_rekey_keeps_working_and_is_offered_the_same_key`,
`rekey_state_survives_restart`, `revoked_device_with_pending_rekey_is_refused`; the `hs_rekey`
transport tests in `packages/e2e`; interop scenario 10 in `tools/e2e-interop.mjs`.
