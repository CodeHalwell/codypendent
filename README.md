# Codypendent

Local-first, agentic developer environment with:

- a reusable backend daemon
- Ratatui TUI and additional clients (IDE/CLI/Web)
- collaborative knowledge/documents
- GitHub automation
- multimodal input (text/voice/image)
- governed plugin ecosystem

Pronounced: `code-ee-pendent`

Positioning: `The agentic developer environment with attachment issues.`

## Naming Baseline

Use `Codypendent` as the product name and avoid shortening executables to `cody` to reduce naming overlap.

- `codypendent` - CLI and TUI entrypoint
- `codypendent daemon` - daemon command
- `codypendentd` - daemon executable
- `.codypendent/` - user and repository configuration
- `Codypendent Protocol` - client/daemon protocol
- `Codypendent Skills` - skill packages
- `Codypendent Fabric` - knowledge system

See scaffold and delivery plan:

- [`docs/PROJECT_SCAFFOLD.md`](docs/PROJECT_SCAFFOLD.md)
- [`docs/TIMELINE.md`](docs/TIMELINE.md)

## Product Boundary

```text
┌─────────────────────────────────────────────────────────────────────┐
│                         User Experience Plane                       │
│  Ratatui TUI · VS Code/Cursor · Zed · JetBrains · CLI · Web        │
└─────────────────────────────────┬───────────────────────────────────┘
                                  │ ACP + custom protocol
┌─────────────────────────────────▼───────────────────────────────────┐
│                         Agent Backend Daemon                        │
│ Sessions · Workflows · Agents · Models · Context · Policy · Events │
├──────────────┬──────────────┬──────────────┬────────────────────────┤
│ Skills       │ Documents    │ Code Graph   │ Memory / Knowledge     │
│ Registry     │ Workspace    │ Indexer      │ Fabric                 │
├──────────────┼──────────────┼──────────────┼────────────────────────┤
│ GitHub       │ Plugin Host  │ IDE Bridges  │ Model Router           │
├──────────────┴──────────────┴──────────────┴────────────────────────┤
│ Execution: Git worktrees · Shell · Files · Browser · MCP · Sandbox │
└─────────────────────────────────────────────────────────────────────┘
```

The backend owns intelligence and session state; all frontends are clients.

## Skills Studio

Skills are first-class editable objects with metadata, tests, history, traces, and version promotion/deprecation.

Baseline package format:

```text
skills/fix-rust-ci/
├── SKILL.md
├── skill.toml
├── tools.toml
├── tests/
├── references/
├── scripts/
└── assets/
```

`skill.toml` captures scope, status, required/optional tools, permissions, and execution limits.

## Collaborative Docs Studio

- CRDT-backed live collaboration (Automerge-suitable)
- Git as reviewed/publication snapshot storage
- explicit AI editing modes: Ask, Suggest, Edit, Co-author, Review, Maintain
- AST-backed document model with typed blocks and authorship records
- graph-linked symbols/workflows/skills to detect stale docs and drive maintenance

## GitHub Integration

- local `gh` + Git credentials for personal use
- GitHub App path for org-scale checks/webhooks/automation
- PR review/reply/fix flows, draft PR creation, CI tracking, reruns, release notes, checks publishing
- default per-agent Git worktrees and orchestrator leases for concurrent writes

## IDE Awareness

Common bridge contract supports workspace state, open docs, selection, diagnostics, symbols, edits, reveal, and diff.

- VS Code/Cursor: shared TypeScript extension base
- Zed: ACP-first + thin extension for ACP gaps
- JetBrains: Kotlin IntelliJ platform plugin
- session handoff is backend-owned, not UI-owned

## Multi-Agent Orchestration

Supported patterns:

- supervisor/workers
- planner/executor/critic
- map/reduce
- review triangle
- optional competitive swarm

Runs use: task graph, blackboard artifacts, scoped memory, artifact registry, leases, budget ledger, cancellation, traces, checkpoints.

## Multimodal Input

Normalize all input into a shared envelope over blocks:

- text
- audio
- image
- file
- editor selection / symbol references
- GitHub references

Voice supports push-to-talk, streaming transcription, optional TTS, and preserving original audio artifacts.
Image ingestion preserves source image plus extracted/OCR interpretation.

## Fully Interactive TUI (Ratatui)

Real event model includes key, mouse, paste, resize, backend, IDE, voice, and tick events.
All mouse actions must have keyboard equivalents.

## Themes

Use semantic tokens (surface/text/status/syntax/diff/agent/focus/selection) rather than hard-coded widget colors.
Theme plugins must not receive execution permissions.

## Scope & Policy

Hierarchy:

`System → Organisation → User → Workspace → Repository → Branch → Session → Task`

Rules:

- lower scopes can override preferences
- higher-scope security restrictions cannot be weakened
- permission resolution favors deny, and temporary grants expire

## Plugin Ecosystem

Plugin classes include MCP servers, ACP agents, skills, workflow packs, provider adapters, IDE bridges, I/O plugins, indexers, themes, and TUI components.

Registry model:

- curated internal catalog
- signatures/checksums/sandboxing
- permission-aware updates requiring approval for added permissions

## Intelligent Model Routing

Route per task node (not per session) using hard constraints first, then utility scoring over quality/cost/latency/risk/failure.
Use cascading escalation: cheapest viable model → validate → escalate only as needed.

## Compaction

Event-sourced immutable ledger + structured active context.

Compaction levels:

1. Observation compaction
2. Episode compaction
3. Session compaction

Rehydrate from artifact links when compacted context becomes relevant.

## Local LLM Support

Treat local models as first-class providers (Ollama, vLLM, llama.cpp server, OpenAI-compatible local/LAN services, managed subprocess models) with measured profiles and policy-aware routing.

## Agentic Setup & Personalization

Setup agent proposes environment configuration from discovered tooling and hardware, but must never silently install executable plugins, broaden permissions, exfiltrate secrets, weaken privacy routing, or alter org policy.

## Workspace Direction

Target Rust workspace split:

- protocol/backend/runtime/event layers
- TUI/theme/command layers
- skills/docs/memory/knowledge layers
- code graph/indexing/language layers
- orchestration/workflow/blackboard layers
- model gateway/router/provider/runtime layers
- context/compaction/evaluation/learning layers
- GitHub/git-worktrees/MCP/ACP/plugin layers
- IDE bridges
- multimodal/audio/image layers
- scope/policy/permissions/secrets/sandbox layers
