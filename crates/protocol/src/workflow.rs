//! Workflow-run observability wire types (Phase 5 STEP 5.2 / T9): the
//! client-facing projection of a durable workflow run's live node lifecycle.
//!
//! A workflow run lives in `codypendent-workflow`'s durable store, **outside** the
//! session ledger — so, exactly like the blackboard (STEP 5.3), its live surface is
//! a per-run subscription, not a session-event stream. This module carries the
//! *view* of that surface across the wire:
//!
//! * [`WorkflowRunSnapshot`] — the catch-up baseline a mid-run subscriber reads
//!   through [`CommandBody::ReadWorkflowRun`](crate::command::CommandBody::ReadWorkflowRun):
//!   the run's current phase plus every node's full current view, in topological
//!   order.
//! * [`WorkflowEvent`] — one live event delivered as
//!   [`Payload::WorkflowEvent`](crate::envelope::Payload::WorkflowEvent) to the
//!   clients subscribed to the run
//!   ([`Subscription::Workflow`](crate::handshake::Subscription::Workflow)) as the
//!   driver advances the graph: a node transition (carrying the node's **full**
//!   new view) or a run-phase change.
//!
//! **Idempotent, watermark-free merge (mirrors the blackboard).** Every
//! [`WorkflowNodeView`] carries a node's *complete* current state (state, attempt,
//! measured cost, failure/block reason, budget warnings) — not a delta — so a
//! client merges each delivery by `node_id` (overwrite). A subscriber takes its
//! baseline from the snapshot and then folds live transitions on top; because each
//! transition is full-state, an overlap between the snapshot and the stream is a
//! harmless re-write, so no per-run sequence watermark is needed. The daemon
//! publishes each transition **after** it is persisted (persist-before-publish), so
//! a snapshot read after subscribing already reflects — or is superseded by — every
//! buffered live event (see the daemon's `workflow_stream` module for the full
//! contract).
//!
//! Cost rides as opaque JSON (a client renders it, never branches structurally on
//! it); the wire stays decoupled from the workflow crate's `NodeCost`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Bounded workflow discovery metadata. No manifest, prompt, or environment is exposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct WorkflowCatalog {
    /// Canonical repository used for resolution; reuse it when starting the selected workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    pub items: Vec<WorkflowCatalogEntry>,
    pub diagnostics: Vec<WorkflowCatalogDiagnostic>,
    /// Some sources could not be inspected completely. No potentially shadowed entries are offered.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct WorkflowCatalogEntry {
    /// SHA-256 of the canonical parsed definition, including its inputs and budget.
    pub definition_digest: String,
    pub id: String,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub inputs: BTreeMap<String, WorkflowCatalogInput>,
    pub node_count: u32,
    pub source: WorkflowCatalogSource,
}

/// Input types are domain names, not JSON Schema or an invented control/default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct WorkflowCatalogInput {
    pub input_type: String,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum WorkflowCatalogSource {
    BuiltIn,
    User,
    Repository,
    #[serde(other)]
    Unknown,
}

/// Static diagnostics intentionally exclude parser context and manifest scalar values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct WorkflowCatalogDiagnostic {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
}

/// The lifecycle state of one workflow node, projected for a client. Mirrors
/// `codypendent_workflow`'s `NodeState` across the wire; a value from a newer peer
/// deserializes to [`Unknown`](WorkflowNodeState::Unknown) rather than failing the
/// frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum WorkflowNodeState {
    /// Not yet scheduled.
    Pending,
    /// An attempt is executing.
    Running,
    /// Parked awaiting an approval.
    WaitingApproval,
    /// A budget dimension was exhausted; the run paused for a human decision.
    Blocked,
    /// The node's work succeeded.
    Completed,
    /// Attempts exhausted; the node failed.
    Failed,
    /// The node will never run — a `CancelWorkflow` skipped a still-pending node
    /// (T9), the one no-producer state a cancel newly produces alongside a
    /// `Cancelled` run.
    Skipped,
    #[serde(other)]
    Unknown,
}

/// The lifecycle state of a workflow **run**, projected for a client. Mirrors
/// `codypendent_workflow`'s `WorkflowRunState`; distinct from the agent-run
/// [`RunState`](crate::run::RunState), which describes a single agent run rather
/// than a durable multi-node workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum WorkflowRunPhase {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
    /// Terminal: a `CancelWorkflow` drained the run (T9). No resume from here.
    Cancelled,
    #[serde(other)]
    Unknown,
}

/// One workflow node's full current state, projected for a client.
///
/// Carried identically in a [`WorkflowRunSnapshot`] and in each live
/// [`WorkflowEvent::NodeTransitioned`], so a client applies either by
/// overwrite-by-`node_id` — an overlap between the snapshot baseline and the live
/// stream is a harmless idempotent re-write. The `workflow_run_id` travels with the
/// view so a client routes a live delivery to the right run without consulting the
/// enclosing frame (the frame is not session-scoped).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct WorkflowNodeView {
    /// The workflow run this node belongs to.
    pub workflow_run_id: String,
    /// The node (step) id, unique within its workflow.
    pub node_id: String,
    /// The node's lifecycle state.
    pub state: WorkflowNodeState,
    /// The 1-based attempt number (0 before the node has ever run).
    pub attempt: u32,
    /// The node's **measured** cost so far (opaque JSON — `wall_time_secs`,
    /// `tool_calls`; only measured dimensions, never a fabricated token/USD
    /// figure), when a run has recorded one. `None` before the node completes an
    /// attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Value>,
    /// The node's latest failure or budget-block reason, when its latest state is
    /// `Failed`/`Blocked` (a `Completed` transition clears it). `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Budget-dimension warnings raised while charging this node (each crossed 80%
    /// of a limit but stayed within it), pre-rendered. Empty when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The node ids this node depends on — the graph's **edges**, so a client can
    /// draw the DAG rather than a flat list (rubric 5). Carried on snapshot reads
    /// (the daemon recompiles the run's stored manifest to recover them); a live
    /// [`WorkflowEvent::NodeTransitioned`] omits them (the graph shape is static
    /// per run), so a client merging live deliveries must preserve the edges it
    /// learned from the snapshot rather than overwriting them with an empty list.
    /// Additive (`#[serde(default)]`): an older daemon sends none and the field
    /// parses back empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

/// A workflow run's full observable state — the catch-up baseline a mid-run
/// subscriber reads (via
/// [`ReadWorkflowRun`](crate::command::CommandBody::ReadWorkflowRun)) before folding
/// the live [`WorkflowEvent`] stream on top. Reconstructed from the durable store,
/// so a late subscriber after a daemon restart still gets a truthful baseline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct WorkflowRunSnapshot {
    /// The run this snapshot is of.
    pub workflow_run_id: String,
    /// The run's current lifecycle phase.
    pub phase: WorkflowRunPhase,
    /// Every node's full current view, in topological order.
    pub nodes: Vec<WorkflowNodeView>,
}

/// One live event on a workflow run's observability stream
/// ([`Subscription::Workflow`](crate::handshake::Subscription::Workflow)), delivered
/// as [`Payload::WorkflowEvent`](crate::envelope::Payload::WorkflowEvent).
///
/// A newer peer's variant deserializes to [`Unknown`](WorkflowEvent::Unknown) so an
/// additive event never breaks an older client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum WorkflowEvent {
    /// A node changed state — carries the node's **full** new view, so a client
    /// merges it by `node_id` (overwrite). Every driver transition (running,
    /// completed, failed, blocked) and a cancel's skip lands as one of these.
    NodeTransitioned(WorkflowNodeView),
    /// The run itself changed lifecycle phase (started running, paused, completed,
    /// failed, cancelled).
    RunPhaseChanged {
        workflow_run_id: String,
        phase: WorkflowRunPhase,
    },
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node_view() -> WorkflowNodeView {
        WorkflowNodeView {
            workflow_run_id: "wfrun-abc".to_string(),
            node_id: "inspect".to_string(),
            state: WorkflowNodeState::Completed,
            attempt: 1,
            cost: Some(json!({ "wall_time_secs": 12, "tool_calls": 3 })),
            error: None,
            warnings: vec!["tool_calls at 4/5 (80%)".to_string()],
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn node_view_round_trips() {
        let original = node_view();
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: WorkflowNodeView = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, parsed);
    }

    #[test]
    fn absent_optionals_are_skipped_and_default_back() {
        // A pending node omits cost/error/warnings, and such a payload reparses
        // with them defaulted (an older peer that sends none still round-trips).
        let item = WorkflowNodeView {
            workflow_run_id: "wfrun-abc".to_string(),
            node_id: "verify".to_string(),
            state: WorkflowNodeState::Pending,
            attempt: 0,
            cost: None,
            error: None,
            warnings: Vec::new(),
            depends_on: Vec::new(),
        };
        let json = serde_json::to_string(&item).expect("serialize");
        assert!(!json.contains("cost"), "cost skipped: {json}");
        assert!(!json.contains("error"), "error skipped: {json}");
        assert!(!json.contains("warnings"), "warnings skipped: {json}");
        assert!(!json.contains("depends_on"), "depends_on skipped: {json}");
        let parsed: WorkflowNodeView = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, item);
    }

    #[test]
    fn depends_on_rides_the_wire_when_present() {
        // The DAG edges (rubric 5): a node view carrying its dependency ids
        // round-trips them, and an older peer's payload (no key) parses back to
        // an empty list rather than failing the frame.
        let mut item = node_view();
        item.node_id = "verify".to_string();
        item.depends_on = vec!["inspect".to_string(), "plan".to_string()];
        let json = serde_json::to_string(&item).expect("serialize");
        assert!(json.contains("depends_on"), "edges on the wire: {json}");
        let parsed: WorkflowNodeView = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, item);
    }

    #[test]
    fn run_snapshot_round_trips() {
        let original = WorkflowRunSnapshot {
            workflow_run_id: "wfrun-abc".to_string(),
            phase: WorkflowRunPhase::Running,
            nodes: vec![node_view()],
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: WorkflowRunSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, parsed);
    }

    #[test]
    fn workflow_events_round_trip() {
        let transitioned = WorkflowEvent::NodeTransitioned(node_view());
        let json = serde_json::to_string(&transitioned).expect("serialize");
        assert_eq!(
            serde_json::from_str::<WorkflowEvent>(&json).expect("deserialize"),
            transitioned
        );

        let phase = WorkflowEvent::RunPhaseChanged {
            workflow_run_id: "wfrun-abc".to_string(),
            phase: WorkflowRunPhase::Cancelled,
        };
        let json = serde_json::to_string(&phase).expect("serialize");
        assert_eq!(
            serde_json::from_str::<WorkflowEvent>(&json).expect("deserialize"),
            phase
        );
    }

    #[test]
    fn unknown_tags_deserialize_to_unknown() {
        let future = json!({ "type": "FromTheFuture", "extra": 1 });
        assert!(matches!(
            serde_json::from_value::<WorkflowNodeState>(future.clone()).expect("node state"),
            WorkflowNodeState::Unknown
        ));
        assert!(matches!(
            serde_json::from_value::<WorkflowRunPhase>(future.clone()).expect("run phase"),
            WorkflowRunPhase::Unknown
        ));
        assert!(matches!(
            serde_json::from_value::<WorkflowEvent>(future).expect("event"),
            WorkflowEvent::Unknown
        ));
    }
}
