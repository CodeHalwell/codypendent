//! Hierarchical workflow budget accounting (STEP 5.5).
//!
//! A workflow declares a `budget:` envelope (manifest [`WorkflowBudget`](crate::WorkflowBudget))
//! and each agent role a `[budget]` slice (profile [`AgentBudget`](crate::AgentBudget)).
//! This module is the **pure, measured** accounting between the two: given the
//! cost a node actually consumed and the workflow's already-consumed total, it
//! decides whether the node stays within budget (perhaps crossing an 80% warning
//! line) or exceeds it — in which case the caller blocks the node and pauses the
//! run for a human decision, so an overrun is never silent.
//!
//! ## Honesty: only measured dimensions are charged
//!
//! This module charges ONLY dimensions the daemon actually measures at the node
//! boundary — it never fabricates a figure to satisfy a ceiling:
//!
//! * **wall-time** (seconds) — measured around the node's execution; nests
//!   (a node's own slice AND the workflow envelope);
//! * **tool-calls** — counted from the node's recorded tool-call events;
//!   enforced at the node slice only (the workflow envelope has no tool-call
//!   field);
//! * **cost** (micro-USD) — the model spend a node's agent run actually reported
//!   through the `ModelDriver` usage seam (Phase 7). Wall-time and tool-calls are
//!   measured for every node, so they are plain counts; cost is measured only
//!   when a run reported usage, so it is carried as an [`Option`] on
//!   [`NodeCost`] and charged **only when present**. An unmeasured node
//!   contributes NO cost — a `None`, never a real `0` — so a run in which no node
//!   reported a measured cost is charged nothing on the cost dimension, behaving
//!   identically to the pre-cost code. A `maximum_cost_usd` ceiling (node slice
//!   or workflow envelope) is therefore enforced when — and only when — real
//!   measured usage backs it, never against a fabricated zero.

use serde_json::{json, Value};

use crate::agent::AgentBudget;
use crate::model::WorkflowBudget;

/// The cost a node actually consumed, over the dimensions the daemon measures.
/// Serialized to the node record's `cost_json`, so the workflow-level
/// consumption is the sum of its nodes' `NodeCost`s (no separate ledger table).
///
/// Wall-time and tool-calls are measured for every node, so they are plain
/// counts. **Cost** is measured only when the node's agent run reported provider
/// usage through the `ModelDriver` seam, so it is an [`Option`]: `None` means
/// "this node reported no cost" (never charged — never a real `0`), `Some(micros)`
/// a real measured spend (`Some(0)` is a genuine measured zero, distinct from
/// `None`). `Value` round-trips are lenient: a missing `wall_time_secs`/
/// `tool_calls` reads `0`, and a missing `cost_micros` reads `None` — so an older
/// record (written before the cost dimension existed) is charged as "cost not
/// measured", never a spurious debit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeCost {
    /// Wall-clock seconds the node ran.
    pub wall_time_secs: u64,
    /// Tool calls the node made (agent tool calls, or the tool node's own call).
    pub tool_calls: u64,
    /// Measured model spend in micro-USD, or `None` when the node reported no
    /// usage. Accumulated across a run's model requests only when measured, so an
    /// unmeasured node never contributes a real `0` toward a cost ceiling.
    pub cost_micros: Option<u64>,
}

impl NodeCost {
    /// The zero cost — a node that has not run yet, or a pre-gate probe.
    #[must_use]
    pub fn zero() -> Self {
        Self::default()
    }

    /// This cost as the `cost_json` value the node record stores. Renders the
    /// measured dimensions only: wall-time and tool-calls always, and
    /// `cost_micros` **only when measured** — an unmeasured cost is omitted, so a
    /// reader (or a re-charge on resume) never mistakes it for a real zero spend.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = serde_json::Map::new();
        object.insert("wall_time_secs".to_string(), json!(self.wall_time_secs));
        object.insert("tool_calls".to_string(), json!(self.tool_calls));
        if let Some(cost_micros) = self.cost_micros {
            object.insert("cost_micros".to_string(), json!(cost_micros));
        }
        Value::Object(object)
    }

    /// Read a `NodeCost` back from a stored `cost_json` value. A missing or
    /// non-numeric `wall_time_secs`/`tool_calls` reads as zero (lenient — see the
    /// type docs). A missing `cost_micros` reads as `None` ("cost not measured",
    /// never a spurious zero debit); present-and-numeric reads the measured spend.
    /// A `null` or unrelated shape yields the zero cost with an unmeasured cost.
    #[must_use]
    pub fn from_json(value: &Value) -> Self {
        let field = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
        Self {
            wall_time_secs: field("wall_time_secs"),
            tool_calls: field("tool_calls"),
            cost_micros: value.get("cost_micros").and_then(Value::as_u64),
        }
    }

    /// The sum of two costs, saturating (a budget total can never wrap to a
    /// spuriously small value that would let an exhausted run keep going). Cost
    /// sums as a MEASURED value: two unmeasured costs stay `None`, and any
    /// measured side carries through, so a run's summed cost is `Some` iff at
    /// least one node reported a spend (never a fabricated zero).
    #[must_use]
    pub fn saturating_add(&self, other: &Self) -> Self {
        Self {
            wall_time_secs: self.wall_time_secs.saturating_add(other.wall_time_secs),
            tool_calls: self.tool_calls.saturating_add(other.tool_calls),
            cost_micros: add_measured_cost(self.cost_micros, other.cost_micros),
        }
    }
}

/// Sum two optional measured costs, preserving "not measured": two `None`s stay
/// `None` (neither side measured a spend), while any measured side carries
/// through. Summing a run's node costs therefore charges only the spend actually
/// reported, and an all-unmeasured set stays `None` — charged nothing.
fn add_measured_cost(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(x), Some(y)) => Some(x.saturating_add(y)),
    }
}

/// Which measured dimension a budget event is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetDimension {
    /// Wall-clock seconds.
    WallTime,
    /// Tool-call count.
    ToolCalls,
    /// Measured model spend, in micro-USD.
    Cost,
}

impl BudgetDimension {
    /// A stable lowercase name for logs, observer events, and block reasons.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetDimension::WallTime => "wall_time_secs",
            BudgetDimension::ToolCalls => "tool_calls",
            BudgetDimension::Cost => "cost_micros",
        }
    }
}

/// Which level of the nested budget an event is about: the node's own `[budget]`
/// slice, or the workflow's `budget:` envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetScope {
    /// The node's own profile `[budget]` slice.
    Node,
    /// The workflow-level `budget:` envelope (across all nodes).
    Workflow,
}

impl BudgetScope {
    /// A stable lowercase name for logs and observer events.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetScope::Node => "node",
            BudgetScope::Workflow => "workflow",
        }
    }
}

/// One budget line crossing its 80% warning threshold (but not yet exceeded).
/// Carried out of [`BudgetLimits::charge`] to the [`NodeObserver`](crate::NodeObserver)
/// so a client (today, the daemon log) learns a run is nearing a ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetWarning {
    /// Which measured dimension.
    pub dimension: BudgetDimension,
    /// Which level of the nested budget.
    pub scope: BudgetScope,
    /// The consumed amount (at or above 80% of `limit`).
    pub used: u64,
    /// The ceiling.
    pub limit: u64,
}

/// A dimension exceeding its ceiling: the node blocks and the run pauses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetExceeded {
    /// Which measured dimension tipped over.
    pub dimension: BudgetDimension,
    /// Which level of the nested budget was breached.
    pub scope: BudgetScope,
    /// The consumed amount (strictly greater than `limit`).
    pub used: u64,
    /// The ceiling that was exceeded.
    pub limit: u64,
}

impl BudgetExceeded {
    /// A legible one-line reason for the durable node `error` column and the
    /// block message a paused-run reader sees.
    #[must_use]
    pub fn reason(&self) -> String {
        format!(
            "workflow.budget-exceeded: {} budget for `{}` exceeded ({} used, limit {})",
            self.scope.as_str(),
            self.dimension.as_str(),
            self.used,
            self.limit
        )
    }
}

/// The verdict of charging a node's measured cost against the nested budgets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetVerdict {
    /// Within every ceiling; `warnings` lists any dimension at or above 80%.
    Within { warnings: Vec<BudgetWarning> },
    /// A ceiling was exceeded — the caller blocks the node and pauses the run.
    /// The first breach found (node slice before workflow envelope, wall-time
    /// before tool-calls) is reported; one legible cause is enough to pause.
    Exceeded(BudgetExceeded),
}

/// The nested budget ceilings in force for one node: its own profile `[budget]`
/// slice plus the workflow `budget:` envelope. Every field is optional — a
/// dimension with no ceiling is never charged (an absent budget means "no
/// limit", exactly as the manifest/profile intend).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetLimits {
    /// The node slice's wall-time ceiling (`AgentBudget::maximum_duration_seconds`).
    pub node_max_wall_secs: Option<u64>,
    /// The node slice's tool-call ceiling (`AgentBudget::maximum_tool_calls`).
    pub node_max_tool_calls: Option<u64>,
    /// The node slice's cost ceiling in micro-USD (`AgentBudget::maximum_cost_usd`),
    /// charged against the node's own MEASURED spend only.
    pub node_max_cost_micros: Option<u64>,
    /// The workflow envelope's wall-time ceiling
    /// (`WorkflowBudget::maximum_duration_seconds`), charged against the sum of
    /// every node's wall-time.
    pub workflow_max_wall_secs: Option<u64>,
    /// The workflow envelope's cost ceiling in micro-USD
    /// (`WorkflowBudget::maximum_cost_usd`), charged against the summed MEASURED
    /// spend of every node.
    pub workflow_max_cost_micros: Option<u64>,
}

impl BudgetLimits {
    /// Build the nested limits from the workflow envelope and (for an agent node)
    /// its resolved profile slice. A tool node passes `node = None`: it has no
    /// role and so no slice, and is charged against the workflow envelope only.
    ///
    /// `maximum_cost_usd` on either budget is resolved to a micro-USD cost ceiling
    /// (Phase 7) and enforced against MEASURED spend only — an unmeasured run is
    /// never charged against it (see [`Self::charge`] and the module docs).
    /// `maximum_agents` is a concurrency cap the scheduler enforces, not a
    /// per-node debit, so it is not a budget dimension.
    #[must_use]
    pub fn resolve(workflow: &WorkflowBudget, node: Option<&AgentBudget>) -> Self {
        Self {
            node_max_wall_secs: node.and_then(|b| b.maximum_duration_seconds),
            node_max_tool_calls: node.and_then(|b| b.maximum_tool_calls),
            node_max_cost_micros: node.and_then(|b| b.maximum_cost_usd).map(usd_to_micros),
            workflow_max_wall_secs: workflow.maximum_duration_seconds,
            workflow_max_cost_micros: workflow.maximum_cost_usd.map(usd_to_micros),
        }
    }

    /// Whether any ceiling is set. When nothing is limited, the caller can skip
    /// the whole budget dance (no measurement is wasted enforcing an absent cap).
    #[must_use]
    pub fn is_unbounded(&self) -> bool {
        self.node_max_wall_secs.is_none()
            && self.node_max_tool_calls.is_none()
            && self.node_max_cost_micros.is_none()
            && self.workflow_max_wall_secs.is_none()
            && self.workflow_max_cost_micros.is_none()
    }

    /// Charge `node_cost` against the nested budgets, given the workflow's
    /// consumption from **every other** node (`others`). The node's own wall-time
    /// and tool-calls are checked against its slice; the workflow envelope is
    /// checked against `others + node_cost` wall-time.
    ///
    /// **Exceed wins over warn**, and the node slice is checked before the
    /// envelope, so the first legible breach pauses the run. Otherwise every
    /// dimension at or above 80% of its ceiling yields a [`BudgetWarning`].
    ///
    /// Passing `NodeCost::zero()` as `node_cost` is the pre-gate: it asks "is the
    /// budget already exhausted by the other nodes alone?" without attributing any
    /// new cost to this node.
    #[must_use]
    pub fn charge(&self, others: &NodeCost, node_cost: &NodeCost) -> BudgetVerdict {
        let mut warnings = Vec::new();

        // Node slice: this node's own consumption against its own ceilings.
        for (limit, used, dimension) in [
            (
                self.node_max_wall_secs,
                node_cost.wall_time_secs,
                BudgetDimension::WallTime,
            ),
            (
                self.node_max_tool_calls,
                node_cost.tool_calls,
                BudgetDimension::ToolCalls,
            ),
        ] {
            if let Some(exceeded) = check(BudgetScope::Node, dimension, used, limit, &mut warnings)
            {
                return BudgetVerdict::Exceeded(exceeded);
            }
        }
        // Node cost — the honesty gate: charged ONLY when this node reported a
        // measured spend. An unmeasured cost (`None`) is skipped entirely, so it
        // is never a real `0` that could satisfy or exceed the ceiling.
        if let Some(used) = node_cost.cost_micros {
            if let Some(exceeded) = check(
                BudgetScope::Node,
                BudgetDimension::Cost,
                used,
                self.node_max_cost_micros,
                &mut warnings,
            ) {
                return BudgetVerdict::Exceeded(exceeded);
            }
        }

        // Workflow envelope: every node's wall-time and measured spend summed.
        let totals = others.saturating_add(node_cost);
        if let Some(exceeded) = check(
            BudgetScope::Workflow,
            BudgetDimension::WallTime,
            totals.wall_time_secs,
            self.workflow_max_wall_secs,
            &mut warnings,
        ) {
            return BudgetVerdict::Exceeded(exceeded);
        }
        // Workflow cost — the honesty gate again: charged ONLY when the summed
        // cost is measured (`Some` iff at least one node reported a spend). An
        // all-unmeasured run stays `None` here and is charged nothing on cost,
        // exactly as the pre-cost code behaved.
        if let Some(used) = totals.cost_micros {
            if let Some(exceeded) = check(
                BudgetScope::Workflow,
                BudgetDimension::Cost,
                used,
                self.workflow_max_cost_micros,
                &mut warnings,
            ) {
                return BudgetVerdict::Exceeded(exceeded);
            }
        }

        BudgetVerdict::Within { warnings }
    }
}

/// Convert a `maximum_cost_usd` ceiling to micro-USD, the unit measured cost is
/// charged in. A non-finite or non-positive value (a nonsensical config) clamps
/// to `0`; the float→int cast saturates, so a huge ceiling never wraps.
fn usd_to_micros(usd: f64) -> u64 {
    if !usd.is_finite() || usd <= 0.0 {
        return 0;
    }
    (usd * 1_000_000.0).round() as u64
}

/// Check one dimension against one ceiling: `Some(exceeded)` when `used > limit`,
/// else push an 80% [`BudgetWarning`] when at/over the threshold and return
/// `None`. An absent ceiling (`None`) is unbounded — never charged.
fn check(
    scope: BudgetScope,
    dimension: BudgetDimension,
    used: u64,
    limit: Option<u64>,
    warnings: &mut Vec<BudgetWarning>,
) -> Option<BudgetExceeded> {
    let limit = limit?;
    if used > limit {
        return Some(BudgetExceeded {
            dimension,
            scope,
            used,
            limit,
        });
    }
    // 80% warning line: `used * 5 >= limit * 4` avoids float rounding at the
    // boundary (a cap of 5 warns at exactly 4). A zero limit is exceeded above,
    // never reaches here, so the multiply is safe.
    if used > 0 && used.saturating_mul(5) >= limit.saturating_mul(4) {
        warnings.push(BudgetWarning {
            dimension,
            scope,
            used,
            limit,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(wall: Option<u64>, tools: Option<u64>) -> BudgetLimits {
        BudgetLimits {
            node_max_wall_secs: wall,
            node_max_tool_calls: tools,
            node_max_cost_micros: None,
            workflow_max_wall_secs: None,
            workflow_max_cost_micros: None,
        }
    }

    /// Node/workflow cost ceilings (micro-USD) with no wall/tool ceilings — the
    /// shape the cost-dimension tests charge against.
    fn cost_slice(node_cost: Option<u64>, workflow_cost: Option<u64>) -> BudgetLimits {
        BudgetLimits {
            node_max_wall_secs: None,
            node_max_tool_calls: None,
            node_max_cost_micros: node_cost,
            workflow_max_wall_secs: None,
            workflow_max_cost_micros: workflow_cost,
        }
    }

    /// A `NodeCost` with the given wall-time and tool-calls and NO measured cost —
    /// the common shape for the wall/tool budget tests.
    fn nc(wall: u64, tools: u64) -> NodeCost {
        NodeCost {
            wall_time_secs: wall,
            tool_calls: tools,
            cost_micros: None,
        }
    }

    /// A `NodeCost` carrying a MEASURED cost (micro-USD) and no wall/tool usage.
    fn measured_cost(micros: u64) -> NodeCost {
        NodeCost {
            wall_time_secs: 0,
            tool_calls: 0,
            cost_micros: Some(micros),
        }
    }

    #[test]
    fn cost_json_round_trips_measured_dimensions_and_reads_leniently() {
        // An unmeasured cost renders ONLY wall-time + tool-calls — never a
        // fabricated (zero) cost field — and reads back as `None`.
        let unmeasured = nc(12, 3);
        let json = unmeasured.to_json();
        assert_eq!(json.as_object().unwrap().len(), 2);
        assert_eq!(json.get("cost_micros"), None);
        assert_eq!(NodeCost::from_json(&json), unmeasured);

        // A MEASURED cost renders the cost field and round-trips exactly (a
        // measured zero survives as `Some(0)`, distinct from unmeasured `None`).
        let measured = NodeCost {
            wall_time_secs: 12,
            tool_calls: 3,
            cost_micros: Some(4_500),
        };
        let json = measured.to_json();
        assert_eq!(json.as_object().unwrap().len(), 3);
        assert_eq!(NodeCost::from_json(&json), measured);
        let zero_cost = NodeCost {
            wall_time_secs: 0,
            tool_calls: 0,
            cost_micros: Some(0),
        };
        assert_eq!(NodeCost::from_json(&zero_cost.to_json()), zero_cost);

        // Missing wall/tool fields read zero; a missing cost field reads `None`
        // (an older, pre-cost record is "cost not measured", never a spurious 0).
        assert_eq!(
            NodeCost::from_json(&serde_json::json!({ "tool_calls": 5 })),
            nc(0, 5)
        );
        assert_eq!(NodeCost::from_json(&nc(1, 2).to_json()).cost_micros, None);
        // A present, numeric cost field reads the measured spend.
        assert_eq!(
            NodeCost::from_json(&serde_json::json!({ "cost_micros": 7_000 })).cost_micros,
            Some(7_000)
        );
        // An unrelated shape (e.g. an old opaque cost) is the zero cost, unmeasured.
        assert_eq!(
            NodeCost::from_json(&serde_json::json!({ "usd": 0.02 })),
            NodeCost::zero()
        );
        assert_eq!(NodeCost::zero().cost_micros, None);
    }

    #[test]
    fn within_budget_reports_no_warning_below_eighty_percent() {
        let limits = slice(Some(100), Some(10));
        let verdict = limits.charge(&NodeCost::zero(), &nc(50, 5));
        assert_eq!(verdict, BudgetVerdict::Within { warnings: vec![] });
    }

    #[test]
    fn crossing_eighty_percent_warns_without_exceeding() {
        // 4 of 5 tool calls == 80% exactly → a warning, still Within.
        let limits = slice(None, Some(5));
        let verdict = limits.charge(&NodeCost::zero(), &nc(0, 4));
        match verdict {
            BudgetVerdict::Within { warnings } => {
                assert_eq!(warnings.len(), 1);
                assert_eq!(warnings[0].dimension, BudgetDimension::ToolCalls);
                assert_eq!(warnings[0].scope, BudgetScope::Node);
                assert_eq!((warnings[0].used, warnings[0].limit), (4, 5));
            }
            other => panic!("expected a warning, got {other:?}"),
        }
    }

    #[test]
    fn exceeding_the_node_tool_call_slice_blocks() {
        // 2 tool calls against a slice of 1 → exceeded (the deterministic
        // scripted-driver enforcement path).
        let limits = slice(None, Some(1));
        let verdict = limits.charge(&NodeCost::zero(), &nc(0, 2));
        assert_eq!(
            verdict,
            BudgetVerdict::Exceeded(BudgetExceeded {
                dimension: BudgetDimension::ToolCalls,
                scope: BudgetScope::Node,
                used: 2,
                limit: 1,
            })
        );
    }

    #[test]
    fn exceeding_the_node_wall_slice_blocks_deterministically() {
        // Wall-time enforcement without any real elapsed time: the pure charge
        // takes a measured cost, so a 10s measured against a 5s slice exceeds.
        let limits = slice(Some(5), None);
        let verdict = limits.charge(&NodeCost::zero(), &nc(10, 0));
        assert_eq!(
            verdict,
            BudgetVerdict::Exceeded(BudgetExceeded {
                dimension: BudgetDimension::WallTime,
                scope: BudgetScope::Node,
                used: 10,
                limit: 5,
            })
        );
    }

    #[test]
    fn within_the_cost_slice_below_eighty_percent_reports_no_warning() {
        // 0.5 USD measured against a 1 USD node slice → within, no warning.
        let limits = cost_slice(Some(1_000_000), None);
        let verdict = limits.charge(&NodeCost::zero(), &measured_cost(500_000));
        assert_eq!(verdict, BudgetVerdict::Within { warnings: vec![] });
    }

    #[test]
    fn crossing_eighty_percent_of_the_cost_slice_warns_without_exceeding() {
        // 0.8 USD == 80% of a 1 USD node slice → a warning, still Within.
        let limits = cost_slice(Some(1_000_000), None);
        let verdict = limits.charge(&NodeCost::zero(), &measured_cost(800_000));
        match verdict {
            BudgetVerdict::Within { warnings } => {
                assert_eq!(warnings.len(), 1);
                assert_eq!(warnings[0].dimension, BudgetDimension::Cost);
                assert_eq!(warnings[0].scope, BudgetScope::Node);
                assert_eq!((warnings[0].used, warnings[0].limit), (800_000, 1_000_000));
            }
            other => panic!("expected a cost warning, got {other:?}"),
        }
    }

    #[test]
    fn exceeding_the_node_cost_slice_blocks() {
        // 2 USD measured against a 1 USD node slice → the cost dimension exceeds.
        let limits = cost_slice(Some(1_000_000), None);
        let verdict = limits.charge(&NodeCost::zero(), &measured_cost(2_000_000));
        assert_eq!(
            verdict,
            BudgetVerdict::Exceeded(BudgetExceeded {
                dimension: BudgetDimension::Cost,
                scope: BudgetScope::Node,
                used: 2_000_000,
                limit: 1_000_000,
            })
        );
    }

    #[test]
    fn the_workflow_cost_envelope_nests_over_all_nodes() {
        // A 1 USD workflow cost envelope. Others already spent 0.9 USD, this node
        // 0.2 USD → 1.1 USD > 1 USD → the WORKFLOW cost scope is exceeded.
        let limits = cost_slice(None, Some(1_000_000));
        let verdict = limits.charge(&measured_cost(900_000), &measured_cost(200_000));
        assert_eq!(
            verdict,
            BudgetVerdict::Exceeded(BudgetExceeded {
                dimension: BudgetDimension::Cost,
                scope: BudgetScope::Workflow,
                used: 1_100_000,
                limit: 1_000_000,
            })
        );
    }

    #[test]
    fn an_all_unmeasured_run_charges_no_cost_even_under_a_ceiling() {
        // THE HONESTY GATE: cost ceilings ARE set on both the node slice and the
        // workflow envelope, yet no node reported a measured cost. Charging costs
        // that are all `None` must be Within with no warning — an unmeasured cost
        // is never a real `0` against the ceiling, so the run behaves exactly as
        // the pre-cost code (cost simply not charged).
        let limits = cost_slice(Some(1_000_000), Some(1_000_000));
        assert!(!limits.is_unbounded(), "the ceilings ARE configured");
        let verdict = limits.charge(&NodeCost::zero(), &NodeCost::zero());
        assert_eq!(verdict, BudgetVerdict::Within { warnings: vec![] });
        // And a many-node aggregate of unmeasured costs is still Within (the
        // pre-gate `others` sum stays `None`, never a fabricated zero debit).
        let others = NodeCost::zero()
            .saturating_add(&nc(5, 5))
            .saturating_add(&nc(5, 5));
        assert_eq!(
            others.cost_micros, None,
            "unmeasured costs never sum to Some"
        );
        assert_eq!(
            limits.charge(&others, &nc(1, 1)),
            BudgetVerdict::Within { warnings: vec![] }
        );
        // The SAME ceiling DOES block once a real cost is measured over it — the
        // ceiling is live, just never charged against an unmeasured spend.
        assert!(matches!(
            limits.charge(&NodeCost::zero(), &measured_cost(2_000_000)),
            BudgetVerdict::Exceeded(BudgetExceeded {
                dimension: BudgetDimension::Cost,
                ..
            })
        ));
    }

    #[test]
    fn the_workflow_envelope_nests_over_all_nodes() {
        // No node slice; a workflow wall envelope of 100. Others already consumed
        // 90, this node 20 → 110 > 100 → the WORKFLOW scope is exceeded.
        let limits = BudgetLimits {
            node_max_wall_secs: None,
            node_max_tool_calls: None,
            node_max_cost_micros: None,
            workflow_max_wall_secs: Some(100),
            workflow_max_cost_micros: None,
        };
        let verdict = limits.charge(&nc(90, 0), &nc(20, 0));
        assert_eq!(
            verdict,
            BudgetVerdict::Exceeded(BudgetExceeded {
                dimension: BudgetDimension::WallTime,
                scope: BudgetScope::Workflow,
                used: 110,
                limit: 100,
            })
        );
    }

    #[test]
    fn the_pre_gate_detects_an_already_exhausted_envelope() {
        // The pre-gate charges a ZERO node cost: if the other nodes alone already
        // blew the envelope, the node blocks before running (the resume-re-block
        // path).
        let limits = BudgetLimits {
            node_max_wall_secs: None,
            node_max_tool_calls: None,
            node_max_cost_micros: None,
            workflow_max_wall_secs: Some(100),
            workflow_max_cost_micros: None,
        };
        assert!(matches!(
            limits.charge(&nc(101, 0), &NodeCost::zero()),
            BudgetVerdict::Exceeded(BudgetExceeded {
                scope: BudgetScope::Workflow,
                ..
            })
        ));
    }

    #[test]
    fn the_node_slice_is_checked_before_the_workflow_envelope() {
        // Both would fail; the node slice breach is reported first (one legible
        // cause is enough to pause).
        let limits = BudgetLimits {
            node_max_wall_secs: Some(5),
            node_max_tool_calls: None,
            node_max_cost_micros: None,
            workflow_max_wall_secs: Some(5),
            workflow_max_cost_micros: None,
        };
        let verdict = limits.charge(&NodeCost::zero(), &nc(10, 0));
        assert!(matches!(
            verdict,
            BudgetVerdict::Exceeded(BudgetExceeded {
                scope: BudgetScope::Node,
                ..
            })
        ));
    }

    #[test]
    fn an_unbounded_budget_charges_nothing() {
        let limits = BudgetLimits::default();
        assert!(limits.is_unbounded());
        assert_eq!(
            limits.charge(&nc(9_999, 9_999), &nc(9_999, 9_999)),
            BudgetVerdict::Within { warnings: vec![] }
        );
    }

    #[test]
    fn resolve_reads_cost_and_both_budgets() {
        let workflow = WorkflowBudget {
            maximum_cost_usd: Some(5.0),
            maximum_duration_seconds: Some(3600),
            maximum_agents: Some(2),
        };
        let node = AgentBudget {
            maximum_cost_usd: Some(3.0),
            maximum_duration_seconds: Some(1800),
            maximum_tool_calls: Some(80),
        };
        let limits = BudgetLimits::resolve(&workflow, Some(&node));
        assert_eq!(limits.node_max_wall_secs, Some(1800));
        assert_eq!(limits.node_max_tool_calls, Some(80));
        // maximum_cost_usd is now RESOLVED to a micro-USD ceiling (no longer
        // dropped): 3 USD → 3_000_000 micros, 5 USD → 5_000_000 micros.
        assert_eq!(limits.node_max_cost_micros, Some(3_000_000));
        assert_eq!(limits.workflow_max_wall_secs, Some(3600));
        assert_eq!(limits.workflow_max_cost_micros, Some(5_000_000));
        // A tool node (no slice) carries only the workflow envelope + its cost.
        let tool = BudgetLimits::resolve(&workflow, None);
        assert_eq!(tool.node_max_wall_secs, None);
        assert_eq!(tool.node_max_tool_calls, None);
        assert_eq!(tool.node_max_cost_micros, None);
        assert_eq!(tool.workflow_max_wall_secs, Some(3600));
        assert_eq!(tool.workflow_max_cost_micros, Some(5_000_000));
    }
}
