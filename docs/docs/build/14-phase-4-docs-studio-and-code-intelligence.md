# Phase 4 — Docs Studio and Richer Code Intelligence

> **Objective:** collaborative CRDT-backed documents with Git publication and evidence-backed links into code; semantic (LSP-grade) code intelligence for Rust plus Python/TypeScript adapters; a staleness engine that keeps documentation honest.
>
> **Specification chapters:** [Roadmap Phase 4](../15-roadmap.md), [Collaborative Docs Studio](../08-docs-studio.md), [Code Intelligence](../07-code-intelligence.md), [Memory and Knowledge Fabric](../06-memory-and-knowledge-fabric.md).
>
> **Exit criteria (from the roadmap):** concurrent edits merge; a document snapshot is reproducible; symbol changes flag affected docs; graph edges expose evidence and revision.

## STEP 4.1 — CRDT benchmark (the validation gate comes first)

[Chapter 08](../08-docs-studio.md) names Loro as the candidate, **gated on benchmarks** — run the gate before writing product code:

1. Create `benches/crdt-bench/` (a separate workspace member, excluded from release builds) comparing **Loro**, **Automerge**, and **Yrs** on the Chapter 08 matrix: 1 KB / 100 KB / 10 MB documents; single + concurrent writers; long histories; large paste/delete; snapshot load; incremental catch-up; post-compaction memory.
2. Emit a Markdown report to `docs/docs/benchmarks/crdt-<date>.md` with a decision table.
3. **Decision rule:** pick Loro unless it loses by >2× on snapshot load or memory for the 10 MB case, or fails rich-text/history requirements; record the decision as ADR-016 in [Chapter 17](../17-architecture-decisions.md) with the numbers.

**COMMIT** `"phase4: crdt benchmark and selection ADR"`

## STEP 4.2 — Document model and storage

In `codypendent-knowledge`, add `docs/`:

1. `KnowledgeDocument` + `DocumentBlock` + `DocumentAuthor` + `AuthorshipRecord` exactly per [Chapter 08](../08-docs-studio.md) (blocks include `EmbeddedSymbol`, `EmbeddedWorkflow`, `EmbeddedSkill`, `Query`).
2. Storage: migration 0004 adds `documents` (id, title, scope, status, metadata_json, crdt_snapshot artifact id, links_json, citations_json) — the **CRDT state is authoritative for drafts** (ADR-004); snapshots are stored as artifacts; block-structured export/import must round-trip losslessly even though the CRDT's internal representation differs.
3. Authorship: every mutation records `Human(user)` or `Agent{run_id, model, policy_version}`; agent sentences must be traceable to their run and evidence.
4. Documents index into search (outbox → Tantivy + vector) like every other entity.

**TESTS** — concurrent edits from two simulated clients merge without loss (exit criterion 1); export→import→export is byte-identical; authorship recorded per block mutation.

## STEP 4.3 — Collaboration modes and protocol

1. Protocol additions: `DocumentInsert`/`DocumentDelete`/`DocumentAnnotate` semantic mutations ([Chapter 03](../03-daemon-client-protocol.md)) and a `Document{document_id}` subscription carrying CRDT sync messages; document edit **leases** (one writer per block-range; readers unlimited) use the Phase 1 lease machinery.
2. Agent collaboration modes per the [Chapter 08](../08-docs-studio.md) table — `Ask`, `Suggest`, `Edit`, `Co-author`, `Review`, `Maintain` — mapped to policy: suggestions are CRDT annotations (proposed ranges + replacement), applied only via an accept command; **organization-scope documents default to Suggest**; `Edit` obeys the run's approval policy.
3. TUI Docs view per the Chapter 08 sketch: document tree by scope; editor pane (block-aware, `$EDITOR` escape for long text); review rail (suggestions count, stale links, citations); accept/reject per suggestion; keyboard-equivalent for every mouse action.

**TESTS** — Suggest mode cannot mutate content directly (policy test); accept applies exactly the annotated range; two clients see each other's presence and edits live.

**COMMIT** `"phase4: collaborative documents with modes, leases, suggestions"`

## STEP 4.4 — Git publication

Publishing per [Chapter 08](../08-docs-studio.md): Git is the **reviewed snapshot store**, not the collaboration algorithm.

1. `publish(document_id, target)` renders blocks → Markdown/MDX deterministically (stable block ordering, stable anchor ids — same CRDT state must always render byte-identical output; that is exit criterion 2 "snapshot is reproducible").
2. Targets: repository file (via changeset + approval), commit on a docs branch, or documentation PR (via Phase 3 GitHub write path). Every publish displays target, changed files, and resulting Git action before approval.
3. Published snapshots record `(document revision ↔ git commit)` so staleness can compare.

**TESTS** — deterministic render (property: render twice, identical bytes); publish-to-repo creates an approval-gated changeset; document links to its publication commit.

## STEP 4.5 — Semantic code intelligence (Rust first)

Upgrade the Phase 2 syntax graph with the semantic layer ([Chapter 07](../07-code-intelligence.md)):

1. `LanguageAdapter` trait exactly as Chapter 07 (`parse`, `symbols`, `diagnostics`, `build_metadata`).
2. **Rust adapter:** rust-analyzer via LSP (spawn as child process; use `lsif`/`scip` export when available, else LSP `textDocument/definition`+`references` walks): resolved references, implementations, trait relations at confidence 0.90; `cargo metadata` for package/dependency nodes; compiler diagnostics via `cargo check --message-format=json`; test discovery via `cargo test -- --list`.
3. Confidence and evidence per edge exactly as the Chapter 07 table (syntax 0.45 / LSP 0.90 / compiler-index 0.98 / runtime 1.0); an LSP-resolved edge **supersedes** its syntax-inferred counterpart rather than duplicating it.
4. **Python and TypeScript adapters** (deliverable-level, thinner): tree-sitter syntax layer + optional LSP (pyright / typescript-language-server) when found on PATH; graceful degradation to syntax-only with lower confidence.
5. Revision-aware queries ([Chapter 07](../07-code-intelligence.md)): implement `graph.changed_between(rev_a, rev_b)`, `graph.callers_of(symbol)`, `graph.blast_radius(symbol, depth)`, `graph.tests_covering(path)` — these power staleness and the Phase 5 planner.
6. Hierarchical repository maps for large repositories: workspace → package → module summaries, generated bottom-up, each map node recording which evidence produced it; the TUI can show why a symbol entered context.

**TESTS** — fixture crate: LSP edge supersession; blast-radius on a known call chain; adapter degradation without pyright; map hierarchy snapshot.

**COMMIT** `"phase4: semantic rust adapter, py/ts adapters, revision-aware graph queries"`

## STEP 4.6 — Staleness engine and docs maintenance

1. Documents may embed `{{ symbol:path::to::symbol }}` references ([Chapter 07](../07-code-intelligence.md)); the publisher resolves them against the graph and records the resolved `SymbolKey` + revision in `links_json`.
2. On `CodeGraphUpdated`: diff affected symbols against document links; signature change / disappearance emits a `DocumentStaleness` finding with **evidence** (the graph delta + commit) and a suggested review scope (exit criterion 3).
3. The `Maintain` collaboration mode consumes findings: a maintenance run drafts a suggestion (never a direct edit) on the stale document, citing the code change. Registered as command `/update-docs`.

**TESTS** — rename a symbol in the fixture crate → linked document flagged; the maintenance suggestion cites the causing commit; unlinked docs untouched.

## Exit checklist

- [ ] Two clients edit one document concurrently; both converge; history preserved (exit criterion 1).
- [ ] Publishing the same document revision twice produces byte-identical Markdown (exit criterion 2).
- [ ] Changing a linked symbol's signature flags exactly the affected documents with evidence (exit criterion 3).
- [ ] Any graph edge in the TUI inspector shows relation, confidence, evidence artifact, and revision (exit criterion 4).
- [ ] CRDT decision ADR-016 exists with benchmark numbers.
- [ ] Suggest-by-default enforced for organization-scope docs.
- [ ] `fmt` / `clippy` / `test` green; commits made; tree clean.
