CREATE TABLE session_search_sources_next (
    session_id TEXT NOT NULL REFERENCES sessions(id),
    source_type TEXT NOT NULL CHECK (
        source_type IN ('title', 'transcript', 'reasoning', 'tool', 'patch', 'artifact', 'path', 'symbol')
    ),
    source_id TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    indexed_at TEXT NOT NULL,
    event_sequence INTEGER,
    run_id TEXT REFERENCES runs(id),
    artifact_id TEXT REFERENCES artifacts(id),
    PRIMARY KEY (session_id, source_type, source_id),
    FOREIGN KEY (session_id, event_sequence)
        REFERENCES events(session_id, sequence)
);

INSERT INTO session_search_sources_next
    (session_id, source_type, source_id, content_hash, indexed_at, event_sequence, run_id, artifact_id)
SELECT session_id, source_type, source_id, content_hash, indexed_at, event_sequence, run_id, artifact_id
FROM session_search_sources;

DROP TABLE session_search_sources;
ALTER TABLE session_search_sources_next RENAME TO session_search_sources;

CREATE INDEX idx_session_search_sources_event
    ON session_search_sources (session_id, event_sequence)
    WHERE event_sequence IS NOT NULL;
CREATE INDEX idx_session_search_sources_run
    ON session_search_sources (run_id) WHERE run_id IS NOT NULL;
CREATE INDEX idx_session_search_sources_artifact
    ON session_search_sources (artifact_id) WHERE artifact_id IS NOT NULL;
