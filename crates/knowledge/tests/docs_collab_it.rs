//! STEP 4.3 collaboration modes + suggestions: Suggest mode cannot mutate
//! content directly (policy), org-scope docs default to Suggest, and accepting a
//! suggestion applies exactly the annotated range.

use codypendent_knowledge::db;
use codypendent_knowledge::docs::collab::{
    CollaborationMode, EditDisposition, NewSuggestion, SuggestionStore,
};
use codypendent_knowledge::docs::model::{
    BlockContent, DocumentAuthor, DocumentBlock, DocumentMetadata, MutationKind,
};
use codypendent_knowledge::docs::store::{DocumentStore, NewDocument};
use codypendent_knowledge::Scope;
use codypendent_protocol::{ModelId, OrganizationId, RunId, UserId};

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

#[test]
fn suggest_mode_cannot_edit_directly_and_org_defaults_to_suggest() {
    // Policy: only Edit permits a direct CRDT mutation.
    assert_eq!(
        CollaborationMode::Edit.disposition(),
        EditDisposition::Direct
    );
    assert!(CollaborationMode::Edit.allows_direct_edit());

    assert_eq!(
        CollaborationMode::Suggest.disposition(),
        EditDisposition::Suggest
    );
    assert!(!CollaborationMode::Suggest.allows_direct_edit());

    // Co-author and Maintain also route through suggestions.
    assert_eq!(
        CollaborationMode::CoAuthor.disposition(),
        EditDisposition::Suggest
    );
    assert_eq!(
        CollaborationMode::Maintain.disposition(),
        EditDisposition::Suggest
    );

    // Ask/Review may not touch content at all.
    assert_eq!(
        CollaborationMode::Ask.disposition(),
        EditDisposition::Denied
    );
    assert_eq!(
        CollaborationMode::Review.disposition(),
        EditDisposition::Denied
    );

    // Organization-scope documentation defaults to Suggest; personal to Edit.
    assert_eq!(
        CollaborationMode::default_for_scope(&Scope::Organization(OrganizationId::new())),
        CollaborationMode::Suggest
    );
    assert_eq!(
        CollaborationMode::default_for_scope(&Scope::User(UserId("dev".into()))),
        CollaborationMode::Edit
    );
}

#[tokio::test]
async fn accept_applies_exactly_the_annotated_range() {
    let (_tmp, pool) = temp_pool().await;
    let docs = DocumentStore::new();
    let suggestions = SuggestionStore::new();
    let human = DocumentAuthor::Human {
        user: UserId("reviewer".into()),
    };
    let agent = DocumentAuthor::Agent {
        run_id: RunId::new(),
        model: ModelId("claude-sonnet-5".into()),
        policy_version: "v1".into(),
    };

    let mut doc = docs
        .create(
            &pool,
            NewDocument {
                title: "Doc".into(),
                scope: Scope::Organization(OrganizationId::new()),
                metadata: DocumentMetadata::default(),
                blocks: vec![DocumentBlock::with_id(
                    "p",
                    BlockContent::Paragraph {
                        text: "hello world".into(),
                    },
                )],
            },
            &human,
        )
        .await
        .unwrap();

    // The agent (in Suggest mode for an org doc) proposes replacing "hello"
    // (chars 0..5) with "HELLO" — recorded as data, changing nothing yet.
    let suggestion = suggestions
        .propose(
            &pool,
            doc.id,
            NewSuggestion {
                block_id: "p".into(),
                range_start: 0,
                range_end: 5,
                replacement: "HELLO".into(),
                author: agent.clone(),
                rationale: Some("emphasise".into()),
            },
        )
        .await
        .unwrap();

    // Content is unchanged while the suggestion is pending.
    assert_eq!(doc.blocks().unwrap()[0].content_text(), "hello world");
    assert_eq!(suggestions.pending(&pool, doc.id).await.unwrap().len(), 1);

    // A human accepts it — exactly the annotated range is applied.
    let rev = suggestions
        .accept(&pool, &docs, &mut doc, &suggestion.id, &human)
        .await
        .unwrap();
    assert_eq!(rev, 2);
    assert_eq!(doc.blocks().unwrap()[0].content_text(), "HELLO world");

    // The suggestion is no longer pending, and the accept is attributed.
    assert!(suggestions.pending(&pool, doc.id).await.unwrap().is_empty());
    let log = docs.authorship(&pool, doc.id).await.unwrap();
    let accept = log.last().unwrap();
    assert_eq!(accept.mutation, MutationKind::AcceptSuggestion);
    assert_eq!(accept.author, human);
    assert_eq!(accept.block_id.as_deref(), Some("p"));

    // Reloading from storage shows the applied edit (persisted, not just in-memory).
    let reloaded = docs.load(&pool, doc.id).await.unwrap().unwrap();
    assert_eq!(reloaded.blocks().unwrap()[0].content_text(), "HELLO world");
}

#[tokio::test]
async fn reject_leaves_content_unchanged() {
    let (_tmp, pool) = temp_pool().await;
    let docs = DocumentStore::new();
    let suggestions = SuggestionStore::new();
    let human = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };

    let doc = docs
        .create(
            &pool,
            NewDocument {
                title: "Doc".into(),
                scope: Scope::User(UserId("dev".into())),
                metadata: DocumentMetadata::default(),
                blocks: vec![DocumentBlock::with_id(
                    "p",
                    BlockContent::Paragraph {
                        text: "keep".into(),
                    },
                )],
            },
            &human,
        )
        .await
        .unwrap();

    let suggestion = suggestions
        .propose(
            &pool,
            doc.id,
            NewSuggestion {
                block_id: "p".into(),
                range_start: 0,
                range_end: 4,
                replacement: "drop".into(),
                author: human.clone(),
                rationale: None,
            },
        )
        .await
        .unwrap();

    suggestions
        .reject(&pool, doc.id, &suggestion.id, &human)
        .await
        .unwrap();

    // No pending suggestions and the document is untouched (still revision 1).
    assert!(suggestions.pending(&pool, doc.id).await.unwrap().is_empty());
    let reloaded = docs.load(&pool, doc.id).await.unwrap().unwrap();
    assert_eq!(reloaded.revision, 1);
    assert_eq!(reloaded.blocks().unwrap()[0].content_text(), "keep");
}

#[tokio::test]
async fn pending_suggestions_are_scoped_to_their_document() {
    let (_tmp, pool) = temp_pool().await;
    let docs = DocumentStore::new();
    let suggestions = SuggestionStore::new();
    let human = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };

    let make = |title: &str| NewDocument {
        title: title.into(),
        scope: Scope::System,
        metadata: DocumentMetadata::default(),
        blocks: vec![DocumentBlock::with_id(
            "p",
            BlockContent::Paragraph { text: "x".into() },
        )],
    };
    let a = docs.create(&pool, make("A"), &human).await.unwrap();
    let b = docs.create(&pool, make("B"), &human).await.unwrap();

    suggestions
        .propose(
            &pool,
            a.id,
            NewSuggestion {
                block_id: "p".into(),
                range_start: 0,
                range_end: 1,
                replacement: "y".into(),
                author: human.clone(),
                rationale: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(suggestions.pending(&pool, a.id).await.unwrap().len(), 1);
    assert!(suggestions.pending(&pool, b.id).await.unwrap().is_empty());
}
