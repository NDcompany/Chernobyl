//! CHERNOBYL companion — Rust local backend. Zero external crates.
//!
//! Modules: json (strict parser/serializer), protocol (v2 wire contract +
//! permission authority), seed (mock snapshot), engine (mock agent engine),
//! ws (RFC 6455 codec), sha1/base64 (handshake primitives), storage
//! (crash-safe journal), config, adapters, opencode (Phase 3 OpenCode
//! adapter), ollama (Phase 7 model-runtime adapter), claude (Phase 7 hook
//! ingress), companion (orchestration), server (WS/HTTP), client (test
//! client), ffi (Windows console/memory).

pub mod adapters;
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
pub mod seed;
pub mod server;
pub mod sha1;
pub mod storage;
pub mod ws;
