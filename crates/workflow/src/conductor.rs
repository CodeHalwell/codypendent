//! Driving, recovering, and controlling durable workflow runs (STEP 5.2).
//!
//! [`WorkflowStore`] holds a run's *state* and [`WorkflowDriver`] advances a run
//! whose graph you already hold. [`WorkflowConductor`] is the layer between them
//! and the daemon: it recompiles a run's **stored manifest** into the graph the
//! driver needs, so the caller supplies only a run id, and it composes the
//! store's lifecycle operations into the commands a client issues:
//!
//! * [`drive`](WorkflowConductor::drive) — recompile a run's manifest and advance
//!   it to a terminal (or paused) state. The daemon spawns this fire-and-forget
//!   right after `StartWorkflow` creates the run, so a created run actually runs.
//! * [`recover_incomplete`](WorkflowConductor::recover_incomplete) — the
//!   startup-recovery pass: for every non-terminal run, recompile from its stored
//!   manifest and drive it, so a run interrupted by a crash resumes exactly once
//!   from where it stopped. A run whose manifest is missing or no longer compiles,
//!   or whose graph changed under it, is reported and skipped rather than crashing
//!   recovery; a **paused** run is deliberately left paused for an explicit resume.
//! * [`pause`](WorkflowConductor::pause) / [`resume`](WorkflowConductor::resume) /
//!   [`retry_from`](WorkflowConductor::retry_from) — the lifecycle commands. Pause
//!   flips the run to [`Paused`](WorkflowRunState::Paused) so a live driver stops
//!   cooperatively (see [`crate::drive`]); resume drives a paused run onward; retry
//!   resets a node and everything downstream of it and re-drives.
//!
//! The conductor is **daemon-free and model-free** — it operates on a SQLite pool
//! and a [`NodeExecutor`] seam — so the whole engine (drive to completion, restart
//! recovery, pause/resume, retry-from-node) is tested here with a fake executor,
//! no daemon and no model call. The daemon fills the seam with the real agent
//! loop / tool layer and adds the transport (interception + per-run drive
//! serialization); this crate owns the logic.

use sqlx::SqlitePool;

use crate::compile::{compile_yaml, CompiledWorkflow, WorkflowError};
use crate::drive::{NodeExecutor, NodeObserver, WorkflowDriver};
use crate::store::{WorkflowRunState, WorkflowStore, WorkflowStoreError};

/// A failure from the conductor: either a store/database error, a run whose
/// manifest is missing or no longer compiles, an unknown run, or an illegal
/// lifecycle transition. The daemon maps each to a structured `CommandRejected`.
#[derive(Debug, thiserror::Error)]
pub enum ConductorError {
    /// A database error from the underlying store.
    #[error(transparent)]
    Store(#[from] WorkflowStoreError),
    /// The run exists but has no stored manifest, so its graph cannot be
    /// recompiled (a run created before manifests were persisted, or without one).
    #[error("workflow run {0} has no stored manifest to recompile")]
    NoManifest(String),
    /// The run's stored manifest no longer compiles (e.g. a schema-breaking
    /// upgrade between the run's creation and this daemon). The
    /// [`GraphSignatureChanged`](WorkflowStoreError::GraphSignatureChanged) guard
    /// catches a manifest that compiles to a *different* graph; this catches one
    /// that no longer compiles at all.
    #[error("stored workflow manifest no longer compiles: {0}")]
    Compile(#[from] WorkflowError),
    /// No workflow run with the given id.
    #[error("no such workflow run: {0}")]
    NotFound(String),
    /// The run is in a state from which the requested lifecycle action is not
    /// allowed (e.g. pausing a completed run, resuming a run that is not paused).
    #[error("workflow run cannot be {action}: it is {state}")]
    IllegalTransition {
        action: &'static str,
        state: &'static str,
    },
}

/// What a [`recover_incomplete`](WorkflowConductor::recover_incomplete) pass did,
/// for a startup log line. Every non-terminal run is `considered`; the rest count
/// how it was handled so a crash-recovery pass is observable and never silently
/// drops a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Non-terminal runs examined.
    pub considered: usize,
    /// Runs driven (to a terminal or paused state) this pass.
    pub driven: usize,
    /// Paused runs deliberately left for an explicit resume.
    pub left_paused: usize,
    /// Runs skipped because they carry no stored manifest to recompile.
    pub skipped_no_manifest: usize,
    /// Runs skipped because their stored manifest no longer compiles.
    pub skipped_uncompilable: usize,
    /// Runs skipped because their graph changed under them (signature mismatch).
    pub signature_changed: usize,
    /// Runs that hit an unexpected store error while driving (logged, not fatal).
    pub errored: usize,
}

/// Recompiles a run's stored manifest and drives, recovers, and controls it
/// through the [`WorkflowStore`] + [`WorkflowDriver`]. Zero-sized and cheap to
/// construct; every method takes the pool.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkflowConductor {
    store: WorkflowStore,
    driver: WorkflowDriver,
}

impl WorkflowConductor {
    /// A conductor over fresh store + driver handles.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Recompile `workflow_run_id`'s stored manifest and drive it to a terminal
    /// (or, if a pause lands mid-drive, paused) [`WorkflowRunState`]. This is the
    /// entry point the daemon spawns right after a run is created, so a created run
    /// actually advances, and the primitive every other method composes.
    pub async fn drive<E: NodeExecutor, O: NodeObserver>(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        executor: &E,
        observer: &O,
    ) -> Result<WorkflowRunState, ConductorError> {
        let compiled = self.recompile(pool, workflow_run_id).await?;
        Ok(self
            .driver
            .run_observed(pool, workflow_run_id, &compiled, executor, observer)
            .await?)
    }

    /// The startup-recovery pass: drive every non-terminal run onward exactly once.
    ///
    /// A daemon calls this after a restart (backgrounded, so a slow run does not
    /// stall startup). Each `pending`/`running` run is recompiled from its stored
    /// manifest and driven — a `running` run had its interrupted node reset to
    /// pending by the driver's resume contract, so it continues from where it
    /// stopped. A **paused** run is left paused (it awaits an explicit resume, not
    /// an automatic one). Any per-run failure — no manifest, an uncompilable
    /// manifest, a changed graph, or a store error — is *counted and skipped*, not
    /// propagated, so one poisoned run never blocks recovery of the others. Only a
    /// failure of the initial run enumeration is returned.
    pub async fn recover_incomplete<E: NodeExecutor, O: NodeObserver>(
        &self,
        pool: &SqlitePool,
        executor: &E,
        observer: &O,
    ) -> Result<RecoveryReport, ConductorError> {
        let runs = self.store.list_incomplete_runs(pool).await?;
        let mut report = RecoveryReport::default();
        for run in runs {
            report.considered += 1;
            if run.state == WorkflowRunState::Paused {
                report.left_paused += 1;
                continue;
            }
            match self.drive(pool, &run.id, executor, observer).await {
                Ok(_) => report.driven += 1,
                Err(ConductorError::NoManifest(_)) => report.skipped_no_manifest += 1,
                Err(ConductorError::Compile(_)) => report.skipped_uncompilable += 1,
                Err(ConductorError::Store(WorkflowStoreError::GraphSignatureChanged {
                    ..
                })) => {
                    report.signature_changed += 1;
                }
                Err(_) => report.errored += 1,
            }
        }
        Ok(report)
    }

    /// Pause a run so a live driver stops launching new nodes (STEP 5.2). Setting
    /// the state is all it takes — the driver re-reads it each scheduling round and
    /// returns cooperatively (in-flight work in the current round drains first).
    /// Pausing an already-paused run is an idempotent no-op; pausing a terminal run
    /// is an [`IllegalTransition`](ConductorError::IllegalTransition).
    pub async fn pause(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
    ) -> Result<(), ConductorError> {
        match self.state(pool, workflow_run_id).await? {
            WorkflowRunState::Pending | WorkflowRunState::Running => {
                self.store
                    .set_run_state(pool, workflow_run_id, WorkflowRunState::Paused)
                    .await?;
                Ok(())
            }
            WorkflowRunState::Paused => Ok(()),
            terminal => Err(ConductorError::IllegalTransition {
                action: "paused",
                state: terminal.as_str(),
            }),
        }
    }

    /// Validate that a run may be resumed — that it is [`Paused`] — and flip it
    /// to [`Running`](WorkflowRunState::Running), so the daemon can reply to a
    /// `ResumeWorkflow` command *synchronously* (accepted / rejected) and then
    /// drive in the background (mirrors [`prepare_retry`](Self::prepare_retry)'s
    /// validate-then-mutate shape). Only a paused run may be resumed; a
    /// pending/running/terminal run is an
    /// [`IllegalTransition`](ConductorError::IllegalTransition), so a resume
    /// never double-drives a run that is already advancing.
    ///
    /// The mutation matters, not only the validation: [`drive`](Self::drive) (the
    /// library-level driver) refuses to touch anything but a
    /// [`Pending`](WorkflowRunState::Pending)/`Running` run (P5-D5) — a paused
    /// run must already have left `Paused` by the time `drive` sees it, or
    /// driving it would be a clean no-op instead of the intended resume.
    ///
    /// [`Paused`]: WorkflowRunState::Paused
    pub async fn prepare_resume(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
    ) -> Result<(), ConductorError> {
        match self.state(pool, workflow_run_id).await? {
            WorkflowRunState::Paused => {
                self.store
                    .set_run_state(pool, workflow_run_id, WorkflowRunState::Running)
                    .await?;
                Ok(())
            }
            other => Err(ConductorError::IllegalTransition {
                action: "resumed",
                state: other.as_str(),
            }),
        }
    }

    /// Resume a paused run: validate it is paused (flipping it to `Running`),
    /// then drive it onward from its ready frontier. The direct
    /// drive-to-completion form; the daemon instead pairs
    /// [`prepare_resume`](Self::prepare_resume) with a backgrounded
    /// [`drive`](Self::drive).
    pub async fn resume<E: NodeExecutor, O: NodeObserver>(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        executor: &E,
        observer: &O,
    ) -> Result<WorkflowRunState, ConductorError> {
        self.prepare_resume(pool, workflow_run_id).await?;
        self.drive(pool, workflow_run_id, executor, observer).await
    }

    /// Reset a run for a retry from `node_id` **without** driving it: reset that
    /// node and everything transitively downstream of it to `pending` and set the
    /// run `running` (STEP 5.2 retry-from-node). Lets the daemon reply to a
    /// `RetryWorkflowNode` command synchronously and then drive in the background.
    /// The store's [`retry_from_node`](WorkflowStore::retry_from_node) enforces the
    /// graph signature and that `node_id` exists.
    pub async fn prepare_retry(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        node_id: &str,
    ) -> Result<(), ConductorError> {
        let compiled = self.recompile(pool, workflow_run_id).await?;
        self.store
            .retry_from_node(pool, workflow_run_id, node_id, &compiled)
            .await?;
        Ok(())
    }

    /// Retry a run from `node_id`: reset that node and everything transitively
    /// downstream of it, set the run `running`, then drive. The direct
    /// drive-to-completion form composing [`prepare_retry`](Self::prepare_retry)
    /// and [`drive`](Self::drive); the daemon backgrounds the drive.
    pub async fn retry_from<E: NodeExecutor, O: NodeObserver>(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        node_id: &str,
        executor: &E,
        observer: &O,
    ) -> Result<WorkflowRunState, ConductorError> {
        self.prepare_retry(pool, workflow_run_id, node_id).await?;
        self.drive(pool, workflow_run_id, executor, observer).await
    }

    /// Recompile the run's stored manifest into its graph, or a structured error
    /// when the run does not exist, the run exists but has no stored manifest, or
    /// the manifest no longer compiles.
    ///
    /// [`WorkflowStore::manifest`] collapses "no such run" and "run exists with
    /// no manifest" into the same `Ok(None)` (by its own contract — a recovery
    /// pass never needs the distinction, since it only ever calls this on rows it
    /// just enumerated). A **client-supplied** run id (e.g. `RetryWorkflowNode`
    /// on a made-up id) is not so guaranteed, so this checks existence first
    /// (P5-D6a): a nonexistent run is [`NotFound`](ConductorError::NotFound), kept
    /// distinct from [`NoManifest`](ConductorError::NoManifest) so a client sees
    /// "no such run" rather than the misleading "this run has no manifest".
    async fn recompile(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
    ) -> Result<CompiledWorkflow, ConductorError> {
        if self.store.snapshot(pool, workflow_run_id).await?.is_none() {
            return Err(ConductorError::NotFound(workflow_run_id.to_owned()));
        }
        let manifest = self
            .store
            .manifest(pool, workflow_run_id)
            .await?
            .ok_or_else(|| ConductorError::NoManifest(workflow_run_id.to_owned()))?;
        Ok(compile_yaml(&manifest)?)
    }

    /// The run's current lifecycle state, or [`NotFound`](ConductorError::NotFound).
    async fn state(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
    ) -> Result<WorkflowRunState, ConductorError> {
        self.store
            .snapshot(pool, workflow_run_id)
            .await?
            .map(|snapshot| snapshot.run.state)
            .ok_or_else(|| ConductorError::NotFound(workflow_run_id.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::{json, Value};

    use super::*;
    use crate::db;
    use crate::drive::{NodeContext, NodeOutcome};
    use crate::store::{NodeState, WorkflowRunSnapshot};

    /// Create a durable run from a manifest and return its id (the manifest is
    /// stored so the conductor can recompile it), mirroring what the daemon's
    /// `StartWorkflow` seam does with `create_run_idempotent`.
    async fn create_run_from_manifest(pool: &SqlitePool, manifest: &str, inputs: &Value) -> String {
        let compiled = compile_yaml(manifest).expect("manifest compiles");
        WorkflowStore::new()
            .create_run(pool, &compiled, None, inputs, Some(manifest))
            .await
            .expect("create run")
    }

    const LINEAR: &str = "\
schema_version: 1
id: linear
version: 1
steps:
  - id: a
    tool: repository.test
  - id: b
    depends_on: [a]
    tool: repository.test
  - id: c
    depends_on: [b]
    tool: repository.test
";

    async fn temp_pool() -> (tempfile::TempDir, SqlitePool) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::open(&tmp.path().join("wf.db")).await.unwrap();
        (tmp, pool)
    }

    /// A configurable fake node executor: completes every node unless told to fail
    /// it, and can pause its run as a chosen node completes (modelling a pause
    /// command arriving mid-drive). Records every node it ran.
    #[derive(Default)]
    struct FakeExecutor {
        fail: HashSet<String>,
        pause_after: Option<(SqlitePool, String)>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeExecutor {
        fn failing(node: &str) -> Self {
            let mut fail = HashSet::new();
            fail.insert(node.to_owned());
            Self {
                fail,
                ..Self::default()
            }
        }

        fn pausing_after(pool: SqlitePool, node: &str) -> Self {
            Self {
                pause_after: Some((pool, node.to_owned())),
                ..Self::default()
            }
        }

        fn ran(&self, node: &str) -> bool {
            self.calls.lock().unwrap().iter().any(|id| id == node)
        }
    }

    #[async_trait]
    impl NodeExecutor for FakeExecutor {
        async fn execute(&self, ctx: NodeContext<'_>) -> NodeOutcome {
            self.calls.lock().unwrap().push(ctx.node.id.clone());
            if let Some((pool, node)) = &self.pause_after {
                if &ctx.node.id == node {
                    WorkflowStore::new()
                        .set_run_state(pool, ctx.workflow_run_id, WorkflowRunState::Paused)
                        .await
                        .expect("pause the run");
                }
            }
            if self.fail.contains(&ctx.node.id) {
                NodeOutcome::failed("boom")
            } else {
                NodeOutcome::completed()
            }
        }
    }

    #[tokio::test]
    async fn drive_runs_a_created_run_to_completion() {
        let (_tmp, pool) = temp_pool().await;
        let run_id = create_run_from_manifest(&pool, LINEAR, &json!({})).await;

        let conductor = WorkflowConductor::new();
        let executor = FakeExecutor::default();
        let state = conductor
            .drive(&pool, &run_id, &executor, &())
            .await
            .unwrap();

        assert_eq!(state, WorkflowRunState::Completed);
        let snap = WorkflowStore::new()
            .snapshot(&pool, &run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(snap.nodes.iter().all(|n| n.state == NodeState::Completed));
    }

    #[tokio::test]
    async fn drive_without_a_manifest_is_a_clean_no_manifest_error() {
        // A run created with no manifest (the store's lower-level path) cannot be
        // recompiled, so the conductor reports it rather than panicking.
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(LINEAR).unwrap();
        let run_id = WorkflowStore::new()
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();

        let err = WorkflowConductor::new()
            .drive(&pool, &run_id, &FakeExecutor::default(), &())
            .await
            .unwrap_err();
        assert!(matches!(err, ConductorError::NoManifest(id) if id == run_id));
    }

    #[tokio::test]
    async fn driving_or_retrying_a_nonexistent_run_is_not_found_not_no_manifest() {
        // P5-D6a: a run that never existed must be reported distinctly from one
        // that exists but has no stored manifest (the previous test) — both used
        // to collapse into `NoManifest` because `WorkflowStore::manifest` returns
        // `None` for either case.
        let (_tmp, pool) = temp_pool().await;
        let conductor = WorkflowConductor::new();

        let err = conductor
            .drive(&pool, "wfrun-does-not-exist", &FakeExecutor::default(), &())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ConductorError::NotFound(id) if id == "wfrun-does-not-exist"),
            "expected NotFound, got {err:?}"
        );

        let err = conductor
            .prepare_retry(&pool, "wfrun-does-not-exist", "verify")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ConductorError::NotFound(id) if id == "wfrun-does-not-exist"),
            "expected NotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn recover_incomplete_drives_pending_runs_and_leaves_paused_ones() {
        let (_tmp, pool) = temp_pool().await;
        let store = WorkflowStore::new();

        // A pending run (created, never driven — e.g. a crash between create and
        // the drive spawn) and a paused run (awaiting an explicit resume).
        let pending = create_run_from_manifest(&pool, LINEAR, &json!({})).await;
        let paused = create_run_from_manifest(&pool, LINEAR, &json!({})).await;
        store
            .set_run_state(&pool, &paused, WorkflowRunState::Paused)
            .await
            .unwrap();
        // A run with no manifest is skipped, never errored.
        let compiled = compile_yaml(LINEAR).unwrap();
        let no_manifest = store
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();

        let report = WorkflowConductor::new()
            .recover_incomplete(&pool, &FakeExecutor::default(), &())
            .await
            .unwrap();

        assert_eq!(report.considered, 3);
        assert_eq!(report.driven, 1);
        assert_eq!(report.left_paused, 1);
        assert_eq!(report.skipped_no_manifest, 1);

        // The pending run completed; the paused run is untouched; the no-manifest
        // run stays pending.
        let state_of = |snap: WorkflowRunSnapshot| snap.run.state;
        assert_eq!(
            state_of(store.snapshot(&pool, &pending).await.unwrap().unwrap()),
            WorkflowRunState::Completed
        );
        assert_eq!(
            state_of(store.snapshot(&pool, &paused).await.unwrap().unwrap()),
            WorkflowRunState::Paused
        );
        assert_eq!(
            state_of(store.snapshot(&pool, &no_manifest).await.unwrap().unwrap()),
            WorkflowRunState::Pending
        );
    }

    #[tokio::test]
    async fn recover_resets_and_re_drives_an_interrupted_running_node() {
        // A run left `running` with a node stuck `running` (a crash mid-node) is
        // recovered: the driver's resume contract resets the node and drives it.
        let (_tmp, pool) = temp_pool().await;
        let store = WorkflowStore::new();
        let run_id = create_run_from_manifest(&pool, LINEAR, &json!({})).await;
        store
            .set_run_state(&pool, &run_id, WorkflowRunState::Running)
            .await
            .unwrap();
        store
            .transition_node(&pool, &run_id, "a", NodeState::Running, 1, None, None)
            .await
            .unwrap();

        let report = WorkflowConductor::new()
            .recover_incomplete(&pool, &FakeExecutor::default(), &())
            .await
            .unwrap();
        assert_eq!(report.driven, 1);
        let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(snap.run.state, WorkflowRunState::Completed);
        assert!(snap.nodes.iter().all(|n| n.state == NodeState::Completed));
    }

    #[tokio::test]
    async fn pause_then_resume_completes_the_run() {
        let (_tmp, pool) = temp_pool().await;
        let store = WorkflowStore::new();
        let run_id = create_run_from_manifest(&pool, LINEAR, &json!({})).await;
        let conductor = WorkflowConductor::new();

        // Drive with an executor that pauses the run as `a` completes.
        let pauser = FakeExecutor::pausing_after(pool.clone(), "a");
        let state = conductor.drive(&pool, &run_id, &pauser, &()).await.unwrap();
        assert_eq!(state, WorkflowRunState::Paused);
        assert!(pauser.ran("a") && !pauser.ran("b"));

        // Resume drives the rest to completion.
        let resumer = FakeExecutor::default();
        let state = conductor
            .resume(&pool, &run_id, &resumer, &())
            .await
            .unwrap();
        assert_eq!(state, WorkflowRunState::Completed);
        assert!(!resumer.ran("a"), "the completed node a must not re-run");
        assert!(resumer.ran("b") && resumer.ran("c"));
        let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
        assert!(snap.nodes.iter().all(|n| n.state == NodeState::Completed));
    }

    #[tokio::test]
    async fn pause_a_terminal_run_is_rejected() {
        let (_tmp, pool) = temp_pool().await;
        let run_id = create_run_from_manifest(&pool, LINEAR, &json!({})).await;
        let conductor = WorkflowConductor::new();
        conductor
            .drive(&pool, &run_id, &FakeExecutor::default(), &())
            .await
            .unwrap();

        let err = conductor.pause(&pool, &run_id).await.unwrap_err();
        assert!(matches!(
            err,
            ConductorError::IllegalTransition {
                action: "paused",
                state: "completed"
            }
        ));
    }

    #[tokio::test]
    async fn resume_a_run_that_is_not_paused_is_rejected() {
        let (_tmp, pool) = temp_pool().await;
        let run_id = create_run_from_manifest(&pool, LINEAR, &json!({})).await;
        // A freshly created (pending) run is not paused, so resume is illegal —
        // driving a run that has not started is `drive`, not `resume`.
        let err = WorkflowConductor::new()
            .resume(&pool, &run_id, &FakeExecutor::default(), &())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ConductorError::IllegalTransition {
                action: "resumed",
                state: "pending"
            }
        ));
    }

    #[tokio::test]
    async fn retry_from_re_drives_a_failed_node_and_its_dependents() {
        let (_tmp, pool) = temp_pool().await;
        let store = WorkflowStore::new();
        let run_id = create_run_from_manifest(&pool, LINEAR, &json!({})).await;
        let conductor = WorkflowConductor::new();

        // First drive fails at `b`, so the run fails with `c` never reached.
        let state = conductor
            .drive(&pool, &run_id, &FakeExecutor::failing("b"), &())
            .await
            .unwrap();
        assert_eq!(state, WorkflowRunState::Failed);
        let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
        let node = |id: &str| snap.nodes.iter().find(|n| n.node_id == id).unwrap().state;
        assert_eq!(node("b"), NodeState::Failed);
        assert_eq!(node("c"), NodeState::Pending);

        // Retry from `b` with an all-succeeding executor completes the run; `a`
        // (upstream, already completed) is not re-run.
        let retry = FakeExecutor::default();
        let state = conductor
            .retry_from(&pool, &run_id, "b", &retry, &())
            .await
            .unwrap();
        assert_eq!(state, WorkflowRunState::Completed);
        assert!(!retry.ran("a"), "completed upstream node a must not re-run");
        assert!(retry.ran("b") && retry.ran("c"));
    }
}
