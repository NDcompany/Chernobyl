# ADR-008: Hermes adapter deferred until the runtime is installed

**Status:** Accepted
**Date:** 2026-10-21
**Relates to:** PRD §7.6, §4.1, §25 Phase 7; conflict register C9

## Context

PRD §7.6 requires investigating *the installed Hermes Agent version*. Phase 0 environment inventory (see `docs/architecture/PHASE0_BASELINE.md`): `hermes` is **not installed** on this machine (checked 2026-10-21, `Get-Command hermes` → not found). OpenCode 1.17.7, Claude Code 2.1.301 and Ollama 0.40.6 are installed.

## Decision

- Phase 7 adapter order becomes: **Claude Code → Ollama → Hermes**.
- The Hermes adapter is not started, stubbed or advertised until a real Hermes installation exists to probe; no capability claims are made for it in UI or docs before then.
- If Hermes is never installed, the release documents it as "not supported on this build" — the adapter SDK (Phase 1) must make unsupported adapters invisible rather than broken.

## Consequences

- No reverse-engineering doc is written for Hermes in Phase 0 (nothing to probe); it is created when the adapter is actually taken up.
