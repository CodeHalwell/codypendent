//! The regression suite (STEP 7.4/7.5): fixed failures never come back.
//!
//! Every historical failure that gets fixed adds a guard case to the regression
//! suite (`evals/regressions/`), re-run in CI
//! ([Chapter 13](../../docs/docs/13-observability-evaluation-learning.md), exit
//! criterion 3 — the suite grows over time). It is also the **offline regression
//! gate** the promotion pipeline runs first: a candidate that regresses any guard
//! case cannot advance.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::case::{Assertion, CaseResult, EvalCase, RunObservation};
use crate::cluster::FailureCluster;

/// A growable suite of regression guard cases, keyed by case id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegressionSuite {
    cases: Vec<EvalCase>,
}

impl RegressionSuite {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a guard case (idempotent by id — re-adding the same id replaces it).
    pub fn add(&mut self, case: EvalCase) {
        if let Some(existing) = self.cases.iter_mut().find(|c| c.id == case.id) {
            *existing = case;
        } else {
            self.cases.push(case);
        }
    }

    /// Add a guard case derived from a **fixed** failure cluster: a minimal case
    /// whose id encodes the cluster and which asserts the previously-failing run
    /// now succeeds (tests pass, no forbidden network). The caller supplies the
    /// revision + prompt that reproduces the scenario.
    pub fn add_fixed_cluster(
        &mut self,
        cluster: &FailureCluster,
        repository_revision: impl Into<String>,
        prompt: impl Into<String>,
    ) {
        let id = format!("regression/{}", cluster.key.as_key());
        self.add(EvalCase {
            id,
            repository_revision: repository_revision.into(),
            prompt: prompt.into(),
            policy: "regression".into(),
            expected: vec![Assertion::TestsPass],
            maximum_cost_usd: None,
            maximum_duration_ms: None,
            task_class: Some(cluster.key.task_class.clone()),
        });
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.cases.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cases.is_empty()
    }

    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.cases.iter().any(|c| c.id == id)
    }

    #[must_use]
    pub fn cases(&self) -> &[EvalCase] {
        &self.cases
    }

    /// Evaluate the suite against per-case observations. A case with **no**
    /// observation counts as regressed — an unproven guard case is treated as a
    /// failure, never silently skipped.
    #[must_use]
    pub fn evaluate(&self, observations: &BTreeMap<String, RunObservation>) -> RegressionReport {
        let mut results = Vec::with_capacity(self.cases.len());
        let mut missing = Vec::new();
        for case in &self.cases {
            match observations.get(&case.id) {
                Some(obs) => results.push(case.score(obs)),
                None => missing.push(case.id.clone()),
            }
        }
        RegressionReport { results, missing }
    }
}

/// The result of running the regression suite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegressionReport {
    pub results: Vec<CaseResult>,
    /// Guard cases with no observation supplied (treated as regressions).
    pub missing: Vec<String>,
}

impl RegressionReport {
    /// Whether anything regressed: any failed guard case, or any missing one.
    #[must_use]
    pub fn regressed(&self) -> bool {
        !self.missing.is_empty() || self.results.iter().any(|r| !r.passed())
    }

    /// The ids of guard cases that regressed (failed or missing).
    #[must_use]
    pub fn regressed_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .results
            .iter()
            .filter(|r| !r.passed())
            .map(|r| r.case_id.clone())
            .collect();
        ids.extend(self.missing.iter().cloned());
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{cluster_failures, FailureCluster};
    use crate::grade::{grade, Trace};

    fn passing_obs() -> RunObservation {
        RunObservation {
            tests_passed: Some(true),
            ..Default::default()
        }
    }

    fn a_cluster() -> FailureCluster {
        let g = grade(&Trace {
            trace_id: "t1".into(),
            task_class: "ci-diagnosis".into(),
            tool: Some("cargo".into()),
            error_fingerprint: Some("E0308".into()),
            command_failures: 1,
            ..Default::default()
        });
        cluster_failures(&[g]).into_iter().next().unwrap()
    }

    #[test]
    fn a_fixed_failure_lands_in_the_suite_and_passes() {
        // The exit-criterion-3 flow: fix a clustered failure → add a guard case →
        // re-run → it passes (no regression).
        let mut suite = RegressionSuite::new();
        suite.add_fixed_cluster(&a_cluster(), "fixed-rev", "reproduce the CI failure");
        assert_eq!(suite.len(), 1);
        let case_id = suite.cases()[0].id.clone();
        assert!(suite.contains(&case_id));

        let mut obs = BTreeMap::new();
        obs.insert(case_id, passing_obs());
        let report = suite.evaluate(&obs);
        assert!(!report.regressed(), "the fixed failure now passes");
    }

    #[test]
    fn a_reintroduced_failure_is_caught() {
        let mut suite = RegressionSuite::new();
        suite.add_fixed_cluster(&a_cluster(), "rev", "reproduce");
        let case_id = suite.cases()[0].id.clone();

        // The bug comes back: tests fail again.
        let mut obs = BTreeMap::new();
        obs.insert(
            case_id.clone(),
            RunObservation {
                tests_passed: Some(false),
                ..Default::default()
            },
        );
        let report = suite.evaluate(&obs);
        assert!(report.regressed());
        assert_eq!(report.regressed_ids(), vec![case_id]);
    }

    #[test]
    fn a_missing_observation_counts_as_a_regression() {
        let mut suite = RegressionSuite::new();
        suite.add_fixed_cluster(&a_cluster(), "rev", "reproduce");
        // No observation supplied — the guard case is unproven, so it regresses.
        let report = suite.evaluate(&BTreeMap::new());
        assert!(
            report.regressed(),
            "an unproven guard case is never silently skipped"
        );
    }

    #[test]
    fn adding_a_case_is_idempotent_by_id() {
        let mut suite = RegressionSuite::new();
        let cluster = a_cluster();
        suite.add_fixed_cluster(&cluster, "rev1", "reproduce");
        suite.add_fixed_cluster(&cluster, "rev2", "reproduce again");
        assert_eq!(
            suite.len(),
            1,
            "same cluster ⇒ same guard-case id ⇒ no duplicate"
        );
        assert_eq!(
            suite.cases()[0].repository_revision,
            "rev2",
            "re-add replaces"
        );
    }
}
