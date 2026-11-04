# Phase 9 — production release gate

**PRD:** §25 Phase 9, §26 Definition of Done. **Assessed:** 2026-10-25 (late); counts, evidence
lines and live-session status updated 2026-11-04 at `62c0aba`. Every row says what was run and what it showed. A row is ✅ only
with evidence, ⛔ when an external condition blocks it, and ⚠️ when it is partly met.

## Verdict

**Ready for private use; a public release still needs three external things:** one run of
the checklist on a physical Android phone (real camera, radio and OEM battery policy), a human
third-party security audit, and the signing decisions (debug-signed APK, unsigned Windows exe
with published checksums, ADR-007).

Everything a machine without a phone can prove has been proven. The release APK runs on an
Android 15 emulator against the real companion: 30 of the 31 checklist rows in sections 1-8
were run and all pass (row 6.3 needs a PC reboot). An independent adversarial review by 95
agents found 22 issues; all are fixed. The ADR-015 live agent sessions landed on 2026-11-04;
their emulator rows (section 9, 9.1-9.13) are NOT RUN, so no row below counts them.

| # | PRD Phase 9 requirement | Status | Evidence |
|---|---|---|---|
| 1 | Security tests pass | ✅ | `npm run verify:rust`: crypto known-answer tests (RFC 4231/5869/8439), the E2E adversarial matrix (replay, tamper, desync, bad proof, expired/spent QR, a QR replayed after pairing (refused once the phone acked its rotated device key, threat model §3.11), revoked device, live revocation 4003, replayed server transcript with a fresh client nonce) and golden sealed frames at n=0/1 pinned in both languages. `@chernobyl/e2e` 40/40; interop with the real binary 26/26. Independent review: `docs/security/INDEPENDENT-REVIEW-2026-10-25.md` |
| 2 | Real end-to-end test passes | ✅ (emulator) / ⚠️ (phone) | Release APK on an Android 15 emulator against the real companion: 30 of 31 rows in sections 1-8 run, all pass (pairing, E2E handshake, QR replay refused after the device key rotation, live snapshot, restart recovery, network loss, background service, live revoke, token refusal; `PHONE-VALIDATION-CHECKLIST.md`, `evidence/2026-10-25-emulator/`). Companion with real OpenCode 1.19.4: PASS. **Not yet on a physical phone;** camera QR decoding is unverified |
| 3 | Installers work on the supported platforms | ✅ / ⚠️ | Windows: release exe plus `release/SHA256SUMS.txt`, `--verify` (ADR-007). Android: `node scripts/build-android.mjs` builds the release APK (arm64-v8a + x86_64), which installs and runs on the emulator. It is debug-signed (installable, not a Play identity) |
| 4 | Account deletion and revocation work | ✅ | No accounts (ADR-002). `--revoke` closes the live session (4003); on the emulator the app went to "Access denied" and its background service stopped (row 7.1); 5 relaunches with the revoked pairing stayed denied without a crash (row 7.2). Phone: Settings → Security → *Unpair this host* |
| 5 | Privacy and retention policies are published | ✅ | `docs/PRIVACY.md`; every store is bounded |
| 6 | Backup restore is tested | ✅ | backup tests; live CLI round trip with an identical SHA-256; `--restore` keeps revocations and refuses while a companion runs |
| 7 | No production credentials are committed | ✅ | Full `git log --all -p` scan, no hits |
| 8 | Supported integrations have documented versions and capabilities | ✅ | `docs/INTEGRATIONS.md` and `GET /adapters`. Codex is listed with every capability `limited` (fake app-server only, no live run) |
| 9 | No unimplemented mock feature is advertised as real | ✅ | Demo data carries `simulated` end to end: on a real paired phone the companion's demo adapter shows the Simulation badge and "Simulated" tags (`evidence/2026-10-25-emulator/1.1-home.png`). The fake sign-in is gone; a live host never starts with demo data; uncertain outcomes show as unknown |

## Security gates carried from the threat model

S1–S3, S5–S8, S10–S12 passed with cited evidence (`docs/decisions/PHASE5-THREAT-MODEL.md` §6;
S8 = live revocation, D4). **S4** (device validation): the emulator run is complete; one
physical phone is still owed. **S9** (independent audit): the adversarial agent review is done
and all its findings are fixed; a human third-party audit is still owed before a public release.

## Live agent sessions (ADR-015, 2026-11-04)

| Runtime | Evidence | Status |
|---|---|---|
| Claude Code (driven `claude.exe`) | Unit and fake-process tests; live round trip `live_claude_haiku_inline_permission_round_trip` against Haiku, CLI 2.1.310, about $0.01 | ✅ live on the desktop; ⛔ emulator rows 9.1-9.11 NOT RUN |
| OpenCode (attach) | `live_opencode_rich_parts` and `live_opencode_end_to_end` passed on an isolated `opencode serve` 1.19.4 with `qwen3.5:4b` (24k context) on local Ollama | ✅ live on the desktop; ⛔ emulator row 9.12 NOT RUN |
| Codex (driven `codex app-server`) | Unit, fake-process and interop scenario 11 tests against `tools/fake-codex-app-server.mjs` | ⚠️ `limited`: no live run |
| Old APK against the new companion | — | ⛔ row 9.13 NOT RUN |

Threats T1-T11 and their controls as built: threat model §3.12. Transcripts are memory-only and
never journaled (`docs/PRIVACY.md`).

## What a release still needs

1. Run `node scripts/device/validate.mjs` (or the checklist by hand) on one physical Android
   phone, including a real camera scan of a pairing QR and an approval arriving as a
   notification while backgrounded.
2. A human third-party review of the E2E layer (`e2e.rs`, `crypto.rs`, `server.rs` handshake,
   `packages/e2e`). Hand over `docs/security/INDEPENDENT-REVIEW-2026-10-25.md`, the threat
   model, ADR-011 to ADR-014 and the golden vectors.
3. Signing: a release keystore for the APK (and a Play `specialUse` foreground-service
   declaration if it ever ships on Play); Windows code signing stays an ADR-007 decision.
4. Run the live-session rows 9.1-9.13 on the emulator, then on the physical phone.
5. A live Codex run before Codex is described as anything but `limited`.

## Final verification run

Run on 2026-10-25 (night) against the commit that updates this file:

| Suite | Result |
|---|---|
| `npm run verify` | exit 0. Tests: domain 13/13, host 24/24, adapter-sdk 9/9, e2e 37/37. Lint: 0 errors (2 pre-existing RHF `watch` warnings). Android export builds |
| `npm run verify:rust` | 241 unit + 7 CLI + 5 lifecycle integration tests, 0 failed (4 + 1 ignored microbench/live); clippy `-D warnings` and fmt clean; `--self-test` ALL CHECKS PASSED. **2026-11-04 at `62c0aba`** (`cargo test` only): 369 unit + 13 CLI + 5 lifecycle, 0 failed (6 + 2 ignored) |
| Interop (Node client vs the real binary) | 19/19 PASS, including live revoke → 4003 |
| `npm run soak:smoke` | 10/10, `pass: true` (the 24 h S11 run passed 2026-10-24 on `f47c4f6`; later builds have smoke evidence only) |
| Emulator, release APK | 30 of 31 rows in sections 1-8 run, all pass (row 2.2 on 2026-10-26 after the device key rotation; 6.3 NOT RUN, needs a PC reboot; `evidence/2026-10-25-emulator/results.json`); row 1.1 re-passed on the APK with the new logo and live wiring (`ae7d47b`). Section 9 (live sessions): NOT RUN |
| `node scripts/build-android.mjs` | `release/chernobyl-0.1.0-arm64-v8a+x86_64.apk` (71 MB, both native ABIs) |
| `node scripts/package-release.mjs` + `--verify` | exe + APK hashed into `release/SHA256SUMS.txt`, both verified |
| `git diff --check` | clean |
