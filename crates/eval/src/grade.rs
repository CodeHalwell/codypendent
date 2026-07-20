//! Trace graders (STEP 7.4): execution-grounded [`Signal`]s from a terminal run.
//!
//! A grader consumes a terminal-run [`Trace`] and emits the
//! [Chapter 13](../../docs/docs/13-observability-evaluation-learning.md) objective
//! signals (`+patch applies` … `−policy violation`) as a [`TraceGrade`]. The core
//! set is **execution-grounded only** — no model-vibes grading (an optional LLM
//! rubric grader may exist elsewhere, marked subjective, and never gates alone).
//! The grade's signals are the input to failure clustering ([`crate::cluster`])
//! and, positively, to skill-synthesis candidates.

use serde::{Deserialize, Serialize};

/// An objective, execution-grounded signal (Chapter 13). Positive signals reward,
/// negative signals penalize; each is derived from a fact in the trace, never a
/// judgment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Signal {
    // Positive.
    PatchApplies,
    CompilationSucceeds,
    TargetedTestsPass,
    FullSuitePasses,
    LintPasses,
    RegressionTestAdded,
    UserAcceptsPatch,
    // Negative.
    InvalidToolCall,
    CommandFailure,
    Regression,
    UnnecessaryEdits,
    ExcessiveCost,
    FabricatedDependency,
    PolicyViolation,
}

impl Signal {
    /// `+1` for a positive signal, `-1` for a negative one.
    #[must_use]
    pub fn polarity(self) -> i32 {
        if self.is_negative() {
            -1
        } else {
            1
        }
    }

    /// Whether this is a negative (failure) signal.
    #[must_use]
    pub fn is_negative(self) -> bool {
        matches!(
            self,
            Signal::InvalidToolCall
                | Signal::CommandFailure
                | Signal::Regression
                | Signal::UnnecessaryEdits
                | Signal::ExcessiveCost
                | Signal::FabricatedDependency
                | Signal::PolicyViolation
        )
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Signal::PatchApplies => "patch-applies",
            Signal::CompilationSucceeds => "compilation-succeeds",
            Signal::TargetedTestsPass => "targeted-tests-pass",
            Signal::FullSuitePasses => "full-suite-passes",
            Signal::LintPasses => "lint-passes",
            Signal::RegressionTestAdded => "regression-test-added",
            Signal::UserAcceptsPatch => "user-accepts-patch",
            Signal::InvalidToolCall => "invalid-tool-call",
            Signal::CommandFailure => "command-failure",
            Signal::Regression => "regression",
            Signal::UnnecessaryEdits => "unnecessary-edits",
            Signal::ExcessiveCost => "excessive-cost",
            Signal::FabricatedDependency => "fabricated-dependency",
            Signal::PolicyViolation => "policy-violation",
        }
    }
}

/// A terminal-run trace — the execution facts a grader reads. Every field is an
/// observed outcome, so the grade is reproducible from the trace alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    pub trace_id: String,
    /// The task class (a string key, matching the router's task classes).
    pub task_class: String,
    /// The primary tool involved (for clustering), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub patch_applies: bool,
    pub compiles: bool,
    pub targeted_tests_pass: bool,
    pub full_suite_passes: bool,
    pub lint_passes: bool,
    pub regression_test_added: bool,
    pub user_accepted: bool,
    pub invalid_tool_calls: u32,
    pub command_failures: u32,
    pub caused_regression: bool,
    pub unnecessary_edits: u32,
    pub cost_usd: f64,
    pub cost_budget_usd: f64,
    pub fabricated_dependency: bool,
    pub policy_violations: u32,
    /// A stable fingerprint of the primary error (for clustering), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_fingerprint: Option<String>,
}

impl Default for Trace {
    fn default() -> Self {
        Self {
            trace_id: String::new(),
            task_class: "general".into(),
            tool: None,
            patch_applies: false,
            compiles: false,
            targeted_tests_pass: false,
            full_suite_passes: false,
            lint_passes: false,
            regression_test_added: false,
            user_accepted: false,
            invalid_tool_calls: 0,
            command_failures: 0,
            caused_regression: false,
            unnecessary_edits: 0,
            cost_usd: 0.0,
            cost_budget_usd: f64::INFINITY,
            fabricated_dependency: false,
            policy_violations: 0,
            error_fingerprint: None,
        }
    }
}

/// The grade of a trace: its signals, kept in a stable order, with the metadata
/// clustering needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceGrade {
    pub trace_id: String,
    pub task_class: String,
    pub tool: Option<String>,
    pub error_fingerprint: Option<String>,
    /// The signals, sorted (deterministic).
    pub signals: Vec<Signal>,
}

impl TraceGrade {
    /// The net score: sum of signal polarities (positive minus negative count).
    #[must_use]
    pub fn score(&self) -> i32 {
        self.signals.iter().map(|s| s.polarity()).sum()
    }

    /// The negative signals present (the failure axes for clustering).
    #[must_use]
    pub fn negative_signals(&self) -> Vec<Signal> {
        self.signals
            .iter()
            .copied()
            .filter(|s| s.is_negative())
            .collect()
    }

    /// Whether the trace carries any negative signal — the gate for entering the
    /// failure-clustering queue.
    #[must_use]
    pub fn has_negative_signal(&self) -> bool {
        self.signals.iter().any(|s| s.is_negative())
    }
}

/// Grade a trace into its objective signals. Deterministic and execution-grounded.
#[must_use]
pub fn grade(trace: &Trace) -> TraceGrade {
    let mut signals = Vec::new();
    // Positive signals.
    if trace.patch_applies {
        signals.push(Signal::PatchApplies);
    }
    if trace.compiles {
        signals.push(Signal::CompilationSucceeds);
    }
    if trace.targeted_tests_pass {
        signals.push(Signal::TargetedTestsPass);
    }
    if trace.full_suite_passes {
        signals.push(Signal::FullSuitePasses);
    }
    if trace.lint_passes {
        signals.push(Signal::LintPasses);
    }
    if trace.regression_test_added {
        signals.push(Signal::RegressionTestAdded);
    }
    if trace.user_accepted {
        signals.push(Signal::UserAcceptsPatch);
    }
    // Negative signals.
    if trace.invalid_tool_calls > 0 {
        signals.push(Signal::InvalidToolCall);
    }
    if trace.command_failures > 0 {
        signals.push(Signal::CommandFailure);
    }
    if trace.caused_regression {
        signals.push(Signal::Regression);
    }
    if trace.unnecessary_edits > 0 {
        signals.push(Signal::UnnecessaryEdits);
    }
    if trace.cost_usd > trace.cost_budget_usd {
        signals.push(Signal::ExcessiveCost);
    }
    if trace.fabricated_dependency {
        signals.push(Signal::FabricatedDependency);
    }
    if trace.policy_violations > 0 {
        signals.push(Signal::PolicyViolation);
    }
    signals.sort();
    TraceGrade {
        trace_id: trace.trace_id.clone(),
        task_class: trace.task_class.clone(),
        tool: trace.tool.clone(),
        error_fingerprint: trace.error_fingerprint.clone(),
        signals,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn success_trace() -> Trace {
        Trace {
            trace_id: "t1".into(),
            task_class: "small-bug-fix".into(),
            tool: Some("cargo".into()),
            patch_applies: true,
            compiles: true,
            targeted_tests_pass: true,
            full_suite_passes: true,
            lint_passes: true,
            regression_test_added: true,
            user_accepted: true,
            cost_usd: 0.10,
            cost_budget_usd: 0.50,
            ..Default::default()
        }
    }

    #[test]
    fn a_clean_success_grades_all_positive() {
        let g = grade(&success_trace());
        assert!(g.score() > 0);
        assert!(!g.has_negative_signal());
        assert!(g.signals.contains(&Signal::TargetedTestsPass));
    }

    #[test]
    fn failures_produce_negative_signals() {
        let mut t = success_trace();
        t.targeted_tests_pass = false;
        t.full_suite_passes = false;
        t.command_failures = 2;
        t.caused_regression = true;
        let g = grade(&t);
        assert!(g.has_negative_signal());
        let negatives = g.negative_signals();
        assert!(negatives.contains(&Signal::CommandFailure));
        assert!(negatives.contains(&Signal::Regression));
    }

    #[test]
    fn excessive_cost_is_signalled_when_over_budget() {
        let mut t = success_trace();
        t.cost_usd = 1.00;
        t.cost_budget_usd = 0.50;
        let g = grade(&t);
        assert!(g.signals.contains(&Signal::ExcessiveCost));
    }

    #[test]
    fn grading_is_deterministic_and_sorted() {
        let t = success_trace();
        let a = grade(&t);
        let b = grade(&t);
        assert_eq!(a, b);
        let mut sorted = a.signals.clone();
        sorted.sort();
        assert_eq!(a.signals, sorted, "signals are emitted in sorted order");
    }

    #[test]
    fn polarity_maps_positive_and_negative() {
        assert_eq!(Signal::PatchApplies.polarity(), 1);
        assert_eq!(Signal::PolicyViolation.polarity(), -1);
        assert!(Signal::Regression.is_negative());
        assert!(!Signal::FullSuitePasses.is_negative());
    }
}
