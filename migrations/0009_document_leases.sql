-- Phase 4 (STEP 4.3 transport): block-range edit leases for collaborative
-- documents. One active writer per (document_id, block_id); a whole-document
-- lease (block_id IS NULL) covers structural edits and conflicts with any block
-- lease on that document. Readers take no lease and are unlimited. Leases carry
-- an expiry; an expired lease is reclaimed lazily on the next acquire/require, so
-- a crashed holder never blocks the document forever (mirrors the Phase-1
-- workspace-lease reconciliation approach).
CREATE TABLE document_leases (
    id TEXT PRIMARY KEY,
    document_id TEXT NOT NULL,
    -- NULL = a whole-document structural lease (block insert/delete/reorder).
    block_id TEXT,
    -- The DocumentAuthor holding the lease (full record, for display/provenance).
    holder_json TEXT NOT NULL,
    -- A stable identity string for the holder, so a re-acquire by the same
    -- writer renews rather than conflicts (human:<user>, agent:<run>, …).
    holder_key TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'active', -- active | released
    acquired_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT
);

-- Conflict detection scans a document's active leases; index that path.
CREATE INDEX ix_document_leases_active
    ON document_leases (document_id)
    WHERE state = 'active';
