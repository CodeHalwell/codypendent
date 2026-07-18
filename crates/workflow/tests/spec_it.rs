//! STEP 5.1 regression: the canonical `docs/specs/workflow.yaml`
//! (`repair-github-check`) parses and compiles, and lowers to the node graph the
//! executor expects. If the manifest shape or the compiler drifts apart, this
//! breaks.

use codypendent_workflow::{
    compile_yaml, ApprovalPolicy, NodeAction, OrchestrationReason, WorkspaceMode,
};

/// The canonical manifest, embedded so the test travels with the crate.
const REPAIR_GITHUB_CHECK: &str = include_str!("../../../docs/specs/workflow.yaml");

#[test]
fn the_canonical_manifest_compiles() {
    let workflow = compile_yaml(REPAIR_GITHUB_CHECK).expect("the canonical manifest must compile");

    assert_eq!(workflow.id, "repair-github-check");
    assert_eq!(workflow.version, 1);
    assert_eq!(
        workflow.orchestration_reason,
        Some(OrchestrationReason::IndependentReview)
    );

    // The typed input is present and required.
    let input = workflow
        .inputs
        .get("pull_request")
        .expect("pull_request input");
    assert_eq!(input.input_type, "github_pull_request");
    assert!(input.required);

    // Budget carried through.
    assert_eq!(workflow.budget.maximum_agents, Some(2));

    // Five nodes, in dependency order.
    let order: Vec<&str> = workflow.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(order, ["inspect", "patch", "verify", "review", "publish"]);

    // Three agent steps (investigator, implementer, reviewer).
    assert_eq!(workflow.agent_node_count(), 3);

    // `inspect` is an investigator agent applying a skill.
    match &workflow.node("inspect").unwrap().action {
        NodeAction::Agent { role, skill, .. } => {
            assert_eq!(role, "investigator");
            assert_eq!(skill.as_deref(), Some("github.inspect-failed-check"));
        }
        other => panic!("expected an agent action, got {other:?}"),
    }

    // `patch` writes in an isolated worktree, gated on approval before writing.
    let patch = workflow.node("patch").unwrap();
    assert_eq!(patch.workspace_mode, WorkspaceMode::IsolatedWorktree);
    assert_eq!(patch.approval, Some(ApprovalPolicy::BeforeWrite));
    assert_eq!(patch.depends_on, vec!["inspect"]);

    // `verify` is a tool step with a retry policy.
    let verify = workflow.node("verify").unwrap();
    assert!(matches!(&verify.action, NodeAction::Tool { name } if name == "repository.test"));
    assert_eq!(verify.retry.attempts, 2);
    assert_eq!(verify.retry.backoff_seconds, 5);

    // `publish` always requires approval and is the graph's sink.
    let publish = workflow.node("publish").unwrap();
    assert_eq!(publish.approval, Some(ApprovalPolicy::Always));
    assert!(publish.dependents.is_empty());
}
