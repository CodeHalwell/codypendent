//! STEP 5.1 regression: the canonical `docs/specs/workflow.yaml`
//! (`repair-github-check`) parses and compiles, and lowers to the node graph the
//! executor expects. If the manifest shape or the compiler drifts apart, this
//! breaks.

use codypendent_workflow::{
    compile_yaml, compile_yaml_with_registry, ApprovalPolicy, CompileError, NodeAction,
    OrchestrationReason, SetRegistry, WorkflowError, WorkspaceMode,
};

/// The canonical manifest, embedded so the test travels with the crate.
const REPAIR_GITHUB_CHECK: &str = include_str!("../../../docs/specs/workflow.yaml");

/// A registry that knows exactly the tools, skills, and roles the canonical
/// manifest references — the set a correctly configured daemon would present.
fn repair_github_check_registry() -> SetRegistry {
    SetRegistry::new()
        .with_agent_role("investigator")
        .with_agent_role("implementer")
        .with_agent_role("reviewer")
        .with_skill("github.inspect-failed-check")
        .with_skill("code.repair")
        .with_tool("repository.test")
        .with_tool("github.update-pull-request")
}

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

#[test]
fn the_canonical_manifest_cross_checks_against_a_matching_registry() {
    // Every tool/skill/role the manifest names resolves against a registry that
    // knows them — the STEP 5.1 cross-check passes end to end.
    let registry = repair_github_check_registry();
    let workflow = compile_yaml_with_registry(REPAIR_GITHUB_CHECK, &registry)
        .expect("the canonical manifest must cross-check against its registry");
    assert_eq!(workflow.id, "repair-github-check");
}

#[test]
fn a_missing_skill_fails_the_cross_check() {
    // Drop `code.repair` from the registry; the `patch` step can no longer resolve
    // its skill, so the cross-check rejects the otherwise-valid manifest.
    let registry = SetRegistry::new()
        .with_agent_role("investigator")
        .with_agent_role("implementer")
        .with_agent_role("reviewer")
        .with_skill("github.inspect-failed-check")
        .with_tool("repository.test")
        .with_tool("github.update-pull-request");

    let err = compile_yaml_with_registry(REPAIR_GITHUB_CHECK, &registry).unwrap_err();
    match err {
        WorkflowError::Compile(CompileError::UnknownSkill { step, skill }) => {
            assert_eq!(step, "patch");
            assert_eq!(skill, "code.repair");
        }
        other => panic!("expected an UnknownSkill compile error, got {other:?}"),
    }
}

#[test]
fn a_missing_tool_fails_the_cross_check() {
    // A registry with the agents/skills but neither tool: the first tool step in
    // topological order (`verify` → `repository.test`) is the one reported.
    let registry = SetRegistry::new()
        .with_agent_role("investigator")
        .with_agent_role("implementer")
        .with_agent_role("reviewer")
        .with_skill("github.inspect-failed-check")
        .with_skill("code.repair");
    let err = compile_yaml_with_registry(REPAIR_GITHUB_CHECK, &registry).unwrap_err();
    match err {
        WorkflowError::Compile(CompileError::UnknownTool { step, tool }) => {
            assert_eq!(step, "verify");
            assert_eq!(tool, "repository.test");
        }
        other => panic!("expected an UnknownTool compile error, got {other:?}"),
    }
}
