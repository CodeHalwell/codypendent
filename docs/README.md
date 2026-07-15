# Codypendent

> **The local-first agentic developer environment with attachment issues.**

Codypendent is a persistent, Rust-native developer workbench that coordinates coding agents, tools, skills, workflows, documentation, memory, code intelligence, GitHub, and IDE clients from a single local daemon.

The core product rule is simple:

> **Clients may disappear. Agent runs must not.**

A Ratatui TUI, command-line client, VS Code/Cursor extension, Zed ACP client, JetBrains plugin, headless automation, and future web client all attach to the same persistent backend. The daemon owns execution and durable state; clients own presentation.

## What this documentation contains

This repository is a design and implementation manual for Codypendent. It includes:

- product principles and architectural invariants;
- daemon/client protocol and recovery semantics;
- agent runtime and multi-agent workflow design;
- semantic tool and skill retrieval;
- memory, search, code graph, and knowledge-fabric design;
- collaborative documentation;
- model routing, local models, and compaction;
- GitHub and IDE integration;
- plugin security and policy governance;
- `agent-framework-rs` integration;
- data contracts, implementation phases, and testing strategy;
- example manifests for skills, plugins, workflows, and policy.

Start with [the documentation index](docs/00-index.md).

Two consolidated entry points:

- [The Codypendent Story](docs/21-the-codypendent-story.md) — every document in this repository unified into one coherent narrative.
- [End-to-End Build Guide](docs/build/00-how-to-use-this-guide.md) — verbose, step-ordered implementation plans (Phase 0–7) written so an implementation agent with no prior context can build the system, with compile-verified code for Phase 0 and explicit schemas, rules, tests, and exit checklists for every later phase.

## Current architectural position

Codypendent should **not** build a new agent framework from scratch.

The published [`agent-framework`](https://crates.io/crates/agent-framework) Rust workspace already provides core agent abstractions, provider adapters, tools, skills, sessions, middleware, compaction, graph workflows, checkpointing, human-in-the-loop controls, MCP, A2A, hosting, memory adapters, observability, and several multi-agent orchestration patterns.

Codypendent adds the product and operating-system layer around that framework:

- persistent daemon lifecycle;
- durable event ledger and crash recovery;
- worktree-isolated coding environments;
- interactive TUI and IDE projections;
- semantic tool/skill registry;
- scoped knowledge fabric and code graph;
- model policy and cost-aware routing;
- plugin governance and sandboxing;
- collaborative docs;
- trace-based evaluation and controlled learning.

## Suggested repository shape

```text
codypendent/
├── Cargo.toml
├── crates/
│   ├── protocol/
│   ├── daemon/
│   ├── runtime/
│   ├── knowledge/
│   ├── integrations/
│   ├── sandbox/
│   ├── tui/
│   ├── cli/
│   └── test-support/
├── extensions/
│   ├── vscode/
│   ├── jetbrains/
│   └── zed/
├── docs/
├── specs/
├── migrations/
├── tests/
└── .github/workflows/
```

The initial workspace deliberately avoids turning every subsystem into its own crate. Modules should become crates only when they require a security boundary, separate distribution, optional heavy dependencies, or independent versioning.

## Documentation status

- Product name: **Codypendent**
- Document version: **0.3**
- Status: **Architecture and implementation draft**
- Date: **15 July 2026**


## Newly incorporated design patterns

Version 0.2 added explicit modes, durable specifications, session branching and steering, cumulative change sets, first-class hooks and commands, chronicles, JSONL operation, mid-session model switching, remote-attach/runner architecture, browser verification and repository-map strategies. Version 0.3 consolidates the suite into [The Codypendent Story](docs/21-the-codypendent-story.md) and adds the [End-to-End Build Guide](docs/build/00-how-to-use-this-guide.md).

See [Competitive Design Synthesis](docs/19-competitive-design-synthesis.md) and [Interaction and Autonomy Model](docs/20-interaction-and-autonomy-model.md).
