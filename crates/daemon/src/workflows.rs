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

use codypendent_protocol::{ClientId, CodypendentError, WorkflowCatalog};
use serde_json::Value;

/// A ceiling imposed on a workflow run from **outside** its manifest, carried
/// across the start seam.
///
/// Only the two dimensions the workflow budget envelope actually enforces are
/// representable here. `WorkflowBudget` (the manifest's `budget:` block, the one
/// thing `BudgetLimits::resolve` reads for the workflow scope) has a wall-clock
/// ceiling and a cost ceiling and nothing else: there is no workflow-scope
/// tool-call ceiling and no token ceiling anywhere in the budget machinery, and
/// tokens are a *recorded* dimension that is never charged. So a caller holding a
/// ceiling this type cannot express — an `automation_bindings.budget_tool_calls`
/// or `budget_tokens` — is forced to decide what to do about it (refuse the
/// firing) rather than handing it over and having it silently dropped. The type
/// is deliberately unable to carry an unenforceable ceiling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkflowRunCeiling {
    /// Ceiling on the run's total measured wall-clock seconds, tightening the
    /// manifest's `budget.maximum_duration_seconds`.
    pub max_wall_time_seconds: Option<u64>,
    /// Ceiling on the run's total MEASURED model spend in micro-USD, tightening
    /// the manifest's `budget.maximum_cost_usd`. Charged only against spend a run
    /// actually reported — an unmeasured run is never charged against it — which
    /// is the pre-existing honesty rule of the budget module, not a weakening of
    /// this ceiling.
    pub max_cost_micros: Option<u64>,
}

impl WorkflowRunCeiling {
    /// Whether this ceiling constrains anything. An all-`None` ceiling is
    /// indistinguishable from no ceiling and leaves the manifest untouched.
    #[must_use]
    pub fn declares_any(&self) -> bool {
        self.max_wall_time_seconds.is_some() || self.max_cost_micros.is_some()
    }
}

/// A client's request to start a durable workflow run from a manifest.
#[derive(Debug, Clone)]
pub struct StartWorkflowRequest {
    /// Optional selected-definition precondition, checked against the resolved bytes before creation.
    pub expected_definition_digest: Option<String>,
    /// The workflow manifest YAML (its content, never a path — the daemon does not
    /// read an arbitrary client-named file). Empty when [`workflow_id`](Self::workflow_id)
    /// names a workflow the assembly resolves from its own sources instead.
    pub manifest: String,
    /// A named workflow to resolve from the assembly's sources (embedded
    /// built-ins + the user config directory + the run repository's
    /// `.codypendent/workflows`) rather than compiling an inline `manifest` — the
    /// `/fix-ci` path. When `Some`, the [`WorkflowStarter`] resolves it (enforcing
    /// the registry's version-stability + shadowing rules) and ignores `manifest`.
    pub workflow_id: Option<String>,
    /// The typed inputs the manifest declares (opaque JSON to the daemon; the
    /// store records them with the run).
    pub inputs: Value,
    /// The command's idempotency key: a duplicate `StartWorkflow` delivery (a
    /// client retrying after a lost acknowledgement) carries the same key, so the
    /// seam creates the run idempotently — the same key resolves to the same run
    /// rather than a second one.
    pub idempotency_key: String,
    /// The canonical repository root the run's agent nodes operate on (Phase 5
    /// T5). Persisted with the durable run so a per-node isolated worktree is
    /// carved from the right checkout — and so recovery drives it there after a
    /// restart. `None` (an older client that sends none) leaves the node executor
    /// to fall back to the daemon's startup repository root.
    pub repository: Option<String>,
    /// The server-derived operating-system principal that owns the run. This is
    /// never accepted from the wire: the daemon stamps it before crossing this
    /// seam so ownership is committed atomically with the durable run.
    pub owner_uid: u32,
    /// The identity of the starting client, for attribution.
    pub client_id: ClientId,
    /// A ceiling imposed on this run from outside its manifest — today an
    /// automation binding's `budget_*` columns, which are the binding row's (not
    /// the payload's) authority over what a firing may spend. The seam
    /// implementation **tightens the run's stored manifest** with it, never
    /// loosens it: the lower of (declared, imposed) wins per dimension, and the
    /// tightened manifest is what is persisted, so the ceiling survives a restart
    /// (node execution re-reads the envelope by recompiling the stored manifest).
    /// `None` imposes nothing and the manifest's own envelope applies unchanged.
    pub budget_ceiling: Option<WorkflowRunCeiling>,
}

/// The future a [`WorkflowStarter`] returns: the new durable workflow-run id to
/// reply with, or a structured [`CodypendentError`] the server rejects with. Boxed
/// so the trait stays object-safe without an `async-trait` dependency (matching
/// the [`DocumentMutator`](crate::documents::DocumentMutator) seam).
pub type WorkflowStartFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, CodypendentError>> + Send + 'a>>;

/// Discovery uses the same daemon-owned source directories and authenticated caller as execution.
#[derive(Debug, Clone)]
pub struct WorkflowCatalogRequest {
    pub repository: Option<String>,
    pub owner_uid: u32,
    pub client_id: ClientId,
}

pub type WorkflowCatalogFuture<'a> =
    Pin<Box<dyn Future<Output = Result<WorkflowCatalog, CodypendentError>> + Send + 'a>>;

/// The daemon's seam for *creating* a durable run from an accepted `StartWorkflow`.
///
/// Implemented by the assembly over `codypendent-workflow` — compile the manifest,
/// then `WorkflowStore::create_run` on the daemon's pool — and injected alongside
/// the [`RunExecutor`](crate::executor::RunExecutor). The assembly also **drives**
/// the created run (fire-and-forget) so it advances to a terminal state; this seam
/// returns as soon as the run is durably created.
pub trait WorkflowStarter: Send + Sync {
    /// Optional discovery seam. Embeddings that only implement start remain explicitly unavailable.
    fn catalog(&self, _request: WorkflowCatalogRequest) -> WorkflowCatalogFuture<'_> {
        Box::pin(async {
            Err(CodypendentError::new(
                "workflow.catalog-unavailable",
                "workflow discovery is not enabled on this daemon",
                false,
            ))
        })
    }

    /// Compile `request`'s manifest and create a durable run, returning its id. A
    /// manifest that does not compile (or a store failure) is surfaced verbatim to
    /// the client as a `CommandRejected`; nothing is created.
    ///
    /// A [`budget_ceiling`](StartWorkflowRequest::budget_ceiling) is applied to
    /// the manifest **before** it is compiled and stored, so the run that is
    /// created already carries the tightened envelope and a caller that supplied
    /// a ceiling can rely on it being enforced rather than dropped.
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

/// A client's request to cancel a durable workflow run (T9).
#[derive(Debug, Clone)]
pub struct CancelWorkflowRequest {
    pub workflow_run_id: String,
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
    /// Cancel a run (T9): a cooperative drain (a live driver stops launching further
    /// nodes), every still-`Pending` node becomes `Skipped`, any in-flight node's
    /// agent run is interrupted through the same cancellation machinery `CancelRun`
    /// uses, and the run lands `Cancelled` (terminal — no resume). Idempotent on an
    /// already-cancelled run; an error (`workflow.illegal-transition`) on a
    /// completed/failed run.
    fn cancel(&self, request: CancelWorkflowRequest) -> WorkflowLifecycleFuture<'_>;
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    struct StartOnly;
    impl WorkflowStarter for StartOnly {
        fn start(&self, _request: StartWorkflowRequest) -> WorkflowStartFuture<'_> {
            Box::pin(async { Ok("unused".to_owned()) })
        }
    }

    #[tokio::test]
    async fn catalog_defaults_to_explicit_unavailable_without_execution() {
        let error = StartOnly
            .catalog(WorkflowCatalogRequest {
                repository: None,
                owner_uid: 42,
                client_id: ClientId::new(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, "workflow.catalog-unavailable");
    }
}
