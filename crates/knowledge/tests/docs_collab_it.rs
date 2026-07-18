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
use codypendent_knowledge::docs::store::{DocStoreError, DocumentStore, NewDocument};
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
                source_revision: doc.revision,
                original: "hello".into(),
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
        .accept(&pool, &mut doc, &suggestion.id, &human)
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
                source_revision: doc.revision,
                original: "keep".into(),
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
                source_revision: a.revision,
                original: "x".into(),
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

#[tokio::test]
async fn a_suggestion_cannot_be_accepted_twice() {
    let (_tmp, pool) = temp_pool().await;
    let docs = DocumentStore::new();
    let suggestions = SuggestionStore::new();
    let human = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };

    let mut doc = docs
        .create(
            &pool,
            NewDocument {
                title: "Doc".into(),
                scope: Scope::System,
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

    // Insert " x" at position 5 (an insertion, so a double-apply would duplicate).
    let suggestion = suggestions
        .propose(
            &pool,
            doc.id,
            NewSuggestion {
                block_id: "p".into(),
                range_start: 5,
                range_end: 5,
                source_revision: doc.revision,
                original: String::new(),
                replacement: " x".into(),
                author: human.clone(),
                rationale: None,
            },
        )
        .await
        .unwrap();

    // First accept applies it once.
    suggestions
        .accept(&pool, &mut doc, &suggestion.id, &human)
        .await
        .unwrap();
    assert_eq!(doc.blocks().unwrap()[0].content_text(), "hello x world");

    // A retried accept is rejected — the range is NOT applied a second time.
    let err = suggestions
        .accept(&pool, &mut doc, &suggestion.id, &human)
        .await
        .unwrap_err();
    assert!(matches!(err, DocStoreError::SuggestionNotPending(_)));

    // Content and revision are unchanged by the rejected retry.
    let reloaded = docs.load(&pool, doc.id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.blocks().unwrap()[0].content_text(),
        "hello x world"
    );
    assert_eq!(reloaded.revision, 2);

    // A reject after an accept is likewise refused (already resolved).
    let reject_err = suggestions
        .reject(&pool, doc.id, &suggestion.id, &human)
        .await
        .unwrap_err();
    assert!(matches!(reject_err, DocStoreError::SuggestionNotPending(_)));
}

#[tokio::test]
async fn rejecting_a_suggestion_enqueues_a_document_changed_event() {
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
                scope: Scope::System,
                metadata: DocumentMetadata::default(),
                blocks: vec![DocumentBlock::with_id(
                    "p",
                    BlockContent::Paragraph { text: "hi".into() },
                )],
            },
            &human,
        )
        .await
        .unwrap();
    let s = suggestions
        .propose(
            &pool,
            doc.id,
            NewSuggestion {
                block_id: "p".into(),
                range_start: 0,
                range_end: 2,
                source_revision: doc.revision,
                original: "hi".into(),
                replacement: "yo".into(),
                author: human.clone(),
                rationale: None,
            },
        )
        .await
        .unwrap();
    suggestions
        .reject(&pool, doc.id, &s.id, &human)
        .await
        .unwrap();

    // create + propose + reject each enqueue a DocumentChanged row, so index
    // workers and subscribers see the review rail change on rejection too.
    let rows = codypendent_knowledge::outbox::unprocessed(&pool, 100)
        .await
        .unwrap();
    let n = rows
        .iter()
        .filter(|r| r.event_kind == "document_changed" && r.entity_id == doc.id.to_string())
        .count();
    assert_eq!(n, 3);
}

#[tokio::test]
async fn accept_refuses_a_drifted_range() {
    // A suggestion targets "hello" at 0..5; the block is edited before it is
    // accepted, shifting the range. Accept must refuse rather than corrupt the
    // wrong characters, and leave the suggestion pending.
    let (_tmp, pool) = temp_pool().await;
    let docs = DocumentStore::new();
    let suggestions = SuggestionStore::new();
    let human = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };
    let mut doc = docs
        .create(
            &pool,
            NewDocument {
                title: "Doc".into(),
                scope: Scope::System,
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
    let s = suggestions
        .propose(
            &pool,
            doc.id,
            NewSuggestion {
                block_id: "p".into(),
                range_start: 0,
                range_end: 5,
                source_revision: doc.revision,
                original: "hello".into(),
                replacement: "HELLO".into(),
                author: human.clone(),
                rationale: None,
            },
        )
        .await
        .unwrap();

    // An editor prepends text, shifting the target range (0..5 now covers "X hel").
    doc.crdt.insert_text("p", 0, "X ").unwrap();
    docs.save(&pool, &mut doc, &human, MutationKind::EditText, Some("p"))
        .await
        .unwrap();

    let err = suggestions
        .accept(&pool, &mut doc, &s.id, &human)
        .await
        .unwrap_err();
    assert!(matches!(err, DocStoreError::SuggestionRangeDrifted(_)));

    // The content is not corrupted and the suggestion is still pending.
    assert_eq!(doc.blocks().unwrap()[0].content_text(), "X hello world");
    assert_eq!(suggestions.pending(&pool, doc.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn accept_refuses_a_drifted_insertion() {
    // A zero-length insertion (empty `original`) at offset 5; the block is edited
    // before acceptance. The empty-vs-empty text check cannot detect the shift,
    // but the source-revision guard refuses the stale insert.
    let (_tmp, pool) = temp_pool().await;
    let docs = DocumentStore::new();
    let suggestions = SuggestionStore::new();
    let human = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };
    let mut doc = docs
        .create(
            &pool,
            NewDocument {
                title: "Doc".into(),
                scope: Scope::System,
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
    let s = suggestions
        .propose(
            &pool,
            doc.id,
            NewSuggestion {
                block_id: "p".into(),
                range_start: 5,
                range_end: 5,
                source_revision: doc.revision,
                original: String::new(),
                replacement: " INSERT".into(),
                author: human.clone(),
                rationale: None,
            },
        )
        .await
        .unwrap();

    // A saved edit prepends text, advancing the revision and shifting offset 5.
    doc.crdt.insert_text("p", 0, "XX").unwrap();
    docs.save(&pool, &mut doc, &human, MutationKind::EditText, Some("p"))
        .await
        .unwrap();

    let err = suggestions
        .accept(&pool, &mut doc, &s.id, &human)
        .await
        .unwrap_err();
    assert!(matches!(err, DocStoreError::SuggestionRangeDrifted(_)));
    assert_eq!(doc.blocks().unwrap()[0].content_text(), "XXhello world");
    assert_eq!(suggestions.pending(&pool, doc.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn propose_refuses_a_stale_source_revision() {
    // The proposer computed offsets against revision 1, but the document advances
    // to revision 2 before the suggestion arrives. `propose` must refuse the
    // already-stale offsets rather than record a suggestion anchored to a revision
    // the client never saw — the offsets could point at the wrong characters.
    let (_tmp, pool) = temp_pool().await;
    let docs = DocumentStore::new();
    let suggestions = SuggestionStore::new();
    let human = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };
    let mut doc = docs
        .create(
            &pool,
            NewDocument {
                title: "Doc".into(),
                scope: Scope::System,
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
    let stale_revision = doc.revision;

    // The document advances before the (revision-1) proposal is submitted.
    doc.crdt.insert_text("p", 0, "XX").unwrap();
    docs.save(&pool, &mut doc, &human, MutationKind::EditText, Some("p"))
        .await
        .unwrap();
    assert_eq!(doc.revision, 2);

    let err = suggestions
        .propose(
            &pool,
            doc.id,
            NewSuggestion {
                block_id: "p".into(),
                range_start: 0,
                range_end: 5,
                source_revision: stale_revision,
                original: "hello".into(),
                replacement: "HELLO".into(),
                author: human.clone(),
                rationale: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        DocStoreError::StaleRevision { expected, .. } if expected == stale_revision
    ));

    // Nothing was recorded — no stale suggestion lingers on the review rail.
    assert!(suggestions.pending(&pool, doc.id).await.unwrap().is_empty());
}
