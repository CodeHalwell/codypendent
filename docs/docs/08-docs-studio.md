# Collaborative Docs Studio

## Purpose

The Docs Studio is a local-first knowledge workspace for:

- company policies;
- engineering standards;
- architecture decisions;
- runbooks;
- product knowledge;
- personal notes;
- repository documentation;
- learning material;
- skill references.

Human and agent collaborate in the same document with attribution, suggestions, comments, and version history.

## Working and published forms

```text
CRDT working document
├── concurrent edits
├── suggestions
├── comments
├── presence
└── agent attribution
        ↓ review
Git-backed Markdown/MDX snapshot
        ↓
documentation site, repository, or export
```

Git is the publication and review system. It is not the live collaboration algorithm.

## CRDT selection

Loro is a strong candidate because it is Rust-native and supports incremental updates, rich text, and history. It should still be benchmarked against Automerge and Yrs using Codypendent-specific documents.

Required benchmarks:

- 1 KB, 100 KB, and 10 MB documents;
- single and concurrent writers;
- long edit histories;
- large paste/delete operations;
- annotations and comments;
- snapshot load;
- incremental catch-up;
- post-compaction memory;
- cross-language interoperability.

## Document model

```rust
pub struct KnowledgeDocument {
    pub id: DocumentId,
    pub title: String,
    pub scope: Scope,
    pub status: DocumentStatus,
    pub metadata: DocumentMetadata,
    pub blocks: Vec<DocumentBlock>,
    pub links: Vec<DocumentLink>,
    pub citations: Vec<Citation>,
    pub authorship: Vec<AuthorshipRecord>,
}
```

Block types:

```rust
pub enum DocumentBlock {
    Heading,
    Paragraph,
    Code,
    Diagram,
    Table,
    Callout,
    Checklist,
    Query,
    EmbeddedFile,
    EmbeddedSymbol,
    EmbeddedWorkflow,
    EmbeddedSkill,
}
```

The CRDT representation may differ internally, but export/import must preserve a stable structured model.

## Collaboration modes

| Mode | Agent behaviour |
|---|---|
| Ask | answer without editing |
| Suggest | create proposed changes |
| Edit | apply changes under the active approval policy |
| Co-author | continuously propose edits |
| Review | add comments and findings |
| Maintain | detect staleness and propose updates |

The default for organization documentation is **Suggest**.

## Attribution

```rust
pub enum DocumentAuthor {
    Human(UserId),
    Agent {
        run_id: RunId,
        model: ModelId,
        policy_version: PolicyVersion,
    },
    Integration(IntegrationId),
}
```

A generated sentence should be traceable to the run and supporting evidence.

## Knowledge graph links

```text
Payment Runbook
├── REFERENCES → charge_customer
├── REFERENCES → payments-ci workflow
├── OWNED_BY → Platform Team
├── IMPLEMENTS → Retry Policy
├── USED_BY → investigate-payment-failure skill
└── SUPERSEDES → Payment Runbook v1
```

## Staleness engine

A document can become stale because:

- a referenced symbol changed;
- an endpoint disappeared;
- a policy was superseded;
- a dependency version changed;
- an owning team changed;
- a runbook failed during an incident;
- a linked workflow changed.

The maintenance engine emits a finding with evidence and suggested scope of review.

## TUI experience

```text
┌─ Documents ─────────┬─ Editor / Preview ─────────────┬─ Review ────────┐
│ Architecture        │ # Payment Service              │ 3 suggestions   │
│ Runbooks            │                                │ 1 stale link    │
│ Policies            │ {{ symbol:payments::charge }}  │ citations       │
│ Personal            │                                │                 │
├─────────────────────┴────────────────────────────────┴─────────────────┤
│ Ask, edit, review, cite, link symbol, publish…                          │
└─────────────────────────────────────────────────────────────────────────┘
```

Mouse interaction and keyboard commands are equivalent.

## Publishing

Publishing may:

- update a repository file;
- create a commit;
- open a documentation pull request;
- export a bundle;
- publish to an internal docs site;
- update a Notion integration later.

Every publish operation displays the target, changed files, and resulting Git action.
