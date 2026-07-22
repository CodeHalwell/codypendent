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
//!
//! **Cooperative pause/cancel.** The driver re-reads the run's persisted state at
//! each scheduling boundary, so a [`pause`](crate::WorkflowConductor::pause) or
//! cancel that flipped the run to [`Paused`](WorkflowRunState::Paused) /
//! [`Cancelled`](WorkflowRunState::Cancelled) mid-drive stops it launching further
//! nodes; the driver returns that state without overwriting it, and a later
//! [`resume`](crate::WorkflowConductor::resume) picks the run back up from the next
//! ready node. The in-flight wave of the current round finishes first (drain then
//! stop) — this is the lifecycle-command half of STEP 5.2, driven through the
//! [`WorkflowConductor`](crate::WorkflowConductor).

use async_trait::async_trait;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::budget::BudgetWarning;
use crate::compile::{CompiledNode, CompiledWorkflow};
use crate::store::{
    ready_node_ids, NodeState, WorkflowRunState, WorkflowStore, WorkflowStoreError,
};

/// The outcome of executing one node attempt.
#[derive(Debug, Clone)]
pub enum NodeOutcome {
    /// The node's work succeeded. `agent_run_id` links an agent node to its run
    /// row; `cost` is the node's **measured** spend (only the dimensions the
    /// executor actually measured — see [`crate::budget::NodeCost`]), both
    /// surfaced by the graph view. `warnings` lists any budget dimension that
    /// crossed 80% while charging this node — the driver relays each to the
    /// [`NodeObserver`] so a client (today, the daemon log) learns a run nears a
    /// ceiling. Success is never withheld for a warning; a warning only informs.
    Completed {
        agent_run_id: Option<String>,
        cost: Option<Value>,
        warnings: Vec<BudgetWarning>,
    },
    /// The node's work failed. The driver retries per the node's policy and, once
    /// attempts are exhausted, marks the node `Failed`. `error` is carried for the
    /// caller's diagnostics and persisted on the node's durable `error` column.
    Failed { error: String },
    /// The node exhausted a budget dimension: the executor charged its measured
    /// cost and a node slice or the workflow envelope was exceeded. The driver
    /// records the node `Blocked` (with `error` naming the dimension and `cost`
    /// the measured spend) and **pauses the run** for a human decision — an
    /// overrun is never silent. A block is NOT retried (retrying re-spends against
    /// the same exhausted budget); a later `ResumeWorkflow` re-evaluates it.
    Blocked { error: String, cost: Option<Value> },
}

impl NodeOutcome {
    /// A bare success with no recorded cost, agent-run id, or budget warning.
    #[must_use]
    pub fn completed() -> Self {
        NodeOutcome::Completed {
            agent_run_id: None,
            cost: None,
            warnings: Vec::new(),
        }
    }

    /// A failure carrying an error message.
    #[must_use]
    pub fn failed(error: impl Into<String>) -> Self {
        NodeOutcome::Failed {
            error: error.into(),
        }
    }

    /// A budget block carrying its dimension reason and the measured cost that
    /// tipped the node over.
    #[must_use]
    pub fn blocked(error: impl Into<String>, cost: Option<Value>) -> Self {
        NodeOutcome::Blocked {
            error: error.into(),
            cost,
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

/// The full detail of one node-state transition the driver reports to a
/// [`NodeObserver`] (T9). It carries not just the new `state` and `attempt` but the
/// node's **measured cost** and **failure/block reason** at that transition, and any
/// **budget warnings** raised while charging it — everything a client-facing
/// `WorkflowNodeView` needs, captured synchronously at the transition (the observer
/// callback is sync, so it cannot re-read the store, and the store write that
/// preceded this call has the same values). Fields not applicable to a state are
/// absent (a `Running` transition carries no cost/error; a `Completed` carries cost
/// and any warnings; a `Failed`/`Blocked` carries the reason).
#[derive(Debug, Clone, Copy)]
pub struct NodeTransition<'a> {
    /// The node that transitioned.
    pub node_id: &'a str,
    /// The state it entered.
    pub state: NodeState,
    /// The 1-based attempt the transition is for.
    pub attempt: u32,
    /// The node's measured cost, when the transition records one (`Completed`,
    /// `Blocked`), else `None`.
    pub cost: Option<&'a Value>,
    /// The failure/block reason, when the transition carries one (`Failed`,
    /// `Blocked`), else `None`.
    pub error: Option<&'a str>,
    /// Budget dimensions that crossed 80% while charging this node (relayed on the
    /// `Completed` transition); empty otherwise.
    pub warnings: &'a [BudgetWarning],
}

impl<'a> NodeTransition<'a> {
    /// A bare transition into `state` on `attempt`, with no cost/error/warnings —
    /// the shape a `Running` transition (and most tests) uses.
    #[must_use]
    pub fn new(node_id: &'a str, state: NodeState, attempt: u32) -> Self {
        Self {
            node_id,
            state,
            attempt,
            cost: None,
            error: None,
            warnings: &[],
        }
    }
}

/// Observes each node-state transition as the driver records it — the seam the
/// daemon fills to publish `WorkflowNodeTransitioned` events to a run's subscribers
/// (T9). A no-op implementation is provided for `()`, so a caller that does not
/// observe passes `&()`; a `(A, B)` tuple composes two observers (the daemon runs a
/// logging observer and a publishing one over one drive — compose, don't replace).
///
/// Beyond durable state transitions, the driver reports three kinds of progress
/// that are *not* durable node state (they are history, not the latest fact), so
/// they surface here rather than in the store: a **budget warning** (a dimension
/// crossed 80%), an **intermediate retry failure** (a failed attempt the node
/// will retry — the durable `error` column keeps only the latest/terminal
/// reason, so the per-attempt history goes here), and a **recovery reset** (a
/// node left `Running`/`Blocked` by an interrupted drive, reset to re-drive).
/// All three have default no-op bodies, so an observer that only cares about
/// transitions (and `()`) is unaffected; the daemon's logging observer overrides
/// them.
pub trait NodeObserver: Send + Sync {
    /// Called after the driver records the [`NodeTransition`] durably (so a
    /// publisher observes persist-before-publish).
    fn on_transition(&self, transition: NodeTransition<'_>);

    /// A budget dimension crossed 80% while charging `node_id` on `attempt`
    /// (the node stayed within budget — this is a warning, not a block).
    fn on_budget_warning(&self, _node_id: &str, _warning: BudgetWarning, _attempt: u32) {}

    /// An intermediate attempt of `node_id` failed with `error` and will be
    /// retried (the node is not yet `Failed`). The durable `error` column keeps
    /// only the latest/terminal reason, so this per-attempt reason is history.
    fn on_attempt_failed(&self, _node_id: &str, _attempt: u32, _error: &str) {}

    /// `node_id` was reset from a non-terminal `Running`/`Blocked` state left by
    /// an interrupted earlier drive, so it re-drives from `attempt` (P5-D4: the
    /// recovery reset was previously invisible to the observer).
    fn on_recovery_reset(&self, _node_id: &str, _attempt: u32) {}
}

impl NodeObserver for () {
    fn on_transition(&self, _transition: NodeTransition<'_>) {}
}

/// Compose two observers over one drive: every callback fans out to both, in
/// order. Lets the daemon run its logging observer alongside a publishing observer
/// without either knowing about the other (T9: compose, don't replace).
impl<A: NodeObserver, B: NodeObserver> NodeObserver for (A, B) {
    fn on_transition(&self, transition: NodeTransition<'_>) {
        self.0.on_transition(transition);
        self.1.on_transition(transition);
    }

    fn on_budget_warning(&self, node_id: &str, warning: BudgetWarning, attempt: u32) {
        // `BudgetWarning` is `Copy`, so both observers get their own value.
        self.0.on_budget_warning(node_id, warning, attempt);
        self.1.on_budget_warning(node_id, warning, attempt);
    }

    fn on_attempt_failed(&self, node_id: &str, attempt: u32, error: &str) {
        self.0.on_attempt_failed(node_id, attempt, error);
        self.1.on_attempt_failed(node_id, attempt, error);
    }

    fn on_recovery_reset(&self, node_id: &str, attempt: u32) {
        self.0.on_recovery_reset(node_id, attempt);
        self.1.on_recovery_reset(node_id, attempt);
    }
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

        // A run that is not `Pending`/`Running` is not this call's to advance
        // (P5-D5). A `Paused` run awaits an explicit resume — which flips it to
        // `Running` *before* calling here (see
        // [`WorkflowConductor::prepare_resume`](crate::conductor::WorkflowConductor::prepare_resume))
        // — and a terminal run (`Completed`/`Failed`/`Cancelled`) is finished.
        // Without this guard, calling this function directly (bypassing the
        // conductor's lifecycle checks — e.g. a stray/duplicate drive, or a
        // future caller that does not know about pause/terminal states) would
        // set the run `Running` unconditionally below, resurrecting it: a
        // paused run would silently resume with no explicit ask, and a
        // terminal run would flicker back to `Running` before landing on a
        // terminal state again. A clean no-op reporting the run's CURRENT
        // state is safe for every existing caller — the daemon host and this
        // crate's own conductor never reach here with a run in one of these
        // states except through the sanctioned resume path.
        if !matches!(
            snapshot.run.state,
            WorkflowRunState::Pending | WorkflowRunState::Running
        ) {
            return Ok(snapshot.run.state);
        }

        // Recover any node left non-terminal by an earlier drive so it re-enters
        // the ready frontier (the scheduler only surfaces `Pending` nodes):
        //
        // * a `Running` node was interrupted mid-execution (a crash), so reset it
        //   to `Pending` to re-drive exactly once (effects are idempotent — the
        //   resume contract), CLEARING `agent_run_id`/cost explicitly (P5-D6b)
        //   rather than the COALESCE preserve `transition_node` uses — the attempt
        //   being thrown away must never leave a stale link behind;
        // * a `WaitingApproval` node (a tool node parked on the in-memory approval
        //   broker) is likewise RESUMABLE: the broker's waiter is lost on a daemon
        //   restart, so a still-`Running` run left with a parked node would
        //   otherwise strand — an empty frontier, then a `Failed` terminal that
        //   discards all completed work (MF-2). Reset it exactly like `Running` so
        //   it re-enters the frontier and re-drives the park once against the
        //   restarted broker. This is safe/idempotent because the park happens
        //   BEFORE the node's effect (the GitHub write / patch+test), so a parked
        //   node has performed no external effect to repeat;
        // * a `Blocked` node paused the run on a budget exhaustion, so reset it to
        //   `Pending` to re-evaluate on this (resume) drive, PRESERVING its
        //   measured cost + block reason so the executor's pre-gate re-blocks
        //   against the same durable budget consumption without re-running the
        //   work (the "resume re-evaluates, still exhausted → re-blocks" loop).
        //
        // All were previously invisible; each now reports to the observer (P5-D4).
        for node in &snapshot.nodes {
            match node.state {
                NodeState::Running | NodeState::WaitingApproval => {
                    self.store
                        .reset_interrupted_node(pool, workflow_run_id, &node.node_id, node.attempt)
                        .await?;
                    observer.on_recovery_reset(&node.node_id, node.attempt);
                }
                NodeState::Blocked => {
                    self.store
                        .reset_blocked_node(pool, workflow_run_id, &node.node_id)
                        .await?;
                    observer.on_recovery_reset(&node.node_id, node.attempt);
                }
                _ => {}
            }
        }

        // Start driving: a conditional transition (P5-D5 follow-up), not the
        // unconditional write this used to be. `PauseWorkflow` deliberately
        // does not take the per-run drive lock a live drive holds (P5-D3, so
        // the cooperative "stops at the next boundary" contract stays
        // snappy), so a pause can commit between the snapshot read above and
        // this point. Without the CAS, this write would silently clobber
        // that just-committed `Paused` back to `Running` and the frontier
        // loop below would proceed as though the pause never happened.
        if !self
            .try_transition_to_running(pool, workflow_run_id)
            .await?
        {
            // Someone else won the race and moved the run before we could —
            // bail without touching the frontier, reporting the run's
            // CURRENT (freshly re-read) state, exactly like the P5-D5 guard
            // above. The pause sticks.
            let current = self
                .store
                .snapshot(pool, workflow_run_id)
                .await?
                .ok_or_else(|| WorkflowStoreError::NotFound(workflow_run_id.to_owned()))?;
            return Ok(current.run.state);
        }

        // Advance the frontier until nothing can make progress — or until a
        // concurrent `pause`/`cancel` (STEP 5.2 lifecycle commands) asks the driver
        // to stop. Each round re-reads the run so a state the conductor flipped to
        // `Paused`/`Cancelled` is observed at the next scheduling boundary: the
        // driver stops launching new nodes and returns, leaving the run in that
        // state for a later `resume` to continue from (the in-flight wave of the
        // current round finishes first — a cooperative "drain then stop"). Each
        // non-empty round drives every ready node to a terminal state, so the
        // pending set strictly shrinks and the loop terminates.
        //
        // The snapshot taken here also feeds the pure `ready_node_ids` frontier, so
        // this is one read per round, not two (the signature was already guarded
        // above and the graph cannot change under a single drive).
        loop {
            let snapshot = self
                .store
                .snapshot(pool, workflow_run_id)
                .await?
                .ok_or_else(|| WorkflowStoreError::NotFound(workflow_run_id.to_owned()))?;
            if matches!(
                snapshot.run.state,
                WorkflowRunState::Paused | WorkflowRunState::Cancelled
            ) {
                return Ok(snapshot.run.state);
            }
            let ready = ready_node_ids(compiled, &snapshot);
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
        // A CAS (not an unconditional write), mirroring the Blocked branch's "a
        // concurrent cancel must win" above: `cancel` deliberately does not take the
        // per-run drive lock (P5-D3), so a `CancelWorkflow` — already accepted, the
        // client told OK — can commit `Cancelled` in the window between the frontier
        // loop's last read (which saw `Running`, so the loop broke on an empty ready
        // set rather than the top-of-round cancel check) and this write. An
        // unconditional write would clobber that `Cancelled` (or a raced `Paused`)
        // back to `Completed`/`Failed`, silently reverting the accepted cancel. Gating
        // on `Running` means this only applies while the run is still ours to finish;
        // if it affects 0 rows, the run was concurrently moved out of `Running` — that
        // state wins, so report the run's actual (re-read) current state.
        let affected = self
            .store
            .set_run_state_if_legal(
                pool,
                workflow_run_id,
                &[WorkflowRunState::Running],
                final_state,
            )
            .await?;
        if affected == 0 {
            let current = self
                .store
                .snapshot(pool, workflow_run_id)
                .await?
                .ok_or_else(|| WorkflowStoreError::NotFound(workflow_run_id.to_owned()))?;
            return Ok(current.run.state);
        }
        Ok(final_state)
    }

    /// Attempt to start driving `workflow_run_id`: transition it from
    /// `Pending`/`Running` into `Running`, via a conditional `UPDATE` rather
    /// than an unconditional write (P5-D5 follow-up). `PauseWorkflow`
    /// deliberately does not take the per-run drive lock a live drive holds
    /// (P5-D3 — taking it there would make pause block until the CURRENT
    /// drive fully finishes, breaking its cooperative "stops at the next
    /// boundary" contract), so a pause can commit between
    /// [`run_observed`](Self::run_observed)'s initial signature/state read
    /// and this call. Without the CAS here, this write would silently
    /// clobber that just-committed `Paused` back to `Running`, and the
    /// frontier loop would then proceed as though the pause never happened —
    /// a lost pause with no signal to the client.
    ///
    /// Returns `true` if the transition applied (the caller proceeds to
    /// drive the frontier); `false` if it did not — the run's current state
    /// is no longer `Pending`/`Running`, and the caller must bail without
    /// touching the frontier, reporting the run's freshly re-read current
    /// state.
    async fn try_transition_to_running(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
    ) -> Result<bool, WorkflowStoreError> {
        let affected = self
            .store
            .set_run_state_if_legal(
                pool,
                workflow_run_id,
                &[WorkflowRunState::Pending, WorkflowRunState::Running],
                WorkflowRunState::Running,
            )
            .await?;
        Ok(affected > 0)
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
        // Resume from the node's **persisted** attempt, not a fresh 1: a node
        // interrupted mid-retry (reset `Running` → `Pending` on entry, its attempt
        // preserved) must not restart its attempt count, or it could exceed
        // `max_attempts` across a restart and regress its recorded provenance. A
        // never-run node carries attempt 0, so this starts it at 1.
        let mut attempt = self
            .store
            .snapshot(pool, workflow_run_id)
            .await?
            .and_then(|snapshot| {
                snapshot
                    .nodes
                    .into_iter()
                    .find(|record| record.node_id == node.id)
                    .map(|record| record.attempt)
            })
            .unwrap_or(0)
            .max(1);
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
            observer.on_transition(NodeTransition::new(&node.id, NodeState::Running, attempt));

            let outcome = executor
                .execute(NodeContext {
                    workflow_run_id,
                    node,
                    attempt,
                })
                .await;
            match outcome {
                NodeOutcome::Completed {
                    agent_run_id,
                    cost,
                    warnings,
                } => {
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
                    // The Completed transition carries the node's measured cost and
                    // any budget warnings, so a publishing observer builds the full
                    // node view in one synchronous call (T9).
                    observer.on_transition(NodeTransition {
                        node_id: &node.id,
                        state: NodeState::Completed,
                        attempt,
                        cost: cost.as_ref(),
                        error: None,
                        warnings: &warnings,
                    });
                    // Also relay each 80%-threshold budget warning on its own — the
                    // daemon's logging observer (and T8's tests) consume this
                    // per-warning callback; a publisher reads them off the transition
                    // above instead.
                    for warning in &warnings {
                        observer.on_budget_warning(&node.id, *warning, attempt);
                    }
                    return Ok(());
                }
                // A budget exhaustion: the executor charged the node's measured
                // cost and a slice/envelope was exceeded. Record the block (state,
                // measured cost, dimension reason) and pause the run — the frontier
                // loop observes `Paused` at its next boundary and stops launching
                // nodes, leaving the run resumable for a human decision. A block is
                // never retried (that would re-spend the same exhausted budget).
                NodeOutcome::Blocked { error, cost } => {
                    self.store
                        .transition_node_with_error(
                            pool,
                            workflow_run_id,
                            &node.id,
                            NodeState::Blocked,
                            attempt,
                            None,
                            cost.as_ref(),
                            Some(&error),
                        )
                        .await?;
                    observer.on_transition(NodeTransition {
                        node_id: &node.id,
                        state: NodeState::Blocked,
                        attempt,
                        cost: cost.as_ref(),
                        error: Some(&error),
                        warnings: &[],
                    });
                    // Pause only from `Running` (a concurrent cancel must win): the
                    // conditional write never clobbers a state that already moved.
                    self.store
                        .set_run_state_if_legal(
                            pool,
                            workflow_run_id,
                            &[WorkflowRunState::Running],
                            WorkflowRunState::Paused,
                        )
                        .await?;
                    return Ok(());
                }
                // A retryable failure: report the intermediate reason to the
                // observer (the durable `error` column keeps only the latest, so
                // per-attempt history is the observer's), honour the declared
                // backoff, then bump the attempt and re-drive (the store records
                // the new attempt on the next `Running` transition). A zero backoff
                // (the default) waits not at all.
                NodeOutcome::Failed { error } if attempt < max_attempts => {
                    observer.on_attempt_failed(&node.id, attempt, &error);
                    if node.retry.backoff_seconds > 0 {
                        tokio::time::sleep(std::time::Duration::from_secs(
                            node.retry.backoff_seconds,
                        ))
                        .await;
                    }
                    attempt += 1;
                }
                // Attempts exhausted: mark the node `Failed`, persisting the final
                // reason on its durable `error` column (P5-D4 — the reason was
                // previously dropped at the driver).
                NodeOutcome::Failed { error } => {
                    self.store
                        .transition_node_with_error(
                            pool,
                            workflow_run_id,
                            &node.id,
                            NodeState::Failed,
                            attempt,
                            None,
                            None,
                            Some(&error),
                        )
                        .await?;
                    observer.on_transition(NodeTransition {
                        node_id: &node.id,
                        state: NodeState::Failed,
                        attempt,
                        cost: None,
                        error: Some(&error),
                        warnings: &[],
                    });
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

    /// Records the transition sequence — and the three non-transition observer
    /// events (budget warning, intermediate attempt failure, recovery reset) — so
    /// a test can assert lifecycle order and that the P5-D4 gaps now report.
    #[derive(Default)]
    struct RecordingObserver {
        seen: Mutex<Vec<(String, NodeState, u32)>>,
        warnings: Mutex<Vec<(String, crate::budget::BudgetWarning, u32)>>,
        attempt_failures: Mutex<Vec<(String, u32, String)>>,
        recovery_resets: Mutex<Vec<(String, u32)>>,
    }

    impl NodeObserver for RecordingObserver {
        fn on_transition(&self, transition: NodeTransition<'_>) {
            self.seen.lock().unwrap().push((
                transition.node_id.to_owned(),
                transition.state,
                transition.attempt,
            ));
        }

        fn on_budget_warning(
            &self,
            node_id: &str,
            warning: crate::budget::BudgetWarning,
            attempt: u32,
        ) {
            self.warnings
                .lock()
                .unwrap()
                .push((node_id.to_owned(), warning, attempt));
        }

        fn on_attempt_failed(&self, node_id: &str, attempt: u32, error: &str) {
            self.attempt_failures.lock().unwrap().push((
                node_id.to_owned(),
                attempt,
                error.to_owned(),
            ));
        }

        fn on_recovery_reset(&self, node_id: &str, attempt: u32) {
            self.recovery_resets
                .lock()
                .unwrap()
                .push((node_id.to_owned(), attempt));
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
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();

        // `b` completes with a recorded cost + agent-run id (node-level provenance).
        let executor = ScriptedExecutor::default().with(
            "b",
            vec![NodeOutcome::Completed {
                agent_run_id: Some("run-b".to_owned()),
                cost: Some(json!({ "usd": 0.02 })),
                warnings: Vec::new(),
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
            .create_run(&pool, &compiled, None, &json!({}), None)
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
            .create_run(&pool, &compiled, None, &json!({}), None)
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
    async fn resume_honors_the_persisted_attempt_count() {
        // A node crashed while running attempt 2 of a 2-attempt policy: it is left
        // `Running` with attempt 2 in the store. On resume it must run at most once
        // more (attempt 2) and then fail — never restart at attempt 1 and execute a
        // third time.
        let manifest = "\
schema_version: 1
id: flaky
version: 1
steps:
  - id: a
    tool: repository.test
    retry:
      attempts: 2
";
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(manifest).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();

        // Simulate the crash: `a` was interrupted while running attempt 2.
        store
            .transition_node(&pool, &run_id, "a", NodeState::Running, 2, None, None)
            .await
            .unwrap();

        // An executor that always fails `a`. Without honouring the persisted
        // attempt this would run twice more (a fresh 1 then 2); with the fix it runs
        // exactly once (attempt 2) and then exhausts the policy.
        let executor = ScriptedExecutor::default().with(
            "a",
            vec![
                NodeOutcome::failed("still broken"),
                NodeOutcome::failed("still broken"),
            ],
        );
        let final_state = WorkflowDriver::new()
            .run(&pool, &run_id, &compiled, &executor)
            .await
            .unwrap();

        assert_eq!(final_state, WorkflowRunState::Failed);
        assert_eq!(
            executor.calls_for("a"),
            1,
            "a runs once more (attempt 2), not a fresh attempt 1 + 2"
        );
        let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
        let a = snap.nodes.iter().find(|n| n.node_id == "a").unwrap();
        assert_eq!(a.state, NodeState::Failed);
        assert_eq!(a.attempt, 2);
    }

    #[tokio::test]
    async fn resume_after_failure_does_not_re_run_completed_nodes() {
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(LINEAR).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}), None)
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

    /// An executor that flips its run to `Paused` as a chosen node's work
    /// completes — modelling a `pause` command arriving mid-drive. It shares the
    /// pool so it can perform the same `set_run_state` the conductor's `pause`
    /// does, letting the driver's cooperative stop be exercised deterministically.
    struct PauseAfterExecutor {
        pool: SqlitePool,
        pause_after: String,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl NodeExecutor for PauseAfterExecutor {
        async fn execute(&self, ctx: NodeContext<'_>) -> NodeOutcome {
            self.calls.lock().unwrap().push(ctx.node.id.clone());
            if ctx.node.id == self.pause_after {
                WorkflowStore::new()
                    .set_run_state(&self.pool, ctx.workflow_run_id, WorkflowRunState::Paused)
                    .await
                    .expect("pause the run mid-drive");
            }
            NodeOutcome::completed()
        }
    }

    #[tokio::test]
    async fn a_pause_mid_drive_stops_the_frontier_and_preserves_progress() {
        // Linear a→b→c. A pause lands as `a` completes; the driver must observe it
        // at the next scheduling boundary and stop before `b`, leaving the run
        // `Paused` (never overwritten to `Completed`/`Failed`) with `b`/`c` pending
        // and resumable.
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(LINEAR).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();

        let executor = PauseAfterExecutor {
            pool: pool.clone(),
            pause_after: "a".to_owned(),
            calls: Mutex::new(Vec::new()),
        };
        let final_state = WorkflowDriver::new()
            .run(&pool, &run_id, &compiled, &executor)
            .await
            .unwrap();

        assert_eq!(final_state, WorkflowRunState::Paused);
        let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(snap.run.state, WorkflowRunState::Paused);
        let state = |id: &str| snap.nodes.iter().find(|n| n.node_id == id).unwrap().state;
        assert_eq!(state("a"), NodeState::Completed);
        assert_eq!(
            state("b"),
            NodeState::Pending,
            "b must not start after pause"
        );
        assert_eq!(state("c"), NodeState::Pending);
        let ran = executor.calls.lock().unwrap().clone();
        assert_eq!(ran, vec!["a"], "only a ran before the pause took effect");
    }

    #[tokio::test]
    async fn a_diamond_runs_both_branches_before_the_join() {
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(DIAMOND).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}), None)
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

    #[tokio::test]
    async fn driving_a_paused_run_directly_is_a_no_op() {
        // P5-D5: calling the library-level driver directly (bypassing the
        // conductor's `prepare_resume`) on a `Paused` run must not resurrect it
        // — a paused run awaits an explicit resume.
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(LINEAR).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();
        store
            .set_run_state(&pool, &run_id, WorkflowRunState::Paused)
            .await
            .unwrap();

        let executor = ScriptedExecutor::default();
        let final_state = WorkflowDriver::new()
            .run(&pool, &run_id, &compiled, &executor)
            .await
            .unwrap();

        assert_eq!(
            final_state,
            WorkflowRunState::Paused,
            "reported unchanged, not resurrected to Running/Completed"
        );
        assert!(!executor.ran("a"), "a paused run must not execute any node");
        let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(
            snap.run.state,
            WorkflowRunState::Paused,
            "the persisted state is untouched"
        );
    }

    #[tokio::test]
    async fn driving_a_terminal_run_directly_is_a_no_op() {
        // P5-D5: likewise for every terminal state — driving an already-finished
        // run directly must not flicker it back to `Running` before landing on
        // a (possibly different) terminal state again.
        for terminal in [
            WorkflowRunState::Completed,
            WorkflowRunState::Failed,
            WorkflowRunState::Cancelled,
        ] {
            let (_tmp, pool) = temp_pool().await;
            let compiled = compile_yaml(LINEAR).unwrap();
            let store = WorkflowStore::new();
            let run_id = store
                .create_run(&pool, &compiled, None, &json!({}), None)
                .await
                .unwrap();
            store.set_run_state(&pool, &run_id, terminal).await.unwrap();

            let executor = ScriptedExecutor::default();
            let final_state = WorkflowDriver::new()
                .run(&pool, &run_id, &compiled, &executor)
                .await
                .unwrap();

            assert_eq!(final_state, terminal, "{terminal:?} reported unchanged");
            assert!(!executor.ran("a"), "{terminal:?} must not execute any node");
            let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
            assert_eq!(
                snap.run.state, terminal,
                "{terminal:?} persisted state is untouched"
            );
        }
    }

    #[tokio::test]
    async fn crash_recovery_reset_explicitly_clears_a_stale_agent_run_id() {
        // P5-D6b: the crash-recovery reset (a node left `Running` by an
        // interrupted earlier drive) must explicitly clear `agent_run_id`/cost —
        // not preserve them via `transition_node`'s COALESCE semantics — so a
        // node whose reset attempt never reaches a real completion does not
        // keep pointing at a stale agent run.
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(LINEAR).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();

        // Simulate a node stuck `Running` from an earlier interrupted drive,
        // carrying a stale agent_run_id/cost from that attempt (transition_node's
        // COALESCE-preserve lets a Running transition carry a value forward).
        store
            .transition_node(
                &pool,
                &run_id,
                "a",
                NodeState::Running,
                1,
                Some("ghost-agent-run"),
                Some(&json!({ "usd": 0.5 })),
            )
            .await
            .unwrap();
        store
            .set_run_state(&pool, &run_id, WorkflowRunState::Running)
            .await
            .unwrap();

        // Drive with an executor whose outcome for `a` carries no agent_run_id
        // or cost (a node that "never re-reaches agent execution" this attempt).
        let executor = ScriptedExecutor::default();
        let final_state = WorkflowDriver::new()
            .run(&pool, &run_id, &compiled, &executor)
            .await
            .unwrap();
        assert_eq!(final_state, WorkflowRunState::Completed);

        let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
        let a = snap.nodes.iter().find(|n| n.node_id == "a").unwrap();
        assert_eq!(
            a.agent_run_id, None,
            "the crash-recovery reset must not leave the stale agent_run_id in place"
        );
        assert_eq!(
            a.cost, None,
            "the crash-recovery reset must not leave the stale cost in place"
        );
    }

    #[tokio::test]
    async fn try_transition_to_running_refuses_a_run_that_moved_since_the_initial_read() {
        // P5-D5 follow-up (lost-pause race): deterministic reproduction of the
        // exact window a real race hits — no scheduling luck, same discipline
        // as the FP-3 fix. `run_observed`'s initial snapshot read sees
        // Pending/Running (passing the P5-D5 guard), but a `PauseWorkflow`
        // command — which deliberately does not take the per-run drive lock,
        // P5-D3 — commits `Paused` before `run_observed` reaches this exact
        // method. Simulated here by committing that "concurrent" pause
        // directly and sequentially (no real concurrency needed to prove the
        // write itself refuses), then calling the SAME method
        // `run_observed` calls at that point.
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(LINEAR).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();
        store
            .set_run_state(&pool, &run_id, WorkflowRunState::Paused)
            .await
            .unwrap();

        let driver = WorkflowDriver::new();
        let transitioned = driver
            .try_transition_to_running(&pool, &run_id)
            .await
            .unwrap();
        assert!(!transitioned, "must not transition once the run has moved");

        let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(
            snap.run.state,
            WorkflowRunState::Paused,
            "the run must still be Paused, never clobbered back to Running"
        );
    }

    #[tokio::test]
    async fn a_blocked_outcome_records_the_node_blocked_and_pauses_the_run() {
        // A budget block: the executor returns `Blocked`, so the driver records
        // the node `Blocked` (with its measured cost + reason) and pauses the run
        // — never overwritten to Failed, and never retried.
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(LINEAR).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();

        // `b` blocks on budget with a recorded cost; even with attempts:2 it must
        // NOT retry (a block re-spends the same exhausted budget).
        let manifest = "\
schema_version: 1
id: linear
version: 1
steps:
  - id: a
    tool: repository.test
  - id: b
    depends_on: [a]
    tool: repository.test
    retry:
      attempts: 2
  - id: c
    depends_on: [b]
    tool: repository.test
";
        let compiled = compile_yaml(manifest).unwrap();
        let store2 = WorkflowStore::new();
        let run_id2 = store2
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();

        struct BlockingExecutor {
            calls: Mutex<usize>,
        }
        #[async_trait]
        impl NodeExecutor for BlockingExecutor {
            async fn execute(&self, ctx: NodeContext<'_>) -> NodeOutcome {
                if ctx.node.id == "b" {
                    *self.calls.lock().unwrap() += 1;
                    NodeOutcome::blocked(
                        "workflow.budget-exceeded: node budget for `tool_calls` exceeded",
                        Some(json!({ "wall_time_secs": 1, "tool_calls": 9 })),
                    )
                } else {
                    NodeOutcome::completed()
                }
            }
        }
        let executor = BlockingExecutor {
            calls: Mutex::new(0),
        };
        let observer = RecordingObserver::default();
        let final_state = WorkflowDriver::new()
            .run_observed(&pool, &run_id2, &compiled, &executor, &observer)
            .await
            .unwrap();
        let _ = run_id; // the first run is only a fixture for the shared pool

        assert_eq!(
            final_state,
            WorkflowRunState::Paused,
            "a budget block pauses the run for a human decision"
        );
        assert_eq!(
            *executor.calls.lock().unwrap(),
            1,
            "a block is never retried, even under attempts:2"
        );
        let snap = store2.snapshot(&pool, &run_id2).await.unwrap().unwrap();
        let b = snap.nodes.iter().find(|n| n.node_id == "b").unwrap();
        assert_eq!(b.state, NodeState::Blocked);
        assert!(
            b.error.as_deref().unwrap_or_default().contains("budget"),
            "the block reason is persisted: {:?}",
            b.error
        );
        assert_eq!(
            b.cost,
            Some(json!({ "wall_time_secs": 1, "tool_calls": 9 })),
            "the measured cost that tipped the node over is recorded"
        );
        // `c` (b's dependent) never ran — the run paused before it.
        assert_eq!(
            snap.nodes.iter().find(|n| n.node_id == "c").unwrap().state,
            NodeState::Pending
        );
        // The block reached the observer as a Blocked transition.
        assert!(observer
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(id, state, _)| id == "b" && *state == NodeState::Blocked));
    }

    #[tokio::test]
    async fn an_intermediate_retry_failure_is_reported_to_the_observer() {
        // P5-D4: a failed attempt that will be retried was previously invisible.
        // It now reports to the observer (attempt + reason), while the durable
        // `error` column keeps only the terminal reason (here, none — it succeeds).
        let manifest = "\
schema_version: 1
id: flaky
version: 1
steps:
  - id: a
    tool: repository.test
    retry:
      attempts: 2
";
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(manifest).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();

        let executor = ScriptedExecutor::default().with(
            "a",
            vec![
                NodeOutcome::failed("transient boom"),
                NodeOutcome::completed(),
            ],
        );
        let observer = RecordingObserver::default();
        let state = WorkflowDriver::new()
            .run_observed(&pool, &run_id, &compiled, &executor, &observer)
            .await
            .unwrap();
        assert_eq!(state, WorkflowRunState::Completed);

        {
            let failures = observer.attempt_failures.lock().unwrap();
            assert_eq!(failures.len(), 1, "the one intermediate failure reported");
            assert_eq!(failures[0].0, "a");
            assert_eq!(failures[0].1, 1, "reported at attempt 1");
            assert!(failures[0].2.contains("transient"));
        }

        // The node completed, so its durable error column is clear (latest wins).
        let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(snap.nodes[0].error, None);
    }

    #[tokio::test]
    async fn a_terminal_failure_persists_its_reason_and_reports_no_intermediate() {
        // The final (exhausted) failure persists its reason on the node's durable
        // `error` column, and a single-attempt failure reports NO intermediate
        // failure (there is no retry).
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(LINEAR).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();

        let executor = ScriptedExecutor::default().with("b", vec![NodeOutcome::failed("boom")]);
        let observer = RecordingObserver::default();
        let state = WorkflowDriver::new()
            .run_observed(&pool, &run_id, &compiled, &executor, &observer)
            .await
            .unwrap();
        assert_eq!(state, WorkflowRunState::Failed);

        let snap = store.snapshot(&pool, &run_id).await.unwrap().unwrap();
        let b = snap.nodes.iter().find(|n| n.node_id == "b").unwrap();
        assert_eq!(b.state, NodeState::Failed);
        assert_eq!(
            b.error.as_deref(),
            Some("boom"),
            "the terminal failure reason is persisted (P5-D4)"
        );
        assert!(
            observer.attempt_failures.lock().unwrap().is_empty(),
            "a single-attempt failure is terminal, not an intermediate retry"
        );
    }

    #[tokio::test]
    async fn a_recovery_reset_is_reported_to_the_observer() {
        // P5-D4: a node left `Running` by an interrupted drive is reset to
        // re-drive — previously invisible, now an observer event.
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(LINEAR).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();
        // Simulate a crash: `a` stuck Running at attempt 1, the run left Running.
        store
            .transition_node(&pool, &run_id, "a", NodeState::Running, 1, None, None)
            .await
            .unwrap();
        store
            .set_run_state(&pool, &run_id, WorkflowRunState::Running)
            .await
            .unwrap();

        let observer = RecordingObserver::default();
        let state = WorkflowDriver::new()
            .run_observed(
                &pool,
                &run_id,
                &compiled,
                &ScriptedExecutor::default(),
                &observer,
            )
            .await
            .unwrap();
        assert_eq!(state, WorkflowRunState::Completed);

        let resets = observer.recovery_resets.lock().unwrap();
        assert_eq!(resets.len(), 1, "the interrupted node's reset reported");
        assert_eq!(resets[0], ("a".to_owned(), 1));
    }

    #[tokio::test]
    async fn try_transition_to_running_applies_when_nothing_raced_in() {
        // The happy path: no concurrent pause, so the transition applies
        // normally — this is what makes `start`/`resume`/`retry`/`recover`
        // still reach `Running` and drive (their pre-drive state is always
        // Pending or Running with nothing else racing in the tests that
        // exercise them).
        let (_tmp, pool) = temp_pool().await;
        let compiled = compile_yaml(LINEAR).unwrap();
        let store = WorkflowStore::new();
        let run_id = store
            .create_run(&pool, &compiled, None, &json!({}), None)
            .await
            .unwrap();

        let driver = WorkflowDriver::new();
        let transitioned = driver
            .try_transition_to_running(&pool, &run_id)
            .await
            .unwrap();
        assert!(transitioned);
        assert_eq!(
            store
                .snapshot(&pool, &run_id)
                .await
                .unwrap()
                .unwrap()
                .run
                .state,
            WorkflowRunState::Running
        );
    }
}
