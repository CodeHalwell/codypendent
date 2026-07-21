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
use std::sync::{Arc, Mutex};

use codypendent_daemon::workflows::{
    PauseWorkflowRequest, ResumeWorkflowRequest, RetryWorkflowNodeRequest, StartWorkflowRequest,
    WorkflowLifecycle, WorkflowLifecycleFuture, WorkflowStartFuture, WorkflowStarter,
};
use codypendent_protocol::CodypendentError;
use codypendent_workflow::{
    compile_yaml, ConductorError, NodeExecutor, NodeObserver, NodeState, WorkflowConductor,
    WorkflowRunState, WorkflowStore, WorkflowStoreError,
};
use sqlx::SqlitePool;
use tracing::{info, warn};

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
    drive_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
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
        }
    }
}

impl<E: NodeExecutor + 'static> WorkflowConductorHost<E> {
    /// Build a host over the daemon's pool with the node executor to run each node
    /// through. The workflow tables share the daemon's pool (the migrations are
    /// workspace-wide).
    pub fn new(pool: SqlitePool, node_executor: Arc<E>) -> Self {
        Self {
            pool,
            node_executor,
            conductor: WorkflowConductor::new(),
            drive_locks: Arc::new(Mutex::new(HashMap::new())),
        }
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
                // A per-run observer records each node-lifecycle transition (the
                // seam the client-facing `Subscription::Workflow` stream will later
                // publish from); today it surfaces node progress in the daemon log.
                let observer = LoggingNodeObserver {
                    run_id: run_id.clone(),
                };
                match host
                    .conductor
                    .drive(&host.pool, &run_id, host.node_executor.as_ref(), &observer)
                    .await
                {
                    Ok(state) => {
                        info!(run = %run_id, state = state.as_str(), "workflow run driven to a stopping state")
                    }
                    Err(error) => warn!(run = %run_id, %error, "workflow run drive ended in error"),
                }
            }
            host.prune_run_lock(&run_id, lock);
        });
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

            // Create the durable run idempotently, keyed by the command's
            // idempotency key (a duplicate delivery resolves to the same run), and
            // record the manifest so recovery can recompile it. A store error may be
            // transient (a busy database), so mark it retryable.
            let run_id = WorkflowStore::new()
                .create_run_idempotent(
                    &host.pool,
                    &compiled,
                    &idempotency_key,
                    &inputs,
                    Some(&manifest),
                )
                .await
                .map_err(|error| {
                    CodypendentError::new(
                        "workflow.store-error",
                        format!("could not create the workflow run: {error}"),
                        true,
                    )
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
            host.conductor
                .pause(&host.pool, &request.workflow_run_id)
                .await
                .map_err(conductor_error_to_protocol)
        })
    }

    fn resume(&self, request: ResumeWorkflowRequest) -> WorkflowLifecycleFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            // Validate synchronously so the reply is an accurate accept/reject, then
            // drive the resumed run onward in the background.
            host.conductor
                .ensure_resumable(&host.pool, &request.workflow_run_id)
                .await
                .map_err(conductor_error_to_protocol)?;
            host.spawn_drive(request.workflow_run_id.clone());
            Ok(())
        })
    }

    fn retry_node(&self, request: RetryWorkflowNodeRequest) -> WorkflowLifecycleFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            // Reset the node + its dependents synchronously (so an unknown node or a
            // changed graph rejects), then drive the reset run in the background.
            host.conductor
                .prepare_retry(&host.pool, &request.workflow_run_id, &request.node_id)
                .await
                .map_err(conductor_error_to_protocol)?;
            host.spawn_drive(request.workflow_run_id.clone());
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

/// Records each node-lifecycle transition of one run (STEP 5.2 node-lifecycle
/// events over the observer). The [`NodeObserver`] callback carries only the node,
/// state, and attempt, so the observer is bound to its run id. Today it emits a
/// structured tracing event per transition — node progress observable in the
/// daemon log; the same seam is what a client-facing `Subscription::Workflow`
/// stream (like the document CRDT-sync stream) will publish from.
struct LoggingNodeObserver {
    run_id: String,
}

impl NodeObserver for LoggingNodeObserver {
    fn on_transition(&self, node_id: &str, state: NodeState, attempt: u32) {
        info!(
            run = %self.run_id,
            node = %node_id,
            state = state.as_str(),
            attempt,
            "workflow node transition"
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
        // No manifest for a nonexistent run.
        assert_eq!(error.code, "workflow.no-manifest");
    }
}
