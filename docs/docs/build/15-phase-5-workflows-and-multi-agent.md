# Phase 5 — Workflows and Multi-Agent Orchestration

> **Objective:** declarative, durable, resumable workflows; supervised specialist delegation over a typed blackboard; parallel worktrees; node-level budgets; and an independent review agent — with the single-agent baseline always selectable.
>
> **Specification chapters:** [Roadmap Phase 5](../15-roadmap.md), [Agent Runtime and Workflows](../04-agent-runtime-and-workflows.md), [`agent-framework-rs` Integration](../12-agent-framework-rs-integration.md), [Interaction and Autonomy Model](../20-interaction-and-autonomy-model.md). Example manifests: [`specs/workflow.yaml`](../../specs/workflow.yaml), [`specs/agent.toml`](../../specs/agent.toml).
>
> **Exit criteria (from the roadmap):** multi-agent edits do not share writable worktrees; a workflow resumes after daemon restart; node-level cost and provenance are visible; the single-agent baseline remains selectable.

## STEP 5.1 — Declarative workflow definitions

1. Parse workflow YAML with **exactly** the shape of [`specs/workflow.yaml`](../../specs/workflow.yaml): `schema_version`, `id`, `version`, `inputs` (typed, required flags), `budget` (cost/duration/agents), `steps[]` with `id`, `depends_on`, `agent{role, model_policy}` or `tool`, `skill`, `workspace{mode}`, `approval` (`before-write` | `always` | none), `retry{attempts, backoff_seconds}`, `outputs`.
2. Agent profiles from [`specs/agent.toml`](../../specs/agent.toml): role, mode, autonomy, model_policy, skills, tools, `[permissions]`, `[budget]`, `[completion].requires`.
3. Compile: definition → validation (DAG check, unknown tool/skill/agent references are errors, budget must cover step estimates) → a framework workflow graph + daemon metadata (durable node records) per [Chapter 12](../12-agent-framework-rs-integration.md). Store compiled workflows in the registry (versioned; changing YAML without a version bump is an error).
4. Sources: `.codypendent/workflows/*.yaml` (repository scope) and user config dir. Replace the Phase 3 hard-coded `/fix-ci` flow with the declarative `repair-github-check` definition — behaviour must not change (its e2e test is the regression harness).

**TESTS** — parse/validate errors (cycle, unknown ref, missing input); compile round-trip; `/fix-ci` e2e still green on the declarative engine.

**COMMIT** `"phase5: declarative workflow compiler over framework graphs"`

## STEP 5.2 — Durable execution: node records and checkpoints

Migration 0005 (all four tables in this one migration — migrations are append-only, and STEP 5.3 needs the last one): `workflow_runs` (id, workflow id+version, run_id, inputs_json, state), `workflow_nodes` (workflow_run_id, node_id, state, agent_run_id, attempt, cost_json, started/ended), `checkpoints` (id, workflow_run_id, graph_signature, state artifact id, created_at), and `blackboard_items` (id, workflow_run_id, kind, payload_json, author_json, confidence, evidence_json, revision, superseded_by, created_at — consumed in STEP 5.3).

1. Implement `SqliteCheckpointStorage` for the framework checkpoint interface ([Chapter 12](../12-agent-framework-rs-integration.md)): checkpoint = graph signature + node states + shared state + active approvals + artifact refs + worktree revision + model policy version + context/compaction version ([Chapter 04](../04-agent-runtime-and-workflows.md)). Database writes and daemon workflow state share a transaction (or the outbox pattern) — no torn checkpoints.
2. Checkpoint at: node completion, approval waits, and pause. **Resume rejects a changed graph signature** unless a migration is registered ([Chapter 04](../04-agent-runtime-and-workflows.md)).
3. Startup recovery (extends Phase 1 STEP 1.14): live workflow runs re-load their latest checkpoint and continue from the first incomplete node; approval waits re-park (exit criterion 2).
4. Node lifecycle events (`WorkflowNodeTransitioned{node, state, cost}`) join the ledger; retry policy per definition with attempt counting.
5. Controls: pause / resume / retry-from-node / cancel-node-without-cancelling-workflow ([Chapter 20](../20-interaction-and-autonomy-model.md) steering list), exposed as commands + TUI workflow graph view (nodes with state, cost, agent, worktree — node-level cost and provenance is exit criterion 3).

**TESTS** — kill -9 between two nodes → resume completes remaining nodes exactly once (idempotency keys prove no duplicate effects); changed-signature rejection; retry-from-node reruns only downstream work.

**COMMIT** `"phase5: durable checkpoints, workflow recovery, node controls"`

## STEP 5.3 — The blackboard

`runtime/blackboard.rs` per [Chapter 04](../04-agent-runtime-and-workflows.md): typed artifacts only — `Finding`, `Hypothesis`, `Decision`, `CodeLocation`, `ProposedPatch(ArtifactRef)`, `TestResult`, `DocumentDraft`, `OpenQuestion` — each carrying author (agent/run/task), confidence, evidence refs, scope, revision, and supersession status. Stored in the `blackboard_items` table created by migration 0005 in STEP 5.2, scoped to a workflow run; agents read/write it through two registry tools (`blackboard.post`, `blackboard.query`) so every access is traced.

**RULE:** agents in a multi-agent workflow communicate **only** via blackboard artifacts and declared node outputs — never by exchanging raw transcripts ([Chapter 04](../04-agent-runtime-and-workflows.md) task model).

**TESTS** — supersession chains; evidence-required enforcement; cross-workflow isolation.

## STEP 5.4 — Delegation patterns (Levels 2 and 3)

1. **Level 2 — supervisor/specialists:** implement the investigator → implementer → independent reviewer pattern using framework orchestration builders; each specialist gets its own agent profile ([`specs/agent.toml`](../../specs/agent.toml)), its own run row, its own budget slice, and — when it writes — **its own worktree** (exit criterion 1: worktree leases already enforce one writer; add the multi-agent test). The reviewer's profile must exclude write tools (independence is structural, not prompted).
2. **Level 3 — parallel map/reduce:** N parallel read-only inspection nodes over module lists → aggregation node folding blackboard findings → conclusion node. Concurrency capped by the workflow's `maximum_agents` budget.
3. **Level 4 (competitive swarm) is explicitly out of scope** — do not build it (non-goal until justified by evaluation; the ladder is documented in the workflow docs).
4. Router justification rule (ADR-008): a workflow using Level ≥2 must declare `orchestration_reason` in its YAML (`parallelism` | `independent-review` | `access-separation` | `specialist`); missing reason = validation error. The single-agent Level 1 path from Phase 1 remains the default for `StartRun` without a workflow (exit criterion 4).

**TESTS** — two implementer nodes running concurrently hold distinct worktrees (assert paths differ and leases both active); reviewer cannot write (policy denial); map/reduce over a 3-module fixture aggregates 3 finding sets.

**COMMIT** `"phase5: supervisor delegation, map/reduce, blackboard-mediated agents"`

## STEP 5.5 — Budget enforcement

Budgets nest per [Chapter 09](../09-model-routing-and-compaction.md): session → workflow → agent → task node, over currency, tokens, wall time, model calls, tool calls, concurrent agents. Implement as a hierarchical ledger in the daemon: every model/tool call debits the whole chain atomically; crossing 80% emits `BudgetWarning`; exceeding any dimension transitions the node to `Blocked{budget}` and pauses the workflow for a human decision (raise budget / cancel) — **never** silent overrun.

**TESTS** — nested exhaustion (node cap hits before workflow cap); warning at 80%; blocked workflow resumable after budget raise.

## STEP 5.6 — Fleet-adjacent polish (overlay)

From the Phase 3–4 overlay items that belong to workflows: **session forking UI** — fork a session at a checkpoint (`ForkSession{checkpoint, name}` command from [Chapter 03](../03-daemon-client-protocol.md)): forks share immutable prior artifacts, get independent plans/worktrees/budgets ([Chapter 20](../20-interaction-and-autonomy-model.md)); TUI shows the fork tree and a comparison view (chronicle + changeset diff side by side). Full Fleet mode (automatic decomposition) stays out until Phase 7 evaluation exists.

**TESTS** — fork isolation (mutating fork B's worktree/plan leaves A untouched; both reference the same pre-fork artifacts).

## Exit checklist

- [ ] Concurrent writing agents never share a worktree (test green) (exit criterion 1).
- [ ] `kill -9` mid-workflow, restart, resume: completes with no duplicated external effect (exit criterion 2).
- [ ] TUI workflow view shows per-node state, cost, agent, worktree, and evidence links (exit criterion 3).
- [ ] Plain `StartRun` still executes the Phase 1 single-agent loop; Level ≥2 requires a declared orchestration reason (exit criterion 4).
- [ ] `/fix-ci` runs on the declarative engine with unchanged behaviour.
- [ ] Budget exhaustion blocks visibly and is resumable; 80% warnings emitted.
- [ ] Session fork produces isolated branches sharing pre-fork artifacts.
- [ ] `fmt` / `clippy` / `test` green; commits made; tree clean.
