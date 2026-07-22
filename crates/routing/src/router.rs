//! The per-task-node router (STEP 7.3): the Chapter 09 pipeline, exactly.
//!
//! ```text
//! task node → data classification & policy (hard filter: eligible providers)
//!           → required capabilities (hard filter)
//!           → context/output size estimate (hard filter: fits?)
//!           → historical task-class performance
//!           → cost & latency estimate
//!           → utility = predicted_success − λc·cost − λl·latency − λp·privacy − λf·failure
//!           → select cheapest model above the quality threshold
//!           → (caller validates output)
//!           → escalate on objective failure (preserving artifacts and task state)
//! ```
//!
//! Two properties are load-bearing: **security constraints are evaluated before
//! utility** — classified data can never route to an ineligible (hosted) provider,
//! because such models are removed in the hard-filter pass before any score is
//! computed; and **escalation preserves artifacts** — it re-executes the failed
//! node on the next model in the policy's chain, it does not restart the workflow.

use codypendent_protocol::artifact::DataClassification;
use codypendent_protocol::ids::ModelId;
use serde::{Deserialize, Serialize};

use crate::capability::RequiredCapabilities;
use crate::classify::{Classification, TaskClass};
use crate::policy::RoutingPolicy;
use crate::profile::ModelProfile;

/// A task node presented to the router: its class, what it requires, how
/// sensitive its data is, and its size estimate.
#[derive(Debug, Clone)]
pub struct TaskNode {
    pub classification: Classification,
    pub required: RequiredCapabilities,
    /// The most sensitive data this node handles (gates hosted providers).
    pub data_classification: DataClassification,
    pub estimated_input_tokens: u64,
    pub estimated_output_tokens: u64,
}

impl TaskNode {
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.estimated_input_tokens + self.estimated_output_tokens
    }

    #[must_use]
    pub fn class(&self) -> TaskClass {
        self.classification.class
    }
}

/// Why the router selected a particular model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SelectionReason {
    /// The cheapest model whose predicted success clears the quality threshold.
    CheapestAboveThreshold,
    /// No eligible model cleared the threshold; the best-predicted eligible model
    /// was chosen as a best effort (the caller may escalate).
    BestEffortBelowThreshold,
    /// Selected by advancing the escalation chain after an objective failure.
    Escalated,
    /// The strongest eligible model, ignoring cost (the static-strongest arm).
    StaticStrongest,
    /// The cheapest eligible model, ignoring quality (the static-cheap arm).
    StaticCheapest,
    /// The cheapest local model above threshold, preferring on-device (local-first arm).
    LocalFirst,
}

/// A routing decision, with the numbers and provenance a trace records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub model: ModelId,
    pub task_class: TaskClass,
    /// The classifier version that produced the task class (attributable).
    pub classifier_version: String,
    /// The routing-policy revision key (`router/<name>/<version>`, attributable).
    pub policy_key: String,
    pub predicted_success: f64,
    pub expected_cost_usd: f64,
    pub expected_latency_ms: f64,
    pub utility: f64,
    pub reason: SelectionReason,
}

/// A record of an escalation transition (Chapter 09 mid-session switching): the
/// old/new model, the objective reason, how context carried over, and the cost
/// impact — and whether artifacts were preserved (not restarted).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingTransition {
    pub from: ModelId,
    pub to: ModelId,
    /// The objective failure that triggered the escalation.
    pub reason: String,
    /// How the context was transformed for the new model (e.g. re-laid-out).
    pub context_transformation: String,
    /// The change in expected cost from old to new model (USD; may be negative).
    pub cost_impact_usd: f64,
    /// Whether the task's artifacts and state were preserved across the
    /// escalation (not restarted). The router's *decision* to escalate always
    /// intends this — [`Router::escalate`] re-executes the same [`TaskNode`],
    /// never a fresh workflow — but whether that intent held during a real run
    /// is an execution-time fact the router cannot observe: it never executes
    /// anything (STEP 7.3's executor seam is unbuilt, P7-4). `None` means "not
    /// yet reported"; only the executor that actually ran the escalation can
    /// honestly report `Some(_)`. This field must never be fabricated as
    /// `Some(true)` by the router itself.
    pub artifacts_preserved: Option<bool>,
}

/// A routing failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoutingError {
    /// No model passed the hard filters. Failing here is the *correct* outcome
    /// when e.g. classified data has no eligible (local) provider — the router
    /// never relaxes a security constraint to find a candidate.
    #[error("no eligible model for the task node ({reason})")]
    NoEligibleModel { reason: String },
    /// The escalation chain is exhausted — every tier past the current model is
    /// either absent from the registry or fails the hard filters.
    #[error("escalation chain exhausted after {from}")]
    EscalationExhausted { from: ModelId },
    /// The model to escalate from is not in the policy's escalation chain.
    #[error("model {from} is not in the escalation chain")]
    NotInChain { from: ModelId },
}

/// The router over a set of model profiles and a policy.
pub struct Router<'a> {
    pub models: &'a [ModelProfile],
    pub policy: &'a RoutingPolicy,
}

impl<'a> Router<'a> {
    #[must_use]
    pub fn new(models: &'a [ModelProfile], policy: &'a RoutingPolicy) -> Self {
        Self { models, policy }
    }

    /// Route a task node to a model. Applies the hard filters first, then selects
    /// the cheapest model above the quality threshold (falling back to the best
    /// eligible model if none clears it).
    pub fn route(&self, node: &TaskNode) -> Result<RoutingDecision, RoutingError> {
        let eligible = self.eligible(node);
        if eligible.is_empty() {
            return Err(RoutingError::NoEligibleModel {
                reason: self.ineligibility_reason(node),
            });
        }

        // Split eligible models by the quality threshold.
        let above: Vec<&ModelProfile> = eligible
            .iter()
            .copied()
            .filter(|m| {
                m.performance.predicted_success(node.class()) >= self.policy.quality_threshold
            })
            .collect();

        if let Some(chosen) = self.cheapest(&above, node) {
            Ok(self.decide(chosen, node, SelectionReason::CheapestAboveThreshold))
        } else {
            // None cleared the bar — best effort: the highest predicted success.
            let chosen = self
                .best_predicted(&eligible, node)
                .expect("eligible is non-empty");
            Ok(self.decide(chosen, node, SelectionReason::BestEffortBelowThreshold))
        }
    }

    /// **Static-strongest** arm: the eligible model with the highest predicted
    /// success, ignoring cost and the threshold. The quality ceiling the router is
    /// measured against (it should match this quality at lower cost).
    pub fn route_static_strongest(&self, node: &TaskNode) -> Result<RoutingDecision, RoutingError> {
        let eligible = self.eligible(node);
        let chosen =
            self.best_predicted(&eligible, node)
                .ok_or_else(|| RoutingError::NoEligibleModel {
                    reason: self.ineligibility_reason(node),
                })?;
        Ok(self.decide(chosen, node, SelectionReason::StaticStrongest))
    }

    /// **Static-cheap** arm: the cheapest eligible model, ignoring quality. The
    /// cost floor the router is measured against (it should beat this quality at
    /// comparable cost).
    pub fn route_static_cheap(&self, node: &TaskNode) -> Result<RoutingDecision, RoutingError> {
        let eligible = self.eligible(node);
        let chosen =
            self.cheapest(&eligible, node)
                .ok_or_else(|| RoutingError::NoEligibleModel {
                    reason: self.ineligibility_reason(node),
                })?;
        Ok(self.decide(chosen, node, SelectionReason::StaticCheapest))
    }

    /// **Local-first** arm: the cheapest local model above threshold if one
    /// exists, otherwise the normal route. Keeps work on-device when a local model
    /// is good enough, escalating to hosted only when it is not.
    pub fn route_local_first(&self, node: &TaskNode) -> Result<RoutingDecision, RoutingError> {
        let local_above: Vec<&ModelProfile> = self
            .eligible(node)
            .into_iter()
            .filter(|m| {
                m.is_local()
                    && m.performance.predicted_success(node.class())
                        >= self.policy.quality_threshold
            })
            .collect();
        if let Some(chosen) = self.cheapest(&local_above, node) {
            return Ok(self.decide(chosen, node, SelectionReason::LocalFirst));
        }
        self.route(node)
    }

    /// Escalate after an objective validation failure: advance the policy's
    /// escalation chain to the next eligible tier past `from`, re-routing the same
    /// node (never restarting the workflow).
    ///
    /// **Cycle-proof (P7-1):** the chain is walked anchored at the *last*
    /// occurrence of `from` (`rposition`, not `position`), not the first. A
    /// well-formed chain has unique ids (rejected otherwise at
    /// [`crate::policy::RoutingPolicy::validate`]), so this makes no difference
    /// for one — but if a duplicate id ever reaches the router regardless (a
    /// policy constructed in-process without going through validation), this
    /// guarantees each successive call anchors at a strictly greater chain index
    /// than the last, so repeated escalation can never revisit an earlier index
    /// and cycle; it only ever exhausts.
    pub fn escalate(
        &self,
        from: &ModelId,
        reason: impl Into<String>,
        node: &TaskNode,
    ) -> Result<(RoutingDecision, RoutingTransition), RoutingError> {
        let chain = &self.policy.escalation_chain;
        let pos = chain
            .iter()
            .rposition(|m| m == from)
            .ok_or_else(|| RoutingError::NotInChain { from: from.clone() })?;

        // The first tier after `from` that resolves to an eligible profile.
        let next = chain[pos + 1..]
            .iter()
            .filter_map(|id| self.find(id))
            .find(|m| self.is_eligible(m, node))
            .ok_or_else(|| RoutingError::EscalationExhausted { from: from.clone() })?;

        let from_cost = self
            .find(from)
            .map(|m| m.expected_cost_usd(node.total_tokens()))
            .unwrap_or(0.0);
        let mut decision = self.decide(next, node, SelectionReason::Escalated);
        // The decision's cost is the new model's; the transition records the delta.
        let transition = RoutingTransition {
            from: from.clone(),
            to: next.id.clone(),
            reason: reason.into(),
            context_transformation: format!(
                "re-laid-out for {} ({:?})",
                next.id, next.execution.edit_protocol
            ),
            cost_impact_usd: decision.expected_cost_usd - from_cost,
            // P7-4: the router never executes anything, so it cannot observe
            // whether artifacts were actually preserved — only the (unbuilt)
            // executor can report that. `None` here, never a fabricated `true`.
            artifacts_preserved: None,
        };
        decision.reason = SelectionReason::Escalated;
        Ok((decision, transition))
    }

    /// The models that pass every hard filter for this node.
    fn eligible(&self, node: &TaskNode) -> Vec<&'a ModelProfile> {
        self.models
            .iter()
            .filter(|m| self.is_eligible(m, node))
            .collect()
    }

    /// The hard filters, in Chapter 09 order — **security/privacy first**, then
    /// capabilities, then size (capabilities cover size via the context/output
    /// minimums).
    fn is_eligible(&self, model: &ModelProfile, node: &TaskNode) -> bool {
        // 1. Data classification & policy: a hosted model may not process data
        //    above the policy's off-device ceiling. This runs before utility, so a
        //    classified node can never be scored against — let alone routed to — an
        //    ineligible provider.
        if !model.is_local() && !self.policy.hosted_allows(node.data_classification) {
            return false;
        }
        // 2 + 3. Required capabilities, with the node's size *estimates* folded into
        //    the fit check, even when the caller left the explicit `min_*`
        //    requirements at their defaults. The context window must hold the whole
        //    task — input *and* generated output both live in it — so the context
        //    minimum uses `total_tokens()`, not just the input; the output minimum
        //    uses the output estimate. Otherwise a large task could route to a model
        //    whose window cannot hold it.
        let mut required = node.required;
        required.min_context_tokens = required.min_context_tokens.max(node.total_tokens());
        required.min_output_tokens = required.min_output_tokens.max(node.estimated_output_tokens);
        model.capabilities.satisfies(&required)
    }

    /// Cheapest eligible model in `candidates` (min expected cost; ties broken by
    /// higher predicted success, then lexicographic id for determinism).
    fn cheapest<'m>(
        &self,
        candidates: &[&'m ModelProfile],
        node: &TaskNode,
    ) -> Option<&'m ModelProfile> {
        candidates.iter().copied().min_by(|a, b| {
            let ca = a.expected_cost_usd(node.total_tokens());
            let cb = b.expected_cost_usd(node.total_tokens());
            ca.partial_cmp(&cb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let pa = a.performance.predicted_success(node.class());
                    let pb = b.performance.predicted_success(node.class());
                    pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.id.0.cmp(&b.id.0))
        })
    }

    /// The eligible model with the highest predicted success (best-effort fallback).
    fn best_predicted<'m>(
        &self,
        candidates: &[&'m ModelProfile],
        node: &TaskNode,
    ) -> Option<&'m ModelProfile> {
        candidates.iter().copied().max_by(|a, b| {
            let pa = a.performance.predicted_success(node.class());
            let pb = b.performance.predicted_success(node.class());
            pa.partial_cmp(&pb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.id.0.cmp(&a.id.0))
        })
    }

    /// Build the decision record for a chosen model, computing its utility.
    fn decide(
        &self,
        model: &ModelProfile,
        node: &TaskNode,
        reason: SelectionReason,
    ) -> RoutingDecision {
        let predicted = model.performance.predicted_success(node.class());
        let cost = model.expected_cost_usd(node.total_tokens());
        let latency = model.performance.latency_ms_p50;
        let utility = self.utility(model, node);
        RoutingDecision {
            model: model.id.clone(),
            task_class: node.class(),
            classifier_version: node.classification.classifier_version.clone(),
            policy_key: self.policy.registry_key(),
            predicted_success: predicted,
            expected_cost_usd: cost,
            expected_latency_ms: latency,
            utility,
            reason,
        }
    }

    /// The Chapter 09 utility: predicted success minus weighted penalties.
    fn utility(&self, model: &ModelProfile, node: &TaskNode) -> f64 {
        let l = &self.policy.lambdas;
        let predicted = model.performance.predicted_success(node.class());
        let cost = model.expected_cost_usd(node.total_tokens());
        let latency_s = model.performance.latency_ms_p50 / 1000.0;
        let privacy = self.privacy_risk(model, node);
        let failure = model.performance.failure_probability(node.class());
        predicted
            - l.cost * cost
            - l.latency * latency_s
            - l.privacy * privacy
            - l.failure * failure
    }

    /// Privacy risk of running `node`'s data on `model`: zero for a local model;
    /// for a hosted model, proportional to how sensitive the data is (a hosted
    /// model handling `Public` data has no privacy cost; `Secret` the most).
    fn privacy_risk(&self, model: &ModelProfile, node: &TaskNode) -> f64 {
        if model.is_local() {
            0.0
        } else {
            f64::from(node.data_classification.rank())
                / f64::from(DataClassification::Unknown.rank())
        }
    }

    fn find(&self, id: &ModelId) -> Option<&'a ModelProfile> {
        self.models.iter().find(|m| &m.id == id)
    }

    /// A human explanation for why nothing was eligible (for the error).
    fn ineligibility_reason(&self, node: &TaskNode) -> String {
        if self.models.is_empty() {
            return "no models are configured".to_string();
        }
        let any_local = self.models.iter().any(ModelProfile::is_local);
        if !self.policy.hosted_allows(node.data_classification) && !any_local {
            return format!(
                "data classified {:?} may not leave the device and no local model is available",
                node.data_classification
            );
        }
        "no model satisfies the required capabilities or size".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{ModelCapabilities, StructuredOutputSupport, ToolCallSupport};
    use crate::classify::classify;
    use crate::classify::TaskSignals;
    use crate::policy::RoutingPolicy;
    use crate::profile::{
        LocalBench, ModelExecutionProfile, ModelLocation, ModelPerformance, ModelProfile,
    };
    use std::collections::BTreeMap;

    fn caps(context: u64) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tools: ToolCallSupport::Parallel,
            parallel_tools: true,
            structured_output: StructuredOutputSupport::Strict,
            vision: false,
            audio_input: false,
            embeddings: false,
            prompt_caching: true,
            reasoning_controls: false,
            context_tokens: Some(context),
            output_tokens: Some(16_000),
        }
    }

    fn model(
        id: &str,
        location: ModelLocation,
        reliability: f64,
        cost_per_1k: f64,
        latency_ms: f64,
        context: u64,
    ) -> ModelProfile {
        ModelProfile {
            id: ModelId(id.into()),
            location,
            capabilities: caps(context),
            performance: ModelPerformance {
                reliability,
                cost_per_1k_tokens_usd: cost_per_1k,
                latency_ms_p50: latency_ms,
                task_class_success: BTreeMap::new(),
                failure_patterns: vec![],
            },
            execution: ModelExecutionProfile::default(),
            bench: if location.is_local() {
                Some(LocalBench {
                    tokens_per_second: 40.0,
                    time_to_first_token_ms: 200.0,
                    warmup_ms: 500.0,
                    memory_mb: 8000,
                    context_limit: context,
                    structured_output_reliability: 0.8,
                    tool_call_accuracy: 0.75,
                    coding_eval_score: 0.6,
                })
            } else {
                None
            },
        }
    }

    fn node(data: DataClassification) -> TaskNode {
        TaskNode {
            classification: classify(&TaskSignals::from_objective(
                "build",
                "agent",
                4_000,
                "fix the bug",
            )),
            required: RequiredCapabilities {
                tools: true,
                structured_output: true,
                min_context_tokens: 20_000,
                min_output_tokens: 2_000,
                ..Default::default()
            },
            data_classification: data,
            estimated_input_tokens: 8_000,
            estimated_output_tokens: 2_000,
        }
    }

    #[test]
    fn selects_cheapest_model_above_threshold() {
        // Three hosted models all clear the 0.7 bar; the cheapest wins even though
        // a pricier one is marginally more reliable.
        let models = vec![
            model("cheap", ModelLocation::Hosted, 0.80, 0.002, 900.0, 200_000),
            model("mid", ModelLocation::Hosted, 0.88, 0.010, 700.0, 200_000),
            model("strong", ModelLocation::Hosted, 0.95, 0.030, 600.0, 200_000),
        ];
        let policy = RoutingPolicy::balanced();
        let router = Router::new(&models, &policy);
        let d = router.route(&node(DataClassification::Internal)).unwrap();
        assert_eq!(d.model, ModelId("cheap".into()));
        assert_eq!(d.reason, SelectionReason::CheapestAboveThreshold);
        assert_eq!(d.policy_key, "router/balanced/1");
    }

    #[test]
    fn a_model_below_threshold_is_skipped_for_a_pricier_one_above() {
        // The cheapest model is below the 0.7 bar; the router skips it.
        let models = vec![
            model(
                "flaky-cheap",
                ModelLocation::Hosted,
                0.50,
                0.001,
                900.0,
                200_000,
            ),
            model("solid", ModelLocation::Hosted, 0.85, 0.010, 700.0, 200_000),
        ];
        let policy = RoutingPolicy::balanced();
        let router = Router::new(&models, &policy);
        let d = router.route(&node(DataClassification::Internal)).unwrap();
        assert_eq!(d.model, ModelId("solid".into()));
    }

    #[test]
    fn classified_data_never_routes_to_a_hosted_provider() {
        // Secret data with a policy that only allows Internal off-device: every
        // hosted model is filtered out BEFORE scoring. Only the local model is
        // eligible — the security invariant.
        let models = vec![
            model(
                "hosted-strong",
                ModelLocation::Hosted,
                0.99,
                0.001,
                500.0,
                200_000,
            ),
            model("local", ModelLocation::Local, 0.75, 0.000, 1500.0, 128_000),
        ];
        let mut policy = RoutingPolicy::balanced();
        policy.max_off_device = DataClassification::Internal;
        let router = Router::new(&models, &policy);
        let d = router.route(&node(DataClassification::Secret)).unwrap();
        assert_eq!(
            d.model,
            ModelId("local".into()),
            "sensitive data stays local"
        );
    }

    #[test]
    fn classified_data_with_no_local_model_fails_rather_than_leaks() {
        // No local model + secret data + restrictive policy ⇒ the router refuses
        // to route rather than sending sensitive data off-device.
        let models = vec![model(
            "hosted",
            ModelLocation::Hosted,
            0.99,
            0.001,
            500.0,
            200_000,
        )];
        let mut policy = RoutingPolicy::balanced();
        policy.max_off_device = DataClassification::Internal;
        let router = Router::new(&models, &policy);
        let err = router.route(&node(DataClassification::Secret)).unwrap_err();
        assert!(matches!(err, RoutingError::NoEligibleModel { .. }));
    }

    #[test]
    fn a_capability_requirement_filters_the_pool() {
        // A model without tool support is ineligible for a tool-requiring node.
        let mut no_tools = model(
            "no-tools",
            ModelLocation::Hosted,
            0.99,
            0.001,
            500.0,
            200_000,
        );
        no_tools.capabilities.tools = ToolCallSupport::None;
        let ok = model("ok", ModelLocation::Hosted, 0.80, 0.010, 700.0, 200_000);
        let models = vec![no_tools, ok];
        let policy = RoutingPolicy::balanced();
        let router = Router::new(&models, &policy);
        let d = router.route(&node(DataClassification::Internal)).unwrap();
        assert_eq!(d.model, ModelId("ok".into()));
    }

    #[test]
    fn a_too_small_context_window_filters_the_model() {
        // A 10k-context model cannot serve a node whose required context is 20k.
        let small = model(
            "small-ctx",
            ModelLocation::Hosted,
            0.99,
            0.001,
            500.0,
            10_000,
        );
        let big = model(
            "big-ctx",
            ModelLocation::Hosted,
            0.80,
            0.010,
            700.0,
            200_000,
        );
        let models = vec![small, big];
        let policy = RoutingPolicy::balanced();
        let router = Router::new(&models, &policy);
        let d = router.route(&node(DataClassification::Internal)).unwrap();
        assert_eq!(d.model, ModelId("big-ctx".into()));
    }

    #[test]
    fn a_large_size_estimate_filters_a_small_model_even_with_default_requirements() {
        // The node leaves `required` at its defaults (min_context/min_output = 0)
        // but *estimates* a 120k-token input. A 32k-context model must be filtered
        // by folding the estimate into the fit check — otherwise a task too big for
        // the window would route to it.
        let small = model(
            "small-ctx",
            ModelLocation::Hosted,
            0.99,
            0.001,
            500.0,
            32_000,
        );
        let big = model(
            "big-ctx",
            ModelLocation::Hosted,
            0.80,
            0.010,
            700.0,
            200_000,
        );
        let models = vec![small, big];
        let policy = RoutingPolicy::balanced();
        let router = Router::new(&models, &policy);
        let task = TaskNode {
            classification: classify(&TaskSignals::from_objective(
                "build", "agent", 120_000, "big",
            )),
            required: RequiredCapabilities::default(), // no explicit minimums
            data_classification: DataClassification::Internal,
            estimated_input_tokens: 120_000,
            estimated_output_tokens: 2_000,
        };
        let d = router.route(&task).unwrap();
        assert_eq!(
            d.model,
            ModelId("big-ctx".into()),
            "the 32k model can't hold a 120k task"
        );
    }

    #[test]
    fn below_threshold_falls_back_to_best_predicted() {
        // Every model is below the 0.9 bar; the router returns the best available,
        // flagged best-effort so the caller knows to consider escalation.
        let models = vec![
            model("a", ModelLocation::Hosted, 0.60, 0.001, 900.0, 200_000),
            model("b", ModelLocation::Hosted, 0.80, 0.010, 700.0, 200_000),
        ];
        let mut policy = RoutingPolicy::balanced();
        policy.quality_threshold = 0.9;
        let router = Router::new(&models, &policy);
        let d = router.route(&node(DataClassification::Internal)).unwrap();
        assert_eq!(d.model, ModelId("b".into()));
        assert_eq!(d.reason, SelectionReason::BestEffortBelowThreshold);
    }

    #[test]
    fn escalation_advances_the_chain_and_records_a_complete_transition() {
        let models = vec![
            model(
                "local-default",
                ModelLocation::Local,
                0.75,
                0.000,
                1500.0,
                128_000,
            ),
            model(
                "hosted-default",
                ModelLocation::Hosted,
                0.85,
                0.010,
                700.0,
                200_000,
            ),
            model(
                "hosted-strong",
                ModelLocation::Hosted,
                0.96,
                0.030,
                600.0,
                200_000,
            ),
        ];
        let mut policy = RoutingPolicy::balanced();
        policy.escalation_chain = vec![
            ModelId("local-default".into()),
            ModelId("hosted-default".into()),
            ModelId("hosted-strong".into()),
        ];
        let router = Router::new(&models, &policy);
        let n = node(DataClassification::Internal);
        let (decision, transition) = router
            .escalate(&ModelId("local-default".into()), "tests still failing", &n)
            .unwrap();
        assert_eq!(decision.model, ModelId("hosted-default".into()));
        assert_eq!(decision.reason, SelectionReason::Escalated);
        // The transition is complete: from/to, reason, context transformation,
        // cost impact, and artifacts preserved.
        assert_eq!(transition.from, ModelId("local-default".into()));
        assert_eq!(transition.to, ModelId("hosted-default".into()));
        assert_eq!(transition.reason, "tests still failing");
        // P7-4: the router cannot observe whether artifacts were actually
        // preserved during a real run — it never executes anything — so it must
        // not fabricate a claim. `None` ("not yet reported"), never `Some(true)`.
        assert_eq!(
            transition.artifacts_preserved, None,
            "the router must not fabricate an artifacts-preserved claim; only the executor can report it"
        );
        assert!(!transition.context_transformation.is_empty());
        // local ($0) → hosted ($0.01/1k * 10k = $0.10): a positive cost impact.
        assert!((transition.cost_impact_usd - 0.10).abs() < 1e-9);
    }

    #[test]
    fn escalation_skips_ineligible_tiers() {
        // A restrictive policy makes the middle (hosted) tier ineligible for
        // Secret data; escalation must skip it — but here the strong tier is also
        // hosted, so with Secret data the chain is exhausted.
        let models = vec![
            model(
                "local-default",
                ModelLocation::Local,
                0.70,
                0.000,
                1500.0,
                128_000,
            ),
            model(
                "hosted-default",
                ModelLocation::Hosted,
                0.85,
                0.010,
                700.0,
                200_000,
            ),
            model(
                "hosted-strong",
                ModelLocation::Hosted,
                0.96,
                0.030,
                600.0,
                200_000,
            ),
        ];
        let mut policy = RoutingPolicy::balanced();
        policy.max_off_device = DataClassification::Internal;
        policy.escalation_chain = vec![
            ModelId("local-default".into()),
            ModelId("hosted-default".into()),
            ModelId("hosted-strong".into()),
        ];
        let router = Router::new(&models, &policy);
        let n = node(DataClassification::Secret);
        let err = router
            .escalate(&ModelId("local-default".into()), "still failing", &n)
            .unwrap_err();
        assert!(matches!(err, RoutingError::EscalationExhausted { .. }));
    }

    // --- P7-1: escalation is cycle-proof and terminates ---

    #[test]
    fn escalation_on_a_legal_unique_id_chain_terminates_at_the_end() {
        let models = vec![
            model("t1", ModelLocation::Hosted, 0.85, 0.010, 700.0, 200_000),
            model("t2", ModelLocation::Hosted, 0.85, 0.010, 700.0, 200_000),
            model("t3", ModelLocation::Hosted, 0.85, 0.010, 700.0, 200_000),
        ];
        let mut policy = RoutingPolicy::balanced();
        policy.escalation_chain = vec![
            ModelId("t1".into()),
            ModelId("t2".into()),
            ModelId("t3".into()),
        ];
        let router = Router::new(&models, &policy);
        let n = node(DataClassification::Internal);
        let (d1, _) = router
            .escalate(&ModelId("t1".into()), "x", &n)
            .expect("t1 -> t2");
        assert_eq!(d1.model, ModelId("t2".into()));
        let (d2, _) = router.escalate(&d1.model, "x", &n).expect("t2 -> t3");
        assert_eq!(d2.model, ModelId("t3".into()));
        let err = router.escalate(&d2.model, "x", &n).unwrap_err();
        assert!(
            matches!(err, RoutingError::EscalationExhausted { .. }),
            "escalation off the end of the chain terminates rather than looping"
        );
    }

    #[test]
    fn escalation_is_cycle_proof_even_with_a_duplicate_id_in_the_chain() {
        // A chain with a duplicate id should never reach a real Router (it's
        // rejected by `RoutingPolicy::validate`) — but this pins the router's
        // OWN defense-in-depth: even if one does (e.g. a policy built in-process
        // without going through validation, as this test deliberately does),
        // repeated escalation must make monotonic progress and terminate, never
        // cycle. Chain: [a, b, a], both eligible. Under the old `position()` (=
        // first-match) anchor, escalating from "a" always resumes searching
        // right after the FIRST "a" (index 0) no matter how far the chain has
        // actually advanced, so repeated escalation oscillates a -> b -> a -> b
        // forever. Anchoring at the LAST match instead guarantees each call's
        // anchor index is strictly greater than the last.
        let models = vec![
            model("a", ModelLocation::Hosted, 0.85, 0.010, 700.0, 200_000),
            model("b", ModelLocation::Hosted, 0.85, 0.010, 700.0, 200_000),
        ];
        let mut policy = RoutingPolicy::balanced();
        policy.escalation_chain = vec![
            ModelId("a".into()),
            ModelId("b".into()),
            ModelId("a".into()),
        ];
        let router = Router::new(&models, &policy);
        let n = node(DataClassification::Internal);

        let mut current = ModelId("a".into());
        let mut hops = 0usize;
        loop {
            match router.escalate(&current, "still failing", &n) {
                Ok((decision, _transition)) => {
                    current = decision.model;
                    hops += 1;
                    // The chain has 3 entries; escalation can never legitimately
                    // take more hops than that without either exhausting or
                    // erroring. If this bound is exceeded, the loop is cycling.
                    assert!(
                        hops <= policy.escalation_chain.len(),
                        "escalation cycled instead of terminating"
                    );
                }
                Err(RoutingError::EscalationExhausted { .. }) => break,
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
    }

    #[test]
    fn escalating_from_a_model_not_in_chain_errors() {
        let models = vec![model(
            "a",
            ModelLocation::Hosted,
            0.85,
            0.010,
            700.0,
            200_000,
        )];
        let policy = RoutingPolicy::balanced(); // empty chain
        let router = Router::new(&models, &policy);
        let err = router
            .escalate(&ModelId("a".into()), "x", &node(DataClassification::Public))
            .unwrap_err();
        assert!(matches!(err, RoutingError::NotInChain { .. }));
    }

    #[test]
    fn no_models_configured_is_an_error() {
        let models: Vec<ModelProfile> = vec![];
        let policy = RoutingPolicy::balanced();
        let router = Router::new(&models, &policy);
        assert!(matches!(
            router.route(&node(DataClassification::Public)),
            Err(RoutingError::NoEligibleModel { .. })
        ));
    }
}
