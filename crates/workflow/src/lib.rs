//! codypendent-workflow — declarative workflow definitions and their compiler
//! (Phase 5, STEP 5.1).
//!
//! A workflow is authored as YAML with the exact shape of
//! `docs/specs/workflow.yaml`: typed `inputs`, a `budget`, and a list of `steps`,
//! each an agent or tool action with `depends_on` edges, an optional
//! isolated-worktree workspace, an approval policy, a retry policy, and declared
//! `outputs`. [`parse_definition`] reads that YAML into a [`WorkflowDefinition`];
//! [`compile`] validates it (schema version, unique/non-empty ids, exactly one
//! action per step, that every `depends_on` names a real step, that the
//! dependency graph is acyclic, budget sanity, and the multi-agent
//! `orchestration_reason` rule) and lowers it into a [`CompiledWorkflow`] — a
//! topologically ordered node graph the executor drives.
//!
//! The definition + compiler layer holds **no** daemon or agent-framework code,
//! so the format and its validation can be exercised on their own. Reference
//! cross-checking is supplied by a [`registry::WorkflowRegistry`] snapshot the
//! daemon builds from the live registry + loaded agent profiles, so
//! [`compile::compile_with_registry`] can reject an unknown tool/skill/role while
//! the crate itself stays daemon-free. Durable execution storage (STEP 5.2) —
//! workflow runs, node records, and checkpoints — lives in [`store`] over a
//! SQLite pool ([`db`]), still daemon-free so recovery and idempotency are
//! testable in isolation. Lowering the compiled graph onto framework
//! orchestration builders and wiring recovery into the daemon are the remaining
//! steps, tracked in the roadmap.

pub mod agent;
pub mod blackboard;
pub mod compile;
pub mod db;
pub mod model;
pub mod registry;
pub mod store;

pub use agent::{
    parse_agent_profile, AgentBudget, AgentCompletion, AgentPermissions, AgentProfile,
    AgentProfileError,
};
pub use blackboard::{
    BlackboardError, BlackboardItem, BlackboardKind, BlackboardStore, NewBlackboardItem,
};
pub use compile::{
    compile, compile_with_registry, compile_yaml, compile_yaml_with_registry, CompileError,
    CompiledNode, CompiledWorkflow, NodeAction, WorkflowError,
};
pub use model::{
    parse_definition, AgentRef, ApprovalPolicy, OrchestrationReason, ParseError, RetryPolicy,
    WorkflowBudget, WorkflowDefinition, WorkflowInput, WorkflowStep, WorkspaceMode, WorkspaceSpec,
};
pub use registry::{SetRegistry, WorkflowRegistry};
pub use store::{
    blocked_node_ids, ready_node_ids, Checkpoint, NodeState, ResumePlan, WorkflowNodeRecord,
    WorkflowRunRecord,
    WorkflowRunSnapshot, WorkflowRunState, WorkflowStore, WorkflowStoreError,
};
