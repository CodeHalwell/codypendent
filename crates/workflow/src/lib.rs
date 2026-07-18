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
//! This crate is intentionally small and dependency-light (serde + a YAML parser
//! only): it holds **no** daemon, database, or agent-framework code, so the
//! definition format and its validation can be exercised on their own. Lowering
//! the compiled graph onto framework orchestration builders, and cross-checking
//! tool/skill/agent references against the live registry, are wiring steps that
//! belong to the runtime and are tracked separately in the roadmap.

pub mod compile;
pub mod model;

pub use compile::{
    compile, compile_yaml, CompileError, CompiledNode, CompiledWorkflow, NodeAction, WorkflowError,
};
pub use model::{
    parse_definition, AgentRef, ApprovalPolicy, OrchestrationReason, ParseError, RetryPolicy,
    WorkflowBudget, WorkflowDefinition, WorkflowInput, WorkflowStep, WorkspaceMode, WorkspaceSpec,
};
