# Phase 2 — Skills and Knowledge

> **Objective:** the system becomes editable and knowledgeable: a governed registry of tools and skills with semantic retrieval, an always-on memory fabric with provenance, and a basic code symbol graph.
>
> **Specification chapters:** [Roadmap Phase 2](../15-roadmap.md), [Skills, Tools, and Plugins](../05-skills-tools-and-plugins.md), [Memory and Knowledge Fabric](../06-memory-and-knowledge-fabric.md), [Code Intelligence](../07-code-intelligence.md), [`agent-framework-rs` Integration](../12-agent-framework-rs-integration.md). Example manifests: [`specs/skill.toml`](../../specs/skill.toml).
>
> **Exit criteria (from the roadmap):** top-k selection beats full-tool injection on an evaluation set; skill permissions are visible; every retrieved memory opens its source; stale indexes rebuild from authority.

## New crate

**EDIT FILE `Cargo.toml`** — add member `"crates/knowledge"` (`codypendent-knowledge`): registry, retrieval, memory, code graph, search. Workspace dependencies to add: `tantivy = "0.22"` (BM25), plus keep the vector layer **abstracted** — Phase 2 ships a small embedded implementation (brute-force cosine over an in-memory matrix, persisted as an artifact); Qdrant remains a Phase 4+ option behind the same trait, adopted only on measured need ([manual index](../00-index.md) design stance).

## STEP 2.1 — Schema: migration 0003

Tables (same conventions as Phase 1): `registry_items` (all [Chapter 05](../05-skills-tools-and-plugins.md) `RegistryItem` fields; `intents`/`keywords`/`examples`/`permissions` as JSON; `scope`, `trust_tier`, `content_hash`, `risk`), `memories` (all [Chapter 06](../06-memory-and-knowledge-fabric.md) `MemoryRecord` fields; `provenance_json`; `supersedes_json`; `valid_from`/`valid_until` as revision strings), `code_nodes` (`SymbolKey` fields + kind + repository + revision), `code_edges` (`from`, `to`, `relation`, `confidence`, `evidence_kind`, `evidence_artifact`, `revision`), and `index_outbox` (`id`, `event_kind`, `entity_id`, `created_at`, `processed_at`).

**RULES**

1. The **index outbox** pattern is mandatory ([Chapter 06](../06-memory-and-knowledge-fabric.md)): every write to `registry_items`/`memories`/`code_*` inserts an outbox row in the same transaction. Indexer workers consume the outbox to update Tantivy/vector/any derived index. An indexer crash can never corrupt authoritative rows.
2. Derived indexes live under `<data_dir>/index/` and must be deletable at any time; a `codypendent index rebuild` CLI command replays authority into fresh indexes (exit criterion 4).

## STEP 2.2 — Registry and skill packages

1. Implement `RegistryItem` CRUD scoped by the [Chapter 06](../06-memory-and-knowledge-fabric.md) `Scope` enum. Registration paths: built-in tools (from Phase 1, now registered with metadata), workspace skills (`.codypendent/skills/`), user skills (`<config_dir>/codypendent/skills/`).
2. Skill package loader: a directory per [Chapter 05](../05-skills-tools-and-plugins.md) (`SKILL.md`, `skill.toml`, optional `tools.toml`, `tests/`, `references/`, `scripts/`, `assets/`). Parse `skill.toml` with **exactly** the key shapes of [`specs/skill.toml`](../../specs/skill.toml) (id, name, version, scope, status, description, intents, languages, required/optional tools, `[permissions]`, `[limits]`, `[entrypoints]`, `[trust]`). Reject packages with unknown top-level keys or undeclared entrypoint paths.
3. Content-hash every package file into the registry item; a changed file without a version bump flags the item `modified` (visible in the UI).
4. Ship one **reference skill**: `examples/skills/fix-ci/` — write `SKILL.md` (procedure: read failing test output → locate test → reproduce with `cargo test <name>` → minimal fix → rerun → summarize) and a `skill.toml` mirroring the spec example with `id = "rust.fix-ci"`.
5. Skill **scripts are not executable in Phase 2** — record them, display them, but execution waits for the sandbox (Phase 6). Mark this in the registry item so retrieval can't select a script-dependent behaviour.

**TESTS** — package parse round-trip; unknown-key rejection; hash-change detection; scope shadowing (workspace skill overrides user skill of the same id for retrieval, both remain visible).

**COMMIT** `"phase2: scoped registry and skill package loader with reference skill"`

## STEP 2.3 — Hybrid retrieval

Implement the [Chapter 05](../05-skills-tools-and-plugins.md) funnel in `knowledge/retrieval.rs`:

```text
candidates: dense top 100 ∪ BM25 top 100 ∪ exact id/tag top 50 ∪ history top 50
→ hard filters: scope, trust tier, policy/capability (security is a FILTER, never a score)
→ rerank (weighted sum per Chapter 05 scoring; weights in a versioned config struct)
→ dependency closure (a skill pulls its required tools)
→ context budget: disclose 6–12 tool cards + 1–3 skill cards
```

1. Dense embeddings: use the configured embedding model (add an `embedding` entry to `models.toml`; local option documented). Embed registry descriptions + intents at registration (via outbox worker), queries at run time. Cache by content hash.
2. BM25: Tantivy fields `name`, `description`, `intents`, `keywords`.
3. `ToolCard { id, name, summary, risk }` progressive disclosure: cards go into context; full JSON schemas are loaded **only** for tools the model actually selects — integrate with the framework's `SkillsProvider` progressive-disclosure tools per [Chapter 12](../12-agent-framework-rs-integration.md) (`CodypendentSkillsProvider` backed by this registry, filtering `before_run`, recording selected candidates in the trace).
4. Rerank weights, candidate sizes, and disclosure counts live in one versioned struct (`retrieval/config.rs`, `version: u32`) — Phase 7 learning will tune them; traces must record the version used.

**TESTS + EVALUATION GATE** — build the evaluation set now (this is the exit criterion): 30 labeled cases in `crates/test-support/fixtures/retrieval-eval.jsonl` (`{query, expected_tool_ids, forbidden_ids}` — write them from the Phase 1 tool set + reference skill + 20 synthetic decoy registry items). Metrics: recall@8 and unsafe-item exclusion = 100% on forbidden ids. Assert in a test that top-k retrieval achieves ≥ recall 0.8 while a "full injection" baseline (all items, no filter) exceeds the context budget — demonstrating top-k beats full injection under the budget.

**COMMIT** `"phase2: hybrid retrieval with hard security filters and eval gate"`

## STEP 2.4 — Memory fabric

`knowledge/memory.rs`, per [Chapter 06](../06-memory-and-knowledge-fabric.md):

1. **Observer:** subscribe to the daemon's event stream; extract candidates from `ToolCompleted` (repeated command patterns), `RunCompleted` chronicles (findings/decisions), and explicit model proposals (`memory.propose` tool added to the registry).
2. **Curator pipeline (order is normative):** candidate → secret/sensitivity filter (regex set for common key shapes + entropy heuristic; anything matching is dropped and logged as a redaction event) → scope classification (default `Repository`; `User` only for preference-class) → dedup (embedding similarity > 0.92 against same scope+class) → contradiction detection (same subject, incompatible statement → create **supersession**, never delete) → provenance attachment (every memory must carry ≥1 `EvidenceRef` to an event range or artifact — reject evidence-free candidates) → retention (defaults: 365 days; `policy.toml [memory]` respected).
3. Retrieval: memories join the context package with **citations**; the projection for the TUI renders the Chapter 06 provenance card (statement, source, revision, observed, scope, confidence) and the client can request the underlying artifact/event range — "every retrieved memory opens its source" (exit criterion).
4. Deletion: `codypendent memory forget <id|--scope SCOPE>` removes rows, writes index tombstones via outbox, and records an audit event that does **not** contain the deleted content.
5. **Cross-repository isolation is absolute:** retrieval scope-filters at SQL level; add the [Chapter 16](../16-testing-strategy.md) cross-repo leak test (memory in repo A never retrieved in repo B, even with identical language/framework).

**TESTS** — pipeline order; secret candidate dropped; supersession (query at old revision returns old value, at new revision returns new); evidence-free rejection; scope isolation; forget + tombstone.

**COMMIT** `"phase2: memory observer, curator, provenance, scoped retrieval"`

## STEP 2.5 — Basic code graph

`knowledge/codegraph.rs`, the **syntax layer only** ([Chapter 07](../07-code-intelligence.md) — semantic/LSP arrives Phase 4):

1. Tree-sitter parsing for Rust (add `tree-sitter = "0.24"`, `tree-sitter-rust = "0.23"`): extract `File`, `Module`, `Type`, `TraitOrInterface`, `Function`, `Method`, `Constant` nodes and `Contains`/`Defines`/`Imports`/`Calls`-as-written edges, confidence 0.45 for syntax-inferred calls (the Chapter 07 table), each edge carrying `EvidenceRef` (file artifact + byte range) and the Git revision.
2. Incremental pipeline: filesystem watcher (`notify = "7"`) → ignore/generated-file policy (respect `.gitignore` + `target/`) → changed-file reparse → graph delta in one transaction + outbox → `CodeGraphUpdated` event.
3. `SymbolKey` stable identity exactly per Chapter 07 (qualified name + kind + signature hash); durable graph keeps public symbols, functions, types, tests — **no local variables** (generate on demand later).
4. **Repository map v1:** fold the graph into the compact map (packages → important modules → public APIs → tests → current change surface) and register it as a context provider for the agent loop (replacing the Phase 1 placeholder).

**TESTS** — parse fixture crate → expected nodes/edges; rename file keeps SymbolKey for unchanged symbols; incremental delta equals full reparse (property); repository map snapshot.

**COMMIT** `"phase2: tree-sitter code graph, incremental deltas, repository map"`

## STEP 2.6 — Skill Studio (TUI) and registry UI

Extend the TUI with a Skills view ([Chapter 05](../05-skills-tools-and-plugins.md) Skill Studio list, scoped to what exists): browse by scope/trust/status; inspect metadata, **permissions verbatim** (exit criterion: "skill permissions are visible"); create/clone from template; edit `SKILL.md` (open `$EDITOR`; the TUI is not a text editor yet); version bump + changelog line; deprecate. AI-assisted editing appears as a diff proposal through the normal changeset flow — attributed, never silent.

Memory view: list by scope/class, provenance card, open-source action, forget with confirm.

**TESTS** — reducer tests for both views; snapshot of the permission panel.

## Exit checklist

- [ ] Retrieval eval: recall@8 ≥ 0.8, forbidden-item exclusion 100%, disclosure within budget (test green).
- [ ] `rust.fix-ci` reference skill loads, retrieves for "the CI test is failing", and its permissions render in the Studio.
- [ ] A run that discovers "tests use cargo nextest" produces a curated memory whose provenance opens to the source event/artifact.
- [ ] Memory never leaks across repositories (test green).
- [ ] `codypendent index rebuild` after deleting `<data_dir>/index/` restores search results identically (test or scripted check).
- [ ] Agent context now includes repository map + cited memories + retrieved tool/skill cards (inspect a run trace to confirm all three manifests).
- [ ] `fmt` / `clippy` / `test` green; commits made; tree clean.
