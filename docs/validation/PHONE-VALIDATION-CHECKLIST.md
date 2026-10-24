# Phone-to-companion manual validation checklist (Phase 5′ / Phase 6 gate S4)

**Status:** NOT RUN — no physical device has exercised any item on this list. Nothing here is
evidence until a row has a recorded observation.
**Purpose:** the manual steps, observable expected results, and evidence method for the first
real-phone validation of the companion. Gate **S4** closes only when the "Required for S4"
rows carry recorded evidence.
**Scope honesty:** server-side behavior behind these items is proven by the Rust suite
(`cargo test`, 160 passed / 0 failed / 3 ignored on the post-D1/D2/D6 tree, `d12bf9e`+) and
the soak harness — **that is not phone evidence**. This checklist is what converts server
proof into integration proof.

**Status legend:** `NOT RUN` = never attempted · `BLOCKED` = feature not implemented yet
(Phase 6) · `RUN` = attempted, evidence recorded (write date + observation in the row).

**Evidence convention:** for each RUN row attach (a) a screenshot/screen-recording, (b) the
companion stderr log excerpt (`server-*.log` or terminal), and (c) where relevant the soak or
test artifact path. Store under `docs/validation/evidence/<YYYY-MM-DD>-<item-id>/`.

## Prerequisites

- Companion built and running on the PC: `cargo run --manifest-path services/companion-rs -- --token <T>` (loopback only; **never** bind beyond loopback for this checklist).
- Android device + `adb reverse tcp:8787 tcp:8787` so the phone reaches the PC's loopback
  **over USB** — this keeps the listener loopback-bound (no LAN exposure, remote posture off).
- App pointed at the companion: `EXPO_PUBLIC_HOST_WS_URL=ws://127.0.0.1:8787` set when the
  bundle is built/started (`expo start`), token pasted once in Connect-host (goes to keystore).
- Note: `connect-host.tsx` is a prototype screen (it simulates its handshake); the REAL
  transport activates from `EXPO_PUBLIC_HOST_WS_URL` + keystore token (`TransportProvider`).

## 1. Pairing and authentication

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 1.1 | QR pairing scan (companion `--pair` offer → app scan → PSK stored) | Offer QR produced by `--pair`; app stores device identity in keystore; companion logs a paired device | QR + companion log + app screen | **BLOCKED** — no pairing UI or `hs_*` code exists in the app (Phase 6) |
| 1.2 | Connect with correct token (`?token=` in URL) | Status becomes `connected`; `hello` → `hello_ack` with snapshot; dashboard renders server state (not mock) | App screen + companion log | **NOT RUN** — note: at remote posture the pre-session `hello` snapshot exposure vs ADR-013 is tracked as decision **D9** (`PHASE5-THREAT-MODEL.md` §3.10/§5), awaiting owner ratification |
| 1.3 | Connect with empty/wrong token | Connection closes with 4001 (or HTTP 429 after 5 failures); app shows `Host unreachable`/reconnecting, never a working dashboard | App screen + companion log | **NOT RUN** |
| 1.4 | Keystore handling: force-stop and relaunch the app | Token reloads from keystore (no re-paste); no token in app logs or SQLite | Relaunch screen + log inspection | **NOT RUN** |

## 2. Invalid / expired / revoked / reused credentials

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 2.1 | Pair with an offer, wait past `expires_at`, then handshake | `hs_err` (expired); no session | companion log | **BLOCKED** (server-side proven: `expired_pairing_is_rejected`) — needs app pairing UI |
| 2.2 | Redeem one offer twice (second device/re-pair) | Second redemption refused (`spent`) | companion log | **BLOCKED** (`spent_pairing_cannot_be_reused`) |
| 2.3 | Revoke the device (`companion revoke <id>`), then reconnect | Handshake refused with revocation reason; app cannot reconnect as that device | companion log + app screen | **BLOCKED** (`revoked_device_is_rejected`) |
| 2.4 | Reuse a captured `hs_proof` / replay an old frame | Replay detected → connection torn down; no state change | companion log | **BLOCKED** (`replayed_frame_tears_down_connection`) + needs E2E in app |

## 3. Secure-session negotiation and invalid session messages

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 3.1 | Pair + handshake after connect (`hs_hello` → `hs_challenge` → `hs_proof` → `hs_ok`, mutual proof verified) | App shows session active; subsequent traffic sealed (companion log shows no plaintext commands) | app screen + companion log | **BLOCKED** — app has no E2E code yet (Phase 6) |
| 3.2 | Tamper a sealed frame / force a counter replay (debug build) | Server tears the connection down (`e2e: …`); app surfaces disconnect + reconnects | companion log | **BLOCKED** (`tampered_frame_tears_down_connection`, `replayed_frame…`) |
| 3.3 | Connect from a browser page to `ws://127.0.0.1:8787` (D1) | HTTP 403 refused before upgrade — CSWSH closed | browser console | **NOT RUN** (server-side proven: `ws_upgrade_with_origin_header_is_refused_403`) |

## 4. Command submission, ack correlation, authorization

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 4.1 | Submit a prompt from the app; watch the session screen | One `ack.ok=true` for the `commandId`; output events stream; success toast; nothing duplicated | screen recording + companion log | **NOT RUN** |
| 4.2 | Submit two identical commands rapidly (double-tap) | Client-side dedupe or server replay: exactly ONE execution in the companion's command store | companion `commands` counters | **NOT RUN** |
| 4.3 | Approve a permission with a stale revision (render, mutate elsewhere, then approve) | Refused fail-closed with a visible error toast; permission stays pending/expired server-side | screen + companion log | **NOT RUN** |
| 4.4 | Kill companion mid-command, restart, app reconnects | Unresolved command replayed with ORIGINAL commandId → duplicate ack, never re-executed | companion log + app screen | **NOT RUN** |
| 4.5 | Unsupported protocol version from a debug client | `ack_err` + close 4002 (D7) | raw client log | **NOT RUN** (server-side proven: `hello_gate_rejects…`) |

## 5. Disconnect / reconnect behavior

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 5.1 | Toggle phone Wi-Fi off ≥ 30 s, back on | Status `reconnecting` → `connected` (backoff ≤ 15 s); snapshot re-applied; no crash | screen recording | **NOT RUN** |
| 5.2 | Stop reading on the phone while the companion floods replies (D8) | Companion tears the stalled peer down (registry back to 0); phone later reconnects cleanly | companion `/health` + soak-equivalent observation | **NOT RUN** (server-side + harness proven: teardowns 533 ms / 542 ms) |
| 5.3 | Airplane mode while a command is in flight | `Host unreachable — nothing was sent` (or ack timeout ≤ 10 s toast); command never silently lost — visible as unresolved and replayed | screen recording | **NOT RUN** |

## 6. Server restart and app restart

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 6.1 | Restart the companion while the app stays open | App shows reconnecting → connected; snapshot reflects persisted state (sessions/commands survive) | screen + companion log | **NOT RUN** (harness proven: restart 2/2) |
| 6.2 | Force-stop and relaunch the app while the companion stays up | Reconnect ≤ backoff; keystore token reused; no duplicate command execution on relaunch | screen recording | **NOT RUN** |
| 6.3 | Full cold boot of the PC (autostart entry) then app connect | Companion serves normally after boot; connect works with the same token | companion log | **NOT RUN** |

## 7. Revocation and stale-session handling

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 7.1 | Revoke the paired device from the companion while the app holds a live session | Active session ends; further handshake refused; app cannot reconnect as that device | companion log + app screen | **BLOCKED** (`revoking_a_live_device_blocks_reconnect` server-side) |
| 7.2 | After revoke, relaunch app (stale keystore identity) | Keystore PSK/identity is useless — handshake refused with a clear app-side error, no crash | app screen | **BLOCKED** |
| 7.3 | Rotate host token (`HOST_TOKEN`) with app still holding the old one | 4001 loop with backoff, then a clear "token required" state after reconnect failure; no lockout spam against `/health` | app screen + companion log | **NOT RUN** |

## 8. Clear failure behavior and recovery

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 8.1 | Companion not running at app start | `offline`/reconnecting state with a visible toast; UI stays usable (mock-free, honest empty state); no spinner forever | screen recording | **NOT RUN** |
| 8.2 | Companion under the auth-failure cooldown (5 bad tokens) | Next correct attempt still connects (counter resets); wrong ones get fast refusal | companion log | **NOT RUN** (harness proven: 8×4001 + 1×429, recovery OK) |
| 8.3 | Malformed/garbage frame from a debug client | `malformed` counter increments; connection survives (per contract) | companion `/health` | **NOT RUN** (`malformed_message_is_acked_and_connection_survives`) |
| 8.4 | Leave the app backgrounded ≥ 10 min, return | Heartbeat keeps or re-establishes the connection; dead-peer cleanup on the companion side if truly dead | screen + companion log | **NOT RUN** (harness proven: silent peer released at 62 s) |

## Known risks to watch during the first device run

1. **Origin header (D1):** native RN WebSocket is expected to send **no** `Origin` header (ws
   and OkHttp/NSURLSession clients do not). If the phone ever gets **HTTP 403** on connect, the
   device stack IS sending `Origin` and the "no browsers" policy needs the allow-list branch
   documented in the threat model — record it as a compatibility finding, do not weaken the
   check silently.
2. **Ad-hoc pairing identity:** `device_id` provenance before QR pairing exists is manual —
   never treat a self-entered ID as authenticated identity (ADR-002).
3. **`adb reverse` keeps loopback posture** — with it, the D2 remote-posture and D6 health-gate
   paths are NOT exercised from the phone (they are covered by server tests + soak only).
   Validating them requires a deliberately non-loopback bind, which is a Phase 5′ decision for
   the product owner, not a test-harness action.

## To unblock the Phase 6 rows (1.1, 2.x, 3.1, 3.2, 7.1, 7.2)

These are `BLOCKED` only on the **mobile client**, not on the server. The server half is
done and tested; what is missing is a phone client that speaks the frozen wire contract in
`services/companion-rs/README.md` ("E2E wire format"), which is the exact handshake
(`hs_hello → hs_challenge → hs_proof → hs_ok`) plus the sealed-frame envelope
(`{"n":counter,"d":base64 AEAD}`). Building it needs three things that are a
**product/security decision, not a coding unknown** — the reason they were not made
unilaterally:

1. **Mobile crypto stack.** The handshake needs HMAC-SHA-256, HKDF-SHA-256 and
   ChaCha20-Poly1305 on the phone. Options: pure-JS audited libs (e.g. `@noble/*`, no
   build-model change), a native module (`react-native-quick-crypto`, faster but requires
   an Expo dev-client / prebuild, not Expo Go), or hand-rolled TS crypto (rejected — the
   mandate and ADR-011 favour an established implementation over custom crypto where one
   is available). This choice changes the mobile app's security posture and build model.
2. **QR scanning.** Reading the pairing QR needs `expo-camera` (native, camera permission,
   device-only). A manual paste/type fallback for the QR JSON is testable without it.
3. **Foreground service + persistent WS** for backgrounded notifications (Android native,
   device-only) — ADR-011 §4.

Once the stack is chosen, implement the client against the documented contract and validate
it **in a Node harness against the real companion first** (the server's own crypto
known-answer tests give the cross-check vectors), then run rows 1.1→8.4 above on the
physical device. No crypto or transport claim should be treated as proven until the device
rows carry recorded evidence.

## Required for gate S4

Rows **1.2, 1.3, 4.1, 5.1, 6.1, 6.2, 8.1** with recorded evidence from a physical device
(paired rows 1.1/2.x/3.1 additionally once Phase 6 pairing/E2E lands). Everything else is
supporting evidence. Until then S4 stays open — no server-side or harness result substitutes.
