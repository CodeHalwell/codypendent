# The Codypendent Story

> **One coherent narrative of the entire design.** This chapter consolidates every document in this repository — the manual chapters 01–20, the project scaffold, the timeline, the security policy, the contribution rules, the product notes, and the example manifests under `specs/` — into a single story. Nothing here overrides the detailed chapters; where you need precision, follow the reference links. Where this chapter reconciles two documents that grew apart, it says so explicitly.

---

## 1. The problem we are solving

Today's coding agents mostly live inside chat windows. Close the window and the work dies with it. Switch editors and the agent forgets you. Switch models and the session starts over. The agent's "memory" is a transcript, its "plan" is prose that scrolled away, its "changes" are whatever it happened to write into your working directory, and "done" means the model stopped talking.

Codypendent is the answer to that failure mode. It is a **local-first, Rust-native agentic developer environment** — pronounced *code-ee-pendent*, positioned with a wink as *"the agentic developer environment with attachment issues."* The joke is the architecture: the product is deliberately, structurally attached. Sessions attach to a persistent daemon. Clients attach and detach freely. Knowledge attaches to evidence. Changes attach to plans. Nothing important is ever owned by a window.

The core product rule, stated once and enforced everywhere:

> **Clients may disappear. Agent runs must not.**

## 2. The shape of the product

One persistent process, `codypendentd`, owns everything durable: sessions, runs, workflows, approvals, artifacts, memory, indexes, worktrees, budgets, and traces. Everything the user touches — the Ratatui TUI, the plain CLI, the VS Code/Cursor extension, the Zed ACP client, the JetBrains plugin, headless JSONL automation, and a future web/remote client — is a **disposable projection** that attaches to the daemon over a versioned protocol ([ADR-001](17-architecture-decisions.md)).

```text
┌─────────────────────────────────────────────────────────────────────┐
│                         User Experience Plane                       │
│  Ratatui TUI · VS Code/Cursor · Zed · JetBrains · CLI · Web         │
└─────────────────────────────────┬───────────────────────────────────┘
                                  │ Codypendent Protocol (+ ACP adapter)
┌─────────────────────────────────▼───────────────────────────────────┐
│                         Agent Backend Daemon                        │
│ Sessions · Workflows · Agents · Models · Context · Policy · Events  │
├──────────────┬──────────────┬──────────────┬────────────────────────┤
│ Skills       │ Documents    │ Code Graph   │ Memory / Knowledge     │
│ Registry     │ Workspace    │ Indexer      │ Fabric                 │
├──────────────┼──────────────┼──────────────┼────────────────────────┤
│ GitHub       │ Plugin Host  │ IDE Bridges  │ Model Router           │
├──────────────┴──────────────┴──────────────┴────────────────────────┤
│ Execution: Git worktrees · Shell · Files · Browser · MCP · Sandbox  │
└─────────────────────────────────────────────────────────────────────┘
```

The product combines five surfaces ([Vision](01-vision-and-invariants.md)):

1. **Developer workbench** — TUI, CLI, IDE extensions, headless client.
2. **Agent runtime** — agents, tools, workflows, checkpoints, model calls, approvals.
3. **Knowledge workspace** — memory, documents, code graph, search, provenance.
4. **Integration platform** — GitHub, MCP, A2A, model providers, local models, plugins.
5. **Learning system** — traces, evaluations, routing, skill improvement, controlled promotion.

And one deliberate build-vs-buy decision anchors the whole engineering effort ([ADR-005](17-architecture-decisions.md), [Chapter 12](12-agent-framework-rs-integration.md)): **Codypendent does not build a new agent framework.** The published [`agent-framework`](https://crates.io/crates/agent-framework) Rust workspace (v0.1.1, Rust 1.82+) already provides agents, chat clients, tools, sessions, middleware, skills, compaction, graph workflows, checkpointing, human-in-the-loop, MCP, A2A, providers, and observability. Codypendent is the **product and operating-system layer around that framework**: persistence, protocol, policy, knowledge, governance, and user experience.

## 3. The cast of characters

Every subsystem in the manual speaks the same domain language ([Core Data Contracts](14-core-data-contracts.md), [Glossary](18-glossary.md)). Meet the primitives once, here:

| Primitive | What it is | Where it lives |
|---|---|---|
| **Session** | A durable workspace of related intent, runs, documents, artifacts, decisions | Daemon + SQLite |
| **Run** | One execution attempt toward an objective within a session | Event ledger |
| **Task / TaskSpec** | Typed objective with constraints, acceptance criteria, and budget | First-class artifact |
| **Plan** | Versioned task graph compiled from a spec; user-editable, approval-gated | First-class artifact |
| **ChangeSet** | Ordered, reviewable patch stack, separate from execution state | First-class artifact |
| **Chronicle** | Structured narrative of what happened, why, with evidence and costs | Generated from ledger |
| **Event** | Immutable record of an accepted state change or observation | Append-only ledger |
| **Command** | A client's request to change state, carrying an idempotency key | Ledger (before effects) |
| **Artifact** | Content-addressed large content: logs, patches, images, outputs | CAS file store |
| **Tool** | Typed executable operation (`read_file(path, range) → excerpt`) | Registry |
| **Skill** | Versioned procedural package (instructions + tools + tests + refs) | Registry + Git |
| **Plugin** | Distributable extension (MCP server, WASM component, provider, theme…) | Registry + sandbox |
| **Hook** | Typed lifecycle reaction (observe/transform/validate/authorize/notify/agent-evaluate) | Registry |
| **Memory** | Scoped, evidence-backed durable observation | Fabric + indexes |
| **Worktree** | Isolated Git working copy owned by a writing run | Worktree manager |
| **Capability** | Narrow, invocation-scoped permission grant | Policy engine |
| **Mode** | Interaction/policy preset (Ask, Explore, Spec, Plan, Build, Review, Verify, Operate, Autopilot, Fleet) | Session runtime |
| **Scope** | System → Organization → User → Workspace → Repository → Branch → Session → Task | Everywhere |

An optional relationship-comedy vocabulary (Memory → *Baggage*, Permissions → *Boundaries*, Model router → *Couples Counsellor*, Compaction → *Processing*) is a **cosmetic TUI theme only**. Logs, APIs, and documentation always use the technical terms ([Glossary](18-glossary.md), [Terminology](../product/terminology.md)).

## 4. The sixteen rules we never break

The architectural invariants ([Chapter 01](01-vision-and-invariants.md)) are the spine of every design decision. Abbreviated:

1. **Runs outlive clients.** A run ends only by completion, cancellation, policy, unrecoverable failure, or resource limit.
2. **The daemon is the execution authority.** Clients submit commands and render events; they never execute privileged tools themselves.
3. **Models do not own system state.** Switching providers or models never discards memory, workflow state, or session identity.
4. **Commands are idempotent.** Reconnects, retries, and crash recovery must never duplicate commits, PRs, or destructive operations.
5. **Original evidence is immutable.** Summaries and graph edges reference source events/artifacts; compaction never destroys the original.
6. **Derived indexes are rebuildable.** Authority lives in Git, SQLite, CRDT documents, and the artifact store — never in an index.
7. **CRDTs only for concurrently editable content.** Never for workflow transitions, approvals, leases, or billing ([ADR-004](17-architecture-decisions.md)).
8. **Permissions are capability-based.** Each invocation receives only the paths, commands, network, and credentials it needs.
9. **Learning is evaluation-gated.** Agents propose; versioned evaluation and human approval promote ([ADR-010](17-architecture-decisions.md)).
10. **Single-agent execution is the baseline.** Multi-agent orchestration is an explicit, justified choice ([ADR-008](17-architecture-decisions.md)).
11. **Local-first does not mean local-only.** Hosted services are optional integrations governed by data classification.
12. **Human control is visible.** Approvals, budgets, models, tools, and agent state are always inspectable in the UI.
13. **Plans and specifications are durable artifacts** that survive model changes and reconnection ([ADR-011](17-architecture-decisions.md)).
14. **Proposed changes are reviewable independently of execution state.** Worktrees isolate execution; change sets isolate proposals ([ADR-014](17-architecture-decisions.md)).
15. **Every meaningful run produces a chronicle** linking objectives, decisions, actions, changes, evidence, costs, and open issues.
16. **Autonomy is bounded and legible.** Modes are visible policy presets; "autonomous" never means unlimited ambient authority ([ADR-013](17-architecture-decisions.md)).

## 5. A day in the life — the story that ties every subsystem together

*The following walkthrough is fictional but architecturally exact. Every step names the subsystem and chapter that makes it work.*

Dana, a Rust developer, gets a red ❌ on pull request #482: a GitHub Actions check is failing.

**Attach.** Dana runs `codypendent` in the repository. The CLI finds `codypendentd` through socket discovery and attaches; if the daemon weren't running, it would be started. A session already exists for this workspace — the daemon has been here before. The status line shows: mode `Ask`, model policy `coding-balanced`, cost so far today, no pending approvals ([Chapter 03](03-daemon-client-protocol.md), [Chapter 20](20-interaction-and-autonomy-model.md)).

**Command.** Dana types `/fix-ci`. That is a **custom command** — a named entry point defined in a manifest like [`specs/command.toml`](../specs/command.toml) — which resolves to the `repair-github-check` **workflow** ([`specs/workflow.yaml`](../specs/workflow.yaml)) with `pull_request=482` as input, starting in `Spec` mode with `supervised` autonomy.

**Specify.** In Spec mode the agent may only write spec documents. It reads the failed check via the GitHub integration ([Chapter 10](10-ide-github-and-inputs.md)) and drafts a `TaskSpec`: objective ("make check `test-linux` pass on PR #482"), constraints ("no dependency upgrades"), acceptance criteria ("check green; regression test added; no unrelated files touched"), budget ($5.00, 60 minutes). Dana edits one requirement and approves. The spec is now a **versioned artifact**, not chat prose ([Chapter 20](20-interaction-and-autonomy-model.md)).

**Plan.** The planner compiles the spec into a versioned plan graph: `inspect → patch → verify → review → publish`, with cost estimates per node and approval gates marked (`patch: before-write`, `publish: always`). Dana approves plan v1. Any later plan change will record its reason and trigger reapproval if risk or budget grows ([Chapter 04](04-agent-runtime-and-workflows.md)).

**Retrieve.** Before the first model call, the semantic registry retrieves candidate tools and skills — dense + BM25 + exact + graph + history, filtered by scope, trust, and policy, then reranked — and discloses only compact cards: 8 tools, plus the `rust.fix-ci` skill ([`specs/skill.toml`](../specs/skill.toml)). Security is a hard filter, not a ranking penalty ([Chapter 05](05-skills-tools-and-plugins.md)). The context compiler packs the request: repository map, two memories (*"this repo uses `cargo nextest`"* — with provenance), the failing log excerpt as an artifact reference, and the skill catalog ([Chapter 09](09-model-routing-and-compaction.md)).

**Build in isolation.** The `patch` node needs to write, so the worktree manager allocates a dedicated Git worktree with a write lease — the developer's working directory is never touched ([ADR-006](17-architecture-decisions.md)). The implementer agent proposes its first `workspace.apply_patch`; the approval broker turns that into a workflow state, not a UI modal: Dana approves "for the remaining run". A capability grant is minted for exactly that worktree, those commands (`cargo`, `git`, `rg`, `rustfmt`), and no network beyond `api.github.com:443` — the policy resolved from [`specs/policy.toml`](../specs/policy.toml)-style scoped rules where **deny always wins** ([Chapter 11](11-security-and-governance.md)).

**Hooks fire.** When the patch lands, the `rust.verify-after-patch` hook ([`specs/hook.toml`](../specs/hook.toml)) runs `cargo test --workspace` in the worktree, blocking on failure and attaching the output artifact to the change set. Hooks are typed registry items with their own permissions — never hidden shell snippets ([ADR-012](17-architecture-decisions.md)).

**Walk away.** Dana closes the laptop mid-run. Nothing happens to the run — invariant 1. Twenty minutes later Dana opens VS Code; the extension attaches to the same session as a contributor, replays the events it missed from its last sequence number (or receives a snapshot if too far behind), and shows the diff. The TUI, still attached from before, remains the controller. Session handoff is backend-owned ([Chapter 03](03-daemon-client-protocol.md), [Chapter 10](10-ide-github-and-inputs.md)).

**Steer.** Dana queues a steering message — "also update CHANGELOG.md" — applied at the next safe point rather than interrupting mid-tool. The plan records the delta. Mid-session the router also switches models for the summarization node to a cheap local model over Ollama; the routing transition records old profile, new profile, reason, and cost impact. No state is lost — invariant 3 ([Chapter 09](09-model-routing-and-compaction.md)).

**Review the change, not the worktree.** The run produces a **ChangeSet** ([`specs/changeset.yaml`](../specs/changeset.yaml)): an ordered patch stack, each patch linked to the plan node that justified it and to verification evidence. Dana inspects by file and by plan node, drops one cosmetic hunk (selective apply), and accepts the rest ([Chapter 20](20-interaction-and-autonomy-model.md)).

**Publish, exactly once.** The `publish` node is `approval: always`. Dana approves; the daemon follows the crash-consistent write path — validate → persist intent → commit → perform side effect → persist outcome → publish event — so a crash or a retried command can never push twice (invariant 4, [Chapter 03](03-daemon-client-protocol.md)). Commit, push, PR comment, and check summary go out through the GitHub integration; every remote write is visible in the approval and trace systems ([Chapter 10](10-ide-github-and-inputs.md)).

**Remember and learn.** The memory observer extracted candidates throughout: the curator deduplicates, scope-classifies (`repository` scope — never leaking across repos), attaches provenance, and stores *"CI failures in this repo are usually the MSRV job"* with the trace as evidence ([Chapter 06](06-memory-and-knowledge-fabric.md)). The code graph updates for the changed symbols; a runbook in the Docs Studio that references `{{ symbol:parser::unescape }}` is flagged stale, and a maintenance workflow proposes (never silently applies) a doc update ([Chapter 07](07-code-intelligence.md), [Chapter 08](08-docs-studio.md)).

**Account for it.** The run ends with a **Chronicle**: objective, spec, plan versions, findings, decisions, actions, changes, verification, costs, unresolved questions — attached to the PR and stored for retrieval and compaction. Later, the evaluation pipeline mines this trace among thousands: failure clustering → candidate skill improvement → offline regression → shadow → canary → **human-approved promotion**, all versioned and reversible — invariant 9 ([Chapter 13](13-observability-evaluation-learning.md)).

Every subsystem in the manual exists to make one of those paragraphs true.

## 6. How the machine is built

### 6.1 Four planes, one process (at first)

The daemon is logically four planes ([Chapter 02](02-system-architecture.md)):

- **Experience plane** (in the clients): renders state, submits commands, advertises `ClientCapabilities` so the daemon can project appropriately.
- **Control plane**: sessions, attachment, command validation, policy, scheduling, approvals, budgets, durable event ordering, recovery.
- **Execution plane**: model requests, shell, files, Git, GitHub, plugins, MCP, worktrees — every execution under a scoped capability grant, every execution traced.
- **Knowledge plane**: documents and CRDT state, code graph, memories, full-text and vector indexes, registry metadata, artifact references.

Physically, Phase 0 ships one daemon process plus child processes (language servers, MCP/native plugins, local model servers). Heavy indexing may start as daemon worker tasks and split out later.

### 6.2 Storage: boring on purpose

- **SQLite in WAL mode** is the authoritative local store ([ADR-003](17-architecture-decisions.md)): sessions, runs, events, commands, workflow nodes, approvals, registry, memories, graph edges, budgets. PostgreSQL can arrive later behind the same repository traits.
- **Content-addressed artifact store** (`~/.local/share/codypendent/artifacts/sha256/<prefix>/<hash>`) holds anything big: model outputs, shell logs, images, patches, snapshots.
- **Derived indexes are disposable**: ripgrep for immediate exact search, Tantivy for BM25, an embedded vector index (or Qdrant when measured need justifies it), and SQLite edge tables as the first graph projection. All rebuildable from authority — invariant 6.
- **Authority table** ([Chapter 02](02-system-architecture.md)): repository content → Git; workflow state → event ledger; collaborative drafts → CRDT; published docs → Git snapshots; large output → artifact store; plugin permission → policy engine. No derived layer may silently overwrite its authority.

### 6.3 The protocol: own the durable contract, adapt the rest

Codypendent defines its **own client protocol** ([ADR-002](17-architecture-decisions.md), [Chapter 03](03-daemon-client-protocol.md)): Unix socket / named pipe, length-prefixed JSON frames, envelopes with protocol version, correlation, session, and sequence. ACP is an adapter for compatible editors; MCP is an integration protocol for tools. Neither defines Codypendent's durable session, artifact, policy, and recovery model, so neither is allowed to constrain it.

The protocol's soul is **attach-and-resume**: commands (with idempotency keys and expected revisions) go in; ordered events come out; clients subscribe to projections (session summary, run trace, workflow graph, documents, budget) rather than drinking the raw firehose; a client that reconnects hours later resumes from its last sequence or receives a snapshot. Steering, session forking, model-policy switching, and budget tightening are session-control commands. A headless client is just another consumer of the same events, serialized as JSONL ([ADR-015](17-architecture-decisions.md)).

### 6.4 The runtime: framework inside, product outside

The boundary with `agent-framework-rs` is explicit ([Chapter 12](12-agent-framework-rs-integration.md)):

- **Reuse directly**: `ChatClient` + provider crates (OpenAI, Anthropic, Ollama, Gemini, Mistral, Bedrock, Azure…), `Agent`, tools and automatic function invocation, middleware, `AgentSession`, `ContextProvider`, `Skill`/`SkillsProvider`, compaction primitives, workflow builders, checkpoint interfaces, HITL, MCP, A2A, OpenTelemetry conventions.
- **Extend**: registry-backed semantic tool/skill selection, scoped and versioned skills with tests and trust, durable SQLite checkpointing, event-ledger integration, worktree-aware workflows, cost routing, artifact references, approvals and the capability broker, multi-client session sync.
- **Keep outside the framework**: TUI, IDE clients, client protocol, process discovery, worktree manager, GitHub App, plugin installation, scope policy, artifact store, themes, Docs Studio.
- **Never** enable the umbrella `full` feature in the main binary ([ADR-009](17-architecture-decisions.md)); select provider crates behind product feature flags.

Orchestration is a ladder the router climbs only when justified ([Chapter 04](04-agent-runtime-and-workflows.md)): Level 1, one agent walking a deterministic workflow (`Inspect → Plan → Modify → Test → Review → Present`) — the default; Level 2, supervisor with investigator/implementer/reviewer specialists; Level 3, parallel map/reduce over large codebases; Level 4, competitive swarm — expensive and opt-in. Multi-agent coordination happens through a typed **blackboard** of findings, hypotheses, decisions, and patches — never unrestricted transcript exchange. Every writing task gets its own worktree; approvals are workflow states; cancellation is graceful and total; checkpoints capture graph signature, node states, approvals, artifacts, worktree revision, and policy versions so a resume can refuse an incompatible graph.

### 6.5 Knowledge: memory with receipts

Memory is an always-on pipeline, not a tool the model may forget to call ([Chapter 06](06-memory-and-knowledge-fabric.md)): events → candidate extraction → secret/sensitivity filtering → scope classification → dedup and contradiction detection → provenance attachment → retention decision → ledger and indexes. Eight memory classes (working, episodic, semantic, procedural, preference, failure, artifact, code), each scoped along the same hierarchy as policy. Newer facts **supersede** rather than delete older ones, so queries are revision-aware. Every retrieved fact can be opened at its source: *statement, source, revision, observed date, scope, confidence*. Deletion is real: scope deletion, retention expiry, cryptographic erasure, index tombstones.

Code intelligence builds the same way from three evidence layers ([Chapter 07](07-code-intelligence.md)): syntax (Tree-sitter), semantics (LSP/SCIP/compiler — rust-analyzer first), and runtime observation (tests, traces). Every graph edge carries evidence and confidence (`syntax-inferred call 0.45` … `observed runtime call 1.00`). The durable graph keeps public symbols and structure; statement-level detail is generated on demand. Graphs are revision-aware, so you can ask "which docs reference symbols removed by this commit?" — which is exactly how the Docs Studio staleness engine works.

The **Docs Studio** ([Chapter 08](08-docs-studio.md)) is the human-facing half of knowledge: CRDT working documents (Loro is the candidate, gated on benchmarks against Automerge and Yrs) for live collaboration, Git as the reviewed publication snapshot, typed blocks including embedded symbols/workflows/skills, and six explicit AI collaboration modes (Ask, Suggest, Edit, Co-author, Review, Maintain — organization docs default to **Suggest**). Every generated sentence is attributable to a run, model, and policy version.

### 6.6 Models: routed, profiled, interchangeable

Two backend families ([Chapter 09](09-model-routing-and-compaction.md)): **inference backends** (direct model APIs — generation, tools, structured output, embeddings, vision) and **agent-runtime backends** (external coding agents like Codex or Claude Code that own their inner loop) — never forced into one leaky interface. Local models are first-class providers (Ollama, vLLM, llama.cpp, OpenAI-compatible LAN services, managed subprocesses) with **measured** profiles: tokens/sec, first-token latency, structured-output reliability, tool-call accuracy.

Routing happens **per task node, not per session**: hard constraints first (data classification → eligible providers; required capabilities), then utility scoring (predicted success − λ·cost − λ·latency − λ·privacy − λ·failure), then cascading escalation: cheapest viable model → validate → escalate only on objective failure. Budgets nest: organization → user → session → workflow → agent → task node.

Compaction is two-layered: the daemon's **event-sourced compaction** (Level 1: observation — big shell output becomes command + exit code + salient lines + artifact ref; Level 2: episode summaries with findings, decisions, rejected hypotheses; Level 3: session checkpoints) preserves durable reasoning state, while the framework's message compaction shapes the final request to fit a model budget. A summary is never the only path to the source — rehydration reloads original events, artifacts, and current symbol definitions on demand — invariant 5.

### 6.7 Security: capabilities, scopes, and zero trust in content

The threat model ([Chapter 11](11-security-and-governance.md), [SECURITY.md](../SECURITY.md)) assumes prompt injection in repositories, docs, and tool output; malicious skills and MCP servers; secret exfiltration attempts; path traversal; confused-deputy GitHub actions; replayed commands. The defenses are structural:

- Every proposed action flows through: schema validation → source/trust classification → policy evaluation → capability grant → approval if required → sandboxed execution → output sanitization → trace.
- Capabilities are narrow and invocation-scoped (`FileRead(PathScope)`, `CommandExecute(CommandScope)`, `NetworkConnect(NetworkScope)`, `GitPush(RemoteScope)`…).
- The scope hierarchy merges **preferences** downward but never weakens **security restrictions**; deny wins; temporary grants expire; decisions are logged.
- Secrets live in the OS keychain, are brokered to tools without entering model context, and data classification (Public → Secret) gates which providers may see what.
- Commands are structured (`program + args + cwd + env + timeout`), not shell strings, unless a user explicitly approves shell interpretation.
- Plugins: WASM components preferred (explicit imports, metered, no ambient access); native processes sandboxed with clean environment, pre-opened paths, network allowlists, and signed manifests. **MCP compatibility is not a trust guarantee.** Updates that broaden permissions require fresh approval.
- Retrieved content is labeled by origin and can never grant permissions; model output proposing privileged actions still passes policy and approval.

### 6.8 Observability and learning: the loop that closes

Every run captures a full trace ([Chapter 13](13-observability-evaluation-learning.md)) — context manifest, model requests, tool calls, approvals, patches, tests, costs, user corrections — with large payloads as artifacts. Preference goes to execution-grounded signals (patch applies, tests pass, PR unreverted) over vibes. Four evaluation layers (unit, task, workflow, product) feed the self-improvement loop: traces → failure clustering → candidate change → offline regression → shadow → canary → approved promotion. What may learn: retrieval weights, skill selection, model routing, prompt policy, memory consolidation, and skill synthesis — always as drafts, always versioned (`skill/rust-ci/4`, `router/tool-selection/12`), always reversible.

## 7. What we learned from everyone else

The [Competitive Design Synthesis](19-competitive-design-synthesis.md) distills a July 2026 survey of twenty terminal coding agents into one formula:

> Pi's hackability + Codex's security posture + OpenCode's server/client separation + Claude's extension model + Qwen Code's persistent daemon direction + Crush's terminal polish + Aider/Plandex's repository and change control + Junie's IDE semantics + Copilot's lifecycle integration.

Codypendent's differentiator is placing those recurring patterns **on top of** a persistent Rust daemon, an evidence-backed knowledge fabric, governed semantic tool/skill retrieval, durable workflows and worktrees, and cost/privacy-aware routing. Core adoptions: explicit modes, spec-and-acceptance workflows, worktree-isolated parallelism, session resume/fork/steer, cumulative change sets, chronicles, lifecycle hooks, mid-session model switching, JSONL headless mode, repository maps, and the GitHub issue-to-PR lifecycle. Designed-now-shipped-later: remote attach, runner broker, browser verification, repository federation. Deliberately experimental or restricted: desktop computer use, unattended external writes, public session sharing, autonomous plugin creation. Compatibility importers normalize `AGENTS.md`, `CLAUDE.md`, Cursor rules, Agent Skills, and MCP configs into the scoped registry with provenance — meet users where they are, without ecosystem lock-in.

The [Interaction and Autonomy Model](20-interaction-and-autonomy-model.md) turns those patterns into contract: ten modes as policy presets, five autonomy tiers (ReadOnly → Suggest → Supervised → BoundedAutopilot → Unattended), durable TaskSpecs and living plans, change-set operations (selective apply, reorder, rebase, revise), session branching from checkpoints, safe-point steering, chronicles, typed hooks, custom commands, packages, headless JSONL, remote attach that keeps execution local, and a self-guide agent that answers from the installed version's own docs.

## 8. Reconciling the documents

This repository grew in layers. Where documents disagree, this is the resolution (also encoded in the build guide):

1. **Crate layout.** [`PROJECT_SCAFFOLD.md`](../PROJECT_SCAFFOLD.md) sketches five crates (`codypendent-{cli,daemon,protocol,skills,fabric}`); the manual's [suggested shape](../README.md) lists nine directories (`protocol, daemon, runtime, knowledge, integrations, sandbox, tui, cli, test-support`). **Resolution:** the manual's target shape is authoritative as the *destination*, but crates are created only when a phase needs them (per [CONTRIBUTING](../CONTRIBUTING.md): "do not add a crate merely to mirror an architecture diagram"). Phase 0 creates `protocol`, `daemon`, `cli`, `test-support`; `runtime` and `tui` arrive in Phase 1; `knowledge` in Phase 2; `integrations` in Phase 3; `sandbox` in Phase 6. Package names follow the scaffold convention `codypendent-*`; binaries are `codypendent` and `codypendentd`.
2. **Timeline vs roadmap.** [`TIMELINE.md`](../TIMELINE.md) (weeks 0–12, phases 0–5) is the *calendar and go-to-market overlay* — naming, trademark, launch checklist — for the early engineering phases. [Chapter 15](15-roadmap.md) (phases 0–7) is the authoritative *engineering sequence*. The build guide follows Chapter 15 and folds the timeline's identity/launch tasks into its Phase 0 notes.
3. **Stub outlines.** `docs/architecture/*` and `docs/workflows/*` are early outlines superseded by manual chapters 03/11 and the `specs/` manifests; they remain as pointers.
4. **Normative language.** MUST/SHOULD/MAY per the [manual index](00-index.md). Candidate technologies (Loro, Qdrant, Wasmtime) are validated by explicit gates, not asserted.

## 9. How we get there — and how we know it works

The [roadmap](15-roadmap.md) is organized around **usable vertical slices**, each with strict exit criteria:

| Phase | Slice | Headline exit criterion |
|---|---|---|
| 0 | Workspace, daemon skeleton, event fixtures, CI | `daemon start/status --json/stop`; restart preserves the instance DB; fixture replay |
| 1 | Persistent coding-agent slice (TUI, protocol, tools, approvals, worktree, one hosted + one local model) | Client disconnect doesn't stop the run; duplicate commands cause no duplicate effects; restart recovers |
| 2 | Skills + knowledge (registry, retrieval, memory, provenance, basic code graph) | Top-k retrieval beats full-tool injection; every memory opens its source |
| 3 | GitHub + IDE awareness (draft-PR flow, VS Code, Zed/ACP, handoff) | Same run visible in TUI and IDE; PR actions idempotent and approval-gated |
| 4 | Docs Studio + richer code intelligence (CRDT benchmark, publication, staleness) | Concurrent edits merge; symbol changes flag affected docs |
| 5 | Workflows + multi-agent (declarative workflows, checkpoints, blackboard, parallel worktrees) | Workflow resumes after daemon restart; agents never share writable worktrees |
| 6 | Plugins + multimodal (MCP manager, WASM SDK, sandbox, voice, images, themes, setup agent) | Plugin cannot touch an undeclared path; permission expansion requires approval |
| 7 | Routing + learning (classifier, cost router, benchmarks, graders, shadow/canary, rollback) | Router beats static-strongest-model on cost at equal quality; nothing self-promotes |

The MVP depends only on: agent core, an OpenAI-compatible client, tool execution, middleware, sessions/history, compaction, and workflow/checkpoint interfaces — everything else stays feature-gated. A competitive-pattern overlay threads modes, status line, JSONL, chronicles, change sets, and steering into Phase 1–2 rather than leaving them for "later".

Verification is a first-class subsystem of its own ([Chapter 16](16-testing-strategy.md)): the unit/property/integration/end-to-end pyramid; a **recovery matrix** injecting crashes at nine points around every external effect; protocol fixture corpora replayed across versions; worktree and security regression suites (path traversal, symlink escape, injection, forged webhooks, replayed approvals, cross-repo memory leakage); retrieval and routing evaluation; TUI reducer/snapshot/equivalence tests; and release gates that include clean install/uninstall and backup/restore. A benchmark set of 50–100 real repository tasks with objective checks anchors quality from Phase 2 onward.

## 10. What Codypendent is not (yet)

No public plugin marketplace. No org-wide multi-tenancy. No dedicated graph database before measured need. Not every IDE, not every provider, no autonomous self-modification, no default swarming, no desktop computer-use outside explicit experimental policy. A narrow, durable vertical slice beats a broad, unreliable checklist — that sentence appears in the vision chapter and is the correct tiebreaker for every scoping argument.

## 11. Where to go next

- **Understand a subsystem in depth** → the numbered chapters ([index](00-index.md)).
- **Build the system** → the [End-to-End Build Guide](build/00-how-to-use-this-guide.md), written so that an implementation agent with no prior context can execute it phase by phase, with verified code for Phase 0 and explicit specifications, tests, and exit checklists for every phase after.
- **Check a term** → the [Glossary](18-glossary.md).
- **Contribute** → [CONTRIBUTING](../CONTRIBUTING.md) and the ADRs ([Chapter 17](17-architecture-decisions.md)).
