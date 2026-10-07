# Codypendent — Build Roadmap & Progress Tracker

A single, scannable view of where the build is. Phases are **usable vertical
slices**, not isolated subsystems — each one ends with something you can run.

**Legend:** ✅ done & verified · 🟡 in progress · ⬜ not started

For the full narrative and exit criteria see
[`docs/docs/15-roadmap.md`](docs/docs/15-roadmap.md); for step-by-step build
plans see the [End-to-End Build Guide](docs/docs/build/00-how-to-use-this-guide.md).

**What actually gates a release** is the `ci` workflow — `lint`, `test`,
`eval-smoke`, `eval-regression`, `doc-counts`, `deny`, `extension` — plus the
release workflow's own gates. That is the gate every shipped release has passed.
The [Master Acceptance Checklist](docs/docs/build/99-master-acceptance-checklist.md)
is the *aspirational* acceptance document for the finished product: **0 of its 34
boxes are ticked** (counted 2026-08-13) and v0.5.1 shipped anyway, so calling it
"the release gate" was false. It is a to-do list for reaching 1.0, and the
"Every-release hygiene" section at the end of this file is the honest,
maintained subset.

---

## At a glance

| Phase | Slice | Status |
|------:|-------|:------:|
| **0** | Workspace bootstrap — daemon lifecycle, protocol, ledger, CI | ✅ |
| **1** | Persistent coding-agent slice — sessions/runs, tools, approvals, TUI, JSONL | ✅ |
| **2** | Skills & knowledge — registry, retrieval, memory, code graph | ✅ |
| **3** | GitHub & IDE awareness — PR flows, editor extensions, shared session | ✅ |
| **4** | Docs Studio & code intelligence — CRDT docs, semantic index | ✅¹ |
| **5** | Workflows & multi-agent orchestration | ✅² |
| **6** | Plugins & multimodal — MCP/WASM plugins, voice/image, themes | 🟡 |
| **7** | Intelligent routing & learning — model router, graders, canary | 🟡 |

Phases 0–7 are this file's scope. The **hybrid platform programme (M0–M9)** —
session library, inbox, analytics, automation, secrets, marketplace, federation,
control plane, remote runners — is tracked separately in
[`docs/superpowers/plans/2026-08-16-hybrid-platform-program.md`](docs/superpowers/plans/2026-08-16-hybrid-platform-program.md),
which carries a per-task **built vs reachable** verdict. Read that distinction
before assuming any M2–M9 capability ships: a large amount of that code exists,
passes its tests, and has no production caller.

> **v0.11.0 moved several of those verdicts and added one.** Reachable now:
> automation firing (durable lease + fenced compare-and-swap claim — the first
> INSERTs `automation_receipts`/`automation_attempts`/`automation_leases` have
> ever had), `automation_endpoints`, analytics budgets and the budget-alert
> evaluator, Session Library search/lifecycle in the TUI and CLI, session-bundle
> export/import, OS notifications on both hosts, and the packaged desktop bundle
> and `.vsix`. Still NOT reachable: **creating an automation binding** —
> `ManageAutomationBinding` has a decided role floor, a real `named_resources()`
> arm and a wired handler, and no client anywhere constructs it, so the firing
> engine has no author surface. Also still source-only: the control plane, the
> daemon⇄control-plane sync engine, and the remote runner. Details, per item, in
> [`docs/releases/v0.11.0.md`](docs/releases/v0.11.0.md).

> ¹ Phase 4's collaborative-documents vertical is closed (client-side CRDT replica
> + live TUI editing, and `PublishPlan` execution through the approval-gated write
> path). Remaining follow-up: spawning a live language server (rust-analyzer /
> pyright) — the adapter reports the capability; edges are proven with synthesized
> data today. ² Phase 5's workflow/multi-agent slice is complete (declarative
> workflows, durable checkpoints + crash recovery, the blackboard channel, tool-node
> execution with a meaningful patch→verify handoff, per-run isolated worktrees,
> nested budgets, node-level cost/provenance, observability + cancel, and `/fix-ci`
> on the declarative engine). Remaining overlay: session forking (STEP 5.6). Phases
> 6 and 7 landed their engines **and** their first wiring — OS sandbox enforcement
> v1, and a default-off model-router daemon seam + eval-run CLI + promotion
> persistence — but keep 🟡 for the genuinely-remaining slices (WASM runtime +
> client capture for 6; a live measured routing run + real shadow/canary execution
> for 7). See each phase below.

> **You are here:** Phases 0–5 are complete and verified; Phases 6 and 7 have their
> engines **and** their first production wiring, with defined remaining slices. The
> engine-to-wiring gap the earlier project reviews flagged has largely closed — the
> previously unwired routing / eval / sandbox engines now have real production
> consumers.
>
> **Phase 4 — docs vertical closed.** On top of the Loro-backed document engine
> (ADR-016; concurrent-merge convergence, per-mutation authorship, suggest-by-default
> for org docs, deterministic Markdown, the semantic `LanguageAdapter` + revision-aware
> graph queries, the staleness engine) the client + write paths now exist: a
> **client-side CRDT replica** consumes the `DocumentSync` stream for live TUI editing
> (seeded from the authoritative snapshot, idempotent merge — proven with two-client
> socket convergence), and **`PublishPlan` executes through the approval-gated write
> path** (repository file / docs branch / documentation PR), parking a durable approval
> that shows target + changed files + git action before any write. Remaining follow-up:
> spawning a live language server (rust-analyzer / pyright).
>
> **Phase 5 — complete.** Declarative `workflow.yaml` → validated graph → durable
> runs / node records / checkpoints with crash recovery; the model-free
> `WorkflowDriver`; daemon create / drive / recover / **pause·resume·retry·cancel**;
> agent nodes on the real agent loop **and tool nodes** executing through the runtime
> tool layer (namespace-normalized, argument-bound, approval-parked — every GitHub
> write gated); the **blackboard** typed-artifact channel (server-derived authorship,
> evidence-required, per-run isolation, `post`/`query` tools + read command +
> subscription); **per-run isolated worktrees** with read-your-writes coherence
> (concurrent writers never share a tree); **nested budgets** (workflow→node, 80%
> warning, block-on-exceed, resume without re-spend) with **role→profile enforcement**
> (a reviewer is read-only by *policy*, not prompt) and **measured** node cost;
> workflow **observability** (a `WorkflowEvent` stream + live TUI graph) and durable
> failure reasons. The **patch→verify handoff** makes the flagship `repair-github-check`
> workflow genuinely verify a fix (the implementer's diff becomes an artifact; verify
> applies it into its own worktree under approval before testing), and **`/fix-ci` now
> runs that declarative workflow** (the hard-coded prompt template is gone),
> resolved from an embedded built-in shadowable by `.codypendent/workflows`. Remaining
> overlay: session forking (STEP 5.6). The Codex-informed **conversation-centred TUI
> shell** (palette, layout toggle, auto-scroll, contextual footer, live theming) has
> shipped.
>
> **Phase 6 — sandbox enforcement v1 landed (🟡 overall).** Beyond the `codypendent-sandbox`
> decision layer (signed-manifest verification, permission-diff, closed `SandboxProfile`,
> multimodal input model, themes), a real **OS enforcement** executor now consumes a
> profile — genuine macOS Seatbelt confinement (verified by real filesystem/network
> denial tests), a Linux bubblewrap arg-generator, fail-closed elsewhere — plus a
> **trusted-publisher key store** wired into verification and **sandboxed skill-script
> execution**. Remaining: the WASM/wasmtime component runtime, the hook engine,
> client capture (voice/clipboard), and the setup assistant.
>
> **Phase 7 — routing + eval + promotion wired (🟡 overall).** The router
> (`codypendent-routing`) now has a **daemon seam** (default-OFF; when enabled, the
> classification hard-filter fails closed so classified data never reaches a hosted
> provider), a persisted `model_profiles` store (migration 0014), a local **bench**
> harness, and first-use capability probes. The learning loop (`codypendent-eval`) has
> a **`codypendent eval run` CLI** + a runnable fixture corpus + CI smoke, and the
> **promotion pipeline is persisted** (migration 0015) and driven through daemon
> commands with the ADR-010 human-approval gate (nothing promotes itself). Remaining:
> a live *measured* routing run + live escalation re-drive, and real shadow/canary
> execution + eval-export scrubbing (the mechanisms and gates are real; the live
> measurement paths are the remaining slice).
>
> This state was reached by the **roadmap-completion effort** (branch
> `claude/roadmap-completion-w20`, PR #19): 19 tasks + the two–project-review defect
> backlog, each implemented → independently reviewed → fixed → re-verified, closed by
> a multi-agent whole-branch review. Hygiene is green throughout (fmt, clippy
> `-D warnings`, `cargo test --workspace` = **≈4073 tests as of 2026-10-07**
<!-- doc-count:test sources="crates" expect=4073 label="workspace total" -->
> (a `#[test]`/`#[tokio::test]` count over every `crates/**/*.rs` file at HEAD —
> a live `cargo test --workspace` run is the authoritative source but is not
> safe to run in every environment this doc is read in; re-derive with
> `git ls-tree -r --name-only HEAD -- crates | grep '\.rs$' | xargs -I{} git
> show HEAD:{} | grep -cE '#\[(tokio::)?test\]'` summed, or just run the
> suite), `cargo deny`, VS Code extension typecheck/lint/test).

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

- [x] Retrieval eval: **recall@8 = 0.9500** (both the `hashing-trigram` and
      `word-hash-semantic` embedders, ≥ 0.8 gate), 100% unsafe-item exclusion,
      disclosed top-k (384 / 396 tok respectively) fits a budget the
      full-injection baseline (6621 tok) blows through. **Measured, not
      estimated** — reproduce with `cargo test -p codypendent-knowledge
      --test retrieval_eval -- --nocapture`; this is a live number over a
      fixture corpus that grows as other exit criteria add cases to it, so
      re-run rather than trust this line the way an earlier version of it
      (recall@8 = 1.0, 254 tok, 4580 tok) went stale.
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
- [x] **3.3** Webhook ingestion — `X-Hub-Signature-256` HMAC verify **before** parse; normalize → internal events; `X-GitHub-Delivery` GUID replay dedup (migration `0005`); optional loopback listener wired into `codypendentd` (default off); policy-off ⇒ no workflow trigger. **Updated 2026-08-18 (v0.11.0):** policy-*on* now does trigger one. `crates/codypendentd/src/automation.rs`'s `AutomationWebhookSink` is the `WebhookEventSink` this task built and never attached, and a verified, deduplicated delivery is fanned out to every enabled binding on that endpoint. It is a **second, separate** opt-in: `webhooks.toml` must set `automation_dispatch = true` on top of `enabled = true`, and both default to false. The per-endpoint signing key, body ceiling and replay window in `automation_endpoints` also finally govern something — `codypendent webhook endpoint add` is the writer that table never had, so `resolve_endpoint` no longer returns `None` for every request that reaches the listener
- [x] **3.4** IDE bridge + source-provenance live-path — protocol `IdeContextUpdate`/`DirtyBufferDigest`/edit-request types + `SourceProvenance`; `UpdateIdeContext` command stored as a projection (migration `0006`); the run read path labels an excerpt whose disk bytes diverge from an unsaved editor buffer `unsaved-ide-buffer` in the trace; `IdeBridge` trait; deterministic debounce
- [x] **3.5** VS Code / Cursor extension — `extensions/vscode/` (TypeScript, esbuild): frame codec + discovery over the generated `@codypendent/protocol` package (as of v0.11.0 the 811-line hand-written `src/protocol/types.ts` mirror that shipped alongside it is **deleted**; the one remaining locally-declared wire view, `src/remote-ui/wire.ts`, is guarded field-by-field against the generated type at compile time), a `DaemonClient` attaching as `Approver` with reconnect-resume, a side-panel webview, approval notifications → `ResolveApproval`, debounced `IdeContextUpdate` push, `vscode.diff`; 307 vitest tests
<!-- doc-count:vitest project="extensions/vscode" metric="tests" expect=307 label="VS Code vitest suite" -->
      across 14 files
<!-- doc-count:vitest project="extensions/vscode" metric="files" expect=14 label="VS Code vitest files" -->
      (re-derived from a real `npm test` run in the `extension` CI job — needs `sdk/ui` built first, which `npm install` now does) + typecheck + lint green; Cursor compat note
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
Code extension's codec/discovery/reconnect pass its vitest suite. `fmt` / `clippy
--all-features -D warnings` / `test --workspace` green; `extensions/vscode`
typecheck/lint/test green.

## Phase 4 — Docs Studio & richer code intelligence ✅

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
- [x] Live daemon CRDT-sync transport for the `Document` subscription + block-range edit-lease enforcement — *engines:* (a) `apply_mutation` maps a protocol `DocumentMutation` onto the authoritative CRDT + suggestion store under the collaboration-mode gate (Edit applies directly; Suggest/Co-author/Maintain route to the review rail; Ask/Review deny; accept/reject resolve) and returns the `DocumentSync` (`Payload::DocumentSync` carries it on the wire); (b) `DocumentLeaseStore` (migration 0009) enforces **one writer per block-range** — a whole-document lease conflicts with any block lease both ways, leases expire and are reclaimed lazily, the same writer renews, and `require()` is the pre-mutation guard. *Transport (now wired):* `MutateDocument` is intercepted at the connection level (like `AttachSession`/`UpdateIdeContext`, since documents live outside the session ledger) and applied through a daemon `DocumentMutator` seam — implemented in the `codypendentd` assembly over `apply_mutation` (mode derived from the document's **scope** via a lightweight `DocumentStore::scope` read) with lease `require` enforced first; the resulting `DocumentSync` fans out to `Subscription::Document` subscribers over a per-document `DocumentHub` (idempotent CRDT merge ⇒ no watermark needed). *Lease-acquire (now wired):* `CommandBody::AcquireDocumentLease`/`ReleaseDocumentLease` are intercepted at the connection level like `MutateDocument` and applied through a daemon `DocumentLeaser` seam (bundled onto the `RunExecutor`, implemented in the assembly over the same `DocumentLeaseStore`), so a client takes a real block-range lease before editing and is recognised as that writer when its mutation runs `require`; the reply is a `Payload::DocumentLeaseGranted` carrying the minted lease id + expiry, an Observer is role-denied, and a conflicting holder is `document.range-leased`. *Now wired:* the client-side CRDT replica (`DocumentReplica`) consumes the sync stream for live TUI editing — seeded from the authoritative snapshot, idempotent merge (proven by two-client socket convergence + range-lease exclusion + byte-exact suggest-mode accept)
- [x] Executing a `PublishPlan` through the approval-gated change set / Phase 3 GitHub write path (repository-file / docs-branch / documentation-PR targets; the plan's target + changed files + git action park a durable approval before any write; `(revision ↔ commit)` publication record persisted)
- [ ] Spawning a live language server (rust-analyzer/pyright) and folding its resolved edges (the adapter reports the capability; supersession is proven with synthesized edges)

**Exit:** concurrent edits merge ✅; document snapshot reproducible ✅; symbol
changes flag affected docs with evidence ✅; graph edges expose evidence +
revision ✅ (data model + read-only TUI inspector render). ADR-016 recorded ✅;
suggest-by-default enforced ✅; `fmt`/`clippy`/`test` green ✅.

## Phase 5 — Workflow & multi-agent orchestration ✅

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
        daemon log today). **Agent nodes now execute the real agent loop:**
        `AgentLoopNodeExecutor` (in `codypendentd`) synthesizes an objective from the
        node's role + declared outputs + run inputs, creates a session + run, drives
        the agent loop to a terminal `RunDisposition` through the shared run plumbing
        (journal / sink / policy / approvals), and maps it to the node's outcome —
        recording the agent-run id the graph view links to. The model driver is built
        through a `NodeModelDriverFactory` seam, so the whole agent-node path is tested
        with a `ScriptedDriver` (no model, no network): a single-agent workflow drives
        to completion, and a missing model fails the node cleanly rather than hanging.
        *Completing 5.2 (all landed):* **tool-node execution** through the runtime
        tool layer (manifest tool names normalized `-`→`_` against the registry; a
        `repository.test` tool + per-tool argument binding with `${{ inputs.… }}`
        interpolation; every GitHub write approval-parked); harvesting an agent
        node's declared `outputs` onto the run's blackboard (agent nodes build on
        each other); node-level **mode/permission resolution from `agent.toml`**
        (the reviewer role read-only by policy); and the client-facing
        `Subscription::Workflow` stream (a `WorkflowEvent` node-transition + run-phase
        stream + a live TUI graph + `CancelWorkflow`).
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
        posts artifacts — now populated in production). *Completing 5.3 (landed):*
        the `blackboard.post`/`blackboard.query` registry tools (server-derived
        authorship + evidence-required, offered only inside a workflow run), a
        `ReadBlackboard` daemon command, and per-run `Subscription::Blackboard`
        delivery — so agents coordinate only through the typed board.
- [x] Parallel worktrees; budgets; independent review agent — all landed.
      **Pause / resume / retry-from-node / cancel** (conductor + `WorkflowLifecycle`
      commands + CLI). **Per-run isolated worktrees** (T5): every writing node gets
      its own tree carved from the run's repository — two concurrent writers never
      share one, and read-your-writes holds within a node. **Nested budgets** (T8):
      workflow→node over wall-time + tool-calls, an 80% `BudgetWarning`, `Blocked`
      + a cooperative pause on exceed, resume without re-spend. **Independent
      review agent** (T8): a step's `agent.toml` `mode` is enforced by the *policy*
      engine, so a `review`-mode reviewer is structurally denied writes (not merely
      prompted) — the ADR-008 structural independence.
- [x] **STEP 5.6 note:** session forking (`ForkSession{checkpoint}`) remains as the
      one Fleet-adjacent overlay not built; the rest of Phase 5 is complete.

**Exit:** multi-agent edits never share writable worktrees ✅ (per-run isolated
worktrees, concurrent-writer test); workflow resumes after restart ✅ (startup
recovery drives every incomplete run, incl. re-parking a node left `WaitingApproval`);
node-level cost/provenance visible ✅ (measured per-node records + `WorkflowEvent`
stream + live TUI graph); single-agent baseline selectable ✅; `/fix-ci` runs the
declarative `repair-github-check` engine ✅; budget exhaustion blocks visibly ✅.

## Phase 6 — Plugin & multimodal ecosystem 🟡

The security-decision engines landed as daemon-free crates, and **OS enforcement
v1 now consumes their profiles**: a real macOS Seatbelt executor (verified by
filesystem/network denial tests), a Linux bubblewrap arg-generator, fail-closed
elsewhere; a trusted-publisher key store wired into verification; and sandboxed
skill-script execution. The **WASM/wasmtime** runtime, the hook engine, and the
live client-capture paths (voice/clipboard) are the remaining wiring.

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
      before it enters context. 167 unit tests (measured 2026-08-29 over `crates/sandbox/src`, `#[test]`/`#[tokio::test]`; plus 20 more in `crates/sandbox/tests/`). **Surfaced to users** via
<!-- doc-count:test sources="crates/sandbox/src" expect=167 label="sandbox unit tests" -->
      <!-- doc-count:test sources="crates/sandbox/tests" expect=20 label="sandbox integration tests" -->
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
      data exceeds an `OffDevicePolicy` ceiling. 30 round-trip/gate tests (measured 2026-08-16: `crates/protocol/src/input.rs` 9 + `envelope.rs` 21).
<!-- doc-count:test sources="crates/protocol/src/input.rs,crates/protocol/src/envelope.rs" expect=30 label="multimodal round-trip/gate tests" -->
- [x] **6.6 (themes + theme packs)** — six semantic-token variants beyond dark
      (light, high-contrast, color-blind-safe Okabe–Ito, 256-color, 16-color,
      monochrome); `ColorDepth::detect()` (NO_COLOR/COLORTERM/TERM) +
      `Theme::select(depth, prefs)` with a manual override always winning; and a
      **data-only** theme-pack loader that structurally rejects any pack declaring
      capabilities/permissions (README: theme plugins get no execution
      permissions). 29 tests (legibility invariants per variant; measured 2026-08-16: `crates/tui/src/theme.rs` 19 + `theme_pack.rs` 10).
<!-- doc-count:test sources="crates/tui/src/theme.rs,crates/tui/src/theme_pack.rs" expect=29 label="theme tests" -->
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

The routing and learning engines landed as two daemon-free crates, and their
**first daemon wiring** is now in place: a **default-off routing seam** (when
enabled, the classification hard-filter fails closed — classified data never
reaches a hosted provider), a persisted `model_profiles` store (migration 0014) +
a local `models bench` harness + first-use capability probes; a **`codypendent
eval run` CLI** + a runnable fixture corpus + a CI gate that runs it for real
(`eval-regression`, baseline 13/13 — and see
[`evals/README.md`](evals/README.md)'s "What this gate can and cannot detect"
for what it does **not** prove: with a deterministic stub model, a prompt or
skill edit cannot move this score, so "a skill or prompt edit that lowers the
score fails CI" is not what this gate does); and the **persisted
promotion pipeline** (migration 0015) driven through daemon commands with the
ADR-010 human-approval gate. The remaining slice is the **live measured paths**:
a real routing run over the eval suite + live escalation re-drive, and real
shadow/canary execution + eval-export scrubbing (the mechanisms + gates are real
and tested; only the live measurement is deferred).

> **Correction, re-verified 2026-08-18 (the v0.11.0 wave):** the gap the v0.10.0
> correction recorded here is **closed**, and the note is kept rather than deleted
> so nobody re-derives the pessimistic version. The daemon still refuses
> caller-supplied `CanaryMetrics` — `PromotionAction::ObserveCanary` carries no
> `metrics` payload at all, so there is no field through which a caller could
> assert a sample count — but it now MEASURES the slice itself.
> `crates/codypendentd/src/promotion.rs` derives the evidence from
> `execution_observations`, writes it to `promotion_canary_evidence` before
> advancing anything, and then calls `PromotionStore::observe_canary_samples`.
> That accumulator therefore has a production caller, reachable from
> `codypendent promote advance --step observe-canary`, so `MIN_CANARY_SAMPLES = 100` is
> satisfiable by 100 genuine terminal runs and a candidate can reach `Promoted`.
>
> Two limits survive and are not defects: it fails closed on every gap with
> distinct non-retryable codes (unattributable artifact kind, no measured
> candidate executions, no concurrent baseline, latency unmeasured on either
> side — an absent latency is never treated as 0 ms), and **only `model-profile`
> candidates can be promoted.** Skill, prompt, router, workflow and retrieval
> candidates refuse with `promotion.canary-unattributable-artifact`, because
> nothing in the recorded executions ties a run to them; measuring ambient
> traffic and calling it evidence would be worse than refusing.

- [x] **7.1 (eval harness core)** — `codypendent-eval`'s `case` module: the
      Chapter 16 `EvalCase`/`Assertion` model (tests-pass, file changed/unchanged,
      symbol-exists, command executed/not-executed, command/network denied,
      citation, no-forbidden-network, approval-requested, patch-scope-limit)
      scored against an objective `RunObservation`, with cost/duration budgets
      and a `SuiteReport` aggregate. **Every shipped case must carry at least one
      assertion that cannot hold unless the run really acted**
      (`Assertion::requires_observed_action`, enforced over every case file by
      `crates/eval/tests/corpus_it.rs`) — three cases were previously built
      entirely from `file-unchanged` and passed when the harness did nothing.
      *Remaining:* the 50–100 pinned fixture cases in `evals/tasks/` (13 ship).
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
      criterion 1). 55 tests (measured 2026-08-17, `crates/routing/src/*.rs`). *Remaining:* daemon wiring behind the model-execution
<!-- doc-count:test sources="crates/routing/src" expect=55 label="router tests" -->
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
      permission review first. 21 tests incl. "an agent cannot promote itself" (measured 2026-08-13: `crates/eval/src/promote.rs`'s own `#[test]` count).
<!-- doc-count:test sources="crates/eval/src/promote.rs" expect=21 label="promotion pipeline tests" -->
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
      model, so the panes are context, not a separate mode. Pure-reducer; 740 TUI
<!-- doc-count:test sources="crates/tui/src" expect=740 label="TUI shell tests" -->
      tests green (whole-crate count, measured 2026-08-29 — grows with every outcome the TUI vertical adds; re-derive rather than trust a fixed number here).
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
- [x] `cargo deny check` clean (advisories/licenses/bans/sources) via `deny.toml`
      + a CI `deny` job; three unmaintained-transitive advisories carried as dated
      exceptions
- [x] CI green on the release commit; working tree clean
- [ ] Migrations unchanged since first commit — **false, verified against the
      published artifacts** (2026-08-13, by downloading the file at each tag and
      hashing it):

      | Published build | `migrations/0003_phase2.sql` sha256 (first 12) |
      |---|---|
      | `v0.1.0-build.42` | `a29143289fa4` |
      | `v0.1.0-build.43`, `.44`, `.45` | **`a5c81199c24b`** |
      | `v0.1.0-build.46` … `v0.5.1`, HEAD | `a29143289fa4` |

      A four-line comment clarification shipped in three real releases and was
      then reverted, so a database created by build.43/.44/.45 cannot be opened
      by any later release: `sqlx::migrate` refuses to boot on a changed
      checksum ("migration N was previously applied but has been modified").
      **Correction to an earlier version of this entry**, which named
      `0017_promotion_evidence.sql`: that migration is byte-identical
      (`5d5adab8ca8a`) at `v0.1.1-build.50`, at `v0.5.1` and at HEAD — its
      mutating commit `7eef118` landed *before* the tag was cut, so 0017 was
      never shipped mutated, and there is no `v0.1.1` tag at all (the releases
      API returns 404). The conclusion was right; the example was wrong.
      `migrations/README.md` names the real one correctly.
