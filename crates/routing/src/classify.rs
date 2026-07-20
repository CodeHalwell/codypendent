//! Task classification (STEP 7.3): map a task node to a [`TaskClass`].
//!
//! Routing is per **task node**, and the first thing the router needs is *what
//! kind of task this is* — a doc update and a failing-test diagnosis have very
//! different quality/cost trade-offs and different historical performance per
//! model. The classifier is **rule-based first** (mode + node kind + input size +
//! keyword signals → a class), with the door open for a tiny local-model
//! classifier behind a flag later. Its **version is recorded** in every routing
//! decision (and thus every trace), so a change in classification logic is
//! attributable.

use serde::{Deserialize, Serialize};

/// The version tag of the rule-based classifier. Bump when the rules change; it
/// rides along in [`Classification`] so traces attribute a decision to the exact
/// logic that produced it.
pub const RULE_CLASSIFIER_VERSION: &str = "rules/1";

/// The task classes the router distinguishes (the roadmap's benchmark task
/// classes), plus a `General` fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskClass {
    FailingTestDiagnosis,
    SmallBugFix,
    RegressionTestAddition,
    ArchitectureExplanation,
    DocUpdate,
    PrFeedbackResponse,
    CiDiagnosis,
    SafeRefactor,
    /// Anything the rules do not specifically recognize.
    General,
}

impl TaskClass {
    /// A stable string key (used for per-class stats maps).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TaskClass::FailingTestDiagnosis => "failing-test-diagnosis",
            TaskClass::SmallBugFix => "small-bug-fix",
            TaskClass::RegressionTestAddition => "regression-test-addition",
            TaskClass::ArchitectureExplanation => "architecture-explanation",
            TaskClass::DocUpdate => "doc-update",
            TaskClass::PrFeedbackResponse => "pr-feedback-response",
            TaskClass::CiDiagnosis => "ci-diagnosis",
            TaskClass::SafeRefactor => "safe-refactor",
            TaskClass::General => "general",
        }
    }
}

/// The signals the rule-based classifier reads. Deliberately cheap to compute:
/// the agent mode, the workflow node kind, the input size, and lowercase keyword
/// hints extracted from the objective.
#[derive(Debug, Clone, Default)]
pub struct TaskSignals {
    /// The agent mode (e.g. `build`, `explore`, `review`).
    pub mode: String,
    /// The workflow node kind (e.g. `agent`, `tool`, `github`).
    pub node_kind: String,
    /// Estimated input size in tokens.
    pub input_tokens: u64,
    /// Lowercase keyword hints from the objective/prompt.
    pub keywords: Vec<String>,
}

impl TaskSignals {
    /// Build signals from an objective string, splitting it into lowercase
    /// keyword tokens — the convenience path a caller uses when it only has prose.
    #[must_use]
    pub fn from_objective(mode: &str, node_kind: &str, input_tokens: u64, objective: &str) -> Self {
        let keywords = objective
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect();
        Self {
            mode: mode.to_string(),
            node_kind: node_kind.to_string(),
            input_tokens,
            keywords,
        }
    }

    fn has_kw(&self, kw: &str) -> bool {
        self.keywords.iter().any(|k| k == kw)
    }

    fn has_any(&self, kws: &[&str]) -> bool {
        kws.iter().any(|kw| self.has_kw(kw))
    }
}

/// The result of classification: the class and the classifier version that chose
/// it (recorded in the routing decision / trace).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub class: TaskClass,
    pub classifier_version: String,
}

/// Classify a task node from its signals, rule-based. The rules are ordered most-
/// to least specific; the first match wins, and an unrecognized task falls to
/// [`TaskClass::General`].
#[must_use]
pub fn classify(signals: &TaskSignals) -> Classification {
    let class = classify_class(signals);
    Classification {
        class,
        classifier_version: RULE_CLASSIFIER_VERSION.to_string(),
    }
}

fn classify_class(s: &TaskSignals) -> TaskClass {
    // CI / GitHub-check repair is distinctive: a github node or ci/check keywords,
    // combined with a failure signal.
    if (s.node_kind == "github" || s.has_any(&["ci", "check", "workflow", "pipeline"]))
        && s.has_any(&["fail", "failing", "failed", "red", "broken", "repair"])
    {
        return TaskClass::CiDiagnosis;
    }
    // Failing-test diagnosis: a test that is failing (but not the CI system itself).
    if s.has_any(&["test", "tests"])
        && s.has_any(&["fail", "failing", "failed", "diagnose", "flaky"])
    {
        return TaskClass::FailingTestDiagnosis;
    }
    // Regression-test addition: adding/writing a test.
    if s.has_any(&["test", "tests", "regression"]) && s.has_any(&["add", "write", "cover", "new"]) {
        return TaskClass::RegressionTestAddition;
    }
    // PR-feedback response: responding to review comments.
    if s.has_any(&["pr", "review", "comment", "feedback", "reviewer"]) {
        return TaskClass::PrFeedbackResponse;
    }
    // Doc update: documentation work.
    if s.has_any(&[
        "doc",
        "docs",
        "documentation",
        "readme",
        "changelog",
        "comment",
    ]) || s.node_kind == "doc"
    {
        return TaskClass::DocUpdate;
    }
    // Architecture explanation: an explain/how-does in explore/review mode, or a
    // large read-only input with no edit keywords.
    if s.has_any(&["explain", "architecture", "design", "overview", "how"])
        && (s.mode == "explore" || s.mode == "review" || s.mode.is_empty())
    {
        return TaskClass::ArchitectureExplanation;
    }
    // Safe refactor: a refactor with no behavior change.
    if s.has_any(&[
        "refactor", "rename", "extract", "cleanup", "tidy", "simplify",
    ]) {
        return TaskClass::SafeRefactor;
    }
    // Small bug fix: a fix in build mode with a modest input.
    if s.has_any(&["fix", "bug", "patch", "repair"]) {
        return TaskClass::SmallBugFix;
    }
    TaskClass::General
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(mode: &str, kind: &str, obj: &str) -> TaskSignals {
        TaskSignals::from_objective(mode, kind, 2_000, obj)
    }

    #[test]
    fn records_the_classifier_version() {
        let c = classify(&sig("build", "agent", "fix the null pointer bug"));
        assert_eq!(c.classifier_version, RULE_CLASSIFIER_VERSION);
    }

    #[test]
    fn ci_check_repair_is_ci_diagnosis() {
        let c = classify(&sig(
            "build",
            "github",
            "the CI check is failing, repair it",
        ));
        assert_eq!(c.class, TaskClass::CiDiagnosis);
    }

    #[test]
    fn failing_test_is_failing_test_diagnosis() {
        let c = classify(&sig(
            "build",
            "agent",
            "diagnose why the unit test is failing",
        ));
        assert_eq!(c.class, TaskClass::FailingTestDiagnosis);
    }

    #[test]
    fn adding_a_test_is_regression_test_addition() {
        let c = classify(&sig(
            "build",
            "agent",
            "add a regression test covering the fix",
        ));
        assert_eq!(c.class, TaskClass::RegressionTestAddition);
    }

    #[test]
    fn review_feedback_is_pr_feedback_response() {
        let c = classify(&sig(
            "build",
            "agent",
            "respond to the reviewer feedback on the PR",
        ));
        assert_eq!(c.class, TaskClass::PrFeedbackResponse);
    }

    #[test]
    fn doc_work_is_doc_update() {
        let c = classify(&sig("build", "agent", "update the README documentation"));
        assert_eq!(c.class, TaskClass::DocUpdate);
    }

    #[test]
    fn explain_in_explore_is_architecture_explanation() {
        let c = classify(&sig(
            "explore",
            "agent",
            "explain the architecture of the router",
        ));
        assert_eq!(c.class, TaskClass::ArchitectureExplanation);
    }

    #[test]
    fn refactor_is_safe_refactor() {
        let c = classify(&sig("build", "agent", "refactor and simplify this module"));
        assert_eq!(c.class, TaskClass::SafeRefactor);
    }

    #[test]
    fn plain_fix_is_small_bug_fix() {
        let c = classify(&sig("build", "agent", "fix the off-by-one error"));
        assert_eq!(c.class, TaskClass::SmallBugFix);
    }

    #[test]
    fn unrecognized_falls_to_general() {
        let c = classify(&sig("build", "agent", "do the needful thing please"));
        assert_eq!(c.class, TaskClass::General);
    }
}
