-- Phase 0 authoritative store.
-- SQLite in WAL mode is the local metadata and event authority (ADR-003).
-- Every later phase adds tables through new numbered migrations; existing
-- migrations are never edited after they have been committed.

CREATE TABLE daemon_instance (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    instance_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    boot_count INTEGER NOT NULL DEFAULT 0,
    last_started_at TEXT
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    workspace_id TEXT,
    title TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'open',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0
);

-- The append-only event ledger. `sequence` is monotonic per session and the
-- pair (session_id, sequence) is the durable ordering authority (invariant 5).
CREATE TABLE events (
    session_id TEXT NOT NULL REFERENCES sessions(id),
    sequence INTEGER NOT NULL,
    occurred_at TEXT NOT NULL,
    actor TEXT NOT NULL,
    body TEXT NOT NULL,
    causation_id TEXT,
    correlation_id TEXT,
    schema_version INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (session_id, sequence)
);
