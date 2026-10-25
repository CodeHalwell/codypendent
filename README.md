<img src="apps/desktop/src-tauri/icons/128x128.png" alt="Codypendent code brackets" width="80" height="80" />

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

**📍 [ROADMAP.md](ROADMAP.md) — build progress & phase tracker (start here to see what's done).**

See scaffold and delivery plan:

- [`ROADMAP.md`](ROADMAP.md) — at-a-glance status of every phase, with checkboxes
- [`docs/PROJECT_SCAFFOLD.md`](docs/PROJECT_SCAFFOLD.md)
- [`docs/TIMELINE.md`](docs/TIMELINE.md)

Consolidated documentation:

- [The Codypendent Story](docs/docs/21-the-codypendent-story.md) — all design docs unified into one coherent narrative.
- [End-to-End Build Guide](docs/docs/build/00-how-to-use-this-guide.md) — phase-by-phase implementation plans, written for an implementation agent, from empty directory to full product.

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

- CRDT-backed live collaboration (Loro — selected by measured benchmark, ADR-016)
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

- VS Code/Cursor: shared TypeScript extension base — **shipped**
  (`extensions/vscode`, built on `sdk/ui`)
- Zed: ACP-first — **works today** through `codypendent acp serve`, with no
  extension to install; a thin extension for the remaining ACP gaps is planned
- JetBrains: a Kotlin IntelliJ platform plugin is **planned, not built**. There
  is no JetBrains plugin in this repository in any form — verified 2026-08-13:
  zero `.kt` files and no `extensions/jetbrains/` directory
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

Voice supports push-to-talk, transcription against a configured hosted/local endpoint, optional TTS, and
preserving original audio artifacts. Every block type in the envelope (text, audio, image, file, editor
selection, code symbol, GitHub reference) contributes text the run can act on; image/file/editor-selection
blocks are named honestly rather than read, since this build has no OCR or image-understanding pipeline —
see `docs/reviews/2026-08-13-verticals/audio.md` for the current state of that gap.

## Fully Interactive TUI (Ratatui)

Real event model includes key, mouse, paste, resize, backend, IDE, voice, and tick events.
All mouse actions must have keyboard equivalents.

Run `codypendent` for the full TUI, then press `Enter` on the ready screen to
open the workspace. Press `F6` to enter mounted extension UI,
`Shift-F6` to move to the next extension document, and `Esc` to return. For a
cooked, line-oriented screen-reader or plain-terminal client with no alternate
screen or mouse escapes, run `codypendent --accessible` (alias: `--plain`). See
the [CLI & TUI user guide](docs/cli-and-tui-user-guide.md)
for line commands and the complete key reference.

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

## External ACP Agents

Codypendent discovers the official Agent Client Protocol registry automatically,
plus known native ACP clients already installed locally (currently Kimi Code),
and can run any compatible agent as a first-class session runtime. Claude Code,
Codex, Gemini, Kimi Code, Amp, Mistral Vibe, OpenCode, and the rest of the live
catalog appear in the TUI provider picker.

```bash
codypendent acp list
codypendent acp connect claude-code
codypendent acp connect codex
codypendent acp connect kimi-code
codypendent acp connect amp
codypendent acp connect vibe-chat
```

Registry packages are version-pinned; binary downloads are checksum-verified
when the registry supplies a digest. ACP tool permissions still use
Codypendent's durable approval UI, cancellation, and diff review. Build-mode
runs start in an isolated worktree; read-only modes use the selected repository
without granting Codypendent write authority. The connected agent is still a
trusted local vendor executable with the normal OS authority of your user
account; ACP permission requests are a cooperative protocol boundary, not an
OS sandbox.

### Agent councils

Persist a council from any configured mix of native model and ACP profiles,
give each member a role, and choose a separate chair. Members deliberate in
parallel in independent durable read-only sessions; optional later rounds
critique the prior dossier, then the chair produces an attributed synthesis
that preserves material dissent.

```bash
codypendent council create architecture \
  --member acp/claude-acp=architect \
  --member acp/codex-acp=adversarial-reviewer \
  --member acp/kimi-code=researcher \
  --chair acp/amp-acp \
  --rounds 2

codypendent council run architecture \
  --objective "Choose the safest migration plan for this repository"
```

Council membership is stored in private `councils.toml` configuration. Every
participant is pinned by its configured model-profile id, responses and the
chair run remain ordinary durable Codypendent sessions, and `--json` returns
the full member/session/run attribution for automation.

You can also create the definition without leaving the TUI: open `/`, choose
`/council`, then follow the seven-step builder to name the council, select each
configured model and its role, choose the chair and rounds, review, and save.
Unavailable profiles are rejected before save and the same typed validation and
private atomic persistence used by the CLI applies.

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
