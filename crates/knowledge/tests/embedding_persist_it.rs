//! Persisted registry vectors + the index-outbox drain (rubric 9): an outbox
//! row written by a registry change is drained into a `registry_embeddings`
//! row, retrieval sees the fresh content through the persisted vectors, and a
//! model/dims change invalidates stored rows (re-embed on the next pass).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use codypendent_knowledge::retrieval::{
    drain_outbox, embedding_content_hash, load_embeddings, reconcile_embeddings, retrieve,
    semantic_indexes, EmbedError, RetrievalConfig, SemanticEmbedder,
};
use codypendent_knowledge::types::{
    Provenance, RegistryItem, RegistryItemKind, RegistryStatus, RiskClass, Scope, TrustMetadata,
    TrustTier, Version,
};
use codypendent_knowledge::{db, embedding_text, Registry, RetrievalQuery};
use codypendent_protocol::RegistryItemId;

/// A deterministic stand-in for a remote embedding model: three axes counting
/// marker-word occurrences ("tests", "diff", "deploy"), L2-normalized. Real
/// enough that dense cosine ranks a "run the tests" query onto a tests-worded
/// item, deterministic enough for CI, and dimensionally DIFFERENT from the
/// hashing embedder (3 vs 512) so a space mix-up cannot pass silently.
struct MarkerEmbedder {
    model: String,
    calls: AtomicUsize,
    batches: Mutex<Vec<usize>>,
    fail: bool,
}

impl MarkerEmbedder {
    fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            calls: AtomicUsize::new(0),
            batches: Mutex::new(Vec::new()),
            fail: false,
        }
    }

    fn failing() -> Self {
        Self {
            model: "unreachable-model".to_string(),
            calls: AtomicUsize::new(0),
            batches: Mutex::new(Vec::new()),
            fail: true,
        }
    }

    fn embed_one(text: &str) -> Vec<f32> {
        let lower = text.to_ascii_lowercase();
        let mut v = vec![
            lower.matches("test").count() as f32,
            lower.matches("diff").count() as f32,
            lower.matches("deploy").count() as f32,
        ];
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        } else {
            v[0] = 1.0;
        }
        v
    }
}

#[async_trait::async_trait]
impl SemanticEmbedder for MarkerEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.batches.lock().unwrap().push(texts.len());
        if self.fail {
            return Err(EmbedError("endpoint unreachable".to_string()));
        }
        Ok(texts.iter().map(|t| Self::embed_one(t)).collect())
    }
}

fn tool(name: &str, description: &str) -> RegistryItem {
    let now = chrono::Utc::now();
    RegistryItem {
        id: RegistryItemId::new(),
        kind: RegistryItemKind::Tool,
        name: name.to_string(),
        version: Version("1.0.0".to_string()),
        scope: Scope::System,
        description: description.to_string(),
        intents: Vec::new(),
        keywords: Vec::new(),
        examples: Vec::new(),
        input_schema: None,
        output_schema: None,
        dependencies: Vec::new(),
        permissions: Vec::new(),
        risk: RiskClass::Safe,
        provenance: Provenance::BuiltIn,
        trust: TrustMetadata {
            publisher: "codypendent".to_string(),
            signature_required: false,
            signature: None,
            tier: TrustTier::FirstParty,
        },
        status: RegistryStatus::Active,
        content_hash: String::new(),
        executable: true,
        created_at: now,
        updated_at: now,
    }
}

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

/// The headline flow: registry write → outbox row → drain → vector row →
/// content change → drain → vector row UPDATED → retrieval (persisted vectors +
/// same-space query vector) discloses by the fresh content.
#[tokio::test]
async fn outbox_drain_persists_and_refreshes_vectors_retrieval_sees_fresh_content() {
    let (_tmp, pool) = temp_pool().await;
    let registry = Registry::new();
    let embedder = MarkerEmbedder::new("marker-3d");

    let mut item = tool("repo.verify", "verify the change by running the tests");
    registry.upsert(&pool, &item).await.unwrap();

    // Drain the registry-change outbox row into a persisted vector.
    let report = drain_outbox(&pool, Some(&embedder), 100).await.unwrap();
    assert_eq!(report.processed, 1, "one outbox row consumed");
    assert_eq!(report.embedded, 1, "one vector persisted");
    let stored = load_embeddings(&pool).await.unwrap();
    let first = stored.get(&item.id).expect("vector row exists");
    assert_eq!(first.model, "marker-3d");
    assert_eq!(first.dims, 3);
    assert_eq!(
        first.content_hash,
        embedding_content_hash(&embedding_text(&item))
    );

    // A second drain is a no-op: nothing unprocessed, nothing re-embedded.
    let idle = drain_outbox(&pool, Some(&embedder), 100).await.unwrap();
    assert_eq!(idle.processed, 0);

    // Change the item's retrieval text → new outbox row → drained → row updated.
    item.description = "compute the diff between two revisions".to_string();
    registry.upsert(&pool, &item).await.unwrap();
    let report = drain_outbox(&pool, Some(&embedder), 100).await.unwrap();
    assert_eq!((report.processed, report.embedded), (1, 1));
    let stored = load_embeddings(&pool).await.unwrap();
    let second = stored.get(&item.id).expect("vector row still exists");
    assert_ne!(
        second.content_hash, first.content_hash,
        "a content change must refresh the persisted hash"
    );
    assert_ne!(second.vector, first.vector, "and the vector itself");

    // Retrieval over the PERSISTED vectors sees the fresh content: the item now
    // answers a "diff" query dense-first (no keywords/intents, so exact overlap
    // cannot carry it; the marker space puts all its weight on the diff axis).
    let items = registry.list(&pool).await.unwrap();
    let (indexes, query_vector) = semantic_indexes(&pool, &items, Some(&embedder), "show the diff")
        .await
        .unwrap();
    let calls_after_build = embedder.calls.load(Ordering::SeqCst);
    let mut query = RetrievalQuery::new("show the diff", vec![Scope::System], RiskClass::Medium);
    query.query_vector = query_vector;
    let result = retrieve(&items, &indexes, &query, &RetrievalConfig::default()).unwrap();
    assert!(
        result.tools.iter().any(|card| card.name == "repo.verify"),
        "retrieval must disclose by the item's FRESH content: {:?}",
        result.tools
    );
    // The assembly embedded ONLY the query (items came from persisted rows):
    // exactly one batch call, so persisted vectors genuinely replaced
    // per-assembly recomputation.
    assert_eq!(
        calls_after_build,
        3, // two drains that embedded + one assembly (query-only)
        "assembly must not re-embed persisted items"
    );
}

/// A model change (different name → different space, possibly different dims)
/// makes every stored row stale: the next drain/reconcile re-embeds under the
/// new model rather than serving cross-space vectors.
#[tokio::test]
async fn dimension_or_model_mismatch_triggers_reembed() {
    let (_tmp, pool) = temp_pool().await;
    let registry = Registry::new();
    let item = tool("workspace.probe", "probe the workspace tests");
    registry.upsert(&pool, &item).await.unwrap();

    let old = MarkerEmbedder::new("old-model");
    drain_outbox(&pool, Some(&old), 100).await.unwrap();
    let stored = load_embeddings(&pool).await.unwrap();
    assert_eq!(stored.get(&item.id).unwrap().model, "old-model");

    // Same item, new model: the outbox is already drained, so RECONCILE is the
    // pass that must notice the model mismatch and rewrite the row.
    let new = MarkerEmbedder::new("new-model");
    let rewritten = reconcile_embeddings(&pool, &new).await.unwrap();
    assert_eq!(rewritten, 1, "the stale-model row must be re-embedded");
    let stored = load_embeddings(&pool).await.unwrap();
    let row = stored.get(&item.id).unwrap();
    assert_eq!(row.model, "new-model");
    assert_eq!(row.dims, row.vector.len(), "dims must match the vector");

    // Reconcile is idempotent once fresh.
    assert_eq!(reconcile_embeddings(&pool, &new).await.unwrap(), 0);
}

/// Removing an item drains into a DELETE of its vector row; non-registry outbox
/// kinds are acknowledged through the documented hook so the outbox never grows
/// without bound.
#[tokio::test]
async fn drain_deletes_removed_items_and_acknowledges_other_kinds() {
    let (_tmp, pool) = temp_pool().await;
    let registry = Registry::new();
    let embedder = MarkerEmbedder::new("marker-3d");

    let item = tool("doomed.tool", "temporarily present tests helper");
    registry.upsert(&pool, &item).await.unwrap();
    drain_outbox(&pool, Some(&embedder), 100).await.unwrap();
    assert!(load_embeddings(&pool).await.unwrap().contains_key(&item.id));

    registry.remove(&pool, item.id).await.unwrap();
    // A non-registry kind rides the same queue (the plug-in hook's path).
    codypendent_knowledge::outbox::enqueue(
        &pool,
        &codypendent_knowledge::KnowledgeIndexEvent::DocumentChanged(
            codypendent_protocol::DocumentId::new(),
        ),
        chrono::Utc::now(),
    )
    .await
    .unwrap();

    let report = drain_outbox(&pool, Some(&embedder), 100).await.unwrap();
    assert_eq!(report.processed, 2, "both rows consumed");
    assert_eq!(
        report.deleted, 1,
        "the removed item's vector row is dropped"
    );
    assert_eq!(report.deferred_kinds, 1, "the document row hit the hook");
    assert!(
        !load_embeddings(&pool).await.unwrap().contains_key(&item.id),
        "no orphan vector row may remain"
    );
    // Nothing left unprocessed.
    let idle = drain_outbox(&pool, Some(&embedder), 100).await.unwrap();
    assert_eq!(idle.processed, 0);
}

/// A drain that ran while NO embedding model was configured still consumes the
/// outbox (bounding its growth); configuring a model later backfills through
/// `reconcile_embeddings` — the startup pass — so no item is stranded.
#[tokio::test]
async fn unconfigured_drain_consumes_rows_and_reconcile_backfills_later() {
    let (_tmp, pool) = temp_pool().await;
    let registry = Registry::new();
    let item = tool("late.embed", "arrives before any embedding model");
    registry.upsert(&pool, &item).await.unwrap();

    let report = drain_outbox(&pool, None, 100).await.unwrap();
    assert_eq!((report.processed, report.embedded), (1, 0));
    assert!(load_embeddings(&pool).await.unwrap().is_empty());

    let embedder = MarkerEmbedder::new("marker-3d");
    assert_eq!(reconcile_embeddings(&pool, &embedder).await.unwrap(), 1);
    assert!(load_embeddings(&pool).await.unwrap().contains_key(&item.id));
}

/// An unreachable model degrades `semantic_indexes` to the hashing pair —
/// retrieval still runs (today's behavior), and the query vector override is
/// None so the funnel embeds the query in the hashing space it indexed in.
#[tokio::test]
async fn semantic_index_build_falls_back_to_hashing_on_model_failure() {
    let (_tmp, pool) = temp_pool().await;
    let registry = Registry::new();
    let item = tool(
        "resilient.tool",
        "runs the tests even when embeddings are down",
    );
    registry.upsert(&pool, &item).await.unwrap();
    let items = registry.list(&pool).await.unwrap();

    let broken = MarkerEmbedder::failing();
    let (indexes, query_vector) = semantic_indexes(&pool, &items, Some(&broken), "run the tests")
        .await
        .unwrap();
    assert!(query_vector.is_none(), "fallback must not mix spaces");

    let query = RetrievalQuery::new("run the tests", vec![Scope::System], RiskClass::Medium);
    let result = retrieve(&items, &indexes, &query, &RetrievalConfig::default()).unwrap();
    assert!(
        result
            .tools
            .iter()
            .any(|card| card.name == "resilient.tool"),
        "hashing fallback still serves retrieval: {:?}",
        result.tools
    );
    // The failing model left an unprocessed-free table untouched.
    assert!(load_embeddings(&pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn reconciliation_batches_requests_and_skips_fresh_rows() {
    let (_tmp, pool) = temp_pool().await;
    let registry = Registry::new();
    for index in 0..65 {
        registry
            .upsert(&pool, &tool(&format!("tests.{index}"), "run tests"))
            .await
            .unwrap();
    }
    let embedder = MarkerEmbedder::new("batched-model");
    assert_eq!(reconcile_embeddings(&pool, &embedder).await.unwrap(), 65);
    assert_eq!(*embedder.batches.lock().unwrap(), vec![64, 1]);
    assert_eq!(reconcile_embeddings(&pool, &embedder).await.unwrap(), 0);
    assert_eq!(embedder.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn outbox_coalesces_repeated_changes_before_embedding() {
    let (_tmp, pool) = temp_pool().await;
    let registry = Registry::new();
    let mut item = tool("tests.repeat", "run tests");
    for index in 0..5 {
        item.description = format!("run tests {index}");
        registry.upsert(&pool, &item).await.unwrap();
    }
    let other = tool("tests.other", "run other tests");
    registry.upsert(&pool, &other).await.unwrap();
    let embedder = MarkerEmbedder::new("batched-model");
    let report = drain_outbox(&pool, Some(&embedder), 100).await.unwrap();
    assert_eq!((report.processed, report.embedded), (6, 2));
    assert_eq!(*embedder.batches.lock().unwrap(), vec![2]);
    let stored = load_embeddings(&pool).await.unwrap();
    assert_eq!(
        stored[&item.id].content_hash,
        embedding_content_hash(&embedding_text(&item))
    );
}

struct TaskEmbedder {
    space: &'static str,
    output: Option<Vec<Vec<f32>>>,
    query: Vec<f32>,
    documents: Mutex<Vec<usize>>,
    queries: AtomicUsize,
}

impl TaskEmbedder {
    fn new(space: &'static str) -> Self {
        Self {
            space,
            output: None,
            query: vec![1.0, 0.0, 0.0],
            documents: Mutex::new(Vec::new()),
            queries: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl SemanticEmbedder for TaskEmbedder {
    fn model(&self) -> &str {
        "shared-model-alias"
    }
    fn space_id(&self) -> &str {
        self.space
    }

    async fn embed_query(&self, _: &str) -> Result<Vec<f32>, EmbedError> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        Ok(self.query.clone())
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.documents.lock().unwrap().push(texts.len());
        Ok(self
            .output
            .clone()
            .unwrap_or_else(|| vec![self.query.clone(); texts.len()]))
    }
}

#[tokio::test]
async fn retrieval_keeps_query_tasks_separate_and_refreshes_changed_spaces_and_widths() {
    let (_tmp, pool) = temp_pool().await;
    let item = tool("tests.task", "run tests");
    Registry::new().upsert(&pool, &item).await.unwrap();
    let items = vec![item];
    let first = TaskEmbedder::new("endpoint-one:digest-one:task-v1");
    for _ in 0..2 {
        assert!(semantic_indexes(&pool, &items, Some(&first), "test this")
            .await
            .unwrap()
            .1
            .is_some());
    }
    assert_eq!(*first.documents.lock().unwrap(), vec![1]);
    assert_eq!(first.queries.load(Ordering::SeqCst), 2);
    let mut changed = TaskEmbedder::new("endpoint-two:digest-two:task-v1");
    semantic_indexes(&pool, &items, Some(&changed), "test this")
        .await
        .unwrap();
    assert_eq!(*changed.documents.lock().unwrap(), vec![1]);
    assert_eq!(
        load_embeddings(&pool).await.unwrap()[&items[0].id].model,
        changed.space
    );
    changed.query = vec![1.0, 0.0];
    semantic_indexes(&pool, &items, Some(&changed), "test this")
        .await
        .unwrap();
    assert_eq!(*changed.documents.lock().unwrap(), vec![1, 1]);
    assert_eq!(load_embeddings(&pool).await.unwrap()[&items[0].id].dims, 2);
}

#[tokio::test]
async fn malformed_document_vectors_fall_back_without_persisting_partial_rows() {
    for output in [
        vec![vec![1.0, 0.0, 0.0]],
        vec![vec![1.0, 0.0, 0.0], vec![1.0, 0.0]],
        vec![vec![f32::NAN, 0.0, 0.0]; 2],
        vec![vec![0.0, 0.0, 0.0]; 2],
    ] {
        let (_tmp, pool) = temp_pool().await;
        let items = vec![
            tool("tests.first", "first tests"),
            tool("tests.second", "second tests"),
        ];
        for item in &items {
            Registry::new().upsert(&pool, item).await.unwrap();
        }
        let mut embedder = TaskEmbedder::new("invalid-response");
        embedder.output = Some(output);
        assert!(
            semantic_indexes(&pool, &items, Some(&embedder), "test this")
                .await
                .unwrap()
                .1
                .is_none()
        );
        assert!(load_embeddings(&pool).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn failed_batch_leaves_the_outbox_available_for_retry() {
    let (_tmp, pool) = temp_pool().await;
    let item = tool("tests.retry", "run tests");
    Registry::new().upsert(&pool, &item).await.unwrap();
    let mut embedder = TaskEmbedder::new("invalid-response");
    embedder.output = Some(Vec::new());
    assert!(drain_outbox(&pool, Some(&embedder), 100).await.is_err());
    assert!(load_embeddings(&pool).await.unwrap().is_empty());
    assert_eq!(
        codypendent_knowledge::outbox::unprocessed(&pool, 100)
            .await
            .unwrap()
            .len(),
        1
    );
    embedder.output = None;
    assert_eq!(
        drain_outbox(&pool, Some(&embedder), 100)
            .await
            .unwrap()
            .embedded,
        1
    );
}
