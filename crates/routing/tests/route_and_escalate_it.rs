//! End-to-end routing scenario (Phase 7, STEP 7.3): classify → route → escalate,
//! and the five-arm route evaluation with the release gate.
//!
//! Unit tests cover each stage in isolation; this exercises the whole pipeline
//! the way the daemon will drive it — a realistic model fleet (a local tier plus
//! two hosted tiers), a policy with an escalation chain, and a task node that
//! fails on the cheap tier and must escalate.

use codypendent_protocol::artifact::DataClassification;
use codypendent_protocol::ids::ModelId;
use codypendent_routing::{
    arms::{RouteArm, RouteArmResult, RouteEvalReport},
    capability::{
        ModelCapabilities, RequiredCapabilities, StructuredOutputSupport, ToolCallSupport,
    },
    classify::{classify, TaskSignals},
    policy::RoutingPolicy,
    profile::{ModelExecutionProfile, ModelLocation, ModelPerformance, ModelProfile},
    router::{Router, SelectionReason, TaskNode},
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
) -> ModelProfile {
    ModelProfile {
        id: ModelId(id.into()),
        location,
        capabilities: caps(200_000),
        performance: ModelPerformance {
            reliability,
            cost_per_1k_tokens_usd: cost_per_1k,
            latency_ms_p50: latency_ms,
            task_class_success: BTreeMap::new(),
            failure_patterns: vec![],
        },
        execution: ModelExecutionProfile::default(),
        bench: None,
    }
}

/// The canonical three-tier fleet: a cheap local model, a mid hosted model, and a
/// strong hosted model — the Chapter 09 `local-default → hosted-default →
/// hosted-strong` chain.
fn fleet() -> Vec<ModelProfile> {
    vec![
        model("local-default", ModelLocation::Local, 0.72, 0.000, 1600.0),
        model("hosted-default", ModelLocation::Hosted, 0.86, 0.010, 700.0),
        model("hosted-strong", ModelLocation::Hosted, 0.96, 0.030, 600.0),
    ]
}

fn policy_with_chain() -> RoutingPolicy {
    let mut policy = RoutingPolicy::balanced();
    policy.escalation_chain = vec![
        ModelId("local-default".into()),
        ModelId("hosted-default".into()),
        ModelId("hosted-strong".into()),
    ];
    policy
}

fn node(objective: &str, data: DataClassification) -> TaskNode {
    TaskNode {
        classification: classify(&TaskSignals::from_objective(
            "build", "agent", 6_000, objective,
        )),
        required: RequiredCapabilities {
            tools: true,
            structured_output: true,
            min_context_tokens: 30_000,
            min_output_tokens: 4_000,
            ..Default::default()
        },
        data_classification: data,
        estimated_input_tokens: 10_000,
        estimated_output_tokens: 4_000,
    }
}

#[test]
fn a_task_routes_cheap_then_escalates_the_full_chain_on_repeated_failure() {
    let models = fleet();
    let policy = policy_with_chain();
    let router = Router::new(&models, &policy);
    let task = node(
        "fix the failing pagination test",
        DataClassification::Internal,
    );

    // 1. Classified correctly.
    assert_eq!(
        task.classification.class,
        codypendent_routing::TaskClass::FailingTestDiagnosis
    );

    // 2. Initial route: the cheapest model above the 0.7 bar is the free local one.
    let first = router.route(&task).unwrap();
    assert_eq!(first.model, ModelId("local-default".into()));
    assert_eq!(first.reason, SelectionReason::CheapestAboveThreshold);
    assert_eq!(first.expected_cost_usd, 0.0, "local model is free");

    // 3. The local model's patch fails validation → escalate one tier.
    let (second, t1) = router
        .escalate(&first.model, "targeted tests still fail", &task)
        .unwrap();
    assert_eq!(second.model, ModelId("hosted-default".into()));
    assert!(
        t1.artifacts_preserved,
        "escalation preserves artifacts, not a restart"
    );
    assert!(
        t1.cost_impact_usd > 0.0,
        "moving to a paid model costs more"
    );

    // 4. That still fails → escalate to the strong tier.
    let (third, t2) = router
        .escalate(&second.model, "still red after hosted-default", &task)
        .unwrap();
    assert_eq!(third.model, ModelId("hosted-strong".into()));
    assert_eq!(t2.from, ModelId("hosted-default".into()));

    // 5. The chain is now exhausted.
    let exhausted = router.escalate(&third.model, "even strong failed", &task);
    assert!(exhausted.is_err(), "no tier beyond hosted-strong");

    // Every decision carries the classifier + policy provenance a trace records.
    for decision in [&first, &second, &third] {
        assert_eq!(decision.classifier_version, "rules/1");
        assert_eq!(decision.policy_key, "router/balanced/1");
    }
}

#[test]
fn secret_data_stays_local_and_never_escalates_off_device() {
    // A restrictive privacy ceiling: nothing above Internal may leave the device.
    let models = fleet();
    let mut policy = policy_with_chain();
    policy.max_off_device = DataClassification::Internal;
    let router = Router::new(&models, &policy);
    let task = node(
        "diagnose the failing test in the secret module",
        DataClassification::Secret,
    );

    // The only eligible model is the local one.
    let decision = router.route(&task).unwrap();
    assert_eq!(decision.model, ModelId("local-default".into()));

    // Escalation cannot move Secret data to either hosted tier → exhausted, the
    // security-correct outcome (it refuses rather than leaking).
    let err = router.escalate(&decision.model, "local failed", &task);
    assert!(err.is_err(), "sensitive data never escalates off-device");
}

#[test]
fn five_arm_route_eval_gate_holds_when_the_router_matches_quality_cheaper() {
    // Drive each arm's *initial* selection over a small case set, and assemble a
    // RouteEvalReport whose measured numbers reflect a router that reaches the
    // strong model's quality (via escalation) at a fraction of always paying for
    // it. The release gate (exit criterion 1) must hold.
    let models = fleet();
    let policy = policy_with_chain();
    let router = Router::new(&models, &policy);

    let cases = [
        node("fix the failing test", DataClassification::Internal),
        node("update the README docs", DataClassification::Internal),
        node(
            "refactor and simplify the module",
            DataClassification::Public,
        ),
    ];

    // Sanity: every arm can select a model for every case (no arm errors out).
    for arm in RouteArm::all() {
        for case in &cases {
            let decision = arm.select(&router, case).unwrap();
            // Static-strongest always lands on the strong model; static-cheap and
            // the router prefer the free local one.
            match arm {
                RouteArm::StaticStrongest => {
                    assert_eq!(decision.model, ModelId("hosted-strong".into()))
                }
                RouteArm::StaticCheap | RouteArm::Router | RouteArm::RouterEscalation => {
                    assert_eq!(decision.model, ModelId("local-default".into()))
                }
                RouteArm::LocalFirstRouter => {
                    assert_eq!(decision.model, ModelId("local-default".into()))
                }
            }
        }
    }

    // Measured outcomes (as the eval harness would record them): router+escalation
    // reaches static-strongest's success at much lower mean cost.
    let report = RouteEvalReport::new(
        0.85,
        vec![
            RouteArmResult {
                arm: RouteArm::StaticStrongest,
                task_success_rate: 0.90,
                mean_cost_usd: 0.42,
                mean_latency_ms: 600.0,
                escalation_rate: 0.0,
                tool_call_error_rate: 0.02,
                unsafe_proposal_rate: 0.0,
            },
            RouteArmResult {
                arm: RouteArm::StaticCheap,
                task_success_rate: 0.58,
                mean_cost_usd: 0.00,
                mean_latency_ms: 1600.0,
                escalation_rate: 0.0,
                tool_call_error_rate: 0.08,
                unsafe_proposal_rate: 0.0,
            },
            RouteArmResult {
                arm: RouteArm::RouterEscalation,
                task_success_rate: 0.89,
                mean_cost_usd: 0.14,
                mean_latency_ms: 950.0,
                escalation_rate: 0.35,
                tool_call_error_rate: 0.03,
                unsafe_proposal_rate: 0.0,
            },
        ],
    );

    assert!(
        report.meets_release_gate(),
        "router+escalation must meet quality at lower cost than static-strongest: {}",
        report.gate_summary()
    );
    assert!(report.gate_summary().contains("PASS"));
}
