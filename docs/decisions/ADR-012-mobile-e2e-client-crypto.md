# ADR-012: Mobile E2E client — pure-JS `@noble/*` crypto, QR payload gains `dev`

**Status:** Accepted
**Date:** 2026-10-23
**Relates to:** ADR-002 (pairing-only identity), ADR-009 (mock-mode honesty), ADR-011 (E2E
session wire contract), ADR-010 (std-only Rust companion); `docs/validation/PHONE-VALIDATION-CHECKLIST.md`
§"To unblock the Phase 6 rows" items 1-2

## Context

The phone must implement the frozen E2E wire contract in `services/companion-rs/README.md`:
HMAC-SHA-256 handshake proofs, HKDF-SHA-256 directional keys, and ChaCha20-Poly1305 frame
sealing with strict counter nonces. The checklist deferred this choice to the product/security
owner; the continuous-build mandate delegated it with explicit criteria (Expo compatibility,
cryptographic correctness, maintenance, no hand-rolled primitives, no silent wire changes).

Two concrete blockers existed before any client code could be written:

1. No crypto library for those three primitives was installed anywhere in the repo.
2. **Contract defect:** the QR payload (`payload_json`) did not carry the device id, yet the
   server redeems a pairing offer by `store.pairings.get(&hs_hello.deviceId)` (`server.rs:407`)
   and the id was only printed as terminal text. A QR-scan-only client could not pair — the
   documented flow was unimplementable as specified.

## Decision

### Crypto stack: `@noble/hashes@2.4.0` + `@noble/ciphers@2.4.0` (pure JS)

Installed as dependencies of `@chernobyl/protocol`, which owns the wire format, so the mobile
app and the Node interop tests share one implementation of the client side.

- **Correctness proven, not assumed:** noble's outputs were verified against the same RFC
  vectors the Rust suite asserts (RFC 4231 HMAC case 1 `b0344c61…`, RFC 5869 HKDF case 1
  `3cb25f25…`, RFC 8439 §2.8.2 AEAD tag `1ae10b59…`) — exact matches before any product code
  was written.
- **No build-model change:** pure JS keeps `expo export` / Expo Go / the existing dev flow
  exactly as it is (Expo SDK 58, managed).
- **No client CSPRNG required by the protocol:** the server supplies the handshake `nonce` +
  `salt`, the PSK arrives out-of-band via the QR, and the AEAD nonce is the deterministic
  per-direction counter — so there is no point where the phone must generate secret bytes.
- **Performance is a non-issue at this scale:** control frames are hundreds of bytes at
  heartbeat cadence; native acceleration would not be measurable.

**Rejected: `react-native-quick-crypto`.** It changes the build model (prebuild / dev-client,
no longer Expo Go), adds a native toolchain + per-RN-upgrade maintenance burden, and its only
advantage is speed for throughput this protocol never reaches. The primitives are the same
algorithms — no security gain.

**Rejected: hand-rolled TS crypto.** The mandate and ADR-011 both require an established,
well-reviewed implementation over custom crypto where one exists.

### QR scanning: `expo-camera`, with a paste-JSON fallback

`expo-camera` is part of the Expo SDK 58 bundled native modules, so it does **not** change the
build model (consistent with the choice above). The pairing screen accepts the raw QR JSON as
paste/type text too — that path is fully exercisable without a camera. **No camera code has
been run on a physical device; the checklist device rows stay NOT RUN until they carry
recorded evidence.**

### Wire-contract amendment: QR payload carries `dev`

`PairingOffer::payload_json` now emits `"dev": "<device_id>"` alongside `v/host/ep/psk/chal/exp`.
This is **additive and documented, not a silent change:**

- The device id is not a secret (it travels in plaintext inside `hs_hello` and is printed to
  the terminal today); the payload already carries the stronger secret (the PSK), so no
  security property is weakened.
- The handshake crypto is untouched: `proof = HMAC-SHA256(psk, role ‖ 0x00 ‖ deviceId ‖ 0x00 ‖
  nonce ‖ 0x00 ‖ salt)` and key derivation are exactly as ADR-011 and the README specify.
- No released client exists whose behavior could break; the field makes the first working
  client possible. Parsers must reject payloads missing `dev` with a clear error.

## Consequences

- `@chernobyl/protocol` grows two dependencies (both audited, zero-vuln at install); the Rust
  companion stays std-only — nothing changes in `services/companion-rs/Cargo.toml`.
- The pairing payload shape is now part of the frozen contract: `v, host, ep, dev, psk, chal,
  exp` (documented in `services/companion-rs/README.md`).
- Interop is verified by a Node test that spawns the real companion binary and runs the full
  handshake + sealed frames + negative cases (replay, tamper, desync, wrong PSK, revoked
  device, restart) — cross-language evidence, not a device claim.
- Device-only validation (camera scan, foreground service, on-device WS lifecycle) remains
  blocked on physical hardware and is tracked in the phone checklist.
