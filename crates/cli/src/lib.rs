//! `codypendent-cli` library surface.
//!
//! `src/main.rs` is a thin binary wrapper over this library so that
//! integration tests (`tests/jsonl_it.rs`) can drive the connection/streaming
//! logic directly against a hand-rolled mock server, without spawning the
//! `codypendent` binary itself.

pub mod client;
pub mod commands;
pub mod connection;
pub mod stream;
pub mod tui;
