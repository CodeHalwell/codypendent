//! Driving a durable workflow run to completion (STEP 5.2).
//!
//! The [`WorkflowStore`] holds the *state* of a run — which nodes are pending,
//! running, completed, or failed — but not the loop that advances it.
//! [`WorkflowDriver`] is that loop: it repeatedly asks the store for the ready
//! frontier ([`WorkflowStore::ready_nodes`]), executes each ready node through a
//! [`NodeExecutor`] seam, and records the resulting transition
//! ([`WorkflowStore::transition_node`]) with its attempt, cost, and agent-run id,
//! until no node can make progress. The run then lands in a terminal
//! [`WorkflowRunState`].
//!
//! The driver is **daemon-free and model-free**: the daemon implements
//! [`NodeExecutor`] over the agent loop (agent nodes) and the tool layer (tool
//! nodes), while a test implements it with canned outcomes — so the scheduling,
//! retry, and resume logic is exercised without a single model call. Two Phase 5
//! properties fall out of running against the durable store:
//!
//! * **Resume after restart.** A completed node is `Completed` in the store, so
//!   it is never in the ready frontier and never re-runs; a run interrupted
//!   mid-node left a node `Running`, which the driver resets to `Pending` on
//!   entry so it re-drives exactly once. Calling [`WorkflowDriver::run`] again on
//!   a partially-complete run continues from where it stopped.
//! * **Node-level provenance.** Each transition records the attempt, cost, and
//!   agent-run id the graph view surfaces; a [`NodeObserver`] sees every
//!   transition as it happens (the seam the daemon fills to emit
//!   `WorkflowNodeTransitioned` ledger events).
//!
//! A node failure blocks only its *dependents* — independent siblings still run,
//! maximising progress — and leaves the run [`Failed`](WorkflowRunState::Failed);
//! the blocked work is resumable via [`WorkflowStore::retry_from_node`] on the
//! failed node. Concurrent execution of the frontier (into isolated worktrees) is
//! a later refinement; this driver runs the frontier sequentially in topological
//! order, which is enough to prove the durable lifecycle.

use async_trait::async_trait;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::compile::{CompiledNode, CompiledWorkflow};
use crate::store::{NodeState, WorkflowRunState, WorkflowStore, WorkflowStoreError};

/// The outcome of executing one node attempt.
#[derive(Debug, Clone)]
pub enum NodeOutcome {
    /// The node's work succeeded. `agent_run_id` links an agent node to its run
    /// row; `cost` is the node's recorded spend (opaque JSON), both surfaced by
    /// the graph view.
    Completed {
        agent_run_id: Option<String>,
        cost: Option<Value>,
    },
    /// The node's work failed. The driver retries per the node's policy and, once
    /// attempts are exhausted, marks the node `Failed`. `error` is carried for the
    /// caller's diagnostics.
    Failed { error: String },
}

impl NodeOutcome {
    /// A bare success with no recorded cost or agent-run id.
    #[must_use]
    pub fn completed() -> Self {
        NodeOutcome::Completed {
            agent_run_id: None,
            cost: None,
        }
    }

    /// A failure carrying an error message.
    #[must_use]
    pub fn failed(error: impl Into<String>) -> Self {
        NodeOutcome::Failed {
            error: error.into(),
        }
    }
}

/// The context handed to a [`NodeExecutor`] for one node attempt.
pub struct NodeContext<'a> {
    /// The durable workflow-run id the node belongs to.
    pub workflow_run_id: &'a str,
    /// The compiled node being executed (its id, action, workspace, outputs, …).
    pub node: &'a CompiledNode,
    /// The 1-based attempt number (a node retried once runs at attempt 2).
    pub attempt: u32,
}

/// Executes a single workflow node. The daemon implements this over the agent
/// loop for agent nodes and the tool layer for tool nodes; a test implements it
/// with canned outcomes so the driver is exercised without any model call.
#[async_trait]
pub trait NodeExecutor: Send + Sync {
    /// Run one node attempt and report the outcome. Infrastructure that cannot
    /// even attempt the node should be reported as [`NodeOutcome::failed`] — the
    /// driver treats every non-success as a retryable failure.
    async fn execute(&self, ctx: NodeContext<'_>) -> NodeOutcome;
}

/// Observes each node-state transition as the driver records it — the seam the
/// daemon fills to emit `WorkflowNodeTransitioned` ledger events. A no-op
/// implementation is provided for `()`, so a caller that does not observe passes
/// `&()`.
pub trait NodeObserver: Send + Sync {
    /// Called after the driver records `node_id` entering `state` on `attempt`.
    fn on_transition(&self, node_id: &str, state: NodeState, attempt: u32);
}

impl NodeObserver for () {
    fn on_transition(&self, _node_id: &str, _state: NodeState, _attempt: u32) {}
}

/// Drives a durable workflow run to a terminal state through a [`NodeExecutor`].
/// Holds only the (zero-sized) [`WorkflowStore`], so it is cheap to construct.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkflowDriver {
    store: WorkflowStore,
}

impl WorkflowDriver {
    /// A driver over a fresh store handle.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: WorkflowStore::new(),
        }
    }

    /// Drive `workflow_run_id` to a terminal [`WorkflowRunState`] without an
    /// observer (equivalent to [`run_observed`](Self::run_observed) with `&()`).
    pub async fn run<E: NodeExecutor>(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        compiled: &CompiledWorkflow,
        executor: &E,
    ) -> Result<WorkflowRunState, WorkflowStoreError> {
        self.run_observed(pool, workflow_run_id, compiled, executor, &())
            .await
    }

    /// Drive `workflow_run_id` to a terminal [`WorkflowRunState`], reporting every
    /// transition to `observer`.
    ///
    /// **Refuses a changed graph signature** before mutating anything, so a
    /// manifest edited under a live run fails cleanly instead of half-driving a
    /// different graph. Resumable: completed nodes are skipped, and a node left
    /// `Running` by an earlier interrupted drive is reset to `Pending` and
    /// re-driven. Returns the run's terminal state
    /// ([`Completed`](WorkflowRunState::Completed) when every node completed,
    /// else [`Failed`](WorkflowRunState::Failed)).
    pub async fn run_observed<E: NodeExecutor, O: NodeObserver>(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        compiled: &CompiledWorkflow,
        executor: &E,
        observer: &O,
    ) -> Result<WorkflowRunState, WorkflowStoreError> {
        // Guard the signature before any write, so a changed graph never leaves a
        // half-driven run behind.
        let snapshot = self
            .store
            .snapshot(pool, workflow_run_id)
            .await?
            .ok_or_else(|| WorkflowStoreError::NotFound(workflow_run_id.to_owned()))?;
        let current = compiled.signature();
        if snapshot.run.graph_signature != current {
            return Err(WorkflowStoreError::GraphSignatureChanged {
                expected: snapshot.run.graph_signature,
                found: current,
            });
        }

        // Recover any node interrupted mid-execution by an earlier drive: it is
        // still `Running` in the store, so reset it to `Pending` to re-drive it
        // exactly once (effects are idempotent — the resume contract).
        for node in &snapshot.nodes {
            if node.state == NodeState::Running {
                self.store
                    .transition_node(
                        pool,
                        workflow_run_id,
                        &node.node_id,
                        NodeState::Pending,
                        node.attempt,
                        None,
                        None,
                    )
                    .await?;
            }
        }

        self.store
            .set_run_state(pool, workflow_run_id, WorkflowRunState::Running)
            .await?;

        // Advance the frontier until nothing can make progress. Each non-empty
        // round drives every ready node to a terminal state, so the pending set
        // strictly shrinks and the loop terminates.
        loop {
            let ready = self
                .store
                .ready_nodes(pool, workflow_run_id, compiled)
                .await?;
            if ready.is_empty() {
                break;
            }
            for node_id in ready {
                let node = compiled
                    .node(&node_id)
                    .expect("a ready node is part of the compiled graph");
                self.run_node(pool, workflow_run_id, node, executor, observer)
                    .await?;
            }
        }

        // Terminal state: completed iff every node completed; otherwise the run is
        // stuck behind a failure (a failed node blocks its dependents), so it is
        // failed — never reported completed while work remains undone.
        let final_snapshot = self
            .store
            .snapshot(pool, workflow_run_id)
            .await?
            .ok_or_else(|| WorkflowStoreError::NotFound(workflow_run_id.to_owned()))?;
        let final_state = if final_snapshot
            .nodes
            .iter()
            .all(|node| node.state == NodeState::Completed)
        {
            WorkflowRunState::Completed
        } else {
            WorkflowRunState::Failed
        };
        self.store
            .set_run_state(pool, workflow_run_id, final_state)
            .await?;
        Ok(final_state)
    }

    /// Drive one node through its retry policy: transition to `Running`, execute,
    /// and on success record `Completed` with the outcome's cost + agent-run id;
    /// on failure retry up to the node's `attempts`, marking `Failed` once they
    /// are exhausted. Every transition is reported to `observer`.
    async fn run_node<E: NodeExecutor, O: NodeObserver>(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        node: &CompiledNode,
        executor: &E,
        observer: &O,
    ) -> Result<(), WorkflowStoreError> {
        let max_attempts = node.retry.attempts.max(1);
        let mut attempt = 1u32;
        loop {
            self.store
                .transition_node(
                    pool,
                    workflow_run_id,
                    &node.id,
                    NodeState::Running,
                    attempt,
                    None,
                    None,
                )
                .await?;
            observer.on_transition(&node.id, NodeState::Running, attempt);

            let outcome = executor
                .execute(NodeContext {
                    workflow_run_id,
                    node,
                    attempt,
                })
                .await;
            match outcome {
                NodeOutcome::Completed { agent_run_id, cost } => {
                    self.store
                        .transition_node(
                            pool,
                            workflow_run_id,
                            &node.id,
                            NodeState::Completed,
                            attempt,
                            agent_run_id.as_deref(),
                            cost.as_ref(),
                        )
                        .await?;
                    observer.on_transition(&node.id, NodeState::Completed, attempt);
                    return Ok(());
                }
                // A retryable failure: bump the attempt and re-drive (the store
                // records the new attempt on the next `Running` transition).
                NodeOutcome::Failed { .. } if attempt < max_attempts => {
                    attempt += 1;
                }
                NodeOutcome::Failed { .. } => {
                    self.store
                        .transition_node(
                            pool,
                            workflow_run_id,
                            &node.id,
                            NodeState::Failed,
                            attempt,
                            None,
                            None,
                        )
                        .await?;
                    observer.on_transition(&node.id, NodeState::Failed, attempt);
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::compile::compile_yaml;
    use crate::db;

    /// A fake executor scripted per node id. A node not in the script completes;
    /// a node mapped to a list of outcomes returns them in order (one per
    /// attempt), so a `[Failed, Completed]` script models a flaky node that
    /// succeeds on retry. Every `execute` call is recorded so a test can assert
    /// which nodes ran (and how often).
    #[derive(Default)]
    struct ScriptedExecutor {
        script: HashMap<String, Vec<NodeOutcome>>,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedExecutor {
        fn with(mut self, node: &str, outcomes: Vec<NodeOutcome>) -> Self {
            self.script.insert(node.to_owned(), outcomes);
            self
        }

        fn calls_for(&self, node: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|id| *id == node)
                .count()
        }

        fn ran(&self, node: &str) -> bool {
            self.calls_for(node) > 0
        }
    }

    #[async_trait]
    impl NodeExecutor for ScriptedExecutor {
        async fn execute(&self, ctx: NodeContext<'_>) -> NodeOutcome {
            self.calls.lock().unwrap().push(ctx.node.id.clone());
            match self.script.get(&ctx.node.id) {
                // The scripted outcome for this attempt (1-based); past the end,
                // completes (a node scripted to fail once then unscripted succeeds).
                Some(outcomes) => outcomes
                    .get((ctx.attempt - 1) as usize)
                    .cloned()
                    .unwrap_or_else(NodeOutcome::completed),
                None => NodeOutcome::completed(),
            }
        }
    }

    /// Records the transition sequence so a test can assert lifecycle order.
    #[derive(Default)]
    struct RecordingObserver {
        seen: Mutex<Vec<(String, NodeState, u32)>>,
    }

    impl NodeObserver for RecordingObserver {
        fn on_transition(&self, node_id: &str, state: NodeState, attempt: u32) {
            self.seen
                .lock()
                .unwrap()
                .push((node_id.to_owned(), state, attempt));
        }
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

    // A diamond: a fans out to b and c, which both feed d.
    const DIAMOND: &str = "\
schema_version: 1
id: diamond
version: 1
steps:
  - id: a
    tool: repository.test
  - id: b
    depends_on: [a]
    tool: repository.test
  - id: c
    depends_on: [a]
    tool: repository.test
  - id: d
    depends_on: [b, c]
    tool: repository.test
";

    async fn temp_pool() -> (tempfile::TempDir, SqlitePool) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::open(&tmp.path().join("wf.db")).await.unwrap();
        (tmp, pool)
    }

    #[tokio::test]
    async fn drives_a_linear_workflow_to_completion() {
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(LINEAR).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}))
            .await
            .unwrap();

        // `b` completes with a recorded cost + agent-run id (node-level provenance).
        let executor = ScriptedExecutor::default().with(
            "b",
            vec![NodeOutcome::Completed {
                agent_run_id: Some("run-b".to_owned()),
                cost: Some(json!({ "usd": 0.02 })),
            }],
        );
        let final_state = WorkflowDriver::new()
            .run(&pool, &run_id, &compiled, &executor)
            .await
            .unwrap();

        assert_eq!(final_state, WorkflowRunState::Completed);
        let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
        assert!(snap.nodes.iter().all(|n| n.state == NodeState::Completed));
        let b = snap.nodes.iter().find(|n| n.node_id == "b").unwrap();
        assert_eq!(b.agent_run_id.as_deref(), Some("run-b"));
        assert_eq!(b.cost, Some(json!({ "usd": 0.02 })));
    }

    #[tokio::test]
    async fn a_failing_node_fails_the_run_and_blocks_its_dependents() {
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(LINEAR).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}))
            .await
            .unwrap();

        let executor = ScriptedExecutor::default().with("b", vec![NodeOutcome::failed("boom")]);
        let final_state = WorkflowDriver::new()
            .run(&pool, &run_id, &compiled, &executor)
            .await
            .unwrap();

        assert_eq!(final_state, WorkflowRunState::Failed);
        let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
        let state = |id: &str| snap.nodes.iter().find(|n| n.node_id == id).unwrap().state;
        assert_eq!(state("a"), NodeState::Completed);
        assert_eq!(state("b"), NodeState::Failed);
        // `c` depends on the failed `b`, so it never became ready and never ran.
        assert_eq!(state("c"), NodeState::Pending);
        assert!(!executor.ran("c"));
    }

    #[tokio::test]
    async fn retries_a_flaky_node_up_to_its_policy() {
        // `b` fails its first attempt then succeeds; the manifest allows 2 attempts.
        let manifest = "\
schema_version: 1
id: flaky
version: 1
steps:
  - id: a
    tool: repository.test
  - id: b
    depends_on: [a]
    tool: repository.test
    retry:
      attempts: 2
";
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(manifest).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}))
            .await
            .unwrap();

        let executor = ScriptedExecutor::default().with(
            "b",
            vec![NodeOutcome::failed("transient"), NodeOutcome::completed()],
        );
        let final_state = WorkflowDriver::new()
            .run(&pool, &run_id, &compiled, &executor)
            .await
            .unwrap();

        assert_eq!(final_state, WorkflowRunState::Completed);
        assert_eq!(
            executor.calls_for("b"),
            2,
            "b ran twice (fail then succeed)"
        );
        let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
        let b = snap.nodes.iter().find(|n| n.node_id == "b").unwrap();
        assert_eq!(b.state, NodeState::Completed);
        assert_eq!(b.attempt, 2, "the durable record shows the second attempt");
    }

    #[tokio::test]
    async fn resume_after_failure_does_not_re_run_completed_nodes() {
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(LINEAR).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}))
            .await
            .unwrap();

        // First drive: `a` completes, `b` fails (one attempt), so the run fails
        // with `c` still pending.
        let first = ScriptedExecutor::default().with("b", vec![NodeOutcome::failed("boom")]);
        assert_eq!(
            WorkflowDriver::new()
                .run(&pool, &run_id, &compiled, &first)
                .await
                .unwrap(),
            WorkflowRunState::Failed
        );

        // Reset `b` (and its dependent `c`) to retry from the failure, then drive
        // again with an all-succeeding executor.
        store
            .retry_from_node(&pool, &run_id, "b", &compiled)
            .await
            .unwrap();
        let second = ScriptedExecutor::default();
        assert_eq!(
            WorkflowDriver::new()
                .run(&pool, &run_id, &compiled, &second)
                .await
                .unwrap(),
            WorkflowRunState::Completed
        );

        // The already-completed `a` was NOT re-executed on resume; only the reset
        // `b` and `c` ran in the second drive.
        assert!(!second.ran("a"), "completed node a must not re-run");
        assert!(second.ran("b"));
        assert!(second.ran("c"));
    }

    #[tokio::test]
    async fn a_diamond_runs_both_branches_before_the_join() {
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(DIAMOND).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}))
            .await
            .unwrap();

        let executor = ScriptedExecutor::default();
        let observer = RecordingObserver::default();
        let final_state = WorkflowDriver::new()
            .run_observed(&pool, &run_id, &compiled, &executor, &observer)
            .await
            .unwrap();

        assert_eq!(final_state, WorkflowRunState::Completed);
        // Both branches ran, and the join `d` completed after both.
        assert!(executor.ran("b") && executor.ran("c") && executor.ran("d"));
        let completions: Vec<String> = observer
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, state, _)| *state == NodeState::Completed)
            .map(|(id, _, _)| id.clone())
            .collect();
        let pos = |id: &str| completions.iter().position(|c| c == id).unwrap();
        assert!(pos("a") < pos("b"), "a completes before b");
        assert!(pos("a") < pos("c"), "a completes before c");
        assert!(pos("b") < pos("d") && pos("c") < pos("d"), "d joins last");
    }
}
