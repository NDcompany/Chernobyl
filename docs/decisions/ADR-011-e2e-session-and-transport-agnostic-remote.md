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

App-open approvals stream live over the existing fan-out. App-closed: Android foreground
service keeps the WS alive and posts **local notifications** on approval events. Honest
limitation: no delivery while the socket is down. iOS background push (APNs) is out of
scope and would revisit this decision.

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
