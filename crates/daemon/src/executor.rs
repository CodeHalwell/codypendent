//! Run-execution seam (dependency inversion).
//!
//! The daemon crate owns the server + persistence; the agent loop lives in
//! `codypendent-runtime`, which **depends on** this crate. So the daemon cannot
//! depend on the runtime (that would be a cycle). This module is the inversion
//! point: the daemon defines *what* it needs — a [`RunExecutor`] that starts a
//! run — and the assembly binary (`crates/codypendentd`, which depends on both
//! the daemon and the runtime) provides the concrete implementation and injects
//! it into the [`server`](crate::server).
//!
//! When the command write path accepts a `StartRun`, the server hands the
//! executor a [`RunLaunch`] and returns immediately. [`RunExecutor::spawn_run`]
//! is therefore **synchronous and fire-and-forget**: the implementation
//! `tokio::spawn`s its own async work and the daemon never awaits it. This keeps
//! the daemon free of an `async-trait` dependency and keeps a slow (or wedged)
//! run from ever blocking the command-reply path.

use std::path::PathBuf;

use codypendent_protocol::{AgentMode, RunId, SessionId};

/// Everything the executor needs to start one run. Built by the server from the
/// accepted `StartRun` command body (session/objective/mode) plus the run id the
/// write path minted (`CommandOutcome::created_run`).
///
/// Phase 1 has no per-run repository *path* on the wire (the `StartRun` command
/// carries none — see `crates/daemon/src/commands.rs` and the CLI's `run`), so
/// the server fills `repository` from its own working directory. Binding a real
/// repository to a run is a later step (worktree allocation, STEP 1.8).
pub struct RunLaunch {
    /// The session whose ledger the run appends to.
    pub session_id: SessionId,
    /// The run to execute (its `runs` row + `RunStarted` event already exist).
    pub run_id: RunId,
    /// The run objective, seeded as the first transcript item.
    pub objective: String,
    /// The mode preset, mapped to a policy overlay by the runtime.
    pub mode: AgentMode,
    /// The repository root the run operates against.
    pub repository: PathBuf,
}

/// The daemon's seam for actually *executing* an accepted run.
///
/// Implemented by the assembly binary over the runtime agent loop; injected into
/// the server via [`server::run_with_executor`](crate::server::run_with_executor).
/// [`spawn_run`](RunExecutor::spawn_run) must not block — it starts background
/// work and returns.
pub trait RunExecutor: Send + Sync {
    /// Start executing `launch` in the background. Fire-and-forget: returns
    /// immediately; the implementation owns the run to a terminal state.
    fn spawn_run(&self, launch: RunLaunch);

    /// Request cancellation of an in-flight `run_id`. Fire-and-forget and
    /// idempotent: a no-op if the run is not currently executing in this process
    /// (already finished, never launched here, or owned by another process).
    ///
    /// Recording `RunState::Cancelled` on the ledger does not by itself stop the
    /// agent loop — the runtime only relinquishes at a cancellation token — so the
    /// server calls this when a `CancelRun` is accepted, so the live loop actually
    /// stops instead of running on and overwriting the cancelled projection with a
    /// later `Completed`/`Failed`. The default does nothing: the executor-less
    /// server path (`server::run`, the daemon's own tests) drives no runtime loop.
    fn cancel_run(&self, _run_id: RunId) {}

    /// The shared event fan-out and approval broker this executor publishes a
    /// run's events through and resolves approvals against.
    ///
    /// A run's events must reach the *same* [`SubscriptionHub`] the server fans
    /// out to attached clients, and a client's `ResolveApproval` must drive the
    /// *same* [`ApprovalBroker`] the runtime awaits — otherwise a running client
    /// never observes the run and an approval never wakes it. The executor is
    /// built (in the assembly binary) holding these; the server wires its own
    /// `CommandProcessor` and per-client forwarders to whatever this returns.
    ///
    /// The default `None` leaves the server to create its own fresh instances —
    /// the executor-less path taken by [`server::run`](crate::server::run) and
    /// the daemon's own integration tests.
    ///
    /// [`SubscriptionHub`]: crate::subscriptions::SubscriptionHub
    /// [`ApprovalBroker`]: crate::approvals::ApprovalBroker
    fn collaborators(
        &self,
    ) -> Option<(
        crate::subscriptions::SubscriptionHub,
        crate::approvals::ApprovalBroker,
    )> {
        None
    }

    /// The assembly-provided [`DocumentMutator`](crate::documents::DocumentMutator)
    /// that applies an accepted `MutateDocument` command onto the authoritative
    /// collaborative document (Phase 4 STEP 4.3 client transport).
    ///
    /// Bundled with the executor for the same reason as
    /// [`collaborators`](RunExecutor::collaborators): both are seams the daemon
    /// declares and the `codypendentd` assembly (which alone can name the
    /// knowledge crate) implements and injects together. The default `None`
    /// leaves document transport unwired — the executor-less server and the
    /// daemon's own tests then reject `MutateDocument` structurally rather than
    /// applying it.
    fn document_mutator(&self) -> Option<std::sync::Arc<dyn crate::documents::DocumentMutator>> {
        None
    }

    /// The assembly-provided [`DocumentLeaser`](crate::documents::DocumentLeaser)
    /// that acquires/releases the block-range edit leases gating `MutateDocument`
    /// (Phase 4 STEP 4.3 client transport). Bundled with
    /// [`document_mutator`](RunExecutor::document_mutator) for the same reason —
    /// both name `codypendent-knowledge`, which only the assembly can. The default
    /// `None` leaves lease commands unwired (rejected `document.transport-unavailable`).
    fn document_leaser(&self) -> Option<std::sync::Arc<dyn crate::documents::DocumentLeaser>> {
        None
    }

    /// The assembly-provided [`WorkflowStarter`](crate::workflows::WorkflowStarter)
    /// that creates a durable run from an accepted `StartWorkflow` command (Phase 5
    /// STEP 5.2). Bundled with the executor like the document seams — it names
    /// `codypendent-workflow` and the pool, which only the assembly can. The default
    /// `None` leaves workflow-start unwired: the executor-less server and the
    /// daemon's own tests then reject `StartWorkflow` with
    /// `workflow.transport-unavailable`, exactly as they leave a run `Queued`.
    fn workflow_starter(&self) -> Option<std::sync::Arc<dyn crate::workflows::WorkflowStarter>> {
        None
    }

    /// The assembly-provided [`WorkflowLifecycle`](crate::workflows::WorkflowLifecycle)
    /// that pauses/resumes/retries an existing durable run from the corresponding
    /// commands (Phase 5 STEP 5.2). Bundled with the executor like
    /// [`workflow_starter`](RunExecutor::workflow_starter) — it names the
    /// `codypendent-workflow` conductor and the pool, which only the assembly can.
    /// The default `None` leaves those commands unwired: the executor-less server
    /// and the daemon's own tests then reject them `workflow.transport-unavailable`,
    /// exactly as `StartWorkflow` is rejected without a starter.
    fn workflow_lifecycle(
        &self,
    ) -> Option<std::sync::Arc<dyn crate::workflows::WorkflowLifecycle>> {
        None
    }

    /// The assembly-provided [`PromotionGateway`](crate::promotion::PromotionGateway)
    /// that drives the evaluation-gated promotion pipeline (Phase 7 STEP 7.5).
    /// Bundled with the executor like the workflow seams — it names
    /// `codypendent-eval` and the pool, which only the assembly can. The default
    /// `None` leaves every promotion command unwired: the executor-less server
    /// and the daemon's own tests then reject them
    /// `promotion.transport-unavailable`, exactly as `StartWorkflow` is rejected
    /// without a starter.
    fn promotion_gateway(&self) -> Option<std::sync::Arc<dyn crate::promotion::PromotionGateway>> {
        None
    }
}
