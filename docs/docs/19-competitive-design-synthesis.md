# Competitive Design Synthesis

## Purpose

This chapter converts a July 2026 survey of twenty terminal-based coding agents into product decisions for Codypendent.

It is not a feature-parity promise and does not copy implementation details. It identifies recurring interaction patterns, evaluates why they are valuable, and translates them into Codypendent-native abstractions.

## The design formula

> **Pi's hackability + Codex's security posture + OpenCode's server/client separation + Claude's extension model + Qwen Code's persistent daemon and orchestration direction + Crush's terminal polish + Aider and Plandex's repository/change control + Junie's IDE semantics + GitHub Copilot's development-lifecycle integration.**

Codypendent's differentiator is placing these patterns over:

- a persistent local Rust daemon;
- an evidence-backed knowledge fabric;
- a governed semantic tool and skill engine;
- durable workflows and worktrees;
- cost/privacy-aware model routing;
- `agent-framework-rs` as a reusable runtime foundation.

## Pattern adoption matrix

| Pattern | Reference products | Codypendent decision |
|---|---|---|
| Persistent daemon with attachable clients | OpenCode, Kilo, Qwen Code, OpenHands, Pi | **Core.** The daemon is authoritative; TUI, CLI, IDE and remote clients are replaceable projections. |
| OS-enforced sandbox and explicit capabilities | Codex CLI, OpenHands | **Core.** Capability broker, network-deny defaults for untrusted work and OS-specific isolation. |
| Explore/plan/build separation | Claude Code, OpenCode, Aider, Cline | **Core.** Explicit modes with different tool and write policies. |
| Spec and acceptance-criteria workflow | Factory Droid, Kiro, Plandex | **Core.** `TaskSpec` is a first-class artifact that can gate implementation and completion. |
| Skills, plugins, hooks and MCP as separate primitives | Claude Code, Copilot CLI, Kiro, Auggie | **Core.** Separate registries and trust models; packages may bundle them. |
| Worktree-isolated parallel agents | Claude Code, Droid, Qwen Code | **Core for writes.** One writable worktree per independent task. |
| Repository map and graph-aware context | Aider, Plandex, Auggie | **Core.** Compact map plus code graph and semantic IDE evidence. |
| Session resume, fork, branch and replay | Amp, Pi, Droid, Copilot CLI | **Core.** Runs are event-sourced; sessions can be forked at a checkpoint. |
| Message steering during execution | Amp | **Core.** Users can queue instructions, interrupt at safe points, or redirect the active plan. |
| Cumulative change sandbox | Plandex | **Core adaptation.** A `ChangeSet` stages an ordered patch stack before applying or publishing it. |
| Session chronicle and evidence capture | Copilot CLI, Droid | **Core.** Every run produces a structured, inspectable chronicle. |
| Lifecycle hooks and agentic validators | Claude Code, Copilot CLI, Kiro, Auggie | **Core.** Deterministic, policy, prompt-evaluator and agent hooks are distinct hook types. |
| Model switching during a session | Crush, Kilo, Qwen Code | **Core.** State is provider-neutral; model changes create traceable routing transitions. |
| Machine-readable headless mode | Codex, OpenCode, Amp, Cline, Droid | **Core.** JSON/JSONL protocol client for scripts and CI. |
| Remote supervision of local work | Claude Code, Amp, Kilo, Junie | **Designed early, delivered later.** Local execution remains authoritative while authenticated remote clients attach. |
| GitHub issue-to-PR lifecycle | Copilot CLI, OpenCode, Claude Code | **Core integration.** Issues, checks, review feedback and draft PRs become workflow inputs/outputs. |
| Browser and visual verification | Cursor, Cline, Droid | **Planned.** Browser actions and screenshots become evidence-producing tools. |
| Related-repository context | Auggie | **Planned.** Explicit repository federation with policy and provenance, never accidental global context. |
| Remote persistent runners | Amp, Claude Code, cloud agents | **Planned.** A runner broker can target local, LAN or hosted execution while preserving daemon semantics. |
| Automatic subagents and fleets | Claude Code, Copilot CLI, Qwen Code | **Conditional.** Single-agent is default; fleet execution requires decomposition benefit and budget. |
| Public shareable sessions | OpenCode, Amp | **Optional.** Export redacted chronicles; do not make public sharing a core state mechanism. |
| Desktop computer use | Droid, Qwen Code | **Experimental.** High-risk capability with separate policy and evidence requirements. |

## Core product primitives derived from the survey

### Session
A durable workspace for related user intent, runs, documents, artifacts, and decisions.

### Run
One execution attempt within a session.

### Thread
The ordered conversational view of a session. A thread is a projection, not the only source of truth.

### Task specification
A reviewable statement of requirements, constraints, acceptance criteria, risk and expected evidence.

### Plan
A versioned task graph derived from the specification. Users may edit, approve or replace it.

### Change set
An ordered, reviewable patch stack separate from the developer's working tree until accepted.

### Chronicle
A structured narrative of what the system attempted, why, which evidence it used, what changed and how completion was verified.

### Hook
A typed lifecycle reaction that may validate, transform, observe or block an operation.

### Agent profile
A role, model policy, tools, skills, permissions, autonomy level and budget.

### Runner
A local or remote execution environment with declared capabilities and isolation.

## Deliberate improvements over the references

### One authority model
The daemon and runner relationship is explicit and records which process owns every external effect.

### Evidence-first completion
“Finished” is not merely an agent message. A task specification defines completion evidence such as tests, diagnostics, screenshots, diff constraints, GitHub checks, document review or user acceptance.

### Structured plans, not disposable prose
Plans are graph artifacts with versioning, dependencies, cost estimates, approval gates and status.

### Change sets plus worktrees
Worktrees isolate filesystem execution. Change sets isolate the proposed logical change.

### Semantic extensions with governance
The system retrieves relevant extensions without flooding context and excludes untrusted or over-privileged items before the model sees them.

### Compatibility import rather than ecosystem lock-in
Codypendent should discover common repository guidance and extension forms where practical:

- `AGENTS.md`;
- `CLAUDE.md`;
- Cursor rules;
- Agent Skills packages;
- MCP server configuration;
- selected command and hook manifests.

Imported content is normalized into Codypendent's scoped registry and keeps its source provenance.

## Adopt now, design now, defer

### Build in the first two releases
- persistent daemon and resumable sessions;
- Explore, Plan, Build and Review modes;
- task specifications and acceptance criteria;
- change sets;
- worktree isolation;
- chronicle;
- semantic tools and skills;
- approvals and autonomy tiers;
- JSONL headless client;
- repository map;
- GitHub failed-check-to-draft-PR workflow;
- status line, notifications and cost display.

### Preserve in architecture, implement after the core
- remote attach;
- runner broker;
- browser verification;
- session forking UI;
- related-repository federation;
- package hot reload;
- compatibility importers;
- fleet mode.

### Experimental or policy-restricted
- desktop computer use;
- fully automatic cloud delegation;
- nested subagent swarms;
- unattended external writes;
- public session sharing;
- autonomous creation and promotion of plugins.

## Product positioning

> **A persistent, local-first developer operating environment where humans and agents share tools, knowledge, plans, change sets and evidence across the terminal, IDE and GitHub.**
