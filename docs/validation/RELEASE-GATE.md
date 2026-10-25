# Phase 9 — production release gate

**PRD:** §25 Phase 9, §26 Definition of Done. **Assessed:** 2026-10-25 (late) at the commit
that last updates this file. Every row says what was run and what it showed. A row is ✅ only
with evidence, ⛔ when an external condition blocks it, and ⚠️ when it is partly met.

## Verdict

**Ready for private use; a public release still needs three external things:** one run of
the checklist on a physical Android phone (real camera, radio and OEM battery policy), a human
third-party security audit, and the signing decisions (debug-signed APK, unsigned Windows exe
with published checksums, ADR-007).

Everything a machine without a phone can prove has been proven. The release APK runs on an
Android 15 emulator against the real companion and passes 14 checklist rows. An independent
adversarial review by 95 agents found 22 issues; all are fixed.

| # | PRD Phase 9 requirement | Status | Evidence |
|---|---|---|---|
| 1 | Security tests pass | ✅ | `npm run verify:rust`: crypto known-answer tests (RFC 4231/5869/8439), the E2E adversarial matrix (replay, tamper, desync, bad proof, expired/spent QR, revoked device, live revocation 4003, replayed server transcript with a fresh client nonce) and golden sealed frames at n=0/1 pinned in both languages. `@chernobyl/e2e` 37/37; interop with the real binary 19/19. Independent review: `docs/security/INDEPENDENT-REVIEW-2026-10-25.md` |
| 2 | Real end-to-end test passes | ✅ (emulator) / ⚠️ (phone) | Release APK on an Android 15 emulator against the real companion: pairing, E2E handshake, live snapshot, restart recovery, network loss, background service, live revoke and token refusal all pass (`PHONE-VALIDATION-CHECKLIST.md`, `evidence/2026-10-25-emulator/`). Companion with real OpenCode 1.19.4: PASS. **Not yet on a physical phone;** camera QR decoding is unverified |
| 3 | Installers work on the supported platforms | ✅ / ⚠️ | Windows: release exe plus `release/SHA256SUMS.txt`, `--verify` (ADR-007). Android: `node scripts/build-android.mjs` builds the release APK (arm64-v8a + x86_64), which installs and runs on the emulator. It is debug-signed (installable, not a Play identity) |
| 4 | Account deletion and revocation work | ✅ | No accounts (ADR-002). `--revoke` closes the live session (4003); on the emulator the app went to "Access denied" and its background service stopped (row 7.1); 5 relaunches with the revoked pairing stayed denied without a crash (row 7.2). Phone: Settings → Security → *Unpair this host* |
| 5 | Privacy and retention policies are published | ✅ | `docs/PRIVACY.md`; every store is bounded |
| 6 | Backup restore is tested | ✅ | backup tests; live CLI round trip with an identical SHA-256; `--restore` keeps revocations and refuses while a companion runs |
| 7 | No production credentials are committed | ✅ | Full `git log --all -p` scan, no hits |
| 8 | Supported integrations have documented versions and capabilities | ✅ | `docs/INTEGRATIONS.md` and `GET /adapters` |
| 9 | No unimplemented mock feature is advertised as real | ✅ | Demo data carries `simulated` end to end: on a real paired phone the companion's demo adapter shows the Simulation badge and "Simulated" tags (`evidence/2026-10-25-emulator/1.1-home.png`). The fake sign-in is gone; a live host never starts with demo data; uncertain outcomes show as unknown |

## Security gates carried from the threat model

S1–S3, S5–S8, S10–S12 passed with cited evidence (`docs/decisions/PHASE5-THREAT-MODEL.md` §6;
S8 = live revocation, D4). **S4** (device validation): the emulator run is complete; one
physical phone is still owed. **S9** (independent audit): the adversarial agent review is done
and all its findings are fixed; a human third-party audit is still owed before a public release.

## What a release still needs

1. Run `node scripts/device/validate.mjs` (or the checklist by hand) on one physical Android
   phone, including a real camera scan of a pairing QR and an approval arriving as a
   notification while backgrounded.
2. A human third-party review of the E2E layer (`e2e.rs`, `crypto.rs`, `server.rs` handshake,
   `packages/e2e`). Hand over `docs/security/INDEPENDENT-REVIEW-2026-10-25.md`, the threat
   model, ADR-011 to ADR-014 and the golden vectors.
3. Signing: a release keystore for the APK (and a Play `specialUse` foreground-service
   declaration if it ever ships on Play); Windows code signing stays an ADR-007 decision.

## Final verification run

Filled in by the final pass.
