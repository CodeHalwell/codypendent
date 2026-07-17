-- Phase 3 STEP 3.4 — the IDE-context projection.
--
-- An attached IDE pushes its live context (active file, selection, open
-- documents, and unsaved-buffer digests) via `UpdateIdeContext`. It is
-- latest-wins and high-frequency (debounced client-side), so it is stored as a
-- projection keyed by session — NOT appended to the event ledger. A run reads
-- the latest row when it starts, so the read path can label an excerpt whose
-- on-disk bytes diverge from an unsaved editor buffer as `unsaved-ide-buffer`.
CREATE TABLE ide_context (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id),
    update_json TEXT NOT NULL,   -- IdeContextUpdate JSON (latest wins)
    updated_at TEXT NOT NULL
);
