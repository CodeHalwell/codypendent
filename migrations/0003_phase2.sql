-- Phase 2 — Skills and Knowledge (STEP 2.1).
--
-- The governed registry, the memory fabric, and the syntax-layer code graph.
-- Every write to registry_items / memories / code_* also inserts an
-- index_outbox row in the SAME transaction; indexer workers consume the outbox
-- to update Tantivy / vector / derived indexes, so an indexer crash can never
-- corrupt these authoritative rows (Chapter 06 index-outbox pattern).

-- The governed registry of tools, skills, plugins, hooks, and commands
-- (Chapter 05 RegistryItem). JSON-valued columns carry the list/struct fields;
-- `scope_tier`/`scope_key` are the flattened, SQL-filterable projection of the
-- (possibly id-bearing) `scope_json` so retrieval can scope-filter in SQL.
CREATE TABLE registry_items (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,               -- RegistryItemKind: tool|skill|plugin|hook|command
    name TEXT NOT NULL,
    version TEXT NOT NULL,            -- semver string
    scope_json TEXT NOT NULL,         -- Scope (tagged; may carry an id)
    scope_tier TEXT NOT NULL,         -- system|organization|user|workspace|repository|branch|session|task
    scope_key TEXT,                   -- the scope's entity id string, when it has one
    description TEXT NOT NULL,
    intents_json TEXT NOT NULL,       -- Vec<String>
    keywords_json TEXT NOT NULL,      -- Vec<String>
    examples_json TEXT NOT NULL,      -- Vec<UsageExample>
    input_schema_json TEXT,           -- Option<JsonSchema>
    output_schema_json TEXT,          -- Option<JsonSchema>
    dependencies_json TEXT NOT NULL,  -- Vec<RegistryDependency>
    permissions_json TEXT NOT NULL,   -- Vec<CapabilityRequest>
    risk TEXT NOT NULL,               -- RiskClass
    provenance_json TEXT NOT NULL,    -- Provenance
    trust_json TEXT NOT NULL,         -- TrustMetadata
    trust_tier TEXT NOT NULL,         -- untrusted|community|verified|first_party
    content_hash TEXT NOT NULL,       -- hash over the item's package files
    status TEXT NOT NULL,             -- draft|active|modified|deprecated
    executable INTEGER NOT NULL,      -- 0 when the item depends on scripts not yet runnable (Phase 6)
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX idx_registry_scope ON registry_items(scope_tier, scope_key);
CREATE INDEX idx_registry_kind ON registry_items(kind);
-- One live item per (name, scope) — a workspace skill and a user skill of the
-- same name are distinct rows (both visible); shadowing is resolved in code.
CREATE UNIQUE INDEX idx_registry_identity ON registry_items(kind, name, scope_tier, scope_key);

-- The memory ledger (Chapter 06 MemoryRecord). A newer observation never
-- deletes an older one — it supersedes it (supersedes_json + valid_from/until).
-- `scope_tier`/`scope_key` mirror registry_items so cross-repository isolation
-- is enforced by a SQL filter, never inferred.
CREATE TABLE memories (
    id TEXT PRIMARY KEY,
    class TEXT NOT NULL,              -- MemoryClass
    scope_json TEXT NOT NULL,         -- Scope
    scope_tier TEXT NOT NULL,
    scope_key TEXT,
    statement TEXT NOT NULL,
    structured_value_json TEXT,       -- Option<serde_json::Value>
    provenance_json TEXT NOT NULL,    -- Vec<EvidenceRef> (>= 1 required)
    confidence REAL NOT NULL,
    observed_at TEXT NOT NULL,
    valid_from TEXT NOT NULL,         -- Revision string
    valid_until TEXT,                 -- Revision string; NULL = still valid
    supersedes_json TEXT NOT NULL,    -- Vec<MemoryId>
    sensitivity TEXT NOT NULL,        -- DataClassification
    retention_json TEXT NOT NULL,     -- RetentionPolicy
    embedding_hash TEXT,              -- content hash of the embedded text (dedup cache key)
    created_at TEXT NOT NULL
);
CREATE INDEX idx_memories_scope ON memories(scope_tier, scope_key);
CREATE INDEX idx_memories_class ON memories(class);

-- The syntax-layer code graph (Chapter 07). Durable nodes are the important
-- symbols only (public APIs, functions, types, tests) — never local variables.
-- `symbol_key` is the stable identity that survives line movement.
CREATE TABLE code_nodes (
    id TEXT PRIMARY KEY,
    repository TEXT NOT NULL,         -- RepositoryId
    language TEXT NOT NULL,           -- LanguageId
    package TEXT,                     -- Option<String>
    qualified_name TEXT NOT NULL,
    kind TEXT NOT NULL,              -- CodeNodeKind
    signature_hash TEXT,             -- Option<ContentHash>
    symbol_key TEXT NOT NULL,         -- stable composite identity string
    revision TEXT NOT NULL,           -- GitRevision the node was last seen at
    created_at TEXT NOT NULL
);
CREATE INDEX idx_code_nodes_repo ON code_nodes(repository);
CREATE UNIQUE INDEX idx_code_nodes_identity ON code_nodes(repository, symbol_key);

-- Evidence-backed edges (Chapter 07 CodeEdge). Every edge carries its evidence
-- kind + artifact and a confidence (syntax-inferred calls = 0.45).
CREATE TABLE code_edges (
    id TEXT PRIMARY KEY,
    from_node TEXT NOT NULL REFERENCES code_nodes(id),
    to_node TEXT NOT NULL REFERENCES code_nodes(id),
    relation TEXT NOT NULL,           -- CodeRelation
    confidence REAL NOT NULL,
    evidence_kind TEXT NOT NULL,      -- EvidenceKind
    evidence_artifact TEXT,           -- Option<ArtifactRef JSON> (file + byte range)
    revision TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX idx_code_edges_from ON code_edges(from_node);
CREATE INDEX idx_code_edges_to ON code_edges(to_node);

-- The index-outbox: every authoritative write appends one row here in the same
-- transaction. Indexer workers claim unprocessed rows (processed_at IS NULL),
-- update their derived index, then stamp processed_at. Deleting the derived
-- indexes and replaying the authority is exactly `codypendent index rebuild`.
CREATE TABLE index_outbox (
    id TEXT PRIMARY KEY,
    event_kind TEXT NOT NULL,         -- registry_item_changed|memory_changed|symbol_changed|document_changed|artifact_created
    entity_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    processed_at TEXT                 -- NULL until an indexer consumes it
);
CREATE INDEX idx_index_outbox_unprocessed ON index_outbox(processed_at) WHERE processed_at IS NULL;
