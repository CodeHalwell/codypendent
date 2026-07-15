# Interaction and Autonomy Model

## Why modes exist

A single unrestricted agent mode makes it hard to understand what the system may do. Codypendent uses modes as explicit policy and interaction presets, not merely alternative prompts.

## Standard modes

| Mode | Purpose | Default writes | Default commands | Typical output |
|---|---|---:|---:|---|
| **Ask** | explain, answer and retrieve | denied | denied | cited answer |
| **Explore** | investigate repository and systems | denied | read-only or safe probes | findings and repository map |
| **Spec** | convert intent into requirements and acceptance criteria | docs/spec only | denied | `TaskSpec` |
| **Plan** | produce an execution graph | plan only | safe probes | versioned plan |
| **Build** | implement approved work | worktree only | approval/policy controlled | change set |
| **Review** | inspect code or a change set | comments only | read-only verification | findings |
| **Verify** | execute tests, browser checks and evidence capture | generated evidence only | allowlisted | verification report |
| **Operate** | perform operational or cloud work | policy controlled | policy controlled | auditable operations |
| **Autopilot** | continue through approved plan gates | bounded | bounded | completed or blocked workflow |
| **Fleet** | parallel task decomposition | isolated worktrees | bounded per agent | aggregated result |

A user can override a preset, but the effective policy remains visible.

## Autonomy tiers

```rust
pub enum AutonomyTier {
    ReadOnly,
    Suggest,
    Supervised,
    BoundedAutopilot,
    Unattended,
}
```

- **ReadOnly:** no external effects.
- **Suggest:** may produce plans, commands and patches but does not execute them.
- **Supervised:** performs low-risk actions and asks according to policy.
- **BoundedAutopilot:** continues within an approved specification, capability grant, budget and worktree.
- **Unattended:** only for explicitly configured workflows in controlled runners.

## Task specification

```rust
pub struct TaskSpec {
    pub objective: String,
    pub requirements: Vec<Requirement>,
    pub exclusions: Vec<String>,
    pub constraints: Vec<Constraint>,
    pub acceptance: Vec<AcceptanceCriterion>,
    pub risk: RiskAssessment,
    pub expected_artifacts: Vec<ArtifactExpectation>,
    pub budget: TaskBudget,
}
```

## Living plan

```rust
pub struct Plan {
    pub id: PlanId,
    pub version: u32,
    pub specification: TaskSpecId,
    pub nodes: Vec<PlanNode>,
    pub estimated_cost: Option<Money>,
    pub estimated_duration: Option<Duration>,
    pub approval: PlanApprovalState,
}
```

A changed plan records the reason, risk and budget impact and whether reapproval is required.

## Change sets

```rust
pub struct ChangeSet {
    pub id: ChangeSetId,
    pub base_revision: GitRevision,
    pub worktree: WorkspaceLeaseId,
    pub patches: Vec<PatchRef>,
    pub conflicts: Vec<Conflict>,
    pub verification: Vec<VerificationResult>,
    pub status: ChangeSetStatus,
}
```

Operations:

- inspect whole set or by file, symbol or plan node;
- accept all or selected patches;
- reorder or split;
- discard;
- rebase;
- ask the agent to revise;
- commit or open a pull request.

## Session branching

A session may fork from a checkpoint:

```text
session/main
├── fork/use-database-migration
└── fork/use-runtime-adapter
```

Forks share immutable prior artifacts but receive independent plans, worktrees, model routes and budgets.

## Steering and interruption

Users can:

- append a message for the next safe point;
- interrupt the current model call;
- pause before the next tool;
- replace a plan node;
- tighten budget or permissions;
- switch model;
- cancel one agent without cancelling the workflow;
- request an immediate status explanation.

## Session chronicle

```rust
pub struct SessionChronicle {
    pub objective: String,
    pub specification: Option<TaskSpecRef>,
    pub plan_versions: Vec<PlanRef>,
    pub investigations: Vec<Finding>,
    pub decisions: Vec<Decision>,
    pub actions: Vec<ActionDigest>,
    pub changes: Vec<ChangeDigest>,
    pub verification: Vec<VerificationResult>,
    pub costs: UsageSummary,
    pub unresolved: Vec<OpenQuestion>,
}
```

Chronicles can be rendered, exported, attached to pull requests, used as compaction checkpoints, evaluated and redacted.

## Hooks

```rust
pub enum HookKind {
    Observe,
    Transform,
    Validate,
    Authorize,
    Notify,
    AgentEvaluate,
}
```

Lifecycle points include session creation, plan changes, tool execution, patch proposal, verification, blocking and completion.

Hook implementations may be commands, WASM components, HTTP requests, policy rules, prompt evaluators or bounded agent validators. An agent hook cannot grant its own capabilities.

## Commands and packages

Reusable commands are named entry points:

```text
/review-pr
/fix-ci
/explain-symbol
/update-docs
/start-feature-spec
```

A package may bundle skills, agents, commands, hooks, workflows, MCP configuration, themes, TUI components and policy templates.

## Headless operation

```bash
codypendent run --workflow repair-github-check --input pr=482 --jsonl
codypendent attach SESSION_ID --events jsonl
codypendent review --changeset CHANGESET_ID --format json
```

## Remote attach

Future remote control keeps execution local:

```text
authenticated remote client
        ↓ encrypted relay/direct channel
local codypendentd
        ↓
local repository, tools and model credentials
```

## Status line and notifications

Shared status projections include mode, run state, plan progress, model, context use, cost, worktree, approvals and active agents.

## Browser and visual verification

Browser tools produce evidence: DOM/accessibility state, console/network errors, screenshots, action traces and assertion outcomes.

## Self-guide agent

Codypendent should ship a guide agent that answers from the installed version's local documentation and configuration schema and cites the local docs revision.
