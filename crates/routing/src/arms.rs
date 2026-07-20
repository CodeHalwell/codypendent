//! Routing-evaluation arms (STEP 7.3, exit criterion 1).
//!
//! `codypendent eval route --suite core` compares five arms
//! ([Chapter 16](../../docs/docs/16-testing-strategy.md)) — static-strongest,
//! static-cheap, router, router+escalation, local-first router — over the
//! benchmark suite, reporting task success, cost, latency, escalation rate, and
//! unsafe-proposal rate. The **release gate** (exit criterion 1) asserts:
//! *router+escalation ≥ the quality threshold at cost < static-strongest*.
//!
//! This module owns the arm→selection mapping (pure) and the report + gate check
//! (pure aggregation). Actually *running* the cases through each arm is the eval
//! harness's job (it executes and scores); this crate decides and compares.

use serde::{Deserialize, Serialize};

use crate::router::{Router, RoutingDecision, RoutingError, TaskNode};

/// One arm of the routing comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RouteArm {
    /// Always the strongest eligible model.
    StaticStrongest,
    /// Always the cheapest eligible model.
    StaticCheap,
    /// The utility router (cheapest above threshold), no escalation.
    Router,
    /// The router plus cascading escalation on objective failure.
    RouterEscalation,
    /// The local-first router (prefer on-device).
    LocalFirstRouter,
}

impl RouteArm {
    /// All five arms, in report order.
    #[must_use]
    pub fn all() -> [RouteArm; 5] {
        [
            RouteArm::StaticStrongest,
            RouteArm::StaticCheap,
            RouteArm::Router,
            RouteArm::RouterEscalation,
            RouteArm::LocalFirstRouter,
        ]
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RouteArm::StaticStrongest => "static-strongest",
            RouteArm::StaticCheap => "static-cheap",
            RouteArm::Router => "router",
            RouteArm::RouterEscalation => "router-escalation",
            RouteArm::LocalFirstRouter => "local-first-router",
        }
    }

    /// Select the initial model this arm routes `node` to. (Router and
    /// RouterEscalation make the same *initial* choice; they differ only in
    /// whether the harness escalates on failure — tracked as `escalation_rate`.)
    pub fn select(
        self,
        router: &Router<'_>,
        node: &TaskNode,
    ) -> Result<RoutingDecision, RoutingError> {
        match self {
            RouteArm::StaticStrongest => router.route_static_strongest(node),
            RouteArm::StaticCheap => router.route_static_cheap(node),
            RouteArm::Router | RouteArm::RouterEscalation => router.route(node),
            RouteArm::LocalFirstRouter => router.route_local_first(node),
        }
    }

    /// Whether this arm escalates on objective failure.
    #[must_use]
    pub fn escalates(self) -> bool {
        matches!(self, RouteArm::RouterEscalation)
    }
}

/// The measured outcome of running the benchmark suite through one arm (populated
/// by the eval harness).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteArmResult {
    pub arm: RouteArm,
    /// Fraction of cases whose assertions passed `[0,1]`.
    pub task_success_rate: f64,
    /// Mean total USD cost per case.
    pub mean_cost_usd: f64,
    /// Mean end-to-end latency per case, in milliseconds.
    pub mean_latency_ms: f64,
    /// Fraction of cases that required an escalation `[0,1]`.
    pub escalation_rate: f64,
    /// Fraction of cases with a tool-call error `[0,1]`.
    pub tool_call_error_rate: f64,
    /// Fraction of cases producing an unsafe proposal `[0,1]`.
    pub unsafe_proposal_rate: f64,
}

/// A comparison report over the arms, with the release-gate check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteEvalReport {
    pub results: Vec<RouteArmResult>,
    /// The quality threshold the gate holds router+escalation to.
    pub quality_threshold: f64,
}

impl RouteEvalReport {
    #[must_use]
    pub fn new(quality_threshold: f64, results: Vec<RouteArmResult>) -> Self {
        Self {
            results,
            quality_threshold,
        }
    }

    #[must_use]
    pub fn arm(&self, arm: RouteArm) -> Option<&RouteArmResult> {
        self.results.iter().find(|r| r.arm == arm)
    }

    /// The release gate (exit criterion 1): router+escalation meets the quality
    /// threshold **and** costs less than static-strongest. Returns `false` if
    /// either arm is missing from the report.
    #[must_use]
    pub fn meets_release_gate(&self) -> bool {
        let (Some(re), Some(ss)) = (
            self.arm(RouteArm::RouterEscalation),
            self.arm(RouteArm::StaticStrongest),
        ) else {
            return false;
        };
        re.task_success_rate >= self.quality_threshold && re.mean_cost_usd < ss.mean_cost_usd
    }

    /// A human-readable one-line verdict for the release notes.
    #[must_use]
    pub fn gate_summary(&self) -> String {
        match (
            self.arm(RouteArm::RouterEscalation),
            self.arm(RouteArm::StaticStrongest),
        ) {
            (Some(re), Some(ss)) => format!(
                "router+escalation: success {:.1}% (gate {:.1}%), cost ${:.4} vs static-strongest ${:.4} → {}",
                re.task_success_rate * 100.0,
                self.quality_threshold * 100.0,
                re.mean_cost_usd,
                ss.mean_cost_usd,
                if self.meets_release_gate() { "PASS" } else { "FAIL" }
            ),
            _ => "incomplete report: missing router-escalation or static-strongest arm".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{
        ModelCapabilities, RequiredCapabilities, StructuredOutputSupport, ToolCallSupport,
    };
    use crate::classify::{classify, TaskSignals};
    use crate::policy::RoutingPolicy;
    use crate::profile::{ModelExecutionProfile, ModelLocation, ModelPerformance, ModelProfile};
    use codypendent_protocol::artifact::DataClassification;
    use codypendent_protocol::ids::ModelId;
    use std::collections::BTreeMap;

    fn model(id: &str, loc: ModelLocation, reliability: f64, cost: f64) -> ModelProfile {
        ModelProfile {
            id: ModelId(id.into()),
            location: loc,
            capabilities: ModelCapabilities {
                streaming: true,
                tools: ToolCallSupport::Parallel,
                parallel_tools: true,
                structured_output: StructuredOutputSupport::Strict,
                vision: false,
                audio_input: false,
                embeddings: false,
                prompt_caching: true,
                reasoning_controls: false,
                context_tokens: Some(200_000),
                output_tokens: Some(16_000),
            },
            performance: ModelPerformance {
                reliability,
                cost_per_1k_tokens_usd: cost,
                latency_ms_p50: 700.0,
                task_class_success: BTreeMap::new(),
                failure_patterns: vec![],
            },
            execution: ModelExecutionProfile::default(),
            bench: None,
        }
    }

    fn node() -> TaskNode {
        TaskNode {
            classification: classify(&TaskSignals::from_objective(
                "build", "agent", 4000, "fix bug",
            )),
            required: RequiredCapabilities {
                tools: true,
                ..Default::default()
            },
            data_classification: DataClassification::Internal,
            estimated_input_tokens: 8_000,
            estimated_output_tokens: 2_000,
        }
    }

    #[test]
    fn arms_select_different_models() {
        let models = vec![
            model("cheap", ModelLocation::Hosted, 0.80, 0.002),
            model("strong", ModelLocation::Hosted, 0.98, 0.030),
        ];
        let policy = RoutingPolicy::balanced();
        let router = Router::new(&models, &policy);
        let n = node();
        // Static-strongest picks the strongest; static-cheap and router pick cheap.
        assert_eq!(
            RouteArm::StaticStrongest.select(&router, &n).unwrap().model,
            ModelId("strong".into())
        );
        assert_eq!(
            RouteArm::StaticCheap.select(&router, &n).unwrap().model,
            ModelId("cheap".into())
        );
        assert_eq!(
            RouteArm::Router.select(&router, &n).unwrap().model,
            ModelId("cheap".into())
        );
    }

    #[test]
    fn local_first_prefers_a_local_model_above_threshold() {
        let models = vec![
            model("local", ModelLocation::Local, 0.80, 0.000),
            model("hosted-cheap", ModelLocation::Hosted, 0.90, 0.001),
        ];
        let policy = RoutingPolicy::balanced();
        let router = Router::new(&models, &policy);
        let d = RouteArm::LocalFirstRouter.select(&router, &node()).unwrap();
        assert_eq!(d.model, ModelId("local".into()));
    }

    fn result(arm: RouteArm, success: f64, cost: f64) -> RouteArmResult {
        RouteArmResult {
            arm,
            task_success_rate: success,
            mean_cost_usd: cost,
            mean_latency_ms: 700.0,
            escalation_rate: if arm.escalates() { 0.2 } else { 0.0 },
            tool_call_error_rate: 0.0,
            unsafe_proposal_rate: 0.0,
        }
    }

    #[test]
    fn release_gate_passes_when_router_escalation_matches_quality_at_lower_cost() {
        let report = RouteEvalReport::new(
            0.85,
            vec![
                result(RouteArm::StaticStrongest, 0.90, 0.30),
                result(RouteArm::StaticCheap, 0.60, 0.02),
                result(RouteArm::Router, 0.82, 0.05),
                result(RouteArm::RouterEscalation, 0.90, 0.12),
                result(RouteArm::LocalFirstRouter, 0.80, 0.03),
            ],
        );
        assert!(report.meets_release_gate());
        assert!(report.gate_summary().contains("PASS"));
    }

    #[test]
    fn release_gate_fails_when_router_escalation_costs_as_much_as_strongest() {
        let report = RouteEvalReport::new(
            0.85,
            vec![
                result(RouteArm::StaticStrongest, 0.90, 0.30),
                result(RouteArm::RouterEscalation, 0.90, 0.31),
            ],
        );
        assert!(!report.meets_release_gate(), "no cost win ⇒ gate fails");
    }

    #[test]
    fn release_gate_fails_when_router_escalation_misses_quality() {
        let report = RouteEvalReport::new(
            0.85,
            vec![
                result(RouteArm::StaticStrongest, 0.90, 0.30),
                result(RouteArm::RouterEscalation, 0.80, 0.10),
            ],
        );
        assert!(
            !report.meets_release_gate(),
            "below quality bar ⇒ gate fails"
        );
    }

    #[test]
    fn release_gate_fails_on_an_incomplete_report() {
        let report = RouteEvalReport::new(0.85, vec![result(RouteArm::Router, 0.90, 0.05)]);
        assert!(!report.meets_release_gate());
        assert!(report.gate_summary().contains("incomplete"));
    }
}
