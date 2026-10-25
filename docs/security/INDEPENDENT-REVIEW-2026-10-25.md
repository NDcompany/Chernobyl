# Independent security review — 2026-10-25

**What this is:** an adversarial review of the E2E, pairing, authorization and parsing code by
95 AI reviewer agents. None had written or seen the code before, and none were told the
authors' conclusions. Seven reviewers each covered one dimension (crypto primitives,
handshake, session transport, server authorization, parsers and DoS, secrets at rest,
mobile client). Three independent skeptics then tried to refute every finding against the
code: one checked the code itself, one checked whether it was exploitable under this threat
model, and one checked the severity. A finding counted only if at least 2 of 3 upheld it.

**What this is not:** a human third-party audit. Gate S9 asks for an independent reviewer;
this review is the strongest substitute available here. It does not replace a human audit
before a public release.

**Result:** 29 raw findings, 22 upheld (several duplicates), 7 refuted. All upheld findings
are fixed, with regression tests, in commits `dc8856e`, `c9daa2f`, `c4b84c8`, `eeda3d3`,
`b32c916` and `cdb2c25`. Each fix was gated by `cargo test` / clippy / fmt, the TS suites
and the interop run against the real binary.

## Upheld findings and fixes

| Severity | Finding | Fix |
|---|---|---|
| high | The client added no freshness to the handshake, so a recorded `hs_challenge`/`hs_ok` replayed to the phone authenticated an impostor host and made the phone reuse ChaCha20-Poly1305 key+nonce pairs | `hs_hello` carries a 16-byte client nonce, bound into both proofs and the HKDF salt, byte-exact in Rust and TS; new golden frames; the phone gets a platform CSPRNG via expo-crypto (`b32c916`, `cdb2c25`) |
| high | At remote posture, engine events reached connections that never handshook, in plaintext; the same race could interleave a plaintext event into a phone's handshake | Bulk fan-out is dropped for sessionless connections at remote posture and while a handshake is in flight; `hs_ok` is the last plaintext frame (`c4b84c8`, `b32c916`) |
| high | Any local process or Windows user could mint a working pairing PSK through `POST /pairing/offer` | A control key (`<data_dir>/control.key`, from the CSPRNG) is required on `/pairing/offer` and `/devices/revoke`. The CLI sends it only to a companion that holds the journal lock (`c4b84c8`, `eeda3d3`) |
| medium | `POST /hooks/claude` lacked the Origin guard | Origin refused (`c4b84c8`) |
| medium | Slowloris: the request deadline was only checked on read timeouts | The deadline is checked before every read: 30 s for the request head and body, 45 s mid-frame (`c4b84c8`) |
| medium | JSON duplicate-key detection was O(n²) before authentication | Linear, with a HashSet (`dc8856e`) |
| medium | HTTP client: an overflowing chunk size panicked (panic=abort), and the read had no overall deadline | Checked arithmetic and a 30 s overall deadline (`dc8856e`) |
| medium | CLI store opens could compact the live journal and orphan the daemon's append handle; an offline `--revoke` against a live daemon was silently lost | The daemon holds an exclusive data-dir lock; `--devices` is read-only; offline writes and `--restore` refuse while a daemon runs (`c9daa2f`) |
| low | DPAPI decryption ran under the global lock for unauthenticated proofs | Decryption runs outside the lock, with a re-check under it (`b32c916`) |
| low | A revocation racing a finishing handshake could leave a live session | The revoked flag is re-checked after the session is installed (`b32c916`) |
| low | `--restore` silently rolled back revocations | Revocations of the replaced journal are re-applied (`c9daa2f`) |
| low | The adapter `basicAuth` is plaintext in companion.json, contrary to PRIVACY.md | PRIVACY.md corrected (`c9daa2f`) |
| low | `--require-e2e` on a loopback bind (the tunnel recipe) did not require a token | It now requires a token, like a public bind (`c9daa2f`) |
| low | One unauthenticated frame (`hs_err`, a garbage server proof, close 4001/4003) puts the phone in terminal `denied` | Accepted residual: an on-path attacker can always deny service. Recovery is reopening the app or re-pairing; documented |
| info | A captured pairing payload remains a device credential after redemption | Accepted residual of a symmetric PSK design without DH (threat model). Treat the QR like a password and revoke if it is exposed |

## Refuted (0 or 1 of 3 upheld)

- `hs_err` reasons reveal device or offer state (no exploitable signal under this threat model).
- The 4003 close frame races the writer thread. Possible but harmless: the reconnect is
  refused with `hs_err device revoked`, which is also terminal.
- No Host-header validation on the loopback diagnostics listener (DNS rebinding can only read
  counters; the control routes need the control key and refuse Origin).
- The full pairing payload is printed on pairing (by design for paste-pairing; it expires in
  5 minutes and is single-use for registration).
- The payload shows in a text field after scanning (on-device only; same exposure as the QR).
- Token on the command line or in the URL (the token is admission only since D9; it carries
  no state or command access).
- Downgrade to plaintext when a stored pairing fails to parse (only in dev builds with
  `EXPO_PUBLIC_HOST_WS_URL`; a release build falls back to the demo).

## Found later on the emulator (not by the review)

- React Native's Android WebSocket adds a default `Origin`, so every phone connection was
  refused by D1. The app now sends `Origin: chernobyl://app`, the only Origin accepted
  (`e0951db`).
- Release builds blocked cleartext `ws://` entirely (`aebdb49`).
