use super::*;
use crate::{MemoryClass, MemoryRecord, RetentionPolicy, Revision};
use codypendent_protocol::{AgentMode, CommandBody, DataClassification, MemoryId};
use std::path::Path;

#[tokio::test]
async fn knowledge_pages_derive_scopes_from_the_session_and_preserve_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().join("repo");
    std::fs::create_dir_all(&repository).unwrap();
    let pool = crate::db::open(&directory.path().join("store.db"))
        .await
        .unwrap();
    let session = SessionId::new();
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO sessions (id, workspace_id, title, state, created_at, updated_at, repository) VALUES (?, ?, 'knowledge', 'active', ?, ?, ?)")
        .bind(session.to_string()).bind(WorkspaceId::new().to_string()).bind(now.to_rfc3339()).bind(now.to_rfc3339())
        .bind(repository.to_string_lossy().to_string()).execute(&pool).await.unwrap();
    let record = |scope, statement: &str| MemoryRecord {
        id: MemoryId::new(),
        class: MemoryClass::Semantic,
        scope,
        statement: statement.to_owned(),
        structured_value: None,
        provenance: vec![],
        confidence: 0.7,
        observed_at: now,
        valid_from: Revision::sequence(1),
        valid_until: None,
        supersedes: vec![],
        sensitivity: DataClassification::Internal,
        retention: RetentionPolicy::default(),
    };
    let own = record(
        Scope::Repository(anchor_repository_id(&repository)),
        "This repository uses an isolated build directory.",
    );
    let system = record(
        Scope::System,
        "Review proposed changes before promoting a patch.",
    );
    let foreign = record(
        Scope::Repository(codypendent_protocol::RepositoryId::new()),
        "Another repository uses a separate convention.",
    );
    for item in [&own, &system, &foreign] {
        MemoryStore::new().insert(&pool, item).await.unwrap();
    }
    let first = read_page(&pool, session, KnowledgeSection::Memories, 0, 1)
        .await
        .unwrap();
    let second = read_page(
        &pool,
        session,
        KnowledgeSection::Memories,
        first.next_offset.unwrap(),
        1,
    )
    .await
    .unwrap();
    let cards: Vec<_> = first.memories.into_iter().chain(second.memories).collect();
    assert_eq!(
        cards
            .iter()
            .map(|card| card.id.as_str())
            .collect::<Vec<_>>(),
        vec![own.id.to_string(), system.id.to_string()]
    );
    assert!(second.next_offset.is_none());
    assert_eq!(
        (cards[0].confidence, cards[0].source.as_str()),
        (0.7, "(no evidence)")
    );
    assert!(read_page(&pool, session, KnowledgeSection::Unknown, 0, 1)
        .await
        .is_err());
    pool.close().await;
}

async fn add_session(pool: &SqlitePool, repository: Option<&Path>) -> SessionId {
    let session = SessionId::new();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO sessions (id, workspace_id, title, state, created_at, updated_at, repository) VALUES (?, ?, 'knowledge', 'open', ?, ?, ?)")
        .bind(session.to_string()).bind(WorkspaceId::new().to_string()).bind(&now).bind(&now)
        .bind(repository.map(|path| path.to_string_lossy().into_owned())).execute(pool).await.unwrap();
    session
}

async fn record_start(pool: &SqlitePool, session: SessionId, repository: &Path, status: &str) {
    let body = CommandBody::StartRun {
        session_id: session,
        objective: "Inspect the repository".to_owned(),
        mode: AgentMode::Ask,
        repository: Some(repository.to_string_lossy().into_owned()),
        model: None,
    };
    record_command(pool, session, body, status).await;
}

async fn record_command(pool: &SqlitePool, session: SessionId, body: CommandBody, status: &str) {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO commands (id, idempotency_key, session_id, client_id, body, status, received_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(&id).bind(&id).bind(session.to_string()).bind("client")
        .bind(serde_json::to_string(&body).unwrap()).bind(status)
        .bind(chrono::Utc::now().to_rfc3339()).execute(pool).await.unwrap();
}

async fn add_document(
    pool: &SqlitePool,
    scope: Scope,
    title: String,
    text: String,
) -> codypendent_protocol::DocumentId {
    DocumentStore::new()
        .create(
            pool,
            crate::NewDocument {
                title,
                scope,
                metadata: crate::DocumentMetadata::default(),
                blocks: vec![DocumentBlock::new(BlockContent::Paragraph { text })],
            },
            &DocumentAuthor::Human { user: local_user() },
        )
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn legacy_and_forked_sessions_recover_only_applied_repository_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let pool = crate::db::open(&directory.path().join("store.db"))
        .await
        .unwrap();
    let repository = directory.path().join("legacy");
    let foreign = directory.path().join("foreign");
    std::fs::create_dir_all(&repository).unwrap();
    std::fs::create_dir_all(&foreign).unwrap();
    let legacy = add_session(&pool, None).await;
    let fork = add_session(&pool, None).await;
    sqlx::query("UPDATE sessions SET forked_from_session_id = ? WHERE id = ?")
        .bind(legacy.to_string())
        .bind(fork.to_string())
        .execute(&pool)
        .await
        .unwrap();
    record_start(&pool, legacy, &repository, "applied").await;
    record_start(&pool, legacy, &foreign, "received").await;
    record_command(
        &pool,
        legacy,
        CommandBody::StartWorkflow {
            manifest: String::new(),
            workflow_id: None,
            inputs: serde_json::json!({ "type": "StartRun" }),
            repository: None,
        },
        "applied",
    )
    .await;
    let own_document = add_document(
        &pool,
        Scope::Repository(anchor_repository_id(&repository)),
        "Legacy note".to_owned(),
        "Visible repository knowledge".to_owned(),
    )
    .await;
    add_document(
        &pool,
        Scope::Repository(anchor_repository_id(&foreign)),
        "Foreign note".to_owned(),
        "Separate repository knowledge".to_owned(),
    )
    .await;
    for session in [legacy, fork] {
        let page = read_page(&pool, session, KnowledgeSection::Documents, 0, 100)
            .await
            .unwrap();
        assert_eq!(page.repository.as_deref(), repository.to_str());
        assert!(page.workspace_id.is_some());
        assert_eq!(page.documents.len(), 1);
        assert_eq!(page.documents[0].document_id, own_document);
    }
    let durable: Option<String> =
        sqlx::query_scalar("SELECT repository FROM sessions WHERE id = ?")
            .bind(legacy.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(durable.is_none());
    let modern = add_session(&pool, Some(&foreign)).await;
    record_start(&pool, modern, &repository, "applied").await;
    assert_eq!(
        session_identity(&pool, modern).await.unwrap().repository,
        Some(foreign.clone())
    );
    assert!(session_identity(&pool, SessionId::new()).await.is_err());
    record_command(
        &pool,
        legacy,
        CommandBody::StartRun {
            session_id: SessionId::new(),
            objective: "Invalid identity".to_owned(),
            mode: AgentMode::Ask,
            repository: Some(foreign.to_string_lossy().into_owned()),
            model: None,
        },
        "applied",
    )
    .await;
    assert!(session_identity(&pool, legacy)
        .await
        .unwrap_err()
        .to_string()
        .contains("identity mismatch"));
    pool.close().await;
}

#[tokio::test]
async fn document_pages_fit_the_byte_budget_without_losing_or_reordering_cards() {
    let directory = tempfile::tempdir().unwrap();
    let pool = crate::db::open(&directory.path().join("store.db"))
        .await
        .unwrap();
    let session = add_session(&pool, None).await;
    let mut ids = Vec::new();
    for index in 0..100 {
        ids.push(
            add_document(
                &pool,
                Scope::System,
                format!("Document {index}"),
                "a".repeat(50 * 1024),
            )
            .await,
        );
    }
    ids.reverse();
    let mut actual = Vec::new();
    let mut offset = 0;
    let mut pages = 0;
    loop {
        let page = read_page(&pool, session, KnowledgeSection::Documents, offset, 100)
            .await
            .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_PAGE_BYTES);
        let count = page.documents.len() as u32;
        actual.extend(page.documents.into_iter().map(|card| card.document_id));
        pages += 1;
        let Some(next) = page.next_offset else {
            break;
        };
        assert_eq!(next, offset + count);
        offset = next;
    }
    assert_eq!(actual, ids);
    assert!(pages > 1);
    pool.close().await;
}

#[test]
fn an_individually_oversized_card_is_rejected_without_a_nonprogressing_page() {
    let mut next = None;
    let error = paging::slice(
        vec!["a".repeat(MAX_PAGE_BYTES)],
        0,
        100,
        MAX_PAGE_BYTES - 64,
        &mut next,
    )
    .unwrap_err();
    assert!(error.to_string().contains("single knowledge card"));
    assert!(next.is_none());
}
