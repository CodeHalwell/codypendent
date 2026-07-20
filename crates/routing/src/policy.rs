//! The routing policy (STEP 7.3): the versioned weights, threshold, escalation
//! chain, and privacy ceiling the router optimizes under.
//!
//! Per Chapter 09, the λ weights and quality threshold live in a **versioned**
//! [`RoutingPolicy`] (a registry item, `router/<name>/<version>`), selectable per
//! scope — so a routing change is a candidate that goes through the Phase 7
//! promotion pipeline, not an edit. Budgets from Phase 5 stay authoritative; the
//! router optimizes *inside* them.

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

/// A named, versioned routing policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}
