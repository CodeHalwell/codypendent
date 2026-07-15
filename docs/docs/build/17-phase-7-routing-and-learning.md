# Phase 7 — Intelligent Routing and Learning

> **Objective:** close the loop: task-aware cost/quality model routing with cascading escalation; a local-model benchmark harness; trace graders; and the evaluation-gated promotion pipeline (shadow → canary → approve → rollback) for everything that learns.
>
> **Specification chapters:** [Roadmap Phase 7](../15-roadmap.md), [Models, Routing, and Compaction](../09-model-routing-and-compaction.md), [Observability, Evaluation, and Learning](../13-observability-evaluation-learning.md), [Testing Strategy](../16-testing-strategy.md).
>
> **Exit criteria (from the roadmap):** routing meets the quality threshold at lower cost than static strongest-model routing; no learned artifact self-promotes; the regression suite covers historical failures; all promoted versions are attributable and reversible.

## STEP 7.1 — The benchmark task set (build the yardstick first)

Per the roadmap's "first benchmark task set": create 50–100 repository tasks in `evals/tasks/` — each an `EvalCase` exactly per [Chapter 16](../16-testing-strategy.md) (`repository_revision`, `prompt`, `policy`, `expected: Vec<Assertion>`, `maximum_cost`, `maximum_duration`), over pinned fixture repositories (vendored small crates + this repository at fixed revisions). Task classes from the roadmap: failing-test diagnosis, small bug fix, regression-test addition, architecture explanation, doc update, PR-feedback response, CI diagnosis, safe refactor. Assertions from the Chapter 16 list (tests pass, file changed/unchanged, symbol exists, command NOT executed, citation correct, no forbidden network, approval requested, patch scope limit).

Runner: `codypendent eval run --suite core [--policy P] --report out.json` executes cases headlessly (JSONL client), scores assertions, records cost/latency/escalations.

**TESTS** — the runner itself (fixture case passes/fails correctly); CI job running a 10-case smoke suite with the mock model.

**COMMIT** `"phase7: eval harness and core benchmark task set"`

## STEP 7.2 — Model capability and performance profiles

1. Migration 0006: `model_profiles` — declared `ModelCapabilities` ([Chapter 09](../09-model-routing-and-compaction.md)) + probe results + observed reliability, latency/cost distributions, failure patterns; `ModelExecutionProfile` (preferred tool count, edit protocol, context layout, reasoning budget, schema-repair policy).
2. **Local model benchmark harness:** `codypendent models bench <id>` measures the Chapter 09 local profile: tokens/sec, time-to-first-token, warm-up, memory, context limit, structured-output reliability, tool-call accuracy (scripted probes), plus a small coding-eval score (10 benchmark tasks). Results persist to the profile; routing reads **measured** numbers, never vibes.
3. Capability probes on first use of any configured model (streaming? tools? parallel tools? structured output?) with results cached per model+endpoint.

**TESTS** — probe against the mock server (which advertises/denies features per scenario); bench output shape; profile persistence.

## STEP 7.3 — The router

Implement the [Chapter 09](../09-model-routing-and-compaction.md) pipeline exactly, per **task node** (never per session):

```text
task node → data classification & policy (hard filter: eligible providers)
          → required capabilities (hard filter)
          → context/output size estimate (hard filter: fits?)
          → historical task-class performance (from eval + trace data)
          → cost & latency estimate
          → utility = predicted_success − λc·cost − λl·latency − λp·privacy − λf·failure
          → select cheapest model above the quality threshold
          → validate output (objective checks: schema, patch applies, tests)
          → escalate on objective failure (preserving artifacts and task state)
```

1. Task classifier: rule-based first (mode + node kind + input size → task class), with an optional tiny local-model classifier behind a flag; the classifier's version is recorded in traces.
2. Cascading per Chapter 09: escalation chains are declared in the model policy (`local-default → hosted-default → hosted-strong`); an escalation **does not restart the workflow** — it re-executes the failed node with preserved artifacts; each transition records old/new profile, reason, context transformation, cost impact ([Chapter 09](../09-model-routing-and-compaction.md) mid-session switching).
3. λ weights + quality threshold live in a versioned `RoutingPolicy` (registry item, `router/<name>/<version>`), selectable per scope. Budgets from Phase 5 stay authoritative — the router optimizes inside them.
4. **Routing evaluation** (exit criterion 1): `codypendent eval route --suite core` compares the [Chapter 16](../16-testing-strategy.md) five arms — static-strongest, static-cheap, router, router+escalation, local-first router — reporting task success, cost, latency, escalation rate, tool-call errors, unsafe proposals. The release gate asserts: router+escalation ≥ quality threshold at cost < static-strongest.

**TESTS** — hard filters (classified data never routes to an ineligible provider — security test); cheapest-above-threshold selection given synthetic profiles; escalation preserves artifacts; transition records complete.

**COMMIT** `"phase7: per-node utility router with cascading escalation and eval arms"`

## STEP 7.4 — Trace graders and failure clustering

1. Graders consume terminal-run traces and emit the [Chapter 13](../13-observability-evaluation-learning.md) objective signals (+patch applies … −policy violation) as structured `TraceGrade` rows; execution-grounded signals only — no model-vibes grading in the core set (an optional LLM rubric grader may exist but is marked subjective and never gates alone).
2. Failure clustering: group failed/negative-signal traces by (task class, failing signal, tool, error fingerprint) into `failure_clusters` with exemplar traces — the input queue for improvement.
3. Every historical failure that gets fixed adds its trace to the **regression suite** (`evals/regressions/`): re-run nightly/CI (exit criterion 3 grows over time).
4. OpenTelemetry: bridge framework spans + daemon events to an optional OTLP exporter (config-gated, off by default; local-first).

**TESTS** — grader unit tests per signal; clustering determinism; a fixed failure lands in the regression suite and passes.

## STEP 7.5 — The promotion pipeline (nothing promotes itself)

Implement the [Chapter 13](../13-observability-evaluation-learning.md) loop as a first-class, auditable workflow for **all** learnable artifacts — retrieval weights (Phase 2 config), skill versions, prompt policies, routing policies, workflow versions, model execution profiles:

```text
candidate (draft, versioned, attributed)
→ offline regression suite (must not regress)
→ shadow run (execute alongside production choice; compare, don't affect)
→ limited canary (opt-in scope, budget-capped, auto-rollback on signal regression)
→ statistical + safety comparison report
→ HUMAN approval → promotion (version activated for scope)
→ rollback = normal operation (one command, previous version reactivates)
```

**RULES**

1. **No self-promotion** (ADR-010, exit criterion 2): the promotion transition requires an `Actor::Human` approval command; there is no code path from agent/grader to activation. Enforce in the state machine, not convention — write the test that tries.
2. Version identifiers (`skill/rust-ci/4`, `router/tool-selection/12`, `prompt/coding-agent/17`) appear in every trace that used them (exit criterion 4: attributable), and `codypendent versions rollback <id>` restores the prior version, also traced (reversible).
3. Skill synthesis from successful trace clusters ([Chapter 13](../13-observability-evaluation-learning.md)) creates **drafts** that additionally pass permission review before entering evaluation.
4. Privacy for eval exports ([Chapter 13](../13-observability-evaluation-learning.md)): secret scrubbing, repository-policy respect, user-deletion propagation, license classification, dataset lineage records. Confidential code never ships to external evaluators.

**TESTS** — self-promotion attempt fails structurally; canary auto-rollback on regression; rollback restores behaviour (eval suite proves it); deleted user data absent from a subsequent export.

**COMMIT** `"phase7: evaluation-gated promotion with shadow, canary, rollback"`

## STEP 7.6 — Chronicle quality and competitive baselines (overlay)

Score chronicles per [Chapter 13](../13-observability-evaluation-learning.md): can a fresh agent, given only the chronicle, identify the objective, reproduce decisions, locate changes, find verification evidence, resume unresolved work? Implement as an eval scenario set including the Chapter 13 benchmark list (plan-to-build transition, reconnect, forked approaches, selective apply, model switch, GitHub check repair). Add redacted chronicle export (`codypendent chronicle export --redact`) for sharing.

## Exit checklist

- [ ] `eval route` report: router+escalation meets the quality threshold at lower cost than static-strongest (exit criterion 1) — attach the report to the release notes.
- [ ] Self-promotion structurally impossible (test green) (exit criterion 2).
- [ ] Regression suite contains every fixed historical failure and runs in CI (exit criterion 3).
- [ ] Every promoted version is in traces and rollback restores its predecessor (exit criterion 4).
- [ ] Local model bench produces measured profiles that the router consumes.
- [ ] Classified-data routing hard filter verified by security test.
- [ ] Chronicle-quality scenarios pass at the agreed threshold.
- [ ] `fmt` / `clippy` / `test` green; commits made; tree clean.

**When this checklist is green, proceed to the [Master Acceptance Checklist](99-master-acceptance-checklist.md) for the release gate.**
