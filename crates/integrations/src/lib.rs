//! Codypendent integrations: GitHub and IDE awareness (Phase 3).
//!
//! This crate connects the runtime to real developer surfaces:
//!
//! - [`github`] — the personal-mode GitHub client: a typed [`github::GitHubApi`]
//!   trait plus a `reqwest` implementation, secret brokering that keeps the
//!   token out of model context / logs / the database, and idempotent writes
//!   keyed by a hidden marker so a retried command finds its prior object.
//! - [`webhook`] — replay-safe webhook ingestion: `X-Hub-Signature-256`
//!   verification *before* parsing, normalization into internal events, and
//!   `X-GitHub-Delivery`-GUID idempotency.
//! - [`ide`] — the IDE bridge contract ([`ide::IdeBridge`]) and source-provenance
//!   resolution that prefers an unsaved editor buffer over the filesystem when
//!   their digests diverge.
//!
//! It depends only on the protocol crate and external crates; the assembly
//! layer (`codypendentd`) wires the GitHub client into the tool layer and the
//! webhook listener into daemon startup.

pub mod acp;
pub mod acp_client;
pub mod github;
pub mod ide;
pub mod webhook;
