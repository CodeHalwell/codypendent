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

use codypendent_protocol::{AgentMode, ModelId, RunId, SessionId};

/// One entry of a session's prior transcript, carried into a continuation
/// run's [`RunLaunch`] (continuous-session plan, Task 2).
///
/// Mirrors `codypendent_runtime::agent::TurnItem` variant-for-variant, but is
/// a distinct, crate-local type rather than a re-export or direct use of it:
/// `codypendent-daemon` must never depend on `codypendent-runtime` (see this
/// module's doc comment above — that dependency runs the other way, and
/// `RunExecutor`/[`RunLaunch`] are precisely the seam that lets the assembly
/// binary bridge the two crates without a cycle). `RunLaunch` is built
/// directly by this crate's own code (`server.rs`, from an accepted command),
/// so its `prior` element type must be nameable here — this enum is that
/// dependency-safe carrier. The assembly executor (`crates/codypendentd`,
/// which depends on both crates) converts it 1:1 into
/// `Vec<codypendent_runtime::agent::TurnItem>` when it builds the
/// `RunContext`.
///
/// Server-internal, not a wire type: never serialized, never sent over the
/// transport, never persisted — it only crosses the in-process
/// `RunLaunch` → `RunContext` handoff.
#[derive(Debug, Clone, PartialEq)]
pub enum PriorTurn {
    /// Mirrors `TurnItem::Objective`.
    Objective(String),
    /// Mirrors `TurnItem::Assistant`.
    Assistant(String),
    /// Mirrors `TurnItem::ToolResult`.
    ToolResult {
        /// The tool that produced the observation.
        tool: String,
        /// The compacted, model-facing output.
        output: String,
    },
    /// Mirrors `TurnItem::Steering`.
    Steering(String),
}

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
    /// The model the operator **pinned** for this run via the `/model` picker
    /// (STEP MP2), carried from the accepted `StartRun`. `Some(id)` makes the
    /// executor run on exactly that model — subject to the classification hard
    /// filter (a pinned hosted model for classified data is refused when routing
    /// is on, never run off-device). `None` lets the executor resolve/route the
    /// model exactly as before. Mirrors [`repository`](RunLaunch::repository) as
    /// an optional per-run override.
    pub model: Option<ModelId>,
    /// The prior conversation transcript to seed a continuation run with
    /// (continuous-session plan, Task 2), as a dependency-safe [`PriorTurn`]
    /// carrier. Empty for a plain/first run (every construction site defaults
    /// it today); a later task populates it for a `SubmitUserInput`-launched
    /// continuation. Converted 1:1 into `RunContext.prior`
    /// (`codypendent_runtime::agent::RunContext`) where the assembly executor
    /// builds the run context — this crate cannot name that type directly
    /// (see [`PriorTurn`]'s doc comment).
    pub prior: Vec<PriorTurn>,
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

    /// The assembly-provided [`DocumentPublisher`](crate::documents::DocumentPublisher)
    /// that computes an accepted `PublishDocument` command's plan, parks its
    /// approval, and (once approved) executes it (Phase 4 STEP 4.4). Bundled with
    /// the document seams for the same reason — it names `codypendent-knowledge`,
    /// which only the assembly can. The default `None` leaves publication unwired
    /// (rejected `document.transport-unavailable`).
    fn document_publisher(
        &self,
    ) -> Option<std::sync::Arc<dyn crate::documents::DocumentPublisher>> {
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

    /// The assembly-provided [`BlackboardReader`](crate::blackboard::BlackboardReader)
    /// that projects a workflow run's board for an accepted `ReadBlackboard` command
    /// (Phase 5 STEP 5.3). Bundled with the executor like the workflow seams — it
    /// names `codypendent-workflow`'s `BlackboardStore` and the pool, which only the
    /// assembly can. The default `None` leaves board reads unwired: the
    /// executor-less server and the daemon's own tests then reject `ReadBlackboard`
    /// with `workflow.transport-unavailable`.
    fn blackboard_reader(&self) -> Option<std::sync::Arc<dyn crate::blackboard::BlackboardReader>> {
        None
    }

    /// The per-run blackboard fan-out ([`BlackboardHub`](crate::blackboard::BlackboardHub))
    /// the workflow executor publishes posted artifacts through and the server
    /// subscribes a client's `Subscription::Blackboard` forwarder to (Phase 5
    /// STEP 5.3). Returned here — rather than created fresh by the server like the
    /// document hub — because the *publisher* is the agent loop deep inside the
    /// executor, not a command the server intercepts, so both sides must share the
    /// executor's hub (exactly as they share its
    /// [`collaborators`](RunExecutor::collaborators) `SubscriptionHub`). The default
    /// `None` leaves the server to create its own fresh (never-published) hub — the
    /// executor-less path.
    fn blackboard_hub(&self) -> Option<crate::blackboard::BlackboardHub> {
        None
    }

    /// The assembly-provided [`WorkflowReader`](crate::workflow_stream::WorkflowReader)
    /// that projects a workflow run's observability snapshot for an accepted
    /// `ReadWorkflowRun` command (Phase 5 STEP 5.2 / T9). Bundled with the executor
    /// like the workflow seams — it names the `codypendent-workflow` store and the
    /// pool, which only the assembly can. The default `None` leaves snapshot reads
    /// unwired: the executor-less server and the daemon's own tests then reject
    /// `ReadWorkflowRun` with `workflow.transport-unavailable`.
    fn workflow_reader(
        &self,
    ) -> Option<std::sync::Arc<dyn crate::workflow_stream::WorkflowReader>> {
        None
    }

    /// The per-run node-lifecycle fan-out
    /// ([`WorkflowHub`](crate::workflow_stream::WorkflowHub)) the workflow host
    /// publishes node transitions through and the server subscribes a client's
    /// `Subscription::Workflow` forwarder to (Phase 5 STEP 5.2 / T9). Returned here —
    /// rather than created fresh by the server like the document hub — because the
    /// *publisher* is the workflow host + observer deep inside the executor, not a
    /// command the server intercepts, so both sides must share the executor's hub
    /// (exactly as the blackboard hub is shared). The default `None` leaves the server
    /// to create its own fresh (never-published) hub — the executor-less path.
    fn workflow_hub(&self) -> Option<crate::workflow_stream::WorkflowHub> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_launch_prior_defaults_empty_and_a_seeded_value_round_trips() {
        // Task 2 (continuous-session plan): `RunLaunch.prior` is the
        // dependency-safe carrier (see `PriorTurn`) threaded through to a
        // `RunContext`'s own `prior` by the assembly executor. Every
        // construction site defaults it empty; this proves the field itself
        // round-trips.
        let launch = RunLaunch {
            session_id: SessionId::new(),
            run_id: RunId::new(),
            objective: "objective".to_string(),
            mode: AgentMode::Build,
            repository: PathBuf::from("."),
            model: None,
            prior: Vec::new(),
        };
        assert!(launch.prior.is_empty());

        let seeded = vec![PriorTurn::Objective("earlier turn".to_string())];
        let launch = RunLaunch {
            prior: seeded.clone(),
            ..launch
        };
        assert_eq!(launch.prior, seeded);
    }
}
