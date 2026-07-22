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
use codypendent_routing::{ModelCapabilities, ModelProfile};
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
}
