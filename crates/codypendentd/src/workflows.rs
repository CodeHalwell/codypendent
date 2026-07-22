//! The daemon's workflow-execution host: creates, **drives**, recovers, and
//! controls durable workflow runs (Phase 5 STEP 5.2).
//!
//! Like [`KnowledgeDocumentMutator`](crate::documents::KnowledgeDocumentMutator),
//! this lives in the assembly binary because it bridges the daemon (which declares
//! the [`WorkflowStarter`] / [`WorkflowLifecycle`] seams) and `codypendent-workflow`
//! (which owns the compiler, the durable [`WorkflowStore`], and the
//! [`WorkflowConductor`]). The daemon crate cannot name the workflow crate, so the
//! composition happens here.
//!
//! [`WorkflowConductorHost`] fills both seams over one shared [`WorkflowConductor`]:
//!
//! * `StartWorkflow` → [`start`](WorkflowStarter::start): compile the manifest,
//!   create a durable run (recording the manifest so recovery can recompile it),
//!   and **spawn the conductor's drive** in the background so the run actually
//!   advances toward a terminal state — fire-and-forget, like `spawn_run`.
//! * `PauseWorkflow` / `ResumeWorkflow` / `RetryWorkflowNode` →
//!   [`WorkflowLifecycle`]: the synchronous state change, then (for resume/retry) a
//!   backgrounded drive.
//! * [`recover`](WorkflowConductorHost::recover): the startup pass — spawn a drive
//!   for every incomplete run so a crash-interrupted run resumes.
//!
//! **Per-run serialization.** Every drive (start, resume, retry, recovery) runs
//! under a per-run async lock, so two drives never advance one run concurrently —
//! a resume that races a still-draining pause, or a duplicate `StartWorkflow`, can
//! never launch two schedulers onto the same run. A drive also skips a run that has
//! already reached a terminal state, so a redundant drive is a clean no-op.
//!
//! **The node-execution seam.** The conductor advances the graph; the actual work
//! of one node — running an agent through the agent loop, or a tool through the
//! tool layer — is the [`NodeExecutor`] the host is generic over. This crate can
//! supply the real, agent-loop-backed executor; the host's own tests inject a fake
//! that completes/fails nodes on command, so the whole daemon path (create → drive
//! → recover → pause/resume/retry) is verified without a model. The real leaf —
//! `AgentLoopNodeExecutor` — lives in [`crate::workflow_exec`] and drives an agent
//! node through the agent loop.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use codypendent_daemon::workflow_stream::{
    ReadWorkflowRunRequest, WorkflowHub, WorkflowReadFuture, WorkflowReader,
};
use codypendent_daemon::workflows::{
    CancelWorkflowRequest, PauseWorkflowRequest, ResumeWorkflowRequest, RetryWorkflowNodeRequest,
    StartWorkflowRequest, WorkflowLifecycle, WorkflowLifecycleFuture, WorkflowStartFuture,
    WorkflowStarter,
};
use codypendent_protocol::{
    CodypendentError, WorkflowEvent, WorkflowNodeState, WorkflowNodeView, WorkflowRunPhase,
    WorkflowRunSnapshot as ProtocolWorkflowRunSnapshot,
};
use codypendent_workflow::{
    compile_yaml, BudgetWarning, ConductorError, NodeExecutor, NodeObserver, NodeState,
    NodeTransition, WorkflowConductor, WorkflowNodeRecord, WorkflowRunState, WorkflowStore,
    WorkflowStoreError,
};
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::workflow_exec::{load_agent_profiles, WorkflowRunCancellations};

/// The per-run drive-lock registry a [`WorkflowConductorHost`] holds: one async
/// lock per in-flight run id, so no two drives advance one run at once.
///
/// Exposed (rather than kept private to the host) so a builder that
/// **reconfigures** a host — e.g.
/// [`RuntimeExecutor::with_github`](crate::executor::RuntimeExecutor::with_github),
/// which swaps the node executor after startup — can carry the SAME registry
/// into the replacement via [`WorkflowConductorHost::with_drive_locks`] instead
/// of starting a fresh, disconnected one (P5-D6c): rebuilding with a fresh map
/// would silently defeat per-run serialization for any drive that started under
/// the OLD host and is still in flight when the NEW host takes over.
pub type DriveLockRegistry = Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

/// Creates, drives, recovers, and controls durable workflow runs over the daemon's
/// pool, executing each node through the injected [`NodeExecutor`] `E`. Cheap to
/// clone — a pool handle, the shared per-run drive locks, and the executor handle.
pub struct WorkflowConductorHost<E> {
    pool: SqlitePool,
    node_executor: Arc<E>,
    conductor: WorkflowConductor,
    /// One async lock per in-flight run id, so no two drives advance one run at
    /// once. Shared across every clone of the host (and thus across the
    /// `WorkflowStarter` and `WorkflowLifecycle` seams the server pulls out), so
    /// start / resume / retry / recovery all serialize on the same lock per run.
    drive_locks: DriveLockRegistry,
    /// The per-run node-lifecycle fan-out (T9): the drive's [`PublishingNodeObserver`]
    /// publishes each node transition here, and this host publishes run-phase changes
    /// here, so a client's `Subscription::Workflow` forwarder delivers them. Shared
    /// with the server (which subscribes) via the executor's hub — a fresh empty hub
    /// in a test host whose events no client observes.
    workflows: WorkflowHub,
    /// In-flight node agent-run cancellation handles (T9), shared with the node
    /// executor (which registers) so the [`cancel`](WorkflowLifecycle::cancel) seam
    /// interrupts an in-flight node's agent run.
    cancellations: WorkflowRunCancellations,
}

// Manual `Clone` so the host clones regardless of whether `E: Clone` — only
// `Arc<E>` is cloned, never `E` itself.
impl<E> Clone for WorkflowConductorHost<E> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            node_executor: self.node_executor.clone(),
            conductor: self.conductor,
            drive_locks: self.drive_locks.clone(),
            workflows: self.workflows.clone(),
            cancellations: self.cancellations.clone(),
        }
    }
}

impl<E: NodeExecutor + 'static> WorkflowConductorHost<E> {
    /// Build a host over the daemon's pool with the node executor to run each node
    /// through, and a **fresh** drive-lock registry. The workflow tables share the
    /// daemon's pool (the migrations are workspace-wide).
    ///
    /// Use [`with_drive_locks`](Self::with_drive_locks) instead when
    /// reconfiguring an EXISTING host (a new node executor over the same
    /// running daemon) — this constructor is only safe for the very first host a
    /// process builds, before any drive could have started (P5-D6c).
    pub fn new(pool: SqlitePool, node_executor: Arc<E>) -> Self {
        Self::with_drive_locks(pool, node_executor, Arc::new(Mutex::new(HashMap::new())))
    }

    /// Build a host sharing an **existing** drive-lock registry — e.g. when
    /// reconfiguring the host with a different node executor after startup
    /// (P5-D6c). Carrying the same registry forward (rather than minting a
    /// fresh one, which [`new`](Self::new) does) means a drive already
    /// serializing on a run id under the OLD host still serializes against the
    /// NEW host's drives for that same run id.
    pub fn with_drive_locks(
        pool: SqlitePool,
        node_executor: Arc<E>,
        drive_locks: DriveLockRegistry,
    ) -> Self {
        Self {
            pool,
            node_executor,
            conductor: WorkflowConductor::new(),
            drive_locks,
            // A fresh (empty) hub + cancellation registry by default — a test host's
            // observability goes nowhere and its cancel seam fires nothing. Production
            // replaces both with the shared ones via [`with_streaming`] so the server
            // and node executor meet the host (T9).
            workflows: WorkflowHub::new(),
            cancellations: WorkflowRunCancellations::default(),
        }
    }

    /// Bind the host to the SHARED node-lifecycle hub (its drives publish
    /// transitions + run-phase changes here, and the server subscribes to it) and
    /// the SHARED cancellation registry (its cancel seam fires the in-flight node
    /// agent-run tokens the node executor registered) — T9. The assembly's
    /// `build_workflow_host` calls this so all three (server, host, node executor)
    /// meet on one hub + one registry; a test host keeps the fresh empty defaults.
    #[must_use]
    pub(crate) fn with_streaming(
        mut self,
        workflows: WorkflowHub,
        cancellations: WorkflowRunCancellations,
    ) -> Self {
        self.workflows = workflows;
        self.cancellations = cancellations;
        self
    }

    /// This host's shared per-run drive-lock registry, so a caller
    /// reconfiguring the host (see [`with_drive_locks`](Self::with_drive_locks))
    /// can carry it into the replacement instead of starting a fresh,
    /// disconnected one (P5-D6c).
    #[must_use]
    pub fn drive_locks(&self) -> DriveLockRegistry {
        self.drive_locks.clone()
    }

    /// Startup recovery: spawn a background drive for every non-terminal run, so a
    /// run interrupted by a crash resumes exactly once from where it stopped. A
    /// **paused** run is left paused (it awaits an explicit resume). Returns how
    /// many drives were spawned. A drive that cannot proceed (no manifest, changed
    /// graph, …) is a no-op logged by the drive task, never fatal here — mirroring
    /// [`relaunch_queued_runs`](crate::executor::RuntimeExecutor::relaunch_queued_runs).
    pub async fn recover(&self) -> Result<usize, WorkflowStoreError> {
        let runs = WorkflowStore::new()
            .list_incomplete_runs(&self.pool)
            .await?;
        let mut spawned = 0usize;
        for run in runs {
            if run.state == WorkflowRunState::Paused {
                continue;
            }
            self.spawn_drive(run.id);
            spawned += 1;
        }
        Ok(spawned)
    }

    /// Spawn a background task that drives `run_id` to a stopping state under its
    /// per-run lock. Fire-and-forget: the caller never awaits it. Skips a run that
    /// is already terminal (a duplicate start, or a run another drive finished
    /// while this one waited on the lock), so a redundant drive is a clean no-op.
    fn spawn_drive(&self, run_id: String) {
        let host = self.clone();
        tokio::spawn(async move {
            let lock = host.run_lock(&run_id);
            {
                let _guard = lock.lock().await;
                match WorkflowStore::new().snapshot(&host.pool, &run_id).await {
                    Ok(Some(snapshot)) if snapshot.run.state.is_terminal() => return,
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        warn!(run = %run_id, "workflow run vanished before its drive");
                        return;
                    }
                    Err(error) => {
                        warn!(run = %run_id, %error, "could not read a workflow run before driving");
                        return;
                    }
                }
                // A per-run observer records each node-lifecycle transition: the
                // logging observer (daemon log) COMPOSED with a publishing observer
                // that fans each transition out to `Subscription::Workflow`
                // subscribers (T9 — compose, don't replace).
                let observer = (
                    LoggingNodeObserver {
                        run_id: run_id.clone(),
                    },
                    PublishingNodeObserver {
                        run_id: run_id.clone(),
                        hub: host.workflows.clone(),
                    },
                );
                // The run is about to advance — announce it Running to subscribers
                // (the driver's own Pending→Running is an internal node/run write;
                // this is the run-phase signal). Idempotent full-state, so a paused
                // run that never actually advances simply re-announces its phase below.
                //
                // Ordering caveat (T9 review Finding C): this publish PRECEDES the
                // driver persisting `Running` (which happens inside `conductor.drive`'s
                // `try_transition_to_running`), so a subscriber attaching in that tiny
                // window can miss the Running phase in BOTH this live event and a
                // snapshot read (which would still show Pending). It is cosmetic — the
                // authoritative NODE transitions that follow are unaffected, and the
                // run-phase badge self-corrects on the next phase change (or a re-read)
                // — so the run-phase signal is best-effort by design; node state, not
                // the badge, is the source of truth for progress.
                host.publish_run_phase(&run_id, WorkflowRunState::Running);
                match host
                    .conductor
                    .drive(&host.pool, &run_id, host.node_executor.as_ref(), &observer)
                    .await
                {
                    Ok(state) => {
                        // Announce the stopping phase (Completed / Failed / Paused /
                        // Cancelled) to subscribers.
                        host.publish_run_phase(&run_id, state);
                        info!(run = %run_id, state = state.as_str(), "workflow run driven to a stopping state")
                    }
                    Err(error) => warn!(run = %run_id, %error, "workflow run drive ended in error"),
                }
                // The drive has fully drained — drop any cancellation entry for this
                // run so a cancelled run's sticky registry entry does not linger (T9).
                host.cancellations.finish(&run_id);
            }
            host.prune_run_lock(&run_id, lock);
        });
    }

    /// Publish a run-phase change to the run's `Subscription::Workflow` subscribers
    /// (T9). Best-effort: with no subscribers (or the executor-less test host's empty
    /// hub) it is a silent no-op; the durable store remains the record a late
    /// subscriber reconstructs its baseline from.
    fn publish_run_phase(&self, workflow_run_id: &str, state: WorkflowRunState) {
        self.workflows.publish(
            workflow_run_id,
            WorkflowEvent::RunPhaseChanged {
                workflow_run_id: workflow_run_id.to_owned(),
                phase: run_state_to_wire(state),
            },
        );
    }

    /// The per-run drive lock, created on first use.
    fn run_lock(&self, run_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.drive_locks
            .lock()
            .expect("drive-locks registry")
            .entry(run_id.to_owned())
            .or_default()
            .clone()
    }

    /// Drop a run's lock entry once no drive holds or awaits it, so the registry
    /// does not grow without bound over a long-lived daemon. `lock` is this task's
    /// own clone; with the map entry that is two strong refs when no one else waits,
    /// so a count of two or fewer means the entry is safe to remove. A waiter holds
    /// a third ref (taken under the same registry mutex as this check), so the entry
    /// is kept and both drives share the one lock.
    fn prune_run_lock(&self, run_id: &str, lock: Arc<tokio::sync::Mutex<()>>) {
        let mut locks = self.drive_locks.lock().expect("drive-locks registry");
        if Arc::strong_count(&lock) <= 2 {
            locks.remove(run_id);
        }
    }

    /// Whether `run_id` is a freshly created run that no drive has touched: pending,
    /// with every node still pending at attempt zero. Only such a run is driven by
    /// `start`, so a duplicate `StartWorkflow` that resolved to an already-advancing
    /// (or paused, or finished) run never kicks off a second drive.
    async fn is_fresh(&self, run_id: &str) -> bool {
        match WorkflowStore::new().snapshot(&self.pool, run_id).await {
            Ok(Some(snapshot)) => {
                snapshot.run.state == WorkflowRunState::Pending
                    && snapshot
                        .nodes
                        .iter()
                        .all(|node| node.state == NodeState::Pending && node.attempt == 0)
            }
            _ => false,
        }
    }
}

impl<E: NodeExecutor + 'static> WorkflowStarter for WorkflowConductorHost<E> {
    fn start(&self, request: StartWorkflowRequest) -> WorkflowStartFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            let StartWorkflowRequest {
                manifest,
                inputs,
                idempotency_key,
                repository,
                ..
            } = request;

            // Compile the manifest — a malformed workflow is the client's to fix,
            // surfaced verbatim. Non-retryable: recompiling the same text fails the
            // same way.
            let compiled = compile_yaml(&manifest).map_err(|error| {
                CodypendentError::new(
                    "workflow.invalid-manifest",
                    format!("workflow manifest does not compile: {error}"),
                    false,
                )
            })?;

            // Role → profile validation (T8), scoped to a MULTI-agent workflow
            // whose run repository HAS profiles configured: every agent role must
            // resolve, else a reviewer-style role would silently fall back to the
            // permissive `Build` mode at execution instead of its structurally
            // read-only profile (STEP 5.4.1 independence is structural, not
            // prompted). The conservative matrix shipped (see the T8 report):
            //
            //   * no profiles directory (empty set) → the single-agent baseline
            //     (`Build`/`hosted-default`) stays selectable; NO rejection here,
            //     even for a multi-agent workflow — the defaults apply uniformly;
            //   * a single-agent (or tool-only) workflow → NOT validated here; an
            //     unresolved role fails legibly at execution (never a silent
            //     default), so a single agent stays selectable;
            //   * a multi-agent workflow WITH profiles configured → every agent
            //     role must resolve, else this rejects before the run starts.
            //
            // A malformed/ambiguous profile is a real misconfiguration, surfaced
            // here for a multi-agent workflow rather than deferred to a node
            // failure mid-run.
            if let Some(repo) = repository.as_deref() {
                if compiled.agent_node_count() >= 2 {
                    let profiles = load_agent_profiles(Path::new(repo)).map_err(|reason| {
                        CodypendentError::new("workflow.invalid-agent-profile", reason, false)
                    })?;
                    if !profiles.is_empty() {
                        let unresolved = profiles.unresolved_roles(&compiled);
                        if !unresolved.is_empty() {
                            let detail = unresolved
                                .iter()
                                .map(|r| format!("step `{}` \u{2192} role `{}`", r.step, r.role))
                                .collect::<Vec<_>>()
                                .join(", ");
                            return Err(CodypendentError::new(
                                "workflow.unresolved-role",
                                format!(
                                    "multi-agent workflow has {} unresolved agent role(s) against \
                                     the repository's .codypendent/agents: {detail}",
                                    unresolved.len()
                                ),
                                false,
                            ));
                        }
                    }
                }
            }

            // Create the durable run idempotently, keyed by the command's
            // idempotency key (a duplicate delivery resolves to the same run), and
            // record the manifest so recovery can recompile it. A store error may be
            // transient (a busy database), so mark it retryable — EXCEPT a reused
            // key under a different manifest (P5-D2), which is a client error (the
            // same retry would fail again) surfaced under its own dotted code
            // rather than the generic store-error.
            let run_id = WorkflowStore::new()
                .create_run_idempotent(
                    &host.pool,
                    &compiled,
                    &idempotency_key,
                    &inputs,
                    Some(&manifest),
                    // Persist the run's repository so recovery carves each agent
                    // node's isolated worktree from the right checkout (T5).
                    repository.as_deref(),
                )
                .await
                .map_err(|error| match error {
                    WorkflowStoreError::IdempotencyKeyReused { .. } => CodypendentError::new(
                        "workflow.idempotency-mismatch",
                        error.to_string(),
                        false,
                    ),
                    other => CodypendentError::new(
                        "workflow.store-error",
                        format!("could not create the workflow run: {other}"),
                        true,
                    ),
                })?;

            // Drive the created run to a terminal state in the background — but only
            // a genuinely fresh run, so a duplicate delivery that resolved to an
            // existing run leaves whatever is driving it alone.
            if host.is_fresh(&run_id).await {
                host.spawn_drive(run_id.clone());
            }
            Ok(run_id)
        })
    }
}

impl<E: NodeExecutor + 'static> WorkflowLifecycle for WorkflowConductorHost<E> {
    fn pause(&self, request: PauseWorkflowRequest) -> WorkflowLifecycleFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            // Deliberately does NOT take the per-run drive lock (unlike
            // `retry_node` below, P5-D3): pause only flips `workflow_runs.state`,
            // which a live drive's in-flight node execution never writes back
            // over (node rows are a disjoint set of columns), so there is no
            // "in-flight executor overwrites the mutation" hazard here to close.
            // Taking the lock would instead make `PauseWorkflow` block until the
            // CURRENT drive fully finishes — breaking the cooperative contract
            // ("the driver stops at the next scheduling boundary", not "once
            // the whole run is already done").
            host.conductor
                .pause(&host.pool, &request.workflow_run_id)
                .await
                .map_err(conductor_error_to_protocol)
        })
    }

    fn resume(&self, request: ResumeWorkflowRequest) -> WorkflowLifecycleFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            // Validate synchronously so the reply is an accurate accept/reject,
            // flipping the run from Paused to Running (P5-D5 — the library-level
            // driver refuses to touch anything but Pending/Running), then drive
            // the resumed run onward in the background.
            host.conductor
                .prepare_resume(&host.pool, &request.workflow_run_id)
                .await
                .map_err(conductor_error_to_protocol)?;
            host.spawn_drive(request.workflow_run_id.clone());
            Ok(())
        })
    }

    fn retry_node(&self, request: RetryWorkflowNodeRequest) -> WorkflowLifecycleFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            // Acquire the SAME per-run lock a live drive holds for its whole
            // duration (P5-D3): without this, resetting the node here can race
            // an in-flight `NodeExecutor::execute` call for that same node,
            // whose eventual (stale) completion write lands straight over the
            // reset via `transition_node`'s COALESCE-preserve semantics —
            // silently discarding the retry. Acquiring the lock means a retry
            // issued against an actively driving run *waits* for that drive to
            // reach its next stopping point before the reset is applied: the
            // reset is deferred, never lost.
            let lock = host.run_lock(&request.workflow_run_id);
            {
                let _guard = lock.lock().await;
                // Reset the node + its dependents (so an unknown node or a
                // changed graph rejects), still holding the lock so nothing else
                // can drive this run while the reset is in progress.
                host.conductor
                    .prepare_retry(&host.pool, &request.workflow_run_id, &request.node_id)
                    .await
                    .map_err(conductor_error_to_protocol)?;
            }
            // Drive the reset run in the background, after releasing the lock
            // above (spawn_drive re-acquires it itself).
            host.spawn_drive(request.workflow_run_id.clone());
            Ok(())
        })
    }

    fn cancel(&self, request: CancelWorkflowRequest) -> WorkflowLifecycleFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            // Validate + drain through the conductor FIRST (a terminal run rejects
            // cleanly, without side effects): flip the run `Cancelled` — a live driver
            // observes it at its next scheduling boundary and stops launching further
            // nodes, exactly like pause — and mark every still-`Pending` node
            // `Skipped`. Like `pause` (P5-D3) this deliberately does NOT take the
            // per-run drive lock: cancel writes the run state + the disjoint pending
            // node rows, never the in-flight node's row, so there is no
            // "in-flight executor overwrites the mutation" hazard — and taking the
            // lock would make cancel block until the whole drive finished, breaking
            // the cooperative "stops at the next boundary" contract.
            let skipped = host
                .conductor
                .cancel(&host.pool, &request.workflow_run_id)
                .await
                .map_err(conductor_error_to_protocol)?;
            // Interrupt any in-flight node's agent run through the SAME cancellation
            // machinery `CancelRun` uses — recording `Cancelled` does not by itself
            // stop the runtime loop, so fire its token (a no-op when no node is in
            // flight, e.g. a paused run cancelled).
            host.cancellations.cancel(&request.workflow_run_id);
            // Publish the cancel drain to live subscribers: each newly-skipped node's
            // view, then the Cancelled run phase. A late subscriber instead
            // reconstructs both from the snapshot (both are persisted), so the live and
            // catch-up paths agree.
            if !skipped.is_empty() {
                if let Ok(Some(snapshot)) = WorkflowStore::new()
                    .snapshot(&host.pool, &request.workflow_run_id)
                    .await
                {
                    for node_id in &skipped {
                        if let Some(record) = snapshot.nodes.iter().find(|n| &n.node_id == node_id)
                        {
                            host.workflows.publish(
                                &request.workflow_run_id,
                                WorkflowEvent::NodeTransitioned(node_record_to_wire(
                                    &request.workflow_run_id,
                                    record,
                                )),
                            );
                        }
                    }
                }
            }
            host.publish_run_phase(&request.workflow_run_id, WorkflowRunState::Cancelled);
            Ok(())
        })
    }
}

/// Map a [`ConductorError`] to the wire [`CodypendentError`] a client branches on
/// by code. A store/database hiccup is retryable; every semantic rejection (no
/// manifest, illegal transition, unknown run/node, changed graph) is not.
fn conductor_error_to_protocol(error: ConductorError) -> CodypendentError {
    let message = error.to_string();
    let (code, retryable) = match &error {
        ConductorError::NoManifest(_) => ("workflow.no-manifest", false),
        ConductorError::Compile(_) => ("workflow.invalid-manifest", false),
        ConductorError::NotFound(_) => ("workflow.not-found", false),
        ConductorError::IllegalTransition { .. } => ("workflow.illegal-transition", false),
        ConductorError::Store(WorkflowStoreError::NotFound(_)) => ("workflow.not-found", false),
        ConductorError::Store(WorkflowStoreError::GraphSignatureChanged { .. }) => {
            ("workflow.graph-changed", false)
        }
        ConductorError::Store(_) => ("workflow.store-error", true),
    };
    CodypendentError::new(code, message, retryable)
}

/// Publishes each node-lifecycle transition of one run to its
/// `Subscription::Workflow` subscribers (T9) — the client-facing half of the
/// observer seam, composed alongside the [`LoggingNodeObserver`]. Every transition
/// carries the node's **full** current view (state, attempt, measured cost,
/// failure/block reason, budget warnings), captured synchronously from the
/// [`NodeTransition`] the driver reports (the callback is sync — it cannot re-read
/// the store — but the store write that preceded it holds the same values, so this
/// is persist-before-publish). A client merges each delivery by `node_id`.
struct PublishingNodeObserver {
    run_id: String,
    hub: WorkflowHub,
}

impl NodeObserver for PublishingNodeObserver {
    fn on_transition(&self, transition: NodeTransition<'_>) {
        let view = WorkflowNodeView {
            workflow_run_id: self.run_id.clone(),
            node_id: transition.node_id.to_owned(),
            state: node_state_to_wire(transition.state),
            attempt: transition.attempt,
            cost: transition.cost.cloned(),
            error: transition.error.map(str::to_owned),
            warnings: transition
                .warnings
                .iter()
                .map(render_budget_warning)
                .collect(),
        };
        self.hub
            .publish(&self.run_id, WorkflowEvent::NodeTransitioned(view));
    }

    // `on_budget_warning` / `on_attempt_failed` / `on_recovery_reset` are the logging
    // observer's concern: the warnings already ride the `Completed` transition above
    // (so the published node view carries them), and per-attempt failure history /
    // recovery resets are not the latest durable node fact the graph view shows. The
    // publisher keeps the trait's no-op defaults for them.
}

/// Map the workflow crate's durable [`NodeState`] to the wire
/// [`WorkflowNodeState`] a client renders (T9).
fn node_state_to_wire(state: NodeState) -> WorkflowNodeState {
    match state {
        NodeState::Pending => WorkflowNodeState::Pending,
        NodeState::Running => WorkflowNodeState::Running,
        NodeState::WaitingApproval => WorkflowNodeState::WaitingApproval,
        NodeState::Blocked => WorkflowNodeState::Blocked,
        NodeState::Completed => WorkflowNodeState::Completed,
        NodeState::Failed => WorkflowNodeState::Failed,
        NodeState::Skipped => WorkflowNodeState::Skipped,
    }
}

/// Map the workflow crate's durable [`WorkflowRunState`] to the wire
/// [`WorkflowRunPhase`] a client renders (T9).
fn run_state_to_wire(state: WorkflowRunState) -> WorkflowRunPhase {
    match state {
        WorkflowRunState::Pending => WorkflowRunPhase::Pending,
        WorkflowRunState::Running => WorkflowRunPhase::Running,
        WorkflowRunState::Paused => WorkflowRunPhase::Paused,
        WorkflowRunState::Completed => WorkflowRunPhase::Completed,
        WorkflowRunState::Failed => WorkflowRunPhase::Failed,
        WorkflowRunState::Cancelled => WorkflowRunPhase::Cancelled,
    }
}

/// Pre-render a budget warning for the wire (T9) — the same fields the logging
/// observer emits, as one human line.
fn render_budget_warning(warning: &BudgetWarning) -> String {
    format!(
        "{} {} at {}/{} (80%)",
        warning.scope.as_str(),
        warning.dimension.as_str(),
        warning.used,
        warning.limit
    )
}

/// Project one durable node record into the wire [`WorkflowNodeView`] (T9), used for
/// both the cancel-drain `Skipped` publishes and the `ReadWorkflowRun` snapshot.
fn node_record_to_wire(workflow_run_id: &str, record: &WorkflowNodeRecord) -> WorkflowNodeView {
    WorkflowNodeView {
        workflow_run_id: workflow_run_id.to_owned(),
        node_id: record.node_id.clone(),
        state: node_state_to_wire(record.state),
        attempt: record.attempt,
        cost: record.cost.clone(),
        error: record.error.clone(),
        // Warnings are transient history the observer relays live, not a durable node
        // fact — a snapshot/skip view carries none.
        warnings: Vec::new(),
    }
}

/// Reads a durable workflow run's observability snapshot for a `ReadWorkflowRun`
/// command (T9) — the daemon's [`WorkflowReader`] seam over the workflow store on the
/// shared pool. The catch-up baseline a client folds a `Subscription::Workflow` live
/// stream on top of; reconstructed from the store, so a late subscriber after a
/// daemon restart still gets a truthful snapshot.
pub(crate) struct WorkflowRunReader {
    pool: SqlitePool,
}

impl WorkflowRunReader {
    #[must_use]
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl WorkflowReader for WorkflowRunReader {
    fn read(&self, request: ReadWorkflowRunRequest) -> WorkflowReadFuture<'_> {
        let pool = self.pool.clone();
        Box::pin(async move {
            match WorkflowStore::new()
                .snapshot(&pool, &request.workflow_run_id)
                .await
            {
                Ok(Some(snapshot)) => Ok(ProtocolWorkflowRunSnapshot {
                    workflow_run_id: request.workflow_run_id.clone(),
                    phase: run_state_to_wire(snapshot.run.state),
                    nodes: snapshot
                        .nodes
                        .iter()
                        .map(|record| node_record_to_wire(&request.workflow_run_id, record))
                        .collect(),
                }),
                // A run either exists or it does not — unlike a board, whose own board
                // is simply empty. A missing run is a legible rejection.
                Ok(None) => Err(CodypendentError::new(
                    "workflow.run-not-found",
                    format!("no workflow run {}", request.workflow_run_id),
                    false,
                )),
                Err(error) => Err(CodypendentError::new(
                    "workflow.store-error",
                    format!("could not read the workflow run: {error}"),
                    true,
                )),
            }
        })
    }
}

/// Records each node-lifecycle transition of one run in the daemon log (STEP 5.2).
/// Bound to its run id; composed alongside the [`PublishingNodeObserver`] over one
/// drive (T9 — compose, don't replace), so node progress is both logged and streamed.
struct LoggingNodeObserver {
    run_id: String,
}

impl NodeObserver for LoggingNodeObserver {
    fn on_transition(&self, transition: NodeTransition<'_>) {
        info!(
            run = %self.run_id,
            node = %transition.node_id,
            state = transition.state.as_str(),
            attempt = transition.attempt,
            "workflow node transition"
        );
    }

    fn on_budget_warning(&self, node_id: &str, warning: BudgetWarning, attempt: u32) {
        // A budget dimension crossed 80% (the node stayed within budget). Today
        // this lands in the daemon log the way transitions do; the client-facing
        // stream (T9) will carry it to attached clients.
        warn!(
            run = %self.run_id,
            node = %node_id,
            attempt,
            scope = warning.scope.as_str(),
            dimension = warning.dimension.as_str(),
            used = warning.used,
            limit = warning.limit,
            "workflow node budget warning (80%)"
        );
    }

    fn on_attempt_failed(&self, node_id: &str, attempt: u32, error: &str) {
        // The per-attempt history of a retried failure (the durable `error` column
        // keeps only the terminal reason — P5-D4).
        warn!(
            run = %self.run_id,
            node = %node_id,
            attempt,
            %error,
            "workflow node attempt failed (will retry)"
        );
    }

    fn on_recovery_reset(&self, node_id: &str, attempt: u32) {
        info!(
            run = %self.run_id,
            node = %node_id,
            attempt,
            "workflow node reset for recovery/resume"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use codypendent_protocol::ClientId;
    use codypendent_workflow::{NodeContext, NodeOutcome};
    use serde_json::json;
    use std::collections::HashSet;
    use std::time::Duration;

    const MANIFEST: &str = "\
schema_version: 1
id: repair-github-check
version: 1
orchestration_reason: independent-review
budget:
  maximum_agents: 2
steps:
  - id: inspect
    agent:
      role: investigator
    outputs: [finding]
  - id: verify
    depends_on: [inspect]
    tool: repository.test
    outputs: [test_result]
";

    /// A fake leaf executor that completes every node unless told to fail one, so
    /// the host's scheduling/recovery/lifecycle path is exercised without a model.
    #[derive(Default)]
    struct FakeExecutor {
        fail: HashSet<String>,
    }

    impl FakeExecutor {
        fn failing(node: &str) -> Self {
            let mut fail = HashSet::new();
            fail.insert(node.to_owned());
            Self { fail }
        }
    }

    #[async_trait]
    impl NodeExecutor for FakeExecutor {
        async fn execute(&self, ctx: NodeContext<'_>) -> NodeOutcome {
            if self.fail.contains(&ctx.node.id) {
                NodeOutcome::failed("boom")
            } else {
                NodeOutcome::completed()
            }
        }
    }

    async fn temp_pool() -> (tempfile::TempDir, SqlitePool) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = codypendent_workflow::db::open(&tmp.path().join("codypendent.db"))
            .await
            .unwrap();
        (tmp, pool)
    }

    fn host_with(pool: SqlitePool, executor: FakeExecutor) -> WorkflowConductorHost<FakeExecutor> {
        WorkflowConductorHost::new(pool, Arc::new(executor))
    }

    fn start_request(key: &str) -> StartWorkflowRequest {
        StartWorkflowRequest {
            manifest: MANIFEST.to_owned(),
            inputs: json!({ "pull_request": 7 }),
            idempotency_key: key.to_owned(),
            repository: None,
            client_id: ClientId::new(),
        }
    }

    /// Poll a run's state until it reaches `target` or the attempts run out — the
    /// drive is spawned fire-and-forget, so a test waits for it to land.
    async fn wait_for_state(pool: &SqlitePool, run_id: &str, target: WorkflowRunState) {
        for _ in 0..200 {
            let snap = WorkflowStore::new()
                .snapshot(pool, run_id)
                .await
                .unwrap()
                .unwrap();
            if snap.run.state == target {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let snap = WorkflowStore::new()
            .snapshot(pool, run_id)
            .await
            .unwrap()
            .unwrap();
        panic!(
            "run never reached {target:?}; last state {:?}",
            snap.run.state
        );
    }

    #[tokio::test]
    async fn start_creates_and_drives_a_run_to_completion() {
        let (_tmp, pool) = temp_pool().await;
        let host = host_with(pool.clone(), FakeExecutor::default());

        let run_id = host.start(start_request("cmd-1")).await.expect("start");

        // The drive is backgrounded; wait for it to complete the run.
        wait_for_state(&pool, &run_id, WorkflowRunState::Completed).await;
        let snap = WorkflowStore::new()
            .snapshot(&pool, &run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(snap.nodes.iter().all(|n| n.state == NodeState::Completed));
    }

    #[tokio::test]
    async fn a_duplicate_start_resolves_to_the_same_run_without_a_second_drive() {
        let (_tmp, pool) = temp_pool().await;
        let host = host_with(pool.clone(), FakeExecutor::default());

        let first = host.start(start_request("cmd-dup")).await.unwrap();
        wait_for_state(&pool, &first, WorkflowRunState::Completed).await;
        // A duplicate delivery (same key) returns the same run and does not re-drive
        // the already-completed run (is_fresh is false; a stray drive would also be
        // a terminal no-op).
        let second = host.start(start_request("cmd-dup")).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(
            WorkflowStore::new()
                .list_incomplete_runs(&pool)
                .await
                .unwrap()
                .len(),
            0,
            "the run stays completed, not resurrected to running"
        );
    }

    #[tokio::test]
    async fn start_rejects_an_uncompilable_manifest() {
        let (_tmp, pool) = temp_pool().await;
        let host = host_with(pool, FakeExecutor::default());
        let error = host
            .start(StartWorkflowRequest {
                manifest: "schema_version: 1\nid: empty\nversion: 1\nsteps: []\n".to_owned(),
                inputs: json!(null),
                idempotency_key: "cmd-bad".to_owned(),
                repository: None,
                client_id: ClientId::new(),
            })
            .await
            .expect_err("uncompilable manifest is rejected");
        assert_eq!(error.code, "workflow.invalid-manifest");
    }

    #[tokio::test]
    async fn a_failing_node_fails_the_run_then_retry_from_node_completes_it() {
        let (_tmp, pool) = temp_pool().await;
        // `verify` fails on the first drive.
        let host = host_with(pool.clone(), FakeExecutor::failing("verify"));
        let run_id = host.start(start_request("cmd-retry")).await.unwrap();
        wait_for_state(&pool, &run_id, WorkflowRunState::Failed).await;

        // Retry from `verify` with a host whose executor no longer fails it.
        let good = host_with(pool.clone(), FakeExecutor::default());
        good.retry_node(RetryWorkflowNodeRequest {
            workflow_run_id: run_id.clone(),
            node_id: "verify".to_owned(),
            client_id: ClientId::new(),
        })
        .await
        .expect("retry accepted");
        wait_for_state(&pool, &run_id, WorkflowRunState::Completed).await;
    }

    #[tokio::test]
    async fn pause_is_rejected_on_a_completed_run_and_resume_requires_a_paused_run() {
        let (_tmp, pool) = temp_pool().await;
        let host = host_with(pool.clone(), FakeExecutor::default());
        let run_id = host.start(start_request("cmd-life")).await.unwrap();
        wait_for_state(&pool, &run_id, WorkflowRunState::Completed).await;

        // Pausing a completed run is an illegal transition.
        let pause_err = host
            .pause(PauseWorkflowRequest {
                workflow_run_id: run_id.clone(),
                client_id: ClientId::new(),
            })
            .await
            .expect_err("cannot pause a completed run");
        assert_eq!(pause_err.code, "workflow.illegal-transition");

        // Resuming a run that is not paused is likewise illegal.
        let resume_err = host
            .resume(ResumeWorkflowRequest {
                workflow_run_id: run_id.clone(),
                client_id: ClientId::new(),
            })
            .await
            .expect_err("cannot resume a completed run");
        assert_eq!(resume_err.code, "workflow.illegal-transition");
    }

    #[tokio::test]
    async fn cancel_a_running_run_skips_pending_nodes_and_rejects_a_later_resume() {
        // A running run (its `inspect` node held in flight, `verify` still pending) is
        // cancelled: the run lands `Cancelled`, the pending `verify` becomes
        // `Skipped`, and a subsequent resume is rejected (Cancelled is terminal). This
        // is the daemon-seam drain (T9); the interrupted-agent-run half is proven at
        // the `AgentLoopNodeExecutor` level (workflow_exec tests).
        let (_tmp, pool) = temp_pool().await;
        let gate = Arc::new(tokio::sync::Notify::new());
        let executor = Arc::new(GatedExecutor::new("inspect", gate.clone()));
        let host = WorkflowConductorHost::new(pool.clone(), executor.clone());

        let run_id = host.start(start_request("cmd-cancel")).await.unwrap();
        wait_for_node_state(&pool, &run_id, "inspect", NodeState::Running).await;

        // Cancel while `inspect` is in flight and `verify` is still pending.
        host.cancel(CancelWorkflowRequest {
            workflow_run_id: run_id.clone(),
            client_id: ClientId::new(),
        })
        .await
        .expect("cancel accepted");

        // The run is Cancelled and the pending node is Skipped immediately (cancel is
        // synchronous; the in-flight node drains in the background).
        let snap = WorkflowStore::new()
            .snapshot(&pool, &run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snap.run.state, WorkflowRunState::Cancelled);
        assert_eq!(
            snap.nodes
                .iter()
                .find(|n| n.node_id == "verify")
                .unwrap()
                .state,
            NodeState::Skipped,
            "the still-pending node was skipped"
        );

        // Resuming a cancelled (terminal) run is rejected — no resume from Cancelled.
        let resume_err = host
            .resume(ResumeWorkflowRequest {
                workflow_run_id: run_id.clone(),
                client_id: ClientId::new(),
            })
            .await
            .expect_err("cannot resume a cancelled run");
        assert_eq!(resume_err.code, "workflow.illegal-transition");

        // Let the gated in-flight node drain so the drive task winds down cleanly.
        gate.notify_one();
        wait_for_node_state(&pool, &run_id, "inspect", NodeState::Completed).await;
        // The run stays Cancelled after the drive drains (never resurrected).
        assert_eq!(
            WorkflowStore::new()
                .snapshot(&pool, &run_id)
                .await
                .unwrap()
                .unwrap()
                .run
                .state,
            WorkflowRunState::Cancelled
        );
    }

    #[tokio::test]
    async fn a_mid_run_subscriber_gets_a_snapshot_then_the_subsequent_transitions() {
        // T9 catch-up/idempotency: a client subscribing MID-RUN reads the run
        // snapshot as its baseline, then folds the live node transitions on top.
        // Folding snapshot + subsequent transitions reconstructs the full run with no
        // gaps (verify's transitions arrive live) and no harmful duplicates (each node
        // ends in exactly one state).
        let (_tmp, pool) = temp_pool().await;
        let hub = WorkflowHub::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        let executor = Arc::new(GatedExecutor::new("inspect", gate.clone()));
        // Share the hub (its drive publishes transitions here) via `with_streaming`.
        let host = WorkflowConductorHost::new(pool.clone(), executor.clone())
            .with_streaming(hub.clone(), WorkflowRunCancellations::default());

        let run_id = host.start(start_request("cmd-sub")).await.unwrap();
        wait_for_node_state(&pool, &run_id, "inspect", NodeState::Running).await;

        // Subscribe mid-run (inspect running, verify pending), then read the snapshot
        // baseline over the reader seam — the same order a watch client uses.
        let mut rx = hub.subscribe(run_id.clone());
        let reader = WorkflowRunReader::new(pool.clone());
        let snapshot = reader
            .read(ReadWorkflowRunRequest {
                workflow_run_id: run_id.clone(),
                client_id: ClientId::new(),
            })
            .await
            .expect("snapshot read");
        assert_eq!(snapshot.phase, WorkflowRunPhase::Running);
        // Seed a folded view from the baseline.
        let mut folded: HashMap<String, WorkflowNodeState> = snapshot
            .nodes
            .iter()
            .map(|n| (n.node_id.clone(), n.state))
            .collect();
        assert_eq!(folded.get("inspect"), Some(&WorkflowNodeState::Running));
        assert_eq!(folded.get("verify"), Some(&WorkflowNodeState::Pending));

        // Release the gated node — the run drives to completion.
        gate.notify_one();
        wait_for_state(&pool, &run_id, WorkflowRunState::Completed).await;

        // Fold the live stream on top of the baseline until the run-completed phase.
        let mut run_completed = false;
        while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            match event {
                WorkflowEvent::NodeTransitioned(view) => {
                    folded.insert(view.node_id, view.state);
                }
                WorkflowEvent::RunPhaseChanged {
                    phase: WorkflowRunPhase::Completed,
                    ..
                } => {
                    run_completed = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(
            run_completed,
            "the run-completed phase reached the subscriber"
        );
        // Snapshot + subsequent transitions ⇒ every node Completed (no gaps).
        assert_eq!(folded.get("inspect"), Some(&WorkflowNodeState::Completed));
        assert_eq!(folded.get("verify"), Some(&WorkflowNodeState::Completed));
    }

    #[tokio::test]
    async fn a_late_subscriber_after_a_restart_gets_a_truthful_snapshot() {
        // T9 durability: the node records + transitions ARE the record — a fresh
        // reader (standing in for a subscriber attaching after a daemon restart) reads
        // a truthful snapshot straight from the durable store, no in-memory state
        // needed. Here a run is driven to completion, then a brand-new reader
        // reconstructs its terminal snapshot.
        let (_tmp, pool) = temp_pool().await;
        let host = host_with(pool.clone(), FakeExecutor::default());
        let run_id = host.start(start_request("cmd-restart")).await.unwrap();
        wait_for_state(&pool, &run_id, WorkflowRunState::Completed).await;

        // A FRESH reader over the same durable pool (as a restarted daemon would build)
        // reconstructs the run's terminal snapshot.
        let reader = WorkflowRunReader::new(pool.clone());
        let snapshot = reader
            .read(ReadWorkflowRunRequest {
                workflow_run_id: run_id.clone(),
                client_id: ClientId::new(),
            })
            .await
            .expect("snapshot read");
        assert_eq!(snapshot.phase, WorkflowRunPhase::Completed);
        assert!(
            snapshot
                .nodes
                .iter()
                .all(|n| n.state == WorkflowNodeState::Completed),
            "every node's terminal state is reconstructed from the store"
        );

        // An unknown run is a legible rejection, not an empty snapshot.
        let err = reader
            .read(ReadWorkflowRunRequest {
                workflow_run_id: "wfrun-nope".to_owned(),
                client_id: ClientId::new(),
            })
            .await
            .expect_err("unknown run rejected");
        assert_eq!(err.code, "workflow.run-not-found");
    }

    #[tokio::test]
    async fn recover_drives_a_pending_run_left_by_a_crash() {
        let (_tmp, pool) = temp_pool().await;
        // Create a run WITHOUT driving it (a crash between create and the drive
        // spawn), by using the store directly with the manifest recorded.
        let compiled = compile_yaml(MANIFEST).unwrap();
        let run_id = WorkflowStore::new()
            .create_run(&pool, &compiled, None, &json!({}), Some(MANIFEST))
            .await
            .unwrap();

        // A fresh host recovers it and drives it to completion.
        let host = host_with(pool.clone(), FakeExecutor::default());
        let spawned = host.recover().await.unwrap();
        assert_eq!(spawned, 1);
        wait_for_state(&pool, &run_id, WorkflowRunState::Completed).await;
    }

    #[tokio::test]
    async fn retry_on_an_unknown_run_is_a_not_found_rejection() {
        let (_tmp, pool) = temp_pool().await;
        let host = host_with(pool, FakeExecutor::default());
        let error = host
            .retry_node(RetryWorkflowNodeRequest {
                workflow_run_id: "wfrun-missing".to_owned(),
                node_id: "verify".to_owned(),
                client_id: ClientId::new(),
            })
            .await
            .expect_err("unknown run rejected");
        // P5-D6a: a run that never existed is `not-found`, distinct from a run
        // that exists but has no stored manifest (`workflow.no-manifest`).
        assert_eq!(error.code, "workflow.not-found");
    }

    #[tokio::test]
    async fn start_rejects_the_same_idempotency_key_reused_for_a_different_manifest() {
        // P5-D2: a client reusing an idempotency key for a DIFFERENT manifest
        // must be rejected, not silently resolved to the first run's id.
        let (_tmp, pool) = temp_pool().await;
        let host = host_with(pool.clone(), FakeExecutor::default());

        let first = host.start(start_request("cmd-shared-key")).await.unwrap();
        wait_for_state(&pool, &first, WorkflowRunState::Completed).await;

        // The SAME idempotency key, but a manifest with an extra step (a
        // different graph signature).
        let different_manifest = format!(
            "{MANIFEST}  - id: extra\n    depends_on: [verify]\n    tool: repository.test\n"
        );
        let error = host
            .start(StartWorkflowRequest {
                manifest: different_manifest,
                inputs: json!({ "pull_request": 7 }),
                idempotency_key: "cmd-shared-key".to_owned(),
                repository: None,
                client_id: ClientId::new(),
            })
            .await
            .expect_err("a reused key with a different manifest must be rejected");
        assert_eq!(error.code, "workflow.idempotency-mismatch");

        // Only the first run exists.
        assert_eq!(
            WorkflowStore::new()
                .list_incomplete_runs(&pool)
                .await
                .unwrap()
                .len(),
            0,
            "the rejected duplicate created no second (incomplete) run"
        );
    }

    #[tokio::test]
    async fn reconfiguring_the_host_shares_the_drive_lock_registry() {
        // P5-D6c: a builder that reconfigures the host (e.g.
        // `RuntimeExecutor::with_github`, which swaps the node executor after
        // startup) must carry the SAME per-run drive-lock registry into the
        // replacement — rebuilding via `new` would mint a fresh, disconnected
        // registry, so a drive in flight under the OLD host would not
        // serialize against the NEW host's drives for the same run id.
        let (_tmp, pool) = temp_pool().await;
        let original = host_with(pool.clone(), FakeExecutor::default());
        let shared_locks = original.drive_locks();

        let reconfigured = WorkflowConductorHost::with_drive_locks(
            pool,
            Arc::new(FakeExecutor::default()),
            shared_locks,
        );

        let lock_from_original = original.run_lock("run-x");
        let lock_from_reconfigured = reconfigured.run_lock("run-x");
        assert!(
            Arc::ptr_eq(&lock_from_original, &lock_from_reconfigured),
            "the reconfigured host must reuse the SAME per-run lock, not mint a fresh one"
        );
    }

    /// An executor that blocks on `gate` the FIRST time it runs `gated_node`,
    /// so a test can hold that node "in flight" (its store row `Running`, the
    /// coroutine suspended inside `execute`) for as long as it wants — the
    /// window P5-D3's race needs. Every subsequent call to `gated_node`
    /// (e.g. after a retry re-drives it) completes immediately. Records every
    /// node it ran (including repeats across drives).
    struct GatedExecutor {
        gated_node: String,
        gate: Arc<tokio::sync::Notify>,
        gated_once: std::sync::atomic::AtomicBool,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl GatedExecutor {
        fn new(gated_node: &str, gate: Arc<tokio::sync::Notify>) -> Self {
            Self {
                gated_node: gated_node.to_owned(),
                gate,
                gated_once: std::sync::atomic::AtomicBool::new(false),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn calls_for(&self, node: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|id| *id == node)
                .count()
        }
    }

    #[async_trait]
    impl NodeExecutor for GatedExecutor {
        async fn execute(&self, ctx: NodeContext<'_>) -> NodeOutcome {
            self.calls.lock().unwrap().push(ctx.node.id.clone());
            if ctx.node.id == self.gated_node
                && !self
                    .gated_once
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                self.gate.notified().await;
            }
            NodeOutcome::completed()
        }
    }

    /// Poll until `node_id`'s store row reaches `target`, or panic — mirrors
    /// [`wait_for_state`] but at node granularity.
    async fn wait_for_node_state(
        pool: &SqlitePool,
        run_id: &str,
        node_id: &str,
        target: NodeState,
    ) {
        for _ in 0..200 {
            let snap = WorkflowStore::new()
                .snapshot(pool, run_id)
                .await
                .unwrap()
                .unwrap();
            if snap
                .nodes
                .iter()
                .find(|n| n.node_id == node_id)
                .map(|n| n.state)
                == Some(target)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("node {node_id} never reached {target:?}");
    }

    #[tokio::test]
    async fn retry_issued_during_a_live_drive_is_not_silently_lost() {
        // P5-D3: retrying a node while its run is ACTIVELY driving must not be
        // silently discarded by the in-flight executor's own completion write
        // overwriting the reset. `inspect` is held mid-execution (its store row
        // is `Running`, the executor coroutine suspended) while the retry is
        // issued; the retry must survive — it takes hold once the current
        // drive completes, and a fresh drive re-runs `inspect` (and its
        // downstream `verify`) rather than leaving the run `Completed` from
        // before the retry was ever asked for.
        let (_tmp, pool) = temp_pool().await;
        let gate = Arc::new(tokio::sync::Notify::new());
        let executor = Arc::new(GatedExecutor::new("inspect", gate.clone()));
        let host = WorkflowConductorHost::new(pool.clone(), executor.clone());

        let run_id = host.start(start_request("cmd-race")).await.unwrap();
        wait_for_node_state(&pool, &run_id, "inspect", NodeState::Running).await;

        // Issue the retry WHILE `inspect` is still gated — the drive holds the
        // per-run lock the entire time it is blocked inside `execute`, so
        // without the fix this would mutate the node rows immediately,
        // racing the in-flight completion write.
        let retry = tokio::spawn({
            let host = host.clone();
            let run_id = run_id.clone();
            async move {
                host.retry_node(RetryWorkflowNodeRequest {
                    workflow_run_id: run_id,
                    node_id: "inspect".to_owned(),
                    client_id: ClientId::new(),
                })
                .await
            }
        });

        // Let the gated drive proceed to completion.
        gate.notify_one();
        wait_for_state(&pool, &run_id, WorkflowRunState::Completed).await;

        // The retry, having waited for the SAME per-run lock, now applies and
        // drives again.
        retry
            .await
            .expect("retry task did not panic")
            .expect("retry accepted");
        wait_for_state(&pool, &run_id, WorkflowRunState::Completed).await;

        assert_eq!(
            executor.calls_for("inspect"),
            2,
            "inspect re-executed after the retry — the reset was not lost"
        );
        assert_eq!(
            executor.calls_for("verify"),
            2,
            "verify (inspect's downstream) also re-executed"
        );
    }
}
