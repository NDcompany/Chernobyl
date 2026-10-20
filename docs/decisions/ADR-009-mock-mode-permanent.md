# ADR-009: Mock mode is permanent; simulation never mixes with real streams

**Status:** Accepted
**Date:** 2026-10-21
**Relates to:** PRD §2.1-9, §13.1, §23.3; conflict register C3

## Context

The prototype's `MockEngine` powers the whole UI offline and is required for development and automated tests (PRD preservation rule). Once real companion connections exist, the same screens would display mock data indistinguishable from real agent activity — the PRD forbids presenting simulated permission requests or agent work as real (§13.1, Phase 9: "no longer advertises unimplemented mock features as real").

## Decision

- Mock mode remains a first-class, supported **development/test mode** — never deleted, always the default in development (PRD §23.3).
- In any build, exactly one mode is active per session: **demo (mock)** or **real (companion)**. The two streams are never merged.
- In demo mode the UI must carry a persistent, visible **"Simulation"** indication wherever state or approvals are shown; nothing rendered from `MockEngine` may claim to be a real runtime decision.
- The mode is explicit in settings (and by transport selection); real mode requires a paired companion connection.

## Consequences

- Phase 6 (mobile integration) adds the simulation badge and mode switch as required work, gated by this ADR.
- Contract tests (Phase 1) run against both the mock adapter and real adapters through the same schema layer, keeping mock fidelity honest.
