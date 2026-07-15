# Agent Runtime and Workflows

## Runtime ownership

`agent-framework-rs` should provide the in-process agent and workflow primitives. Codypendent wraps them with durable daemon semantics.

```text
Codypendent daemon
├── durable task and run state
├── worktree ownership
├── policy and approvals
├── artifact/event persistence
├── model routing
└── recovery
        │
        ▼
agent-framework-rs
├── Agent / ChatClient
├── tools and middleware
├── AgentSession / ContextProvider
├── compaction
├── WorkflowBuilder
├── checkpointing and HITL
└── orchestration builders
```

## Run state machine

```rust
pub enum RunState {
    Queued,
    Preparing,
    Running,
    WaitingForApproval,
    WaitingForUserInput,
    Paused,
    Recovering,
    Completed,
    Failed,
    Cancelled,
}
```

Transitions are persisted before the state is exposed to clients.

## Task model

Agents should not coordinate by exchanging unrestricted conversational transcripts. They operate on typed tasks and artifacts.

```rust
pub struct Task {
    pub id: TaskId,
    pub parent: Option<TaskId>,
    pub objective: String,
    pub inputs: Vec<ArtifactRef>,
    pub constraints: Vec<Constraint>,
    pub required_capabilities: CapabilityRequirements,
    pub acceptance: Vec<AcceptanceCriterion>,
    pub budget: TaskBudget,
}
```

## Orchestration levels

### Level 1: single agent, deterministic workflow

Default:

```text
Inspect → Plan → Modify → Test → Review → Present
```

The same agent may perform several nodes, while the workflow enforces verification and approval boundaries.

### Level 2: specialist delegation

Use when there is meaningful role separation:

```text
Supervisor
├── repository investigator
├── implementation worker
└── independent reviewer
```

### Level 3: parallel map/reduce

Use for large codebases or independent modules:

```text
module inspections
        ↓
evidence aggregation
        ↓
architecture conclusion
```

### Level 4: competitive swarm

Several agents produce independent proposals and a verifier compares them. This is expensive and opt-in.

## Blackboard

```rust
pub enum BlackboardArtifact {
    Finding(Finding),
    Hypothesis(Hypothesis),
    Decision(Decision),
    CodeLocation(CodeLocation),
    ProposedPatch(ArtifactRef),
    TestResult(TestResult),
    DocumentDraft(DocumentId),
    OpenQuestion(OpenQuestion),
}
```

Each item contains:

- author;
- run and task;
- confidence;
- evidence references;
- scope;
- revision;
- supersession status.

## Worktree isolation

Every writing task receives a dedicated Git worktree unless policy explicitly allows direct edits.

```text
repository/
worktrees/
├── run-01-investigation/
├── run-02-implementation/
└── run-03-review/
```

The manager tracks:

- branch and base commit;
- worktree path;
- owner run;
- write lease;
- process IDs;
- bound services;
- dirty state;
- unmerged commits;
- cleanup status.

Git remains authoritative. On startup, the daemon reconciles records with:

```bash
git worktree list --porcelain
```

## Service allocation

Applications should normally bind to port `0` and let the operating system allocate a free port. A stable port pool is used only when a workflow requires predictable endpoints.

## Approvals

Approval is a workflow state, not a UI modal only.

```rust
pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub run_id: RunId,
    pub action: ProposedAction,
    pub risk: RiskAssessment,
    pub requested_capabilities: Vec<Capability>,
    pub expires_at: Option<DateTime<Utc>>,
}
```

Approvals may be:

- once;
- for the remaining run;
- for a command pattern;
- for a repository;
- denied;
- delegated to organization policy.

## Cancellation

Every model call, tool invocation, workflow node, and child process receives a cancellation token. Cancellation should:

1. stop new work;
2. signal active operations;
3. terminate child processes after a grace period;
4. persist partial artifacts;
5. mark unresolved external effects for reconciliation.

## Checkpointing

The framework checkpoint abstraction can capture workflow execution. Codypendent adds a durable implementation backed by SQLite plus artifact snapshots.

A checkpoint contains:

- graph signature;
- workflow node states;
- shared state;
- active approvals;
- artifact references;
- worktree revision;
- model policy version;
- context/compaction version.

A resume must reject incompatible graph signatures unless a migration is supplied.

## Workflow definition

```yaml
name: repair-github-check
version: 1

inputs:
  pull_request:
    type: github_pull_request

steps:
  - id: inspect
    skill: github.inspect_failed_check
    agent: investigator

  - id: patch
    depends_on: [inspect]
    skill: code.repair
    agent: implementer
    approval: before_write

  - id: verify
    depends_on: [patch]
    tool: repository.test
    retry:
      attempts: 2

  - id: review
    depends_on: [verify]
    agent: reviewer

  - id: publish
    depends_on: [review]
    tool: github.update_pull_request
    approval: always
```

## Workflow-as-agent

A reusable workflow may be exposed as an agent capability through `agent-framework-rs`'s workflow agent support. Codypendent should preserve the distinction in traces: the outer agent delegated to a versioned workflow rather than a mysterious nested model.

## Modes, specifications, and change sets

```rust
pub enum AgentMode {
    Ask, Explore, Spec, Plan, Build, Review, Verify, Operate, Autopilot, Fleet,
}
```

Modes select prompt policy, default tools, approval profile, write boundary and completion behaviour.

A `TaskSpec` supplies requirements and acceptance criteria. A `Plan` compiles them into workflow nodes. A `ChangeSet` links patches to the plan node that justified them and to verification evidence.

Autopilot continues only while the approved plan, capability grant, budget, risk class and verification gates remain valid. Fleet mode allocates isolated tasks and worktrees and must justify duplicated context cost.
