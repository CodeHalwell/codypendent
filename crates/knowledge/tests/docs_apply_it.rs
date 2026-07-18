//! STEP 4.3 transport: `apply_mutation` maps a protocol `DocumentMutation` onto
//! the authoritative CRDT + suggestion store under a collaboration-mode gate, and
//! returns the `DocumentSync` to broadcast. Covers Edit (direct), Suggest
//! (routes to the review rail), Ask/Review (denied), and accept/reject
//! resolution.

use codypendent_knowledge::db;
use codypendent_knowledge::docs::apply::{apply_mutation, ApplyError, MutationEffect};
use codypendent_knowledge::docs::collab::SuggestionStore;
use codypendent_knowledge::docs::model::{
    BlockContent, DocumentAuthor, DocumentBlock, DocumentMetadata, MutationKind,
};
use codypendent_knowledge::docs::store::{DocumentStore, NewDocument};
use codypendent_knowledge::CollaborationMode;
use codypendent_knowledge::Scope;
use codypendent_protocol::document::{DocumentMutation, SuggestionInput};
use codypendent_protocol::{DocumentId, UserId};

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

fn human() -> DocumentAuthor {
    DocumentAuthor::Human {
        user: UserId("dev".into()),
    }
}

async fn seed_paragraph(pool: &sqlx::SqlitePool, text: &str) -> DocumentId {
    let doc = DocumentStore::new()
        .create(
            pool,
            NewDocument {
                title: "Doc".into(),
                scope: Scope::System,
                metadata: DocumentMetadata::default(),
                blocks: vec![DocumentBlock::with_id(
                    "p",
                    BlockContent::Paragraph { text: text.into() },
                )],
            },
            &human(),
        )
        .await
        .unwrap();
    doc.id
}

async fn block_text(pool: &sqlx::SqlitePool, id: DocumentId) -> String {
    let doc = DocumentStore::new().load(pool, id).await.unwrap().unwrap();
    doc.blocks().unwrap()[0].content_text().to_owned()
}

#[tokio::test]
async fn edit_mode_applies_text_directly_and_advances_the_revision() {
    let (_tmp, pool) = temp_pool().await;
    let id = seed_paragraph(&pool, "hello world").await;

    // Replace "hello" (0..5) with "HELLO": delete 5, insert "HELLO" at 0.
    let outcome = apply_mutation(
        &pool,
        id,
        &DocumentMutation::EditText {
            block_id: "p".into(),
            position: 0,
            delete_len: 5,
            insert: "HELLO".into(),
        },
        CollaborationMode::Edit,
        &human(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.effect,
        MutationEffect::Applied(MutationKind::EditText)
    );
    // The sync carries the advanced revision and non-empty CRDT bytes.
    assert_eq!(outcome.sync.document_id, id);
    assert_eq!(outcome.sync.revision, 2);
    assert!(!outcome.sync.update.is_empty());
    // The content is applied and persisted.
    assert_eq!(block_text(&pool, id).await, "HELLO world");
}

#[tokio::test]
async fn edit_mode_inserts_and_deletes_blocks() {
    let (_tmp, pool) = temp_pool().await;
    let id = seed_paragraph(&pool, "keep").await;

    // Insert a heading at index 0.
    let content = serde_json::to_value(BlockContent::Heading {
        level: 1,
        text: "Title".into(),
    })
    .unwrap();
    let inserted = apply_mutation(
        &pool,
        id,
        &DocumentMutation::Insert {
            index: 0,
            block_id: "h".into(),
            content,
        },
        CollaborationMode::Edit,
        &human(),
    )
    .await
    .unwrap();
    assert_eq!(
        inserted.effect,
        MutationEffect::Applied(MutationKind::InsertBlock)
    );

    let doc = DocumentStore::new().load(&pool, id).await.unwrap().unwrap();
    let blocks = doc.blocks().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].id, "h");

    // Delete the original paragraph.
    let deleted = apply_mutation(
        &pool,
        id,
        &DocumentMutation::Delete {
            block_id: "p".into(),
        },
        CollaborationMode::Edit,
        &human(),
    )
    .await
    .unwrap();
    assert_eq!(
        deleted.effect,
        MutationEffect::Applied(MutationKind::DeleteBlock)
    );
    let doc = DocumentStore::new().load(&pool, id).await.unwrap().unwrap();
    assert_eq!(doc.blocks().unwrap().len(), 1);
}

#[tokio::test]
async fn suggest_mode_routes_a_text_edit_to_the_review_rail() {
    let (_tmp, pool) = temp_pool().await;
    let id = seed_paragraph(&pool, "hello world").await;

    // In Suggest mode, an EditText becomes a pending suggestion — content is
    // untouched and the revision does not advance.
    let outcome = apply_mutation(
        &pool,
        id,
        &DocumentMutation::EditText {
            block_id: "p".into(),
            position: 0,
            delete_len: 5,
            insert: "HELLO".into(),
        },
        CollaborationMode::Suggest,
        &human(),
    )
    .await
    .unwrap();

    let suggestion_id = match &outcome.effect {
        MutationEffect::Suggested(id) => id.clone(),
        other => panic!("expected Suggested, got {other:?}"),
    };
    assert_eq!(outcome.sync.revision, 1);
    assert_eq!(block_text(&pool, id).await, "hello world");
    let pending = SuggestionStore::new().pending(&pool, id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, suggestion_id);
    assert_eq!(pending[0].replacement, "HELLO");
}

#[tokio::test]
async fn suggest_mode_refuses_a_block_insert_without_a_suggestion_form() {
    let (_tmp, pool) = temp_pool().await;
    let id = seed_paragraph(&pool, "x").await;

    let content = serde_json::to_value(BlockContent::Paragraph { text: "y".into() }).unwrap();
    let err = apply_mutation(
        &pool,
        id,
        &DocumentMutation::Insert {
            index: 1,
            block_id: "q".into(),
            content,
        },
        CollaborationMode::Suggest,
        &human(),
    )
    .await
    .unwrap_err();

    assert!(matches!(
        err,
        ApplyError::Denied {
            mode: CollaborationMode::Suggest,
            ..
        }
    ));
    // Nothing was added.
    let doc = DocumentStore::new().load(&pool, id).await.unwrap().unwrap();
    assert_eq!(doc.blocks().unwrap().len(), 1);
}

#[tokio::test]
async fn ask_and_review_modes_deny_content_edits() {
    let (_tmp, pool) = temp_pool().await;
    let id = seed_paragraph(&pool, "hello").await;

    for mode in [CollaborationMode::Ask, CollaborationMode::Review] {
        let err = apply_mutation(
            &pool,
            id,
            &DocumentMutation::EditText {
                block_id: "p".into(),
                position: 0,
                delete_len: 0,
                insert: "x".into(),
            },
            mode,
            &human(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ApplyError::Denied { .. }));
    }
    assert_eq!(block_text(&pool, id).await, "hello");
    assert!(SuggestionStore::new()
        .pending(&pool, id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn annotate_then_accept_applies_exactly_the_range() {
    let (_tmp, pool) = temp_pool().await;
    let id = seed_paragraph(&pool, "hello world").await;

    // An explicit annotation (a proposed range replacement) is always a suggestion.
    let proposed = apply_mutation(
        &pool,
        id,
        &DocumentMutation::Annotate {
            suggestion: SuggestionInput {
                block_id: "p".into(),
                range_start: 6,
                range_end: 11,
                replacement: "WORLD".into(),
                rationale: Some("emphasise".into()),
            },
        },
        CollaborationMode::Suggest,
        &human(),
    )
    .await
    .unwrap();
    let suggestion_id = match &proposed.effect {
        MutationEffect::Suggested(id) => id.clone(),
        other => panic!("expected Suggested, got {other:?}"),
    };

    // Accepting is a resolution — not mode-gated here; it advances the revision
    // and applies exactly the annotated range.
    let accepted = apply_mutation(
        &pool,
        id,
        &DocumentMutation::AcceptSuggestion {
            suggestion_id: suggestion_id.clone(),
        },
        CollaborationMode::Suggest,
        &human(),
    )
    .await
    .unwrap();
    assert_eq!(accepted.effect, MutationEffect::Accepted(suggestion_id));
    assert_eq!(accepted.sync.revision, 2);
    assert_eq!(block_text(&pool, id).await, "hello WORLD");
    assert!(SuggestionStore::new()
        .pending(&pool, id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn reject_leaves_content_unchanged() {
    let (_tmp, pool) = temp_pool().await;
    let id = seed_paragraph(&pool, "keep").await;

    let proposed = apply_mutation(
        &pool,
        id,
        &DocumentMutation::Annotate {
            suggestion: SuggestionInput {
                block_id: "p".into(),
                range_start: 0,
                range_end: 4,
                replacement: "drop".into(),
                rationale: None,
            },
        },
        CollaborationMode::Suggest,
        &human(),
    )
    .await
    .unwrap();
    let suggestion_id = match proposed.effect {
        MutationEffect::Suggested(id) => id,
        other => panic!("expected Suggested, got {other:?}"),
    };

    let rejected = apply_mutation(
        &pool,
        id,
        &DocumentMutation::RejectSuggestion {
            suggestion_id: suggestion_id.clone(),
        },
        CollaborationMode::Suggest,
        &human(),
    )
    .await
    .unwrap();
    assert_eq!(rejected.effect, MutationEffect::Rejected(suggestion_id));
    // Reject touches no content, so the revision is unchanged.
    assert_eq!(rejected.sync.revision, 1);
    assert_eq!(block_text(&pool, id).await, "keep");
    assert!(SuggestionStore::new()
        .pending(&pool, id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_missing_document_is_reported_not_panicked() {
    let (_tmp, pool) = temp_pool().await;
    let err = apply_mutation(
        &pool,
        DocumentId::new(),
        &DocumentMutation::Delete {
            block_id: "p".into(),
        },
        CollaborationMode::Edit,
        &human(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ApplyError::NoSuchDocument(_)));
}
