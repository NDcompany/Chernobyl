# Phase 5′ threat model & security release gates (documentation only)

**Status:** Draft for review — every "verified" claim below was re-traced to source on 2026-10-23 (HEAD `e190736` + uncommitted P1 tests); no gate is declared passed without cited evidence.
**Scope:** Analysis and gate definitions for Phase 5′ (transport-agnostic remote access, ADR-011). This document **implements nothing**: no TLS, no handshake change, no auth change, no endpoint advertisement, no remote access, no non-loopback listener.
**Relation:** Links — does not restate — ADR-001 (local-first), ADR-002 (pairing-only identity), ADR-006 (tickets, accepted/unimplemented), ADR-007 (unsigned installers), ADR-010 (Rust companion), ADR-011 (E2E + transport-agnostic), conflict C1, PRD §11.4/§15.2/§25 Phase 5. Duplicates neither the ADRs nor `services/companion-rs/README.md` (wire contract).

---

## 1. Assets and adversaries

**Assets.** Paired-device PSKs (highest value — they authorize command execution); command/event traffic (can contain secrets the user pastes into prompts); permission-approval flow (an attacker who can approve wins tool execution); the on-disk registry/journal; availability of the companion.

**Adversaries considered.** (a) Network attacker on the same LAN / inside a tunnel path; (b) malicious webpage in the user's browser (CSWSH); (c) malicious local process; (d) stolen/lost paired phone; (e) the tunnel/relay operator as passive observer; (f) brute-force/DoS noise. Not modeled: OS-level compromise of the companion's own user account (defeats DPAPI by construction), physical DMA attacks.

## 2. Trust boundaries (verified binding scope)

| Boundary | Current state (verified) | Source |
|---|---|---|
| Main WS listener | Default `127.0.0.1:8787`; `--bind` configurable; **non-loopback bind without token refused at startup** | `main.rs:153-158`, README:11/25 |
| Diagnostics listener | Separate port, **hard-bound `127.0.0.1` by contract** | `server.rs:94-95` (PRD §15.2) |
| HTTP routes | Reachable on **both** listeners, **before any token check** | `server.rs:202-205` vs `:215` |
| Wire (pre-E2E) | Plain `ws://` — HTTP upgrade, `?token=`, `hs_*` messages all plaintext | ADR-011 §1; no TLS exists (std-only) |
| Wire (post-E2E) | Every Text frame ChaCha20-Poly1305 sealed | `server.rs:474-482` (open inbound), `:569-573` (seal outbound) |
| Tunnel (Phase 5′, not built) | Would terminate TLS at the edge; companion side stays `ws://` | ADR-011 §2; TODO:230-233 unchecked |
| At rest | PSKs DPAPI-protected, journal never holds plaintext | `storage.rs:72-76`, `ffi.rs:236` |

## 3. Verified facts

### 3.1 Auth-mode separation (how the layers compose)

1. **Transport token** (optional on loopback, mandatory off-loopback): `?token=` query **or** `Authorization: Bearer`, both constant-time compared (`server.rs:41-57`). Wrong token → socket upgrades, then Close **4001** (`:213-234`); ≥5 failures → HTTP **429** without upgrading (`:216-222`, limit `:27`); success resets the counter (`:236`).
2. **E2E session** (per-device PSK): `hs_hello → hs_challenge → hs_proof → hs_ok`, fresh server nonce+salt per attempt (`server.rs:496-533`); proof = HMAC-SHA-256 over role‖device_id‖nonce‖salt (`e2e.rs:47-65`), mutual (direction-bound, `e2e.rs:197-199`); handshake rejects expired/spent/unknown/**revoked** devices (`server.rs:321-365`); `hs_err` does **not** close the connection (verified by new test `hs_proof_before_hello_is_rejected_but_connection_survives`).
3. **E2E is client-opt-in, not enforced:** with no live session, arbitrary valid messages (e.g. `prompt_async`) are parsed and executed in **plaintext** and answered unsealed (`server.rs:483-495`, `:558-574`). This is the intentional parity/dev path (ADR-010 deviation note; differential A/B depends on it), but it means **token (or loopback) alone gates command access** — see open decision D2.
4. **Version gate:** wrong `hello` version → error ack then Close 4002, ordering guaranteed (`server.rs:534-556`; self-test `main.rs:537-563`). The check is `*protocol as u32 != PROTOCOL_VERSION` applied to the `f64` field (`server.rs:535`; `protocol: f64` `protocol.rs:329`), so `2.5` truncates to 2 and **passes the Rust gate**; parser-level acceptance of `2.5` is pinned by `protocol_number_field_follows_is_num_boundaries` (`protocol.rs:1058-1069`). The TypeScript host uses strict inequality (`msg.protocol !== PROTOCOL_VERSION`, `services/host/src/host.ts:85`) and **rejects** `2.5` (ack error + Close 4002, `host.ts:84-96`) — cross-implementation divergence, verified. Tested: parser acceptance. Untested: the Rust server-gate outcome for `2.5` (predicted gate-pass → `HelloAck`, `server.rs:558-564`, by code reading only) — see D7.

### 3.2 E2E handshake and sealed frames (post-compromise scope)

- Per-connection directional keys HKDF-SHA-256 from PSK + fresh random salt (`e2e.rs:31-43`); monotonic per-direction counter as nonce → replay-proof (`e2e.rs:141-168`); AEAD tag failure, counter skip, or non-UTF8 sealed payload **tears the connection down** (`server.rs:474-482`), unlike message-level garbage which only gets a `malformed` ack (`:492-494`; new tests `malformed_message_is_acked_and_connection_survives`, ws/json/e2e truncation sweeps).
- **No X25519 forward secrecy** (documented decision): compromise of a PSK exposes traffic recorded under the salts of sessions that used it. Fresh salt per connection limits blast radius to sessions using that device's PSK. Post-compromise secrecy is **not claimed** (`crypto.rs:11`).
- Handshake proof cannot be replayed elsewhere: bound to the server's fresh nonce + device + role (`e2e.rs:197-207`).

### 3.3 TLS trade-offs

- std-only Rust has no TLS crate; hand-rolling TLS is off the table (ADR-011 Context). Confidentiality/integrity therefore lives in the E2E layer, not the pipe — every transport (LAN, VPN, tunnel) is treated identically.
- **What the wire exposes pre-E2E:** handshake metadata (device_id, proof = HMAC output, nonce/salt), possibly `?token=` in URLs (PRD §11.4 flags URL leakage; ADR-006's motivation), and plaintext parity-mode commands if no E2E handshake is performed. Sealed frames reveal only lengths/timing.
- Tunnel option (Phase 5′): edge-terminated TLS protects the public hop; the LAN segment companion↔tunnel stays plaintext `ws://`. Acceptable **only** because E2E seals command traffic — this is the stated design (ADR-011 §1-2).

### 3.4 Browser Origin / CSWSH

- **No `Origin` header validation exists anywhere** in the WS handshake (reviewed call paths — every header consumer in the accept flow): `is_websocket_upgrade` checks only Upgrade/Connection/Sec-WebSocket-Key (`ws.rs:99-108`); `validate_upgrade` additionally checks the WebSocket version and key (`ws.rs:161-169`); neither — nor `authorized` (`server.rs:41-57`) — reads an `Origin` header.
- Consequence in the **default dev config** (loopback, no token): a malicious webpage could open `ws://127.0.0.1:8787` from the user's browser and — per §3.1.3 — execute plaintext commands without any credential. Mitigations today are client-side only (https→ws:// mixed-content blocking; Chrome local-network-access restrictions) which are browser/version dependent and **not relied upon here**.
- With a token set: browser WebSocket APIs cannot set `Authorization`, and `?token=` would have to be known → CSWSH needs token theft first.
- A future web app (ADR-011 §5) reuses the same handshake → an Origin allow-list decision will be required then. See D1.

### 3.5 Token handling

- Token value is never logged (all print/format sites inspected; startup logs only the flag `token auth on` / `token auth OFF`, `main.rs:187-197`); compared in constant time.
- `?token=` in query strings can still leak via proxies/history — **ADR-006 (short-lived single-use tickets) is accepted but unimplemented**: zero matches for "ticket" in `*.rs`; the token flow remains the production path. Conflict C1 stays open. See D3.

### 3.6 Revocation

- `companion revoke <id>` flips the registry flag (`main.rs:366-390`); it is enforced **at handshake time** (`server.rs:356-358`).
- A **live** session of a revoked device is **not** terminated — evidence: the revocation test proves only that the *next* handshake fails (`revoking_a_live_device_blocks_reconnect`, `server.rs:894-914`), and no code path disconnects existing sessions on revoke (grep: none). A lost-phone attacker keeps an open session until it drops; README:42 already states "at the next handshake". See D4.

### 3.7 Diagnostics exposure

- `/health` is served on **both** listeners with **no auth** (`server.rs:265-268`, route runs before the token check) and returns counters only — clients/hosts/sessions/pending/lastSeq/commands/unresolved/events/malformed/storageFailed/uptime/rss (`companion.rs:409-438`); no secrets, no entity contents.
- `/version` and `/adapters` are diagnostics-listener-only (`server.rs:270-284`) → loopback-only by the bind contract.
- Binding the main listener non-loopback (with a token) therefore exposes `/health` counters to the network — counts only; flagged as D6 rather than a vulnerability.

### 3.8 Abuse hardening (implementation verified; test coverage varies)

**Test-covered** (asserted in-suite): payload cap 256 000 B checked **before allocation** (`ws.rs:9`, `:270` — exact boundary pinned by `payload_size_boundaries_at_cap_and_length_marker_edges`); handshake cap 16 KiB (`ws.rs:10`, test `handshake_size_cap` `ws.rs:357-365`); ≤64 headers, colon-required lines (`ws.rs:136-151`, boundary tests added); hostile corpora for JSON/frames/handshake/client messages (this session: truncation sweeps, LCG stress, close-payload boundaries — 16 new tests).

**Implemented and verified in source, but NOT covered by any identified test** (no test coverage implied): heartbeat read timeout + miss-limit (`server.rs:434-450`); 30 s handshake read timeout (`server.rs:193`); auth-failure 429 pre-upgrade path (`server.rs:218-221`, limit `:27`, counter reset on success `:236`) — only the 4001 wrong-token close has test coverage (`main.rs:514-530`).

**Availability risk — unresolved (see D8):** outbound messages queue in an **unbounded** `std::sync::mpsc` channel (`server.rs:20`, `:396`), drained by a per-connection writer thread that blocks in `write_frame` with no write timeout configured anywhere (`server.rs:412-428`; no `set_write_timeout` in the codebase). The read-side miss-limit teardown (`server.rs:447-450`) fires only when inbound reads go silent — on timeout the server sends a Ping (`:451-455`) and any received frame resets `misses` (`:461`) — so a client that keeps answering but does not consume its socket can keep the connection alive while queued replies/events accumulate. This is a **plausible** slow-consumer memory-growth failure mode under the DoS adversary (§1); it is **not demonstrated and not claimed to be reliably exploitable**. No queue bound, drop policy, or write timeout exists; none is implemented by this document.

### 3.9 Endpoint / tunnel trust boundaries (current state, Phase 5′ unbuilt)

- QR carries one `--endpoint` ws-url + PSK + challenge today (the QR **schema** accepts an endpoint **array** — `pairing.rs:71` — while the current CLI passes a single endpoint — `main.rs:319`; `README:21`, `qr_ref_data.rs:18`); **runtime endpoint-list advertisement and ordered reconnect do not exist yet** (ADR-011 §2 vs TODO:230-233).
- Default deployment exposes no non-loopback listener and adds no remote path; no tunnel is integrated; nothing is advertised anywhere. Phase 5′ must keep that invariant until its own gate passes.

## 4. Unverified assumptions (explicitly not proven)

- **No external/independent security audit** (TODO:246-247 — the hostile-input corpus + KATs are the only adversarial evidence).
- Constant-time behavior of `ct_eq`/Poly1305 arithmetic is by construction, **never measured** by side-channel instrumentation.
- No fuzzing, Miri, ASAN/UBSAN, or Haskell-style property harness ran over the hand-rolled crypto/framing; determinism evidence is the seeded corpora only.
- DPAPI assumes the Windows user-profile boundary; same-user code can unprotect PSKs (by design of DPAPI).
- Browser-side CSWSH mitigations (mixed content, local-network access) vary by browser/version — not tested against real browsers.
- Malicious-local-process adversary in loopback-no-token mode is accepted in dev parity mode; not a claim for production posture.

## 5. Open decisions (product owner; none taken here)

- **D1 — Origin policy:** allow-list for browser WS origins vs "no browsers supported until the web app". Required before any web client ships; also covers CSWSH in default config (§3.4).
- **D2 — Plaintext parity path:** enforce `e2e.is_some()` before non-handshake messages (breaking dev/differential parity, which currently relies on plaintext) vs keep token-only gating and document it as the production posture. Directly affects what a stolen token is worth (§3.1.3).
- **D3 — ADR-006 tickets:** implement the ticket migration (scope/date) or formally re-schedule with an interim statement. Until then the long-lived query token stays the production credential (PRD §11.4, C1).
- **D4 — Revocation semantics:** also terminate live sessions on revoke (needs a registry→connection watch) vs keep "next handshake" semantics with the exposure documented (§3.6).
- **D5 — Transport TLS:** accept edge-terminated tunnel TLS + E2E as the complete story (ADR-011) vs revisit if a first-party hosted relay ever enters scope. std-only constraint makes native TLS impossible without changing ADR-010.
- **D6 — `/health` on non-loopback:** gate it behind the token when bind ≠ loopback, or accept counters-only exposure (§3.7).
- **D7 — Version-gate strictness:** verified divergence — Rust truncates `2.5` and passes the gate; TypeScript strict `!==` rejects it (§3.1.4). Parser-side acceptance is already pinned (`protocol.rs:1058-1069`); decide which gate behavior is correct (strict equality vs the truncating cast, with TS parity as the reference), then add a **server-gate** test asserting the chosen behavior. That server-gate test is the remaining action; not written here.
- **D8 — Outbound queue bound:** the per-connection outbound queue is an unbounded `std::sync::mpsc` channel (`server.rs:396`; writer thread `:412-428`, no write timeout) — decide between bounding it (capacity + drop/backpressure policy) and formally accepting the slow-consumer memory-growth risk documented in §3.8. Neither implemented; exploitation not demonstrated.

## 6. Release gates

Status: ✅ passed **with cited evidence** · ⛔ not passed · ⏳ deferred to a later phase. Nothing below may be flipped without its evidence.

| Gate | Status | Evidence / requirement |
|---|---|---|
| **S1** Hostile-input corpus green (JSON, frames, handshake, client messages, E2E envelopes) | ✅ | `npm run verify:rust` 2026-10-23: **145 passed / 0 failed / 3 ignored**, clippy `-D warnings` clean, fmt clean, self-test `ALL CHECKS PASSED`; incl. 16 new boundary/truncation/stress tests this session |
| **S2** Crypto known-answer tests (SHA-256, HMAC RFC 4231, HKDF RFC 5869, ChaCha20 §2.3.2, Poly1305 RFC 8439, AEAD tag) | ✅ | in-suite `crypto.rs` KATs, part of the S1 run |
| **S3** E2E adversarial matrix (MITM/bad proof, replay, desync, tamper, QR reuse/expiry, revoked device, reconnect) | ✅ | `server.rs` `e2e_tests` + `e2e.rs` tests in the S1 run; state-integrity test added this session |
| **S4** Manual phone-scan gate (real Android scans QR, connects, survives reconnect) | ⛔ | needs Phase 6 mobile client + physical device |
| **S5** Origin/CSWSH policy decided, implemented, tested (D1) | ⛔ | decision first; then handshake test with allowed/denied origins |
| **S6** Plaintext-command posture decided and matching code+docs (D2) | ⛔ | decision first |
| **S7** ADR-006 ticket migration implemented **or** re-scheduled with interim statement (D3) | ⛔ | currently neither implemented nor re-decided |
| **S8** Revocation semantics for live sessions decided; implemented if "terminate" (D4) | ⛔ | decision first |
| **S9** Independent security audit | ⛔ | explicitly out of current scope; tracked honestly (TODO:246) |
| **S10** Phase 5′ endpoint advertisement + reconnect security review (ordering, downgrade, replay of endpoint lists) | ⛔ | Phase 5′ scope; nothing built yet |
| **S11** 24 h soak window (separate-process harness) | ⏳ | Phase 8 (harness resets on sleep/reboot today) |
| **S12** `npm run verify` + `npm run verify:rust` green at every commit | ✅ (this change) | both exit 0 on 2026-10-23 (fmt failure was caught and fixed pre-report) |

## 7. Explicitly not done by this document

No code changed (this file is documentation only): TLS not implemented, E2E handshake untouched, auth untouched, no endpoint advertised, no remote access added, no non-loopback listener exposed, no gate flipped without evidence, phase map and TODO phase list untouched.
