# Vision and Architectural Invariants

## Product vision

Codypendent should feel like a developer environment rather than a chat client.

The user may begin in a terminal, open the same session in VS Code, inspect an agent-created worktree in JetBrains, approve a GitHub action from the TUI, and later reconnect after the clients have closed. The session, workflow, artifacts, approvals, and repository state remain owned by the daemon.

The product combines five surfaces:

1. **Developer workbench** — TUI, CLI, IDE extensions, and headless client.
2. **Agent runtime** — agents, tools, workflows, checkpoints, model calls, and approvals.
3. **Knowledge workspace** — memory, documents, code graph, search, and provenance.
4. **Integration platform** — GitHub, MCP, A2A, providers, local models, and plugins.
5. **Learning system** — traces, evaluations, model routing, skill improvement, and controlled promotion.

## Architectural invariants

### 1. Runs outlive clients

Closing the TUI or IDE MUST NOT terminate an active run. A run ends only through completion, cancellation, policy action, unrecoverable failure, or an explicit resource limit.

### 2. The daemon is the execution authority

Clients MUST NOT directly execute privileged tools on behalf of a session. They submit commands and render resulting events. This provides a single policy, audit, and recovery boundary.

### 3. Models do not own system state

Switching from a hosted model to a local model, or from one provider to another, MUST NOT discard memory, workflow state, tool state, or session identity.

### 4. Commands are idempotent

Every command that may cause an external effect MUST carry an idempotency key. Reconnection, retries, and crash recovery must not duplicate commits, pull requests, deployments, or destructive file operations.

### 5. Original evidence is immutable

A summary, memory, or graph edge MUST reference its source events or artifacts. Compaction and knowledge extraction may create derived representations, but must not destroy the original evidence required for rehydration or audit.

### 6. Derived indexes are rebuildable

Vector indexes, BM25 indexes, symbol indexes, and graph projections are derived state. The authoritative stores are Git, the transactional database, CRDT documents, and the content-addressed artifact store.

### 7. CRDTs are limited to concurrently editable content

CRDTs are appropriate for collaborative documents, comments, and shared editable canvases. They MUST NOT be the primary representation for workflow transitions, approvals, leases, model billing, or process ownership.

### 8. Permissions are capability-based

A tool invocation receives only the paths, commands, network destinations, credentials, and subprocess rights required for that invocation. Broad ambient authority is prohibited.

### 9. Learning is evaluation-gated

Agents may propose new prompts, skills, policies, routing weights, or workflows. They MUST NOT silently promote them into production. Every learned change is versioned, evaluated, attributable, and reversible.

### 10. Single-agent execution is the baseline

Multi-agent workflows introduce cost and coordination risk. A single agent with deterministic workflow nodes and strong verification is the default. Delegation or swarming is selected only when parallelism, independent review, access separation, or specialist capability justifies it.

### 11. Local-first does not mean local-only

The system works without a cloud control plane and stores authoritative personal state locally. Hosted providers and services remain optional integrations governed by data-classification policy.

### 12. Human control is visible

Approvals, permissions, budgets, selected models, active tools, and agent state should be visible in the TUI. Important automation must not be hidden behind an opaque “magic” layer.

## Product boundaries

Codypendent owns:

- daemon lifecycle and persistence;
- client synchronization;
- workspace and worktree management;
- tool, skill, plugin, and policy registries;
- knowledge indexing and retrieval;
- cost-aware model routing;
- docs and IDE collaboration;
- GitHub workflows;
- trace storage and evaluation.

`agent-framework-rs` provides the reusable in-process framework primitives described in the dedicated integration chapter.

## Non-goals for the first release

The first release does not need:

- a public plugin marketplace;
- organization-wide multi-tenancy;
- a dedicated graph database;
- every IDE;
- every model provider;
- autonomous self-modification;
- arbitrary native plugins without sandbox policy;
- default swarming for routine coding tasks.

A narrow, durable vertical slice is more valuable than a broad but unreliable feature checklist.

## Additional product invariants from comparative research

### 13. Plans and specifications are durable artifacts
Specifications, acceptance criteria, plan revisions and completion evidence survive model changes and reconnection.

### 14. Proposed changes are reviewable independently of execution state
Worktrees isolate execution; change sets isolate logical proposals.

### 15. Every meaningful run produces a chronicle
The chronicle links objectives, decisions, actions, changes, evidence, costs and unresolved issues.

### 16. Autonomy is bounded and legible
Modes correspond to visible policy presets. “Autonomous” never means unlimited ambient authority.
