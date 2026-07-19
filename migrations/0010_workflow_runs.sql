-- Phase 5 (STEP 5.2): durable workflow execution — runs, node records, and
-- checkpoints — plus the blackboard table (its store lands in STEP 5.3). A
-- workflow run stores the compiled graph's signature so resume can refuse a
-- changed graph; each node record tracks its lifecycle state, attempt count, and
-- cost for node-level provenance (exit criterion 3).

CREATE TABLE workflow_runs (
    id TEXT PRIMARY KEY,
    workflow_id TEXT NOT NULL,
    workflow_version INTEGER NOT NULL,
    -- Hash of the compiled graph; resume refuses a run whose graph has changed.
    graph_signature TEXT NOT NULL,
    -- The session run this workflow drives, when bound (a logical link, not an FK,
    -- so workflow storage is self-contained).
    run_id TEXT,
    inputs_json TEXT NOT NULL,
    state TEXT NOT NULL, -- pending | running | paused | completed | failed | cancelled
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE workflow_nodes (
    id TEXT PRIMARY KEY,
    workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id) ON DELETE CASCADE,
    node_id TEXT NOT NULL,
    state TEXT NOT NULL, -- pending | running | waiting_approval | blocked | completed | failed | skipped
    agent_run_id TEXT,
    attempt INTEGER NOT NULL DEFAULT 0,
    cost_json TEXT,
    topo_order INTEGER NOT NULL,
    started_at TEXT,
    ended_at TEXT,
    UNIQUE (workflow_run_id, node_id)
);

CREATE TABLE workflow_checkpoints (
    id TEXT PRIMARY KEY,
    workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id) ON DELETE CASCADE,
    graph_signature TEXT NOT NULL,
    state_artifact_id TEXT,
    created_at TEXT NOT NULL
);

-- Created here (migrations are append-only and STEP 5.3 needs it); the typed
-- blackboard store is implemented in STEP 5.3.
CREATE TABLE blackboard_items (
    id TEXT PRIMARY KEY,
    workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    author_json TEXT NOT NULL,
    confidence REAL,
    evidence_json TEXT,
    revision INTEGER NOT NULL DEFAULT 1,
    superseded_by TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX ix_workflow_nodes_run ON workflow_nodes (workflow_run_id);
CREATE INDEX ix_workflow_checkpoints_run ON workflow_checkpoints (workflow_run_id);
CREATE INDEX ix_blackboard_items_run ON blackboard_items (workflow_run_id);
