//! `codypendent-cli` library surface.
//!
//! `src/main.rs` is a thin binary wrapper over this library so that
//! integration tests (`tests/jsonl_it.rs`) can drive the connection/streaming
//! logic directly against a hand-rolled mock server, without spawning the
//! `codypendent` binary itself.

pub mod acp;
pub mod acp_clients;
pub mod client;
pub mod commands;
pub mod connection;
pub mod council;
pub mod doctor;
mod editor_handoff;
pub mod eval;
pub mod finetune;
pub mod local_endpoints;
pub mod models_file;
pub mod models_pull;
/// The one place this crate answers "which repository is this?" (2026-08-13
/// review §1.1): a repository's durable identity is its checkout, on both the
/// path axis and the id axis.
pub mod repo_anchor;
pub mod restart;
/// The skill-writer (outcome 4's authoring half, 2026-08-13 review F4.2):
/// authors a validated `skill.toml` + `SKILL.md` package into the governed
/// registry, landing as `draft`, through the same install pipeline
/// `codypendent skill add` uses.
pub mod skill_writer;
pub mod stream;
pub mod theme_select;
pub mod tui;
pub mod update;
/// The TUI's voice host (voice v1, rubric 8): push-to-talk capture and spoken
/// replies, kept out of `codypendent-tui` so that crate stays free of
/// subprocesses, audio, and network.
pub mod voice;
/// The writer for `automation_endpoints` (`codypendent webhook endpoint …`).
/// Until it existed the table had one SELECT — the ingest-path resolver — and no
/// INSERT anywhere, so every per-endpoint webhook control governed nothing.
pub mod webhook_endpoints;
