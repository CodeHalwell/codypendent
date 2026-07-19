//! The declarative workflow definition (STEP 5.1) — the parsed shape of
//! `docs/specs/workflow.yaml`, before validation.
//!
//! These types are a faithful serde mirror of the manifest: parsing here does no
//! validation beyond what serde enforces (shape, enum spellings, required keys).
//! Semantic checks — acyclic graph, resolvable dependencies, one action per
//! step, budget sanity — live in [`crate::compile`], so a caller always parses
//! first and compiles second.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The schema version this build understands. A manifest declaring a different
/// `schema_version` is rejected by [`crate::compile`] rather than silently
/// misinterpreted.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// A parse failure: the YAML was malformed or did not match the manifest shape.
#[derive(Debug, thiserror::Error)]
#[error("invalid workflow manifest: {0}")]
pub struct ParseError(#[from] serde_yaml::Error);

/// Parse a workflow manifest from YAML into its [`WorkflowDefinition`]. This does
/// **not** validate the definition — call [`crate::compile`] on the result.
pub fn parse_definition(yaml: &str) -> Result<WorkflowDefinition, ParseError> {
    Ok(serde_yaml::from_str(yaml)?)
}

/// A declarative workflow, as authored (the shape of `docs/specs/workflow.yaml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDefinition {
    /// Manifest schema version (must be [`SUPPORTED_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The workflow's stable identifier (e.g. `repair-github-check`).
    pub id: String,
    /// The workflow's own version. Changing the manifest without bumping this is
    /// an error the registry enforces (STEP 5.1); the compiler requires `>= 1`.
    pub version: u32,
    /// A human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Typed inputs, keyed by name. Each declares a `type` and whether it is
    /// `required`.
    #[serde(default)]
    pub inputs: BTreeMap<String, WorkflowInput>,
    /// The workflow-level budget envelope.
    #[serde(default)]
    pub budget: WorkflowBudget,
    /// The steps (nodes) of the workflow.
    #[serde(default)]
    pub steps: Vec<WorkflowStep>,
    /// Why this workflow uses multi-agent orchestration (ADR-008). Required by
    /// the compiler once the workflow has more than one agent step; a single-agent
    /// (Level 1) workflow leaves it unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestration_reason: Option<OrchestrationReason>,
}

/// A typed workflow input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowInput {
    /// The input's type name (e.g. `github_pull_request`).
    #[serde(rename = "type")]
    pub input_type: String,
    /// Whether the input must be supplied.
    #[serde(default)]
    pub required: bool,
}

/// The workflow-level budget envelope. Every field is optional in the manifest;
/// the compiler checks that whatever is present is sane, and that a workflow with
/// agent steps caps its concurrency (`maximum_agents`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowBudget {
    /// Ceiling on total spend, in USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_cost_usd: Option<f64>,
    /// Ceiling on wall-clock duration, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_duration_seconds: Option<u64>,
    /// Ceiling on concurrently running agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_agents: Option<u32>,
}

/// One step (node) of a workflow. A step performs exactly one action: it is
/// either an `agent` step (optionally using a `skill`) or a `tool` step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStep {
    /// The step's id, unique within the workflow and referenced by `depends_on`.
    pub id: String,
    /// The steps that must complete before this one runs.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// The agent that performs this step, if it is an agent step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentRef>,
    /// The tool that performs this step, if it is a tool step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// A skill the agent applies (valid only on an agent step).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// The workspace the step runs in (e.g. an isolated worktree for a writer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceSpec>,
    /// The approval policy gating this step's effects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalPolicy>,
    /// The retry policy for this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryPolicy>,
    /// The blackboard artifact kinds this step is declared to produce.
    #[serde(default)]
    pub outputs: Vec<String>,
}

/// An agent reference on an agent step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRef {
    /// The agent role (resolved to an agent profile at execution time).
    pub role: String,
    /// The model-selection policy for the agent (e.g. `economical-coding`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_policy: Option<String>,
}

/// A step's workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSpec {
    /// How the step's workspace is provisioned.
    pub mode: WorkspaceMode,
}

/// How a step's workspace is provisioned. A writing step in
/// [`WorkspaceMode::IsolatedWorktree`] gets its own worktree so concurrent
/// writers never share one (Phase 5 exit criterion 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceMode {
    /// Runs in the shared repository worktree (default for read-only steps).
    SharedWorktree,
    /// Runs in a dedicated, isolated worktree.
    IsolatedWorktree,
}

/// The approval policy gating a step's effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    /// Require approval before the step performs any write.
    BeforeWrite,
    /// Require approval before the step runs at all.
    Always,
}

/// A step's retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    /// How many times to attempt the step (total attempts, `>= 1`).
    pub attempts: u32,
    /// Backoff between attempts, in seconds.
    #[serde(default)]
    pub backoff_seconds: u64,
}

impl Default for RetryPolicy {
    /// The default policy runs a step once with no backoff.
    fn default() -> Self {
        Self {
            attempts: 1,
            backoff_seconds: 0,
        }
    }
}

/// Why a workflow uses multi-agent orchestration (ADR-008 router justification).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OrchestrationReason {
    /// Independent work is run in parallel for throughput.
    Parallelism,
    /// A separate agent reviews another's work for independence.
    IndependentReview,
    /// Steps are separated so each holds only the access it needs.
    AccessSeparation,
    /// Distinct specialists handle distinct sub-problems.
    Specialist,
}
