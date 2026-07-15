CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    objective TEXT NOT NULL,
    state TEXT NOT NULL,              -- RunState as string
    mode TEXT NOT NULL,               -- AgentMode as string
    model_policy TEXT NOT NULL,
    workspace_lease_id TEXT,
    budget_json TEXT NOT NULL,
    started_at TEXT,
    ended_at TEXT
);
CREATE INDEX idx_runs_session ON runs(session_id);

CREATE TABLE commands (
    id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    session_id TEXT,
    client_id TEXT NOT NULL,
    body TEXT NOT NULL,               -- CommandBody JSON
    status TEXT NOT NULL,             -- received | applied | rejected
    result_json TEXT,
    received_at TEXT NOT NULL,
    applied_at TEXT
);

CREATE TABLE pending_effects (
    id TEXT PRIMARY KEY,
    command_id TEXT NOT NULL REFERENCES commands(id),
    kind TEXT NOT NULL,               -- e.g. shell, git-commit, file-write
    intent_json TEXT NOT NULL,
    state TEXT NOT NULL,              -- intended | performed | reconciled | abandoned
    created_at TEXT NOT NULL,
    resolved_at TEXT
);

CREATE TABLE approvals (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    action_json TEXT NOT NULL,        -- ProposedAction
    risk_json TEXT NOT NULL,
    capabilities_json TEXT NOT NULL,
    state TEXT NOT NULL,              -- pending | approved | rejected | expired
    scope TEXT NOT NULL,              -- once | run | pattern | repository
    resolved_by TEXT,
    requested_at TEXT NOT NULL,
    resolved_at TEXT,
    expires_at TEXT
);

-- Artifact ROWS are per-occurrence metadata (id, classification, provenance);
-- only the underlying BLOB (the file, keyed by sha256) is deduplicated. Two
-- occurrences of identical bytes with different sources/classifications are
-- two rows sharing one blob — never one row (see STEP 1.4).
CREATE TABLE artifacts (
    id TEXT PRIMARY KEY,
    sha256 TEXT NOT NULL,
    media_type TEXT NOT NULL,
    byte_length INTEGER NOT NULL,
    classification TEXT NOT NULL,     -- DataClassification
    created_at TEXT NOT NULL,
    provenance_json TEXT NOT NULL
);
CREATE INDEX idx_artifacts_hash ON artifacts(sha256);

CREATE TABLE workspace_leases (
    id TEXT PRIMARY KEY,
    repository_path TEXT NOT NULL,
    worktree_path TEXT NOT NULL UNIQUE,
    branch TEXT NOT NULL,
    base_commit TEXT NOT NULL,
    owner_run_id TEXT NOT NULL REFERENCES runs(id),
    mode TEXT NOT NULL,               -- write | read
    state TEXT NOT NULL,              -- active | released | orphaned
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT
);
