//! Workflow-start seam (dependency inversion), Phase 5 STEP 5.2.
//!
//! A `StartWorkflow` command creates a durable workflow run that lives in its own
//! store *outside* the session ledger — so, like `MutateDocument`, it is
//! intercepted at the connection level and applied through a seam the daemon
//! declares and the `codypendentd` assembly fills (only the assembly can name
//! `codypendent-workflow` and reach the pool). The default-`None`
//! [`RunExecutor::workflow_starter`](crate::executor::RunExecutor::workflow_starter)
//! leaves it unwired — the lib-only / test server then rejects `StartWorkflow`
//! with `workflow.transport-unavailable`, exactly as an executor-less run stays
//! `Queued`.

use std::future::Future;
use std::pin::Pin;

use codypendent_protocol::{ClientId, CodypendentError};
use serde_json::Value;

/// A client's request to start a durable workflow run from a manifest.
#[derive(Debug, Clone)]
pub struct StartWorkflowRequest {
    /// The workflow manifest YAML (its content, never a path — the daemon does not
    /// read an arbitrary client-named file).
    pub manifest: String,
    /// The typed inputs the manifest declares (opaque JSON to the daemon; the
    /// store records them with the run).
    pub inputs: Value,
    /// The command's idempotency key: a duplicate `StartWorkflow` delivery (a
    /// client retrying after a lost acknowledgement) carries the same key, so the
    /// seam creates the run idempotently — the same key resolves to the same run
    /// rather than a second one.
    pub idempotency_key: String,
    /// The identity of the starting client, for attribution.
    pub client_id: ClientId,
}

/// The future a [`WorkflowStarter`] returns: the new durable workflow-run id to
/// reply with, or a structured [`CodypendentError`] the server rejects with. Boxed
/// so the trait stays object-safe without an `async-trait` dependency (matching
/// the [`DocumentMutator`](crate::documents::DocumentMutator) seam).
pub type WorkflowStartFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, CodypendentError>> + Send + 'a>>;

/// The daemon's seam for *creating* a durable run from an accepted `StartWorkflow`.
///
/// Implemented by the assembly over `codypendent-workflow` — compile the manifest,
/// then `WorkflowStore::create_run` on the daemon's pool — and injected alongside
/// the [`RunExecutor`](crate::executor::RunExecutor). The assembly also **drives**
/// the created run (fire-and-forget) so it advances to a terminal state; this seam
/// returns as soon as the run is durably created.
pub trait WorkflowStarter: Send + Sync {
    /// Compile `request`'s manifest and create a durable run, returning its id. A
    /// manifest that does not compile (or a store failure) is surfaced verbatim to
    /// the client as a `CommandRejected`; nothing is created.
    fn start(&self, request: StartWorkflowRequest) -> WorkflowStartFuture<'_>;
}

/// A client's request to pause a durable workflow run.
#[derive(Debug, Clone)]
pub struct PauseWorkflowRequest {
    /// The durable workflow-run id (e.g. `wfrun-…`).
    pub workflow_run_id: String,
    /// The requesting client, for attribution.
    pub client_id: ClientId,
}

/// A client's request to resume a paused durable workflow run.
#[derive(Debug, Clone)]
pub struct ResumeWorkflowRequest {
    pub workflow_run_id: String,
    pub client_id: ClientId,
}

/// A client's request to re-drive a durable workflow run from a chosen node.
#[derive(Debug, Clone)]
pub struct RetryWorkflowNodeRequest {
    pub workflow_run_id: String,
    /// The node id to re-drive from (its transitive dependents reset with it).
    pub node_id: String,
    pub client_id: ClientId,
}

/// The future a [`WorkflowLifecycle`] method returns: the synchronous outcome of
/// the lifecycle mutation (the actual driving continues in the background), or a
/// structured [`CodypendentError`] the server rejects with. Boxed so the trait
/// stays object-safe, matching [`WorkflowStartFuture`].
pub type WorkflowLifecycleFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), CodypendentError>> + Send + 'a>>;

/// The daemon's seam for *controlling* an existing durable run — pause, resume,
/// retry-from-node (Phase 5 STEP 5.2 lifecycle commands).
///
/// Implemented by the assembly over the `codypendent-workflow` conductor and
/// injected alongside the [`RunExecutor`](crate::executor::RunExecutor). Each
/// method performs its synchronous state change (validate + mutate) and — for
/// resume/retry — spawns the drive in the background, so it returns as soon as the
/// command is accepted or rejected. A run in a state that forbids the transition
/// (a terminal run paused, a non-paused run resumed) is surfaced verbatim as a
/// `CommandRejected`; nothing changes.
pub trait WorkflowLifecycle: Send + Sync {
    /// Pause a pending/running run (idempotent on an already-paused run; an error
    /// on a terminal run). A live driver stops cooperatively.
    fn pause(&self, request: PauseWorkflowRequest) -> WorkflowLifecycleFuture<'_>;
    /// Resume a paused run: validate it is paused, then drive it onward in the
    /// background. An error when the run is not paused.
    fn resume(&self, request: ResumeWorkflowRequest) -> WorkflowLifecycleFuture<'_>;
    /// Reset a run for a retry from `node_id` (that node + its transitive
    /// dependents), then drive it onward in the background. An error on an unknown
    /// node or a changed graph.
    fn retry_node(&self, request: RetryWorkflowNodeRequest) -> WorkflowLifecycleFuture<'_>;
}
