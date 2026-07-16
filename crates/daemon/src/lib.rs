//! `codypendentd` library: persistence, ledger, replay, and the client
//! protocol server. The binary in `src/main.rs` wires these together.

// Phase 0
pub mod db;
pub mod instance;
pub mod ledger;
pub mod replay;
pub mod server;

// Phase 1
pub mod approvals;
pub mod artifacts;
pub mod commands;
pub mod policy;
pub mod projections;
pub mod recovery;
pub mod subscriptions;
pub mod worktrees;
