# Observability, Evaluation, and Learning

## Trace model

Every run captures:

- user request;
- task decomposition;
- model/provider/profile;
- selected memories, tools, and skills;
- context package manifest;
- model input hash;
- tool calls and outputs;
- approvals;
- commands and exit codes;
- patches and diffs;
- tests and diagnostics;
- cost and latency;
- user corrections;
- final disposition.

Large payloads are artifacts. Trace events reference them.

## OpenTelemetry

`agent-framework-rs` already emits tracing spans following GenAI conventions and optionally metrics. Codypendent should bridge these into:

- local trace inspection;
- OTLP exporter;
- structured run timeline;
- evaluation dataset;
- security audit.

## Event types

```rust
pub enum TraceEvent {
    RunStarted,
    ContextBuilt(ContextManifest),
    ModelRequested(ModelRequestMetadata),
    ModelStreamed(ModelStreamMetadata),
    ToolProposed(ToolProposal),
    ApprovalResolved(ApprovalResolution),
    ToolCompleted(ToolOutcome),
    ArtifactCreated(ArtifactRef),
    WorkflowTransition(WorkflowTransition),
    VerificationCompleted(VerificationResult),
    RunCompleted(RunOutcome),
}
```

## Objective signals

Prefer execution-grounded signals:

```text
+ patch applies
+ compilation succeeds
+ targeted tests pass
+ full suite passes
+ lint/format passes
+ regression test added
+ user accepts patch
+ PR remains unreverted
- invalid tool call
- command failure
- regression
- unnecessary edits
- excessive cost
- fabricated dependency
- policy violation
```

## Evaluation layers

### Unit-level

- tool schema compliance;
- routing eligibility;
- compaction invariants;
- policy decisions;
- memory extraction.

### Task-level

- repository issue completion;
- test repair;
- review quality;
- documentation update;
- skill selection.

### Workflow-level

- correct node ordering;
- recovery;
- human-input pauses;
- worktree isolation;
- budget compliance.

### Product-level

- user acceptance;
- time saved;
- cost;
- interruption/recovery success;
- false approval burden;
- trust.

## Self-improvement loop

```text
production traces
    ↓
failure clustering
    ↓
candidate prompt/skill/router/workflow change
    ↓
offline regression suite
    ↓
shadow run
    ↓
limited canary
    ↓
statistical and safety comparison
    ↓
approved promotion
```

## What may learn

### Tool retrieval

Weights for dense, lexical, graph, history, latency, and cost signals.

### Skill selection

Task-conditioned success estimates.

### Model routing

Which models work best for planning, patching, review, summarization, and vision.

### Prompt policy

Versioned instructions and examples.

### Memory consolidation

Repeated validated observations may become procedural or semantic memory.

### Skill synthesis

Successful trace clusters may propose new skills. Proposed skills begin as drafts and must pass permission review and evaluation.

## Model profile

```rust
pub struct ModelProfile {
    pub model: ModelId,
    pub task_policies: HashMap<TaskClass, PolicyVersion>,
    pub preferred_tool_count: usize,
    pub preferred_context_layout: ContextLayout,
    pub known_failures: Vec<FailurePattern>,
    pub evaluation: EvaluationSummary,
}
```

## Versioning

Version identifiers must appear in traces:

```text
skill/rust-ci/4
router/tool-selection/12
prompt/coding-agent/17
workflow/repair-check/3
model-profile/local-qwen/9
```

Rollback is a normal operation.

## Privacy

Training/evaluation exports must:

- remove secrets;
- respect repository policy;
- preserve user deletion;
- classify source licenses;
- avoid moving confidential code to external evaluators;
- record dataset lineage.

## Chronicle quality and competitive baselines

Evaluate whether a chronicle lets a fresh human or agent identify the objective, reproduce decisions, locate changes, understand rejected alternatives, find verification evidence and resume unresolved work.

Benchmark scenarios include safe plan-to-build transition, reconnect, forked approaches, selective apply, remote observation, model switch, GitHub check repair and browser evidence capture.
