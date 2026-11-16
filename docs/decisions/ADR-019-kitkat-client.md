# ADR-019: A separate native client for Android 4.4 (KitKat) tablets

Status: accepted, 2026-11-12; parity addendum the same day (the parity addendum). Code: `android-kitkat/`. Evidence: `docs/validation/evidence/kitkat/`.

## Context

The owner has an Android 4.4.2 tablet (API 19). The main app (`apps/mobile`, React Native 0.88 /
Expo 58) needs Android 7.0 (API 24), so it can never install there. Back-porting it is not possible
(Hermes, the new architecture, AndroidX and Expo modules all assume API 21+), and an old React Native
would fork the whole UI.

## Decision

A second, small client written against the plain Android framework in Java 8, in its own top-level
folder `android-kitkat/` (outside the npm workspaces). It speaks the same protocol v2 and the same
E2E layer as the main app, byte for byte, so the companion needs no change and treats it like any
other paired device.

- `minSdk 19`, `compileSdk`/`targetSdk 36`, AGP 9.2.1 + Gradle 9.4.1 (both already in the PC's
  Gradle cache; the wrapper is copied from `apps/mobile/android`). No AndroidX, no Kotlin, no OkHttp.
- One third-party library: ZXing core 3.3.3 (QR decoding, Java 7, fine on API 19). JUnit 4 and
  org.json are test-only.
- Release APK about 300 KB, signed with the owner's release key by `android-kitkat/build-release.ps1`
  (apksigner `--min-sdk-version 19`, v1 JAR + v2; KitKat verifies only v1).

### Files

| File | Role |
| --- | --- |
| `Crypto.java` | ChaCha20, Poly1305, RFC 8439 AEAD, HKDF, strict base64 and UTF-8, CSPRNG |
| `E2e.java` | pairing payload, handshake proofs, key derivation, sealed frames (port of `packages/e2e`) |
| `WebSocket.java` | RFC 6455 client over a plain or TLS socket (text, ping/pong, close codes) |
| `Transport.java` | connection lifecycle (port of `packages/e2e/src/transport.ts`) |
| `Model.java` | snapshot reducer, selectors, transcript views incl. MCP state (port of `reducer.ts` and `transcript.ts`) |
| `Composer.java` | pure helpers: slash filter, image / frame budget, camera sizes, model / mode / context labels, MCP summary, working verbs, git pull / push labels, terminal run assembly and history (ports of `apps/mobile/src/utils/{composer,agentIdentity,terminal,workspace}.ts`) |
| `Md.java` | markdown subset and diff line classes |
| `App.java`, `ConnService.java`, `SecureStore.java` | process state, TLS sockets, notifications, foreground service, key storage |
| `MainActivity.java`, `ConversationView.java`, `WorkspaceView.java`, `ScanActivity.java`, `Ui.java` | screens |

## Wire parity with the main app

- Pairing: the `payload: {...}` line from `chernobyl-companion --pair`, pasted or scanned, validated
  exactly like `pairing.ts` (v, host, ep, dev, psk 32 B, chal 16 B, exp).
- Handshake: `hs_hello` with a fresh 16-byte `cnonce`, `hs_challenge`, `hs_proof`, `hs_ok`; proofs
  `HMAC-SHA256(psk, role‖0‖deviceId‖0‖nonce‖0‖salt‖0‖cnonce)` compared in constant time; keys
  `HKDF-SHA256(psk, salt‖cnonce, "chernobyl-e2e-c2h" / "chernobyl-e2e-h2c")`.
- Sealed frames: `{"n":<counter>,"d":"<base64 ciphertext‖tag>"}`, nonce = 8-byte big-endian counter +
  4 zero bytes, strict per-direction counters; replay, desync, tamper and garbage are refused without
  advancing state and drop the connection (the next one re-handshakes).
- `hs_rekey`: the new device key is written to disk (synchronous commit) before `hs_rekey_ack` is sent.
- Token in `?token=` on every endpoint; `Origin: chernobyl://app` (the companion refuses any other).
- Ordered failover over the QR's `ep` list, equal-jitter exponential backoff (1 s to 15 s, reset only
  after 30 s up), sealed ping every 15 s with a 10 s pong deadline, pending commands replayed with
  their original `commandId` after a reconnect.
- Terminal ("Refused", no retry loop): `hs_err`, a server proof mismatch, close 4001 / 4002 / 4003.
  The Pair screen can change only the token and keep the pairing.
- Live transcripts: `session_subscribe` with the view's `epoch` + `afterSeq`, the ADR-015 §3.2
  reducer rules (snapshot replaces, other epoch or gap re-subscribes at most once a second,
  duplicates skipped, unknown kinds kept but not drawn), `session_unsubscribe` when the view closes
  and once for stray frames. Views are memory only (threat model T2).
- Workspace (ADR-016): reads wait for their `workspace_result` before the ack; changes resolve with
  their one `command_result` (git's own message is shown).
- A lost ack is shown as "Outcome unknown — the host may have run it", never as failed;
  "offline" means nothing was sent.

## Crypto on API 19

KitKat's JCE has no ChaCha20-Poly1305, so ChaCha20, Poly1305 (26-bit limbs, poly1305-donna style)
and the AEAD construction are plain Java. HMAC-SHA256 and SHA-256 come from `javax.crypto` /
`java.security`. The CSPRNG XORs `SecureRandom` with bytes read straight from `/dev/urandom`, so a
device still carrying the 2013 Android PRNG seeding bug stays safe.

Proof (`gradlew test`, 40 JVM tests, all pass):

- RFC 8439 2.3.2 (block), 2.4.2 (encryption), 2.5.2 (Poly1305), 2.8.2 (AEAD tag
  `1ae10b594f09e26a7e902ecbd0600691`), Appendix A.3 Poly1305 reduction edge cases.
- 19 AEAD vectors (0 to 4096 bytes, with and without AAD) generated by Node's OpenSSL, and 20
  Poly1305 vectors (random and all-0xFF keys/messages) from pyca/cryptography
  (`src/test/resources/`).
- FIPS 180-4 SHA-256, RFC 4231 HMAC, RFC 5869 HKDF case 1.
- The golden frames of `packages/e2e/src/session.test.ts` byte for byte, both directions, and every
  sealed-frame rule of that file (replay, desync, tamper, nine hostile envelopes, wrong keys).
- `kats.test.ts` proof binding and key derivation rules; base64 and UTF-8 strictness cases.
- The repo's protocol fixtures (`packages/protocol/fixtures/live/*.json`) through the reducers.
- `ComposerTest`: the composer, terminal, MCP and workspace fixtures through the parity helpers
  (slash filter, image budget and sealed-frame size, camera size and downscale, model / mode /
  context labels, MCP summary and callback check, working verbs equal to `agentIdentity.ts`'s,
  terminal output / exit / lost-exit assembly and history, pull / push labels, markdown tables).
- `TransportTest`: the real `Transport` + `WebSocket` against an in-test companion double that
  speaks the wire contract as the host: first endpoint refused then failover, the exact `Origin`
  header and `?token=`, handshake, `hs_rekey` stored then acked, a command acked through the sealed
  channel, reconnect proving the rotated key, close 4003 terminal.

## TLS (wss:// through ngrok)

KitKat supports TLS 1.2 but leaves it off on client sockets. `App.OPENER` enables TLS 1.2 (and 1.3
where present) before the handshake, adds the ECDHE AES-GCM suites the device has, drops RC4 / NULL
/ export / anon suites, sets SNI and checks the hostname with the platform verifier after the
handshake.

Measured on the API 19 emulator: `*.ngrok-free.app` negotiates **TLSv1.2
TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA** (ngrok's ECDSA certificate; KitKat has no ECDSA-GCM suite).
The first attempt failed with "Trust anchor for certification path not found": ngrok's chain is
`*.ngrok-free.app ← YE2 ← Root YE ← ISRG Root X2 ← ISRG Root X1`, and Android 4.4 ships neither ISRG
root. The app therefore trusts the system store **plus** ISRG Root X1 and X2 from `res/raw`
(SHA-256 `96:BC:EC:06:26:49:76:F3:74:60:77:9A:CF:28:C5:A7:CF:E8:A3:C0:AA:E1:1A:8F:FC:EE:05:C0:BD:DF:08:C6`
and `69:72:9B:8E:15:A8:6E:FC:17:7A:57:AF:B7:17:1D:FC:64:AD:D2:8C:2F:CA:8C:F1:50:7E:34:45:3C:CB:14:70`,
checked against letsencrypt.org). A chain passes if either store validates it; nothing is pinned or
skipped. The E2E layer stays the real protection either way (ADR-011): TLS only hides metadata.

## Keys at rest

`SecureStore` seals the pairing JSON, the rotated device key and the token with ChaCha20-Poly1305
under a random 32-byte data key. That key is wrapped with an RSA key pair in AndroidKeyStore (API 18+;
KitKat has no Keystore AES). If the Keystore refuses, the data key is kept in app-private
SharedPreferences instead, and the Pair screen says which ("Keys stored in: …"). On the emulator it
is the Keystore. Removing or changing the lock screen can wipe KitKat's Keystore: the stored values
then cannot be opened, are deleted, and the tablet must pair again. `allowBackup` is off.

## What differs from the main app

Built: pair (paste or camera QR), Home (connection, hosts online, agents, running sessions, pending
approvals, activity), Sessions (all sessions including Claude history ones; two panes at 720 dp and
wider, one pane with Back otherwise), New session (agent, workspace, model when the agent offers
`composer: model`, title, first prompt), Conversation (markdown, tool cards with input / output /
diffs, plan, thinking, notices, turn footers with time / tokens / cost, the working row with the
agent's animated 8×8 mark from `agentMark.tsx`, composer with Send / Queued / Joins the current turn,
Stop), inline and standalone approvals (the runtime's options, deny with a reason, question answers,
persistent grants confirmed), Approvals tab, approval notifications that open the conversation at the
card, a foreground service, Files and Changes (browse, read, git status, diff, stage, unstage, commit,
push). Legacy (non-transcript) sessions show their output lines and approvals. Demo data from the mock
adapter carries a "Simulated" tag and a Home banner.

Not built (yet): sign-in / biometrics, the agent detail screen, photos on the New session form
(they attach in the conversation), gallery photos (camera only, like the main app). The
"Parity addendum" below lists what the 2026-11-12 addendum added.

QR scanning uses `android.hardware.Camera` preview frames + ZXing; the emulator camera shows no QR,
so only the paste path is device-tested.

Debug builds accept an adb test hook (`--es pairPayload … --es token …`) that only pre-fills the
Pair form; release builds ignore it. `--es tlsprobe <host>` logs the negotiated TLS protocol and
suite (used for the ngrok measurement above).

## Emulator results (API 19, Nexus 7 2013, 2026-11-12)

Companion built from this branch, isolated `CHERNOBYL_HOME`, port 18797, token on, endpoint
`ws://10.0.2.2:18797`; mock adapter first, then a Claude adapter on a temp git workspace with Haiku
(three turns, $0.018 total).

| Check | Result |
| --- | --- |
| JVM unit tests (crypto vectors, golden frames, fixtures, markdown/diff, transport) | PASS 31/31 |
| Pair (paste) → handshake → `hs_rekey` stored → Home online | PASS |
| Release APK (v1 + v2 signed) installs and pairs on 4.4 | PASS |
| Missing token → "Refused: unauthorized", no retry loop; token-only fix | PASS |
| Mock sessions, approval from the conversation card, next request appears | PASS |
| Live Claude session: working row, inline approval with diff, Allow, file written, streamed reply, turn footer | PASS |
| Approval notification while backgrounded; tap opens the conversation scrolled to the card | PASS (scroll fixed during the run) |
| Approvals tab: runtime options list, deny with a reason the model receives | PASS |
| Companion restart → reconnect, re-sync, transcript re-subscribed (history session) | PASS |
| Files: list, read; Changes: status, stage, commit (commit landed in git), push error shown verbatim | PASS |
| TLS 1.2 to `*.ngrok-free.app` with the bundled ISRG roots | PASS (failed before the roots were added) |
| Portrait single pane after rotation | PASS (blank pane fixed during the run) |
| Camera QR scan | not run (emulator camera) |
| Physical 4.4 tablet | not run |

## Parity addendum (2026-11-12, branch `feat/kitkat-parity`)

The tablet client now has what the main app gained in ADR-016 (addendum), ADR-017, ADR-018 and
ADR-020, with the same messages and rules (`packages/protocol` is the source of truth):

- **Composer (ADR-017)**, only for what `Agent.live.composer` lists: model chip + sheet
  (`set_model`, models from `LiveState.models`, else the four aliases), slash menu on "/" (names
  from `LiveState.slashCommands`, starts-with first, at most 8), "Context N%" chip, rename sheet
  (`rename_session`, 1-80 characters, no control characters), permission-mode chip + sheet
  (`set_permission_mode`, only default / acceptEdits / plan), End session with a confirm
  (`session_end`), Files / Changes chips for the session's workspace.
- **Photos**: the camera screen (`ScanActivity` in photo mode, `android.hardware.Camera`, no
  file URI or FileProvider) shoots at most 1280 px on the long edge, decodes with a power-of-two
  sample size, scales to ≤ 1280 px, re-encodes as JPEG at 70 / 50 / 35 / 20 / 10 until the base64
  fits what is left of the 180,000-character budget; at most 4 per prompt, kept in memory only.
  Before sending, the real serialized message (Android's org.json escapes "/") is sized as a
  sealed frame and refused above 256,000 bytes. Not corrected for sensor rotation (the screen is
  landscape like a tablet's back camera).
- **PC sessions (ADR-018)**: "● Live on PC" replaces the status pill (list and header); the
  view mirrors the PC's turn; Stop, mode, model, rename and End are hidden; the composer says
  "Continue on phone (fork)…", sends `submit_prompt` with `fork: true` and opens the session the
  ack names; "Forked from “…”" under the header and in the list; approvals carry "Approval needed
  · from a PC session" and their notification "Approval needed (PC session)".
- **git pull (ADR-016 addendum)**: "Pull N commit(s)" with a confirm naming upstream → branch;
  Push now also confirms branch → upstream; both disabled without an upstream or on a detached HEAD.
- **Terminal (ADR-020)**: a third tab on Files, live only with `terminal.v1` (otherwise it says
  how to turn it on); the warning banner "Runs on your PC without approval prompts", Run / Stop
  (`terminal_run` / `terminal_stop`), streamed `terminal_output`, the exit line from
  `terminal_exit` (or the `command_result` after a reconnect), the last 20 command lines per
  workspace in app-private preferences; output in memory only, the last 60,000 characters drawn.
- **MCP sheet**: servers with status and error, Sign in on PC (`mcp_auth` desktop), finish on
  this tablet (`mcp_auth` phone-callback, then the URL, a confirm before opening it, and the pasted
  callback checked against `callbackPrefix` before `mcp_auth_callback`), Manage on claude.ai,
  Reconnect (`mcp_reconnect`). The sheet follows `transcript_snapshot.mcp` and `mcp` ops.
- **Settings tab**: approval alerts and session-finished alerts on/off (both on by default), the
  keep-awake switch (moved from Pair), and the audit history: every resolved request (agent, risk,
  decision, command, "from a PC session"), newest first, 200 kept in app-private preferences.
- **Identity**: the working row now follows `workingStatus` / `workingLabel` (rotating verbs every
  3 s seeded by the turn id, `Running <Tool>…`, "Waiting for you", elapsed, tokens, cost); marks
  and reply heads were already the `agentMark.tsx` sprites.
- **Old-tablet deviations (2026-11-16)**: a waiting approval shows whole minutes (`formatWaited`,
  blank under a minute) instead of ticking seconds, the working row rewrites its lines only when
  the text changes, and the mark stays still when the system animator scale is 0 ("Remove
  animations"). An unanswered approval no longer redraws the screen every frame for hours.
- **Markdown tables**: pipe tables become one aligned monospace block (header, rule, rows).

### Parity results (emulator API 19, Nexus 7 2013 profile, 2026-11-12)

Companion built from this branch, isolated `CHERNOBYL_HOME`, ports 18797 / 19497, token on,
`"terminal": {"enabled": true}`, one Claude adapter on a temp git workspace with a local bare
remote as upstream, `ANTHROPIC_MODEL=haiku`, no other `CLAUDE*` / `ANTHROPIC_*` variables. Live
spend: about $0.055 (three tablet turns $0.026, three PC `claude -p` runs $0.029).

| Feature | Result | Evidence |
| --- | --- | --- |
| JVM unit tests | PASS 40/40 | `gradlew testDebugUnitTest` |
| Terminal: run, streamed output, exit code, history | PASS | 30, 35 |
| Terminal: Stop kills a running command | PASS | 31, 32 |
| git pull with confirm (upstream → branch), fast-forward landed | PASS | 33, 34 |
| Stage, commit, push with confirm (branch → upstream), remote has it | PASS | 36, 37 |
| New Claude session, working row with rotating verb, reply head, table reply | PASS | 40, 41 |
| MCP sheet (32 servers from the PC's config), Reconnect reaches the companion | PASS | 42, 43 |
| MCP sign-in flows (desktop, phone-callback) | NOT RUN (would start OAuth against the owner's real accounts) | — |
| Permission mode chip + sheet, Plan applied, back to default | PASS | 44, 45 |
| Model chip + sheet from `LiveState.models`, Sonnet applied, back to Haiku | PASS | 46, 47 |
| Context chip, rename sheet, renamed in list and header | PASS | 48, 49, 50 |
| Slash menu filters and inserts | PASS | 51, 52 |
| Camera photo (640×480 emulator sensor, 26,540 base64 chars), sent, Claude described it | PASS | 53, 54, 55, 56 |
| PC session: Live on PC tag, mirrored tool + approval "from a PC session", Allow, file written, reply mirrored | PASS | 60, 61, 62, 63 |
| Fork from the tablet: new session opens, "Forked from", reply | PASS | 64, 65 |
| Approval alert on: "Approval needed (PC session)" notification opens the card; deny with a reason | PASS | 71, 72, 73 |
| Approval alert off: no notification while the request is pending; Approvals tab deny | PASS | 74, 75, 76 |
| Settings: alerts, keep-awake, audit history | PASS | 70, 82 |
| End session with confirm, composer closed | PASS | 77, 78 |
| Portrait single pane with chips | PASS | 79 |
| Session Changes chip opens that workspace | PASS | 80 |
| Release APK 0.2.0 (v1 + v2 + v3 signed, min-sdk 19) installs, pairs, shows the chips | PASS | 81, 82 |
| `Running <Tool>…` label on the device | not captured (unit-tested; tools ran too briefly) | — |

Found on the way: with the mock adapter enabled beside a live Claude adapter, the companion's
snapshot drops the mock agents and sessions but keeps the mock seed permissions and activity, so
demo approvals appear without their agent and therefore without a "Simulated" tag (main app too).
The run used the Claude adapter alone; the companion fix is open (`companion.rs` `snapshot`).
A KitKat platform bug: making a selectable `TextView` unselectable again overflows the stack under
accessibility (uiautomator); the slash rows therefore never use the selectable mono style.
