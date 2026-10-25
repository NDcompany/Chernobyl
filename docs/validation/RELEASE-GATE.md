# Phase 9 — production release gate

**PRD:** §25 Phase 9, §26 Definition of Done. **Assessed:** 2026-10-25 at the commit that adds or
last updates this file. Every row says what was run and what it showed. A row is ✅ only with
evidence, ⛔ when an external condition blocks it, and ⚠️ when it is partly met.

## Verdict

**Not releasable to the public yet.** The code-side gates pass. Three items need conditions
this machine does not have: an Android device or SDK, an independent security reviewer, and
a code-signing certificate (decided as "unsigned + checksums", ADR-007). Private use on your
own desktop and phone is supported once the device rows below have been run.

| # | PRD Phase 9 requirement | Status | Evidence |
|---|---|---|---|
| 1 | Security tests pass | ✅ | `npm run verify:rust`: the Rust suite covers the crypto known-answer tests (RFC 4231/5869/8439), the E2E adversarial matrix (replay, tamper, desync, bad proof, expired/spent QR, revoked device, live revocation 4003) and golden sealed frames at n=0/1 pinned in both languages. `@chernobyl/e2e`: 35/35. Interop against the real binary (`npm run verify:e2e`): 19/19. See "Final verification run" below for the counts. |
| 2 | Real end-to-end test passes | ⚠️ | Companion ↔ real OpenCode 1.19.4: PASS (`docs/validation/PHASE3-LIVE-OPENCODE-E2E.md`). Phone client ↔ real companion over real sockets: PASS (interop, Node runtime of the same `@chernobyl/e2e` package the app ships). **Not run:** the app on a physical phone (`PHONE-VALIDATION-CHECKLIST.md`, gate S4, no Android device or SDK). |
| 3 | Installers work on the supported platforms | ⚠️ | Windows: `node scripts/package-release.mjs` builds the release exe and `release/SHA256SUMS.txt`, and `--verify` re-hashes it (ADR-007, unsigned by decision, SmartScreen guidance in `docs/INSTALL.md`). Autostart install/uninstall verified live. **Android APK:** not buildable here (no Android SDK); `expo export --platform android` passes as the bundle check. |
| 4 | Account deletion and revocation work | ✅ | There are no accounts (ADR-002). Revocation: `--revoke` reaches the running companion and closes the device's live session with 4003; after a restart it is still refused (interop scenario 7, `revocation_route_terminates_live_session_…`). Phone side: Settings → Security → *Unpair this host* deletes the pairing key and token from the keystore. |
| 5 | Privacy and retention policies are published | ✅ | `docs/PRIVACY.md`. Every store has a bound: commands 5 000, events 5 000 / 7 days, audit 1 000 (desktop and phone), journal compaction at 64 MB, HTTP bodies 16 MiB, SSE lines 1 MiB. |
| 6 | Backup restore is tested | ✅ | `backup::round_trip_restores_original_bytes`, `backup::tampered_backup_is_refused` (SHA-256 sidecar). Live CLI round trip in the final run below. |
| 7 | No production credentials are committed | ✅ | Full `git log --all -p` scan for API keys, tokens, private keys and `HOST_TOKEN=` values: no hits (2026-10-25). The token is env-only; PSKs are DPAPI-protected at rest. |
| 8 | Supported integrations have documented versions and capabilities | ✅ | `docs/INTEGRATIONS.md` plus the live `GET /adapters` route. |
| 9 | No unimplemented mock feature is advertised as real | ✅ | ADR-009 Simulation badge on every data screen in mock mode; the fake "Sign In" was replaced by *Pair with a host* / *Explore the demo (Simulation)* (C14); "Sign out on all devices", which did nothing remote, was removed; uncertain command outcomes show as unknown, never as done. |

## Security gates carried from the threat model

S1–S3, S5–S7, S10–S12 passed with cited evidence (`docs/decisions/PHASE5-THREAT-MODEL.md` §6).
**S8 (live revocation)** is now implemented and tested (D4, ADR-014 accepted 2026-10-25).
**S4** (physical device) and **S9** (independent security audit) remain ⛔ on external
conditions. D9 was decided 2026-10-25 (option B: beyond loopback the state snapshot is
session-only).

## What a release still needs

1. Run `PHONE-VALIDATION-CHECKLIST.md` rows 1.2, 1.3, 4.1, 5.1, 6.1, 6.2 and 8.1 on a real
   Android phone, including a camera QR scan.
2. Build and install a signed or development APK (needs the Android SDK).
3. An independent review of the E2E layer (`e2e.rs`, `crypto.rs`, `packages/e2e`). The package
   is kept audit-ready: threat model, ADRs 011–014, golden vectors.
4. Optional: an Android foreground service so the WebSocket survives long backgrounding. Today
   the app re-dials when it returns to the foreground and notifications are local only.

## Final verification run

Run on 2026-10-25 against the tree that adds this file:

| Suite | Result |
|---|---|
| `npm run verify` (typecheck, lint, TS tests, builds incl. Android export) | exit 0. Tests: domain 13/13, host 22/22, adapter-sdk 9/9, e2e 35/35. Lint: 0 errors, 2 pre-existing warnings (RHF `watch`) |
| `npm run verify:rust` | Rust 219 unit + 4 CLI + 5 lifecycle integration, 0 failed (4 + 1 ignored microbench/live tests); clippy `-D warnings` clean; fmt clean; `--self-test` ALL CHECKS PASSED |
| `npm run verify:e2e` (Node client vs real binary) | 19/19 PASS, including live revoke → 4003 |
| `npm run soak:smoke` | 10/10 criteria, `pass: true` (the 24 h S11 run passed 2026-10-24 on `f47c4f6`; later transport changes have smoke evidence, not a new 24 h run) |
| `node scripts/package-release.mjs` + `--verify` | fresh release build (687 KB), `SHA256SUMS.txt` written and verified |
| Live CLI on the release exe | `--pair` against the running companion: "pairing offer created via running companion"; `/health` shows `connectionsActive` / `connectionsRejected`; `--backup` → `--restore` round trip with an identical SHA-256 |
| `git diff --check` | clean |
