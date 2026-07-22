-- Phase 7 (STEP 7.5): promotion-pipeline persistence. `codypendent-eval`'s
-- `Candidate`/`PromotionRecord` state machine (draft -> regression -> shadow ->
-- canary -> human-approval -> promote -> rollback) already enforces the
-- no-self-promotion invariant in-memory (`Candidate::approve` requires
-- `Actor::Human`); this schema lets the daemon persist it across restarts
-- WITHOUT adding a back door — `promotion_candidates.candidate_json` is the
-- whole `Candidate` (private fields and all, via serde), so the only way a row
-- ever reaches `stage = 'promoted'` is by round-tripping through the real
-- `Candidate::approve` method and re-serializing its result. `stage` is a
-- denormalized, queryable copy of `candidate_json`'s own stage — always derived
-- FROM the just-mutated `Candidate`, never written independently (mirrors how
-- the Rust type keeps its `stage` field private).

CREATE TABLE promotion_candidates (
    id TEXT PRIMARY KEY,
    artifact_kind TEXT NOT NULL,
    artifact_name TEXT NOT NULL,
    artifact_version INTEGER NOT NULL,
    -- draft | regression-passed | shadow | canary | comparison-ready | promoted
    -- | rolled-back | rejected — kept in sync with candidate_json on every write.
    stage TEXT NOT NULL,
    -- The full serialized `Candidate` (its private fields included); reloaded
    -- and advanced through its own state-machine methods, never hand-edited.
    candidate_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX ix_promotion_candidates_stage ON promotion_candidates (stage);
CREATE INDEX ix_promotion_candidates_artifact
    ON promotion_candidates (artifact_kind, artifact_name);

-- The audit trail: one row per `PromotionRecord` a `Candidate` ever minted
-- (`approve`'s human promotion, a manual `rollback`, or the system-attributed
-- auto-rollback `observe_canary` produces on a canary regression). Attribution
-- (exit criterion 4) lives here: which actor, what stage, why (a rollback's
-- reason), and when.
CREATE TABLE promotion_events (
    id TEXT PRIMARY KEY,
    candidate_id TEXT NOT NULL REFERENCES promotion_candidates(id) ON DELETE CASCADE,
    artifact_kind TEXT NOT NULL,
    artifact_name TEXT NOT NULL,
    artifact_version INTEGER NOT NULL,
    actor_kind TEXT NOT NULL,
    stage TEXT NOT NULL,
    reason TEXT,
    occurred_at TEXT NOT NULL
);

CREATE INDEX ix_promotion_events_candidate ON promotion_events (candidate_id);

-- The active version of each artifact stem (`router/tool-selection`), mirroring
-- `ActiveVersions`' in-memory activation stack: one row per position ever
-- pushed, so a rollback pops the top row and the predecessor is exactly the
-- next-highest position. A version is only ever inserted here as the result of
-- activating a genuine `Promoted` `PromotionRecord` (see `PromotionStore::approve`)
-- — there is no path that inserts one from anything else.
CREATE TABLE promotion_active_versions (
    stem TEXT NOT NULL,
    position INTEGER NOT NULL,
    version INTEGER NOT NULL,
    activated_at TEXT NOT NULL,
    PRIMARY KEY (stem, position)
);
