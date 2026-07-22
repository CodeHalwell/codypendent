-- Phase 7 (STEP 7.2): persisted model profiles — the MEASURED inputs the
-- Phase-7 utility router (`codypendent-routing`) reads to route a task node to a
-- model. Chapter 09 is explicit that routing reads measured numbers, never
-- vibes: a `ModelProfile` carries declared `ModelCapabilities`, first-use
-- capability-probe results, observed `ModelPerformance` (reliability, per-task-
-- class success, cost/latency distributions, failure patterns), a
-- `ModelExecutionProfile` (preferred tool count, edit protocol, context layout,
-- reasoning budget, schema-repair policy), and — for a local model — the
-- `codypendent models bench <id>` harness's `LocalBench`.
--
-- Keyed by (model_id, endpoint): the SAME model id served from two endpoints
-- (a local Ollama vs a hosted mirror) is two distinct measured profiles, and
-- STEP 7.2.3 caches capability probes "per model+endpoint" — so the endpoint is
-- part of the key, not an attribute.
--
-- Storage shape mirrors 0015_promotion's `candidate_json` discipline: the whole
-- `ModelProfile` lives in `profile_json` (a serde round-trip, authoritative),
-- and the scalar columns the router filters/scores on HOT are denormalized
-- copies always derived FROM `profile_json` at write time — never written
-- independently (`ModelProfileStore::upsert` is the only writer, and it derives
-- them from the value it is persisting):
--
--   * location               — 'local' | 'hosted'. THE security-relevant column:
--                              the routing hard filter refuses a hosted model for
--                              classified data BEFORE any scoring, so `is_local`
--                              must be answerable without deserializing a row.
--   * context_tokens         — the model's context-window limit (NULL = unbounded
--                              / not advertised). The size hard filter ("does the
--                              task fit?") reads it.
--   * cost_per_1k_tokens_usd — the blended cost the cheapest-above-threshold
--                              selection ranks on.
--   * reliability            — the baseline predicted-success the quality
--                              threshold gates on when a task class has no history.
--
-- `probed_capabilities_json` caches the STEP 7.2.3 first-use capability probe
-- (streaming? tools? parallel tools? structured output?) per model+endpoint;
-- NULL until first probed. It records what the endpoint ACTUALLY advertised,
-- kept beside the declared capabilities in `profile_json` so a probe that denies
-- a declared feature is visible to the router.
--
-- Append-only (migrations never edit an existing file): a fresh DB creates the
-- table; an existing DB gains it empty. Routing is default-OFF, so an empty
-- table changes nothing until a profile is benched/probed and routing enabled.

CREATE TABLE model_profiles (
    model_id TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    -- 'local' | 'hosted' — derived from profile_json's ModelLocation.
    location TEXT NOT NULL,
    -- Context-window limit; NULL means unbounded / not advertised.
    context_tokens INTEGER,
    -- Blended cost per 1K tokens (USD) the router ranks the cheapest model on.
    cost_per_1k_tokens_usd REAL NOT NULL,
    -- Baseline observed success rate [0,1].
    reliability REAL NOT NULL,
    -- The whole serialized `codypendent_routing::ModelProfile` (authoritative).
    profile_json TEXT NOT NULL,
    -- First-use capability-probe result (a serialized `ModelCapabilities`);
    -- NULL until the model+endpoint has been probed once.
    probed_capabilities_json TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (model_id, endpoint)
);

-- The router lists eligible profiles and filters hosted models wholesale for
-- classified data, so locality is the first thing it partitions on.
CREATE INDEX ix_model_profiles_location ON model_profiles (location);
