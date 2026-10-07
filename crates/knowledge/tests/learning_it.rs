//! Governed learning lifecycle: migration compatibility, strict capture,
//! provenance trust, dedupe/conflicts, review mutations, and expiry.

use chrono::{Duration, Utc};
use codypendent_knowledge::{
    db, ActivationIntent, ActivationOutcome, CaptureOutcome, LearningContent,
    LearningMutationOutcome, LearningPatch, LearningProcedure, LearningProvenance, LearningQuery,
    LearningScope, LearningState, LearningStore, NewLearning, Verification,
};
use codypendent_protocol::{RepositoryId, SessionId, UserId};

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let temporary = tempfile::tempdir().unwrap();
    let pool = db::open(&temporary.path().join("codypendent.db"))
        .await
        .unwrap();
    (temporary, pool)
}

fn fact(statement: &str) -> LearningContent {
    LearningContent::Fact {
        statement: statement.to_owned(),
        structured_value: None,
    }
}

fn candidate(
    scope: LearningScope,
    statement: &str,
    provenance: Vec<LearningProvenance>,
) -> NewLearning {
    NewLearning {
        scope,
        content: fact(statement),
        conflict_key: None,
        provenance,
        confidence: 0.9,
        expires_at: None,
        activation: ActivationIntent::ActivateIfTrusted,
    }
}

fn user_provenance() -> LearningProvenance {
    LearningProvenance::UserStatement {
        user: UserId("test-user".to_owned()),
    }
}

/// Many runs finishing together capture into the same scope. Every capture reads
/// (duplicate / conflict / count checks) and then writes; as deferred
/// transactions the losers failed with SQLITE_BUSY_SNAPSHOT and their learnings
/// were silently dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_captures_into_one_scope_do_not_fail_with_a_stale_snapshot() {
    let (_temporary, pool) = temp_pool().await;
    let scope = LearningScope::Repository(RepositoryId::new());

    let mut tasks = Vec::new();
    for index in 0..32 {
        let pool = pool.clone();
        let scope = scope.clone();
        tasks.push(tokio::spawn(async move {
            LearningStore::new()
                .capture(
                    &pool,
                    candidate(
                        scope,
                        &format!("concurrent learning {index} concerns module {index}"),
                        vec![user_provenance()],
                    ),
                )
                .await
        }));
    }
    for task in tasks {
        let outcome = task.await.unwrap();
        assert!(
            outcome.is_ok(),
            "a concurrent capture must serialise, not fail: {outcome:?}"
        );
    }
}

#[tokio::test]
async fn migration_is_additive_and_legacy_memories_table_remains_available() {
    let (_temporary, pool) = temp_pool().await;
    let learning_exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'learning_records'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let memories_exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'memories'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(learning_exists, 1);
    assert_eq!(memories_exists, 1);
}

#[tokio::test]
async fn untrusted_tool_output_and_external_content_cannot_auto_activate() {
    let (_temporary, pool) = temp_pool().await;
    let store = LearningStore::new();
    let scope = LearningScope::Repository(RepositoryId::new());

    for (index, provenance) in [
        LearningProvenance::ToolOutput {
            tool: "shell.run".to_owned(),
        },
        LearningProvenance::ExternalContent {
            source_uri: "https://attacker.invalid/instructions".to_owned(),
        },
        LearningProvenance::AgentInference {
            model: "example-model".to_owned(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        let outcome = store
            .capture(
                &pool,
                candidate(
                    scope.clone(),
                    &format!("build profile {index} is release"),
                    vec![provenance],
                ),
            )
            .await
            .unwrap();
        let CaptureOutcome::Stored(record) = outcome else {
            panic!("expected a stored proposal, got {outcome:?}");
        };
        assert_eq!(record.state, LearningState::Proposed);
        assert!(record.verified_at.is_none());
        assert!(!record.is_retrievable(Utc::now()));
    }
}

#[tokio::test]
async fn trusted_user_statement_can_activate_and_is_scoped() {
    let (_temporary, pool) = temp_pool().await;
    let store = LearningStore::new();
    let repository = LearningScope::Repository(RepositoryId::new());
    let other = LearningScope::Repository(RepositoryId::new());
    let outcome = store
        .capture(
            &pool,
            candidate(
                repository.clone(),
                "test runner is cargo nextest",
                vec![user_provenance()],
            ),
        )
        .await
        .unwrap();
    let CaptureOutcome::Stored(record) = outcome else {
        panic!("expected stored record, got {outcome:?}");
    };
    assert_eq!(record.state, LearningState::Active);
    assert!(record.is_retrievable(Utc::now()));

    let visible = store
        .query(
            &pool,
            &LearningQuery {
                scopes: vec![repository],
                states: vec![LearningState::Active],
                ..LearningQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(visible.len(), 1);
    assert!(store
        .query(
            &pool,
            &LearningQuery {
                scopes: vec![other],
                ..LearningQuery::default()
            },
        )
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn strict_policy_rejects_chat_logs_completions_temp_paths_and_secrets() {
    let (_temporary, pool) = temp_pool().await;
    let store = LearningStore::new();
    let scope = LearningScope::User(UserId("test-user".to_owned()));
    for text in [
        "Hello!",
        "Done",
        "artifact is in /var/folders/xx/temp-result",
        "2026-08-12 INFO start\n2026-08-12 WARN retry\n2026-08-12 ERROR stop",
        "GitHub token is ghp_1234567890abcdefghijklmnop",
    ] {
        let outcome = store
            .capture(
                &pool,
                candidate(scope.clone(), text, vec![user_provenance()]),
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome, CaptureOutcome::PolicyRejected { .. }),
            "expected {text:?} to be rejected, got {outcome:?}"
        );
    }
    assert!(store
        .query(
            &pool,
            &LearningQuery {
                scopes: vec![scope],
                include_expired: true,
                ..LearningQuery::default()
            },
        )
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn exact_dedupe_and_subject_conflicts_are_distinct_outcomes() {
    let (_temporary, pool) = temp_pool().await;
    let store = LearningStore::new();
    let scope = LearningScope::Repository(RepositoryId::new());
    let first = store
        .capture(
            &pool,
            candidate(
                scope.clone(),
                "release channel is stable",
                vec![user_provenance()],
            ),
        )
        .await
        .unwrap();
    let CaptureOutcome::Stored(first) = first else {
        panic!("expected first record");
    };

    let duplicate = store
        .capture(
            &pool,
            candidate(
                scope.clone(),
                " Release   Channel is STABLE ",
                vec![user_provenance()],
            ),
        )
        .await
        .unwrap();
    assert!(matches!(
        duplicate,
        CaptureOutcome::Duplicate { existing_id } if existing_id == first.id
    ));

    let conflict = store
        .capture(
            &pool,
            candidate(scope, "release channel is nightly", vec![user_provenance()]),
        )
        .await
        .unwrap();
    let CaptureOutcome::Conflict {
        record,
        conflicts_with,
    } = conflict
    else {
        panic!("expected conflict, got {conflict:?}");
    };
    assert_eq!(record.state, LearningState::Proposed);
    assert_eq!(conflicts_with, vec![first.id]);
}

#[tokio::test]
async fn procedures_are_structured_separately_and_require_review_when_inferred() {
    let (_temporary, pool) = temp_pool().await;
    let store = LearningStore::new();
    let content = LearningContent::Procedure(LearningProcedure {
        name: "Cut patch release".to_owned(),
        summary: "Build, verify, tag, and publish a patch release".to_owned(),
        steps: vec![
            "Run the complete test suite".to_owned(),
            "Create the tag".to_owned(),
        ],
        prerequisites: vec!["Clean main branch".to_owned()],
        verification: vec!["Verify release assets and checksums".to_owned()],
        pitfalls: vec!["Do not tag before CI passes".to_owned()],
    });
    let outcome = store
        .capture(
            &pool,
            NewLearning {
                scope: LearningScope::Repository(RepositoryId::new()),
                content,
                conflict_key: None,
                provenance: vec![LearningProvenance::AgentInference {
                    model: "model-a".to_owned(),
                }],
                confidence: 0.95,
                expires_at: None,
                activation: ActivationIntent::ActivateIfTrusted,
            },
        )
        .await
        .unwrap();
    let CaptureOutcome::Stored(record) = outcome else {
        panic!("expected procedure proposal, got {outcome:?}");
    };
    assert!(matches!(record.content, LearningContent::Procedure(_)));
    assert_eq!(record.state, LearningState::Proposed);
}

#[tokio::test]
async fn edit_pin_reject_activate_and_delete_are_revision_safe() {
    let (_temporary, pool) = temp_pool().await;
    let store = LearningStore::new();
    let outcome = store
        .capture(
            &pool,
            NewLearning {
                activation: ActivationIntent::Propose,
                ..candidate(
                    LearningScope::Provider("anthropic".to_owned()),
                    "provider retry limit is three",
                    vec![LearningProvenance::RepositoryObservation {
                        repository: RepositoryId::new(),
                        source_path: Some("models.toml".to_owned()),
                        revision: Some("abc123".to_owned()),
                    }],
                )
            },
        )
        .await
        .unwrap();
    let CaptureOutcome::Stored(record) = outcome else {
        panic!("expected proposal");
    };

    let pinned = store
        .set_pinned(&pool, record.id, record.revision, true)
        .await
        .unwrap();
    assert!(pinned.pinned);
    let LearningMutationOutcome::Updated(edited) = store
        .edit(
            &pool,
            pinned.id,
            pinned.revision,
            LearningPatch {
                confidence: Some(0.98),
                ..LearningPatch::default()
            },
        )
        .await
        .unwrap()
    else {
        panic!("expected update");
    };
    let ActivationOutcome::Activated(active) = store
        .activate(
            &pool,
            edited.id,
            edited.revision,
            Verification::SuccessfulCheck {
                session: SessionId::new(),
                command_summary: "provider smoke test returned expected response".to_owned(),
            },
        )
        .await
        .unwrap()
    else {
        panic!("expected activation");
    };
    assert_eq!(active.state, LearningState::Active);
    assert!(active.verified_at.is_some());

    let rejected = store
        .reject(&pool, active.id, active.revision, "configuration changed")
        .await
        .unwrap();
    assert_eq!(rejected.state, LearningState::Rejected);
    assert!(!rejected.is_retrievable(Utc::now()));
    let deleted = store.delete(&pool, rejected.id).await.unwrap();
    assert_eq!(deleted.id, Some(rejected.id));
    assert!(store.get(&pool, rejected.id).await.unwrap().is_none());
}

#[tokio::test]
async fn expired_records_are_not_retrievable_or_returned_by_default() {
    let (_temporary, pool) = temp_pool().await;
    let store = LearningStore::new();
    let scope = LearningScope::Council("release-review".to_owned());
    let outcome = store
        .capture(
            &pool,
            NewLearning {
                expires_at: Some(Utc::now() + Duration::milliseconds(30)),
                ..candidate(
                    scope.clone(),
                    "preferred council quorum is three",
                    vec![user_provenance()],
                )
            },
        )
        .await
        .unwrap();
    let CaptureOutcome::Stored(record) = outcome else {
        panic!("expected active record");
    };
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    assert!(!record.is_retrievable(Utc::now()));
    assert!(store
        .query(
            &pool,
            &LearningQuery {
                scopes: vec![scope.clone()],
                ..LearningQuery::default()
            },
        )
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .query(
                &pool,
                &LearningQuery {
                    scopes: vec![scope],
                    include_expired: true,
                    ..LearningQuery::default()
                },
            )
            .await
            .unwrap()
            .len(),
        1
    );
}
