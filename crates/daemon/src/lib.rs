//! `codypendentd` library: persistence, ledger, replay, and the client
//! protocol server. The `codypendentd` binary — in the sibling
//! `crates/codypendentd` assembly crate — wires these together and injects a
//! [`RunExecutor`](crate::executor::RunExecutor) over the runtime agent loop.

// Phase 0
pub mod db;
pub mod instance;
pub mod ledger;
pub mod replay;
pub mod server;

// Phase 1
pub mod approvals;
pub mod artifacts;
pub mod blackboard;
pub mod commands;
pub mod documents;
pub mod executor;
pub mod policy;
pub mod projections;
pub mod promotion;
pub mod recovery;
pub mod subscriptions;
pub mod workflows;
pub mod worktrees;
