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
//! testable in isolation. [`drive`] closes the loop over that store: a
//! [`WorkflowDriver`](drive::WorkflowDriver) advances a run through the ready
//! frontier, executing each node via a [`NodeExecutor`](drive::NodeExecutor)
//! seam and recording every transition — resumable and model-free, so the whole
//! lifecycle is tested with a fake executor. [`conductor`] sits above the store
//! and driver: a [`WorkflowConductor`](conductor::WorkflowConductor) recompiles a
//! run's stored manifest so the daemon supplies only a run id, and composes the
//! store operations into the run lifecycle — drive a created run, recover the
//! incomplete runs after a restart, and pause/resume/retry a run — still
//! daemon-free and model-free, so all of it is tested with a fake executor. Role
//! resolution ([`resolve`]) binds a manifest's short role to an `agent.toml`
//! profile. The daemon fills the [`NodeExecutor`](drive::NodeExecutor) seam with
//! the real agent loop / tool layer and adds the transport; this crate owns the
//! scheduling, recovery, and lifecycle logic.

pub mod agent;
pub mod binding;
pub mod blackboard;
pub mod compile;
pub mod conductor;
pub mod db;
pub mod drive;
pub mod model;
pub mod registry;
pub mod resolve;
pub mod store;

pub use agent::{
    parse_agent_profile, AgentBudget, AgentCompletion, AgentPermissions, AgentProfile,
    AgentProfileError,
};
pub use binding::{bind_with, normalize_tool_name, scan_input_refs};
pub use blackboard::{
    BlackboardError, BlackboardItem, BlackboardKind, BlackboardStore, NewBlackboardItem,
};
pub use compile::{
    compile, compile_with_registry, compile_yaml, compile_yaml_with_registry, CompileError,
    CompiledNode, CompiledWorkflow, NodeAction, WorkflowError,
};
pub use conductor::{ConductorError, RecoveryReport, WorkflowConductor};
pub use drive::{NodeContext, NodeExecutor, NodeObserver, NodeOutcome, WorkflowDriver};
pub use model::{
    parse_definition, AgentRef, ApprovalPolicy, OrchestrationReason, ParseError, RetryPolicy,
    WorkflowBudget, WorkflowDefinition, WorkflowInput, WorkflowStep, WorkspaceMode, WorkspaceSpec,
};
pub use registry::{SetRegistry, WorkflowRegistry};
pub use resolve::{AgentProfileSet, AgentProfileSetError, UnresolvedRole};
pub use store::{
    blocked_node_ids, ready_node_ids, Checkpoint, NodeState, ResumePlan, WorkflowNodeRecord,
    WorkflowRunRecord, WorkflowRunSnapshot, WorkflowRunState, WorkflowStore, WorkflowStoreError,
};
