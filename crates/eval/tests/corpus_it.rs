//! Phase 7 STEP 7.1: "the core corpus loads and each case parses to a valid
//! `EvalCase`." Loads the REAL, shipped `evals/tasks/core/` suite (found via
//! `CARGO_MANIFEST_DIR`, since a test's working directory is the crate root,
//! not the workspace root) and checks its shape: every file parses, ids are
//! unique and non-empty, every case pins the same fixture revision, the
//! required task classes are represented, and each of the three
//! brief-mandated assertion kinds (no-forbidden-network, approval-requested,
//! command-not-executed) appears at least once.

use std::collections::HashSet;
use std::path::PathBuf;

use codypendent_eval::{Assertion, EvalCase};

fn core_suite_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("evals")
        .join("tasks")
        .join("core")
}

fn load_core_suite() -> Vec<(PathBuf, EvalCase)> {
    let dir = core_suite_dir();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    entries.sort();
    entries
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            let case = serde_json::from_str::<EvalCase>(&text)
                .unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()));
            (path, case)
        })
        .collect()
}

#[test]
fn the_core_suite_ships_a_real_runnable_range_of_cases() {
    let cases = load_core_suite();
    assert!(
        cases.len() >= 8 && cases.len() <= 12,
        "expected 8-12 core-suite cases per the task brief, found {}",
        cases.len()
    );
}

#[test]
fn every_case_file_parses_to_a_valid_eval_case() {
    // load_core_suite() itself panics on any parse failure, naming the file —
    // reaching this line at all is most of the assertion. Also check the
    // basic shape every case must have.
    for (path, case) in load_core_suite() {
        assert!(
            !case.id.is_empty(),
            "{}: case id must not be empty",
            path.display()
        );
        assert!(
            !case.prompt.trim().is_empty(),
            "{}: case prompt must not be empty",
            path.display()
        );
        assert!(
            !case.expected.is_empty(),
            "{}: case {} has no assertions at all",
            path.display(),
            case.id
        );
        assert_eq!(
            case.repository_revision.len(),
            40,
            "{}: repository_revision must be a full 40-character git SHA, got {:?}",
            path.display(),
            case.repository_revision
        );
    }
}

#[test]
fn every_case_id_is_unique() {
    let cases = load_core_suite();
    let mut seen = HashSet::new();
    for (path, case) in &cases {
        assert!(
            seen.insert(case.id.clone()),
            "{}: duplicate case id {:?}",
            path.display(),
            case.id
        );
    }
}

#[test]
fn every_case_pins_the_same_fixture_revision() {
    // The core suite runs entirely against one vendored fixture at one
    // revision (see evals/README.md) — a case pinning a different revision
    // would silently target a different repository state than its siblings.
    let cases = load_core_suite();
    let revisions: HashSet<&str> = cases
        .iter()
        .map(|(_, c)| c.repository_revision.as_str())
        .collect();
    assert_eq!(
        revisions.len(),
        1,
        "expected every core-suite case to pin the same revision, found {revisions:?}"
    );
}

#[test]
fn the_required_task_classes_are_all_represented() {
    let cases = load_core_suite();
    let classes: HashSet<String> = cases
        .iter()
        .filter_map(|(_, c)| c.task_class.clone())
        .collect();
    for required in [
        "failing-test-diagnosis",
        "small-bug-fix",
        "regression-test-addition",
        "doc-update",
        "ci-diagnosis",
        "safe-refactor",
    ] {
        assert!(
            classes.contains(required),
            "no core-suite case declares task_class {required:?}; found {classes:?}"
        );
    }
}

#[test]
fn the_three_brief_mandated_assertion_kinds_each_appear_at_least_once() {
    let cases = load_core_suite();
    let mut has_no_forbidden_network = false;
    let mut has_approval_requested = false;
    let mut has_command_not_executed = false;
    for (_, case) in &cases {
        for assertion in &case.expected {
            match assertion {
                Assertion::NoForbiddenNetwork { .. } => has_no_forbidden_network = true,
                Assertion::ApprovalRequested => has_approval_requested = true,
                Assertion::CommandNotExecuted { .. } => has_command_not_executed = true,
                _ => {}
            }
        }
    }
    assert!(
        has_no_forbidden_network,
        "no case asserts no-forbidden-network"
    );
    assert!(has_approval_requested, "no case asserts approval-requested");
    assert!(
        has_command_not_executed,
        "no case asserts command-not-executed"
    );
}

#[test]
fn a_case_that_asserts_tests_pass_actually_resolves_the_seeded_bug() {
    // The fixture's ONE seeded failure (math::add_one) makes `tests-pass` a
    // whole-suite signal (RunObservation::tests_passed is one bool for the
    // whole `cargo test` run, not per-test) — a case that asserts `tests-pass`
    // without also fixing that bug could never pass no matter what the agent
    // does. This test only confirms at least one such case exists and that
    // its prompt actually asks for the fix (a cheap, deliberately loose text
    // check — the real proof is `eval_it.rs`'s known-pass/known-fail smoke
    // test in the cli crate).
    let cases = load_core_suite();
    let fixes_the_bug: Vec<&EvalCase> = cases
        .iter()
        .map(|(_, c)| c)
        .filter(|c| c.expected.contains(&Assertion::TestsPass))
        .collect();
    assert!(
        !fixes_the_bug.is_empty(),
        "no case asserts tests-pass at all"
    );
    for case in fixes_the_bug {
        assert!(
            case.prompt.to_lowercase().contains("add_one") || case.prompt.to_lowercase().contains("fix"),
            "case {:?} asserts tests-pass but its prompt does not mention fixing the seeded bug: {:?}",
            case.id,
            case.prompt
        );
    }
}
