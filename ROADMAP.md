# Codypendent — Build Roadmap & Progress Tracker

A single, scannable view of where the build is. Phases are **usable vertical
slices**, not isolated subsystems — each one ends with something you can run.

**Legend:** ✅ done & verified · 🟡 in progress · ⬜ not started

For the full narrative and exit criteria see
[`docs/docs/15-roadmap.md`](docs/docs/15-roadmap.md); for step-by-step build
plans see the [End-to-End Build Guide](docs/docs/build/00-how-to-use-this-guide.md);
the release gate is the
[Master Acceptance Checklist](docs/docs/build/99-master-acceptance-checklist.md).

---

## At a glance

| Phase | Slice | Status |
|------:|-------|:------:|
| **0** | Workspace bootstrap — daemon lifecycle, protocol, ledger, CI | ✅ |
| **1** | Persistent coding-agent slice — sessions/runs, tools, approvals, TUI, JSONL | ✅ |
| **2** | Skills & knowledge — registry, retrieval, memory, code graph | ✅ |
| **3** | GitHub & IDE awareness — PR flows, editor extensions, shared session | ✅ |
| **4** | Docs Studio & code intelligence — CRDT docs, semantic index | 🟡 |
| **5** | Workflows & multi-agent orchestration | 🟡 |
| **6** | Plugins & multimodal — MCP/WASM plugins, voice/image, themes | 🟡 |
| **7** | Intelligent routing & learning — model router, graders, canary | 🟡 |

> **You are here:** Phases 0–3 are complete, and Phase 4's engine is in place.
> The knowledge fabric now carries a Loro-backed collaborative document model
> (selected by a real benchmark, ADR-016) with lossless block round-trip,
> concurrent-merge convergence, per-mutation authorship, collaboration modes with
> suggest-by-default for org docs, deterministic Markdown publication, a semantic
> `LanguageAdapter` layer with LSP-edge supersession and revision-aware graph
> queries (callers/blast-radius/tests-covering/changed-between), and a
> documentation staleness engine (`/update-docs`). Two slices of
> **client-surface wiring** have now landed. First, a read-only TUI Docs view
> (tree / editor / review rail) and a code-graph edge inspector, fed by the CLI
> projection seam (`D` / `G`). Second, **live daemon CRDT transport**: the
> `MutateDocument` command now applies onto the authoritative Loro document
> through a `DocumentMutator` assembly seam (mode-gated by the document's scope,
> single-writer via edit-lease `require`), and the resulting `DocumentSync` fans
> out to `Subscription::Document` subscribers over a per-document `DocumentHub`.
> What remains for Phase 4 is executing publication through the approval-gated
> write path, spawning a live language server, and the client-side CRDT replica
> that consumes the sync stream — external-tool / client work tracked below. With those
> deferred, **Phase 5 is underway**: the `codypendent-workflow` crate compiles
> declarative `workflow.yaml` manifests into a validated node graph (5.1 compiler
> core), persists runs / node records / checkpoints with resume-guarding in a
> `WorkflowStore` (5.2 durable store), carries agent-profile (`agent.toml`)
> parsing, and holds a per-run `BlackboardStore` for the typed, evidence-gated
> artifact channel agents share (5.3) — the daemon-free foundation for durable
> multi-agent orchestration, now advancing on three fronts. A model-free
> **driver** (`WorkflowDriver` + a `NodeExecutor` seam) advances a run through the
> ready frontier — resumable, node-level provenance recorded — proven end-to-end
> with a fake executor. **Runs are now driven, recovered, and controlled through
> the daemon**: a `StartWorkflow` command compiles a manifest, creates a durable
> run (its manifest persisted for recovery, migration 0011), and the assembly's
> `WorkflowConductorHost` spawns the driver to advance it to a terminal state
> under a per-run lock; a startup pass resumes every incomplete run after a crash;
> and `Controller`-gated `PauseWorkflow`/`ResumeWorkflow`/`RetryWorkflowNode`
> commands (reachable from `codypendent workflow run/pause/resume/retry`) drive the
> conductor's cooperative-pause / resume / retry-from-node lifecycle — the leaf
> per-node execution (the agent-loop bridge) being the one seam still stubbed. And
> two read-only **client surfaces** have landed — a TUI workflow-graph view over the
> compiled projection (per-node state / agent / worktree, grouped by workflow) and
> a TUI blackboard view over the per-run artifact boards (kind / author /
> confidence / evidence) — each fed by a CLI seam.
> In parallel, a **Codex-informed TUI backlog** is
> tracked near the end of this file: the conversation-centred shell, command
> palette (`/`), layout toggle (F2), auto-scroll follow, and contextual footer
> have all shipped.
>
> **Phases 6 and 7 have now begun as daemon-free engines.** Phase 6 landed the
> `codypendent-sandbox` crate — the plugin **security boundary**: `plugin.toml`
> parsing, sha256+ed25519 verification under a default-deny unsigned policy, the
> capability **permission-diff** that gates a widening update on re-approval, a
> **closed** `SandboxProfile` derived from the granted capabilities (the decision
> layer the OS/WASM sandbox will enforce), the install→verify→enable→update→revoke
> lifecycle, and untrusted-output sanitization — plus the Chapter 10 multimodal
> `InputEnvelope`/`InputBlock` model (original media never replaced by a summary;
> a remote-transcription classification gate) and six semantic-token theme
> variants with a data-only theme-pack loader that refuses execution permissions.
> Phase 7 landed the **router** (`codypendent-routing`): a version-stamped task
> classifier, the Chapter 09 pipeline with **security hard-filters before utility**
> (classified data never routes to a hosted provider), cheapest-above-threshold
> selection, cascading escalation that preserves artifacts, and the five
> eval-route arms + release gate — and the **learning loop** (`codypendent-eval`):
> the `EvalCase`/`Assertion` harness, execution-grounded trace graders,
> deterministic failure clustering, a growing regression suite, and the
> shadow→canary→**human-approval**→rollback promotion pipeline that structurally
> forbids self-promotion (only an `Actor::Human` can promote). What remains for
> both phases is the daemon wiring, the persisted migrations/profiles, the OS/WASM
> sandbox enforcement, and the live capture/measurement paths — the engines are in
> place and unit-tested.

---

## Phase 0 — Workspace bootstrap ✅

Daemon starts, persists an instance database, and replays a fixture event log.

- [x] Cargo workspace + pinned `agent-framework-rs` (0.3–0.8)
- [x] Domain IDs & event contracts; migration `0001_init` (0.4–0.5)
- [x] `codypendentd` daemon: db, instance, ledger, replay, socket server (0.6)
- [x] `codypendent` CLI: `daemon start` / `status --json` / `stop` (0.7)
- [x] Test support + fixture event log; integration tests (0.8–0.9)
- [x] CI (fmt, clippy, test); full verification & exit criteria (0.10–0.12)

**Exit:** `daemon start/status/stop` work; restart preserves `instance_id`,
increments `boot_count`; fixture log replays deterministically. ✅

## Phase 1 — Persistent coding-agent slice ✅

> *Open a repo, ask an agent to diagnose a failing test, approve commands,
> inspect a patch, rerun tests, close the TUI, reconnect, and continue.*

- [x] **1.1** Schema migration `0002` (runs, commands, effects, approvals, artifacts, leases)
- [x] **1.2** Protocol v1.1 (handshake, catchup, artifact refs, unknown-variant tolerance)
- [x] **1.3** Command handling — crash-consistent 6-step write path + idempotency
- [x] **1.4** Content-addressed artifact store (SHA-256 dedup)
- [x] **1.5** Policy engine & capabilities (path canonicalization, deny-wins)
- [x] **1.6** Approval broker (park in `WaitingForApproval`, durable, live-published)
- [x] **1.7** Tool layer (file, search, shell, git) with policy/approval middleware
- [x] **1.8** Worktree manager (allocation, stale-lease reconciliation, unmerged-work rescue)
- [x] **1.9** Model providers (hosted + OpenAI-compatible, behind features)
- [x] **1.10** The agent loop (`FrameworkAgentRuntime`, run-state machine, chronicle)
- [x] **1.11** Protocol server — attach, resume, subscriptions, heartbeat
- [x] **1.12** Ratatui TUI **+ interactive harness wired into `codypendent`**
- [x] **1.13** Headless JSONL client (`run --jsonl`, `attach --events jsonl`)
- [x] **1.14** Recovery & the failure matrix (kill-9 → run recovered/failed)
- [x] **Wiring** agent loop ↔ daemon via a `RunExecutor` seam (`codypendentd` assembly crate)

**Exit criteria**

- [x] Client disconnect does not stop the run (verified: TUI reconnect resumes the session)
- [x] Duplicate command delivery does not duplicate an effect (idempotency keys)
- [x] Daemon restart recovers or cleanly marks the run (kill-9 integration test)
- [x] A run started from the TUI reaches a terminal state (driven to a terminal `RunState`; the JSONL client asserts the terminal exit code in `crates/cli/tests/jsonl_it.rs`)
- [x] Patch is reviewable and attributable (change-set + artifact provenance)
- [x] Worktree cleanup protects unmerged work (safety patch before force-remove)
- [x] `Explore` mode cannot write; status line; JSONL/TUI observe the same events

**Follow-ups tracked into later phases (not blocking the slice):**

- [ ] Bind a dedicated per-run worktree in the executor (module exists; the loop
      currently runs in the repo root — full binding lands with Phase 5 parallel worktrees)
- [x] Catch-up `Snapshot` rendering in the TUI (folds a `Snapshot` into title +
      run stubs; test `catchup_snapshot_seeds_title_and_run_stubs`)
- [x] Surface `CommandRejected` in the TUI as a transient notice (reader
      forwards the rejection → status-line notice with ~5s expiry)

## Phase 2 — Skills & knowledge ✅

New `codypendent-knowledge` crate; migration `0003`; the mandatory index-outbox.

- [x] **2.1** Schema `0003` + crate foundation (registry/memory/code-graph/outbox tables, shared types)
- [x] **2.2** Scoped registry + `skill.toml` package loader (strict keys, content-hash change detection) + built-in tools + `rust.fix-ci` reference skill
- [x] **2.3** Hybrid retrieval (dense + BM25 + exact + history) with hard security filters, rerank, dependency closure, budget disclosure
- [x] **2.4** Memory observer + curator pipeline + provenance + SQL-level scoped retrieval + supersession
- [x] **2.5** Tree-sitter code graph (nodes/edges + evidence) + repository map v1
- [x] **2.6** Skill Studio + memory browser in the TUI (permissions verbatim, provenance card)
- [x] Daemon registers built-in tools on startup; `codypendent index rebuild`; run-lifecycle context manifest + memory-on-completion

**Exit criteria**

- [x] Retrieval eval: **recall@8 = 1.0** (≥ 0.8 gate), 100% unsafe-item exclusion, disclosed top-k (254 tok) fits a budget the full-injection baseline (4580 tok) blows through
- [x] `rust.fix-ci` loads, is retrieved for "the CI test is failing", and its permissions render verbatim in the Studio
- [x] Memory never leaks across repositories (SQL scope filter; leak test green)
- [x] `codypendent index rebuild` after deleting `<data_dir>/index/` restores identical results
- [x] Every retrieved memory opens its source (provenance card + open-source affordance)
- [x] Agent context includes repository map + retrieved cards + cited memories (emitted into the run trace); a run's events are curated into provenance-bearing memories
- [x] `fmt` / `clippy` / `test` green; commits made; tree clean

## Phase 3 — GitHub & IDE awareness ✅

New `codypendent-integrations` crate; protocol `ide` module + `ProposedAction::GitHubMutation` + `UpdateIdeContext`/`ClientPresenceChanged`; migrations `0005` (webhook delivery idempotency) and `0006` (IDE context); `extensions/vscode/`.

- [x] **3.1** GitHub personal-mode client — `GitHubApi` trait + `reqwest` client (get PR, check-runs, job logs, review comments, draft PR, update PR, check-run summary); opaque `GitHubToken` broker (`gh auth token`/`GITHUB_TOKEN`, redacted, never serialized); hidden-marker idempotency (list-before-create); `eval_github_mutation` policy gate (network-scoped to `api.github.com:443`, always approval-gated); wiremock tests
- [x] **3.2** GitHub in the agent loop + `/fix-ci` — five `github.*` tools wired into the runtime (get PR, list check-runs as network reads; create-draft-PR, update-PR, check-run-summary as approval-gated `GitHubMutation`s), the client injected from the personal-mode token at daemon startup, the policy admitting `api.github.com:443` only when configured, `/fix-ci` registered as a built-in `Command` (in the Skill Studio) with a hard-coded objective template. End-to-end tested: the /fix-ci sequence (read check → test → update PR → post summary) with each write parking for a durable approval before it happens; rejected/denied writes never call GitHub. *(The declarative workflow engine that replaces the prompt-encoded sequence is Phase 5.)*
- [x] **3.3** Webhook ingestion — `X-Hub-Signature-256` HMAC verify **before** parse; normalize → internal events; `X-GitHub-Delivery` GUID replay dedup (migration `0005`); optional loopback listener wired into `codypendentd` (default off); policy-off ⇒ no workflow trigger
- [x] **3.4** IDE bridge + source-provenance live-path — protocol `IdeContextUpdate`/`DirtyBufferDigest`/edit-request types + `SourceProvenance`; `UpdateIdeContext` command stored as a projection (migration `0006`); the run read path labels an excerpt whose disk bytes diverge from an unsaved editor buffer `unsaved-ide-buffer` in the trace; `IdeBridge` trait; deterministic debounce
- [x] **3.5** VS Code / Cursor extension — `extensions/vscode/` (TypeScript, esbuild): frame codec + discovery mirroring the Rust protocol, a `DaemonClient` attaching as `Approver` with reconnect-resume, a side-panel webview, approval notifications → `ResolveApproval`, debounced `IdeContextUpdate` push, `vscode.diff`; 30 vitest tests + typecheck + lint green; Cursor compat note
- [x] **3.6** Zed via ACP adapter — minimal ACP over stdio JSON-RPC (initialize/session·new/prompt/cancel + permission requests) decoupled behind an `AcpBackend`; `codypendent acp` CLI subcommand; round-trip + cancellation tests
- [x] **3.7** Session handoff + presence — `ClientPresenceChanged` event; the server publishes presence on attach/detach; `codypendent open <session> --in <ide>` hands a session to an editor as a contributor without restarting the run

**Exit:** same run visible in TUI + IDE; unsaved-buffer provenance shown; PR
actions idempotent + approval-gated; webhook replay safe.

**Verified:** GitHub writes are idempotent and approval-gated end-to-end through
the agent loop; the token never enters `Debug`/serialization/logs; a read of a
diverging unsaved buffer is labeled `unsaved-ide-buffer` in the trace; a replayed
webhook (same GUID) produces no second event and a forged signature is rejected
before parsing; a second client attaching emits a `ClientPresenceChanged` the
first observes; the ACP handshake/prompt/cancel round-trips over stdio; the VS
Code extension's codec/discovery/reconnect pass 30 vitest tests. `fmt` / `clippy
--all-features -D warnings` / `test --workspace` green; `extensions/vscode`
typecheck/lint/test green.

## Phase 4 — Docs Studio & richer code intelligence 🟡

Engine complete and tested in `codypendent-knowledge` + `codypendent-protocol`;
client-surface wiring is the remaining slice.

- [x] **4.1** CRDT benchmark (Loro vs Automerge vs Yrs, `benches/crdt-bench`) → **ADR-016 selects Loro**, with the measured report in `docs/docs/benchmarks/`
- [x] **4.2** Document model + storage (migration `0008`): `KnowledgeDocument`/`DocumentBlock`/authorship, a Loro CRDT layer (block↔CRDT bijection), lossless export/import, concurrent-merge convergence, per-mutation attribution, `DocumentChanged` outbox
- [x] **4.3** Collaboration modes (Ask/Suggest/Edit/Co-author/Review/Maintain) + **suggest-by-default for org docs**; suggestions apply exactly the annotated range on accept; protocol `DocumentMutation`/`DocumentSync`/`MutateDocument`/`Document` subscription
- [x] **4.4** Deterministic Markdown render (byte-identical) + `PublishPlan` (target/changed-files/git-action) + `(revision ↔ commit)` publication record
- [x] **4.5** `LanguageAdapter` trait + Rust/Python/TypeScript adapters (graceful syntax-only degradation), **LSP-edge supersession** + confidence tiers, revision-aware queries (`callers_of`/`blast_radius`/`tests_covering`/`changed_between`), hierarchical repository map with evidence
- [x] **4.6** Staleness engine: `{{ symbol:… }}` link resolution, signature-change/disappearance findings with evidence, Maintain-mode suggestions, `/update-docs` command

**Deferred to a client-wiring follow-up (not blocking the engine):**

- [x] TUI Docs view (tree / editor / review rail) and the graph-edge inspector — read-only render over the existing document + code-graph data, wired through the CLI projection seam and reached from the command palette (in the conversation shell the bare `D`/`G` keys compose text; they act only once a browser overlay is open); the inspector surfaces each edge's relation + confidence + evidence + revision (exit criterion 4). Live editing is the next bullet
- [x] Live daemon CRDT-sync transport for the `Document` subscription + block-range edit-lease enforcement — *engines:* (a) `apply_mutation` maps a protocol `DocumentMutation` onto the authoritative CRDT + suggestion store under the collaboration-mode gate (Edit applies directly; Suggest/Co-author/Maintain route to the review rail; Ask/Review deny; accept/reject resolve) and returns the `DocumentSync` (`Payload::DocumentSync` carries it on the wire); (b) `DocumentLeaseStore` (migration 0009) enforces **one writer per block-range** — a whole-document lease conflicts with any block lease both ways, leases expire and are reclaimed lazily, the same writer renews, and `require()` is the pre-mutation guard. *Transport (now wired):* `MutateDocument` is intercepted at the connection level (like `AttachSession`/`UpdateIdeContext`, since documents live outside the session ledger) and applied through a daemon `DocumentMutator` seam — implemented in the `codypendentd` assembly over `apply_mutation` (mode derived from the document's **scope** via a lightweight `DocumentStore::scope` read) with lease `require` enforced first; the resulting `DocumentSync` fans out to `Subscription::Document` subscribers over a per-document `DocumentHub` (idempotent CRDT merge ⇒ no watermark needed). *Lease-acquire (now wired):* `CommandBody::AcquireDocumentLease`/`ReleaseDocumentLease` are intercepted at the connection level like `MutateDocument` and applied through a daemon `DocumentLeaser` seam (bundled onto the `RunExecutor`, implemented in the assembly over the same `DocumentLeaseStore`), so a client takes a real block-range lease before editing and is recognised as that writer when its mutation runs `require`; the reply is a `Payload::DocumentLeaseGranted` carrying the minted lease id + expiry, an Observer is role-denied, and a conflicting holder is `document.range-leased`. *Remaining:* the client-side CRDT replica that consumes the sync stream
- [ ] Executing a `PublishPlan` through the approval-gated change set / Phase 3 GitHub write path
- [ ] Spawning a live language server (rust-analyzer/pyright) and folding its resolved edges (the adapter reports the capability; supersession is proven with synthesized edges)

**Exit:** concurrent edits merge ✅; document snapshot reproducible ✅; symbol
changes flag affected docs with evidence ✅; graph edges expose evidence +
revision ✅ (data model + read-only TUI inspector render). ADR-016 recorded ✅;
suggest-by-default enforced ✅; `fmt`/`clippy`/`test` green ✅.

## Phase 5 — Workflow & multi-agent orchestration 🟡

- [ ] Declarative workflows; durable checkpoint storage; supervisor/specialist delegation; blackboard
  - [x] **5.1 (compiler core)** `codypendent-workflow` crate: the declarative
        `workflow.yaml` model + a compiler that validates a definition (schema
        version, unique/non-empty step ids, exactly one action per step,
        skill⇒agent, resolvable `depends_on`, acyclic graph via topological sort,
        budget sanity, and the ADR-008 multi-agent `orchestration_reason` rule)
        and lowers it into a topologically ordered node graph. The canonical
        `repair-github-check` manifest compiles (regression test).
        **Registry cross-checks have landed:** a `WorkflowRegistry` lookup seam
        plus `compile_with_registry` / `CompiledWorkflow::validate_references`
        reject a step naming an unknown tool, an agent role with no profile, or a
        skill the registry does not know (structural validation runs first, so a
        malformed graph fails with its structural error before any name is looked
        up). The workflow crate stays daemon-free — the trait is the seam the
        daemon fills from the live registry + loaded agent profiles; `SetRegistry`
        is the in-memory implementation the tests use. The compiler also has a
        user-facing entry point now: `codypendent workflow validate <file>`
        parses + compiles a manifest and reports the validated graph (or the
        precise error, tagged with the file), so an author checks a manifest
        before it ever runs. **Role→profile resolution is now defined** — the gate
        the rest of 5.1 waited on. An `AgentProfileSet` loads a directory of
        `agent.toml` profiles and indexes them by the role each *fulfils*:
        `AgentProfile::fulfilled_role` is the profile's explicit `role` field, else
        the last dotted segment of its id — so the canonical `code.implementer`
        binds a manifest's short `role: implementer` — and the set refuses a
        directory where two profiles claim one role (a role resolves to exactly one
        profile). `codypendent workflow validate <file> --agents <dir>` uses it to
        cross-check that every agent step's role resolves, reporting each
        unresolved `step → role` before a run reaches it (the tool/skill half still
        needs the live registry). Agent-profile (`agent.toml`) parsing had already
        landed — `parse_agent_profile` reads
        role/mode/autonomy/model_policy/skills/tools/permissions/budget/completion.
        *Remaining for 5.1:* lowering the compiled graph onto framework
        orchestration builders, and replacing the hard-coded `/fix-ci` flow with
        the declarative `repair-github-check` definition.
  - [x] **5.2 (durable store)** migration 0010 + a `WorkflowStore` over SQLite:
        durable workflow runs, a per-node record (state / attempt / cost /
        start+end times — the node-level provenance the graph view needs), and
        checkpoints. `resume` reports the first incomplete node and **refuses a
        changed graph signature** (`CompiledWorkflow::signature()` hashes the
        graph shape). `retry_from_node` re-drives a chosen node and everything
        transitively downstream of it — resetting them to a clean `Pending`
        (attempt / timings / cost / agent-run id cleared) and the run to
        `Running`, under the same signature guard — so a `resume` then picks up
        from that node (the durable-store half of retry-from-node).
        `list_incomplete_runs` enumerates the non-terminal runs
        (pending/running/paused) a daemon must reconcile on startup, so recovery
        is a recompile-and-`resume` per run. `ready_nodes` (pure core
        `ready_node_ids`) returns the parallel scheduler's frontier — every
        `Pending` node whose dependencies are all `Completed` — the full set an
        executor may launch concurrently into isolated worktrees (Phase 5's
        parallel-worktrees criterion), where `resume` gives only the single next
        node. The compiled graph is now a serializable projection
        (`CompiledWorkflow: Serialize`, tagged node actions), surfaced by
        `codypendent workflow show <file> [--json]` — the read model a graph view
        renders. **The TUI workflow-graph view over that projection has now
        landed:** a read-only overlay (reached from the command palette, or the
        bare `W` once a browser is open — like `D`/`G` in the conversation shell)
        that lists a repository's compiled workflow nodes in topological order,
        grouped by workflow, and — for the focused node — renders its action,
        lifecycle state, agent, worktree, approval, retry, dependencies, and
        declared outputs (exit criterion 3's per-node state / agent / worktree).
        It is fed by a CLI seam that compiles `.codypendent/workflows/*.yaml`
        into self-contained `WorkflowNodeCard`s (the one place the workflow crate
        meets the pure TUI crate, mirroring the Docs/Edges wiring), skipping a
        manifest that does not compile rather than failing the view. State/cost
        are the pre-run values (`pending` / `—`); overlaying a durable run's live
        per-node state and cost lands with the daemon executor. **The engine loop
        over the store — the `WorkflowDriver` — has now landed:** it advances a
        run through the `ready_nodes` frontier, executing each node via a
        `NodeExecutor` seam and recording the transition (attempt / cost /
        agent-run id) through `transition_node`, until the run reaches a terminal
        `Completed`/`Failed`. It is **resumable** (a `Completed` node is never in
        the frontier; a node left `Running` by an interrupted drive is reset to
        `Pending` and re-driven exactly once) and **model-free** — the daemon
        fills `NodeExecutor` with the agent loop / tool layer, while the crate's
        tests fill it with a fake executor, so linear completion, failure blocking
        only its dependents, retry-to-success, resume-skips-completed, and a
        diamond frontier are all proven without a model call. A `NodeObserver`
        sees every transition (the seam the daemon fills to emit
        `WorkflowNodeTransitioned` events). **Runs are now creatable through the
        daemon:** a `StartWorkflow` command (carrying the manifest YAML + typed
        inputs) is intercepted at the connection level like `MutateDocument` and
        applied through a `WorkflowStarter` seam — implemented in the `codypendentd`
        assembly over `compile_yaml` + `WorkflowStore::create_run_idempotent` (keyed
        by the command's idempotency key, so a duplicate delivery resolves to the
        same run) on the daemon's pool — replying `WorkflowRunStarted` with the new
        run id (or
        `CommandRejected` when the manifest does not compile; a daemon without the
        seam rejects it `workflow.transport-unavailable`, an Observer is
        role-denied). **The daemon now drives, recovers, and controls those runs:**
        a `WorkflowConductor` (in `codypendent-workflow`) recompiles a run's stored
        **manifest** (persisted with the run by migration 0011) into its graph and
        advances it through the `WorkflowDriver`; the assembly's
        `WorkflowConductorHost` **spawns that drive fire-and-forget** right after
        `StartWorkflow` creates the run — so a created run actually advances — under
        a **per-run drive lock** so no two drives ever race one run. **Startup
        recovery** resumes every incomplete run from where it stopped
        (`recover_incomplete` over `list_incomplete_runs`; a `running` node
        interrupted by a crash is reset and re-driven exactly once; a **paused** run
        is left for an explicit resume). **Pause / resume / retry-from-node are real
        commands** — `PauseWorkflow` / `ResumeWorkflow` / `RetryWorkflowNode`,
        `Controller`-gated, intercepted like `StartWorkflow` and applied through a
        daemon `WorkflowLifecycle` seam over the conductor: pause flips the run so
        the driver stops **cooperatively** at the next scheduling boundary (drain
        then stop), while resume/retry mutate synchronously (so the reply is an
        accurate accept/reject) then drive in the background. All four are reachable
        from the CLI (`codypendent workflow run/pause/resume/retry`). A
        `NodeObserver` emits a node-lifecycle event per transition (surfaced in the
        daemon log today). *Remaining for 5.2:* the **leaf per-node execution** — the
        agent-loop bridge (an agent node → a real agent run) and tool-node execution
        (the manifest tool-name namespace reconciled with the runtime tool registry
        + the per-node tool arguments the compiled graph does not yet carry) — is the
        one seam still stubbed (`AgentLoopNodeExecutor` reports each node
        not-yet-executable, so a run driven today fails cleanly and legibly per
        node); everything *around* the leaf (create → drive → recover →
        pause/resume/retry, per-run serialization) is complete and tested with a
        completing fake executor. The client-facing `Subscription::Workflow` stream
        that publishes the observer's transitions (mirroring the document CRDT-sync
        stream) is the other remaining piece.
  - [x] **5.3 (blackboard)** the `BlackboardStore` (migration 0010's
        `blackboard_items` table): the typed, attributed artifact channel agents
        share *within* a workflow run — findings, hypotheses, decisions, code
        locations, proposed patches, test results, document drafts, open
        questions (Chapter 04's "communicate only via blackboard artifacts and
        declared outputs, never raw transcripts"). Claim-like kinds (finding /
        decision / test-result / proposed-patch / code-location) are **refused
        without evidence**; a corrected item **supersedes** rather than deletes
        (the chain is stamped in one transaction); boards are **isolated per
        run**. Payload/author/evidence ride as opaque JSON so the crate stays
        daemon-decoupled. The read surface a projection needs is in place:
        `query` (live or full board, kind-filtered), `get` (one item by id,
        run-scoped), and `history` (an artifact's full supersession lineage,
        oldest first). **The TUI blackboard view has now landed:** a read-only
        overlay (command palette, or the bare `B`) that lists the artifacts on the
        active runs' boards — grouped by run — and, for the focused artifact,
        renders its kind, author, confidence, evidence, revision, and a payload
        summary, dimming a superseded item. It is fed by a CLI seam that queries
        each incomplete run's board over the shared pool and renders the opaque
        JSON payload/author/evidence to human strings (empty until the executor
        posts artifacts). *Remaining for 5.3:* daemon read/write **commands** +
        subscription delivery over that surface (the write path an agent's
        `blackboard.post` tool drives).
- [ ] Parallel worktrees; budgets; independent review agent — **pause / resume /
      retry-from-node have landed** (conductor + `WorkflowLifecycle` commands +
      CLI); parallel worktrees, budget enforcement, and the independent review
      agent remain.

**Exit:** multi-agent edits never share writable worktrees; workflow resumes
after restart ✅ (startup recovery drives every incomplete run); node-level
cost/provenance visible ✅ (per-node records + observer); single-agent baseline
selectable.

## Phase 6 — Plugin & multimodal ecosystem 🟡

The security-decision engines have landed as daemon-free crates; the OS/WASM
*enforcement* that consumes their profiles, and the live client capture paths,
are the remaining wiring.

- [x] **6.1 (plugin manifests, verification, lifecycle, permission-diff)** — the
      new `codypendent-sandbox` crate (the manual's "crate justified by a
      security boundary"). It parses `plugin.toml` (the `docs/specs/plugin.toml`
      shape) with `deny_unknown_fields`; verifies the artifact by sha256 checksum
      and an ed25519 publisher signature over a canonical
      `codypendent-plugin-signature-v1` digest of the **whole manifest** (every
      field but the signature) — so a valid signature can't be replayed against
      any altered field (capabilities, runtime command, resource caps, scopes) —
      under a default-**deny** unsigned policy; models capabilities as a comparable
      `CapabilitySet` and computes the **permission diff** that blocks a
      capability-expanding update until re-approved while auto-applying an
      identical/narrowing one (exit criterion 2, rendered `+ network: host:443`);
      derives a **closed** `SandboxProfile` from the *granted* set (env allowlist,
      pre-opened paths, network allowlist, resource caps) so an executor honouring
      it cannot reach an undeclared path/host (exit criterion 1, the decision layer
      the OS/WASM sandbox enforces); drives the discover → verify →
      install-disabled → smoke-test → enable → update → revoke lifecycle as a
      guarded state machine carrying each plugin's trust record; and neutralizes
      untrusted plugin/MCP output (origin label, size cap, control-sequence strip)
      before it enters context. 42 unit tests. **Surfaced to users** via
      `codypendent plugin inspect <file>` (renders identity + the requested
      capability list + resource caps + trust posture — the "evaluate permissions"
      step) and `codypendent plugin diff <installed> <update>` (prints the
      permission diff and exits non-zero on an expansion, so CI can gate on
      re-approval) — the CLI seam mirroring `workflow validate`, with example
      manifests under `examples/plugins/word-count/`.
- [x] **6.5 (multimodal input model)** — the Chapter 10 `InputEnvelope`/`InputBlock`
      model in `codypendent-protocol`: a uniform envelope of typed blocks (Text,
      Audio, Image, File, EditorSelection, CodeSymbol, GitHubReference, forward-
      compatible `Unknown`). `ImageArtifact` keeps all four artifacts distinct
      (original + extracted text + observations + crop/coordinate regions) and
      `AudioArtifact` keeps the original audio linked to its reviewed transcript —
      the original is never replaced by a summary (exit criterion 3). The
      classification gate (`transcription_allowed`, media default `Confidential`)
      permits local transcription always but blocks remote transcription when the
      data exceeds an `OffDevicePolicy` ceiling. 10 round-trip/gate tests.
- [x] **6.6 (themes + theme packs)** — six semantic-token variants beyond dark
      (light, high-contrast, color-blind-safe Okabe–Ito, 256-color, 16-color,
      monochrome); `ColorDepth::detect()` (NO_COLOR/COLORTERM/TERM) +
      `Theme::select(depth, prefs)` with a manual override always winning; and a
      **data-only** theme-pack loader that structurally rejects any pack declaring
      capabilities/permissions (README: theme plugins get no execution
      permissions). 17 tests (legibility invariants per variant).
- [ ] **6.2/6.3/6.4 (enforcement + WASM + executable hooks)** — the native OS
      sandbox (bubblewrap+seccomp / sandbox-exec / AppContainer), the `wasmtime`
      component runtime + WASM SDK, the brokered-secrets host, and executing hooks
      / skill `scripts/` through the sandbox. These *consume* the STEP 6.1
      `SandboxProfile`; this is the "OS sandbox enforcement gates Phase 6"
      cross-cutting item.
- [ ] **6.5/6.7 (client capture + setup assistant)** — TUI clipboard/voice
      capture and IDE drag-drop feeding the input model; the agentic `setup`
      assistant under a restricted profile.

**Exit:** plugin cannot access undeclared path/network (decision layer ✅,
OS enforcement pending); permission-expansion on update requires approval ✅;
original audio/image artifacts linked ✅ (model); setup assistant proposes,
never silently changes (pending).

## Phase 7 — Intelligent routing & learning 🟡

The routing and learning **engines** have landed as two daemon-free crates
(`codypendent-routing`, `codypendent-eval`); the daemon wiring, the persisted
profiles/migrations, and the fixture task corpus are the remaining slice.

- [x] **7.1 (eval harness core)** — `codypendent-eval`'s `case` module: the
      Chapter 16 `EvalCase`/`Assertion` model (tests-pass, file changed/unchanged,
      symbol-exists, command-not-executed, citation, no-forbidden-network,
      approval-requested, patch-scope-limit) scored against an objective
      `RunObservation`, with cost/duration budgets and a `SuiteReport` aggregate.
      *Remaining:* the `codypendent eval run` CLI over the JSONL client and the
      50–100 pinned fixture cases in `evals/tasks/`.
- [x] **7.2 (capability + performance profiles)** — `codypendent-routing`'s
      `ModelCapabilities` (the Chapter 09 shape) + `RequiredCapabilities` hard
      filter, and a `ModelProfile` carrying **measured** performance (reliability,
      per-task-class success, cost/latency), a `ModelExecutionProfile`, and the
      `LocalBench` shape the harness fills. *Remaining:* migration `model_profiles`,
      the `codypendent models bench` harness that measures a local model, and
      first-use capability probes.
- [x] **7.3 (the router)** — the Chapter 09 pipeline exactly, per task node: a
      version-stamped rule-based task classifier; **security/privacy hard filters
      first** (classified data can never be scored against — let alone routed to —
      a hosted provider; it refuses rather than leaks); cheapest-model-above-the-
      quality-threshold selection with a utility score; a versioned `RoutingPolicy`
      (`router/<name>/<version>`); and **cascading escalation** that re-executes a
      failed node on the next chain tier preserving artifacts and recording a
      complete transition. The five eval-route arms + the release-gate report
      (router+escalation ≥ quality at cost < static-strongest) land here too (exit
      criterion 1). 37 tests. *Remaining:* daemon wiring behind the model-execution
      seam and running the arms over a real suite.
- [x] **7.4 (graders + clustering + regression suite)** — execution-grounded
      `Signal`s (+patch-applies … −policy-violation) from a terminal-run `Trace`
      (no model-vibes grading); deterministic `FailureCluster`ing by (task-class,
      failing signal, tool, error-fingerprint) into the improvement queue; and a
      `RegressionSuite` that grows with each fixed failure (a fixed cluster becomes
      a guard case) and treats a missing observation as a regression. *Remaining:*
      the OTLP exporter and daemon persistence.
- [x] **7.5 (promotion pipeline — nothing promotes itself)** — the draft →
      offline-regression → shadow → canary → **human approval** → promote →
      rollback state machine for every learnable artifact. **No self-promotion
      (ADR-010, exit criterion 2):** `approve()` requires an `Actor::Human` and is
      the *only* path to `Promoted` — an agent/system/integration approver is
      refused structurally; a canary regression auto-rolls-back without a human;
      `ActiveVersions::rollback` restores the predecessor (attributable +
      reversible, exit criterion 4); synthesized skill candidates must pass
      permission review first. 12 tests incl. "an agent cannot promote itself".
      *Remaining:* the daemon commands + persistence and the real shadow/canary
      execution + eval-export privacy scrubbing.

**Exit:** routing meets quality threshold at lower cost than static
strongest-model ✅ (engine + gate; measured run pending); no learned artifact
self-promotes ✅; regressions covered ✅ (suite engine); every promotion
attributable and reversible ✅.

---

## Client & TUI experience — Codex-informed backlog

Direction: adopt the **conversation-centred shell** — the Claude Code / Codex
CLI look and feel (a transcript-dominant surface, a persistent composer, `/`
slash commands, minimal permanent chrome) — as the base, and keep Codypendent's
richer surfaces (runs, approvals, docs, knowledge, code graph, workflows) as
overlays reachable from the palette. The feel is chat-first; the capability set
is deliberately broader. (Visualized in a TUI mock + borrow review produced
alongside this work.)

- [x] **Conversation-centred shell + layout toggle** — the base view is a
      full-width transcript + a persistent bottom composer + a one-row status
      footer. Type to send (a message starts a run, or steers the live one); `/`
      on an empty composer opens the palette; PgUp/PgDn scroll; Ctrl-↑/↓ switch
      runs; a pending approval owns the input until resolved. **`F2` (or the
      palette) toggles to a workspace layout** — Runs │ conversation │ approvals
      panes for at-a-glance state — sharing the same composer, footer, and input
      model, so the panes are context, not a separate mode. Pure-reducer; 70 TUI
      tests green.
- [x] **Command palette** (`/`) — one searchable surface for every command, the
      command hub now that typing composes a message rather than firing single-key
      actions.
- [x] **Rich approval cards** — action + risk + requested capabilities verbatim,
      at the point of decision (the approval modal owns input when pending).
- [x] **Narrative transcript** — typed, event-sourced cells (model prose, tool
      cards, diffs, markers) in one attributable stream — the shell's main surface.
- [x] **Contextual footer** — the status line drops fields by priority as the
      terminal narrows (mode/model/cost/worktree fall away first; state +
      attention always survive) and carries a right-aligned instructional hint
      that shifts by context: approve/reject when an approval is pending, `↧ latest`
      when scrolled up, send/clear while drafting, else `/ cmds · F2 layout`.
- [x] **Auto-scroll** — the conversation follows the latest by default (streaming
      stays pinned to the bottom); PgUp leaves follow to read history, PgDn (or
      sending a message) snaps back. The renderer measures the wrapped height and
      caches the bottom so paging is exact.
- [ ] **Composer polish** — the persistent composer exists; the rich editor
      remains: multiline, input history + reverse-search, `@` file/symbol mentions,
      large-paste placeholders, queue-while-working.
- [ ] **Side conversations & forks** — inspect or branch without derailing the
      main run; converges with Phase 5 STEP 5.6 `ForkSession{checkpoint}`.
- [ ] **Terminal-native polish** — resize reflow, paste-burst detection, IME
      input, terminal hyperlinks, copy-friendly output (folds into Phase 6 themes).

## Cross-cutting, Codex-informed priorities

From the broader Codex comparison, sequencing notes that touch several phases:

- [ ] **OS sandbox enforcement gates Phase 6.** The policy engine *decides*
      (deny / allow / approve); it does not yet *enforce*. Native isolation
      (bubblewrap + seccomp / Seatbelt / AppContainer) should land as a
      prerequisite for the plugin host and untrusted content, not after it — treat
      the policy engine as the compiler that emits a sandbox profile.
- [ ] **Finish the Phase 4 document vertical before deepening Phase 5.** One
      end-to-end slice (open → concurrent-edit → review suggestions → inspect graph
      evidence → publish through approval → reconnect) demonstrates the thesis
      better than breadth. The mutation engine, `DocumentSync` payload, edit-lease
      store, **the daemon transport, and lease acquire/release** now exist
      (`MutateDocument` applies through the assembly `DocumentMutator` seam and
      fans out to `Document` subscribers; `AcquireDocumentLease`/`ReleaseDocumentLease`
      take a real block-range lease through the `DocumentLeaser` seam). What still
      closes the loop: a client-side CRDT replica that consumes the sync stream,
      and publishing a `PublishPlan` through the approval-gated write path.
- [ ] **Trust boundary as plumbing, not new design.** Retrieved memories, skill
      descriptions, and CI/PR text must render as *evidence*, not instructions —
      the fabric already carries `EvidenceRef` / `TrustTier` / `DataClassification`
      / `Scope`, so this is finishing the wiring, not inventing it.
- [ ] **Generate the protocol SDK.** The VS Code extension hand-duplicates the
      Rust wire codec; a generated TypeScript + JSON-Schema pipeline from the
      protocol crate removes that drift risk as the protocol grows.

---

## Every-release hygiene (any phase)

- [x] `cargo fmt --all -- --check` clean
- [x] `cargo clippy --workspace --all-targets` clean
- [x] `cargo test --workspace` green
- [ ] `cargo deny check` / `cargo audit` clean or with dated exceptions
- [x] CI green on the release commit; working tree clean; migrations unchanged since first commit
