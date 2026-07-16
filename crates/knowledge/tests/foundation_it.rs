//! STEP 2.1 foundation: migration 0003 applies and the index outbox round-trips
//! (enqueue in a transaction, claim unprocessed, mark processed, reset).

use codypendent_knowledge::outbox::{self, KnowledgeIndexEvent};
use codypendent_knowledge::{db, types::Scope};
use codypendent_protocol::{MemoryId, RegistryItemId, RepositoryId};

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

#[tokio::test]
async fn migration_creates_the_phase2_tables() {
    let (_tmp, pool) = temp_pool().await;
    for table in [
        "registry_items",
        "memories",
        "code_nodes",
        "code_edges",
        "index_outbox",
    ] {
        let exists: Option<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' AND name = ?")
                .bind(table)
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(exists, Some((table.to_string(),)), "missing table {table}");
    }
}

#[tokio::test]
async fn outbox_enqueue_claim_process_and_reset() {
    let (_tmp, pool) = temp_pool().await;
    let now = chrono::Utc::now();

    // Enqueue inside a transaction (mirrors an authoritative write's atomicity).
    let mut tx = pool.begin().await.unwrap();
    let registry_event = KnowledgeIndexEvent::RegistryItemChanged(RegistryItemId::new());
    let memory_event = KnowledgeIndexEvent::MemoryChanged(MemoryId::new());
    outbox::enqueue(&mut *tx, &registry_event, now)
        .await
        .unwrap();
    outbox::enqueue(&mut *tx, &memory_event, now).await.unwrap();
    tx.commit().await.unwrap();

    // Both surface as unprocessed, oldest first.
    let rows = outbox::unprocessed(&pool, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].event_kind, "registry_item_changed");
    assert_eq!(rows[1].event_kind, "memory_changed");
    assert_eq!(rows[0].entity_id, registry_event.entity_id());

    // Processing one removes it from the unprocessed set.
    outbox::mark_processed(&pool, &rows[0].id, now)
        .await
        .unwrap();
    let remaining = outbox::unprocessed(&pool, 10).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].event_kind, "memory_changed");

    // `index rebuild`'s first move: reset every row back to unprocessed.
    let reset = outbox::reset_all(&pool).await.unwrap();
    assert_eq!(reset, 2);
    assert_eq!(outbox::unprocessed(&pool, 10).await.unwrap().len(), 2);
}

#[test]
fn scope_flattens_to_a_filterable_tier_and_key() {
    let repo = RepositoryId::new();
    let scope = Scope::Repository(repo);
    assert_eq!(scope.tier(), "repository");
    assert_eq!(scope.key(), Some(repo.to_string()));

    // System is keyless; a more specific scope wins shadowing.
    assert_eq!(Scope::System.key(), None);
    assert!(
        Scope::Repository(repo).specificity()
            > Scope::User(codypendent_protocol::UserId("u".into())).specificity()
    );
}

#[test]
fn scope_round_trips_through_serde() {
    // Adjacent tagging must round-trip both the keyless and id-bearing variants
    // (internal tagging cannot serialize the latter) — memory records embed a
    // Scope and are serialized as JSON.
    for scope in [
        Scope::System,
        Scope::Repository(RepositoryId::new()),
        Scope::User(codypendent_protocol::UserId("alice".into())),
    ] {
        let json = serde_json::to_string(&scope).expect("scope serializes");
        let back: Scope = serde_json::from_str(&json).expect("scope deserializes");
        assert_eq!(scope, back, "round-trip mismatch for {json}");
    }
    // The wire shape is exactly the flattened tier/key projection.
    assert_eq!(
        serde_json::to_value(Scope::System).unwrap(),
        serde_json::json!({ "tier": "system" })
    );
    let repo = RepositoryId::new();
    assert_eq!(
        serde_json::to_value(Scope::Repository(repo)).unwrap(),
        serde_json::json!({ "tier": "repository", "key": repo.to_string() })
    );
}
