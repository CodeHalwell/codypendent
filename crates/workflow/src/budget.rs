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
//! The runtime does not surface provider token usage (the `ModelDriver` seam
//! leaves `prompt_tokens`/`cost_micros` at zero — see
//! [`crate::agent`] callers), so there is no honest per-run **cost-USD** or
//! **token** figure to debit. This module therefore accounts ONLY the dimensions
//! the daemon can actually measure at the node boundary:
//!
//! * **wall-time** (seconds) — measured around the node's execution; nests
//!   (a node's own slice AND the workflow envelope);
//! * **tool-calls** — counted from the node's recorded tool-call events;
//!   enforced at the node slice only (the workflow envelope has no tool-call
//!   field).
//!
//! A profile's `maximum_cost_usd` (and the workflow's) is carried through the
//! data model but deliberately **not enforced here**: charging a fabricated
//! spend would be exactly the dishonesty this phase closes. When real usage
//! plumbing lands (Phase 7), a cost dimension slots in beside these two.

use serde_json::{json, Value};

use crate::agent::AgentBudget;
use crate::model::WorkflowBudget;

/// The cost a node actually consumed, over the dimensions the daemon measures.
/// Serialized to the node record's `cost_json`, so the workflow-level
/// consumption is the sum of its nodes' `NodeCost`s (no separate ledger table).
///
/// Only measured dimensions appear — never a fabricated token or USD figure (see
/// the module docs). `Value` round-trips are lenient: a missing field reads zero,
/// so an older cost record (or one written before a dimension existed) is charged
/// as "not measured", never as a spuriously large debit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeCost {
    /// Wall-clock seconds the node ran.
    pub wall_time_secs: u64,
    /// Tool calls the node made (agent tool calls, or the tool node's own call).
    pub tool_calls: u64,
}

impl NodeCost {
    /// The zero cost — a node that has not run yet, or a pre-gate probe.
    #[must_use]
    pub fn zero() -> Self {
        Self::default()
    }

    /// This cost as the `cost_json` value the node record stores. Renders **only**
    /// the measured dimensions, so a reader never mistakes an unmeasured dimension
    /// for a real (zero) figure.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "wall_time_secs": self.wall_time_secs,
            "tool_calls": self.tool_calls,
        })
    }

    /// Read a `NodeCost` back from a stored `cost_json` value. A missing or
    /// non-numeric field reads as zero (lenient — see the type docs); a `null`
    /// or unrelated shape yields the zero cost.
    #[must_use]
    pub fn from_json(value: &Value) -> Self {
        let field = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
        Self {
            wall_time_secs: field("wall_time_secs"),
            tool_calls: field("tool_calls"),
        }
    }

    /// The sum of two costs, saturating (a budget total can never wrap to a
    /// spuriously small value that would let an exhausted run keep going).
    #[must_use]
    pub fn saturating_add(&self, other: &Self) -> Self {
        Self {
            wall_time_secs: self.wall_time_secs.saturating_add(other.wall_time_secs),
            tool_calls: self.tool_calls.saturating_add(other.tool_calls),
        }
    }
}

/// Which measured dimension a budget event is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetDimension {
    /// Wall-clock seconds.
    WallTime,
    /// Tool-call count.
    ToolCalls,
}

impl BudgetDimension {
    /// A stable lowercase name for logs, observer events, and block reasons.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetDimension::WallTime => "wall_time_secs",
            BudgetDimension::ToolCalls => "tool_calls",
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
    /// The workflow envelope's wall-time ceiling
    /// (`WorkflowBudget::maximum_duration_seconds`), charged against the sum of
    /// every node's wall-time.
    pub workflow_max_wall_secs: Option<u64>,
}

impl BudgetLimits {
    /// Build the nested limits from the workflow envelope and (for an agent node)
    /// its resolved profile slice. A tool node passes `node = None`: it has no
    /// role and so no slice, and is charged against the workflow envelope only.
    ///
    /// Cost-USD ceilings on either budget are intentionally dropped here — the
    /// runtime surfaces no measured spend, so there is nothing honest to charge
    /// them against (module docs). `maximum_agents` is a concurrency cap the
    /// scheduler enforces, not a per-node debit, so it is not a budget dimension.
    #[must_use]
    pub fn resolve(workflow: &WorkflowBudget, node: Option<&AgentBudget>) -> Self {
        Self {
            node_max_wall_secs: node.and_then(|b| b.maximum_duration_seconds),
            node_max_tool_calls: node.and_then(|b| b.maximum_tool_calls),
            workflow_max_wall_secs: workflow.maximum_duration_seconds,
        }
    }

    /// Whether any ceiling is set. When nothing is limited, the caller can skip
    /// the whole budget dance (no measurement is wasted enforcing an absent cap).
    #[must_use]
    pub fn is_unbounded(&self) -> bool {
        self.node_max_wall_secs.is_none()
            && self.node_max_tool_calls.is_none()
            && self.workflow_max_wall_secs.is_none()
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

        // Workflow envelope: every node's wall-time summed, against the envelope.
        let workflow_wall = others.saturating_add(node_cost).wall_time_secs;
        if let Some(exceeded) = check(
            BudgetScope::Workflow,
            BudgetDimension::WallTime,
            workflow_wall,
            self.workflow_max_wall_secs,
            &mut warnings,
        ) {
            return BudgetVerdict::Exceeded(exceeded);
        }

        BudgetVerdict::Within { warnings }
    }
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
            workflow_max_wall_secs: None,
        }
    }

    #[test]
    fn cost_json_round_trips_only_measured_dimensions() {
        let cost = NodeCost {
            wall_time_secs: 12,
            tool_calls: 3,
        };
        let json = cost.to_json();
        // Only the two measured dimensions are rendered — never a fabricated
        // tokens/USD field.
        assert_eq!(json.as_object().unwrap().len(), 2);
        assert_eq!(NodeCost::from_json(&json), cost);
        // A missing field reads zero, never a spurious debit.
        assert_eq!(
            NodeCost::from_json(&serde_json::json!({ "tool_calls": 5 })),
            NodeCost {
                wall_time_secs: 0,
                tool_calls: 5
            }
        );
        // An unrelated shape (e.g. an old opaque cost) is the zero cost.
        assert_eq!(
            NodeCost::from_json(&serde_json::json!({ "usd": 0.02 })),
            NodeCost::zero()
        );
    }

    #[test]
    fn within_budget_reports_no_warning_below_eighty_percent() {
        let limits = slice(Some(100), Some(10));
        let verdict = limits.charge(
            &NodeCost::zero(),
            &NodeCost {
                wall_time_secs: 50,
                tool_calls: 5,
            },
        );
        assert_eq!(verdict, BudgetVerdict::Within { warnings: vec![] });
    }

    #[test]
    fn crossing_eighty_percent_warns_without_exceeding() {
        // 4 of 5 tool calls == 80% exactly → a warning, still Within.
        let limits = slice(None, Some(5));
        let verdict = limits.charge(
            &NodeCost::zero(),
            &NodeCost {
                wall_time_secs: 0,
                tool_calls: 4,
            },
        );
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
        let verdict = limits.charge(
            &NodeCost::zero(),
            &NodeCost {
                wall_time_secs: 0,
                tool_calls: 2,
            },
        );
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
        let verdict = limits.charge(
            &NodeCost::zero(),
            &NodeCost {
                wall_time_secs: 10,
                tool_calls: 0,
            },
        );
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
    fn the_workflow_envelope_nests_over_all_nodes() {
        // No node slice; a workflow wall envelope of 100. Others already consumed
        // 90, this node 20 → 110 > 100 → the WORKFLOW scope is exceeded.
        let limits = BudgetLimits {
            node_max_wall_secs: None,
            node_max_tool_calls: None,
            workflow_max_wall_secs: Some(100),
        };
        let others = NodeCost {
            wall_time_secs: 90,
            tool_calls: 0,
        };
        let verdict = limits.charge(
            &others,
            &NodeCost {
                wall_time_secs: 20,
                tool_calls: 0,
            },
        );
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
            workflow_max_wall_secs: Some(100),
        };
        let others = NodeCost {
            wall_time_secs: 101,
            tool_calls: 0,
        };
        assert!(matches!(
            limits.charge(&others, &NodeCost::zero()),
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
            workflow_max_wall_secs: Some(5),
        };
        let verdict = limits.charge(
            &NodeCost::zero(),
            &NodeCost {
                wall_time_secs: 10,
                tool_calls: 0,
            },
        );
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
            limits.charge(
                &NodeCost {
                    wall_time_secs: 9_999,
                    tool_calls: 9_999,
                },
                &NodeCost {
                    wall_time_secs: 9_999,
                    tool_calls: 9_999,
                },
            ),
            BudgetVerdict::Within { warnings: vec![] }
        );
    }

    #[test]
    fn resolve_drops_cost_usd_and_reads_both_budgets() {
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
        assert_eq!(limits.workflow_max_wall_secs, Some(3600));
        // A tool node (no slice) carries only the workflow envelope.
        let tool = BudgetLimits::resolve(&workflow, None);
        assert_eq!(tool.node_max_wall_secs, None);
        assert_eq!(tool.node_max_tool_calls, None);
        assert_eq!(tool.workflow_max_wall_secs, Some(3600));
    }
}
