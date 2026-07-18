-- Phase 4 — Collaborative Docs Studio (STEP 4.2).
--
-- Documents are collaborative knowledge objects (Chapter 08). The Loro CRDT
-- snapshot is authoritative for the draft (ADR-004/016) and is stored inline as
-- a BLOB here — the draft's durable home — while *published* Markdown snapshots
-- go to Git (STEP 4.4). `scope_tier`/`scope_key` mirror registry_items/memories
-- so document retrieval is scope-filtered in SQL and never leaks across
-- repositories. Every write also appends an index_outbox row (document_changed)
-- in the SAME transaction, like every other authoritative entity.
CREATE TABLE documents (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    scope_json TEXT NOT NULL,         -- Scope (tagged; may carry an id)
    scope_tier TEXT NOT NULL,         -- system|organization|user|workspace|repository|branch|session|task
    scope_key TEXT,                   -- the scope's entity id string, when it has one
    status TEXT NOT NULL,             -- DocumentStatus: draft|in_review|published|archived
    metadata_json TEXT NOT NULL,      -- DocumentMetadata
    crdt_snapshot BLOB NOT NULL,      -- Loro snapshot (authoritative draft state)
    links_json TEXT NOT NULL,         -- Vec<DocumentLink>
    citations_json TEXT NOT NULL,     -- Vec<Citation>
    revision INTEGER NOT NULL,        -- monotonic per-document revision (bumped per mutation)
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX idx_documents_scope ON documents(scope_tier, scope_key);
CREATE INDEX idx_documents_status ON documents(status);

-- The attribution log (Chapter 08): one row per recorded mutation, so every
-- block change is traceable to a Human or an Agent{run_id, model, policy_version}.
-- A generated sentence is always attributable to its run and evidence.
CREATE TABLE document_authorship (
    id TEXT PRIMARY KEY,
    document_id TEXT NOT NULL REFERENCES documents(id),
    block_id TEXT,                    -- the affected block, when the mutation targets one
    author_json TEXT NOT NULL,        -- DocumentAuthor
    mutation TEXT NOT NULL,           -- insert_block|delete_block|edit_text|set_block|suggest|accept_suggestion|reject_suggestion
    revision INTEGER NOT NULL,        -- the document revision this mutation produced
    at TEXT NOT NULL
);
CREATE INDEX idx_document_authorship_doc ON document_authorship(document_id);

-- Suggestions (STEP 4.3): a proposed replacement over a block range, recorded as
-- data (Suggest collaboration mode / organization-scope default). A suggestion
-- mutates nothing until it is accepted; accept/reject stamps `resolved_*`.
CREATE TABLE document_suggestions (
    id TEXT PRIMARY KEY,
    document_id TEXT NOT NULL REFERENCES documents(id),
    block_id TEXT NOT NULL,           -- the block the suggestion targets
    range_start INTEGER NOT NULL,     -- character offset (inclusive) within the block text
    range_end INTEGER NOT NULL,       -- character offset (exclusive)
    source_revision INTEGER NOT NULL, -- the document revision the suggestion was proposed against; accept refuses if the document has advanced (covers zero-length insertion drift)
    original TEXT NOT NULL,           -- the text the proposer saw at [range_start, range_end); accept refuses if it has drifted
    replacement TEXT NOT NULL,        -- the proposed text for [range_start, range_end)
    author_json TEXT NOT NULL,        -- DocumentAuthor who proposed it
    rationale TEXT,                   -- optional explanation / citation
    status TEXT NOT NULL,             -- pending|accepted|rejected
    created_at TEXT NOT NULL,
    resolved_at TEXT,                 -- when accepted/rejected
    resolved_by_json TEXT             -- DocumentAuthor who resolved it
);
CREATE INDEX idx_document_suggestions_doc ON document_suggestions(document_id, status);

-- Published-snapshot provenance (STEP 4.4): a document revision published to Git
-- records the resulting commit so staleness (STEP 4.6) can compare the live graph
-- against what a published document assumed.
CREATE TABLE document_publications (
    id TEXT PRIMARY KEY,
    document_id TEXT NOT NULL REFERENCES documents(id),
    revision INTEGER NOT NULL,        -- the document revision that was published
    target TEXT NOT NULL,             -- rendered target description (file path / branch / PR)
    git_commit TEXT,                  -- resulting commit-ish, when known
    rendered_hash TEXT NOT NULL,      -- SHA-256 of the deterministic Markdown render
    published_at TEXT NOT NULL
);
CREATE INDEX idx_document_publications_doc ON document_publications(document_id);
