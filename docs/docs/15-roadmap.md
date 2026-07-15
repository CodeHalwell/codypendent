# Implementation Roadmap

The roadmap is organized around usable vertical slices, not subsystem completion in isolation.

## Phase 0 — Repository and framework alignment

### Deliverables

- Codypendent repository and Cargo workspace;
- pin `agent-framework-rs` dependencies;
- domain IDs and event contracts;
- SQLite migration setup;
- architectural tests and CI;
- `codypendentd` process discovery;
- minimal CLI health command.

### Exit criteria

```bash
codypendent daemon start
codypendent daemon status --json
codypendent daemon stop
```

Daemon restart preserves its instance database and can replay a fixture event log.

## Phase 1 — Persistent coding-agent slice

### User story

> Open a repository, ask an agent to diagnose a failing test, approve commands, inspect a patch, rerun tests, close the TUI, reconnect, and continue.

### Deliverables

- Ratatui client;
- local client protocol;
- sessions and runs;
- one hosted model provider;
- one local/OpenAI-compatible provider;
- file, search, shell, and Git tools;
- approval broker;
- artifact store;
- dedicated worktree;
- trace viewer;
- basic message compaction.

### Exit criteria

- client disconnect does not stop the run;
- duplicate command delivery does not duplicate an effect;
- daemon restart recovers or cleanly marks the run;
- patch is reviewable and attributable;
- worktree cleanup protects unmerged work.

## Phase 2 — Skills and knowledge

### Deliverables

- registry and scope hierarchy;
- skill package loader;
- Skill Studio;
- hybrid BM25/dense/exact retrieval;
- memory observer and curator;
- provenance UI;
- basic code symbol graph;
- framework-compatible registry skill provider.

### Exit criteria

- top-k selection beats full-tool injection on an evaluation set;
- skill permissions are visible;
- every retrieved memory opens its source;
- stale indexes rebuild from authority.

## Phase 3 — GitHub and IDE awareness

### Deliverables

- GitHub read and draft-PR workflows;
- GitHub App option;
- VS Code/Cursor extension;
- Zed ACP adapter;
- IDE active-file, selection, diagnostics, and dirty-buffer context;
- shared session handoff.

### Exit criteria

- same run is visible in TUI and IDE;
- unsaved-buffer provenance is displayed;
- PR action is idempotent and approval-gated;
- webhook delivery replay is safe.

## Phase 4 — Docs Studio and richer code intelligence

### Deliverables

- CRDT benchmark and selected implementation;
- collaborative documents;
- Git publication;
- document/code symbol links;
- Rust semantic index;
- Python and TypeScript adapters;
- documentation staleness workflows.

### Exit criteria

- concurrent edits merge;
- document snapshot is reproducible;
- symbol changes flag affected docs;
- graph edges expose evidence and revision.

## Phase 5 — Workflow and multi-agent orchestration

### Deliverables

- declarative workflows;
- durable framework checkpoint storage;
- supervisor/specialist delegation;
- blackboard;
- parallel worktrees;
- budgets;
- pause/resume/retry-from-node;
- independent review agent.

### Exit criteria

- multi-agent edits do not share writable worktrees;
- workflow resumes after daemon restart;
- node-level cost and provenance are visible;
- single-agent baseline remains selectable.

## Phase 6 — Plugin and multimodal ecosystem

### Deliverables

- MCP plugin manager;
- WASM component SDK;
- native process sandbox;
- plugin permission UI;
- voice input;
- image/screenshot input;
- themes and theme packs;
- agentic setup assistant.

### Exit criteria

- plugin cannot access an undeclared path;
- permission expansion on update requires approval;
- original audio/image artifacts remain linked;
- setup assistant proposes rather than silently changes sensitive configuration.

## Phase 7 — Intelligent routing and learning

### Deliverables

- task classifier;
- cost/quality model router;
- local model benchmark harness;
- route cascading;
- trace graders;
- skill/prompt experiments;
- shadow and canary promotion;
- rollback UI.

### Exit criteria

- routing meets quality threshold at lower cost than static strongest-model routing;
- no learned artifact self-promotes;
- regression suite covers historical failures;
- all promoted versions are attributable and reversible.

## MVP dependency boundary

The MVP should depend on the following framework capabilities:

- agent core;
- OpenAI-compatible client;
- tool execution;
- middleware;
- sessions/history;
- compaction;
- workflow/checkpoint interfaces.

MCP, A2A, multiple providers, advanced orchestration, and organization services can remain feature-gated.

## First benchmark task set

Create 50–100 repository tasks:

- identify failing test cause;
- implement small bug fix;
- add regression test;
- explain architecture;
- update documentation;
- respond to PR feedback;
- diagnose CI;
- perform safe refactor.

Each task should have objective checks and a known repository revision.

## Competitive-pattern delivery overlay

### Phase 1
Explore/Plan/Build/Review modes, status line, JSONL, chronicle, change-set review and safe-point steering.

### Phase 2
Task specifications, hooks, custom commands, instruction import, repository map inspection and traceable model switching.

### Phase 3–4
Session forking, browser verification, remote attach, related-repository federation and package hot reload.

### Phase 5–7
Fleet cost/benefit routing, runner broker, agentic hooks, redacted chronicle sharing and experimental desktop verification.
