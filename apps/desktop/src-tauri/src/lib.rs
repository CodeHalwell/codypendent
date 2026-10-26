//! Codypendent desktop shell.
//!
//! The Rust side owns the `codypendentd` connection (adoption 14 §4.1); the
//! webview is a projection of daemon state and holds no privileged path of its
//! own. Split in two so the part with logic is testable without a window:
//!
//! - [`daemon`] — the socket connection, built on the shared protocol crate
//!   and the same `Connection` the CLI and TUI use. No `tauri` types.
//! - [`bridge`] — the Tauri commands and event channel that expose it.
//! - [`council`] — councils, which are LOCAL CONFIGURATION (`councils.toml`),
//!   not protocol: the daemon has no council command.
//! - [`repository`] — which checkout the client works in, gated so a folder
//!   that is not a repository (or is `$HOME`) can never reach the indexer.

#[cfg(feature = "app")]
mod artifacts;
#[cfg(feature = "app")]
pub mod bridge;
pub mod council;
pub mod daemon;
#[cfg(feature = "app")]
pub mod desktop_tools;
#[cfg(feature = "app")]
pub mod environment;
#[cfg(feature = "app")]
mod native_menu;
#[cfg(feature = "app")]
pub mod python_sandbox;
#[cfg(feature = "app")]
mod sandbox;
// Starting `codypendentd` when nothing is listening: the CLI's `ensure_daemon`
// for a shell whose own binary is not `codypendent`.
pub mod knowledge;
pub mod launcher;
pub mod repository;
// Which checkout the app is looking at. Board and knowledge scopes are keyed by
// the checkout root, never the launch directory (see the module header).
pub mod repo_anchor;
// The LOCAL CONFIG surfaces: `models.toml`, `providers.toml` and `auth.json`
// under the data dir. Not protocol — there is no wire command for any of them,
// so the shell reads and writes the files the CLI and TUI read and write.
pub mod models;

pub use daemon::{ConnectionInfo, DaemonClient, DaemonFrame, FrameSink, RunHandle, SessionRow};
pub use repository::RepositorySelection;
