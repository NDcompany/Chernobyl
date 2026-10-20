# ADR-010: The local backend runs as one std-only Rust binary (Phase R)

**Status:** Accepted
**Date:** 2026-10-21
**Relates to:** PRD §16.2, §19, §25 (phase map); conflict register C15; ADR-001, ADR-003, ADR-004

## Context

Phases 0-2 delivered a green Node implementation (`services/host` + `services/companion`) with the
protocol-v2 server and the companion foundation. The PRD's end state (§25) is an always-on local
daemon the mobile app connects to directly: it must be cheap to keep resident for days, start fast,
and survive hostile input without crashing (§19). A long-running Node process (node_modules,
JIT warmup, GC pauses) is workable but is the wrong shape for a resident single-binary utility,
and the user directed a Rust rewrite of the companion with **zero external modules**.

## Decision

- **Phase R:** the local backend becomes one standalone Rust binary, `services/companion-rs`,
  containing the protocol-v2 WS server, the Phase-2 companion foundation (config, storage,
  adapters, diagnostics), and the hardening requirements — compiled to a single self-contained
  `.exe`.
- **std-only:** `Cargo.toml` keeps an empty `[dependencies]`. WebSocket framing, SHA-1, base64,
  strict JSON, threading, and Windows FFI (`SetConsoleCtrlHandler`, `GetProcessMemoryInfo`) are
  hand-rolled in-crate. No tokio, no serde, no SQLite.
- **Port, don't reinvent:** behavior is ported test-for-test from the proven TS sources
  (`packages/protocol/src/engine.ts`, `services/host/src/host.ts`, `packages/domain/src/permission.ts`).
  The Node services stay as the reference/conformance implementation until the Rust binary is
  proven in use, then are decommissioned (per ADR-004: workspaces live only while a phase needs them).
- **Storage:** an append-only JSONL journal (atomic tmp+rename compaction, torn-tail truncation,
  in-memory state) replaces PRD §16.2 SQLite — see conflict register **C15** for the rationale and
  the re-evaluation trigger.
- **Gates:** `npm run verify` stays green and unchanged; `npm run verify:rust` is a separate,
  equally strict gate (`cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`,
  `--self-test` end-to-end probe). Nothing may be committed unless both gates pass.
- **Documented deviation:** the Node host accepted any `HOST_TOKEN`-less loopback bind by
  design; the Rust binary refuses any non-loopback bind unless a token is set
  (secure-by-default). Loopback-without-token parity with Node is preserved.

## Consequences

- Measured on this machine: boot RSS ≈ 3.8 MB, release exe ≈ 407 KB, full self-test (7
  end-to-end checks over real sockets) ≈ 1.4 s. Idle memory is dominated by the in-memory
  snapshot + journal index, not a VM.
- In-memory state means restart = journal replay + seed reconciliation; there are no ad-hoc
  SQL queries against state. If cross-process querying or multi-writer access is ever needed,
  C15's re-evaluation trigger applies.
- Future adapters (Phase 7) and pairing/tunnel phases (4'/5') are added as Rust modules in the
  same binary, not as separate services.
- The journal format is now a persisted contract: compaction must keep records newline-terminated
  and the loader must keep tolerating torn tails (covered by `storage.rs` tests).
