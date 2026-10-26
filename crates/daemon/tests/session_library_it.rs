//! Session Library integration coverage: ranked owner-scoped reads over the
//! migrated daemon database. Socket dispatch is covered in `server_it` once
//! this service contract is established.

use codypendent_daemon::{db, principal::PeerPrincipal, session_library};
use codypendent_protocol::{
    Actor, AgentMode, ArtifactRef, ChangeSetId, ClientId, CommandBody, CommandId,
    DataClassification, EventBody, InputBlock, InputEnvelope, InputSource, ModelId, RepositoryId,
    RunId, RunState, ScopeLevel, SessionDeepLink, SessionEvent, SessionId, SessionSearchFilters,
    SessionSearchQuery, SessionSearchSource, SymbolRef, WorkflowId,
};
use sqlx::SqlitePool;

struct SessionFixture<'a> {
    id: SessionId,
    owner_uid: Option<u32>,
    title: &'a str,
    updated_at: &'a str,
    repository_id: Option<RepositoryId>,
}

async fn insert_session(pool: &SqlitePool, fixture: SessionFixture<'_>) {
    sqlx::query(
        "INSERT INTO sessions \
         (id, title, state, created_at, updated_at, revision, owner_uid, repository_id) \
         VALUES (?, ?, 'open', ?, ?, 0, ?, ?)",
    )
    .bind(fixture.id.to_string())
    .bind(fixture.title)
    .bind(fixture.updated_at)
    .bind(fixture.updated_at)
    .bind(fixture.owner_uid.map(i64::from))
    .bind(fixture.repository_id.map(|id| id.to_string()))
    .execute(pool)
    .await
    .expect("insert session fixture");
}

fn query(text: &str, limit: u32) -> SessionSearchQuery {
    SessionSearchQuery {
        query: text.to_string(),
        filters: SessionSearchFilters::default(),
        limit,
        cursor: None,
    }
}

#[tokio::test]
async fn reasoning_streams_commit_and_rebuild_without_losing_transcript_sources() {
    let temp = tempfile::tempdir().unwrap();
    let pool = db::open_database(&temp.path().join("reasoning.db"))
        .await
        .unwrap();
    let session = SessionId::new();
    let run = RunId::new();
    codypendent_daemon::ledger::create_session(&pool, session, "Reasoning stream")
        .await
        .unwrap();
    for (sequence, thought) in [(1, true), (2, false)] {
        codypendent_daemon::ledger::append_event(
            &pool,
            session,
            &SessionEvent {
                sequence,
                occurred_at: "2026-10-19T10:00:00Z".parse().unwrap(),
                causation_id: None,
                correlation_id: None,
                actor: Actor::System,
                body: EventBody::ModelStreamDelta {
                    run_id: run,
                    text: format!("streamed fragment {sequence}"),
                    thought,
                },
            },
        )
        .await
        .unwrap();
    }
    let load = || {
        sqlx::query_as::<_, (String, String, Option<i64>)>(
        "SELECT source_type, source_id, event_sequence FROM session_search_sources ORDER BY source_type, source_id"
    ).fetch_all(&pool)
    };
    let before = load().await.unwrap();
    assert!(
        before
            .iter()
            .any(|(kind, _, seq)| kind == "reasoning" && *seq == Some(1))
    );
    assert!(
        before
            .iter()
            .any(|(kind, _, seq)| kind == "transcript" && *seq == Some(2))
    );
    session_library::rebuild_search_sources(&pool)
        .await
        .unwrap();
    assert_eq!(before, load().await.unwrap());
    assert_eq!(
        codypendent_daemon::ledger::load_events(&pool, session)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn title_page_pruning_preserves_transcript_ties_and_later_pages() {
    let temp = tempfile::tempdir().unwrap();
    let pool = db::open_database(&temp.path().join("rank-pruning.db"))
        .await
        .unwrap();
    let repository = RepositoryId::new();
    let first = SessionId::new();
    let second = SessionId::new();
    let token = SessionId::new();
    for (id, owner, title, repo) in [
        (first, 1000, "needle first", repository),
        (second, 1000, "needle second", repository),
        (token, 1000, "a needle token", repository),
        (SessionId::new(), 1001, "needle", repository),
        (SessionId::new(), 1000, "needle", RepositoryId::new()),
    ] {
        insert_session(
            &pool,
            SessionFixture {
                id,
                owner_uid: Some(owner),
                title,
                updated_at: "2026-08-17T10:00:00Z",
                repository_id: Some(repo),
            },
        )
        .await;
    }
    sqlx::query("UPDATE sessions SET pinned = 1 WHERE id = ?")
        .bind(token.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let run_id = RunId::new();
    for (index, body) in [
        EventBody::NoteAppended {
            text: "needle".into(),
            run_id: None,
        },
        EventBody::ModelStreamDelta {
            run_id,
            text: "needle".into(),
            thought: false,
        },
        EventBody::PatchProposed {
            run_id,
            changeset_id: ChangeSetId::new(),
            artifact: ArtifactRef {
                id: codypendent_protocol::ArtifactId::new(),
                media_type: "text/x-diff".into(),
                byte_length: 1,
                sha256: "a".repeat(64),
                sensitivity: DataClassification::Internal,
            },
            files: vec![],
            additions: 1,
            deletions: 0,
            preview: "needle".into(),
            preview_truncated: false,
        },
    ]
    .into_iter()
    .enumerate()
    {
        codypendent_daemon::ledger::append_event(
            &pool,
            first,
            &SessionEvent {
                sequence: index as u64 + 1,
                occurred_at: chrono::Utc::now(),
                causation_id: None,
                correlation_id: None,
                actor: Actor::System,
                body,
            },
        )
        .await
        .unwrap();
    }
    let mut search = query("needle", 100);
    search.filters.repository_ids = vec![repository];
    let complete =
        session_library::search_sessions(&pool, 1000, PeerPrincipal::from_uid(1000), &search)
            .await
            .unwrap();
    assert_eq!(
        complete
            .items
            .iter()
            .map(|item| item.score)
            .collect::<Vec<_>>(),
        vec![900.0, 800.0, 800.0, 700.0, 700.0, 650.0]
    );
    assert_eq!(
        complete.items[3].session.session_id, token,
        "pinned title wins the exact 700 tie"
    );
    assert_eq!(complete.items[4].source, SessionSearchSource::Transcript);
    let expected = complete
        .items
        .into_iter()
        .map(|item| item.stable_identity)
        .collect::<Vec<_>>();

    search.limit = 1;
    let mut paged = Vec::new();
    for _ in 0..10 {
        let page =
            session_library::search_sessions(&pool, 1000, PeerPrincipal::from_uid(1000), &search)
                .await
                .unwrap();
        assert_eq!(page.items.len(), 1);
        paged.push(page.items[0].stable_identity.clone());
        search.cursor = page.next_cursor;
        if search.cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        paged, expected,
        "title pruning preserves every later transcript and cursor boundary"
    );
}

#[tokio::test]
async fn title_search_is_ranked_filtered_and_cursor_paged_without_overlap() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::open_database(&temp.path().join("library.db"))
        .await
        .expect("open database");
    let repository_id = RepositoryId::new();
    let exact = SessionId::new();
    let prefix = SessionId::new();
    let token = SessionId::new();
    let other_repository = SessionId::new();

    for fixture in [
        SessionFixture {
            id: exact,
            owner_uid: Some(1000),
            title: "parser",
            updated_at: "2026-08-17T10:00:00Z",
            repository_id: Some(repository_id),
        },
        SessionFixture {
            id: prefix,
            owner_uid: Some(1000),
            title: "Parser performance",
            updated_at: "2026-08-17T11:00:00Z",
            repository_id: Some(repository_id),
        },
        SessionFixture {
            id: token,
            owner_uid: Some(1000),
            title: "Rust parser notes",
            updated_at: "2026-08-17T12:00:00Z",
            repository_id: Some(repository_id),
        },
        SessionFixture {
            id: other_repository,
            owner_uid: Some(1000),
            title: "parser",
            updated_at: "2026-08-17T13:00:00Z",
            repository_id: Some(RepositoryId::new()),
        },
    ] {
        insert_session(&pool, fixture).await;
    }

    let mut first_query = query("parser", 2);
    first_query.filters.repository_ids = vec![repository_id];
    let first =
        session_library::search_sessions(&pool, 1000, PeerPrincipal::from_uid(1000), &first_query)
            .await
            .expect("first search page");

    assert_eq!(
        first
            .items
            .iter()
            .map(|item| item.session.session_id)
            .collect::<Vec<_>>(),
        vec![exact, prefix]
    );
    assert!(first.items[0].score > first.items[1].score);
    assert!(first
        .items
        .iter()
        .all(|item| item.source == SessionSearchSource::Title));
    assert_eq!(
        first.items[0].deep_link,
        SessionDeepLink::Session { session_id: exact }
    );
    assert_eq!(
        first.items[0].stable_identity,
        format!("session:{exact}:title")
    );

    let mut second_query = first_query;
    second_query.cursor = first.next_cursor.clone();
    let second =
        session_library::search_sessions(&pool, 1000, PeerPrincipal::from_uid(1000), &second_query)
            .await
            .expect("second search page");
    assert_eq!(
        second
            .items
            .iter()
            .map(|item| item.session.session_id)
            .collect::<Vec<_>>(),
        vec![token]
    );
    assert!(second.next_cursor.is_none());
}

#[tokio::test]
async fn search_applies_principal_scope_before_returning_results() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::open_database(&temp.path().join("owners.db"))
        .await
        .expect("open database");
    let own = SessionId::new();
    let other = SessionId::new();
    let legacy = SessionId::new();

    for fixture in [
        SessionFixture {
            id: own,
            owner_uid: Some(1000),
            title: "ownership probe",
            updated_at: "2026-08-17T10:00:00Z",
            repository_id: None,
        },
        SessionFixture {
            id: other,
            owner_uid: Some(1001),
            title: "ownership probe",
            updated_at: "2026-08-17T11:00:00Z",
            repository_id: None,
        },
        SessionFixture {
            id: legacy,
            owner_uid: None,
            title: "ownership probe",
            updated_at: "2026-08-17T12:00:00Z",
            repository_id: None,
        },
    ] {
        insert_session(&pool, fixture).await;
    }

    let search = query("ownership", 20);
    let daemon_owner =
        session_library::search_sessions(&pool, 1000, PeerPrincipal::from_uid(1000), &search)
            .await
            .expect("daemon owner search");
    let daemon_owner_ids = daemon_owner
        .items
        .iter()
        .map(|item| item.session.session_id)
        .collect::<Vec<_>>();
    assert!(daemon_owner_ids.contains(&own));
    assert!(daemon_owner_ids.contains(&legacy));
    assert!(!daemon_owner_ids.contains(&other));

    let second_owner =
        session_library::search_sessions(&pool, 1000, PeerPrincipal::from_uid(1001), &search)
            .await
            .expect("second owner search");
    assert_eq!(
        second_owner
            .items
            .iter()
            .map(|item| item.session.session_id)
            .collect::<Vec<_>>(),
        vec![other]
    );
}

#[tokio::test]
async fn durable_events_produce_transcript_tool_patch_path_and_artifact_hits() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::open_database(&temp.path().join("event-sources.db"))
        .await
        .expect("open database");
    let session_id = SessionId::new();
    let run_id = RunId::new();
    let patch_artifact = ArtifactRef {
        id: codypendent_protocol::ArtifactId::new(),
        media_type: "text/x-diff".to_string(),
        byte_length: 512,
        sha256: "a".repeat(64),
        sensitivity: DataClassification::Internal,
    };
    insert_session(
        &pool,
        SessionFixture {
            id: session_id,
            owner_uid: Some(1000),
            title: "Compiler investigation",
            updated_at: "2026-08-17T12:00:00Z",
            repository_id: None,
        },
    )
    .await;

    for (sequence, body) in [
        (
            1,
            EventBody::NoteAppended {
                text: "The tokenizer drops unicode combining marks".to_string(),
                run_id: Some(run_id),
            },
        ),
        (
            2,
            EventBody::ToolStarted {
                run_id,
                tool: "workspace.read_file".to_string(),
                args_digest: "digest".to_string(),
                label: Some("src/parser/tokenizer.rs".to_string()),
            },
        ),
        (
            3,
            EventBody::PatchProposed {
                run_id,
                changeset_id: ChangeSetId::new(),
                artifact: patch_artifact.clone(),
                files: vec!["src/parser/tokenizer.rs".to_string()],
                additions: 12,
                deletions: 2,
                preview: "normalize unicode combining marks before tokenizing".to_string(),
                preview_truncated: false,
            },
        ),
    ] {
        codypendent_daemon::ledger::append_event(
            &pool,
            session_id,
            &SessionEvent {
                sequence,
                occurred_at: chrono::Utc::now(),
                causation_id: None,
                correlation_id: None,
                actor: Actor::System,
                body,
            },
        )
        .await
        .expect("append event fixture");
    }

    async fn sources_for(
        pool: &SqlitePool,
        text: &str,
    ) -> Vec<codypendent_protocol::SessionSearchResult> {
        session_library::search_sessions(
            pool,
            1000,
            PeerPrincipal::from_uid(1000),
            &query(text, 20),
        )
        .await
        .expect("event search")
        .items
    }

    let transcript = sources_for(&pool, "combining marks").await;
    assert!(transcript
        .iter()
        .any(|item| item.source == SessionSearchSource::Transcript));

    let tools = sources_for(&pool, "workspace.read_file").await;
    assert!(tools
        .iter()
        .any(|item| item.source == SessionSearchSource::ToolObservation));

    let patches = sources_for(&pool, "normalize unicode").await;
    assert!(patches.iter().any(|item| {
        item.source == SessionSearchSource::Patch
            && item.deep_link
                == (SessionDeepLink::Event {
                    session_id,
                    sequence: 3,
                })
    }));

    let paths = sources_for(&pool, "tokenizer.rs").await;
    assert!(paths.iter().any(|item| {
        item.source == SessionSearchSource::ChangedPath
            && item.deep_link
                == (SessionDeepLink::Path {
                    session_id,
                    path: "src/parser/tokenizer.rs".to_string(),
                    line: None,
                    column: None,
                })
    }));

    let artifacts = sources_for(&pool, "text/x-diff").await;
    assert!(artifacts.iter().any(|item| {
        item.source == SessionSearchSource::Artifact
            && item.deep_link
                == (SessionDeepLink::Artifact {
                    session_id,
                    artifact_id: patch_artifact.id,
                })
    }));
}

#[tokio::test]
async fn typed_input_symbols_remain_searchable_with_symbol_deep_links() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::open_database(&temp.path().join("symbols.db"))
        .await
        .expect("open database");
    let session_id = SessionId::new();
    insert_session(
        &pool,
        SessionFixture {
            id: session_id,
            owner_uid: Some(1000),
            title: "Symbol investigation",
            updated_at: "2026-08-17T12:00:00Z",
            repository_id: None,
        },
    )
    .await;
    let body = CommandBody::SubmitUserInput {
        session_id,
        text: "explain this symbol".to_string(),
        mode: AgentMode::Ask,
        model: None,
        envelope: Some(InputEnvelope {
            source: InputSource::Ide,
            blocks: vec![InputBlock::CodeSymbol(SymbolRef {
                path: "src/workflow/driver.rs".to_string(),
                symbol: "WorkflowDriver::advance".to_string(),
                kind: Some("method".to_string()),
                line: Some(412),
            })],
            scope: ScopeLevel::Session,
            attachments: Vec::new(),
        }),
    };
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO commands \
         (id, idempotency_key, session_id, client_id, body, status, result_json, received_at, applied_at) \
         VALUES (?, ?, ?, ?, ?, 'applied', '{}', ?, ?)",
    )
    .bind(CommandId::new().to_string())
    .bind("symbol-input")
    .bind(session_id.to_string())
    .bind(ClientId::new().to_string())
    .bind(serde_json::to_string(&body).expect("serialize command body"))
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .expect("insert command fixture");
    session_library::rebuild_search_sources(&pool)
        .await
        .expect("rebuild typed command sources");
    let (symbol_sources,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM session_search_sources \
         WHERE session_id = ? AND source_type = 'symbol'",
    )
    .bind(session_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("count rebuilt symbol source");
    assert_eq!(symbol_sources, 1);

    let results = session_library::search_sessions(
        &pool,
        1000,
        PeerPrincipal::from_uid(1000),
        &query("WorkflowDriver::advance", 20),
    )
    .await
    .expect("symbol search");
    assert!(results.items.iter().any(|item| {
        item.source == SessionSearchSource::Symbol
            && item.deep_link
                == (SessionDeepLink::Symbol {
                    session_id,
                    symbol: "WorkflowDriver::advance".to_string(),
                    path: Some("src/workflow/driver.rs".to_string()),
                })
    }));
}

#[tokio::test]
async fn workflow_model_date_and_run_state_filters_apply_together() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::open_database(&temp.path().join("filters.db"))
        .await
        .expect("open database");
    let session_id = SessionId::new();
    let repository_id = RepositoryId::new();
    insert_session(
        &pool,
        SessionFixture {
            id: session_id,
            owner_uid: Some(1000),
            title: "filtered parser run",
            updated_at: "2026-08-17T12:00:00Z",
            repository_id: Some(repository_id),
        },
    )
    .await;
    let run_id = RunId::new();
    sqlx::query(
        "INSERT INTO runs \
         (id, session_id, objective, state, mode, model_policy, budget_json) \
         VALUES (?, ?, 'parser', 'Running', 'Build', 'default', '{}')",
    )
    .bind(run_id.to_string())
    .bind(session_id.to_string())
    .execute(&pool)
    .await
    .expect("insert run fixture");
    let workflow_id = WorkflowId::new();
    sqlx::query(
        "INSERT INTO workflow_runs \
         (id, workflow_id, workflow_version, graph_signature, run_id, inputs_json, state, \
          created_at, updated_at) \
         VALUES (?, ?, 1, 'signature', ?, '{}', 'running', ?, ?)",
    )
    .bind("workflow-run-filter")
    .bind(workflow_id.to_string())
    .bind(run_id.to_string())
    .bind("2026-08-17T12:00:00Z")
    .bind("2026-08-17T12:00:00Z")
    .execute(&pool)
    .await
    .expect("insert workflow fixture");
    let model_id = ModelId("model-filter".to_string());
    sqlx::query(
        "INSERT INTO model_task_outcomes \
         (model_id, endpoint, task_class, success, run_id, recorded_at) \
         VALUES (?, 'local', 'general', 1, ?, ?)",
    )
    .bind(model_id.to_string())
    .bind(run_id.to_string())
    .bind("2026-08-17T12:00:00Z")
    .execute(&pool)
    .await
    .expect("insert model outcome fixture");

    let mut search = query("parser", 20);
    search.filters.repository_ids = vec![repository_id];
    search.filters.workflow_ids = vec![workflow_id];
    search.filters.model_ids = vec![model_id];
    search.filters.run_states = vec![RunState::Running];
    search.filters.created_after = Some(
        "2026-08-17T11:00:00Z"
            .parse()
            .expect("created-after timestamp"),
    );
    search.filters.created_before = Some(
        "2026-08-17T13:00:00Z"
            .parse()
            .expect("created-before timestamp"),
    );
    let page =
        session_library::search_sessions(&pool, 1000, PeerPrincipal::from_uid(1000), &search)
            .await
            .expect("combined filtered search");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].session.session_id, session_id);
    assert_eq!(page.items[0].session.last_run_id, Some(run_id));
    assert_eq!(page.items[0].session.run_state, Some(RunState::Running));

    search.filters.run_states = vec![RunState::Completed];
    assert!(
        session_library::search_sessions(&pool, 1000, PeerPrincipal::from_uid(1000), &search,)
            .await
            .expect("nonmatching run-state filter")
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn default_browse_omits_archived_internal_sessions_but_explicit_search_can_find_them() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::open_database(&temp.path().join("visibility.db"))
        .await
        .expect("open database");
    let visible = SessionId::new();
    let archived = SessionId::new();
    let internal = SessionId::new();
    for (id, title) in [
        (visible, "visible parser"),
        (archived, "archived parser"),
        (internal, "internal parser"),
    ] {
        insert_session(
            &pool,
            SessionFixture {
                id,
                owner_uid: Some(1000),
                title,
                updated_at: "2026-08-17T12:00:00Z",
                repository_id: None,
            },
        )
        .await;
    }
    sqlx::query("UPDATE sessions SET archived_at = ? WHERE id = ?")
        .bind("2026-08-17T13:00:00Z")
        .bind(archived.to_string())
        .execute(&pool)
        .await
        .expect("archive fixture");
    sqlx::query("UPDATE sessions SET internal = 1 WHERE id = ?")
        .bind(internal.to_string())
        .execute(&pool)
        .await
        .expect("internal fixture");

    let browse = session_library::search_sessions(
        &pool,
        1000,
        PeerPrincipal::from_uid(1000),
        &query("", 20),
    )
    .await
    .expect("default library browse");
    assert_eq!(
        browse
            .items
            .iter()
            .map(|item| item.session.session_id)
            .collect::<Vec<_>>(),
        vec![visible]
    );

    let explicit = session_library::search_sessions(
        &pool,
        1000,
        PeerPrincipal::from_uid(1000),
        &query("parser", 20),
    )
    .await
    .expect("explicit search");
    let explicit_ids = explicit
        .items
        .iter()
        .map(|item| item.session.session_id)
        .collect::<Vec<_>>();
    assert!(explicit_ids.contains(&visible));
    assert!(explicit_ids.contains(&archived));
    assert!(explicit_ids.contains(&internal));
}

#[tokio::test]
async fn ledger_appends_index_incrementally_and_rebuild_repairs_missing_sources() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::open_database(&temp.path().join("source-rebuild.db"))
        .await
        .expect("open database");
    let session_id = SessionId::new();
    let run_id = RunId::new();
    codypendent_daemon::ledger::create_session(&pool, session_id, "Index repair")
        .await
        .expect("create indexed session");
    for (sequence, body) in [
        (
            1,
            EventBody::NoteAppended {
                text: "repair the parser index".to_string(),
                run_id: Some(run_id),
            },
        ),
        (
            2,
            EventBody::ToolStarted {
                run_id,
                tool: "workspace.read_file".to_string(),
                args_digest: "digest".to_string(),
                label: Some("src/parser.rs".to_string()),
            },
        ),
    ] {
        codypendent_daemon::ledger::append_event(
            &pool,
            session_id,
            &SessionEvent {
                sequence,
                occurred_at: "2026-08-17T14:00:00Z".parse().expect("event timestamp"),
                causation_id: None,
                correlation_id: None,
                actor: Actor::System,
                body,
            },
        )
        .await
        .expect("append indexed event");
    }

    async fn sources(pool: &SqlitePool) -> Vec<(String, String, String, String, Option<i64>)> {
        sqlx::query_as(
            "SELECT source_type, source_id, content_hash, indexed_at, event_sequence \
             FROM session_search_sources ORDER BY source_type, source_id",
        )
        .fetch_all(pool)
        .await
        .expect("load indexed sources")
    }

    let incrementally_indexed = sources(&pool).await;
    assert_eq!(incrementally_indexed.len(), 4);
    assert!(incrementally_indexed
        .iter()
        .all(|(_, _, hash, _, _)| hash.len() == 64));

    sqlx::query("DELETE FROM session_search_sources")
        .execute(&pool)
        .await
        .expect("simulate interrupted indexing");
    assert_eq!(
        session_library::rebuild_search_sources(&pool)
            .await
            .expect("repair source index"),
        4
    );
    assert_eq!(sources(&pool).await, incrementally_indexed);

    session_library::rebuild_search_sources(&pool)
        .await
        .expect("repeat deterministic rebuild");
    assert_eq!(sources(&pool).await, incrementally_indexed);
}

/// Scoring lowercases with Unicode rules; the SQL row filter that now keeps a
/// keystroke from reading the whole ledger folds case for ASCII only. Exactly
/// two characters fall in that gap — U+212A KELVIN SIGN lowercases to `k`, and
/// `İ` to `i` — so a transcript spelled with one has to come back anyway. A
/// plain `LIKE` prefilter drops these silently, which is the failure mode a
/// filter must never have.
#[tokio::test]
async fn a_transcript_spelled_with_a_folded_character_is_still_found() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::open_database(&temp.path().join("folded.db"))
        .await
        .expect("open database");
    let session_id = SessionId::new();
    insert_session(
        &pool,
        SessionFixture {
            id: session_id,
            owner_uid: Some(1000),
            title: "folded transcript",
            updated_at: "2026-08-17T10:00:00Z",
            repository_id: None,
        },
    )
    .await;
    codypendent_daemon::ledger::append_event(
        &pool,
        session_id,
        &SessionEvent {
            sequence: 1,
            occurred_at: chrono::Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            // U+212A, not an ASCII K.
            body: EventBody::NoteAppended {
                text: "measured 300 \u{212a} at the sensor".to_string(),
                run_id: None,
            },
        },
    )
    .await
    .expect("append");

    let page = session_library::search_sessions(
        &pool,
        1000,
        PeerPrincipal::from_uid(1000),
        &query("k", 20),
    )
    .await
    .expect("search");
    assert_eq!(
        page.items.len(),
        1,
        "the transcript folds to `k` and must still be returned"
    );
    assert_eq!(page.items[0].source, SessionSearchSource::Transcript);
}

/// Boot used to re-derive the entire index — every event of every session, read
/// and JSON-decoded under `BEGIN IMMEDIATE` — on every start. Indexing happens
/// in the same transaction as the write it describes, so the only sessions that
/// can need anything are the ones that were never indexed at all.
#[tokio::test]
async fn boot_indexes_only_sessions_that_were_never_indexed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::open_database(&temp.path().join("catch-up.db"))
        .await
        .expect("open database");

    // Indexed as it was written.
    let indexed = SessionId::new();
    codypendent_daemon::ledger::create_session(&pool, indexed, "already indexed")
        .await
        .expect("create session");
    // A marker row that ONLY a whole-index rebuild would remove.
    sqlx::query(
        "INSERT INTO session_search_sources \
         (session_id, source_type, source_id, content_hash, indexed_at) \
         VALUES (?, 'transcript', 'sentinel', 'sentinel', '2026-08-17T10:00:00Z')",
    )
    .bind(indexed.to_string())
    .execute(&pool)
    .await
    .expect("sentinel");

    // Written by a path that does not index: exactly what boot has to repair.
    let unindexed = SessionId::new();
    insert_session(
        &pool,
        SessionFixture {
            id: unindexed,
            owner_uid: Some(1000),
            title: "never indexed",
            updated_at: "2026-08-17T10:00:00Z",
            repository_id: None,
        },
    )
    .await;

    let repaired = session_library::catch_up_search_sources(&pool)
        .await
        .expect("catch up");
    assert!(repaired > 0, "the unindexed session is indexed");

    let sources: Vec<(String, String)> = sqlx::query_as(
        "SELECT session_id, source_id FROM session_search_sources ORDER BY session_id, source_id",
    )
    .fetch_all(&pool)
    .await
    .expect("sources");
    assert!(
        sources
            .iter()
            .any(|(session, source)| session == &unindexed.to_string() && source == "title"),
        "the session that was never indexed now is: {sources:?}"
    );
    assert!(
        sources
            .iter()
            .any(|(session, source)| session == &indexed.to_string() && source == "sentinel"),
        "the session that was already indexed was not re-derived: {sources:?}"
    );

    let second = session_library::catch_up_search_sources(&pool)
        .await
        .expect("second catch up");
    assert_eq!(
        second, 0,
        "a boot with nothing to repair reads no history and writes nothing"
    );
}
