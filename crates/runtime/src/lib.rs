//! codypendent-runtime.
//!
//! Agent runs, the tool layer, the approvals bridge, model integration,
//! context, and compaction. This is the only crate that depends on the
//! `agent-framework-rs` provider crates, and it does so behind provider
//! features (ADR-009: selected crates, never the umbrella `full`).

pub mod models;
