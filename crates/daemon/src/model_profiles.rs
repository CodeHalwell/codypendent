//! Durable model-profile storage (Phase 7 STEP 7.2 daemon wiring): a
//! [`ModelProfileStore`] over a SQLite pool (migration 0014), mirroring
//! [`crate::promotion`]'s store discipline in shape.
//!
//! The Phase-7 router (`codypendent-routing`) is a daemon-free engine that reads
//! **measured** [`ModelProfile`]s handed to it; nothing there makes a network
//! call or touches a database. This store is the daemon's half: it persists the
//! profiles the `codypendent models bench <id>` harness measures and the
//! first-use capability probes cache, and lists them back for the routing seam.
//!
//! **`profile_json` is authoritative.** Every write serializes the whole
//! [`ModelProfile`] into `profile_json`; the scalar columns the router filters
//! and scores on hot (`location`, `context_tokens`, `cost_per_1k_tokens_usd`,
//! `reliability`) are denormalized copies derived FROM the value being
//! persisted, never written independently — so a read that reconstructs a
//! `ModelProfile` always deserializes `profile_json`, and the columns exist
//! purely to answer `WHERE`/`ORDER BY` without opening every row.

use chrono::Utc;
use codypendent_protocol::ids::ModelId;
use codypendent_routing::{ModelCapabilities, ModelProfile, TaskClass};
use sqlx::{Row, SqlitePool};

/// An error from the model-profile store.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ModelProfileStoreError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    /// A stored row could not be decoded (should never happen; the store wrote it).
    #[error("corrupt model-profile row for {model}@{endpoint}: {detail}")]
    Corrupt {
        model: String,
        endpoint: String,
        detail: String,
    },
}

/// A stored profile plus the endpoint it was measured against — the key the
/// router needs to build a driver for the selected model against the right
/// endpoint, and the key first-use capability probes cache under.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredModelProfile {
    /// The endpoint (base URL) this profile was measured/probed against.
    pub endpoint: String,
    /// The reconstructed measured profile the router consumes.
    pub profile: ModelProfile,
}

/// The durable model-profile store. Stateless; the pool is passed to each method
/// (mirrors [`crate::promotion`] / `codypendent_eval::PromotionStore`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelProfileStore;

impl ModelProfileStore {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Insert or replace the profile for `(profile.id, endpoint)`. The scalar
    /// columns are derived from `profile` here — the only place they are written
    /// — so they can never drift from `profile_json`. A repeat bench/probe of
    /// the same model+endpoint overwrites the prior measurement (and preserves
    /// the row's `created_at`).
    pub async fn upsert(
        &self,
        pool: &SqlitePool,
        endpoint: &str,
        profile: &ModelProfile,
    ) -> Result<(), ModelProfileStoreError> {
        // Re-fold the learned per-task-class rates before writing.
        //
        // `bench_to_store` builds a profile with an EMPTY `task_class_success`,
        // and this upsert replaces `profile_json` wholesale — so re-benchmarking
        // a model silently erased every rate learned from real runs, and routing
        // fell back to overall reliability until each class happened to be
        // observed again. The durable evidence was never lost (the
        // `model_task_outcomes` rows survive); only the snapshot derived from it
        // was. Recomputing here means the snapshot cannot drift from the rows it
        // is a projection of, whatever the caller happened to pass in.
        let mut profile = profile.clone();
        let learned: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT task_class, COALESCE(SUM(success), 0), COUNT(*) \
             FROM model_task_outcomes WHERE model_id = ? AND endpoint = ? \
             GROUP BY task_class",
        )
        .bind(profile.id.0.as_str())
        .bind(endpoint)
        .fetch_all(pool)
        .await?;
        for (task_class, successes, total) in learned {
            if total > 0 {
                profile
                    .performance
                    .task_class_success
                    .insert(task_class, successes as f64 / total as f64);
            }
        }
        let profile = &profile;

        let json = serde_json::to_string(profile)?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO model_profiles \
             (model_id, endpoint, location, context_tokens, cost_per_1k_tokens_usd, reliability, \
              profile_json, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(model_id, endpoint) DO UPDATE SET \
               location = excluded.location, \
               context_tokens = excluded.context_tokens, \
               cost_per_1k_tokens_usd = excluded.cost_per_1k_tokens_usd, \
               reliability = excluded.reliability, \
               profile_json = excluded.profile_json, \
               updated_at = excluded.updated_at",
        )
        .bind(profile.id.0.as_str())
        .bind(endpoint)
        .bind(location_str(profile))
        .bind(profile.capabilities.context_tokens.map(|c| c as i64))
        .bind(profile.performance.cost_per_1k_tokens_usd)
        .bind(profile.performance.reliability)
        .bind(&json)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// The profile for `(model, endpoint)`, or `None` if none was ever stored.
    pub async fn get(
        &self,
        pool: &SqlitePool,
        model: &ModelId,
        endpoint: &str,
    ) -> Result<Option<ModelProfile>, ModelProfileStoreError> {
        let row = sqlx::query(
            "SELECT profile_json FROM model_profiles WHERE model_id = ? AND endpoint = ?",
        )
        .bind(model.0.as_str())
        .bind(endpoint)
        .fetch_optional(pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let profile = decode_profile(&row, &model.0, endpoint)?;
        Ok(Some(profile))
    }

    /// Every stored profile, oldest first — the eligible pool the routing seam
    /// hands to [`codypendent_routing::Router`].
    pub async fn list(
        &self,
        pool: &SqlitePool,
    ) -> Result<Vec<StoredModelProfile>, ModelProfileStoreError> {
        let rows = sqlx::query(
            "SELECT model_id, endpoint, profile_json FROM model_profiles \
             ORDER BY created_at ASC, model_id ASC, endpoint ASC",
        )
        .fetch_all(pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let model_id: String = row.get("model_id");
                let endpoint: String = row.get("endpoint");
                let profile = decode_profile(&row, &model_id, &endpoint)?;
                Ok(StoredModelProfile { endpoint, profile })
            })
            .collect()
    }

    /// The cached first-use capability probe for `(model, endpoint)` (STEP
    /// 7.2.3), or `None` if the model+endpoint has never been probed. The router
    /// probes-and-caches on the first routing decision that considers a model.
    pub async fn cached_capabilities(
        &self,
        pool: &SqlitePool,
        model: &ModelId,
        endpoint: &str,
    ) -> Result<Option<ModelCapabilities>, ModelProfileStoreError> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT probed_capabilities_json FROM model_profiles \
             WHERE model_id = ? AND endpoint = ?",
        )
        .bind(model.0.as_str())
        .bind(endpoint)
        .fetch_optional(pool)
        .await?;
        match row {
            Some((Some(json),)) => Ok(Some(serde_json::from_str(&json)?)),
            _ => Ok(None),
        }
    }

    /// Cache a first-use capability probe for `(model, endpoint)` (STEP 7.2.3).
    /// Requires the profile row to already exist (a model is benched/registered
    /// before it is probed); returns the number of rows updated so a caller can
    /// tell whether the profile was present.
    pub async fn cache_capabilities(
        &self,
        pool: &SqlitePool,
        model: &ModelId,
        endpoint: &str,
        capabilities: &ModelCapabilities,
    ) -> Result<u64, ModelProfileStoreError> {
        let json = serde_json::to_string(capabilities)?;
        let now = Utc::now().to_rfc3339();
        let affected = sqlx::query(
            "UPDATE model_profiles SET probed_capabilities_json = ?, updated_at = ? \
             WHERE model_id = ? AND endpoint = ?",
        )
        .bind(&json)
        .bind(&now)
        .bind(model.0.as_str())
        .bind(endpoint)
        .execute(pool)
        .await?
        .rows_affected();
        Ok(affected)
    }

    /// Record one REAL run's outcome for `(model, endpoint, task_class)` and
    /// immediately fold the recomputed aggregate into
    /// `performance.task_class_success` — outcome 11's missing writer
    /// (migration 0025, `crates/routing/src/profile.rs::ModelPerformance`'s
    /// per-task-class map had exactly one non-test constructor in the
    /// workspace before this, `BenchOutcome::into_profile`, and it always
    /// wrote an empty one). The recompute is exact — a fresh `AVG` over every
    /// `model_task_outcomes` row for this key, not an incrementally-updated
    /// running average — so repeated calls cannot accumulate floating-point
    /// drift.
    ///
    /// Requires the profile row to already exist, mirroring
    /// [`Self::cache_capabilities`]'s precedent (a model is benched/registered
    /// before its per-task-class history can be tracked); returns `Ok(false)`
    /// rather than an error when it does not, so a caller can decide whether
    /// that is worth surfacing (e.g. the daemon logging a warning and moving
    /// on, rather than failing the run it is trying to attribute an already-
    /// terminal outcome to).
    ///
    /// Idempotent per `run_id`: a retried call for the exact same
    /// `(model, endpoint, task_class, run_id)` is a no-op (migration 0025's
    /// `UNIQUE` index) — a crash-and-retry in the caller cannot double-count
    /// the same run.
    pub async fn record_outcome(
        &self,
        pool: &SqlitePool,
        model: &ModelId,
        endpoint: &str,
        task_class: TaskClass,
        success: bool,
        run_id: &str,
    ) -> Result<bool, ModelProfileStoreError> {
        // IMMEDIATE: this reads the profile, then writes an outcome and the folded
        // profile. A deferred transaction would fail with SQLITE_BUSY_SNAPSHOT when
        // another run's outcome committed in between, and the outcome (the routing
        // signal) would be dropped.
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

        let Some(existing_json) = sqlx::query(
            "SELECT profile_json FROM model_profiles WHERE model_id = ? AND endpoint = ?",
        )
        .bind(model.0.as_str())
        .bind(endpoint)
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| row.get::<String, _>("profile_json")) else {
            // No profile row yet: nothing to fold the outcome into. Do not
            // insert an orphaned observation either — a model that is later
            // benched starts its task-class history from that point, exactly
            // like `cache_capabilities` starts probing from that point.
            return Ok(false);
        };
        let mut profile: ModelProfile =
            serde_json::from_str(&existing_json).map_err(|e| ModelProfileStoreError::Corrupt {
                model: model.0.clone(),
                endpoint: endpoint.to_string(),
                detail: e.to_string(),
            })?;

        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO model_task_outcomes \
             (model_id, endpoint, task_class, success, run_id, recorded_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(model_id, endpoint, task_class, run_id) DO NOTHING",
        )
        .bind(model.0.as_str())
        .bind(endpoint)
        .bind(task_class.as_str())
        .bind(i64::from(success))
        .bind(run_id)
        .bind(&now)
        .execute(&mut *tx)
        .await?;

        let (successes, total): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(success), 0), COUNT(*) FROM model_task_outcomes \
             WHERE model_id = ? AND endpoint = ? AND task_class = ?",
        )
        .bind(model.0.as_str())
        .bind(endpoint)
        .bind(task_class.as_str())
        .fetch_one(&mut *tx)
        .await?;
        // total >= 1 always holds here: the INSERT above either added this
        // run's row or a prior call already had, so at least one observation
        // exists for this key by the time this SELECT runs.
        let rate = successes as f64 / total.max(1) as f64;
        profile
            .performance
            .task_class_success
            .insert(task_class.as_str().to_string(), rate);

        let updated_json = serde_json::to_string(&profile)?;
        sqlx::query(
            "UPDATE model_profiles SET profile_json = ?, updated_at = ? \
             WHERE model_id = ? AND endpoint = ?",
        )
        .bind(&updated_json)
        .bind(&now)
        .bind(model.0.as_str())
        .bind(endpoint)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(true)
    }
}

/// The `location` column value derived from a profile (the only writer).
fn location_str(profile: &ModelProfile) -> &'static str {
    if profile.is_local() {
        "local"
    } else {
        "hosted"
    }
}

/// Decode `profile_json` from a row into a [`ModelProfile`], attributing a
/// corrupt row to its key.
fn decode_profile(
    row: &sqlx::sqlite::SqliteRow,
    model_id: &str,
    endpoint: &str,
) -> Result<ModelProfile, ModelProfileStoreError> {
    let json: String = row.get("profile_json");
    serde_json::from_str(&json).map_err(|e| ModelProfileStoreError::Corrupt {
        model: model_id.to_string(),
        endpoint: endpoint.to_string(),
        detail: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_routing::{
        EditProtocol, LocalBench, ModelExecutionProfile, ModelLocation, ModelPerformance,
        SchemaRepairPolicy, StructuredOutputSupport, ToolCallSupport,
    };
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    async fn test_pool(dir: &std::path::Path) -> SqlitePool {
        crate::db::open_database(&dir.join("test.db"))
            .await
            .expect("open database (0014 applies on a fresh DB)")
    }

    /// A rich local profile exercising every JSON-column structure: per-task-class
    /// success, failure patterns, a full execution profile, and a `LocalBench`.
    fn rich_local_profile(id: &str) -> ModelProfile {
        let mut task_class_success = BTreeMap::new();
        task_class_success.insert("small-bug-fix".to_string(), 0.82);
        task_class_success.insert("doc-update".to_string(), 0.91);
        ModelProfile {
            id: ModelId(id.to_string()),
            location: ModelLocation::Local,
            capabilities: ModelCapabilities {
                streaming: true,
                tools: ToolCallSupport::Parallel,
                parallel_tools: true,
                structured_output: StructuredOutputSupport::JsonMode,
                vision: false,
                audio_input: false,
                embeddings: false,
                prompt_caching: false,
                reasoning_controls: false,
                context_tokens: Some(128_000),
                output_tokens: Some(8_192),
            },
            performance: ModelPerformance {
                reliability: 0.76,
                cost_per_1k_tokens_usd: 0.0,
                latency_ms_p50: 1_500.0,
                task_class_success,
                failure_patterns: vec!["schema-drift".to_string(), "tool-loop".to_string()],
            },
            execution: ModelExecutionProfile {
                preferred_tool_count: 6,
                edit_protocol: EditProtocol::WholeFile,
                context_layout: "system-history-context".to_string(),
                reasoning_budget: Some(2_048),
                schema_repair: SchemaRepairPolicy::LocalRepair,
            },
            bench: Some(LocalBench {
                tokens_per_second: 42.5,
                time_to_first_token_ms: 180.0,
                warmup_ms: 640.0,
                memory_mb: 9_200,
                context_limit: 128_000,
                structured_output_reliability: 0.79,
                tool_call_accuracy: 0.74,
                coding_eval_score: 0.61,
            }),
        }
    }

    #[tokio::test]
    async fn round_trips_a_rich_profile() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let store = ModelProfileStore::new();
        let profile = rich_local_profile("qwen-local");
        let endpoint = "http://localhost:11434/v1";

        store.upsert(&pool, endpoint, &profile).await.unwrap();
        let got = store
            .get(&pool, &ModelId("qwen-local".into()), endpoint)
            .await
            .unwrap()
            .expect("profile round-trips");
        assert_eq!(got, profile, "the whole ModelProfile survives round-trip");

        // The denormalized scalar columns are derived from the profile.
        let (location, ctx, cost): (String, Option<i64>, f64) = sqlx::query_as(
            "SELECT location, context_tokens, cost_per_1k_tokens_usd FROM model_profiles \
             WHERE model_id = ?",
        )
        .bind("qwen-local")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(location, "local");
        assert_eq!(ctx, Some(128_000));
        assert!((cost - 0.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn migration_0014_applies_on_a_fresh_and_a_reopened_database() {
        // Fresh DB: `open_database` runs every migration through the head
        // (0001..0015, including 0014), so the table exists and a write succeeds.
        let dir = tempdir().unwrap();
        let db = dir.path().join("existing.db");
        let store = ModelProfileStore::new();
        {
            let pool = crate::db::open_database(&db).await.expect("fresh migrate");
            store
                .upsert(&pool, "http://localhost:11434/v1", &rich_local_profile("m"))
                .await
                .expect("write on the freshly-migrated table");
            pool.close().await;
        }
        // Reopen the SAME file: the migrator sees 0014 (and every other version)
        // already applied and is a clean no-op — the daemon restarts cleanly and
        // the prior row is still readable.
        let pool = crate::db::open_database(&db).await.expect("reopen migrate");
        assert!(store
            .get(&pool, &ModelId("m".into()), "http://localhost:11434/v1")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn upsert_overwrites_and_list_returns_all() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let store = ModelProfileStore::new();

        let mut hosted = rich_local_profile("hosted-strong");
        hosted.location = ModelLocation::Hosted;
        hosted.performance.cost_per_1k_tokens_usd = 0.03;
        store
            .upsert(&pool, "https://api.example.com/v1", &hosted)
            .await
            .unwrap();
        store
            .upsert(
                &pool,
                "http://localhost:11434/v1",
                &rich_local_profile("local"),
            )
            .await
            .unwrap();

        // Re-bench the hosted model with a better reliability: upsert overwrites.
        hosted.performance.reliability = 0.93;
        store
            .upsert(&pool, "https://api.example.com/v1", &hosted)
            .await
            .unwrap();

        let all = store.list(&pool).await.unwrap();
        assert_eq!(
            all.len(),
            2,
            "one row per (model,endpoint), overwrite not add"
        );
        let strong = all
            .iter()
            .find(|p| p.profile.id == ModelId("hosted-strong".into()))
            .unwrap();
        assert!((strong.profile.performance.reliability - 0.93).abs() < 1e-9);
        assert_eq!(strong.endpoint, "https://api.example.com/v1");
    }

    #[tokio::test]
    async fn same_model_id_two_endpoints_are_distinct_rows() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let store = ModelProfileStore::new();
        let profile = rich_local_profile("shared-id");

        store
            .upsert(&pool, "http://localhost:11434/v1", &profile)
            .await
            .unwrap();
        store
            .upsert(&pool, "http://localhost:8080/v1", &profile)
            .await
            .unwrap();

        assert_eq!(store.list(&pool).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn capability_probe_cache_round_trips_and_starts_absent() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let store = ModelProfileStore::new();
        let endpoint = "http://localhost:11434/v1";
        let model = ModelId("qwen-local".into());
        store
            .upsert(&pool, endpoint, &rich_local_profile("qwen-local"))
            .await
            .unwrap();

        // No probe cached yet (first-use is absent).
        assert!(store
            .cached_capabilities(&pool, &model, endpoint)
            .await
            .unwrap()
            .is_none());

        let probed = ModelCapabilities {
            streaming: true,
            tools: ToolCallSupport::Single,
            parallel_tools: false,
            structured_output: StructuredOutputSupport::None,
            vision: false,
            audio_input: false,
            embeddings: false,
            prompt_caching: false,
            reasoning_controls: false,
            context_tokens: Some(128_000),
            output_tokens: Some(8_192),
        };
        let affected = store
            .cache_capabilities(&pool, &model, endpoint, &probed)
            .await
            .unwrap();
        assert_eq!(affected, 1, "the existing profile row is updated");
        assert_eq!(
            store
                .cached_capabilities(&pool, &model, endpoint)
                .await
                .unwrap(),
            Some(probed)
        );
    }

    /// Outcome 11's headline fix, end to end: `predicted_success` for a task
    /// class starts as the overall `reliability` (no history), then diverges
    /// once real run outcomes are recorded — proving `task_class_success` is
    /// no longer permanently the empty map every non-test path used to write.
    /// A re-benchmark must not erase what real runs taught the router.
    ///
    /// `bench_to_store` builds a profile whose `task_class_success` is empty and
    /// `upsert` replaces `profile_json` wholesale, so `models bench` on an
    /// already-measured model wiped every learned rate — routing fell back to
    /// overall reliability even though the `model_task_outcomes` rows were still
    /// sitting there. Outcome 11 is "route from measured outcomes"; re-measuring
    /// one dimension must not discard another.
    #[tokio::test]
    async fn re_benchmarking_preserves_rates_learned_from_real_runs() {
        use codypendent_routing::TaskClass;

        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let store = ModelProfileStore::new();
        let endpoint = "http://localhost:11434/v1";
        let model = ModelId("qwen-local".into());

        store
            .upsert(&pool, endpoint, &rich_local_profile("qwen-local"))
            .await
            .unwrap();
        for (run, ok) in [("run-1", true), ("run-2", true), ("run-3", false)] {
            store
                .record_outcome(&pool, &model, endpoint, TaskClass::SafeRefactor, ok, run)
                .await
                .unwrap();
        }
        let learned = store
            .get(&pool, &model, endpoint)
            .await
            .unwrap()
            .unwrap()
            .performance
            .predicted_success(TaskClass::SafeRefactor);
        assert!(
            (learned - (2.0 / 3.0)).abs() < 1e-9,
            "precondition: 2/3 learned"
        );

        // Exactly what `models bench` does on a second run: a freshly measured
        // profile carrying no task-class history at all.
        let mut rebenched = rich_local_profile("qwen-local");
        rebenched.performance.task_class_success.clear();
        store.upsert(&pool, endpoint, &rebenched).await.unwrap();

        let after = store
            .get(&pool, &model, endpoint)
            .await
            .unwrap()
            .unwrap()
            .performance
            .predicted_success(TaskClass::SafeRefactor);
        assert!(
            (after - (2.0 / 3.0)).abs() < 1e-9,
            "a re-benchmark must re-fold the durable outcome rows, got {after}"
        );
    }

    #[tokio::test]
    async fn record_outcome_folds_real_run_results_into_task_class_success() {
        use codypendent_routing::TaskClass;

        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let store = ModelProfileStore::new();
        let endpoint = "http://localhost:11434/v1";
        let model = ModelId("qwen-local".into());
        // reliability = 0.76 (see rich_local_profile), no doc-update history yet.
        store
            .upsert(&pool, endpoint, &rich_local_profile("qwen-local"))
            .await
            .unwrap();

        let before = store.get(&pool, &model, endpoint).await.unwrap().unwrap();
        assert_eq!(
            before
                .performance
                .predicted_success(TaskClass::SafeRefactor),
            0.76,
            "no history yet: falls back to overall reliability"
        );

        // Two real runs succeed, one fails, at SafeRefactor -> 2/3.
        for (run, ok) in [("run-1", true), ("run-2", true), ("run-3", false)] {
            let recorded = store
                .record_outcome(&pool, &model, endpoint, TaskClass::SafeRefactor, ok, run)
                .await
                .unwrap();
            assert!(recorded, "a profile row exists, so the outcome is recorded");
        }

        let after = store.get(&pool, &model, endpoint).await.unwrap().unwrap();
        let rate = after.performance.predicted_success(TaskClass::SafeRefactor);
        assert!(
            (rate - (2.0 / 3.0)).abs() < 1e-9,
            "expected 2/3 from three real observations, got {rate}"
        );
        // A DIFFERENT task class this model has never run still falls back —
        // recording one class's history must not contaminate another's.
        assert_eq!(
            after.performance.predicted_success(TaskClass::DocUpdate),
            0.91,
            "unrelated class keeps its own history (from rich_local_profile), untouched"
        );
        // Nothing else in the profile was disturbed by the fold.
        assert_eq!(
            after.performance.reliability,
            before.performance.reliability
        );
        assert_eq!(after.capabilities, before.capabilities);
        assert_eq!(after.bench, before.bench);
    }

    #[tokio::test]
    async fn record_outcome_is_a_no_op_without_an_existing_profile() {
        use codypendent_routing::TaskClass;

        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let store = ModelProfileStore::new();
        let model = ModelId("never-benched".into());

        let recorded = store
            .record_outcome(
                &pool,
                &model,
                "http://localhost:11434/v1",
                TaskClass::SmallBugFix,
                true,
                "run-1",
            )
            .await
            .unwrap();
        assert!(
            !recorded,
            "no profile row exists yet, so there is nothing to fold the outcome into"
        );
        // And no orphaned raw observation was left behind either.
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM model_task_outcomes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn record_outcome_is_idempotent_per_run_id() {
        use codypendent_routing::TaskClass;

        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let store = ModelProfileStore::new();
        let endpoint = "http://localhost:11434/v1";
        let model = ModelId("qwen-local".into());
        store
            .upsert(&pool, endpoint, &rich_local_profile("qwen-local"))
            .await
            .unwrap();

        // The SAME run_id recorded twice (a crash-and-retry) must count once.
        for _ in 0..2 {
            store
                .record_outcome(
                    &pool,
                    &model,
                    endpoint,
                    TaskClass::CiDiagnosis,
                    true,
                    "run-retried",
                )
                .await
                .unwrap();
        }
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM model_task_outcomes WHERE run_id = 'run-retried'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            count, 1,
            "a retried call for the same run_id must not double-insert"
        );

        let after = store.get(&pool, &model, endpoint).await.unwrap().unwrap();
        assert_eq!(
            after.performance.predicted_success(TaskClass::CiDiagnosis),
            1.0,
            "one successful observation, not two, so the rate is 1.0 not diluted"
        );
    }
}
