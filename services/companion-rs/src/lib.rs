//! CHERNOBYL companion — Rust local backend. Zero external crates.
//!
//! Modules: json (strict parser/serializer), protocol (v2 wire contract +
//! permission authority), seed (mock snapshot), engine (mock agent engine),
//! ws (RFC 6455 codec), sha1/base64 (handshake primitives), storage
//! (crash-safe journal), backup (Phase 8 journal backup/restore), config,
//! adapters, opencode (Phase 3 OpenCode adapter), ollama (Phase 7
//! model-runtime adapter), claude (Phase 7 hook ingress), companion
//! (orchestration), server (WS/HTTP), client (test client), tui (Phase 8
//! terminal UI), ffi (Windows console/memory).
//! runtime_io: ADR-015 child process IO, env scrub, redaction, desktop
//! opener and OAuth callback check. transcript: ADR-015 transcript hub
//! (memory-only live transcripts, subscriptions, backpressure).
//! codex: ADR-015 step 7 Codex app-server adapter core (JSON-RPC client,
//! notification decoder, approvals); codex_runtime: its LiveRuntime (one
//! app-server child per adapter, sessions as threads).
//! claude_live: ADR-015 Claude Code driver core (frame decoder, permission
//! options, stdin encoders, launch and workspace policy). claude_runtime:
//! ADR-015 Claude Code live runtime (driven processes behind LiveRuntime).
//! claude_history: Claude Code sessions on disk (discovery and replay).
//! workspace: ADR-016 read-only files and git for allow-listed workspaces.
//! terminal: ADR-020 opt-in one-shot command runner (off by default).

pub mod adapters;
pub mod backup;
pub mod base64;
pub mod claude;
pub mod claude_history;
pub mod claude_live;
pub mod claude_runtime;
pub mod client;
pub mod codex;
pub mod codex_runtime;
pub mod companion;
pub mod config;
pub mod crypto;
pub mod e2e;
pub mod engine;
pub mod ffi;
pub mod json;
pub mod ollama;
pub mod opencode;
pub mod pairing;
pub mod protocol;
pub mod qr;
pub mod runtime_io;
pub mod seed;
pub mod server;
pub mod sha1;
pub mod storage;
pub mod terminal;
pub mod transcript;
pub mod tui;
pub mod workspace;
pub mod ws;
