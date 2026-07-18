//! STEP 4.6 staleness engine: resolving a document's `{{ symbol:… }}` links
//! against the graph, then flagging exactly the documents whose linked symbols
//! changed signature or disappeared — with a Maintain suggestion citing the
//! causing commit — while leaving unlinked docs untouched.

use codypendent_knowledge::codegraph;
use codypendent_knowledge::db;
use codypendent_knowledge::docs::collab::SuggestionStore;
use codypendent_knowledge::docs::model::{
    BlockContent, DocumentAuthor, DocumentBlock, DocumentMetadata, MutationKind,
};
use codypendent_knowledge::docs::staleness::{
    detect_staleness, resolve_links, symbol_references, StalenessReason,
};
use codypendent_knowledge::docs::store::{DocumentStore, NewDocument};
use codypendent_knowledge::{GitRevision, Scope};
use codypendent_protocol::{RepositoryId, UserId};

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

const V1: &str = "pub fn charge_customer(amount: u32) -> bool { true }";
const V2: &str =
    "pub fn charge_customer(amount: u32, currency: String) -> Result<bool, ()> { Ok(true) }";

fn runbook_blocks() -> Vec<DocumentBlock> {
    vec![
        DocumentBlock::with_id(
            "intro",
            BlockContent::Paragraph {
                text: "The charge path is {{ symbol:charge_customer }} — keep it current.".into(),
            },
        ),
        DocumentBlock::with_id(
            "embed",
            BlockContent::EmbeddedSymbol {
                symbol: "charge_customer".into(),
            },
        ),
    ]
}

#[test]
fn symbol_references_finds_block_and_inline_markers() {
    let refs = symbol_references(&runbook_blocks());
    // The inline marker in the paragraph and the dedicated embed block.
    let names: Vec<&str> = refs.iter().map(|r| r.qualified_name.as_str()).collect();
    assert_eq!(names, ["charge_customer", "charge_customer"]);
    assert!(refs.iter().any(|r| r.block_id.as_deref() == Some("intro")));
    assert!(refs.iter().any(|r| r.block_id.as_deref() == Some("embed")));
}

#[tokio::test]
async fn signature_change_flags_the_linked_document_with_evidence() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let rev1 = GitRevision("rev1".into());
    codegraph::upsert_file_graph(&pool, repo, &rev1, "src/payments.rs", V1)
        .await
        .unwrap();

    // Resolve the runbook's symbol links at rev1.
    let links = resolve_links(&pool, repo, &runbook_blocks(), &rev1)
        .await
        .unwrap();
    assert!(
        links.iter().all(|l| l.resolved.is_some()),
        "both refs resolved"
    );
    let doc_id = codypendent_protocol::DocumentId::new();

    // The symbol's signature changes at rev2.
    let rev2 = GitRevision("rev2".into());
    codegraph::upsert_file_graph(&pool, repo, &rev2, "src/payments.rs", V2)
        .await
        .unwrap();
    let current = codegraph::symbol_snapshot(&pool, repo).await.unwrap();

    let findings = detect_staleness(doc_id, &links, &current, &rev2);
    // One finding per resolved link (inline + embed both reference the symbol).
    assert_eq!(findings.len(), 2);
    for finding in &findings {
        assert_eq!(finding.reason, StalenessReason::SignatureChanged);
        assert_eq!(finding.qualified_name, "charge_customer");
        assert_eq!(finding.revision, "rev2");
        assert!(finding.before_signature != finding.after_signature);
        assert!(finding.after_signature.is_some());
    }
}

#[tokio::test]
async fn disappearance_flags_the_document() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let rev1 = GitRevision("rev1".into());
    codegraph::upsert_file_graph(&pool, repo, &rev1, "src/payments.rs", V1)
        .await
        .unwrap();
    let links = resolve_links(&pool, repo, &runbook_blocks(), &rev1)
        .await
        .unwrap();

    // The symbol is removed entirely at rev2.
    let rev2 = GitRevision("rev2".into());
    codegraph::upsert_file_graph(&pool, repo, &rev2, "src/payments.rs", "pub fn other() {}")
        .await
        .unwrap();
    let current = codegraph::symbol_snapshot(&pool, repo).await.unwrap();

    let findings = detect_staleness(
        codypendent_protocol::DocumentId::new(),
        &links,
        &current,
        &rev2,
    );
    assert!(findings
        .iter()
        .all(|f| f.reason == StalenessReason::Disappeared));
    assert!(findings[0].after_signature.is_none());
}

#[tokio::test]
async fn unlinked_documents_are_untouched() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let rev1 = GitRevision("rev1".into());
    codegraph::upsert_file_graph(&pool, repo, &rev1, "src/payments.rs", V1)
        .await
        .unwrap();

    // A document that references no symbols.
    let plain = vec![DocumentBlock::with_id(
        "p",
        BlockContent::Paragraph {
            text: "No code references here.".into(),
        },
    )];
    let links = resolve_links(&pool, repo, &plain, &rev1).await.unwrap();
    assert!(links.is_empty());

    let rev2 = GitRevision("rev2".into());
    codegraph::upsert_file_graph(&pool, repo, &rev2, "src/payments.rs", V2)
        .await
        .unwrap();
    let current = codegraph::symbol_snapshot(&pool, repo).await.unwrap();
    let findings = detect_staleness(
        codypendent_protocol::DocumentId::new(),
        &links,
        &current,
        &rev2,
    );
    assert!(findings.is_empty(), "unlinked docs are never flagged");
}

#[tokio::test]
async fn maintenance_drafts_a_suggestion_citing_the_commit() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let store = DocumentStore::new();
    let suggestions = SuggestionStore::new();
    let human = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };
    let maintainer = DocumentAuthor::Agent {
        run_id: codypendent_protocol::RunId::new(),
        model: codypendent_protocol::ModelId("claude-sonnet-5".into()),
        policy_version: "v1".into(),
    };

    // A real, persisted document with the runbook blocks.
    let mut doc = store
        .create(
            &pool,
            NewDocument {
                title: "Payment Runbook".into(),
                scope: Scope::Repository(repo),
                metadata: DocumentMetadata::default(),
                blocks: runbook_blocks(),
            },
            &human,
        )
        .await
        .unwrap();

    // Resolve + persist the links at rev1 (a metadata update, no revision bump).
    let rev1 = GitRevision("rev1".into());
    codegraph::upsert_file_graph(&pool, repo, &rev1, "src/payments.rs", V1)
        .await
        .unwrap();
    let links = resolve_links(&pool, repo, &doc.blocks().unwrap(), &rev1)
        .await
        .unwrap();
    store.set_links(&pool, &mut doc, links).await.unwrap();
    assert_eq!(
        doc.revision, 1,
        "resolving links does not bump the revision"
    );

    // rev2 changes the signature; detect staleness against the persisted links.
    let rev2 = GitRevision("rev2".into());
    codegraph::upsert_file_graph(&pool, repo, &rev2, "src/payments.rs", V2)
        .await
        .unwrap();
    let current = codegraph::symbol_snapshot(&pool, repo).await.unwrap();
    let findings = detect_staleness(doc.id, &doc.links, &current, &rev2);
    assert!(!findings.is_empty());

    // Maintain mode drafts a SUGGESTION (never a direct edit) that cites the
    // commit. findings[0] is the inline marker in the "intro" paragraph (a
    // text-bearing block), so a note can be drafted there.
    let blocks = doc.blocks().unwrap();
    let finding = &findings[0];
    let new_suggestion = finding.as_suggestion(maintainer.clone(), &blocks).unwrap();
    assert!(new_suggestion
        .rationale
        .as_deref()
        .unwrap()
        .contains("rev2"));
    let suggestion = suggestions
        .propose(&pool, doc.id, new_suggestion)
        .await
        .unwrap();

    // The document content is unchanged; the suggestion is pending for review.
    let pending = suggestions.pending(&pool, doc.id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, suggestion.id);
    assert!(pending[0]
        .rationale
        .as_deref()
        .unwrap()
        .contains("charge_customer"));
    // Still revision 1 — Maintain proposed, it did not edit.
    let reloaded = store.load(&pool, doc.id).await.unwrap().unwrap();
    assert_eq!(reloaded.revision, 1);
}

#[tokio::test]
async fn staleness_matches_the_resolved_file_not_a_same_named_symbol() {
    // Two files define a symbol with the same qualified name. A link resolves to
    // exactly one of them; a change to the OTHER must not flag the document.
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let rev1 = GitRevision("rev1".into());
    codegraph::upsert_file_graph(
        &pool,
        repo,
        &rev1,
        "src/a.rs",
        "pub fn charge() -> bool { true }",
    )
    .await
    .unwrap();
    codegraph::upsert_file_graph(
        &pool,
        repo,
        &rev1,
        "src/b.rs",
        "pub fn charge() -> bool { true }",
    )
    .await
    .unwrap();

    let blocks = vec![DocumentBlock::with_id(
        "e",
        BlockContent::EmbeddedSymbol {
            symbol: "charge".into(),
        },
    )];
    let links = resolve_links(&pool, repo, &blocks, &rev1).await.unwrap();
    assert_eq!(links.len(), 1);
    let resolved_path = links[0].resolved.as_ref().unwrap().source_path.clone();
    let other_path = if resolved_path == "src/a.rs" {
        "src/b.rs"
    } else {
        "src/a.rs"
    };

    // Change the OTHER same-named symbol; the resolved one is untouched.
    let rev2 = GitRevision("rev2".into());
    codegraph::upsert_file_graph(
        &pool,
        repo,
        &rev2,
        other_path,
        "pub fn charge(x: u32) -> bool { true }",
    )
    .await
    .unwrap();
    let current = codegraph::symbol_snapshot(&pool, repo).await.unwrap();
    let doc_id = codypendent_protocol::DocumentId::new();
    assert!(
        detect_staleness(doc_id, &links, &current, &rev2).is_empty(),
        "a change to a same-named symbol in another file must not flag the doc"
    );

    // Now change the RESOLVED file's symbol — that DOES flag it.
    codegraph::upsert_file_graph(
        &pool,
        repo,
        &rev2,
        &resolved_path,
        "pub fn charge(y: u64) -> bool { true }",
    )
    .await
    .unwrap();
    let current = codegraph::symbol_snapshot(&pool, repo).await.unwrap();
    let findings = detect_staleness(doc_id, &links, &current, &rev2);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].reason, StalenessReason::SignatureChanged);
}

#[tokio::test]
async fn a_content_save_does_not_clobber_resolved_links() {
    // A resolver writes resolved symbol links while an editor holds an earlier
    // snapshot; the editor's content save must not overwrite the resolved links
    // (content saves do not manage links).
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let store = DocumentStore::new();
    let human = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };
    let rev1 = GitRevision("rev1".into());
    codegraph::upsert_file_graph(&pool, repo, &rev1, "src/payments.rs", V1)
        .await
        .unwrap();

    let created = store
        .create(
            &pool,
            NewDocument {
                title: "Runbook".into(),
                scope: Scope::Repository(repo),
                metadata: DocumentMetadata::default(),
                blocks: runbook_blocks(),
            },
            &human,
        )
        .await
        .unwrap();

    // Two replicas at revision 1: an editor and a resolver.
    let mut editor = store.load(&pool, created.id).await.unwrap().unwrap();
    let mut resolver = store.load(&pool, created.id).await.unwrap().unwrap();
    assert!(editor.links.is_empty());

    // The resolver resolves + persists the symbol links (no revision bump).
    let links = resolve_links(&pool, repo, &resolver.blocks().unwrap(), &rev1)
        .await
        .unwrap();
    assert!(!links.is_empty());
    store.set_links(&pool, &mut resolver, links).await.unwrap();

    // The editor, still holding empty links, saves a content edit.
    editor.crdt.insert_text("intro", 0, "x").unwrap();
    store
        .save(
            &pool,
            &mut editor,
            &human,
            MutationKind::EditText,
            Some("intro"),
        )
        .await
        .unwrap();

    // The resolved links survived the content save.
    let reloaded = store.load(&pool, created.id).await.unwrap().unwrap();
    assert!(
        !reloaded.links.is_empty(),
        "a content save must not clobber resolved links"
    );
}

#[tokio::test]
async fn maintenance_skips_non_text_blocks() {
    // A staleness finding on an EmbeddedSymbol block yields no suggestion (a note
    // there would be invisible), while a text-bearing block still drafts one.
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let store = DocumentStore::new();
    let human = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };
    let rev1 = GitRevision("rev1".into());
    codegraph::upsert_file_graph(&pool, repo, &rev1, "src/payments.rs", V1)
        .await
        .unwrap();

    let doc = store
        .create(
            &pool,
            NewDocument {
                title: "Runbook".into(),
                scope: Scope::Repository(repo),
                metadata: DocumentMetadata::default(),
                blocks: runbook_blocks(),
            },
            &human,
        )
        .await
        .unwrap();
    let blocks = doc.blocks().unwrap();
    let links = resolve_links(&pool, repo, &blocks, &rev1).await.unwrap();

    let rev2 = GitRevision("rev2".into());
    codegraph::upsert_file_graph(&pool, repo, &rev2, "src/payments.rs", V2)
        .await
        .unwrap();
    let current = codegraph::symbol_snapshot(&pool, repo).await.unwrap();
    let findings = detect_staleness(doc.id, &links, &current, &rev2);

    let embed = findings
        .iter()
        .find(|f| f.block_id.as_deref() == Some("embed"))
        .unwrap();
    assert!(
        embed.as_suggestion(human.clone(), &blocks).is_none(),
        "an embed block gets no (invisible) inline suggestion"
    );
    let intro = findings
        .iter()
        .find(|f| f.block_id.as_deref() == Some("intro"))
        .unwrap();
    assert!(
        intro.as_suggestion(human.clone(), &blocks).is_some(),
        "a text-bearing block still drafts a suggestion"
    );
}

#[tokio::test]
async fn resolving_links_enqueues_a_document_changed_event() {
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let store = DocumentStore::new();
    let human = DocumentAuthor::Human {
        user: UserId("dev".into()),
    };
    let rev1 = GitRevision("rev1".into());
    codegraph::upsert_file_graph(&pool, repo, &rev1, "src/payments.rs", V1)
        .await
        .unwrap();
    let mut doc = store
        .create(
            &pool,
            NewDocument {
                title: "Runbook".into(),
                scope: Scope::Repository(repo),
                metadata: DocumentMetadata::default(),
                blocks: runbook_blocks(),
            },
            &human,
        )
        .await
        .unwrap();
    let links = resolve_links(&pool, repo, &doc.blocks().unwrap(), &rev1)
        .await
        .unwrap();
    store.set_links(&pool, &mut doc, links).await.unwrap();

    // create + set_links each enqueue a DocumentChanged row, so index workers and
    // subscribers learn the document now has resolved links.
    let rows = codypendent_knowledge::outbox::unprocessed(&pool, 100)
        .await
        .unwrap();
    let n = rows
        .iter()
        .filter(|r| r.event_kind == "document_changed" && r.entity_id == doc.id.to_string())
        .count();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn resolve_prefers_a_real_definition_over_an_external_ref() {
    // An unresolved call synthesizes an ExternalDependency node with the same name
    // (parsed first). A `{{ symbol:charge }}` link must resolve to the real
    // definition — which carries a signature — not the signature-less external ref.
    let (_tmp, pool) = temp_pool().await;
    let repo = RepositoryId::new();
    let rev1 = GitRevision("rev1".into());
    codegraph::upsert_file_graph(
        &pool,
        repo,
        &rev1,
        "src/a.rs",
        "pub fn caller() { charge(); }",
    )
    .await
    .unwrap();
    codegraph::upsert_file_graph(
        &pool,
        repo,
        &rev1,
        "src/payments.rs",
        "pub fn charge() -> bool { true }",
    )
    .await
    .unwrap();

    let blocks = vec![DocumentBlock::with_id(
        "e",
        BlockContent::EmbeddedSymbol {
            symbol: "charge".into(),
        },
    )];
    let links = resolve_links(&pool, repo, &blocks, &rev1).await.unwrap();
    assert_eq!(links.len(), 1);
    let resolved = links[0]
        .resolved
        .as_ref()
        .expect("resolved to the real definition, not the external ref");
    assert_eq!(resolved.source_path, "src/payments.rs");
    assert!(
        resolved.signature_hash.is_some(),
        "the real function carries a signature"
    );
}
