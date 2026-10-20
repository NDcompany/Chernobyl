# ADR-005: Keep node:test; end-to-end via Playwright on the mobile web build

**Status:** Accepted
**Date:** 2026-10-21
**Relates to:** PRD §5.1 (tests), §22.6 (end-to-end), §2.1-7 (reuse workspace tooling)

## Context

The repository's suite runs on Node's built-in `node:test` (14 tests, wired into `npm run verify`). The PRD's first production E2E gate describes phone-driven flows, but this machine has no Android SDK (build check = `expo export`), and adding a device-test framework would be a large dependency with nothing to run it on. The Expo app's web build already renders every screen and is driven successfully by browser automation during development.

## Decision

- **`node:test` stays** as the runner for unit, contract, integration and security tests — no Vitest/Jest migration without a documented gap.
- **E2E is automated against the mobile web build via Playwright** plus `node:test` orchestration for companion/relay processes, covering the PRD §22.6 steps that do not require a physical device.
- Physical-device runs (push notification receipt, native SecureStore, installers) are executed manually or via EAS/Android SDK when available, and are listed as evidence in the release gate — never claimed from web-only tests.

## Consequences

- `npm run verify` extends with new test directories but keeps its current shape.
- The §22.6 gate is met with an honest split: automated web E2E + explicitly documented on-device checks.
