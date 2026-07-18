//! STEP 4.2 collaborative documents: block ↔ CRDT round-trip is lossless,
//! concurrent edits from two replicas converge without loss (exit criterion 1),
//! export→import→export is byte-identical, and every mutation is attributed.

use codypendent_knowledge::db;
use codypendent_knowledge::docs::crdt::{DocCrdtError, DocumentCrdt};
use codypendent_knowledge::docs::model::{
    BlockContent, ChecklistItem, DocumentAuthor, DocumentBlock, DocumentMetadata, MutationKind,
};
use codypendent_knowledge::docs::store::{DocStoreError, DocumentStore, NewDocument};
use codypendent_knowledge::Scope;
use codypendent_protocol::{ModelId, RepositoryId, RunId, UserId};

/// A migrated in-memory-ish pool in a temp dir (mirrors the other IT suites).
async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

/// One block of every kind, to prove the mapping is total and lossless.
fn sample_blocks() -> Vec<DocumentBlock> {
    vec![
        DocumentBlock::with_id(
            "b-h",
            BlockContent::Heading {
                level: 2,
                text: "Payment Service".into(),
            },
        ),
        DocumentBlock::with_id(
            "b-p",
            BlockContent::Paragraph {
                text: "Handles charges.".into(),
            },
        ),
        DocumentBlock::with_id(
            "b-c",
            BlockContent::Code {
                language: Some("rust".into()),
                text: "fn charge() {}".into(),
            },
        ),
        DocumentBlock::with_id(
            "b-d",
            BlockContent::Diagram {
                format: "mermaid".into(),
                source: "graph TD; A-->B".into(),
            },
        ),
        DocumentBlock::with_id(
            "b-t",
            BlockContent::Table {
                rows: vec![vec!["k".into(), "v".into()], vec!["a".into(), "1".into()]],
            },
        ),
        DocumentBlock::with_id(
            "b-cal",
            BlockContent::Callout {
                kind: "warning".into(),
                text: "Be careful.".into(),
            },
        ),
        DocumentBlock::with_id(
            "b-ck",
            BlockContent::Checklist {
                items: vec![ChecklistItem {
                    text: "step".into(),
                    checked: true,
                }],
            },
        ),
        DocumentBlock::with_id(
            "b-q",
            BlockContent::Query {
                query: "status:open".into(),
            },
        ),
        DocumentBlock::with_id(
            "b-ef",
            BlockContent::EmbeddedFile {
                path: "src/lib.rs".into(),
            },
        ),
        DocumentBlock::with_id(
            "b-es",
            BlockContent::EmbeddedSymbol {
                symbol: "payments::charge".into(),
            },
        ),
        DocumentBlock::with_id(
            "b-ew",
            BlockContent::EmbeddedWorkflow {
                workflow: "payments-ci".into(),
            },
        ),
        DocumentBlock::with_id(
            "b-ek",
            BlockContent::EmbeddedSkill {
                skill: "investigate-payment".into(),
            },
        ),
    ]
}

#[test]
fn block_crdt_round_trip_is_lossless() {
    let blocks = sample_blocks();
    let crdt = DocumentCrdt::from_blocks(&blocks).unwrap();
    let read_back = crdt.to_blocks().unwrap();
    assert_eq!(
        blocks, read_back,
        "blocks → CRDT → blocks must be the identity"
    );
}

#[test]
fn export_import_export_is_byte_identical() {
    let blocks = sample_blocks();
    let crdt = DocumentCrdt::from_blocks(&blocks).unwrap();

    // "Export" is the block-structured JSON; "import" rebuilds a fresh CRDT from
    // it; re-exporting must yield identical bytes even though Loro's internal
    // representation differs and ids are preserved.
    let export1 = serde_json::to_vec(&crdt.to_blocks().unwrap()).unwrap();
    let imported =
        DocumentCrdt::from_blocks(&serde_json::from_slice::<Vec<DocumentBlock>>(&export1).unwrap())
            .unwrap();
    let export2 = serde_json::to_vec(&imported.to_blocks().unwrap()).unwrap();
    assert_eq!(
        export1, export2,
        "export→import→export must be byte-identical"
    );
}

#[test]
fn concurrent_text_edits_converge_without_loss() {
    // Two replicas fork from the same snapshot and edit the SAME paragraph block
    // at disjoint positions, plus each makes a structural edit elsewhere.
    let base = DocumentCrdt::from_blocks(&[
        DocumentBlock::with_id(
            "intro",
            BlockContent::Paragraph {
                text: "middle".into(),
            },
        ),
        DocumentBlock::with_id(
            "keep",
            BlockContent::Paragraph {
                text: "keep me".into(),
            },
        ),
    ])
    .unwrap();
    let snapshot = base.snapshot().unwrap();

    let a = DocumentCrdt::from_snapshot(&snapshot).unwrap();
    let b = DocumentCrdt::from_snapshot(&snapshot).unwrap();

    // A prepends, B appends — disjoint character ranges in the same block.
    a.insert_text("intro", 0, "A-").unwrap();
    // B inserts at the end of its current view of the text.
    b.insert_text("intro", "middle".chars().count(), "-B")
        .unwrap();
    // A also appends a brand-new block; B leaves structure alone.
    a.push_block(&DocumentBlock::with_id(
        "added",
        BlockContent::Paragraph { text: "new".into() },
    ))
    .unwrap();

    // Converge in both directions.
    a.merge_snapshot(&b.snapshot().unwrap()).unwrap();
    b.merge_snapshot(&a.snapshot().unwrap()).unwrap();

    let a_blocks = a.to_blocks().unwrap();
    let b_blocks = b.to_blocks().unwrap();
    assert_eq!(
        a_blocks, b_blocks,
        "replicas must converge to identical content"
    );

    // No edit was lost: the intro text carries both concurrent insertions.
    let intro = a_blocks.iter().find(|blk| blk.id == "intro").unwrap();
    let text = intro.content_text();
    assert!(text.starts_with("A-"), "A's prepend survived: {text:?}");
    assert!(text.ends_with("-B"), "B's append survived: {text:?}");
    assert!(text.contains("middle"), "base text survived: {text:?}");
    // A's new block survived the merge into B.
    assert!(a_blocks.iter().any(|blk| blk.id == "added"));
}

#[test]
fn concurrent_block_edits_on_different_blocks_converge() {
    let base = DocumentCrdt::from_blocks(&[
        DocumentBlock::with_id("one", BlockContent::Paragraph { text: "one".into() }),
        DocumentBlock::with_id("two", BlockContent::Paragraph { text: "two".into() }),
    ])
    .unwrap();
    let snapshot = base.snapshot().unwrap();
    let a = DocumentCrdt::from_snapshot(&snapshot).unwrap();
    let b = DocumentCrdt::from_snapshot(&snapshot).unwrap();

    a.replace_text("one", "ONE").unwrap();
    b.replace_text("two", "TWO").unwrap();

    a.merge_snapshot(&b.snapshot().unwrap()).unwrap();
    b.merge_snapshot(&a.snapshot().unwrap()).unwrap();

    assert_eq!(a.to_blocks().unwrap(), b.to_blocks().unwrap());
    let blocks = a.to_blocks().unwrap();
    assert_eq!(
        blocks
            .iter()
            .find(|x| x.id == "one")
            .unwrap()
            .content_text(),
        "ONE"
    );
    assert_eq!(
        blocks
            .iter()
            .find(|x| x.id == "two")
            .unwrap()
            .content_text(),
        "TWO"
    );
}

#[tokio::test]
async fn create_load_round_trips_through_the_store() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentStore::new();
    let author = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };

    let created = store
        .create(
            &pool,
            NewDocument {
                title: "Runbook".into(),
                scope: Scope::Repository(RepositoryId::new()),
                metadata: DocumentMetadata {
                    summary: Some("how to".into()),
                    ..Default::default()
                },
                blocks: sample_blocks(),
            },
            &author,
        )
        .await
        .unwrap();

    let loaded = store.load(&pool, created.id).await.unwrap().unwrap();
    assert_eq!(loaded.title, "Runbook");
    assert_eq!(loaded.revision, 1);
    assert_eq!(loaded.blocks().unwrap(), sample_blocks());
    assert_eq!(loaded.metadata.summary.as_deref(), Some("how to"));
}

#[tokio::test]
async fn every_mutation_is_attributed() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentStore::new();
    let human = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };
    let agent = DocumentAuthor::Agent {
        run_id: RunId::new(),
        model: ModelId("claude-sonnet-5".into()),
        policy_version: "v1".into(),
    };

    let mut doc = store
        .create(
            &pool,
            NewDocument {
                title: "Doc".into(),
                scope: Scope::User(UserId("dev".into())),
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

    // An agent edits the paragraph text.
    doc.crdt.insert_text("p", 2, " there").unwrap();
    let rev = store
        .save(&pool, &mut doc, &agent, MutationKind::EditText, Some("p"))
        .await
        .unwrap();
    assert_eq!(rev, 2);

    let log = store.authorship(&pool, doc.id).await.unwrap();
    assert_eq!(log.len(), 2, "create + one edit");
    assert_eq!(log[0].author, human);
    assert_eq!(log[0].mutation, MutationKind::InsertBlock);
    assert_eq!(log[1].author, agent);
    assert_eq!(log[1].mutation, MutationKind::EditText);
    assert_eq!(log[1].block_id.as_deref(), Some("p"));
    assert_eq!(log[1].revision, 2);

    // The full read model reflects the edited text.
    let full = store
        .snapshot_document(&pool, doc.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(full.blocks[0].content_text(), "hi there");
    assert_eq!(full.authorship.len(), 2);
}

#[tokio::test]
async fn documents_never_leak_across_scopes() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentStore::new();
    let author = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };
    let repo_a = RepositoryId::new();
    let repo_b = RepositoryId::new();

    for (scope, title) in [
        (Scope::Repository(repo_a), "A"),
        (Scope::Repository(repo_b), "B"),
    ] {
        store
            .create(
                &pool,
                NewDocument {
                    title: title.into(),
                    scope,
                    metadata: DocumentMetadata::default(),
                    blocks: vec![],
                },
                &author,
            )
            .await
            .unwrap();
    }

    let only_a = store
        .list(&pool, &[Scope::Repository(repo_a)])
        .await
        .unwrap();
    assert_eq!(only_a.len(), 1);
    assert_eq!(only_a[0].title, "A");
    // An empty scope slice matches nothing.
    assert!(store.list(&pool, &[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn writes_enqueue_document_changed_outbox_rows() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentStore::new();
    let author = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };

    let mut doc = store
        .create(
            &pool,
            NewDocument {
                title: "Doc".into(),
                scope: Scope::System,
                metadata: DocumentMetadata::default(),
                blocks: vec![DocumentBlock::with_id(
                    "p",
                    BlockContent::Paragraph { text: "x".into() },
                )],
            },
            &author,
        )
        .await
        .unwrap();
    doc.crdt.replace_text("p", "y").unwrap();
    store
        .save(&pool, &mut doc, &author, MutationKind::EditText, Some("p"))
        .await
        .unwrap();

    let rows = codypendent_knowledge::outbox::unprocessed(&pool, 100)
        .await
        .unwrap();
    let doc_events: Vec<_> = rows
        .iter()
        .filter(|r| r.event_kind == "document_changed" && r.entity_id == doc.id.to_string())
        .collect();
    assert_eq!(doc_events.len(), 2, "one on create, one on save");
}

#[test]
fn text_ops_out_of_bounds_error_instead_of_panicking() {
    // Loro's text ops panic on out-of-bounds indices; a stale concurrent range
    // must surface a recoverable error, never crash the daemon.
    let crdt = DocumentCrdt::from_blocks(&[DocumentBlock::with_id(
        "p",
        BlockContent::Paragraph { text: "hi".into() },
    )])
    .unwrap();

    // "hi" has length 2; inserting past the end or deleting past the end errors.
    assert!(matches!(
        crdt.insert_text("p", 5, "x"),
        Err(DocCrdtError::OutOfBounds { pos: 5, length: 2 })
    ));
    assert!(matches!(
        crdt.delete_text("p", 0, 5),
        Err(DocCrdtError::OutOfBounds { length: 2, .. })
    ));
    // In-bounds ops still work and the block is intact.
    crdt.insert_text("p", 2, "!").unwrap();
    assert_eq!(crdt.to_blocks().unwrap()[0].content_text(), "hi!");
}

#[tokio::test]
async fn save_guards_against_a_stale_revision() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentStore::new();
    let author = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };
    let created = store
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
            &author,
        )
        .await
        .unwrap();

    // Two replicas load the same revision-1 document.
    let mut first = store.load(&pool, created.id).await.unwrap().unwrap();
    let mut second = store.load(&pool, created.id).await.unwrap().unwrap();

    // The first editor saves, advancing the document to revision 2.
    first.crdt.insert_text("p", 2, " there").unwrap();
    store
        .save(
            &pool,
            &mut first,
            &author,
            MutationKind::EditText,
            Some("p"),
        )
        .await
        .unwrap();

    // The second, still at revision 1, is rejected instead of clobbering.
    second.crdt.insert_text("p", 0, "oops ").unwrap();
    let err = store
        .save(
            &pool,
            &mut second,
            &author,
            MutationKind::EditText,
            Some("p"),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        DocStoreError::StaleRevision { expected: 1, .. }
    ));

    // The first editor's content survived; the stale write was not applied.
    let reloaded = store.load(&pool, created.id).await.unwrap().unwrap();
    assert_eq!(reloaded.revision, 2);
    assert_eq!(reloaded.blocks().unwrap()[0].content_text(), "hi there");
}

#[tokio::test]
async fn set_links_guards_against_a_stale_revision() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentStore::new();
    let author = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };
    let created = store
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
            &author,
        )
        .await
        .unwrap();

    // A content editor advances the document while a resolver holds a revision-1
    // snapshot; the stale link write is rejected rather than persisting links for
    // a version that no longer exists.
    let mut editor = store.load(&pool, created.id).await.unwrap().unwrap();
    let mut resolver = store.load(&pool, created.id).await.unwrap().unwrap();
    editor.crdt.insert_text("p", 2, "!").unwrap();
    store
        .save(
            &pool,
            &mut editor,
            &author,
            MutationKind::EditText,
            Some("p"),
        )
        .await
        .unwrap();

    let err = store
        .set_links(&pool, &mut resolver, Vec::new())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        DocStoreError::StaleRevision { expected: 1, .. }
    ));
}
