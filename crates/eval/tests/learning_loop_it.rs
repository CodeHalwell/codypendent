//! End-to-end learning loop (Phase 7, STEP 7.4/7.5): grade → cluster → fix →
//! regression-guard → promote, and the no-self-promotion invariant across the
//! whole flow.
//!
//! Unit tests cover each stage; this exercises the full self-improvement loop the
//! way the daemon will: production traces are graded, their failures cluster, a
//! candidate fix is drafted, the fixed failure becomes a regression guard case,
//! and the candidate is driven through the promotion pipeline — where only a
//! human can flip the final switch.

use std::collections::BTreeMap;

use codypendent_eval::{
    cluster_failures, grade, rank_by_frequency, ActiveVersions, ArtifactKind, ArtifactVersion,
    CanaryOutcome, Candidate, PromotionError, PromotionRecord, PromotionStage, RegressionSuite,
    RunObservation, Signal, Trace,
};
use codypendent_protocol::events::Actor;
use codypendent_protocol::ids::{AgentId, ModelId, RunId, UserId};

fn human() -> Actor {
    Actor::Human {
        user_id: UserId("danielhalwell".into()),
    }
}

fn grader_agent() -> Actor {
    Actor::Agent {
        agent_id: AgentId::new(),
        run_id: RunId::new(),
        model: ModelId("claude-sonnet-5".into()),
    }
}

/// Drive a candidate all the way through a human approval and return its receipt
/// — the only way to obtain a `PromotionRecord` (its fields are private and it
/// does not deserialize, so it cannot be forged).
fn promote_to_receipt(artifact: ArtifactVersion) -> PromotionRecord {
    let mut c = Candidate::draft(artifact, &human());
    c.run_regression(false).unwrap();
    c.start_shadow().unwrap();
    c.start_canary().unwrap();
    c.observe_canary(false).unwrap();
    c.finish_canary().unwrap();
    c.approve(&human()).unwrap()
}

/// Three production traces for the same failure mode (a cargo command failure on
/// a CI-diagnosis task with the same error fingerprint), plus one clean success.
fn production_traces() -> Vec<Trace> {
    let failing = |id: &str| Trace {
        trace_id: id.into(),
        task_class: "ci-diagnosis".into(),
        tool: Some("cargo".into()),
        error_fingerprint: Some("E0599-no-method".into()),
        patch_applies: true,
        compiles: false,
        command_failures: 1,
        ..Default::default()
    };
    vec![
        failing("run-1"),
        failing("run-2"),
        failing("run-3"),
        Trace {
            trace_id: "run-4".into(),
            task_class: "doc-update".into(),
            patch_applies: true,
            compiles: true,
            targeted_tests_pass: true,
            full_suite_passes: true,
            user_accepted: true,
            ..Default::default()
        },
    ]
}

#[test]
fn a_recurring_failure_clusters_becomes_a_guard_case_and_a_fix_promotes() {
    // 1. GRADE every production trace.
    let grades: Vec<_> = production_traces().iter().map(grade).collect();
    // The clean run has no negative signal; the three failures do.
    assert_eq!(grades.iter().filter(|g| g.has_negative_signal()).count(), 3);

    // 2. CLUSTER the failures. The three identical failures form one cluster.
    let clusters = rank_by_frequency(cluster_failures(&grades));
    assert!(!clusters.is_empty());
    let top = &clusters[0];
    assert_eq!(
        top.count(),
        3,
        "the recurring failure is the biggest cluster"
    );
    assert_eq!(top.key.failing_signal, Signal::CommandFailure);
    assert_eq!(top.key.task_class, "ci-diagnosis");
    assert_eq!(top.exemplars, vec!["run-1", "run-2", "run-3"]);

    // 3. FIX → the fixed cluster becomes a regression guard case.
    let mut suite = RegressionSuite::new();
    suite.add_fixed_cluster(top, "fixed-revision", "reproduce the E0599 CI failure");
    let case_id = suite.cases()[0].id.clone();

    // The guard case passes now that the bug is fixed (no regression).
    let mut obs = BTreeMap::new();
    obs.insert(
        case_id.clone(),
        RunObservation {
            tests_passed: Some(true),
            ..Default::default()
        },
    );
    assert!(!suite.evaluate(&obs).regressed(), "the fix holds");

    // 4. PROMOTE a candidate skill fix through the pipeline. The candidate is
    //    drafted by the grader agent (fine), and the offline gate is exactly this
    //    regression suite passing.
    let artifact = ArtifactVersion::new(ArtifactKind::Skill, "rust-ci", 5);
    let mut candidate = Candidate::draft(artifact.clone(), &grader_agent());
    let regressed = suite.evaluate(&obs).regressed();
    candidate.run_regression(regressed).unwrap();
    assert_eq!(candidate.stage, PromotionStage::RegressionPassed);
    candidate.start_shadow().unwrap();
    candidate.start_canary().unwrap();
    assert_eq!(
        candidate.observe_canary(false).unwrap(),
        CanaryOutcome::Continuing
    );
    candidate.finish_canary().unwrap();

    // 5. The grader agent that drafted it CANNOT promote it (no self-promotion).
    let self_promote = candidate.approve(&grader_agent());
    assert!(matches!(
        self_promote,
        Err(PromotionError::RequiresHumanApproval { .. })
    ));
    assert_eq!(
        candidate.stage,
        PromotionStage::ComparisonReady,
        "still not promoted"
    );

    // 6. A human approves → promoted, and the version activates. Activation
    //    requires this promotion receipt — there is no way to activate v5 without
    //    the human-approved record `approve` returned (the record's fields are
    //    private and it does not deserialize, so it can't be forged).
    let record = candidate.approve(&human()).unwrap();
    assert_eq!(candidate.stage, PromotionStage::Promoted);
    assert_eq!(record.actor_kind(), "human");
    assert_eq!(record.artifact().to_string(), "skill/rust-ci/5");

    let mut active = ActiveVersions::new();
    // The predecessor v4 was itself activated by its own prior human promotion —
    // its receipt comes from a real approval, not a hand-built struct.
    let predecessor_record =
        promote_to_receipt(ArtifactVersion::new(ArtifactKind::Skill, "rust-ci", 4));
    active.activate(&predecessor_record).unwrap();
    active.activate(&record).unwrap();
    assert_eq!(active.active("skill/rust-ci"), Some(5));

    // 7. Rollback is one operation and restores the predecessor (reversible).
    assert_eq!(active.rollback("skill/rust-ci"), Some(4));
    assert_eq!(active.active("skill/rust-ci"), Some(4));
}

#[test]
fn a_candidate_that_reintroduces_the_failure_is_rejected_before_promotion() {
    // The same loop, but the "fix" actually regresses the guard case: the pipeline
    // rejects it at the offline gate — it never reaches shadow, canary, or a human.
    let grades: Vec<_> = production_traces().iter().map(grade).collect();
    let top = rank_by_frequency(cluster_failures(&grades)).remove(0);
    let mut suite = RegressionSuite::new();
    suite.add_fixed_cluster(&top, "rev", "reproduce");
    let case_id = suite.cases()[0].id.clone();

    // The bug is still present: the guard case fails.
    let mut obs = BTreeMap::new();
    obs.insert(
        case_id,
        RunObservation {
            tests_passed: Some(false),
            ..Default::default()
        },
    );
    let report = suite.evaluate(&obs);
    assert!(report.regressed());

    let mut candidate = Candidate::draft(
        ArtifactVersion::new(ArtifactKind::Skill, "rust-ci", 6),
        &human(),
    );
    let err = candidate.run_regression(report.regressed()).unwrap_err();
    assert_eq!(err, PromotionError::RegressedOffline);
    assert_eq!(candidate.stage, PromotionStage::Rejected);
    // No human — and no code path — can promote a rejected candidate.
    assert!(candidate.approve(&human()).is_err());
}

#[test]
fn a_canary_regression_auto_rolls_back_a_human_approved_pipeline() {
    // Even a candidate a human intends to promote is stopped automatically if the
    // canary regresses — stopping a bad change needs no human, only promoting a
    // good one does.
    let mut candidate = Candidate::draft(
        ArtifactVersion::new(ArtifactKind::Router, "tool-selection", 13),
        &human(),
    );
    candidate.run_regression(false).unwrap();
    candidate.start_shadow().unwrap();
    candidate.start_canary().unwrap();
    assert_eq!(
        candidate.observe_canary(true).unwrap(),
        CanaryOutcome::AutoRolledBack
    );
    assert_eq!(candidate.stage, PromotionStage::RolledBack);
    // It cannot then be approved — it never reached the decision point.
    assert!(candidate.approve(&human()).is_err());
}
