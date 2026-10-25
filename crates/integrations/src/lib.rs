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
//! - [`mcp`] — the hand-rolled MCP (Model Context Protocol) *client* for stdio
//!   servers: the `mcp.toml` loader, an ndjson JSON-RPC layer (no new external
//!   dependency), per-server lazy spawn behind the [`mcp::McpBridge`] trait the
//!   runtime consumes. Returned tool content is untrusted evidence, sanitized
//!   in the runtime.
//! - [`search`] — web search (PR C1): the [`search::SearchApi`] trait plus a
//!   bounded `reqwest` Tavily client, with the key brokered opaquely from
//!   `TAVILY_API_KEY` and all returned content sanitized as untrusted evidence
//!   in the runtime.
//! - [`unsloth`] — Hugging Face Hub discovery for the Unsloth GGUF catalog
//!   (local models): the [`unsloth::HfCatalogApi`] trait plus a bounded
//!   `reqwest` client, listing an author's GGUF repos and, per repo, their
//!   quant variants (with sizes) grouped from the file tree. Keyless — the
//!   Hub's `api/models` surface is public.
//!
//! It depends only on the protocol crate and external crates; the assembly
//! layer (`codypendentd`) wires the GitHub client into the tool layer and the
//! webhook listener into daemon startup.

pub mod acp;
pub mod acp_client;
mod acp_google_home;
mod acp_tool_failure;
mod acp_tool_history;
pub use acp_tool_failure::format_tool_failure;
pub mod acp_registry;
pub mod github;
pub mod ide;
pub mod mcp;
pub mod poison;
pub mod search;
pub mod unsloth;
pub mod webhook;
