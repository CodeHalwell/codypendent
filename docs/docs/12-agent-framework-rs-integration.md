# `agent-framework-rs` Integration

## Current package

As of 14 July 2026, the published umbrella crate is `agent-framework` version `0.1.1`, requiring Rust 1.82 or newer.

The workspace includes core runtime and optional crates for:

- OpenAI;
- Anthropic;
- Ollama;
- Gemini;
- Mistral;
- Foundry Local;
- Bedrock;
- GitHub Copilot;
- Azure OpenAI;
- MCP;
- A2A;
- declarative agents/workflows;
- HTTP hosting;
- Redis;
- Mem0;
- Foundry;
- Azure AI Search;
- Cosmos DB;
- Copilot Studio;
- Purview.

The core crate exposes agents, clients, tools, sessions, history, context providers, middleware, observability, skills, compaction, workflows, checkpointing, HITL, and orchestration builders.

## Ownership boundary

### Reuse directly

Codypendent should reuse:

- `ChatClient` and provider implementations;
- `Agent` / agent run abstractions;
- automatic function invocation;
- tool definitions and executors;
- middleware pipelines;
- `AgentSession`;
- `ContextProvider`;
- `Skill` and `SkillsProvider` concepts;
- compaction primitives;
- workflow builders and executors;
- checkpoint interfaces;
- HITL request/response;
- MCP and A2A integrations;
- tracing and OpenTelemetry conventions.

### Extend

Codypendent should extend:

- registry-backed semantic tool/skill selection;
- scoped and versioned skills;
- skill tests, scripts, provenance, and trust;
- durable SQLite checkpointing;
- event-ledger integration;
- worktree-aware workflow execution;
- model cost routing;
- structured artifact references;
- code graph and knowledge retrieval;
- product approvals and capability broker;
- session synchronization across clients.

### Keep outside the framework

The following are product/daemon concerns:

- TUI state;
- IDE clients;
- client protocol;
- daemon process discovery;
- Git worktree manager;
- GitHub App/webhooks;
- plugin installation;
- user scope and organization policy;
- local artifact store;
- UI themes;
- collaborative Docs Studio.

## Dependency strategy

Do not enable the umbrella `full` feature in the main binary by default. It increases compile time and attack surface.

Prefer selected dependencies:

```toml
[dependencies]
agent-framework-core = "0.1.1"
agent-framework-openai = { version = "0.1.1", optional = true }
agent-framework-anthropic = { version = "0.1.1", optional = true }
agent-framework-ollama = { version = "0.1.1", optional = true }
agent-framework-mcp = { version = "0.1.1", optional = true }
agent-framework-a2a = { version = "0.1.1", optional = true }

[features]
default = ["provider-openai"]
provider-openai = ["dep:agent-framework-openai"]
provider-anthropic = ["dep:agent-framework-anthropic"]
provider-ollama = ["dep:agent-framework-ollama"]
mcp = ["dep:agent-framework-mcp"]
a2a = ["dep:agent-framework-a2a"]
```

The Codypendent distribution may ship common providers, while less-used integrations are optional packages or plugins.

## Runtime adapter

```rust
pub struct FrameworkAgentRuntime {
    models: ModelRegistry,
    tools: CodypendentToolRegistry,
    contexts: ContextProviderFactory,
    checkpoints: Arc<dyn CheckpointStorage>,
    trace_sink: Arc<dyn TraceSink>,
}
```

A run:

1. resolves the selected model profile;
2. constructs the framework client;
3. creates context providers;
4. supplies selected tools and skills;
5. wraps middleware for policy and tracing;
6. runs an agent or workflow;
7. translates framework events into daemon events;
8. persists artifacts and state.

## Skills integration

The framework `SkillsProvider` uses an in-memory set and progressive disclosure.

Codypendent should implement a compatible provider:

```rust
pub struct CodypendentSkillsProvider {
    registry: Arc<dyn SkillRegistry>,
    retriever: Arc<dyn SkillRetriever>,
    scope: ScopeContext,
    policy: Arc<dyn PolicyEngine>,
    loaded: Arc<LoadedSkillSet>,
}
```

`before_run`:

1. retrieves semantically relevant skills;
2. filters by scope, trust, and permissions;
3. injects a compact catalog;
4. exposes load/read tools;
5. records selected candidates.

Future script execution must go through Codypendent's sandbox and capability broker.

## Tool integration

`FunctionTool` and `ToolDefinition` become the execution-compatible representation. Codypendent maintains richer registry metadata outside the framework and creates framework definitions only for selected tools.

Middleware enforces:

- policy;
- approvals;
- timeouts;
- rate limits;
- redaction;
- tracing;
- artifact conversion.

## Context providers

Implement providers for:

- session history;
- knowledge retrieval;
- repository map;
- active IDE context;
- selected documents;
- code graph;
- GitHub context;
- compacted episode state.

Provider order matters. History and retrieval run before compaction; final policy/redaction runs immediately before the provider request.

## Workflows

Use framework workflows for in-process graph execution and orchestration patterns. Codypendent adds:

- durable node records;
- queue and scheduling;
- worktree allocation;
- task budgets;
- recovery policy;
- external event correlation;
- client projections.

A Codypendent workflow definition compiles into a framework graph plus daemon metadata.

## Checkpoints

Implement:

```rust
pub struct SqliteCheckpointStorage {
    db: SqlitePool,
    artifacts: ArtifactStore,
}
```

The checkpoint should include graph signature and artifact references. Database writes and daemon workflow state must share a transaction boundary or a recoverable outbox pattern.

## Compaction

Use framework message compaction as the final request-shaping layer.

Codypendent's event/episode compaction happens earlier and produces structured context. The two layers solve different problems:

- daemon compaction: preserve durable reasoning state over long sessions;
- framework compaction: fit the final message list into a model budget.

## Provider coverage

The framework provider crates cover a strong initial set. Codypendent's “top 50 providers” goal should be met through:

1. native framework providers for important families;
2. OpenAI-compatible configuration;
3. optional gateway compatibility;
4. external agent-runtime adapters;
5. contribution of reusable provider crates upstream.

Do not create fifty bespoke implementations before usage justifies them.

## Contribution opportunities

Features that could improve both projects:

- registry-backed skill provider;
- sandboxed skill scripts;
- SQLite history and checkpoint providers;
- structured tool provenance;
- semantic tool selector;
- durable workflow event hooks;
- ACP adapter;
- artifact-aware compaction;
- richer model capability metadata;
- framework-to-daemon event bridge.

## References

- Crate: https://crates.io/crates/agent-framework
- Repository: https://github.com/CodeHalwell/agent-framework-rs
- Core source: https://github.com/CodeHalwell/agent-framework-rs/tree/main/crates/agent-framework-core
