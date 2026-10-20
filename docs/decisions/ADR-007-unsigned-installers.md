# ADR-007: Unsigned installer + published checksums (initially)

**Status:** Accepted
**Date:** 2026-10-21
**Relates to:** PRD §6.3 (signed installer), §18.3 (secure update verification); conflict register C12

## Context

PRD asks for a signed installer "where the distribution platform supports it". Code signing on Windows costs money (OV/EV certificate) and macOS requires an Apple Developer account; neither exists for this project.

## Decision

- Phase 8 ships the companion installer as an **unsigned binary with published SHA-256 checksums** and a documented verify step.
- Update downloads (when implemented) must still verify the checksum/signature of the payload against the release manifest before applying.
- This is a documented release limitation until a signing certificate is obtained.

## Consequences

- Windows SmartScreen warnings on first run are expected and must be covered in the install docs (Phase 8 exit criteria: troubleshooting documentation).
- The release gate (Phase 9) records "unsigned distribution" as an accepted, disclosed risk — not a silent gap.
