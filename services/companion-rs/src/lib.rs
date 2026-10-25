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
//! opener and OAuth callback check.

pub mod adapters;
pub mod backup;
pub mod base64;
pub mod claude;
pub mod client;
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
pub mod tui;
pub mod ws;
