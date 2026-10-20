# ADR-004: Create packages when a phase needs them (no scaffolding from the diagram)

**Status:** Accepted
**Date:** 2026-10-21
**Relates to:** PRD §5.2 (proposed repository structure), §2.1-8 (avoid unnecessary artifacts)

## Context

The PRD's proposed tree introduces ~10 packages (`domain`, `adapter-sdk`, `security`, `persistence`, `config`, `telemetry`, plus `infra/`, `tests/` …) and 5 services up front. The PRD also says to *adapt* the structure rather than move files mechanically, and to avoid unnecessary dependencies and generated artifacts. Most of those packages have exactly one consumer at their phase of introduction.

## Decision

Scaffold a new package/service only when its owning phase begins and only with the code that phase actually runs:

- Phase 1 creates `packages/domain` and `packages/adapter-sdk` (each has real consumers and contract tests there).
- `services/companion` is created in Phase 2; `services/relay`'s role is absorbed by the companion gateway (ADR-001); `services/cloud-api` and `services/notification-worker` are never created as standalone services (ADR-001 — notification logic lives in the companion).
- `security`, `persistence`, `config`, `telemetry` are extracted **only if** duplication or independent reuse demonstrably justifies them; until then their code lives inside the service that needs it.
- `tests/{contract,integration,security,e2e}` directories are created when the first test of each kind lands.

## Consequences

- The repo grows one honest workspace per verified step; nothing empty ships.
- The PRD tree remains the target shape for review, not a creation checklist.
- A package that later needs extraction is refactored behind its existing tests (cheap, reversible).
