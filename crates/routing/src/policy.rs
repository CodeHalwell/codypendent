//! The routing policy (STEP 7.3): the versioned weights, threshold, escalation
//! chain, and privacy ceiling the router optimizes under.
//!
//! Per Chapter 09, the λ weights and quality threshold live in a **versioned**
//! [`RoutingPolicy`] (a registry item, `router/<name>/<version>`), selectable per
//! scope — so a routing change is a candidate that goes through the Phase 7
//! promotion pipeline, not an edit. Budgets from Phase 5 stay authoritative; the
//! router optimizes *inside* them.

use std::collections::HashSet;

use codypendent_protocol::artifact::DataClassification;
use codypendent_protocol::ids::ModelId;
use serde::{Deserialize, Serialize};

/// The λ weights on the utility function's penalty terms (Chapter 09):
///
/// ```text
/// utility = predicted_success − λc·cost − λl·latency − λp·privacy − λf·failure
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Lambdas {
    /// Weight on expected cost (per USD).
    pub cost: f64,
    /// Weight on expected latency (per second).
    pub latency: f64,
    /// Weight on privacy risk `[0,1]`.
    pub privacy: f64,
    /// Weight on failure probability `[0,1]`.
    pub failure: f64,
}

impl Default for Lambdas {
    fn default() -> Self {
        // A balanced default: cost and failure matter most, latency and privacy
        // are secondary tie-breakers. Tuned so a small quality gain justifies a
        // small cost increase but not a large one.
        Self {
            cost: 1.0,
            latency: 0.05,
            privacy: 0.5,
            failure: 0.5,
        }
    }
}

/// A routing-policy validation failure (P7-6 / P7-1a): malformed weights,
/// thresholds, or an ambiguous escalation chain must be rejected before a
/// policy is used, not silently accepted and tie-broken (a `NaN` weight makes
/// every `partial_cmp`-based comparison in [`crate::router::Router`] degrade to
/// an arbitrary, unstable pick) or walked into a potential cycle (a duplicate
/// model id makes [`crate::router::Router::escalate`]'s position-in-chain
/// lookup ambiguous).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PolicyError {
    /// A λ weight is not finite (`NaN` or ±∞).
    #[error("lambda weight `{field}` must be finite, got {value}")]
    NonFiniteWeight { field: &'static str, value: f64 },
    /// A λ weight is negative — it would *reward*, not penalize, its quantity
    /// (e.g. a negative cost weight pays the router to prefer expensive models).
    #[error("lambda weight `{field}` must be >= 0, got {value}")]
    NegativeWeight { field: &'static str, value: f64 },
    /// The quality threshold is not finite.
    #[error("quality_threshold must be finite, got {value}")]
    NonFiniteThreshold { value: f64 },
    /// The quality threshold is outside `[0, 1]` — it gates a predicted-success
    /// probability, which never exceeds 1 or drops below 0.
    #[error("quality_threshold must be within [0, 1], got {value}")]
    ThresholdOutOfRange { value: f64 },
    /// The escalation chain lists the same model id more than once. Walking the
    /// chain by id (`position()`/`rposition()`) is inherently ambiguous when an
    /// id repeats, so this is rejected outright rather than tolerated.
    #[error("escalation chain contains duplicate model id: {model}")]
    DuplicateModelInChain { model: ModelId },
}

/// A named, versioned routing policy.
///
/// **Validated at deserialization.** `RoutingPolicy` cannot be deserialized
/// (`serde_json::from_str` or similar) without passing [`RoutingPolicy::validate`]
/// — malformed data (a `NaN`/negative λ weight, an out-of-range threshold, a
/// chain with a duplicate model id) is rejected with a [`PolicyError`] rather
/// than silently accepted. This gates the untrusted-input boundary (loading a
/// policy from a registry/config file); ordinary in-crate Rust construction
/// (e.g. [`RoutingPolicy::balanced`] plus direct field assignment, as this
/// module's own tests do to exercise specific shapes) is unaffected — the same
/// trust-boundary split `codypendent-eval`'s promotion pipeline documents:
/// mechanism here, and only deserialized/external data is gated by it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RoutingPolicyWire")]
pub struct RoutingPolicy {
    /// The policy name (the registry key stem, `router/<name>`).
    pub name: String,
    /// The policy version (`router/<name>/<version>`) — recorded in traces so a
    /// routing decision is attributable to an exact policy revision.
    pub version: u32,
    /// The utility λ weights.
    pub lambdas: Lambdas,
    /// The minimum predicted success a model must clear to be *eligible*
    /// (cheapest-above-threshold selects among models over this bar).
    pub quality_threshold: f64,
    /// The escalation chain, cheapest → strongest, as model ids. On an objective
    /// validation failure the router advances along this chain.
    #[serde(default)]
    pub escalation_chain: Vec<ModelId>,
    /// The most sensitive data classification permitted to leave the device.
    /// Hosted models are ineligible for data above this ceiling (the security
    /// hard filter). Local models are always eligible.
    pub max_off_device: DataClassification,
}

/// The wire shape of a [`RoutingPolicy`]: identical fields, but deserializing
/// it directly is not `pub` — every deserialized `RoutingPolicy` is built via
/// `TryFrom<RoutingPolicyWire>`, which calls [`RoutingPolicy::validate`].
#[derive(Debug, Clone, Deserialize)]
struct RoutingPolicyWire {
    name: String,
    version: u32,
    lambdas: Lambdas,
    quality_threshold: f64,
    #[serde(default)]
    escalation_chain: Vec<ModelId>,
    max_off_device: DataClassification,
}

impl TryFrom<RoutingPolicyWire> for RoutingPolicy {
    type Error = PolicyError;

    fn try_from(wire: RoutingPolicyWire) -> Result<Self, Self::Error> {
        let policy = RoutingPolicy {
            name: wire.name,
            version: wire.version,
            lambdas: wire.lambdas,
            quality_threshold: wire.quality_threshold,
            escalation_chain: wire.escalation_chain,
            max_off_device: wire.max_off_device,
        };
        policy.validate()?;
        Ok(policy)
    }
}

impl RoutingPolicy {
    /// The registry key for this policy revision (`router/<name>/<version>`).
    #[must_use]
    pub fn registry_key(&self) -> String {
        format!("router/{}/{}", self.name, self.version)
    }

    /// Whether a hosted model may process data at `classification` under this
    /// policy's privacy ceiling.
    #[must_use]
    pub fn hosted_allows(&self, classification: DataClassification) -> bool {
        classification.allowed_off_device(self.max_off_device)
    }

    /// Validate that this policy's numbers are meaningful (P7-6) and its
    /// escalation chain is unambiguous (P7-1): every λ weight and the quality
    /// threshold must be finite and non-negative, the threshold must sit within
    /// `[0, 1]`, and the escalation chain must not repeat a model id. Called
    /// automatically on deserialization (`TryFrom<RoutingPolicyWire>`); exposed
    /// so any other constructor path can call it too.
    pub fn validate(&self) -> Result<(), PolicyError> {
        for (field, value) in [
            ("cost", self.lambdas.cost),
            ("latency", self.lambdas.latency),
            ("privacy", self.lambdas.privacy),
            ("failure", self.lambdas.failure),
        ] {
            if !value.is_finite() {
                return Err(PolicyError::NonFiniteWeight { field, value });
            }
            if value < 0.0 {
                return Err(PolicyError::NegativeWeight { field, value });
            }
        }
        if !self.quality_threshold.is_finite() {
            return Err(PolicyError::NonFiniteThreshold {
                value: self.quality_threshold,
            });
        }
        if !(0.0..=1.0).contains(&self.quality_threshold) {
            return Err(PolicyError::ThresholdOutOfRange {
                value: self.quality_threshold,
            });
        }
        let mut seen: HashSet<&ModelId> = HashSet::with_capacity(self.escalation_chain.len());
        for model in &self.escalation_chain {
            if !seen.insert(model) {
                return Err(PolicyError::DuplicateModelInChain {
                    model: model.clone(),
                });
            }
        }
        Ok(())
    }

    /// A sensible default policy for tests and first-run: a 0.7 quality bar,
    /// balanced weights, `Confidential` permitted off-device, no escalation chain.
    #[must_use]
    pub fn balanced() -> Self {
        Self {
            name: "balanced".to_string(),
            version: 1,
            lambdas: Lambdas::default(),
            quality_threshold: 0.7,
            escalation_chain: Vec::new(),
            max_off_device: DataClassification::Confidential,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_key_encodes_name_and_version() {
        let mut p = RoutingPolicy::balanced();
        p.name = "coding".into();
        p.version = 7;
        assert_eq!(p.registry_key(), "router/coding/7");
    }

    #[test]
    fn hosted_allows_respects_the_privacy_ceiling() {
        let mut p = RoutingPolicy::balanced();
        p.max_off_device = DataClassification::Internal;
        assert!(p.hosted_allows(DataClassification::Public));
        assert!(p.hosted_allows(DataClassification::Internal));
        assert!(!p.hosted_allows(DataClassification::Confidential));
        assert!(!p.hosted_allows(DataClassification::Secret));
    }

    // --- P7-6: NaN/negative weights and out-of-range thresholds are rejected ---

    #[test]
    fn the_default_balanced_policy_validates() {
        assert!(RoutingPolicy::balanced().validate().is_ok());
    }

    #[test]
    fn a_nan_lambda_weight_is_rejected() {
        let mut p = RoutingPolicy::balanced();
        p.lambdas.cost = f64::NAN;
        assert!(matches!(
            p.validate(),
            Err(PolicyError::NonFiniteWeight { field: "cost", .. })
        ));
    }

    #[test]
    fn an_infinite_lambda_weight_is_rejected() {
        let mut p = RoutingPolicy::balanced();
        p.lambdas.latency = f64::INFINITY;
        assert!(matches!(
            p.validate(),
            Err(PolicyError::NonFiniteWeight {
                field: "latency",
                ..
            })
        ));
    }

    #[test]
    fn a_negative_lambda_weight_is_rejected() {
        let mut p = RoutingPolicy::balanced();
        p.lambdas.privacy = -0.5;
        assert!(matches!(
            p.validate(),
            Err(PolicyError::NegativeWeight {
                field: "privacy",
                value
            }) if value == -0.5
        ));
    }

    #[test]
    fn a_nan_quality_threshold_is_rejected() {
        let mut p = RoutingPolicy::balanced();
        p.quality_threshold = f64::NAN;
        assert!(matches!(
            p.validate(),
            Err(PolicyError::NonFiniteThreshold { .. })
        ));
    }

    #[test]
    fn a_quality_threshold_outside_zero_one_is_rejected() {
        let mut too_high = RoutingPolicy::balanced();
        too_high.quality_threshold = 1.5;
        assert!(matches!(
            too_high.validate(),
            Err(PolicyError::ThresholdOutOfRange { .. })
        ));

        let mut too_low = RoutingPolicy::balanced();
        too_low.quality_threshold = -0.01;
        assert!(matches!(
            too_low.validate(),
            Err(PolicyError::ThresholdOutOfRange { .. })
        ));

        let mut boundary_ok = RoutingPolicy::balanced();
        boundary_ok.quality_threshold = 1.0;
        assert!(boundary_ok.validate().is_ok(), "1.0 is a valid boundary");
    }

    // --- P7-1a: a duplicate model id in the escalation chain is a validation error ---

    #[test]
    fn a_duplicate_model_id_in_the_escalation_chain_is_rejected() {
        let mut p = RoutingPolicy::balanced();
        p.escalation_chain = vec![
            ModelId("a".into()),
            ModelId("b".into()),
            ModelId("a".into()),
        ];
        assert!(matches!(
            p.validate(),
            Err(PolicyError::DuplicateModelInChain { model }) if model == ModelId("a".into())
        ));
    }

    #[test]
    fn a_chain_with_unique_ids_validates() {
        let mut p = RoutingPolicy::balanced();
        p.escalation_chain = vec![
            ModelId("a".into()),
            ModelId("b".into()),
            ModelId("c".into()),
        ];
        assert!(p.validate().is_ok());
    }

    // --- Validation is wired into deserialization, not just an unused method ---

    #[test]
    fn deserializing_a_policy_with_a_duplicate_chain_id_fails() {
        let json = r#"{
            "name": "bad",
            "version": 1,
            "lambdas": {"cost": 1.0, "latency": 0.05, "privacy": 0.5, "failure": 0.5},
            "quality_threshold": 0.7,
            "escalation_chain": ["a", "b", "a"],
            "max_off_device": {"type": "Confidential"}
        }"#;
        let err = serde_json::from_str::<RoutingPolicy>(json).unwrap_err();
        assert!(err.to_string().contains("duplicate model id"));
    }

    #[test]
    fn deserializing_a_policy_with_a_negative_weight_fails() {
        // Unlike NaN (which has no valid JSON number representation, so any JSON
        // encoding of it would be rejected by serde's own type-checking, not by
        // our validation), a negative number is perfectly valid JSON — this only
        // fails because `RoutingPolicy::validate` rejects it.
        let json = r#"{
            "name": "bad",
            "version": 1,
            "lambdas": {"cost": -1.0, "latency": 0.05, "privacy": 0.5, "failure": 0.5},
            "quality_threshold": 0.7,
            "escalation_chain": [],
            "max_off_device": {"type": "Confidential"}
        }"#;
        let err = serde_json::from_str::<RoutingPolicy>(json).unwrap_err();
        assert!(err.to_string().contains("cost"));
    }

    #[test]
    fn deserializing_a_policy_with_an_out_of_range_threshold_fails() {
        let json = r#"{
            "name": "bad",
            "version": 1,
            "lambdas": {"cost": 1.0, "latency": 0.05, "privacy": 0.5, "failure": 0.5},
            "quality_threshold": 1.7,
            "escalation_chain": [],
            "max_off_device": {"type": "Confidential"}
        }"#;
        let err = serde_json::from_str::<RoutingPolicy>(json).unwrap_err();
        assert!(err.to_string().contains("quality_threshold"));
    }

    #[test]
    fn deserializing_a_valid_policy_round_trips() {
        let json = r#"{
            "name": "coding",
            "version": 3,
            "lambdas": {"cost": 1.0, "latency": 0.05, "privacy": 0.5, "failure": 0.5},
            "quality_threshold": 0.7,
            "escalation_chain": ["a", "b"],
            "max_off_device": {"type": "Confidential"}
        }"#;
        let p: RoutingPolicy = serde_json::from_str(json).expect("valid policy deserializes");
        assert_eq!(p.registry_key(), "router/coding/3");
        assert_eq!(p.escalation_chain.len(), 2);
    }
}
