# Models, Routing, and Compaction

## Provider architecture

Codypendent supports two backend families.

### Inference backends

Direct model interfaces:

- generation and streaming;
- tool calls;
- structured output;
- embeddings;
- multimodal input;
- reasoning controls;
- prompt caching.

### Agent-runtime backends

External coding-agent runtimes that already own an internal loop, such as Codex or Claude Code integrations.

These should not be forced into one leaky `complete(messages)` interface.

## Framework reuse

`agent-framework-rs` already provides provider crates for OpenAI, Anthropic, Ollama, Gemini, Mistral, Foundry Local, Bedrock, GitHub Copilot, Azure, Foundry, and related services. Codypendent should adopt selected crates behind product feature flags and add runtime adapters where a subscription-backed coding agent is used.

## Capability model

```rust
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tools: ToolCallSupport,
    pub parallel_tools: bool,
    pub structured_output: StructuredOutputSupport,
    pub vision: bool,
    pub audio_input: bool,
    pub embeddings: bool,
    pub prompt_caching: bool,
    pub reasoning_controls: bool,
    pub context_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}
```

Store:

- declared capabilities;
- probe results;
- successful observed capabilities;
- reliability;
- latency and cost distributions;
- known failure patterns.

## Routing pipeline

```text
task node
    ↓
data classification and policy
    ↓
required capabilities
    ↓
context/output size estimate
    ↓
historical task performance
    ↓
cost and latency estimate
    ↓
select cheapest model above quality threshold
    ↓
validate
    ↓
escalate when objective checks fail
```

## Utility

```text
utility =
    predicted task success
  - λcost × expected cost
  - λlatency × expected latency
  - λprivacy × privacy risk
  - λfailure × failure probability
```

Security and capability constraints are evaluated before utility.

## Cascading

Example:

```text
local classifier
→ local/cheap retrieval summarizer
→ capable coding model
→ local static critic
→ expensive verifier only on disagreement
```

Escalation preserves artifacts and task state. It does not restart the entire workflow unless context corruption is detected.

## Budget hierarchy

```text
organization
└── user
    └── session
        └── workflow
            └── agent
                └── task node
```

Budgets include:

- currency;
- tokens;
- wall time;
- model calls;
- tool calls;
- concurrent agents.

## Local models

Supported transport forms:

```rust
pub enum ProviderTransport {
    Http(Url),
    UnixSocket(PathBuf),
    NamedPipe(String),
    ChildProcess(ProcessSpec),
    Embedded(EmbeddedRuntimeId),
}
```

Local model profiles store measured:

- tokens/second;
- time to first token;
- warm-up time;
- memory use;
- context limit;
- structured-output reliability;
- tool-call accuracy;
- coding evaluation score.

## Compaction layers

The current framework supplies message-level strategies such as truncation, sliding windows, token budgets, selective tool-result removal, and a compaction provider.

Codypendent adds structured, event-sourced compaction.

### Level 1: observation compaction

Large shell output becomes:

- command;
- exit code;
- important lines;
- artifact reference;
- hash.

### Level 2: episode compaction

```rust
pub struct EpisodeSummary {
    pub objective: String,
    pub findings: Vec<Finding>,
    pub decisions: Vec<Decision>,
    pub rejected_hypotheses: Vec<RejectedHypothesis>,
    pub affected_symbols: Vec<SymbolRef>,
    pub artifacts: Vec<ArtifactRef>,
    pub verification: Vec<VerificationResult>,
    pub source_events: RangeInclusive<u64>,
    pub summarizer: ModelProfileId,
}
```

### Level 3: session checkpoint

Contains:

- goal;
- constraints;
- accepted decisions;
- open questions;
- workflow state;
- worktree revision;
- pinned evidence;
- active artifacts;
- selected memory;
- model/router policy version.

## Rehydration

A summary is never the only path to the source. When a summary becomes relevant, the context builder can load:

- original event range;
- exact artifact;
- current symbol definition;
- relevant tests;
- associated decisions.

## Pinning

Items may be:

- exact pinned;
- semantically pinned;
- temporary;
- discardable;
- sensitive;
- prohibited from remote models.

## Context packaging

```rust
pub struct ContextPackage {
    pub task: TaskContext,
    pub repository_map: RepositoryMap,
    pub memories: Vec<MemoryCitation>,
    pub symbols: Vec<SymbolContext>,
    pub files: Vec<FileExcerpt>,
    pub skills: Vec<LoadedSkill>,
    pub tools: Vec<ToolDefinition>,
    pub diff: Option<DiffContext>,
    pub constraints: ExecutionConstraints,
}
```

Packing policies are model-specific and evaluated empirically.

## Mid-session switching and model-specific execution profiles

A session may switch models without losing its specification, plan, change set or event history. Routing transitions record the old/new profiles, reason, context transformation, estimated cost impact and validation result.

```rust
pub struct ModelExecutionProfile {
    pub preferred_tool_count: usize,
    pub edit_protocol: EditProtocol,
    pub context_layout: ContextLayout,
    pub reasoning_budget: Option<ReasoningBudget>,
    pub schema_repair: SchemaRepairPolicy,
}
```

Editing protocols are evaluation-derived: structured patch tools, whole-file edits or architect/implementer separation may suit different models.
