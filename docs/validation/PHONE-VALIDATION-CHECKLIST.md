# Phone-to-companion manual validation checklist (Phase 5′ / Phase 6 gate S4)

**Status:** emulator run 2026-10-25 (Android 15 x86_64, release APK, real companion): 30 of the 31 rows in sections 1-8 RUN, all PASS (row 2.2 after the device-key rotation fix; 6.3 needs a PC reboot), see each row and `evidence/2026-10-25-emulator/results.json`. Section 9 (ADR-015 live sessions, 13 rows, added 2026-11-04): all NOT RUN. **No physical device yet** — the emulator does not exercise a real camera, radio or OEM battery policy.
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

**Current procedure (2026-10-25):** build the APK (`node scripts/build-android.mjs`, see
`docs/INSTALL.md`), install it, and run `node scripts/device/validate.mjs` with a device or
emulator attached. It drives the real APK against a real companion: pairing by payload,
restarts, network loss, background service, revoke, token refusal. It saves evidence under
`evidence/<run>/`. The notes below describe the original manual setup and predate the
pairing UI.

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
| 1.1 | QR pairing scan (companion `--pair` offer → app scan → PSK stored) | Offer QR produced by `--pair`; app stores device identity in keystore; companion logs a paired device | QR + companion log + app screen | **RUN 2026-10-25 (emulator, release APK)** — PASS: pasted payload → `hs_*` handshake → "2 hosts online"; device registered (`evidence/2026-10-25-emulator/1.1-*`). Camera path: permission + live preview verified, decode NOT verified on the emulator (`evidence/2026-10-25-emulator/1.1cam-scanned.png`) |
| 1.2 | Connect with correct token (`?token=` in URL) | Status becomes `connected`; after the `hs_*` handshake the sealed `hello` → `hello_ack` with snapshot; dashboard renders server state (not mock). At remote posture a pre-session plaintext `hello` is refused (`e2e session required`, D9 option B) | App screen + companion log | **RUN 2026-10-25 (emulator)** — PASS on loopback posture (no token): sealed `hello` → snapshot rendered as live; demo-adapter data correctly badged Simulation (`evidence/2026-10-25-emulator/1.1-home.png`) |
| 1.3 | Connect with empty/wrong token | Connection closes with 4001 (or HTTP 429 after 5 failures); app shows `Host unreachable`/reconnecting, never a working dashboard | App screen + companion log | **RUN 2026-10-25 (emulator)** — PASS: companion restarted with a token the phone lacks → app shows "Access denied — re-pair this phone" and stops retrying (`evidence/2026-10-25-emulator/1.3-*`) |
| 1.4 | Keystore handling: force-stop and relaunch the app | Token reloads from keystore (no re-paste); no token in app logs or SQLite | Relaunch screen + log inspection | **RUN 2026-10-25 (emulator)** — PASS: force-stop + relaunch reconnects from the keystore without re-pairing (row 6.2) |

## 2. Invalid / expired / revoked / reused credentials

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 2.1 | Pair with an offer, wait past `expires_at`, then handshake | `hs_err` (expired); no session | companion log | **RUN 2026-10-26 (emulator)** — PASS: an offer used 5 min 5 s after creation → app shows "Access denied" (`evidence/2026-10-25-emulator/2.1-expired-offer.png`) |
| 2.2 | Pair with an offer and let the phone connect (it stores the rotated device key and acks), then wipe the app (`pm clear`) or take a second phone and pair again with the same QR payload | The re-used QR is refused once the first phone has paired and acked: `hs_err` (proof verification failed), app shows "Access denied". A first phone that was not wiped stays connected | companion log + app screen | **RUN 2026-10-26 (emulator)** — PASS: the phone paired, stored the rotated device key and acked it (journal: `pskNext` pending, then promoted); the same QR re-used after `pm clear` → `hs_err` proof verification failed, app shows "Access denied — re-pair this phone" (`evidence/2026-10-25-emulator/2.2-reused-offer.png`). Rows 1.1, 6.2 and 6.1 re-passed on the same build (relaunch and companion restart reconnect with the rotated key). The 2026-10-25 run had FAILED (the QR PSK stayed the device key); fixed by device key rotation at first use (ADR-011 addendum, threat model §3.11) |
| 2.3 | Revoke the device (`companion revoke <id>`), then reconnect | Handshake refused with revocation reason; app cannot reconnect as that device | companion log + app screen | **RUN 2026-10-25 (emulator)** — PASS via row 7.1 (live revoke → 4003 → denied; relaunch refused) |
| 2.4 | Reuse a captured `hs_proof` / replay an old frame | Replay detected → connection torn down; no state change | companion log | **RUN 2026-10-26 (interop: real binary + the app’s TS E2E client)** — PASS: an exact replay of a sealed frame tears the connection down; a recorded server handshake replayed to a new attempt fails `server-proof-mismatch` (`npm run verify:e2e`, all pass) |

## 3. Secure-session negotiation and invalid session messages

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 3.1 | Pair + handshake after connect (`hs_hello` → `hs_challenge` → `hs_proof` → `hs_ok`, mutual proof verified) | App shows session active; subsequent traffic sealed (companion log shows no plaintext commands) | app screen + companion log | **RUN 2026-10-25 (emulator)** — PASS via row 1.1 (mutual handshake with the client nonce; every frame sealed) |
| 3.2 | Tamper a sealed frame / force a counter replay (debug build) | Server tears the connection down (`e2e: …`); app surfaces disconnect + reconnects | companion log | **RUN 2026-10-26 (interop: real binary + the app’s TS E2E client)** — PASS: a tampered sealed frame tears the connection down (`npm run verify:e2e`) |
| 3.3 | Connect from a browser page to `ws://127.0.0.1:8787` (D1) | HTTP 403 refused before upgrade — CSWSH closed | browser console | **RUN 2026-10-26 (emulator)** — PASS: a Chrome page on the emulator opening `ws://10.0.2.2:<port>` was refused (`WS-REFUSED 1006`, `evidence/2026-10-25-emulator/3.3-browser.png`) |

## 4. Command submission, ack correlation, authorization

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 4.1 | Submit a prompt from the app; watch the session screen | One `ack.ok=true` for the `commandId`; output events stream; success toast; nothing duplicated | screen recording + companion log | **RUN 2026-10-26 (emulator)** — PASS: a task submitted from the phone created exactly one session (5 → 6) and live output streamed on the session screen (`evidence/2026-10-25-emulator/4.1-session-output.png`) |
| 4.2 | Submit two identical commands rapidly (double-tap) | Client-side dedupe or server replay: exactly ONE execution in the companion's command store | companion `commands` counters | **RUN 2026-10-26 (emulator)** — PASS: a double tap on "Start session" executed once (sessions 6 → 7, `evidence/2026-10-25-emulator/4.2-after-double-tap.png`) |
| 4.3 | Approve a permission with a stale revision (render, mutate elsewhere, then approve) | Refused fail-closed with a visible error toast; permission stays pending/expired server-side | screen + companion log | **RUN 2026-10-26 (emulator)** — PASS: a second client rejected the pending request first (ack ok); a late approve with the same revision was refused fail-closed (`request is no longer pending`); the request left the phone’s pending list (`evidence/2026-10-25-emulator/4.3-pending-on-phone.png`, `4.3-after-other-client.png`) |
| 4.4 | Kill companion mid-command, restart, app reconnects | Unresolved command replayed with ORIGINAL commandId → duplicate ack, never re-executed | companion log + app screen | **RUN 2026-10-26 (emulator)** — PASS: companion killed right after submit, then restarted: never executed twice (demo sessions are in memory, so the restart returns to the seed) (`evidence/2026-10-25-emulator/4.4-companion-down.png`, `4.4-after-restart.png`) |
| 4.5 | Unsupported protocol version from a debug client | `ack_err` + close 4002 (D7) | raw client log | **RUN 2026-10-26 (host side, real binary)** — PASS: `hello` with protocol 1 → "protocol mismatch" refusal, then close 4002 |

## 5. Disconnect / reconnect behavior

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 5.1 | Toggle phone Wi-Fi off ≥ 30 s, back on | Status `reconnecting` → `connected` (backoff ≤ 15 s); snapshot re-applied; no crash | screen recording | **RUN 2026-10-25 (emulator)** — PASS: Wi-Fi + data off 30 s → "Reconnecting…"; reconnected 14 s after the network returned (`evidence/2026-10-25-emulator/5.1-*`) |
| 5.2 | Stop reading on the phone while the companion floods replies (D8) | Companion tears the stalled peer down (registry back to 0); phone later reconnects cleanly | companion `/health` + soak-equivalent observation | **RUN 2026-10-26 (`chernobyl-companion --soak smoke`, real binary)** — PASS: `d8_saturation_teardown`: a client that stops reading is torn down by message count and by bytes, and the registry returns to 0; the phone-side reconnect is covered by rows 5.1 and 6.1 |
| 5.3 | Airplane mode while a command is in flight | `Host unreachable — nothing was sent` (or ack timeout ≤ 10 s toast); command never silently lost — visible as unresolved and replayed | screen recording | **RUN 2026-10-26 (emulator)** — PASS: submit while offline → "Host unreachable — nothing was sent"; nothing executed after the network returned (sessions 6 → 6, `evidence/2026-10-25-emulator/5.3-offline-submit.png`) |

## 6. Server restart and app restart

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 6.1 | Restart the companion while the app stays open | App shows reconnecting → connected; snapshot reflects persisted state (sessions/commands survive) | screen + companion log | **RUN 2026-10-25 (emulator)** — PASS: companion stopped → "Reconnecting…"; restarted → back online in 3 s (`evidence/2026-10-25-emulator/6.1-*`) |
| 6.2 | Force-stop and relaunch the app while the companion stays up | Reconnect ≤ backoff; keystore token reused; no duplicate command execution on relaunch | screen recording | **RUN 2026-10-25 (emulator)** — PASS: force-stop + relaunch → online again in 5 s, no re-pair (`evidence/2026-10-25-emulator/6.2-relaunched.png`) |
| 6.3 | Full cold boot of the PC (autostart entry) then app connect | Companion serves normally after boot; connect works with the same token | companion log | **NOT RUN** |

## 7. Revocation and stale-session handling

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 7.1 | Revoke the paired device from the companion while the app holds a live session | Active session ends; further handshake refused; app cannot reconnect as that device | companion log + app screen | **RUN 2026-10-25 (emulator)** — PASS: `--revoke` against the running companion terminated 1 live session; app → "Access denied"; foreground service stopped (`evidence/2026-10-25-emulator/7.1-*`) |
| 7.2 | After revoke, relaunch app (stale keystore identity) | Keystore PSK/identity is useless — handshake refused with a clear app-side error, no crash | app screen | **RUN 2026-10-25 (emulator)** — PASS: 5 cold launches with the revoked pairing stored all show Access denied; no crash in the crash buffer (`evidence/2026-10-25-emulator/7.2-*`) |
| 7.3 | Rotate host token (`HOST_TOKEN`) with app still holding the old one | 4001 loop with backoff, then a clear "token required" state after reconnect failure; no lockout spam against `/health` | app screen + companion log | **RUN 2026-10-26 (emulator)** — PASS: connected with the token; after restarting with a rotated `--token` the app shows "Access denied" and the companion logged 0 refusals in the next 15 s (no retry storm) (`evidence/2026-10-25-emulator/7.3-connected-with-token.png`, `7.3-after-rotation.png`) |

## 8. Clear failure behavior and recovery

| # | Steps | Expected (observable) | Evidence | Status |
|---|-------|----------------------|----------|--------|
| 8.1 | Companion not running at app start | `offline`/reconnecting state with a visible toast; UI stays usable (mock-free, honest empty state); no spinner forever | screen recording | **RUN 2026-10-25 (emulator)** — PASS: companion down at app start → "Connecting…", UI navigable (`evidence/2026-10-25-emulator/8.1-*`) |
| 8.2 | Companion under the auth-failure cooldown (5 bad tokens) | Next correct attempt still connects (counter resets); wrong ones get fast refusal | companion log | **RUN 2026-10-25 (host side, real binary)** — PASS: wrong token ×5 → 4001 each, then HTTP 429; the correct token afterwards answered |
| 8.3 | Malformed/garbage frame from a debug client | `malformed` counter increments; connection survives (per contract) | companion `/health` | **RUN 2026-10-25 (host side, real binary)** — PASS: a malformed frame moved `malformed` 0 → 1 and the connection survived |
| 8.4 | Leave the app backgrounded ≥ 10 min, return | Heartbeat keeps or re-establishes the connection; dead-peer cleanup on the companion side if truly dead | screen + companion log | **RUN 2026-10-26 (emulator)** — PASS: app backgrounded 10 min with the foreground service; the companion saw 1 client at every minute; foreground was live at once (`evidence/2026-10-25-emulator/8.4-after-10-min.png`) |
| 8.5 | Android: pair, press Home, trigger an approval on the desktop, stop the companion, then revoke the device; repeat and end with sign-out, push off, unpair and swipe-away | Ongoing "Background connection" notification with the `[ · ]` status-bar glyph; the approval arrives as a notification; text turns "Reconnecting…"; on revoke a "Disconnected from your desktop companion" alert; after each ending, `adb shell dumpsys activity services com.chernobyl.app` lists no `ConnectionForegroundService` | screen recording + dumpsys | **RUN 2026-10-25 (emulator)** — PASS (core): Home pressed → `ConnectionForegroundService` running, "Background connection — Connected to your desktop companion" notification, companion kept the client for 2 min in the background; revoke stops it (row 7.1). Approval-arrives-as-notification not yet exercised (`evidence/2026-10-25-emulator/8.5-*`) |
| 8.6 | Android: revoke the device, keep its pairing stored, cold-launch the app 10 times | No crash ("did not then call Service.startForeground()"); status Denied; no service left running | screen + `adb logcat` | **RUN 2026-10-25 (emulator)** — PASS via row 7.2 (5 cold launches, no startForeground crash) |

## 9. Live agent sessions (ADR-015)

Run with `scripts/device/validate.mjs` against the real companion with Claude Code and
OpenCode configured (ADR-015 §7 step 11).

| # | Check | Status |
|---|-------|--------|
| 9.1 | Claude session from the phone: "Starting Claude Code…", then streaming text grows in place | **RUN 2026-11-04 (emulator, Haiku)** — PASS: "Starting Claude Code…", phone text grew in place in step with the companion (375 → 5867 chars), footer "Done · 12.3 s" (`evidence/2026-10-25-emulator/9.1-*`) |
| 9.2 | Tool card shows streaming input, then output; the Edit card shows the diff | **RUN 2026-11-04 (emulator, Haiku)** — PASS: Write card shows input and output; Edit card shows "1 added, 1 removed". The one-line Write diff dropped its `+` line (app bug, fixed after the run) (`evidence/2026-10-25-emulator/9.2-*`) |
| 9.3 | Inline approval inside the card → Allow → the tool completes; the Approvals tab entry clears | **RUN 2026-11-04 (emulator, Haiku)** — PASS: "awaiting approval" inline and on the Approvals tab; Allow completed the tool and cleared the entry (`evidence/2026-10-25-emulator/9.3-*`) |
| 9.4 | "Deny and tell Claude why" → Claude continues and mentions the reason | **RUN 2026-11-04 (emulator, Haiku)** — PASS: card "denied", Claude quoted the reason and finished the turn (`evidence/2026-10-25-emulator/9.4-*`) |
| 9.5 | Follow-up sent mid-turn shows "Queued" and is answered (in that turn or the next) | **RUN 2026-11-04 (emulator, Haiku)** — **FAIL**: the CLI usually answered the follow-up as its own turn, not merged. Fixed by advertising `queue` for Claude. **Re-run 2026-11-12 — PASS**: both follow-ups show "Queued" / "Runs after this turn" and are answered in their own turn; 9.1 re-passed with no "Jump to latest" during streaming, and the composer stays above the keyboard (`evidence/2026-10-25-emulator/9.5*`) |
| 9.6 | Stop mid-stream → "Stopped"; the next prompt works in the same session | **RUN 2026-11-04 (emulator, Haiku)** — PASS: Stop at 171 of 3000 numbers → "Stopped"; the next prompt completed in the same session. "Stopped" showed twice (app bug, fixed after the run) (`evidence/2026-10-25-emulator/9.6-*`) |
| 9.7 | App in the background, an approval arrives as a notification; tapping it opens the conversation at the card | **RUN 2026-11-04 (emulator, Haiku)** — PASS: "Approval needed" notification in the background; tapping it opened the conversation at the card (`evidence/2026-10-25-emulator/9.7-*`) |
| 9.8 | Airplane mode on and off mid-stream → the transcript resumes with no gap or duplicate | **RUN 2026-11-04 (emulator, Haiku)** — PASS: network cut at 456 of 3999 chars; after reconnect the phone text equals the companion item, no gap, one copy (`evidence/2026-10-25-emulator/9.8-*`) |
| 9.9 | MCP chip shows needs-auth → "Sign in on PC" opens the desktop browser → the chip turns connected | **RUN 2026-11-04 (emulator, Haiku)** — PARTIAL: the MCP chip ("15 of 32 connected, needs attention") and mode chip render and the sheet opens; sign-in NOT RUN (it would start OAuth on the owner's real MCP servers) (`evidence/2026-10-25-emulator/9.9-*`) |
| 9.10 | A persistent "always allow" option shows the confirm with the rule and `.claude/settings.local.json` | **RUN 2026-11-04 (emulator, Haiku)** — PASS: confirm shows "Rule: Bash(cp fake.txt k.txt; cat k.txt)" and ".claude/settings.local.json"; Cancel wrote nothing (`evidence/2026-10-25-emulator/9.10-*`) |
| 9.11 | `echo` of a fake `sk-ant-` key appears as `«redacted:anthropic-key»` | **RUN 2026-11-04 (emulator, Haiku)** — PASS: the phone and the companion transcript show `«redacted:anthropic-key»`, never the key (`evidence/2026-10-25-emulator/9.11-*`) |
| 9.12 | OpenCode session (Ollama): streaming, inline bash approval, abort shown as Stopped | **RUN 2026-11-04 (emulator, OpenCode 1.19.4 + Ollama qwen3.5:4b)** — PASS: streaming, inline bash approval → Allow once, Stop → "Stopped". Home showed "0 hosts online" (companion bug, fixed after the run) (`evidence/2026-10-25-emulator/9.12-*`) |
| 9.13 | An old (pre-ADR-015) APK against the new companion: dashboard, approvals and sessions behave as before | **NOT RUN** — no pre-ADR-015 APK was kept |

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
   device-only) — ADR-011 §4. Written 2026-10-25 (`apps/mobile/modules/connection-service`);
   its Kotlin compiles but no APK with it has been built yet. Rows 8.5 and 8.6 check it.

Once the stack is chosen, implement the client against the documented contract and validate
it **in a Node harness against the real companion first** (the server's own crypto
known-answer tests give the cross-check vectors), then run rows 1.1→8.4 above on the
physical device. No crypto or transport claim should be treated as proven until the device
rows carry recorded evidence.

## Required for gate S4

Rows **1.2, 1.3, 4.1, 5.1, 6.1, 6.2, 8.1** with recorded evidence from a physical device
(paired rows 1.1/2.x/3.1 additionally once Phase 6 pairing/E2E lands). Everything else is
supporting evidence. Until then S4 stays open — no server-side or harness result substitutes.
