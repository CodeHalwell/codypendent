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

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use codypendent_daemon::workflow_stream::{
    ReadWorkflowRunRequest, WorkflowHub, WorkflowReadFuture, WorkflowReader,
};
use codypendent_daemon::workflows::{
    CancelWorkflowRequest, PauseWorkflowRequest, ResumeWorkflowRequest, RetryWorkflowNodeRequest,
    StartWorkflowRequest, WorkflowCatalogFuture, WorkflowCatalogRequest, WorkflowLifecycle,
    WorkflowLifecycleFuture, WorkflowRunCeiling, WorkflowStartFuture, WorkflowStarter,
};
use codypendent_protocol::{
    CodypendentError, WorkflowCatalog, WorkflowCatalogDiagnostic, WorkflowCatalogEntry,
    WorkflowCatalogInput, WorkflowCatalogSource, WorkflowEvent, WorkflowNodeState,
    WorkflowNodeView, WorkflowRunPhase, WorkflowRunSnapshot as ProtocolWorkflowRunSnapshot,
};
use codypendent_runtime::blackboard::{
    BlackboardChannelError, WorkflowQueryChannel, WorkflowRunSummary,
};
use codypendent_runtime::workflow_control::{
    WorkflowAgentDraft, WorkflowApprovalDraft, WorkflowControlChannel, WorkflowControlError,
    WorkflowCreateRequest, WorkflowCreated, WorkflowDraft, WorkflowInputDraft,
    WorkflowOrchestrationReasonDraft, WorkflowRetryDraft, WorkflowRunRequest, WorkflowRunTarget,
    WorkflowStarted, WorkflowStepDraft, WorkflowWorkspaceDraft,
};
use codypendent_workflow::{
    compile_yaml, parse_definition, AgentRef, ApprovalPolicy, BudgetWarning, ConductorError,
    NodeExecutor, NodeObserver, NodeState, NodeTransition, OrchestrationReason, RetryPolicy,
    WorkflowBudget, WorkflowConductor, WorkflowDefinition, WorkflowInput, WorkflowNodeRecord,
    WorkflowRunAttribution, WorkflowRunState, WorkflowSourceRegistry, WorkflowStep, WorkflowStore,
    WorkflowStoreError, WorkspaceMode, WorkspaceSpec,
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
    /// The per-user workflow directory (the daemon points this at
    /// `<data_dir>/workflows`), consulted when a `StartWorkflow` names a workflow
    /// by id (`/fix-ci`) instead of shipping a manifest — see
    /// [`resolve_named_workflow`](Self::resolve_named_workflow). `None` in a test
    /// host and on any host that only ever runs inline manifests; the embedded
    /// built-in is always available regardless (STEP 5.1.4).
    user_workflow_dir: Option<PathBuf>,
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
            user_workflow_dir: self.user_workflow_dir.clone(),
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
            // No user-scope source by default; the embedded built-in still
            // resolves. Production sets the real directory via
            // [`with_workflow_source_dir`](Self::with_workflow_source_dir).
            user_workflow_dir: None,
        }
    }

    /// Point the host at the per-user workflow directory a `StartWorkflow`-by-id
    /// consults (below the built-in, above nothing). Production wires
    /// `<data_dir>/workflows`; a test host leaves it `None` (the built-in still
    /// resolves). Builder-style so the assembly's `build_workflow_host` sets it
    /// after `with_streaming`.
    #[must_use]
    pub(crate) fn with_workflow_source_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.user_workflow_dir = dir;
        self
    }

    /// Resolve a `StartWorkflow`'s named workflow (`/fix-ci`'s `repair-github-check`)
    /// to its manifest text from the assembly's sources: the embedded built-in,
    /// the per-user directory, and the run repository's `.codypendent/workflows`
    /// (highest precedence). Enforces the registry's version-stability +
    /// shadowing rules (STEP 5.1.3/5.1.4), surfacing a collision or an unknown id
    /// as a legible, non-retryable [`CodypendentError`].
    fn resolve_named_workflow(
        &self,
        workflow_id: &str,
        repository: Option<&str>,
    ) -> Result<String, CodypendentError> {
        let repo_dir =
            repository.map(|repo| Path::new(repo).join(".codypendent").join("workflows"));
        let registry =
            WorkflowSourceRegistry::load(self.user_workflow_dir.as_deref(), repo_dir.as_deref());
        registry
            .resolve(workflow_id)
            .map(str::to_string)
            .map_err(|error| CodypendentError::new(error.code(), error.to_string(), false))
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
        // A crash can land after terminal workflow persistence but before the
        // derived child-session archival. Reconcile those terminal runs first;
        // the update is idempotent and only touches terminal child runs.
        let terminal_runs: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM workflow_runs WHERE state IN ('completed', 'failed', 'cancelled') \
             ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(WorkflowStoreError::Database)?;
        for (run_id,) in terminal_runs {
            self.archive_internal_sessions(&run_id).await;
        }
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
                    Ok(Some(snapshot)) if snapshot.run.state.is_terminal() => {
                        host.archive_internal_sessions(&run_id).await;
                        return;
                    }
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
                // Bracket the drive as live (paired with `finish` below): while it is
                // open the run's cancellation entry is kept even at zero in-flight
                // handles, so a `CancelWorkflow` landing while a tool node is parked but
                // has not yet registered its token still sticks and wakes the park
                // (MF-1). A run cancelled before this point already flipped to a
                // terminal state, so `drive` short-circuits without running a node.
                host.cancellations.begin(&run_id);
                match host
                    .conductor
                    .drive(&host.pool, &run_id, host.node_executor.as_ref(), &observer)
                    .await
                {
                    Ok(state) => {
                        // Announce the stopping phase (Completed / Failed / Paused /
                        // Cancelled) to subscribers.
                        host.publish_run_phase(&run_id, state);
                        if state.is_terminal() {
                            host.archive_internal_sessions(&run_id).await;
                        }
                        info!(run = %run_id, state = state.as_str(), "workflow run driven to a stopping state")
                    }
                    Err(error) => {
                        // A stored manifest which is missing, no longer
                        // compiles, or compiles to a changed graph can never
                        // make progress on a later retry. End it durably rather
                        // than logging-and-skipping it on every daemon boot.
                        let unrecoverable = matches!(
                            error,
                            ConductorError::NoManifest(_)
                                | ConductorError::Compile(_)
                                | ConductorError::Store(
                                    WorkflowStoreError::GraphSignatureChanged { .. }
                                )
                        );
                        if unrecoverable {
                            match WorkflowStore::new()
                                .set_run_state(&host.pool, &run_id, WorkflowRunState::Failed)
                                .await
                            {
                                Ok(()) => host.publish_run_phase(&run_id, WorkflowRunState::Failed),
                                Err(store_error) => warn!(
                                    run = %run_id,
                                    %store_error,
                                    "could not mark unrecoverable workflow failed"
                                ),
                            }
                        }
                        if unrecoverable {
                            host.archive_internal_sessions(&run_id).await;
                        }
                        warn!(run = %run_id, %error, "workflow run drive ended in error")
                    }
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

    /// Archive node sessions only after the workflow's terminal state has been
    /// durably written. The parent run id persisted on each child makes this
    /// idempotent and lets startup recovery finish cleanup after a crash.
    async fn archive_internal_sessions(&self, workflow_run_id: &str) {
        let archived_at = chrono::Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE sessions SET archived_at = COALESCE(archived_at, ?) \
             WHERE internal = 1 AND id IN (SELECT r.session_id FROM workflow_nodes n \
                 JOIN runs r ON r.id = n.agent_run_id WHERE n.workflow_run_id = ? \
                 AND r.state IN ('Completed', 'Failed', 'Cancelled')) \
             AND EXISTS (SELECT 1 FROM workflow_runs \
                 WHERE id = ? AND state IN ('completed', 'failed', 'cancelled'))",
        )
        .bind(archived_at)
        .bind(workflow_run_id)
        .bind(workflow_run_id)
        .execute(&self.pool)
        .await;
        if let Err(error) = result {
            warn!(run = %workflow_run_id, %error, "could not archive internal workflow sessions");
        }
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
    fn catalog(&self, request: WorkflowCatalogRequest) -> WorkflowCatalogFuture<'_> {
        let user_dir = self.user_workflow_dir.clone();
        Box::pin(async move {
            // Filesystem resolution stays on the daemon and off the async reactor.
            tokio::task::spawn_blocking(move || {
                let repository = request
                    .repository
                    .map(|repository| {
                        let path = Path::new(&repository);
                        if repository.len() > 4096 || !path.is_absolute() {
                            return Err(CodypendentError::new(
                                "workflow.invalid-repository",
                                "select an absolute repository path",
                                false,
                            ));
                        }
                        let canonical = path.canonicalize().map_err(|_| {
                            CodypendentError::new(
                                "workflow.invalid-repository",
                                "the selected repository is unavailable",
                                false,
                            )
                        })?;
                        if !canonical.is_dir() {
                            return Err(CodypendentError::new(
                                "workflow.invalid-repository",
                                "select a repository directory",
                                false,
                            ));
                        }
                        canonical.to_str().map(str::to_owned).ok_or_else(|| {
                            CodypendentError::new(
                                "workflow.invalid-repository",
                                "the repository path is not valid UTF-8",
                                false,
                            )
                        })
                    })
                    .transpose()?;
                let repo_dir = repository
                    .as_ref()
                    .map(|root| Path::new(root).join(".codypendent").join("workflows"));
                let catalog =
                    WorkflowSourceRegistry::catalog(user_dir.as_deref(), repo_dir.as_deref());
                Ok(WorkflowCatalog {
                    repository,
                    items: catalog
                        .entries
                        .into_iter()
                        .map(|entry| WorkflowCatalogEntry {
                            definition_digest: entry.definition_digest,
                            id: entry.id,
                            version: entry.version,
                            description: entry.description,
                            inputs: entry
                                .inputs
                                .into_iter()
                                .map(|(name, input)| {
                                    (
                                        name,
                                        WorkflowCatalogInput {
                                            input_type: input.input_type,
                                            required: input.required,
                                        },
                                    )
                                })
                                .collect(),
                            node_count: entry.node_count as u32,
                            source: match entry.source {
                                codypendent_workflow::WorkflowScope::BuiltIn => {
                                    WorkflowCatalogSource::BuiltIn
                                }
                                codypendent_workflow::WorkflowScope::User => {
                                    WorkflowCatalogSource::User
                                }
                                codypendent_workflow::WorkflowScope::Repository => {
                                    WorkflowCatalogSource::Repository
                                }
                            },
                        })
                        .collect(),
                    diagnostics: catalog
                        .diagnostics
                        .into_iter()
                        .map(|code| WorkflowCatalogDiagnostic {
                            message: catalog_diagnostic_message(&code).to_owned(),
                            code,
                            workflow_id: None,
                        })
                        .collect(),
                    truncated: catalog.truncated,
                })
            })
            .await
            .map_err(|_| {
                CodypendentError::new(
                    "workflow.catalog-unavailable",
                    "workflow discovery could not be completed",
                    true,
                )
            })?
        })
    }

    fn start(&self, request: StartWorkflowRequest) -> WorkflowStartFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            let StartWorkflowRequest {
                expected_definition_digest,
                manifest,
                workflow_id,
                inputs,
                idempotency_key,
                repository,
                owner_uid,
                budget_ceiling,
                ..
            } = request;

            // A retry of an accepted, definition-bound request recovers the original
            // receipt before mutable workflow sources are read again. Reusing a key
            // with changed inputs, repository, owner, or definition never starts work.
            if let Some(expected) = expected_definition_digest.as_deref() {
                if expected.len() != 64
                    || !expected
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(CodypendentError::new(
                        "workflow.invalid-definition-digest",
                        "select a workflow from the current catalog",
                        false,
                    ));
                }
                if let Some(accepted) = WorkflowStore::new()
                    .accepted_request_owned(&host.pool, &idempotency_key, owner_uid)
                    .await
                    .map_err(|_| {
                        CodypendentError::new(
                            "workflow.store-error",
                            "could not reconcile the workflow request",
                            true,
                        )
                    })?
                {
                    let same_definition = accepted
                        .manifest
                        .as_deref()
                        .and_then(|manifest| parse_definition(manifest).ok())
                        .and_then(|definition| {
                            codypendent_workflow::workflow_definition_digest(&definition).ok()
                        })
                        .is_some_and(|digest| digest == expected);
                    if !same_definition
                        || accepted.inputs != inputs
                        || accepted.repository != repository
                        || workflow_id
                            .as_ref()
                            .is_some_and(|id| id != &accepted.workflow_id)
                    {
                        return Err(CodypendentError::new(
                            "workflow.idempotency-mismatch",
                            "this request key belongs to a different workflow request",
                            false,
                        ));
                    }
                    if host.is_fresh(&accepted.id).await {
                        host.spawn_drive(accepted.id.clone());
                    }
                    return Ok(accepted.id);
                }
            }

            // A named workflow (`/fix-ci`'s `repair-github-check`) is resolved from
            // the assembly's sources (built-in / user / repository), enforcing the
            // registry's version + shadowing rules; an inline manifest is taken
            // verbatim. The rest of the flow is identical — the resolved text is a
            // manifest like any other.
            let manifest = match workflow_id.as_deref() {
                Some(id) => host.resolve_named_workflow(id, repository.as_deref())?,
                None => manifest,
            };

            // Compare against these exact resolved bytes, which are subsequently
            // compiled and persisted. There is no second source read after the guard.
            if let Some(expected) = expected_definition_digest.as_deref() {
                let actual = parse_definition(&manifest).ok().and_then(|definition| {
                    codypendent_workflow::workflow_definition_digest(&definition).ok()
                });
                if actual.as_deref() != Some(expected) {
                    return Err(CodypendentError::new("workflow.definition-changed", "the workflow definition changed; refresh the catalog and review it before starting", false));
                }
            }

            // Apply any externally imposed ceiling to the manifest TEXT before
            // compiling, so the tightened envelope is what gets compiled AND what
            // gets stored. Tightening only the in-memory graph would be dropped the
            // moment the daemon restarted, because a node re-reads the envelope by
            // recompiling `workflow_runs.manifest_yaml`.
            let manifest = match budget_ceiling {
                Some(ceiling) if ceiling.declares_any() => {
                    apply_budget_ceiling(&manifest, ceiling)?
                }
                _ => manifest,
            };

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
                .create_run_idempotent_owned(
                    &host.pool,
                    &compiled,
                    &idempotency_key,
                    &inputs,
                    WorkflowRunAttribution {
                        manifest: Some(&manifest),
                        // Persist the run's repository so recovery carves each agent
                        // node's isolated worktree from the right checkout (T5).
                        repository: repository.as_deref(),
                        owner_uid,
                    },
                )
                .await
                .map_err(|error| match error {
                    WorkflowStoreError::IdempotencyKeyReused { .. }
                    | WorkflowStoreError::IdempotencyOwnerMismatch { .. } => CodypendentError::new(
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

fn catalog_diagnostic_message(code: &str) -> &'static str {
    match code {
        "workflow.version-collision" => "A workflow version has conflicting definitions; bump the changed definition's version.",
        "workflow.catalog-invalid-manifest" => "An invalid workflow definition was skipped.",
        "workflow.catalog-limit" => "Workflow discovery exceeded a metadata or source limit; no incomplete selection is offered.",
        "workflow.catalog-unsafe-source" => "A workflow source contains unsupported links or files; no incomplete selection is offered.",
        "workflow.catalog-source-unavailable" => "A workflow source could not be inspected; no incomplete selection is offered.",
        _ => "A workflow could not be offered by bounded discovery.",
    }
}

#[async_trait::async_trait]
impl<E: NodeExecutor + 'static> WorkflowControlChannel for WorkflowConductorHost<E> {
    async fn create(
        &self,
        request: WorkflowCreateRequest,
    ) -> Result<WorkflowCreated, WorkflowControlError> {
        let definition = workflow_definition(request.workflow)?;
        let manifest = validated_manifest(&definition)?;
        let directory = self.user_workflow_dir.clone().ok_or_else(|| {
            WorkflowControlError::Backend(
                "the user workflow directory is not configured on this daemon".to_string(),
            )
        })?;
        let workflow_id = definition.id.clone();
        let version = definition.version;
        let handle = persist_user_workflow(&directory, &workflow_id, manifest.as_bytes())?;
        Ok(WorkflowCreated {
            workflow_id,
            version,
            handle: handle.display().to_string(),
        })
    }

    async fn run(
        &self,
        request: WorkflowRunRequest,
    ) -> Result<WorkflowStarted, WorkflowControlError> {
        let WorkflowRunRequest {
            target,
            inputs,
            repository,
            session_id,
            idempotency_key,
        } = request;
        let owner_uid = sqlx::query_scalar::<_, i64>("SELECT owner_uid FROM sessions WHERE id = ?")
            .bind(session_id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| {
                WorkflowControlError::Backend(format!("could not resolve session owner: {error}"))
            })?
            .and_then(|uid| u32::try_from(uid).ok())
            .ok_or_else(|| {
                WorkflowControlError::Backend("the workflow session has no valid owner".to_string())
            })?;
        let (workflow_id, manifest, named_id) = match target {
            WorkflowRunTarget::Named(id) => (id.clone(), String::new(), Some(id)),
            WorkflowRunTarget::Inline(draft) => {
                let definition = workflow_definition(draft)?;
                let id = definition.id.clone();
                (id, validated_manifest(&definition)?, None)
            }
        };
        let workflow_run_id = WorkflowStarter::start(
            self,
            StartWorkflowRequest {
                expected_definition_digest: None,
                manifest,
                workflow_id: named_id,
                inputs,
                idempotency_key,
                repository: Some(repository),
                owner_uid,
                client_id: codypendent_protocol::ClientId::new(),
                // A client-driven run carries no external ceiling: the manifest's
                // own envelope is the only budget in force.
                budget_ceiling: None,
            },
        )
        .await
        .map_err(|error| {
            WorkflowControlError::Backend(format!("{}: {}", error.code, error.message))
        })?;
        Ok(WorkflowStarted {
            workflow_id,
            workflow_run_id,
        })
    }
}

/// Tighten `manifest`'s `budget:` envelope with an externally imposed `ceiling`,
/// returning the manifest to compile **and store**.
///
/// This rewrites the manifest rather than the compiled graph on purpose. The
/// envelope in force at execution is read back by recompiling the run's stored
/// `manifest_yaml` (`AgentLoopNodeExecutor::budget_limits`), and the startup
/// recovery pass rebuilds a run the same way — so a ceiling applied anywhere else
/// would vanish at the first restart, which is exactly the silent overspend the
/// automation dispatch gate refuses to allow.
///
/// An external ceiling may only ever TIGHTEN: the lower of (declared, imposed)
/// wins per dimension, and a dimension the ceiling leaves unset is untouched.
/// The graph signature does not cover the budget envelope
/// (`CompiledWorkflow::signature`), so a tightened manifest still resumes against
/// the run it created.
fn apply_budget_ceiling(
    manifest: &str,
    ceiling: WorkflowRunCeiling,
) -> Result<String, CodypendentError> {
    let mut definition = parse_definition(manifest).map_err(|error| {
        CodypendentError::new(
            "workflow.invalid-manifest",
            format!("workflow manifest does not parse: {error}"),
            false,
        )
    })?;
    if let Some(seconds) = ceiling.max_wall_time_seconds {
        definition.budget.maximum_duration_seconds = Some(
            definition
                .budget
                .maximum_duration_seconds
                .map_or(seconds, |declared| declared.min(seconds)),
        );
    }
    if let Some(micros) = ceiling.max_cost_micros {
        let imposed = micros_to_usd_ceiling(micros);
        definition.budget.maximum_cost_usd = Some(
            definition
                .budget
                .maximum_cost_usd
                .map_or(imposed, |declared| declared.min(imposed)),
        );
    }
    serde_yaml::to_string(&definition).map_err(|error| {
        CodypendentError::new(
            "workflow.invalid-manifest",
            format!("could not re-encode the workflow manifest with its budget ceiling: {error}"),
            false,
        )
    })
}

/// The USD ceiling that carries `micros` without LOOSENING it.
///
/// The manifest envelope is denominated in USD and the enforcer converts it back
/// with `(usd * 1_000_000.0).round()` (`codypendent_workflow::budget`), a round
/// trip that is exact for every value below 2^53 micros (about $9e9). Above that
/// an `f64` could land a fraction ABOVE the requested ceiling, so the result is
/// stepped down by a micro rather than allowed to round up.
fn micros_to_usd_ceiling(micros: u64) -> f64 {
    let requested = micros as f64;
    let usd = requested / 1_000_000.0;
    if (usd * 1_000_000.0).round() > requested {
        return micros.saturating_sub(1) as f64 / 1_000_000.0;
    }
    usd
}

fn validated_manifest(definition: &WorkflowDefinition) -> Result<String, WorkflowControlError> {
    let manifest = serde_yaml::to_string(definition).map_err(|error| {
        WorkflowControlError::Invalid(format!("could not encode workflow manifest: {error}"))
    })?;
    compile_yaml(&manifest).map_err(|error| {
        WorkflowControlError::Invalid(format!("workflow manifest does not compile: {error}"))
    })?;
    Ok(manifest)
}

fn workflow_definition(draft: WorkflowDraft) -> Result<WorkflowDefinition, WorkflowControlError> {
    let inputs = draft
        .inputs
        .into_iter()
        .map(|(name, input)| (name, workflow_input(input)))
        .collect::<BTreeMap<_, _>>();
    let steps = draft
        .steps
        .into_iter()
        .map(workflow_step)
        .collect::<Vec<_>>();
    Ok(WorkflowDefinition {
        schema_version: 1,
        id: draft.id,
        version: draft.version,
        description: draft.description,
        inputs,
        budget: WorkflowBudget {
            maximum_cost_usd: draft.budget.maximum_cost_usd,
            maximum_duration_seconds: draft.budget.maximum_duration_seconds,
            maximum_agents: draft.budget.maximum_agents,
        },
        steps,
        orchestration_reason: draft.orchestration_reason.map(|reason| match reason {
            WorkflowOrchestrationReasonDraft::Parallelism => OrchestrationReason::Parallelism,
            WorkflowOrchestrationReasonDraft::IndependentReview => {
                OrchestrationReason::IndependentReview
            }
            WorkflowOrchestrationReasonDraft::AccessSeparation => {
                OrchestrationReason::AccessSeparation
            }
            WorkflowOrchestrationReasonDraft::Specialist => OrchestrationReason::Specialist,
        }),
    })
}

fn workflow_input(input: WorkflowInputDraft) -> WorkflowInput {
    WorkflowInput {
        input_type: input.input_type,
        required: input.required,
    }
}

fn workflow_step(step: WorkflowStepDraft) -> WorkflowStep {
    WorkflowStep {
        id: step.id,
        depends_on: step.depends_on,
        agent: step.agent.map(workflow_agent),
        tool: step.tool,
        with: step.with,
        skill: step.skill,
        workspace: step.workspace.map(|mode| WorkspaceSpec {
            mode: match mode {
                WorkflowWorkspaceDraft::SharedWorktree => WorkspaceMode::SharedWorktree,
                WorkflowWorkspaceDraft::IsolatedWorktree => WorkspaceMode::IsolatedWorktree,
            },
        }),
        approval: step.approval.map(|approval| match approval {
            WorkflowApprovalDraft::BeforeWrite => ApprovalPolicy::BeforeWrite,
            WorkflowApprovalDraft::Always => ApprovalPolicy::Always,
        }),
        retry: step.retry.map(workflow_retry),
        outputs: step.outputs,
    }
}

fn workflow_agent(agent: WorkflowAgentDraft) -> AgentRef {
    AgentRef {
        role: agent.role,
        model_policy: agent.model_policy,
    }
}

fn workflow_retry(retry: WorkflowRetryDraft) -> RetryPolicy {
    RetryPolicy {
        attempts: retry.attempts,
        backoff_seconds: retry.backoff_seconds,
    }
}

fn persist_user_workflow(
    directory: &Path,
    workflow_id: &str,
    bytes: &[u8],
) -> Result<PathBuf, WorkflowControlError> {
    if workflow_id.is_empty()
        || workflow_id.contains('/')
        || workflow_id.contains('\\')
        || workflow_id.contains("..")
    {
        return Err(WorkflowControlError::Invalid(format!(
            "invalid workflow id `{workflow_id}`: must be a bare identifier"
        )));
    }
    std::fs::create_dir_all(directory).map_err(|error| {
        WorkflowControlError::Backend(format!(
            "could not create workflow directory `{}`: {error}",
            directory.display()
        ))
    })?;
    let target = directory.join(format!("{workflow_id}.yaml"));
    if target.exists() {
        return Err(WorkflowControlError::Conflict(format!(
            "workflow `{workflow_id}` already exists at `{}`",
            target.display()
        )));
    }
    let temporary = directory.join(format!(
        ".{workflow_id}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &target)?;
        if let Ok(directory_file) = std::fs::File::open(directory) {
            let _ = directory_file.sync_all();
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(WorkflowControlError::Backend(format!(
            "could not atomically save workflow `{workflow_id}`: {error}"
        )));
    }
    Ok(target)
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
                .map_err(conductor_error_to_protocol)?;
            // The durable flip is complete before this full-state event is
            // published. Do not wait for an in-flight node to drain before the
            // controller can render Resume instead of Pause.
            host.publish_run_phase(&request.workflow_run_id, WorkflowRunState::Paused);
            Ok(())
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
            // `prepare_resume` has already persisted Running. Publish that prompt
            // phase synchronously; the background drive also announces Running,
            // but the duplicate is an idempotent full-state update.
            host.publish_run_phase(&request.workflow_run_id, WorkflowRunState::Running);
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
                                // A live transition carries no edges — the graph
                                // shape is static per run, so a client keeps the
                                // ones its snapshot read taught it.
                                WorkflowEvent::NodeTransitioned(node_record_to_wire(
                                    &request.workflow_run_id,
                                    record,
                                    &HashMap::new(),
                                )),
                            );
                        }
                    }
                }
            }
            host.publish_run_phase(&request.workflow_run_id, WorkflowRunState::Cancelled);
            host.archive_internal_sessions(&request.workflow_run_id)
                .await;
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
            // A live transition carries no edges: the graph shape is static per
            // run and the callback is sync (it cannot re-read the manifest). A
            // client learns the edges once from `ReadWorkflowRun` and preserves
            // them across live merges.
            depends_on: Vec::new(),
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
/// `edges` maps node id → its `depends_on` list, recovered from the run's stored
/// manifest by [`node_edges`]; empty when the manifest is unavailable, which
/// degrades the client to the flat node list it rendered before edges existed.
fn node_record_to_wire(
    workflow_run_id: &str,
    record: &WorkflowNodeRecord,
    edges: &HashMap<String, Vec<String>>,
) -> WorkflowNodeView {
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
        depends_on: edges.get(&record.node_id).cloned().unwrap_or_default(),
    }
}

/// Recover a run's DAG **edges** (rubric 5) by recompiling the manifest YAML stored
/// with it: node id → the ids it depends on.
///
/// The durable `workflow_nodes` rows carry state, attempt, cost, and topological
/// order but no adjacency — the graph shape lives only in the manifest — so the
/// snapshot read recompiles it here rather than duplicating edges into a table.
/// Every failure mode (no manifest recorded, a manifest that no longer compiles)
/// degrades to **no edges** rather than failing the read: a client that cannot draw
/// arrows still gets the truthful node list it got before.
async fn node_edges(pool: &SqlitePool, workflow_run_id: &str) -> HashMap<String, Vec<String>> {
    let manifest = match WorkflowStore::new().manifest(pool, workflow_run_id).await {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return HashMap::new(),
        Err(error) => {
            warn!(run = %workflow_run_id, %error, "could not read the workflow manifest for edges");
            return HashMap::new();
        }
    };
    match compile_yaml(&manifest) {
        Ok(compiled) => compiled
            .nodes
            .into_iter()
            .map(|node| (node.id, node.depends_on))
            .collect(),
        Err(error) => {
            warn!(run = %workflow_run_id, %error, "stored workflow manifest no longer compiles; the snapshot carries no edges");
            HashMap::new()
        }
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
                Ok(Some(snapshot)) => {
                    // The snapshot is the ONLY place edges ride the wire (a live
                    // transition omits them), so recover them here from the run's
                    // stored manifest — this is what lets a client draw the DAG.
                    let edges = node_edges(&pool, &request.workflow_run_id).await;
                    Ok(ProtocolWorkflowRunSnapshot {
                        workflow_run_id: request.workflow_run_id.clone(),
                        phase: run_state_to_wire(snapshot.run.state),
                        nodes: snapshot
                            .nodes
                            .iter()
                            .map(|record| {
                                node_record_to_wire(&request.workflow_run_id, record, &edges)
                            })
                            .collect(),
                    })
                }
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

/// Implements the runtime's [`WorkflowQueryChannel`] — the agent-facing
/// `workflow.query` tool (rubric 5) — over the same store the `ReadWorkflowRun`
/// command reads.
///
/// It deliberately reuses [`WorkflowRunReader`] rather than re-projecting: the
/// graph an agent sees (states, edges, costs) is then *by construction* the same
/// graph a human sees in the TUI, so the two surfaces cannot drift. Cheap to
/// clone.
#[derive(Clone)]
pub struct AssemblyWorkflowQuery {
    pool: SqlitePool,
}

impl AssemblyWorkflowQuery {
    /// Build the channel over the daemon's pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl WorkflowQueryChannel for AssemblyWorkflowQuery {
    async fn snapshot(
        &self,
        workflow_run_id: &str,
    ) -> Result<Option<ProtocolWorkflowRunSnapshot>, BlackboardChannelError> {
        let reader = WorkflowRunReader::new(self.pool.clone());
        match reader
            .read(ReadWorkflowRunRequest {
                workflow_run_id: workflow_run_id.to_string(),
                // The agent's own client identity is not meaningful here — this is
                // a read on behalf of a run, not a connection.
                client_id: codypendent_protocol::ClientId::new(),
            })
            .await
        {
            Ok(snapshot) => Ok(Some(snapshot)),
            // "No such run" is not a failure of the tool: the agent named an id
            // that does not exist, and the tool layer says so plainly.
            Err(error) if error.code == "workflow.run-not-found" => Ok(None),
            Err(error) => Err(BlackboardChannelError::Backend(error.message)),
        }
    }

    async fn recent_runs(
        &self,
        repository: &str,
        limit: u32,
    ) -> Result<Vec<WorkflowRunSummary>, BlackboardChannelError> {
        WorkflowStore::new()
            .list_runs_for_repository(&self.pool, repository, limit)
            .await
            .map(|runs| {
                runs.into_iter()
                    .map(|run| WorkflowRunSummary {
                        workflow_run_id: run.id,
                        workflow_id: run.workflow_id,
                        phase: format!("{:?}", run_state_to_wire(run.state)).to_lowercase(),
                    })
                    .collect()
            })
            .map_err(|error| BlackboardChannelError::Backend(error.to_string()))
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

    #[tokio::test]
    async fn catalog_echoes_canonical_repository_and_matches_named_resolution() {
        let (temporary, pool) = temp_pool().await;
        let sources = temporary.path().join(".codypendent/workflows");
        std::fs::create_dir_all(&sources).unwrap();
        std::fs::write(
            sources.join("override.yaml"),
            MANIFEST.replace("\nversion: 1", "\nversion: 2"),
        )
        .unwrap();
        let host = host_with(pool, FakeExecutor::default());
        let catalog = host
            .catalog(WorkflowCatalogRequest {
                repository: Some(temporary.path().to_str().unwrap().to_owned()),
                owner_uid: 42,
                client_id: ClientId::new(),
            })
            .await
            .unwrap();
        let root = temporary.path().canonicalize().unwrap();
        assert_eq!(catalog.repository.as_deref(), root.to_str());
        let entry = catalog
            .items
            .iter()
            .find(|entry| entry.id == "repair-github-check")
            .unwrap();
        let manifest = host
            .resolve_named_workflow(&entry.id, catalog.repository.as_deref())
            .unwrap();
        let actual = compile_yaml(&manifest).unwrap();
        assert_eq!(
            (entry.version, entry.node_count, entry.source),
            (
                actual.version,
                actual.nodes.len() as u32,
                WorkflowCatalogSource::Repository
            )
        );
    }

    #[tokio::test]
    async fn catalog_refuses_relative_repository_without_disclosing_filesystem_errors() {
        let (_temporary, pool) = temp_pool().await;
        let host = host_with(pool, FakeExecutor::default());
        let error = host
            .catalog(WorkflowCatalogRequest {
                repository: Some("../../private".to_owned()),
                owner_uid: 42,
                client_id: ClientId::new(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, "workflow.invalid-repository");
        assert!(!error.message.contains("private"));
    }

    #[tokio::test]
    async fn catalog_digest_rejects_a_changed_definition_before_first_launch() {
        let (temporary, pool) = temp_pool().await;
        let sources = temporary.path().join(".codypendent/workflows");
        std::fs::create_dir_all(&sources).unwrap();
        let source = sources.join("selected.yaml");
        std::fs::write(&source, MANIFEST.replace("\nversion: 1", "\nversion: 2")).unwrap();
        let host = host_with(pool.clone(), FakeExecutor::default());
        let catalog = host
            .catalog(WorkflowCatalogRequest {
                repository: Some(temporary.path().to_str().unwrap().to_owned()),
                owner_uid: 1000,
                client_id: ClientId::new(),
            })
            .await
            .unwrap();
        let selected = catalog
            .items
            .into_iter()
            .find(|entry| entry.id == "repair-github-check")
            .unwrap();
        // Same sole repository version, different inputs/budget: cross-source
        // collision checks alone cannot protect this catalog-to-launch interval.
        std::fs::write(
            &source,
            MANIFEST.replace("\nversion: 1", "\nversion: 2")
                + "\ninputs:\n  changed:\n    type: string\n    required: true\n",
        )
        .unwrap();
        let mut request = start_request("selected-definition");
        request.workflow_id = Some(selected.id);
        request.repository = catalog.repository;
        request.expected_definition_digest = Some(selected.definition_digest);
        let error = host.start(request).await.unwrap_err();
        assert_eq!(error.code, "workflow.definition-changed");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM workflow_runs")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn catalog_digest_recovers_an_accepted_receipt_after_definition_changes() {
        let (temporary, pool) = temp_pool().await;
        let sources = temporary.path().join(".codypendent/workflows");
        std::fs::create_dir_all(&sources).unwrap();
        let source = sources.join("selected.yaml");
        std::fs::write(&source, MANIFEST.replace("\nversion: 1", "\nversion: 2")).unwrap();
        let host = host_with(pool.clone(), FakeExecutor::default());
        let catalog = host
            .catalog(WorkflowCatalogRequest {
                repository: Some(temporary.path().to_str().unwrap().to_owned()),
                owner_uid: 1000,
                client_id: ClientId::new(),
            })
            .await
            .unwrap();
        let selected = catalog
            .items
            .into_iter()
            .find(|entry| entry.id == "repair-github-check")
            .unwrap();
        let mut request = start_request("lost-acknowledgement");
        request.workflow_id = Some(selected.id);
        request.repository = catalog.repository;
        request.expected_definition_digest = Some(selected.definition_digest);
        let run_id = host.start(request.clone()).await.unwrap();
        // Even an unavailable current source must not hide the accepted request.
        std::fs::write(&source, "no longer valid YAML: [").unwrap();
        assert_eq!(host.start(request.clone()).await.unwrap(), run_id);
        request.inputs = json!({ "pull_request": 8 });
        assert_eq!(
            host.start(request).await.unwrap_err().code,
            "workflow.idempotency-mismatch"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM workflow_runs")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
    }

    fn start_request(key: &str) -> StartWorkflowRequest {
        StartWorkflowRequest {
            expected_definition_digest: None,
            manifest: MANIFEST.to_owned(),
            workflow_id: None,
            inputs: json!({ "pull_request": 7 }),
            idempotency_key: key.to_owned(),
            repository: None,
            owner_uid: 1_000,
            client_id: ClientId::new(),
            budget_ceiling: None,
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
    async fn archives_internal_children_only_after_parent_terminal_persistence() {
        let (_tmp, pool) = temp_pool().await;
        // `internal`, `parent_session_id` and `parent_run_id` are supplied by
        // `migrations/0040_session_library.sql`, which `db::open` now applies.
        // Adding them here again fails with `duplicate column name`.
        // `archived_at` likewise comes from 0040_session_library.sql.
        let compiled = compile_yaml(MANIFEST).unwrap();
        let workflow_run_id = WorkflowStore::new()
            .create_run(&pool, &compiled, None, &json!({}), Some(MANIFEST))
            .await
            .unwrap();
        let session_id = codypendent_protocol::SessionId::new();
        let agent_run_id = codypendent_protocol::RunId::new();
        let now = "2026-08-17T00:00:00Z";
        sqlx::query(
            "INSERT INTO sessions (id, title, state, created_at, updated_at, internal) \
             VALUES (?, 'internal child', 'open', ?, ?, 1)",
        )
        .bind(session_id.to_string())
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO runs (id, session_id, objective, mode, model_policy, budget_json, state, started_at, ended_at) \
             VALUES (?, ?, 'child', 'build', 'default', '{}', 'Completed', ?, ?)",
        )
        .bind(agent_run_id.to_string())
        .bind(session_id.to_string())
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE workflow_nodes SET agent_run_id = ? WHERE workflow_run_id = ? AND node_id = 'inspect'",
        )
        .bind(agent_run_id.to_string())
        .bind(&workflow_run_id)
        .execute(&pool)
        .await
        .unwrap();
        let host = host_with(pool.clone(), FakeExecutor::default());

        host.archive_internal_sessions(&workflow_run_id).await;
        let (before,): (Option<String>,) =
            sqlx::query_as("SELECT archived_at FROM sessions WHERE id = ?")
                .bind(session_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(before.is_none(), "a live parent must not archive its child");

        WorkflowStore::new()
            .set_run_state(&pool, &workflow_run_id, WorkflowRunState::Failed)
            .await
            .unwrap();
        host.archive_internal_sessions(&workflow_run_id).await;
        let (after,): (Option<String>,) =
            sqlx::query_as("SELECT archived_at FROM sessions WHERE id = ?")
                .bind(session_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(after.is_some(), "terminal persistence unlocks archival");
        chrono::DateTime::parse_from_rfc3339(after.as_deref().unwrap())
            .expect("archival timestamp remains readable by the Session Library");
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
                expected_definition_digest: None,
                manifest: "schema_version: 1\nid: empty\nversion: 1\nsteps: []\n".to_owned(),
                workflow_id: None,
                inputs: json!(null),
                idempotency_key: "cmd-bad".to_owned(),
                repository: None,
                owner_uid: 1_000,
                client_id: ClientId::new(),
                budget_ceiling: None,
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
    async fn pause_and_resume_publish_the_persisted_prompt_phase_immediately() {
        let (_tmp, pool) = temp_pool().await;
        let hub = WorkflowHub::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        let executor = Arc::new(GatedExecutor::new("inspect", gate.clone()));
        let host = WorkflowConductorHost::new(pool.clone(), executor)
            .with_streaming(hub.clone(), WorkflowRunCancellations::default());
        let run_id = host.start(start_request("cmd-prompt-phase")).await.unwrap();
        wait_for_node_state(&pool, &run_id, "inspect", NodeState::Running).await;
        let mut live = hub.subscribe(run_id.clone());

        host.pause(PauseWorkflowRequest {
            workflow_run_id: run_id.clone(),
            client_id: ClientId::new(),
        })
        .await
        .expect("pause accepted");
        next_phase(&mut live, WorkflowRunPhase::Paused).await;

        host.resume(ResumeWorkflowRequest {
            workflow_run_id: run_id.clone(),
            client_id: ClientId::new(),
        })
        .await
        .expect("resume accepted");
        next_phase(&mut live, WorkflowRunPhase::Running).await;

        gate.notify_one();
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

    /// A ceiling handed across the start seam must land in the run's STORED
    /// manifest, because that manifest — recompiled per node by
    /// `AgentLoopNodeExecutor::budget_limits` and by startup recovery — is the only
    /// place the enforced envelope is read from. A ceiling applied to the in-memory
    /// graph alone would vanish at the next restart, so this test reads it back out
    /// of the database and recompiles it exactly as execution does.
    #[tokio::test]
    async fn a_budget_ceiling_lands_in_the_runs_stored_manifest() {
        let (_tmp, pool) = temp_pool().await;
        let host = host_with(pool.clone(), FakeExecutor::default());

        let mut request = start_request("cmd-ceiling");
        request.budget_ceiling = Some(WorkflowRunCeiling {
            max_wall_time_seconds: Some(900),
            max_cost_micros: Some(1_500_000),
        });
        let run_id = host.start(request).await.expect("start with a ceiling");

        let stored = WorkflowStore::new()
            .manifest(&pool, &run_id)
            .await
            .expect("read the stored manifest")
            .expect("a manifest is recorded with the run");
        let compiled = compile_yaml(&stored).expect("the stored manifest recompiles");
        assert_eq!(compiled.budget.maximum_duration_seconds, Some(900));
        assert_eq!(compiled.budget.maximum_cost_usd, Some(1.5));
        // The rest of the declared envelope survives the rewrite — the ceiling
        // tightens, it does not replace.
        assert_eq!(compiled.budget.maximum_agents, Some(2));
        // And the graph is untouched, so the run resumes against its own signature.
        assert_eq!(
            compiled.signature(),
            compile_yaml(MANIFEST).unwrap().signature()
        );
    }

    /// An imposed ceiling may only tighten: a manifest already declaring a
    /// STRICTER envelope keeps its own, and a looser one is pulled down.
    #[tokio::test]
    async fn an_imposed_ceiling_never_loosens_the_manifests_own_envelope() {
        let manifest = MANIFEST.replace(
            "budget:\n  maximum_agents: 2\n",
            "budget:\n  maximum_agents: 2\n  maximum_duration_seconds: 60\n  maximum_cost_usd: 9.0\n",
        );
        let tightened = apply_budget_ceiling(
            &manifest,
            WorkflowRunCeiling {
                max_wall_time_seconds: Some(6_000),
                max_cost_micros: Some(2_000_000),
            },
        )
        .expect("tighten");
        let compiled = compile_yaml(&tightened).expect("compiles");
        assert_eq!(
            compiled.budget.maximum_duration_seconds,
            Some(60),
            "the manifest's stricter wall-clock ceiling wins over a looser imposed one"
        );
        assert_eq!(
            compiled.budget.maximum_cost_usd,
            Some(2.0),
            "the imposed cost ceiling wins where it is the stricter of the two"
        );
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
                expected_definition_digest: None,
                manifest: different_manifest,
                workflow_id: None,
                inputs: json!({ "pull_request": 7 }),
                idempotency_key: "cmd-shared-key".to_owned(),
                repository: None,
                owner_uid: 1_000,
                client_id: ClientId::new(),
                budget_ceiling: None,
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

    /// Await the next `RunPhaseChanged` on a live subscription, skipping node
    /// transitions.
    ///
    /// A subscriber cannot assume the phase change is the *first* thing it sees.
    /// Node transitions are persist-before-publish (see `PublishingNodeObserver`),
    /// so a test that reaches its starting point by polling the store can subscribe
    /// inside the window after the row is written and before its `NodeTransitioned`
    /// is published — and then observe that event first. Asserting on the first
    /// delivery made this test fail on `main` at c255bec while passing on the
    /// identical tree an hour earlier.
    async fn next_phase(
        live: &mut tokio::sync::broadcast::Receiver<WorkflowEvent>,
        target: WorkflowRunPhase,
    ) {
        for _ in 0..16 {
            // Every non-phase event (a `NodeTransitioned`, or a variant a newer
            // protocol adds — `WorkflowEvent` is `#[non_exhaustive]` here) is
            // skipped rather than asserted on.
            if let WorkflowEvent::RunPhaseChanged { phase, .. } =
                live.recv().await.expect("phase delivered")
            {
                assert_eq!(phase, target, "unexpected run phase");
                return;
            }
        }
        panic!("no RunPhaseChanged({target:?}) within 16 events");
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
