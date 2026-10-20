# Phase 1 gate requirements (binding design constraints from Phase 0 review)

**Status:** Binding — set by the product owner at the Phase 0 review gate (2026-10-21); must be satisfied by Phase 1 (protocol v2, domain FSMs, adapter-sdk + MockAdapter) before its own gate.
**Relation to PRD:** these harden PRD §12.2/§12.4, §13 and §27 into non-negotiable Phase 1 acceptance criteria. Where they conflict with implementation convenience, they win.

---

## G1 — Command acceptance ≠ execution ≠ outcome

**Requirement.** Protocol v2 must distinguish three phases for every mutating command, and no client may render a later phase from an earlier one:

1. **Acceptance** — transport-level ack only ("persisted and routed", incl. duplicate suppression). Never displayed or stored as success.
2. **Execution state** — runtime-reported progress (`accepted` → `running` …), correlated to the command.
3. **Terminal outcome** — exactly one of `completed | failed | cancelled | expired | unknown`, delivered as a command-scoped event. `unknown` is mandatory when safe reconciliation is impossible (PRD §12.4); it must never be coerced into `completed` or `failed`.

**Current state (v1).** `ack {ok: boolean}` is the only command reply (`packages/protocol/src/index.ts:214-223`); terminal state exists only implicitly inside full-entity `session_updated`/`session_output` messages with no command correlation. The mobile client treats `ack.ok` as done (`apps/mobile/src/data/commands.ts:59-65`).

**Why (evidence).** OpenCode probe: `prompt_async` returned **204 while the turn later failed only via SSE `session.error`** (FK skew + provider HTTP 426) — `docs/reverse-engineering/opencode.md` §8-9. An ack-shaped signal carried no outcome information.

**Phase 1 deliverable.** Domain command state machine with validated transitions (illegal transition = error); v2 wire messages for acceptance/progress/terminal events with `commandId` correlation; MockEngine emits the full three-phase lifecycle so the migrated reducer consumes it honestly.

**Verification.** Domain FSM unit tests (legal + illegal transitions); MockEngine test proving a command at `accepted` is *not* rendered done until its terminal event; reducer test asserting UI distinguishes acked vs completed; existing 14 tests keep passing (migrated).

**Out of scope here.** Timeout durations, retry policy, persistence (Phase 2); real adapter outcomes (Phase 3).

## G2 — Permissions stay native and fail-closed

**Requirement.** Every permission record preserves, verbatim from the native runtime: **native request ID**, **supported decisions** (as reported — never assumed), and **revision/fingerprint**. Rules:

- A decision not in the record's `supportedDecisions` → explicit error. **Never silently translated** (e.g. never map `approve-session` onto a runtime that only offers `once|always|reject` without an explicit user-visible mapping table).
- Stale revision / expired / already-resolved / unknown native ID → reject + refresh UI. **Never synthesize an approval**, never re-drive a resolved native request.
- Fail-closed default: any doubt (missing metadata, unsupported shape, adapter uncertainty) → do not decide; surface as "requires local intervention" (PRD §13.5).

**Current state (v1).** `PermissionRequest` has a fixed internal `id`, hard-coded triad `approve-once|approve-session|reject` and a 2-minute expiry, with no native request ID, no supported-decisions list and no revision (`packages/protocol/src/index.ts:133-153`).

**Why (evidence).** PRD §13.1-13.5; OpenCode contract shows native shape `permission.asked {id:"per_…", patterns[], tool{messageID,callID}}` with reply `{once|always|reject}` and 404 on stale (`opencode.md` §7). Runtimes differ; a fixed triad would force translation.

**Phase 1 deliverable.** Domain permission state machine (`requested → decided|expired|superseded`, with revision checks); v2 approval types carrying `nativeRequestId`, `supportedDecisions`, `revision`, `fingerprint`; MockAdapter reports decisions honestly (only what its simulated runtime supports).

**Verification.** Tests: unsupported decision rejected; stale/unknown revision rejected (existing engine test migrated); fail-closed default produces "local intervention" state, not a decision; no code path can emit `approved` without a native-correlated request.

## G3 — Protocol v2 preserves the existing app (this phase only)

**Requirement.** Phase 1 ships protocol v2 **inside the existing product**: MockEngine and the mobile reducer migrate through the existing version gate in the same change. **No real agent streams, no new transports, no user-facing behavior changes**, mock mode stays default. Old-version clients keep receiving the actionable 4002 rejection (existing behavior preserved).

**Current state.** `PROTOCOL_VERSION = 1`, host rejects mismatched `hello` with 4002 (14 tests green); whole app driven by MockEngine snapshots + entity messages.

**Why (evidence).** PRD §2.1 (preserve UI/mock), §27 ("evolve schemas with explicit versioning and migration compatibility"), conflict register C2, ADR-009.

**Phase 1 deliverable.** v2 schemas in `packages/protocol` (v1 exports kept or migrated atomically per workspace needs), MockEngine on v2, reducer/commands on v2, tests migrated with identical assertions.

**Verification.** `npm run verify` green; the 14 baseline tests pass with same assertions; web build click-through renders identical screens/states; no dependency added.

## G4 — Adapter safety contract (from the OpenCode DB incident)

**Requirement.** The adapter-sdk contract (defined in Phase 1, enforced on every real adapter from Phase 3) binds adapters to three safety rules:

1. **Async failure channel.** Runtime errors delivered after acceptance (e.g. `session.error` events) must be surfaced as command/session failures; an ack/HTTP-accept is never the outcome (ties to G1).
2. **Response validation.** Validate `content-type`/shape of every runtime response — unknown routes may return **HTML with HTTP 200** (proven: `/session/:id/status` on 1.17.7); status codes alone prove nothing.
3. **No second server on a shared store.** An adapter may not launch a runtime instance against a data directory it does not exclusively own without a **proven isolation strategy** (for OpenCode 1.17.7: `XDG_DATA_HOME` isolation is verified; `OPENCODE_DATA_DIR` is not honored); prefer attaching to an already-running instance; version-skew signatures in async errors are configuration faults to report, not user errors.

**Why (evidence).** `opencode.md` §8-9: FK-skew failures hidden behind 204s; provider 426 only on the event stream; HTML-200 fallback; two opencode generations sharing one `opencode.db`.

**Phase 1 deliverable.** Safety-contract requirements encoded in adapter-sdk types/docs plus conformance tests; **MockAdapter conforms now**, so the suite is executable in Phase 1 (simulated late async error after ack; simulated HTML/garbage response; simulated existing-instance detection).

**Verification.** Conformance suite green in Phase 1; Phase 3 exit gate reuses the same suite for real adapters.

---

## Gate checklist (Phase 1 exit, additive to PRD §25 Phase 1)

- [x] G1: three-phase command lifecycle in protocol + domain + MockEngine + reducer; FSM tests green.
      (`command_progress accepted|running` + exactly one `command_result` per command, `commandId`-correlated; domain FSM + tracker; appStore `trackCommand` via domain tracker; host wire tests + engine tests green)
- [x] G2: native-ID/supportedDecisions/revision on approvals; fail-closed + no-synthesis tests green.
      (`PermissionRequest.nativeRequestId/supportedDecisions/revision/fingerprint`; `permission_decision` requires revision; engine delegates to domain `evaluatePermissionDecision`; resolution/expiry bumps revision; conformance + engine + wire tests green)
- [x] G3: v2 through existing version gate; 14 baseline tests pass unchanged in assertion; no UI/behavior change; verify green.
      (`PROTOCOL_VERSION=2`, 4002 behavior preserved; 14 baseline tests migrated with identical assertions; reducer/messages unchanged in behavior — envelope + lifecycle fields are additive; `npm run verify` green end-to-end)
- [x] G4: adapter-sdk safety contract + MockAdapter conformance suite green.
      (`packages/adapter-sdk`: Adapter contract + `validateJsonResponse` + MockAdapter fault knobs; `runAdapterConformance` + `runSafetyConformance` 9/9 green)

**Phase 1 result:** 43/43 tests (13 domain + 21 host + 9 adapter-sdk), verify green.

**Carried forward to Phase 2 (not gaps in this gate):** envelope `seq` persistence for replay/resync (C2 remainder) — ~~needed the companion's durable storage~~ **landed in Phase 2** (`event_cursors` + transactional append, see `PHASE2-COMPANION-FOUNDATION.md`); command lifecycle surfacing in UI (state layer tracks it now; screens render it in the production-integration phase).
