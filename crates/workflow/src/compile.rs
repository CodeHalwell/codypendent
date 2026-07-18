//! Compiling a [`WorkflowDefinition`] into an executable node graph (STEP 5.1).
//!
//! [`compile`] performs every semantic check the manifest shape cannot express on
//! its own and lowers the definition into a [`CompiledWorkflow`]: a
//! topologically ordered list of [`CompiledNode`]s with resolved dependency and
//! dependent edges, ready for the executor to schedule. A definition that fails
//! any check produces a precise [`CompileError`] naming the offending step.
//!
//! What is validated here: the schema version, that ids are present and unique,
//! that every step has exactly one action (an agent — optionally with a skill —
//! or a tool), that each `depends_on` names a real step and no step depends on
//! itself, that the dependency graph is acyclic, that the budget is sane, and the
//! multi-agent `orchestration_reason` rule (ADR-008). What is *not* validated
//! here — because it needs the live registry — is whether a named tool, skill, or
//! agent role actually exists; that check joins when the compiler is wired into
//! the runtime (tracked in the roadmap).

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::model::{
    parse_definition, ApprovalPolicy, OrchestrationReason, ParseError, RetryPolicy, WorkflowBudget,
    WorkflowDefinition, WorkflowInput, WorkspaceMode, SUPPORTED_SCHEMA_VERSION,
};

/// A failure to compile a workflow definition. Each variant names the offending
/// step (or the whole-workflow property) so a caller can report it precisely.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompileError {
    /// The manifest declares a schema version this build does not understand.
    #[error("unsupported schema_version {found} (this build supports {supported})")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    /// The workflow id is empty.
    #[error("workflow id must not be empty")]
    EmptyId,
    /// The workflow version is below the minimum (`1`).
    #[error("workflow version must be >= 1, got {0}")]
    InvalidVersion(u32),
    /// The workflow has no steps.
    #[error("workflow has no steps")]
    NoSteps,
    /// A step's id is empty.
    #[error("a step has an empty id")]
    EmptyStepId,
    /// Two steps share an id.
    #[error("duplicate step id: {0}")]
    DuplicateStepId(String),
    /// A step declares neither an agent nor a tool action.
    #[error("step {0} has no action (needs exactly one of `agent` or `tool`)")]
    StepMissingAction(String),
    /// A step declares both an agent and a tool action.
    #[error("step {0} has both `agent` and `tool` (a step performs exactly one action)")]
    StepAmbiguousAction(String),
    /// A step declares a skill without an agent to apply it.
    #[error("step {0} declares a `skill` but has no `agent` to apply it")]
    SkillWithoutAgent(String),
    /// A step depends on itself.
    #[error("step {0} depends on itself")]
    SelfDependency(String),
    /// A step depends on a step that does not exist.
    #[error("step {step} depends on unknown step {depends_on}")]
    UnknownDependency { step: String, depends_on: String },
    /// The dependency graph has a cycle. The payload lists the steps that could
    /// not be ordered (those on or downstream of the cycle), sorted.
    #[error("workflow dependency graph has a cycle among: {}", .0.join(", "))]
    Cycle(Vec<String>),
    /// A budget field is not sane (non-positive, or a required cap is missing).
    #[error("invalid budget: {0}")]
    InvalidBudget(&'static str),
    /// The workflow uses multiple agents but does not declare why (ADR-008).
    #[error(
        "a multi-agent workflow ({agent_steps} agent steps) must declare an `orchestration_reason`"
    )]
    MissingOrchestrationReason { agent_steps: usize },
}

/// A parse-or-compile failure, for the [`compile_yaml`] convenience path.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    /// The YAML did not parse into a definition.
    #[error(transparent)]
    Parse(#[from] ParseError),
    /// The definition parsed but failed validation.
    #[error(transparent)]
    Compile(#[from] CompileError),
}

/// A validated, executable workflow: its metadata plus a topologically ordered
/// node graph. Producing this is proof the definition passed every check in
/// [`compile`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledWorkflow {
    /// The workflow id.
    pub id: String,
    /// The workflow version.
    pub version: u32,
    /// The typed inputs, keyed by name.
    pub inputs: BTreeMap<String, WorkflowInput>,
    /// The validated budget envelope.
    pub budget: WorkflowBudget,
    /// The declared orchestration reason, if any.
    pub orchestration_reason: Option<OrchestrationReason>,
    /// The nodes, in a valid execution order (every node appears after all its
    /// dependencies).
    pub nodes: Vec<CompiledNode>,
}

impl CompiledWorkflow {
    /// The node with the given id, if present.
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&CompiledNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// The number of agent nodes (Level ≥2 when more than one).
    #[must_use]
    pub fn agent_node_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|node| matches!(node.action, NodeAction::Agent { .. }))
            .count()
    }

    /// A stable hash of the graph's *shape*: the workflow id + version, then each
    /// node's id, action kind, and sorted dependencies, in topological order. Two
    /// definitions with the same shape produce the same signature regardless of
    /// incidental field ordering; any structural change (a node, an edge, an
    /// action-kind flip) changes it. A durable run stores this so resume can
    /// refuse a graph that has changed under it (STEP 5.2).
    #[must_use]
    pub fn signature(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.id.as_bytes());
        hasher.update(b"\x00");
        hasher.update(self.version.to_le_bytes());
        for node in &self.nodes {
            hasher.update(b"\xff");
            hasher.update(node.id.as_bytes());
            let kind: &[u8] = match &node.action {
                NodeAction::Agent { .. } => b"agent",
                NodeAction::Tool { .. } => b"tool",
            };
            hasher.update(kind);
            let mut deps = node.depends_on.clone();
            deps.sort();
            for dep in deps {
                hasher.update(b"\x01");
                hasher.update(dep.as_bytes());
            }
        }
        hex::encode(hasher.finalize())
    }
}

/// A validated workflow node.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledNode {
    /// The node's id.
    pub id: String,
    /// The single action the node performs.
    pub action: NodeAction,
    /// The nodes this one depends on (deduplicated).
    pub depends_on: Vec<String>,
    /// The nodes that depend on this one (deduplicated), in definition order.
    pub dependents: Vec<String>,
    /// How the node's workspace is provisioned (defaulting to the shared worktree).
    pub workspace_mode: WorkspaceMode,
    /// The node's approval policy, if any.
    pub approval: Option<ApprovalPolicy>,
    /// The node's retry policy (defaulting to a single attempt).
    pub retry: RetryPolicy,
    /// The blackboard artifact kinds the node is declared to produce.
    pub outputs: Vec<String>,
    /// The node's position in the compiled topological order.
    pub topo_order: usize,
}

/// The single action a node performs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeAction {
    /// An agent step, optionally applying a skill.
    Agent {
        role: String,
        model_policy: Option<String>,
        skill: Option<String>,
    },
    /// A tool step.
    Tool { name: String },
}

/// Parse a YAML manifest and compile it, in one call.
pub fn compile_yaml(yaml: &str) -> Result<CompiledWorkflow, WorkflowError> {
    let definition = parse_definition(yaml)?;
    Ok(compile(&definition)?)
}

/// Validate `definition` and lower it into a [`CompiledWorkflow`]. See the module
/// docs for the checks performed.
pub fn compile(definition: &WorkflowDefinition) -> Result<CompiledWorkflow, CompileError> {
    if definition.schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(CompileError::UnsupportedSchemaVersion {
            found: definition.schema_version,
            supported: SUPPORTED_SCHEMA_VERSION,
        });
    }
    if definition.id.trim().is_empty() {
        return Err(CompileError::EmptyId);
    }
    if definition.version < 1 {
        return Err(CompileError::InvalidVersion(definition.version));
    }
    if definition.steps.is_empty() {
        return Err(CompileError::NoSteps);
    }

    // Unique, non-empty step ids.
    let mut ids = HashSet::with_capacity(definition.steps.len());
    for step in &definition.steps {
        if step.id.trim().is_empty() {
            return Err(CompileError::EmptyStepId);
        }
        if !ids.insert(step.id.as_str()) {
            return Err(CompileError::DuplicateStepId(step.id.clone()));
        }
    }

    // Exactly one action per step; a skill needs an agent.
    let mut agent_steps = 0usize;
    for step in &definition.steps {
        match (step.agent.is_some(), step.tool.is_some()) {
            (false, false) => return Err(CompileError::StepMissingAction(step.id.clone())),
            (true, true) => return Err(CompileError::StepAmbiguousAction(step.id.clone())),
            (true, false) => agent_steps += 1,
            (false, true) => {}
        }
        if step.skill.is_some() && step.agent.is_none() {
            return Err(CompileError::SkillWithoutAgent(step.id.clone()));
        }
    }

    // Dependencies resolve, no self-loops. Deduplicate within a step so a repeated
    // edge cannot distort the in-degree count.
    let mut deduped_deps: HashMap<&str, Vec<&str>> = HashMap::new();
    for step in &definition.steps {
        let mut seen = HashSet::new();
        let mut deps = Vec::new();
        for dep in &step.depends_on {
            if dep == &step.id {
                return Err(CompileError::SelfDependency(step.id.clone()));
            }
            if !ids.contains(dep.as_str()) {
                return Err(CompileError::UnknownDependency {
                    step: step.id.clone(),
                    depends_on: dep.clone(),
                });
            }
            if seen.insert(dep.as_str()) {
                deps.push(dep.as_str());
            }
        }
        deduped_deps.insert(step.id.as_str(), deps);
    }

    validate_budget(&definition.budget, agent_steps)?;

    if agent_steps >= 2 && definition.orchestration_reason.is_none() {
        return Err(CompileError::MissingOrchestrationReason { agent_steps });
    }

    let order = topological_order(definition, &deduped_deps)?;
    let dependents = dependents_of(definition, &deduped_deps);

    let position: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, index))
        .collect();

    // Emit nodes in topological order.
    let mut nodes = Vec::with_capacity(definition.steps.len());
    for id in &order {
        let step = definition
            .steps
            .iter()
            .find(|step| step.id.as_str() == *id)
            .expect("ordered id came from the step list");
        let action = if let Some(agent) = &step.agent {
            NodeAction::Agent {
                role: agent.role.clone(),
                model_policy: agent.model_policy.clone(),
                skill: step.skill.clone(),
            }
        } else {
            NodeAction::Tool {
                name: step.tool.clone().expect("a non-agent step has a tool"),
            }
        };
        nodes.push(CompiledNode {
            id: step.id.clone(),
            action,
            depends_on: deduped_deps[id].iter().map(|d| (*d).to_owned()).collect(),
            dependents: dependents
                .get(*id)
                .map(|v| v.iter().map(|d| (*d).to_owned()).collect())
                .unwrap_or_default(),
            workspace_mode: step
                .workspace
                .as_ref()
                .map_or(WorkspaceMode::SharedWorktree, |w| w.mode),
            approval: step.approval,
            retry: step.retry.unwrap_or_default(),
            outputs: step.outputs.clone(),
            topo_order: position[id],
        });
    }

    Ok(CompiledWorkflow {
        id: definition.id.clone(),
        version: definition.version,
        inputs: definition.inputs.clone(),
        budget: definition.budget.clone(),
        orchestration_reason: definition.orchestration_reason,
        nodes,
    })
}

/// Check that whatever budget fields are present are sane, and that a workflow
/// with agent steps caps its concurrency.
fn validate_budget(budget: &WorkflowBudget, agent_steps: usize) -> Result<(), CompileError> {
    if let Some(cost) = budget.maximum_cost_usd {
        if !(cost.is_finite() && cost > 0.0) {
            return Err(CompileError::InvalidBudget(
                "maximum_cost_usd must be a positive number",
            ));
        }
    }
    if let Some(0) = budget.maximum_duration_seconds {
        return Err(CompileError::InvalidBudget(
            "maximum_duration_seconds must be greater than zero",
        ));
    }
    match budget.maximum_agents {
        Some(0) => {
            return Err(CompileError::InvalidBudget(
                "maximum_agents must be at least 1",
            ))
        }
        None if agent_steps > 0 => {
            return Err(CompileError::InvalidBudget(
                "maximum_agents is required when the workflow has agent steps",
            ))
        }
        _ => {}
    }
    Ok(())
}

/// Kahn's algorithm, breaking ties in definition order so the result is
/// deterministic. Returns the ordered ids, or [`CompileError::Cycle`] listing the
/// steps that could not be ordered.
fn topological_order<'a>(
    definition: &'a WorkflowDefinition,
    deps: &HashMap<&'a str, Vec<&'a str>>,
) -> Result<Vec<&'a str>, CompileError> {
    let mut in_degree: HashMap<&str, usize> = definition
        .steps
        .iter()
        .map(|step| (step.id.as_str(), deps[step.id.as_str()].len()))
        .collect();

    let mut order = Vec::with_capacity(definition.steps.len());
    let mut emitted: HashSet<&str> = HashSet::new();

    // Repeatedly take the first not-yet-emitted step whose dependencies are all
    // satisfied (definition order = deterministic tie-break). Small graphs make
    // the quadratic scan a non-issue.
    while order.len() < definition.steps.len() {
        let next = definition
            .steps
            .iter()
            .map(|step| step.id.as_str())
            .find(|id| !emitted.contains(id) && in_degree[id] == 0);
        let Some(id) = next else { break };
        emitted.insert(id);
        order.push(id);
        // Relax edges out of `id`.
        for step in &definition.steps {
            if deps[step.id.as_str()].contains(&id) {
                *in_degree.get_mut(step.id.as_str()).unwrap() -= 1;
            }
        }
    }

    if order.len() != definition.steps.len() {
        let mut cycle: Vec<String> = definition
            .steps
            .iter()
            .filter(|step| !emitted.contains(step.id.as_str()))
            .map(|step| step.id.clone())
            .collect();
        cycle.sort();
        return Err(CompileError::Cycle(cycle));
    }
    Ok(order)
}

/// Invert the dependency edges: for each step, the steps that depend on it, in
/// definition order.
fn dependents_of<'a>(
    definition: &'a WorkflowDefinition,
    deps: &HashMap<&'a str, Vec<&'a str>>,
) -> HashMap<&'a str, Vec<&'a str>> {
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for step in &definition.steps {
        for dep in &deps[step.id.as_str()] {
            dependents.entry(*dep).or_default().push(step.id.as_str());
        }
    }
    dependents
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentRef, WorkflowStep, WorkspaceSpec};

    /// A minimal agent step.
    fn agent_step(id: &str, depends_on: &[&str]) -> WorkflowStep {
        WorkflowStep {
            id: id.to_owned(),
            depends_on: depends_on.iter().map(|d| (*d).to_owned()).collect(),
            agent: Some(AgentRef {
                role: "worker".to_owned(),
                model_policy: None,
            }),
            tool: None,
            skill: None,
            workspace: None,
            approval: None,
            retry: None,
            outputs: Vec::new(),
        }
    }

    /// A minimal tool step.
    fn tool_step(id: &str, depends_on: &[&str]) -> WorkflowStep {
        WorkflowStep {
            id: id.to_owned(),
            depends_on: depends_on.iter().map(|d| (*d).to_owned()).collect(),
            agent: None,
            tool: Some("repository.test".to_owned()),
            skill: None,
            workspace: None,
            approval: None,
            retry: None,
            outputs: Vec::new(),
        }
    }

    fn definition(steps: Vec<WorkflowStep>) -> WorkflowDefinition {
        WorkflowDefinition {
            schema_version: 1,
            id: "wf".to_owned(),
            version: 1,
            description: None,
            inputs: BTreeMap::new(),
            budget: WorkflowBudget {
                maximum_cost_usd: Some(1.0),
                maximum_duration_seconds: Some(60),
                maximum_agents: Some(2),
            },
            steps,
            orchestration_reason: None,
        }
    }

    #[test]
    fn compiles_a_linear_pipeline_in_topological_order() {
        let def = definition(vec![
            tool_step("c", &["b"]),
            tool_step("a", &[]),
            tool_step("b", &["a"]),
        ]);
        let compiled = compile(&def).unwrap();
        let order: Vec<&str> = compiled.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(order, ["a", "b", "c"]);
        // topo_order matches position, and dependents are inverted.
        assert_eq!(compiled.node("a").unwrap().dependents, vec!["b"]);
        assert_eq!(compiled.node("c").unwrap().depends_on, vec!["b"]);
        assert_eq!(compiled.node("c").unwrap().topo_order, 2);
    }

    #[test]
    fn rejects_a_cycle() {
        let def = definition(vec![tool_step("a", &["b"]), tool_step("b", &["a"])]);
        let err = compile(&def).unwrap_err();
        match err {
            CompileError::Cycle(nodes) => assert_eq!(nodes, vec!["a", "b"]),
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn rejects_an_unknown_dependency() {
        let def = definition(vec![tool_step("a", &["ghost"])]);
        assert_eq!(
            compile(&def).unwrap_err(),
            CompileError::UnknownDependency {
                step: "a".to_owned(),
                depends_on: "ghost".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_a_self_dependency() {
        let def = definition(vec![tool_step("a", &["a"])]);
        assert_eq!(
            compile(&def).unwrap_err(),
            CompileError::SelfDependency("a".to_owned())
        );
    }

    #[test]
    fn rejects_a_duplicate_step_id() {
        let def = definition(vec![tool_step("a", &[]), tool_step("a", &[])]);
        assert_eq!(
            compile(&def).unwrap_err(),
            CompileError::DuplicateStepId("a".to_owned())
        );
    }

    #[test]
    fn rejects_a_step_with_no_action_or_two_actions() {
        let mut none = tool_step("a", &[]);
        none.tool = None;
        assert_eq!(
            compile(&definition(vec![none])).unwrap_err(),
            CompileError::StepMissingAction("a".to_owned())
        );

        let mut both = tool_step("a", &[]);
        both.agent = Some(AgentRef {
            role: "worker".to_owned(),
            model_policy: None,
        });
        assert_eq!(
            compile(&definition(vec![both])).unwrap_err(),
            CompileError::StepAmbiguousAction("a".to_owned())
        );
    }

    #[test]
    fn rejects_a_skill_without_an_agent() {
        let mut step = tool_step("a", &[]);
        step.skill = Some("code.repair".to_owned());
        assert_eq!(
            compile(&definition(vec![step])).unwrap_err(),
            CompileError::SkillWithoutAgent("a".to_owned())
        );
    }

    #[test]
    fn rejects_an_unsupported_schema_version() {
        let mut def = definition(vec![tool_step("a", &[])]);
        def.schema_version = 99;
        assert_eq!(
            compile(&def).unwrap_err(),
            CompileError::UnsupportedSchemaVersion {
                found: 99,
                supported: 1,
            }
        );
    }

    #[test]
    fn requires_maximum_agents_when_there_are_agent_steps() {
        let mut def = definition(vec![agent_step("a", &[])]);
        def.budget.maximum_agents = None;
        assert!(matches!(
            compile(&def).unwrap_err(),
            CompileError::InvalidBudget(_)
        ));
    }

    #[test]
    fn rejects_a_non_positive_cost_budget() {
        let mut def = definition(vec![tool_step("a", &[])]);
        def.budget.maximum_cost_usd = Some(0.0);
        assert!(matches!(
            compile(&def).unwrap_err(),
            CompileError::InvalidBudget(_)
        ));
    }

    #[test]
    fn multi_agent_workflow_requires_an_orchestration_reason() {
        // Two agent steps with no reason declared → rejected.
        let def = definition(vec![agent_step("a", &[]), agent_step("b", &["a"])]);
        assert_eq!(
            compile(&def).unwrap_err(),
            CompileError::MissingOrchestrationReason { agent_steps: 2 }
        );

        // Declaring the reason makes it valid.
        let mut ok = definition(vec![agent_step("a", &[]), agent_step("b", &["a"])]);
        ok.orchestration_reason = Some(OrchestrationReason::IndependentReview);
        assert!(compile(&ok).is_ok());
    }

    #[test]
    fn single_agent_workflow_needs_no_orchestration_reason() {
        let def = definition(vec![agent_step("only", &[])]);
        assert!(compile(&def).is_ok());
    }

    #[test]
    fn a_duplicate_dependency_edge_does_not_distort_ordering() {
        // `b` lists `a` twice; the dedup keeps in-degree at 1 so `b` still orders
        // after `a` (a naive count would strand `b` and look like a cycle).
        let mut b = tool_step("b", &["a"]);
        b.depends_on.push("a".to_owned());
        let def = definition(vec![tool_step("a", &[]), b]);
        let compiled = compile(&def).unwrap();
        assert_eq!(compiled.node("b").unwrap().depends_on, vec!["a"]);
        let order: Vec<&str> = compiled.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(order, ["a", "b"]);
    }

    #[test]
    fn carries_workspace_approval_and_retry_onto_the_node() {
        let mut step = agent_step("build", &[]);
        step.workspace = Some(WorkspaceSpec {
            mode: WorkspaceMode::IsolatedWorktree,
        });
        step.approval = Some(ApprovalPolicy::BeforeWrite);
        step.retry = Some(RetryPolicy {
            attempts: 3,
            backoff_seconds: 5,
        });
        let compiled = compile(&definition(vec![step])).unwrap();
        let node = compiled.node("build").unwrap();
        assert_eq!(node.workspace_mode, WorkspaceMode::IsolatedWorktree);
        assert_eq!(node.approval, Some(ApprovalPolicy::BeforeWrite));
        assert_eq!(node.retry.attempts, 3);
        // A node with no retry declared defaults to a single attempt.
        let plain = compile(&definition(vec![tool_step("t", &[])])).unwrap();
        assert_eq!(plain.node("t").unwrap().retry, RetryPolicy::default());
    }
}
