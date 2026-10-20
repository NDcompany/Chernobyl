# ADR-002: Pairing-only identity, no passwords

**Status:** Accepted
**Date:** 2026-10-21
**Decided by:** Product owner during Phase 0 planning review
**Relates to:** PRD §11 (authentication and pairing), §10.2 identity service

## Context

With ADR-001 removing the account server, the PRD's register/login model has no home. The PRD also forbids inventing a password system. The product owner chose "Pairing-only, no passwords" explicitly.

## Decision

Identity toward a companion is established only by device pairing:

1. The companion (desktop) generates a **single-use, short-lived, rate-limited pairing code**.
2. The phone enters the code through the authenticated app flow; the companion validates it and issues a **signed device credential** (short-lived access token + refreshable credential, stored in the device keystore).
3. The gateway refuses every connection that does not present a valid credential for a paired, non-revoked device.
4. Possession of the tunnel/LAN URL alone grants nothing. Pairing attempts are rate-limited; codes expire and cannot be reused (PRD §11.2 lifecycle retained, minus the account service).
5. Revocation is per-device and per-host, enforced by the companion; revoked credentials and existing connections are rejected immediately (PRD §11.3 retained).

No username/password exists anywhere in the system. The PRD §10.2 "reputable identity provider" requirement is satisfied vacuously — there are no passwords to manage.

## Consequences

- Device credential issuance/verification and pairing-challenge tables live in companion SQLite (Phase 4′).
- Security tests: expired/replayed pairing code, revoked credential reuse, rate-limit exhaustion (PRD §22.5).
- Connection tickets replace the prototype's query-string token (see ADR-006).
- If a future product need requires user accounts, revisit with a maintained auth library — do not hand-roll passwords.
