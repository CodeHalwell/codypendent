# Memory and Knowledge Fabric

## Design principle

Memory is an always-on service, not a tool that the model may forget to call.

Every run emits events into a memory-observation pipeline:

```text
event stream
    ↓
candidate extraction
    ↓
secret and sensitivity filtering
    ↓
scope classification
    ↓
deduplication and contradiction detection
    ↓
provenance attachment
    ↓
retention decision
    ↓
memory ledger and indexes
```

The model may explicitly propose a memory, but the curator decides whether it becomes durable.

## Memory classes

| Class | Purpose |
|---|---|
| Working | current run state and recent observations |
| Episodic | what happened during a previous run |
| Semantic | stable facts about user, organization, or repository |
| Procedural | repeatable successful process |
| Preference | user choices and interface behaviour |
| Failure | failed approaches and causes |
| Artifact | important plans, patches, outputs, and documents |
| Code | symbol, architecture, and dependency summaries |

## Scope

```rust
pub enum Scope {
    System,
    Organization(OrganizationId),
    User(UserId),
    Workspace(WorkspaceId),
    Repository(RepositoryId),
    Branch(BranchId),
    Session(SessionId),
    Task(TaskId),
}
```

Cross-repository memory must never be inferred merely because two repositories share a language.

## Memory record

```rust
pub struct MemoryRecord {
    pub id: MemoryId,
    pub class: MemoryClass,
    pub scope: Scope,
    pub statement: String,
    pub structured_value: Option<serde_json::Value>,
    pub provenance: Vec<EvidenceRef>,
    pub confidence: f32,
    pub observed_at: DateTime<Utc>,
    pub valid_from: Revision,
    pub valid_until: Option<Revision>,
    pub supersedes: Vec<MemoryId>,
    pub sensitivity: DataClassification,
    pub retention: RetentionPolicy,
}
```

## Physical architecture

### Transactional store

SQLite initially stores:

- entity metadata;
- memories;
- graph nodes and edges;
- provenance;
- lifecycle and supersession;
- index outbox.

### Full-text

Tantivy stores BM25 and fielded indexes for:

- documents;
- skills;
- tool definitions;
- memories;
- symbols;
- traces.

### Vector search

The vector layer supports:

- memory embeddings;
- tool/skill embeddings;
- code summaries;
- document chunks;
- query and candidate embeddings.

It should be abstracted so an embedded local index or Qdrant can be selected.

### Exact live search

ripgrep provides immediate search over files before indexes catch up and remains valuable for identifiers, error text, and generated output.

### Artifact store

Large source material is stored by hash. Index entries reference exact artifacts and ranges.

## Retrieval pipeline

```text
query and task context
├── exact/grep
├── BM25
├── dense retrieval
├── temporal retrieval
├── scope-aware history
└── graph seed extraction
        ↓
reciprocal-rank fusion
        ↓
1–2 hop graph expansion
        ↓
cross-encoder or model reranking
        ↓
contradiction and duplicate handling
        ↓
context-budget packing
```

## Provenance

Every returned item should be displayable as:

```text
Fact: This repository requires Rust nightly.
Source: rust-toolchain.toml
Revision: 79acbf1
Observed: 2026-07-14
Scope: repository
Confidence: 1.0
```

The TUI should let the user open the source directly.

## Contradiction and supersession

A newer observation does not delete an older one. It supersedes it:

```text
Memory A: test command is `cargo test`
Memory B: test command is `cargo nextest run`

B supersedes A from commit X because repository documentation changed.
```

Queries use the valid record for the requested revision.

## Unified knowledge fabric

“Unified” is a logical property, not a mandate for one graph database.

Entities include:

- users and organizations;
- repositories, commits, branches, and worktrees;
- files and symbols;
- documents;
- skills, tools, and plugins;
- sessions, runs, tasks, and agents;
- model profiles;
- GitHub objects;
- artifacts and evaluations.

Relationships remain evidence-backed and rebuildable.

## Index update outbox

Changes are written transactionally with an outbox entry:

```rust
pub enum KnowledgeIndexEvent {
    MemoryChanged(MemoryId),
    DocumentChanged(DocumentId),
    SymbolChanged(SymbolId),
    RegistryItemChanged(RegistryItemId),
    ArtifactCreated(ArtifactId),
}
```

Indexer workers consume the outbox and update derived indexes. Failures do not corrupt authoritative state.

## Forgetting and deletion

The system must support:

- user deletion;
- scope deletion;
- retention expiry;
- cryptographic erasure of encrypted artifacts;
- index tombstones;
- export before deletion;
- audit records that do not retain deleted sensitive content.

## Threads, chronicles, and related repositories

A conversational thread is a projection over session events. The chronicle provides a durable structured summary suitable for retrieval and compaction.

Related-repository context requires explicit federation configuration, provenance and model data-policy checks. The system never searches neighboring private repositories merely because credentials make them reachable.
