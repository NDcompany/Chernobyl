# CHERNOBYL — Phase 0 baseline (repository audit)

**Date:** 2026-10-21 · **Agent:** OpenCode (MiMo) · **PRD phase:** 0 — repository audit and reverse engineering (exit-gate record)

## 1. Environment inventory (verified on this machine)

| Tool | Version | Present |
|---|---|---|
| Node.js | 24.20.0 | ✅ |
| npm | (bundled with Node 24) | ✅ |
| git | 2.55.0 | ✅ |
| OpenCode (npm CLI) | 1.17.7 | ✅ |
| OpenCode desktop (bundled CLI) | 2.0.24 | ✅ — shares data dir with 1.17.7, see reverse-engineering doc §9 |
| Claude Code | 2.1.301 | ✅ |
| Ollama | 0.40.6 | ✅ |
| Hermes | — | ❌ not installed (ADR-008) |
| Docker | — | ❌ not installed |
| PostgreSQL (`psql`) | — | ❌ not installed (ADR-003) |
| Android SDK | — | ❌ not installed; build check = `expo export` |
| Code-signing cert | — | ❌ none (ADR-007) |

## 2. Workspace inventory (pre-Phase-1)

```
G:\Chernobyl
├── apps\mobile          @chernobyl/mobile   Expo/React Native, Expo Router (13 route screens), TanStack Query,
│                                            Zustand, React Hook Form + Zod, SQLite, SecureStore; mock mode default
├── services\host        @chernobyl/host     Node WS service: token auth (4001), protocol-version gate (4002),
│                                            heartbeat, 256KB maxPayload, command dedupe, HTTP health;
│                                            createHost() factory + server.ts env entry (PORT, HOST_TOKEN)
├── packages\protocol    @chernobyl/protocol Shared Zod contracts, PROTOCOL_VERSION=1, seed data, MockEngine
└── docs\architecture, docs\decisions, docs\reverse-engineering   (created in Phase 0)
```

Git: repo initialized at `G:\Chernobyl`; prototype committed as `b81c4aa` (96 files); Phase 0 docs committed alongside this record.

## 3. Verification baseline — `npm run verify` (2026-10-21, pre-Phase-1)

| Step | Result |
|---|---|
| `typecheck` (protocol build + host + mobile) | ✅ pass (3 workspaces) |
| `lint` (mobile, eslint) | ✅ 0 errors, 2 known `react-hooks/incompatible-library` warnings (`connect-host.tsx:43`, `new-task.tsx:55` — React Compiler vs RHF `watch()`; documented, not regressions) |
| `test` (host, `node:test` over built dist) | ✅ **14/14 pass** (502 ms) |
| `build` (protocol + host) | ✅ pass |
| `build:mobile` (`expo export --platform android`) | ✅ 1443 modules, Hermes bundle 3.5 MB, 26 assets → `dist-android` |

**Test inventory (14):** engine — duplicate command ids; approval resolves + resumes; stale permission rejected; rejection interrupts; malformed frames rejected; fresh session starts plan; interrupt expires waiting ask. Host — hello snapshot; HTTP health counters; malformed frame error ack; protocol mismatch 4002; token gate 4001 (query + header); commands broadcast to other clients; replayed commandId never runs twice.

**Baseline verdict: green.** This is the number every subsequent phase must keep green (PRD §22.1).

## 4. Phase 0 commands executed

- `npm run verify` — full baseline (table above).
- Environment probes: `opencode --version`, `Get-Command`/process/port checks for installed runtimes.
- OpenCode server probes against a controlled `opencode serve` instance (ports 4096/4097/4099, temp cwd, never the repo workspace): health, `/doc` OpenAPI capture, session/project/agent listing, session create, SSE capture (`/global/event`), `prompt_async` (with and without `noReply`), `abort` (idle + running), `/diff`, `/permission`, `DELETE /session/:id`.
- Read-only SQLite inspection of the shared opencode data dir via Node 24 `node:sqlite` (version-skew root cause).
- Full evidence + findings: `docs/reverse-engineering/opencode.md`.

## 5. Probe hygiene (recorded)

- Probe sessions were created only in throwaway titles and **all deleted** afterward (`DELETE /session/:id`; shared DB session count restored 15→15).
- All probe servers stopped; temp dirs (including the copied `auth.json`) removed. The only surviving artifacts are the evidence files under `docs/reverse-engineering/evidence/`.
- No changes to application code, tests, dependencies or the existing UI. `docs/` additions only.

## 6. Phase 0 exit criteria check (PRD §25)

| Criterion | Status |
|---|---|
| Current message flow and persistence model explainable | ✅ (mock transport → reducer → screens; host WS + MockEngine; documented in PRD import + conflict register) |
| All current tests pass | ✅ 14/14 |
| No existing mobile screens removed | ✅ (nothing touched) |
| Adapter feasibility documented with evidence | ✅ `docs/reverse-engineering/opencode.md` (2 honest ⚠️ items deferred to Phase 3, both release-gate checks not design unknowns) |
| Architecture decision records | ✅ ADR-001…009 + 14-item conflict register |
