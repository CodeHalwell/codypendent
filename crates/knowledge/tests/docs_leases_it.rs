//! STEP 4.3 transport: block-range edit leases enforce one writer per range,
//! whole-document leases conflict with block leases both ways, expiry reclaims a
//! crashed holder's lease, and the same writer renews rather than conflicts.

use std::time::Duration;

use codypendent_knowledge::db;
use codypendent_knowledge::docs::leases::{DocumentLeaseStore, LeaseError};
use codypendent_knowledge::docs::model::DocumentAuthor;
use codypendent_protocol::{DocumentId, UserId};

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

fn writer(name: &str) -> DocumentAuthor {
    DocumentAuthor::Human {
        user: UserId(name.into()),
    }
}

const TTL: Duration = Duration::from_secs(300);

#[tokio::test]
async fn one_writer_per_block_second_writer_conflicts() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentLeaseStore::new();
    let doc = DocumentId::new();

    // Alice acquires the block; Bob is refused.
    store
        .acquire(&pool, doc, Some("p"), &writer("alice"), TTL)
        .await
        .unwrap();
    let err = store
        .acquire(&pool, doc, Some("p"), &writer("bob"), TTL)
        .await
        .unwrap_err();
    assert!(matches!(err, LeaseError::Conflict { holder_key } if holder_key == "human:alice"));

    // require() reflects the same: Bob is blocked, Alice is not, a reader on a
    // different block is free.
    assert!(matches!(
        store.require(&pool, doc, Some("p"), &writer("bob")).await,
        Err(LeaseError::Conflict { .. })
    ));
    store
        .require(&pool, doc, Some("p"), &writer("alice"))
        .await
        .unwrap();
    store
        .require(&pool, doc, Some("other"), &writer("bob"))
        .await
        .unwrap();

    // The active holder is reported.
    let holder = store.active_holder(&pool, doc, Some("p")).await.unwrap();
    assert_eq!(holder, Some(writer("alice")));
}

#[tokio::test]
async fn a_whole_document_lease_conflicts_with_block_leases_both_ways() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentLeaseStore::new();
    let doc = DocumentId::new();

    // A whole-document (structural) lease blocks any block acquire by another.
    store
        .acquire(&pool, doc, None, &writer("alice"), TTL)
        .await
        .unwrap();
    assert!(matches!(
        store
            .acquire(&pool, doc, Some("p"), &writer("bob"), TTL)
            .await,
        Err(LeaseError::Conflict { .. })
    ));

    // And the reverse: a block lease blocks a whole-document acquire by another.
    let (_tmp2, pool2) = temp_pool().await;
    let doc2 = DocumentId::new();
    store
        .acquire(&pool2, doc2, Some("p"), &writer("alice"), TTL)
        .await
        .unwrap();
    assert!(matches!(
        store.acquire(&pool2, doc2, None, &writer("bob"), TTL).await,
        Err(LeaseError::Conflict { .. })
    ));
}

#[tokio::test]
async fn the_same_writer_renews_in_place() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentLeaseStore::new();
    let doc = DocumentId::new();

    let first = store
        .acquire(
            &pool,
            doc,
            Some("p"),
            &writer("alice"),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let renewed = store
        .acquire(&pool, doc, Some("p"), &writer("alice"), TTL)
        .await
        .unwrap();
    // Same lease id (renewed, not duplicated) with a later expiry.
    assert_eq!(first.id, renewed.id);
    assert!(renewed.expires_at >= first.expires_at);
}

#[tokio::test]
async fn an_expired_lease_is_reclaimed() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentLeaseStore::new();
    let doc = DocumentId::new();

    // Alice's lease expires immediately (ttl 0).
    store
        .acquire(
            &pool,
            doc,
            Some("p"),
            &writer("alice"),
            Duration::from_secs(0),
        )
        .await
        .unwrap();
    // Bob can now take it — the expired lease no longer conflicts.
    store
        .acquire(&pool, doc, Some("p"), &writer("bob"), TTL)
        .await
        .unwrap();
    assert_eq!(
        store.active_holder(&pool, doc, Some("p")).await.unwrap(),
        Some(writer("bob"))
    );
}

#[tokio::test]
async fn release_frees_the_range() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentLeaseStore::new();
    let doc = DocumentId::new();

    let lease = store
        .acquire(&pool, doc, Some("p"), &writer("alice"), TTL)
        .await
        .unwrap();
    store.release(&pool, &lease.id).await.unwrap();

    // Released: no holder, and Bob may acquire.
    assert_eq!(
        store.active_holder(&pool, doc, Some("p")).await.unwrap(),
        None
    );
    store
        .acquire(&pool, doc, Some("p"), &writer("bob"), TTL)
        .await
        .unwrap();
    // Releasing again is a no-op.
    store.release(&pool, &lease.id).await.unwrap();
}

#[tokio::test]
async fn leases_are_scoped_to_their_document() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentLeaseStore::new();
    let (a, b) = (DocumentId::new(), DocumentId::new());

    // A lease on document A does not block the same block id on document B.
    store
        .acquire(&pool, a, Some("p"), &writer("alice"), TTL)
        .await
        .unwrap();
    store
        .acquire(&pool, b, Some("p"), &writer("bob"), TTL)
        .await
        .unwrap();
}
