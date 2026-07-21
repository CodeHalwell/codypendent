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

use std::collections::{BTreeSet, HashSet, VecDeque};

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

    /// Whether the run has reached a terminal state — completed, failed, or
    /// cancelled — from which no further driving happens. A driver skips a
    /// terminal run, so a stray drive request (e.g. a duplicate `StartWorkflow`
    /// resolving to a finished run) is a clean no-op.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            WorkflowRunState::Completed | WorkflowRunState::Failed | WorkflowRunState::Cancelled
        )
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
    /// The first *resumable* non-terminal node in topological order — one with
    /// no `Failed`/`Skipped` ancestor — or `None` when the run has no
    /// schedulable work left. A run with `next_node: None` but a non-empty
    /// [`blocked_nodes`](Self::blocked_nodes) is finished-in-failure, not
    /// resumable: a recovery loop must not spin on it (it needs
    /// `retry_from_node` on the failed ancestor, or a terminal disposition).
    pub next_node: Option<String>,
    /// Non-terminal nodes that can never become ready under this snapshot,
    /// paired with the terminal-but-not-completed ancestor blocking each.
    pub blocked_nodes: Vec<(String, String)>,
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
    ///
    /// `manifest` is the workflow YAML the graph was compiled from, stored so a
    /// daemon can **recompile and resume** the run after a restart (STEP 5.2
    /// startup recovery). `None` records no manifest — the run is durable and
    /// drivable in-process, but a recovery pass that reconstructs the graph from
    /// the store cannot pick it up (it has nothing to recompile). The production
    /// `StartWorkflow` seam always supplies one; a store-mechanics test that never
    /// recovers may pass `None`.
    pub async fn create_run(
        &self,
        pool: &SqlitePool,
        compiled: &CompiledWorkflow,
        run_id: Option<&str>,
        inputs: &Value,
        manifest: Option<&str>,
    ) -> Result<String, WorkflowStoreError> {
        let id = Uuid::now_v7().to_string();
        let now = Utc::now().to_rfc3339();
        let signature = compiled.signature();

        let mut tx = pool.begin().await?;
        sqlx::query(
            "INSERT INTO workflow_runs \
             (id, workflow_id, workflow_version, graph_signature, run_id, inputs_json, state, \
              manifest_yaml, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?)",
        )
        .bind(&id)
        .bind(&compiled.id)
        .bind(i64::from(compiled.version))
        .bind(&signature)
        .bind(run_id)
        .bind(serde_json::to_string(inputs)?)
        .bind(manifest)
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

    /// Create a durable run **idempotently**, keyed by a client `idempotency_key`
    /// (the command's key). The run id is derived deterministically from the key,
    /// so a duplicate delivery of the same `StartWorkflow` (a client retrying after
    /// a lost acknowledgement) maps to the *same* run instead of creating a second
    /// one — the durable equivalent of the command write path's idempotency.
    ///
    /// The run row is inserted `OR IGNORE`: on a duplicate the primary-key
    /// (derived id) conflict is ignored, no second run or node set is created, and
    /// the existing id is returned. SQLite serialises writes, so two concurrent
    /// duplicate deliveries resolve to one run. Returns the run id in both cases.
    pub async fn create_run_idempotent(
        &self,
        pool: &SqlitePool,
        compiled: &CompiledWorkflow,
        idempotency_key: &str,
        inputs: &Value,
        manifest: Option<&str>,
    ) -> Result<String, WorkflowStoreError> {
        let id = deterministic_run_id(idempotency_key);
        let now = Utc::now().to_rfc3339();
        let signature = compiled.signature();

        let mut tx = pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO workflow_runs \
             (id, workflow_id, workflow_version, graph_signature, run_id, inputs_json, state, \
              manifest_yaml, created_at, updated_at) \
             VALUES (?, ?, ?, ?, NULL, ?, 'pending', ?, ?, ?)",
        )
        .bind(&id)
        .bind(&compiled.id)
        .bind(i64::from(compiled.version))
        .bind(&signature)
        .bind(serde_json::to_string(inputs)?)
        .bind(manifest)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        // A duplicate delivery: the run (and its nodes) already exist from the
        // first application. Nothing to insert — return the same id.
        if inserted == 0 {
            tx.commit().await?;
            return Ok(id);
        }

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

    /// Every run **not** in a terminal state — `pending`, `running`, or `paused` —
    /// oldest first. These are the runs a daemon must reconcile on startup after a
    /// crash: for each, recompile its workflow and [`resume`](WorkflowStore::resume)
    /// from the first incomplete node (the signature guard refuses one whose graph
    /// changed). Node records are not loaded here — call
    /// [`snapshot`](WorkflowStore::snapshot) or [`resume`](WorkflowStore::resume)
    /// per run when reconciling.
    pub async fn list_incomplete_runs(
        &self,
        pool: &SqlitePool,
    ) -> Result<Vec<WorkflowRunRecord>, WorkflowStoreError> {
        let rows = sqlx::query(
            "SELECT id, workflow_id, workflow_version, graph_signature, run_id, inputs_json, state \
             FROM workflow_runs WHERE state IN ('pending', 'running', 'paused') \
             ORDER BY created_at ASC, id ASC",
        )
        .fetch_all(pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(WorkflowRunRecord {
                    id: row.get("id"),
                    workflow_id: row.get("workflow_id"),
                    workflow_version: row.get::<i64, _>("workflow_version") as u32,
                    graph_signature: row.get("graph_signature"),
                    run_id: row.get("run_id"),
                    inputs: serde_json::from_str(&row.get::<String, _>("inputs_json"))?,
                    state: WorkflowRunState::parse(&row.get::<String, _>("state"))?,
                })
            })
            .collect()
    }

    /// The workflow manifest YAML a run was created from, if one was recorded
    /// (STEP 5.2 startup recovery). `Ok(None)` means either the run does not exist
    /// or it was created without a manifest (a store-mechanics test, or a run from
    /// before the column existed) — a recovery pass skips such a run because there
    /// is nothing to recompile. The daemon recompiles this YAML into the graph it
    /// resume-drives.
    pub async fn manifest(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
    ) -> Result<Option<String>, WorkflowStoreError> {
        let row = sqlx::query("SELECT manifest_yaml FROM workflow_runs WHERE id = ?")
            .bind(workflow_run_id)
            .fetch_optional(pool)
            .await?;
        Ok(row.and_then(|row| row.get::<Option<String>, _>("manifest_yaml")))
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

    /// The set of nodes ready to run **now**: every `Pending` node all of whose
    /// dependencies are `Completed`, in topological order (STEP 5.2). Unlike
    /// [`resume`](WorkflowStore::resume)'s single first-incomplete node, this is the
    /// parallel scheduler's *frontier* — the full set an executor may launch
    /// concurrently (into isolated worktrees, Phase 5's parallel-worktrees exit
    /// criterion). **Refuses a changed graph signature.** A node with a failed,
    /// skipped, or still-running dependency is not ready; it stays blocked until its
    /// inputs complete.
    pub async fn ready_nodes(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
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
        Ok(ready_node_ids(compiled, &snapshot))
    }

    /// Prepare to resume a run against a freshly compiled workflow. **Refuses a
    /// changed graph signature** (STEP 5.2); otherwise returns the snapshot, the
    /// first *resumable* incomplete node to continue from (never one stranded
    /// behind a `Failed`/`Skipped` ancestor — see [`ResumePlan::next_node`]),
    /// the permanently blocked set, and the latest checkpoint.
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
        let blocked_nodes = blocked_node_ids(compiled, &snapshot);
        let blocked: HashSet<&str> = blocked_nodes.iter().map(|(id, _)| id.as_str()).collect();
        let next_node = snapshot
            .nodes
            .iter()
            .find(|node| !node.state.is_terminal() && !blocked.contains(node.node_id.as_str()))
            .map(|node| node.node_id.clone());
        let latest_checkpoint = self.latest_checkpoint(pool, workflow_run_id).await?;
        Ok(ResumePlan {
            snapshot,
            next_node,
            blocked_nodes,
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

/// A deterministic workflow-run id derived from a command's idempotency key, so a
/// duplicate `StartWorkflow` delivery resolves to the same run (the anchor
/// [`WorkflowStore::create_run_idempotent`] inserts on). A `wfrun-` prefix keeps
/// it distinguishable from a random `create_run` id at a glance; the 128-bit SHA-256
/// prefix is collision-resistant for distinct keys.
fn deterministic_run_id(idempotency_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"workflow-run\x00");
    hasher.update(idempotency_key.as_bytes());
    format!("wfrun-{}", hex::encode(&hasher.finalize()[..16]))
}

/// The pure scheduling core of [`WorkflowStore::ready_nodes`]: the ids of the
/// nodes ready to run in `snapshot`, in the compiled graph's topological order. A
/// node is ready when its own state is `Pending` and every dependency is
/// `Completed`. Kept a free function (no pool) so the frontier logic is testable
/// on its own and reusable by an in-memory executor.
#[must_use]
pub fn ready_node_ids(compiled: &CompiledWorkflow, snapshot: &WorkflowRunSnapshot) -> Vec<String> {
    use std::collections::HashMap;
    let states: HashMap<&str, NodeState> = snapshot
        .nodes
        .iter()
        .map(|node| (node.node_id.as_str(), node.state))
        .collect();
    // `compiled.nodes` are already topologically ordered, so filtering in place
    // yields a topologically ordered frontier.
    compiled
        .nodes
        .iter()
        .filter(|node| states.get(node.id.as_str()) == Some(&NodeState::Pending))
        .filter(|node| {
            node.depends_on
                .iter()
                .all(|dep| states.get(dep.as_str()) == Some(&NodeState::Completed))
        })
        .map(|node| node.id.clone())
        .collect()
}

/// The non-terminal nodes that can never become ready under `snapshot`: a
/// dependency — direct or transitive — ended `Failed` or `Skipped` (terminal
/// but not `Completed`), so [`ready_node_ids`] will never surface them. Each is
/// paired with its blocking ancestor. Pure, like [`ready_node_ids`], and the
/// half [`WorkflowStore::resume`] uses to keep `next_node` honest: without it,
/// a recovery loop composing `list_incomplete_runs` + `resume` livelocked on a
/// run whose first incomplete node was stranded behind a failure.
#[must_use]
pub fn blocked_node_ids(
    compiled: &CompiledWorkflow,
    snapshot: &WorkflowRunSnapshot,
) -> Vec<(String, String)> {
    use std::collections::HashMap;
    let states: HashMap<&str, NodeState> = snapshot
        .nodes
        .iter()
        .map(|node| (node.node_id.as_str(), node.state))
        .collect();
    // Propagate blockers in topological order: a node is blocked by a
    // dependency that itself ended Failed/Skipped, or by whatever blocks a
    // dependency. `compiled.nodes` are topologically ordered, so each
    // dependency's entry is settled before its dependents are visited.
    let mut blocker_of: HashMap<&str, &str> = HashMap::new();
    let mut blocked = Vec::new();
    for node in &compiled.nodes {
        let mut blocker: Option<&str> = None;
        for dep in &node.depends_on {
            match states.get(dep.as_str()) {
                Some(NodeState::Failed) | Some(NodeState::Skipped) => {
                    blocker = Some(dep.as_str());
                    break;
                }
                _ => {}
            }
            if let Some(upstream) = blocker_of.get(dep.as_str()) {
                blocker = Some(upstream);
                break;
            }
        }
        if let Some(blocking) = blocker {
            blocker_of.insert(node.id.as_str(), blocking);
            let live = states
                .get(node.id.as_str())
                .map(|state| !state.is_terminal())
                .unwrap_or(false);
            if live {
                blocked.push((node.id.clone(), blocking.to_owned()));
            }
        }
    }
    blocked
}
