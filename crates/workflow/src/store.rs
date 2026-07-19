//! Durable workflow execution storage (STEP 5.2): runs, node records, and
//! checkpoints.
//!
//! A [`WorkflowStore`] persists a compiled workflow as a *run* (the graph's
//! signature, inputs, and lifecycle state) plus a *node record* per step (state,
//! attempt count, cost, and start/end times — the node-level provenance the TUI
//! graph view surfaces). [`WorkflowStore::resume`] rebuilds after a restart:
//! it **refuses a changed graph signature** and otherwise reports the first
//! incomplete node so execution continues exactly once from where it stopped.
//! [`WorkflowStore::retry_from_node`] re-drives a chosen node and everything
//! downstream of it, resetting them to `Pending` so a resume picks up from there.
//!
//! The store is daemon-agnostic — it operates on a SQLite pool (see
//! [`crate::db`]) — so its recovery and idempotency semantics are testable
//! without the daemon; the daemon wires it into workflow startup recovery.

use std::collections::{BTreeSet, VecDeque};

use chrono::Utc;
use serde_json::Value;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::compile::CompiledWorkflow;

/// An error from the workflow store.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowStoreError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    /// No workflow run (or node) with the given id.
    #[error("no such workflow run: {0}")]
    NotFound(String),
    /// A stored row could not be decoded (should never happen; the store wrote it).
    #[error("corrupt workflow row: {0}")]
    Corrupt(String),
    /// Resume refused: the compiled graph differs from the one the run started on.
    #[error("workflow graph changed since the run started (expected {expected}, found {found})")]
    GraphSignatureChanged { expected: String, found: String },
}

/// The lifecycle state of a workflow run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowRunState {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl WorkflowRunState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            WorkflowRunState::Pending => "pending",
            WorkflowRunState::Running => "running",
            WorkflowRunState::Paused => "paused",
            WorkflowRunState::Completed => "completed",
            WorkflowRunState::Failed => "failed",
            WorkflowRunState::Cancelled => "cancelled",
        }
    }

    fn parse(s: &str) -> Result<Self, WorkflowStoreError> {
        Ok(match s {
            "pending" => WorkflowRunState::Pending,
            "running" => WorkflowRunState::Running,
            "paused" => WorkflowRunState::Paused,
            "completed" => WorkflowRunState::Completed,
            "failed" => WorkflowRunState::Failed,
            "cancelled" => WorkflowRunState::Cancelled,
            other => return Err(WorkflowStoreError::Corrupt(format!("run state {other}"))),
        })
    }
}

/// The lifecycle state of a single workflow node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Pending,
    Running,
    WaitingApproval,
    Blocked,
    Completed,
    Failed,
    Skipped,
}

impl NodeState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NodeState::Pending => "pending",
            NodeState::Running => "running",
            NodeState::WaitingApproval => "waiting_approval",
            NodeState::Blocked => "blocked",
            NodeState::Completed => "completed",
            NodeState::Failed => "failed",
            NodeState::Skipped => "skipped",
        }
    }

    /// Whether the node has finished (no further work): completed, failed, or
    /// skipped. Resume continues from the first non-terminal node.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            NodeState::Completed | NodeState::Failed | NodeState::Skipped
        )
    }

    fn parse(s: &str) -> Result<Self, WorkflowStoreError> {
        Ok(match s {
            "pending" => NodeState::Pending,
            "running" => NodeState::Running,
            "waiting_approval" => NodeState::WaitingApproval,
            "blocked" => NodeState::Blocked,
            "completed" => NodeState::Completed,
            "failed" => NodeState::Failed,
            "skipped" => NodeState::Skipped,
            other => return Err(WorkflowStoreError::Corrupt(format!("node state {other}"))),
        })
    }
}

/// A durable workflow run row.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRunRecord {
    pub id: String,
    pub workflow_id: String,
    pub workflow_version: u32,
    pub graph_signature: String,
    pub run_id: Option<String>,
    pub inputs: Value,
    pub state: WorkflowRunState,
}

/// A durable workflow node row.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowNodeRecord {
    pub node_id: String,
    pub state: NodeState,
    pub agent_run_id: Option<String>,
    pub attempt: u32,
    pub topo_order: usize,
    pub cost: Option<Value>,
}

/// A run plus its node records, in topological order.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRunSnapshot {
    pub run: WorkflowRunRecord,
    pub nodes: Vec<WorkflowNodeRecord>,
}

/// A recorded checkpoint (its content artifact is referenced, not inlined).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub id: String,
    pub graph_signature: String,
    pub state_artifact_id: Option<String>,
}

/// The result of [`WorkflowStore::resume`]: the run's snapshot, the first
/// incomplete node to continue from, and the latest checkpoint if any.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumePlan {
    pub snapshot: WorkflowRunSnapshot,
    /// The first non-terminal node in topological order, or `None` when the run
    /// has no work left.
    pub next_node: Option<String>,
    pub latest_checkpoint: Option<Checkpoint>,
}

/// The workflow store. Stateless; the pool is passed to each method.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkflowStore;

impl WorkflowStore {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Create a durable run from a compiled workflow: the run row (state
    /// `pending`, stamped with the graph signature) plus a `pending` node row per
    /// compiled node, in one transaction. Returns the new workflow-run id.
    pub async fn create_run(
        &self,
        pool: &SqlitePool,
        compiled: &CompiledWorkflow,
        run_id: Option<&str>,
        inputs: &Value,
    ) -> Result<String, WorkflowStoreError> {
        let id = Uuid::now_v7().to_string();
        let now = Utc::now().to_rfc3339();
        let signature = compiled.signature();

        let mut tx = pool.begin().await?;
        sqlx::query(
            "INSERT INTO workflow_runs \
             (id, workflow_id, workflow_version, graph_signature, run_id, inputs_json, state, \
              created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'pending', ?, ?)",
        )
        .bind(&id)
        .bind(&compiled.id)
        .bind(i64::from(compiled.version))
        .bind(&signature)
        .bind(run_id)
        .bind(serde_json::to_string(inputs)?)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;

        for node in &compiled.nodes {
            sqlx::query(
                "INSERT INTO workflow_nodes \
                 (id, workflow_run_id, node_id, state, attempt, topo_order, agent_run_id, cost_json) \
                 VALUES (?, ?, ?, 'pending', 0, ?, NULL, NULL)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(&id)
            .bind(&node.id)
            .bind(node.topo_order as i64)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    /// Set a run's lifecycle state.
    pub async fn set_run_state(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        state: WorkflowRunState,
    ) -> Result<(), WorkflowStoreError> {
        let affected =
            sqlx::query("UPDATE workflow_runs SET state = ?, updated_at = ? WHERE id = ?")
                .bind(state.as_str())
                .bind(Utc::now().to_rfc3339())
                .bind(workflow_run_id)
                .execute(pool)
                .await?
                .rows_affected();
        if affected == 0 {
            return Err(WorkflowStoreError::NotFound(workflow_run_id.to_owned()));
        }
        Ok(())
    }

    /// Transition a node to `state` with the given `attempt`, stamping
    /// `started_at` the first time it runs and setting `ended_at` to the terminal
    /// time — or **clearing it** when the node is not terminal, so a node retried
    /// back to `Running` never carries a stale end timestamp. `agent_run_id` and
    /// `cost` are recorded when provided (and otherwise left as they were).
    #[allow(clippy::too_many_arguments)]
    pub async fn transition_node(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        node_id: &str,
        state: NodeState,
        attempt: u32,
        agent_run_id: Option<&str>,
        cost: Option<&Value>,
    ) -> Result<(), WorkflowStoreError> {
        let now = Utc::now().to_rfc3339();
        let cost_json = match cost {
            Some(value) => Some(serde_json::to_string(value)?),
            None => None,
        };
        let started = (state == NodeState::Running).then(|| now.clone());
        let ended = state.is_terminal().then(|| now.clone());

        let affected = sqlx::query(
            "UPDATE workflow_nodes SET state = ?, attempt = ?, \
             agent_run_id = COALESCE(?, agent_run_id), \
             cost_json = COALESCE(?, cost_json), \
             started_at = COALESCE(started_at, ?), \
             ended_at = ? \
             WHERE workflow_run_id = ? AND node_id = ?",
        )
        .bind(state.as_str())
        .bind(i64::from(attempt))
        .bind(agent_run_id)
        .bind(cost_json)
        .bind(started)
        .bind(ended)
        .bind(workflow_run_id)
        .bind(node_id)
        .execute(pool)
        .await?
        .rows_affected();
        if affected == 0 {
            return Err(WorkflowStoreError::NotFound(format!(
                "{workflow_run_id}/{node_id}"
            )));
        }
        sqlx::query("UPDATE workflow_runs SET updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(workflow_run_id)
            .execute(pool)
            .await?;
        Ok(())
    }

    /// Record a checkpoint (its state artifact is referenced by id). Returns the
    /// new checkpoint id.
    pub async fn record_checkpoint(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        graph_signature: &str,
        state_artifact_id: Option<&str>,
    ) -> Result<String, WorkflowStoreError> {
        let id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO workflow_checkpoints \
             (id, workflow_run_id, graph_signature, state_artifact_id, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(workflow_run_id)
        .bind(graph_signature)
        .bind(state_artifact_id)
        .bind(Utc::now().to_rfc3339())
        .execute(pool)
        .await?;
        Ok(id)
    }

    /// The most recent checkpoint for a run, if any.
    pub async fn latest_checkpoint(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
    ) -> Result<Option<Checkpoint>, WorkflowStoreError> {
        let row = sqlx::query(
            "SELECT id, graph_signature, state_artifact_id FROM workflow_checkpoints \
             WHERE workflow_run_id = ? ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(workflow_run_id)
        .fetch_optional(pool)
        .await?;
        Ok(row.map(|r| Checkpoint {
            id: r.get("id"),
            graph_signature: r.get("graph_signature"),
            state_artifact_id: r.get("state_artifact_id"),
        }))
    }

    /// The full run + node snapshot, or `None` if the run does not exist.
    pub async fn snapshot(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
    ) -> Result<Option<WorkflowRunSnapshot>, WorkflowStoreError> {
        let Some(run_row) = sqlx::query(
            "SELECT workflow_id, workflow_version, graph_signature, run_id, inputs_json, state \
             FROM workflow_runs WHERE id = ?",
        )
        .bind(workflow_run_id)
        .fetch_optional(pool)
        .await?
        else {
            return Ok(None);
        };

        let run = WorkflowRunRecord {
            id: workflow_run_id.to_owned(),
            workflow_id: run_row.get("workflow_id"),
            workflow_version: run_row.get::<i64, _>("workflow_version") as u32,
            graph_signature: run_row.get("graph_signature"),
            run_id: run_row.get("run_id"),
            inputs: serde_json::from_str(&run_row.get::<String, _>("inputs_json"))?,
            state: WorkflowRunState::parse(&run_row.get::<String, _>("state"))?,
        };

        let node_rows = sqlx::query(
            "SELECT node_id, state, agent_run_id, attempt, topo_order, cost_json \
             FROM workflow_nodes WHERE workflow_run_id = ? ORDER BY topo_order ASC, node_id ASC",
        )
        .bind(workflow_run_id)
        .fetch_all(pool)
        .await?;
        let mut nodes = Vec::with_capacity(node_rows.len());
        for row in node_rows {
            nodes.push(WorkflowNodeRecord {
                node_id: row.get("node_id"),
                state: NodeState::parse(&row.get::<String, _>("state"))?,
                agent_run_id: row.get("agent_run_id"),
                attempt: row.get::<i64, _>("attempt") as u32,
                topo_order: row.get::<i64, _>("topo_order") as usize,
                cost: match row.get::<Option<String>, _>("cost_json") {
                    Some(json) => Some(serde_json::from_str(&json)?),
                    None => None,
                },
            });
        }
        Ok(Some(WorkflowRunSnapshot { run, nodes }))
    }

    /// Prepare to resume a run against a freshly compiled workflow. **Refuses a
    /// changed graph signature** (STEP 5.2); otherwise returns the snapshot, the
    /// first incomplete node to continue from, and the latest checkpoint.
    pub async fn resume(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        compiled: &CompiledWorkflow,
    ) -> Result<ResumePlan, WorkflowStoreError> {
        let snapshot = self
            .snapshot(pool, workflow_run_id)
            .await?
            .ok_or_else(|| WorkflowStoreError::NotFound(workflow_run_id.to_owned()))?;
        let current = compiled.signature();
        if snapshot.run.graph_signature != current {
            return Err(WorkflowStoreError::GraphSignatureChanged {
                expected: snapshot.run.graph_signature.clone(),
                found: current,
            });
        }
        let next_node = snapshot
            .nodes
            .iter()
            .find(|node| !node.state.is_terminal())
            .map(|node| node.node_id.clone());
        let latest_checkpoint = self.latest_checkpoint(pool, workflow_run_id).await?;
        Ok(ResumePlan {
            snapshot,
            next_node,
            latest_checkpoint,
        })
    }

    /// Re-drive a run from `node_id`: reset that node and **every node transitively
    /// downstream of it** to a fresh `Pending` state, and set the run `Running`
    /// (STEP 5.2 retry-from-node). The downstream nodes are reset too because their
    /// inputs came from the node being retried — leaving them `Completed` would
    /// resume past stale work. Each reset clears the node's attempt, timings, cost,
    /// and agent-run id so the next execution starts it clean; the returned ids are
    /// the reset set, sorted.
    ///
    /// **Refuses a changed graph signature** (like [`resume`](WorkflowStore::resume)):
    /// retrying a node only makes sense against the graph the run started on. A
    /// `node_id` absent from that graph is [`WorkflowStoreError::NotFound`]. This
    /// composes with [`resume`](WorkflowStore::resume) — after a retry-from-node the
    /// first incomplete node is the one retried.
    pub async fn retry_from_node(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        node_id: &str,
        compiled: &CompiledWorkflow,
    ) -> Result<Vec<String>, WorkflowStoreError> {
        let snapshot = self
            .snapshot(pool, workflow_run_id)
            .await?
            .ok_or_else(|| WorkflowStoreError::NotFound(workflow_run_id.to_owned()))?;
        let current = compiled.signature();
        if snapshot.run.graph_signature != current {
            return Err(WorkflowStoreError::GraphSignatureChanged {
                expected: snapshot.run.graph_signature.clone(),
                found: current,
            });
        }
        if compiled.node(node_id).is_none() {
            return Err(WorkflowStoreError::NotFound(format!(
                "{workflow_run_id}/{node_id}"
            )));
        }

        // The node plus its transitive dependents, walked over the compiled graph's
        // dependent edges. A `BTreeSet` dedups (so the DAG walk terminates) and
        // sorts the result for a stable return.
        let mut to_reset: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(node_id.to_owned());
        while let Some(current) = queue.pop_front() {
            if !to_reset.insert(current.clone()) {
                continue;
            }
            if let Some(node) = compiled.node(&current) {
                for dependent in &node.dependents {
                    if !to_reset.contains(dependent) {
                        queue.push_back(dependent.clone());
                    }
                }
            }
        }

        let now = Utc::now().to_rfc3339();
        let mut tx = pool.begin().await?;
        for id in &to_reset {
            sqlx::query(
                "UPDATE workflow_nodes SET state = 'pending', attempt = 0, agent_run_id = NULL, \
                 cost_json = NULL, started_at = NULL, ended_at = NULL \
                 WHERE workflow_run_id = ? AND node_id = ?",
            )
            .bind(workflow_run_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("UPDATE workflow_runs SET state = 'running', updated_at = ? WHERE id = ?")
            .bind(&now)
            .bind(workflow_run_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(to_reset.into_iter().collect())
    }
}
