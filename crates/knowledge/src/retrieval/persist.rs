//! Persisted registry vectors + the index-outbox drain (rubric 9).
//!
//! Two jobs, both over the `registry_embeddings` table (migration 0022):
//!
//! 1. **Persistence** — store one dense vector per registry item, keyed by the
//!    SHA-256 of its retrieval text and stamped with the embedding model + dims,
//!    so [`semantic_indexes`] loads vectors instead of re-embedding every item on
//!    every `assemble_context`, and a description edit / model switch / dims
//!    mismatch is detected as staleness and re-embedded.
//! 2. **The drain** — the first real consumer of the Chapter 06 `index_outbox`:
//!    [`drain_outbox`] claims unprocessed rows, refreshes the vector rows for
//!    changed registry items (deleting rows for removed items), and stamps every
//!    row processed so the outbox stops growing without bound. Non-registry
//!    kinds pass through [`handle_non_registry_event`], the deliberate hook where
//!    document/memory/symbol indexing plugs in later.
//!
//! Everything here is derived data over `registry_items` authority: deletable at
//! any time, rebuilt by replaying the outbox (or by [`reconcile_embeddings`],
//! the full-scan form the daemon runs at startup so items written while no
//! embedding model was configured are picked up).

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use codypendent_protocol::RegistryItemId;
use sha2::{Digest, Sha256};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use crate::outbox::{self, OutboxRow};
use crate::registry::Registry;
use crate::types::RegistryItem;

use super::embed::{EmbedError, SemanticEmbedder};
use super::{embedding_text, HashingEmbedder, RetrievalIndexes, VectorIndex};

const EMBEDDING_BATCH_SIZE: usize = 64;
const LOOKUP_BATCH_SIZE: usize = 500;
const UPSERT_EMBEDDING: &str = "INSERT OR REPLACE INTO registry_embeddings \
    (item_id, content_hash, model, dims, vector, updated_at) VALUES (?, ?, ?, ?, ?, ?)";
type EmbeddingRow = (String, String, String, i64, Vec<u8>);

/// A failure loading/storing persisted vectors or draining the outbox.
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    /// The registry read behind a drain step failed.
    #[error(transparent)]
    Registry(#[from] crate::registry::RegistryError),
    /// The embedding model call failed (the drain leaves the rows unprocessed
    /// and retries on the next pass).
    #[error(transparent)]
    Embed(#[from] EmbedError),
}

/// One persisted vector row, as loaded for staleness checks and index builds.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEmbedding {
    /// Hex SHA-256 of the text the vector was computed from.
    pub content_hash: String,
    /// The embedding model that produced it.
    pub model: String,
    /// The vector's dimensionality.
    pub dims: usize,
    /// The vector itself.
    pub vector: Vec<f32>,
}

/// The hex SHA-256 of an item's retrieval text — the persistence key that makes
/// a description/intent edit invalidate its stored vector.
#[must_use]
pub fn embedding_content_hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// Encode a vector as little-endian f32 bytes (the `vector BLOB` column).
fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Decode a persisted vector, rejecting incomplete values as stale data.
fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    let (values, ragged) = bytes.as_chunks::<4>();
    if !ragged.is_empty() {
        return Vec::new();
    }
    values.iter().copied().map(f32::from_le_bytes).collect()
}

/// Load every persisted vector, keyed by item id. The caller filters by
/// model/hash/dims per item (staleness is per-row, not per-table).
pub async fn load_embeddings(
    pool: &SqlitePool,
) -> Result<HashMap<RegistryItemId, StoredEmbedding>, PersistError> {
    let rows: Vec<EmbeddingRow> = sqlx::query_as(
        "SELECT item_id, content_hash, model, dims, vector FROM registry_embeddings",
    )
    .fetch_all(pool)
    .await?;
    Ok(decode_rows(rows))
}

async fn load_item_embeddings(
    pool: &SqlitePool,
    items: &[RegistryItem],
    space: &str,
) -> Result<HashMap<RegistryItemId, StoredEmbedding>, PersistError> {
    let mut stored = HashMap::with_capacity(items.len());
    for batch in items.chunks(LOOKUP_BATCH_SIZE) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT item_id, content_hash, model, dims, vector FROM registry_embeddings WHERE model = ",
        );
        query.push_bind(space).push(" AND item_id IN (");
        let mut ids = query.separated(", ");
        for item in batch {
            ids.push_bind(item.id.to_string());
        }
        ids.push_unseparated(")");
        stored.extend(decode_rows(
            query
                .build_query_as::<EmbeddingRow>()
                .fetch_all(pool)
                .await?,
        ));
    }
    Ok(stored)
}

fn decode_rows(rows: Vec<EmbeddingRow>) -> HashMap<RegistryItemId, StoredEmbedding> {
    let mut out = HashMap::with_capacity(rows.len());
    for (item_id, content_hash, model, dims, vector) in rows {
        let Ok(id) = item_id.parse::<RegistryItemId>() else {
            continue; // an unparseable id is dead derived data; the drain replaces it
        };
        out.insert(
            id,
            StoredEmbedding {
                content_hash,
                model,
                dims: usize::try_from(dims).unwrap_or(0),
                vector: decode_vector(&vector),
            },
        );
    }
    out
}

/// Insert or replace one item's persisted vector.
pub async fn upsert_embedding(
    pool: &SqlitePool,
    item_id: RegistryItemId,
    content_hash: &str,
    model: &str,
    vector: &[f32],
    now: DateTime<Utc>,
) -> Result<(), PersistError> {
    sqlx::query(UPSERT_EMBEDDING)
        .bind(item_id.to_string())
        .bind(content_hash)
        .bind(model)
        .bind(vector.len() as i64)
        .bind(encode_vector(vector))
        .bind(now.to_rfc3339())
        .execute(pool)
        .await?;
    Ok(())
}

/// Drop one item's persisted vector (its authoritative row was removed).
pub async fn delete_embedding(
    pool: &SqlitePool,
    item_id: RegistryItemId,
) -> Result<(), PersistError> {
    sqlx::query("DELETE FROM registry_embeddings WHERE item_id = ?")
        .bind(item_id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether `stored` is fresh for (`model`, `content_hash`): same model, same
/// text hash, and the dims recorded actually match the vector's length (a model
/// change usually flips the hash comparison via `model`; the explicit dims check
/// additionally catches a provider that changed output width under one name).
fn is_fresh(stored: &StoredEmbedding, model: &str, content_hash: &str) -> bool {
    stored.model == model
        && stored.content_hash == content_hash
        && stored.dims == stored.vector.len()
        && valid_vector(&stored.vector)
}

fn valid_vector(vector: &[f32]) -> bool {
    let norm = vector.iter().map(|value| value * value).sum::<f32>();
    vector.iter().all(|value| value.is_finite()) && norm.is_finite() && norm > 0.0
}

fn validate_vectors(vectors: &[Vec<f32>], count: usize, width: usize) -> Result<(), PersistError> {
    if vectors.len() != count
        || width == 0
        || vectors
            .iter()
            .any(|vector| vector.len() != width || !valid_vector(vector))
    {
        return Err(
            EmbedError("embedding model returned invalid or inconsistent vectors".into()).into(),
        );
    }
    Ok(())
}

/// What one maintenance pass did — logged by the daemon job, asserted by tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DrainReport {
    /// Outbox rows stamped processed this pass.
    pub processed: usize,
    /// Registry vectors recomputed and persisted.
    pub embedded: usize,
    /// Persisted vectors dropped because their item was removed.
    pub deleted: usize,
    /// Non-registry outbox rows passed to the (currently no-op) hook.
    pub deferred_kinds: usize,
}

/// The plug point for non-registry outbox kinds (`memory_changed`,
/// `symbol_changed`, `document_changed`, `artifact_created`). Document indexing
/// (and the rest) slots in here later; today the row is acknowledged so the
/// outbox drains instead of growing forever — the derived indexes for those
/// kinds are still rebuilt from authority on demand, so acknowledging loses
/// nothing.
fn handle_non_registry_event(row: &OutboxRow) {
    tracing::trace!(kind = %row.event_kind, entity = %row.entity_id, "no derived index consumes this outbox kind yet");
}

/// Drain up to `limit` unprocessed `index_outbox` rows.
///
/// For each `registry_item_changed` row: re-read the item from authority; if it
/// still exists and `embedder` is configured, refresh its persisted vector when
/// stale (model/hash/dims mismatch — the re-embed trigger); if it was removed,
/// drop the vector row. Other kinds go through [`handle_non_registry_event`].
/// Every handled row is stamped processed. An embedding failure aborts the pass
/// with rows left unprocessed, so the next pass retries — the outbox is the
/// at-least-once retry queue.
///
/// With no `embedder` the drain still consumes rows (there is nothing to
/// persist for the hashing model — it embeds inline at query time); a later
/// [`reconcile_embeddings`] pass backfills vectors once a model is configured.
pub async fn drain_outbox(
    pool: &SqlitePool,
    embedder: Option<&dyn SemanticEmbedder>,
    limit: i64,
) -> Result<DrainReport, PersistError> {
    let rows = outbox::unprocessed(pool, limit).await?;
    let registry = Registry::new();
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    let mut deleted = Vec::new();
    for row in &rows {
        if row.event_kind == "registry_item_changed" {
            if let Ok(item_id) = row.entity_id.parse::<RegistryItemId>() {
                if seen.insert(item_id) {
                    match registry.get(pool, item_id).await? {
                        Some(item) => items.push(item),
                        None => deleted.push(item_id),
                    }
                }
            }
        }
    }
    let mut report = DrainReport::default();
    if let Some(embedder) = embedder {
        report.embedded = refresh_embeddings(pool, embedder, &items).await?;
    }
    for item_id in deleted {
        delete_embedding(pool, item_id).await?;
        report.deleted += 1;
    }
    for row in rows {
        if row.event_kind != "registry_item_changed" {
            handle_non_registry_event(&row);
            report.deferred_kinds += 1;
        }
        outbox::mark_processed(pool, &row.id, Utc::now()).await?;
        report.processed += 1;
    }
    Ok(report)
}

async fn refresh_embeddings(
    pool: &SqlitePool,
    embedder: &dyn SemanticEmbedder,
    items: &[RegistryItem],
) -> Result<usize, PersistError> {
    let stored = load_item_embeddings(pool, items, embedder.space_id()).await?;
    let pending: Vec<_> = items
        .iter()
        .filter_map(|item| {
            let text = embedding_text(item);
            let hash = embedding_content_hash(&text);
            (!stored
                .get(&item.id)
                .is_some_and(|row| is_fresh(row, embedder.space_id(), &hash)))
            .then_some((item, text, hash))
        })
        .collect();
    Ok(embed_and_store(pool, embedder, &pending, None).await?.len())
}

async fn embed_and_store(
    pool: &SqlitePool,
    embedder: &dyn SemanticEmbedder,
    pending: &[(&RegistryItem, String, String)],
    mut width: Option<usize>,
) -> Result<Vec<(RegistryItemId, Vec<f32>)>, PersistError> {
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let mut vectors = Vec::with_capacity(pending.len());
    for batch in pending.chunks(EMBEDDING_BATCH_SIZE) {
        let texts: Vec<_> = batch.iter().map(|(_, text, _)| text.clone()).collect();
        let embedded = embedder.embed_batch(&texts).await?;
        let expected_width = *width.get_or_insert_with(|| embedded.first().map_or(0, Vec::len));
        validate_vectors(&embedded, texts.len(), expected_width)?;
        vectors.extend(embedded);
    }
    let mut transaction = pool.begin().await?;
    let now = Utc::now().to_rfc3339();
    for ((item, _, hash), vector) in pending.iter().zip(&vectors) {
        sqlx::query(UPSERT_EMBEDDING)
            .bind(item.id.to_string())
            .bind(hash)
            .bind(embedder.space_id())
            .bind(vector.len() as i64)
            .bind(encode_vector(vector))
            .bind(&now)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(pending
        .iter()
        .zip(vectors)
        .map(|((item, _, _), vector)| (item.id, vector))
        .collect())
}

/// Refresh missing or stale registry vectors in bounded document batches.
pub async fn reconcile_embeddings(
    pool: &SqlitePool,
    embedder: &dyn SemanticEmbedder,
) -> Result<usize, PersistError> {
    let items = Registry::new().list(pool).await?;
    refresh_embeddings(pool, embedder, &items).await
}

/// Build the derived retrieval indexes for `items`, plus the query vector for
/// `query_text`, preferring the configured semantic model.
///
/// - **No `semantic` model** (or any model failure): both indexes are built
///   exactly as before — the offline [`HashingEmbedder`] embeds every item and
///   the query inline — and the returned query vector is `None`, so
///   [`retrieve`](super::retrieve) embeds the query through the retained
///   hashing embedder. Behavior-identical to the pre-embedding code.
/// - **With a model**: fresh item vectors come from `registry_embeddings`;
///   missing vectors are embedded in bounded document batches and persisted.
///   Queries use the model's query task in the same space as the index. The
///   caller passes the query vector through
///   [`RetrievalQuery::query_vector`](super::RetrievalQuery::query_vector) so
///   the funnel never mixes embedding spaces.
///
/// Every model-path failure (endpoint down, dims mismatch, bad response)
/// degrades to the hashing pair with a warning — retrieval is an aid, never a
/// gate on running.
pub async fn semantic_indexes(
    pool: &SqlitePool,
    items: &[RegistryItem],
    semantic: Option<&dyn SemanticEmbedder>,
    query_text: &str,
) -> Result<(RetrievalIndexes, Option<Vec<f32>>), super::RetrievalError> {
    let Some(embedder) = semantic else {
        return Ok((
            RetrievalIndexes::build(items, HashingEmbedder::new())?,
            None,
        ));
    };
    match semantic_vectors(pool, items, embedder, query_text).await {
        Ok((vector_index, query_vector)) => {
            let indexes =
                RetrievalIndexes::from_parts(items, vector_index, HashingEmbedder::new())?;
            Ok((indexes, Some(query_vector)))
        }
        Err(error) => {
            tracing::warn!(model = embedder.model(), %error, "semantic embedding unavailable; falling back to the hashing embedder");
            Ok((
                RetrievalIndexes::build(items, HashingEmbedder::new())?,
                None,
            ))
        }
    }
}

/// The semantic half of [`semantic_indexes`]: assemble the item vector index
/// from persisted and batched document vectors in the query's embedding space.
async fn semantic_vectors(
    pool: &SqlitePool,
    items: &[RegistryItem],
    embedder: &dyn SemanticEmbedder,
    query_text: &str,
) -> Result<(VectorIndex, Vec<f32>), PersistError> {
    let query_vector = embedder.embed_query(query_text).await?;
    validate_vectors(std::slice::from_ref(&query_vector), 1, query_vector.len())?;
    let mut stored = load_item_embeddings(pool, items, embedder.space_id()).await?;
    let mut index = VectorIndex::new();
    let mut pending = Vec::new();
    for item in items {
        let text = embedding_text(item);
        let hash = embedding_content_hash(&text);
        match stored.remove(&item.id) {
            Some(row)
                if is_fresh(&row, embedder.space_id(), &hash) && row.dims == query_vector.len() =>
            {
                index.insert(item.id, row.vector)
            }
            _ => pending.push((item, text, hash)),
        }
    }
    for (id, vector) in embed_and_store(pool, embedder, &pending, Some(query_vector.len())).await? {
        index.insert(id, vector);
    }
    Ok((index, query_vector))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_blob_round_trips_exactly() {
        let vector = vec![0.0_f32, 1.5, -2.25, f32::MIN_POSITIVE];
        assert_eq!(decode_vector(&encode_vector(&vector)), vector);
    }

    #[test]
    fn ragged_blob_is_stale() {
        let mut bytes = encode_vector(&[1.0, 2.0]);
        bytes.push(0xFF); // a trailing partial value
        assert!(decode_vector(&bytes).is_empty());
    }

    #[test]
    fn freshness_requires_model_hash_and_dims_agreement() {
        let stored = StoredEmbedding {
            content_hash: "abc".to_string(),
            model: "m1".to_string(),
            dims: 2,
            vector: vec![0.1, 0.2],
        };
        assert!(is_fresh(&stored, "m1", "abc"));
        assert!(!is_fresh(&stored, "m2", "abc"), "a model change is stale");
        assert!(!is_fresh(&stored, "m1", "xyz"), "a text change is stale");
        let mismatched = StoredEmbedding {
            dims: 3,
            ..stored.clone()
        };
        assert!(
            !is_fresh(&mismatched, "m1", "abc"),
            "a dims/vector mismatch is stale"
        );
    }
}
