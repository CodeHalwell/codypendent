//! STEP 4.4 deterministic render + Git publication: the same document revision
//! renders byte-identical Markdown, the publish plan shows target/changed
//! files/Git action before approval, and a publication records the
//! (document revision ↔ git commit) pairing.

use codypendent_knowledge::db;
use codypendent_knowledge::docs::model::{
    BlockContent, ChecklistItem, DocumentAuthor, DocumentBlock, DocumentMetadata,
};
use codypendent_knowledge::docs::render::{
    plan_publication, publications, record_publication, render_document, PublishTarget,
};
use codypendent_knowledge::docs::store::{DocumentStore, NewDocument};
use codypendent_knowledge::Scope;
use codypendent_protocol::UserId;

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

fn sample() -> Vec<DocumentBlock> {
    vec![
        DocumentBlock::with_id(
            "h",
            BlockContent::Heading {
                level: 2,
                text: "Payment Service".into(),
            },
        ),
        DocumentBlock::with_id(
            "p",
            BlockContent::Paragraph {
                text: "Charges customers.".into(),
            },
        ),
        DocumentBlock::with_id(
            "c",
            BlockContent::Code {
                language: Some("rust".into()),
                text: "fn charge() {}".into(),
            },
        ),
        DocumentBlock::with_id(
            "t",
            BlockContent::Table {
                rows: vec![
                    vec!["Field".into(), "Type".into()],
                    vec!["amount".into(), "u64".into()],
                ],
            },
        ),
        DocumentBlock::with_id(
            "cl",
            BlockContent::Checklist {
                items: vec![ChecklistItem {
                    text: "retry".into(),
                    checked: true,
                }],
            },
        ),
        DocumentBlock::with_id(
            "sym",
            BlockContent::EmbeddedSymbol {
                symbol: "payments::charge_customer".into(),
            },
        ),
    ]
}

#[test]
fn render_is_deterministic() {
    let blocks = sample();
    let a = render_document("Runbook", &blocks);
    let b = render_document("Runbook", &blocks);
    assert_eq!(
        a, b,
        "the same revision must render byte-identical Markdown"
    );
}

#[test]
fn render_covers_block_kinds_and_keeps_symbol_markers() {
    let md = render_document("Runbook", &sample());
    assert!(md.starts_with("# Runbook\n"));
    assert!(md.contains("## Payment Service"));
    assert!(md.contains("```rust\nfn charge() {}\n```"));
    assert!(md.contains("| Field | Type |"));
    assert!(md.contains("| --- | --- |"));
    assert!(md.contains("- [x] retry"));
    // The symbol embed keeps its marker verbatim so staleness can resolve it.
    assert!(md.contains("{{ symbol:payments::charge_customer }}"));
}

#[tokio::test]
async fn publish_plan_shows_target_changed_files_and_git_action() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentStore::new();
    let author = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };
    let doc = store
        .create(
            &pool,
            NewDocument {
                title: "Runbook".into(),
                scope: Scope::Repository(codypendent_protocol::RepositoryId::new()),
                metadata: DocumentMetadata::default(),
                blocks: sample(),
            },
            &author,
        )
        .await
        .unwrap();
    let full = store
        .snapshot_document(&pool, doc.id)
        .await
        .unwrap()
        .unwrap();

    let plan = plan_publication(
        &full,
        PublishTarget::DocumentationPr {
            branch: "docs/payment-runbook".into(),
            path: "docs/payment-runbook.md".into(),
            title: "Update payment runbook".into(),
        },
    );
    assert_eq!(
        plan.changed_files,
        vec!["docs/payment-runbook.md".to_string()]
    );
    assert!(plan.git_action.contains("documentation PR"));
    assert!(plan.git_action.contains("docs/payment-runbook.md"));
    assert_eq!(plan.revision, 1);
    // The plan renders exactly what would be committed.
    assert_eq!(plan.rendered, render_document("Runbook", &full.blocks));
    assert_eq!(plan.rendered_hash.len(), 64);
}

#[tokio::test]
async fn publishing_records_revision_to_commit_pairing() {
    let (_tmp, pool) = temp_pool().await;
    let store = DocumentStore::new();
    let author = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };
    let doc = store
        .create(
            &pool,
            NewDocument {
                title: "Runbook".into(),
                scope: Scope::System,
                metadata: DocumentMetadata::default(),
                blocks: sample(),
            },
            &author,
        )
        .await
        .unwrap();
    let full = store
        .snapshot_document(&pool, doc.id)
        .await
        .unwrap()
        .unwrap();
    let plan = plan_publication(
        &full,
        PublishTarget::RepositoryFile {
            path: "docs/runbook.md".into(),
        },
    );

    let published = record_publication(&pool, doc.id, &plan, Some("abc123"))
        .await
        .unwrap();
    assert_eq!(published.revision, 1);
    assert_eq!(published.git_commit.as_deref(), Some("abc123"));

    let history = publications(&pool, doc.id).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].revision, 1);
    assert_eq!(history[0].git_commit.as_deref(), Some("abc123"));
    assert_eq!(history[0].rendered_hash, plan.rendered_hash);
}
