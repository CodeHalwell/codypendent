//! STEP 2.4: the memory fabric — store, curator pipeline, and observer.
//!
//! Covers the normative curator gate order (a secret candidate is redacted
//! before dedup/provenance run), evidence-free rejection, supersession across
//! revisions (the old fact is never deleted), near-duplicate dropping, absolute
//! cross-repository scope isolation, forget + tombstone with a content-free
//! audit, and observer candidate extraction that always carries provenance.

use chrono::Utc;
use codypendent_knowledge::types::{
    EvidenceRef, MemoryClass, MemoryRecord, RetentionPolicy, Revision, Scope,
};
use codypendent_knowledge::{
    db, extract_candidates, provenance_cards, CandidateMemory, Curation, MemoryStore,
};
use codypendent_protocol::{
    Actor, ArtifactId, ArtifactRef, DataClassification, EventBody, MemoryId, RepositoryId,
    RunDisposition, RunId, SessionEvent, SessionId, ToolOutcome,
};

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

/// One evidence ref, so hand-built records satisfy the provenance invariant.
fn some_evidence() -> Vec<EvidenceRef> {
    vec![EvidenceRef::EventRange {
        session_id: SessionId::new(),
        from_sequence: 1,
        to_sequence: 2,
    }]
}

/// A live memory record in `scope`.
fn record(scope: Scope, class: MemoryClass, statement: &str, valid_from: &str) -> MemoryRecord {
    MemoryRecord {
        id: MemoryId::new(),
        class,
        scope,
        statement: statement.to_string(),
        structured_value: None,
        provenance: some_evidence(),
        confidence: 0.9,
        observed_at: Utc::now(),
        valid_from: Revision(valid_from.to_string()),
        valid_until: None,
        supersedes: Vec::new(),
        sensitivity: DataClassification::Internal,
        retention: RetentionPolicy::default(),
    }
}

/// A curator candidate in `scope`.
fn candidate(
    scope: Option<Scope>,
    class: MemoryClass,
    statement: &str,
    provenance: Vec<EvidenceRef>,
) -> CandidateMemory {
    CandidateMemory {
        class,
        scope,
        statement: statement.to_string(),
        structured_value: None,
        provenance,
        confidence: 0.8,
        observed_at: Utc::now(),
        valid_from: Revision("rev1".to_string()),
        sensitivity: DataClassification::Internal,
        retention: None,
    }
}

fn artifact() -> ArtifactRef {
    ArtifactRef {
        id: ArtifactId::new(),
        media_type: "application/json".to_string(),
        byte_length: 42,
        sha256: "0".repeat(64),
        sensitivity: DataClassification::Internal,
    }
}

fn event(sequence: u64, body: EventBody) -> SessionEvent {
    SessionEvent {
        sequence,
        occurred_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        actor: Actor::System,
        body,
    }
}

// --------------------------------------------------------------------------
// Curator pipeline
// --------------------------------------------------------------------------

#[tokio::test]
async fn secret_candidate_is_redacted_before_dedup_or_provenance() {
    let (_tmp, pool) = temp_pool().await;
    let store = MemoryStore::new();
    let repo = RepositoryId::new();

    // The candidate is BOTH secret-bearing AND evidence-free. The secret filter
    // is gate (a), before the provenance gate (e), so the outcome must be
    // Redacted — proving gate order.
    let candidate = candidate(
        Some(Scope::Repository(repo)),
        MemoryClass::Semantic,
        "deploy key AKIAIOSFODNN7EXAMPLE is configured",
        Vec::new(),
    );
    let outcome = store.curate(&pool, candidate).await.unwrap();
    assert!(
        matches!(outcome, Curation::Redacted { .. }),
        "secret must be redacted before provenance rejects it, got {outcome:?}"
    );

    // Nothing was written.
    assert!(store
        .query(&pool, &[Scope::Repository(repo)], None)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn evidence_free_candidate_is_rejected() {
    let (_tmp, pool) = temp_pool().await;
    let store = MemoryStore::new();
    let repo = RepositoryId::new();

    let candidate = candidate(
        Some(Scope::Repository(repo)),
        MemoryClass::Semantic,
        "the project targets Rust 2021 edition",
        Vec::new(),
    );
    let outcome = store.curate(&pool, candidate).await.unwrap();
    match outcome {
        Curation::Rejected { reason } => assert_eq!(reason, "evidence-free"),
        other => panic!("expected evidence-free rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn accepted_candidate_is_inserted_with_default_retention() {
    let (_tmp, pool) = temp_pool().await;
    let store = MemoryStore::new();
    let repo = RepositoryId::new();

    let candidate = candidate(
        Some(Scope::Repository(repo)),
        MemoryClass::Semantic,
        "the default log level is info",
        some_evidence(),
    );
    let outcome = store.curate(&pool, candidate).await.unwrap();
    let Curation::Accepted(record) = outcome else {
        panic!("expected acceptance, got {outcome:?}");
    };
    // The default 365-day retention was applied.
    assert_eq!(record.retention, RetentionPolicy::default());
    assert_eq!(record.retention.ttl_days, Some(365));
    // And the row is queryable in its scope.
    let live = store
        .query(&pool, &[Scope::Repository(repo)], None)
        .await
        .unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, record.id);
}

#[tokio::test]
async fn near_duplicate_candidate_is_dropped() {
    let (_tmp, pool) = temp_pool().await;
    let store = MemoryStore::new();
    let repo = RepositoryId::new();
    let scope = Scope::Repository(repo);

    let existing = record(
        scope.clone(),
        MemoryClass::Semantic,
        "the project builds with cargo build --release",
        "rev1",
    );
    store.insert(&pool, &existing).await.unwrap();

    // An identical same-scope, same-class statement is > 0.92 similar.
    let dup = candidate(
        Some(scope.clone()),
        MemoryClass::Semantic,
        "the project builds with cargo build --release",
        some_evidence(),
    );
    let outcome = store.curate(&pool, dup).await.unwrap();
    match outcome {
        Curation::Duplicate { existing_id } => assert_eq!(existing_id, existing.id),
        other => panic!("expected duplicate, got {other:?}"),
    }
    // Still exactly one memory.
    assert_eq!(store.query(&pool, &[scope], None).await.unwrap().len(), 1);
}

#[tokio::test]
async fn contradicting_candidate_supersedes_via_the_curator() {
    let (_tmp, pool) = temp_pool().await;
    let store = MemoryStore::new();
    let scope = Scope::Repository(RepositoryId::new());

    let first = candidate(
        Some(scope.clone()),
        MemoryClass::Semantic,
        "release channel is stable",
        some_evidence(),
    );
    let Curation::Accepted(old) = store.curate(&pool, first).await.unwrap() else {
        panic!("first candidate should be accepted");
    };

    // Same subject ("release channel"), incompatible value → supersession.
    let second = candidate(
        Some(scope.clone()),
        MemoryClass::Semantic,
        "release channel is nightly",
        some_evidence(),
    );
    let outcome = store.curate(&pool, second).await.unwrap();
    match outcome {
        Curation::Superseded { old_id, record } => {
            assert_eq!(old_id, old.id);
            assert!(record.supersedes.contains(&old.id));
        }
        other => panic!("expected supersession, got {other:?}"),
    }

    // The old record is not deleted.
    assert!(store.get(&pool, old.id).await.unwrap().is_some());
    // Only the new fact is currently live.
    let live = store.query(&pool, &[scope], None).await.unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].statement, "release channel is nightly");
}

// --------------------------------------------------------------------------
// Supersession across revisions
// --------------------------------------------------------------------------

#[tokio::test]
async fn supersession_returns_the_valid_record_per_revision() {
    let (_tmp, pool) = temp_pool().await;
    let store = MemoryStore::new();
    let scope = Scope::Repository(RepositoryId::new());

    // A: test command is `cargo test`, valid from rev1.
    let a = record(
        scope.clone(),
        MemoryClass::Procedural,
        "test command is cargo test",
        "rev1",
    );
    store.insert(&pool, &a).await.unwrap();

    // B contradicts A from rev2.
    let b = record(
        scope.clone(),
        MemoryClass::Procedural,
        "test command is cargo nextest run",
        "rev2",
    );
    let stored_b = store.supersede(&pool, a.id, b.clone()).await.unwrap();
    assert!(stored_b.supersedes.contains(&a.id));

    // At rev1 the query returns A.
    let at_rev1 = store
        .query(
            &pool,
            std::slice::from_ref(&scope),
            Some(&Revision("rev1".to_string())),
        )
        .await
        .unwrap();
    assert_eq!(at_rev1.len(), 1);
    assert_eq!(at_rev1[0].id, a.id);
    assert_eq!(at_rev1[0].statement, "test command is cargo test");

    // At rev2 the query returns B.
    let at_rev2 = store
        .query(
            &pool,
            std::slice::from_ref(&scope),
            Some(&Revision("rev2".to_string())),
        )
        .await
        .unwrap();
    assert_eq!(at_rev2.len(), 1);
    assert_eq!(at_rev2[0].id, b.id);
    assert_eq!(at_rev2[0].statement, "test command is cargo nextest run");

    // A is never deleted — it still exists, now with a valid_until stamp.
    let a_now = store.get(&pool, a.id).await.unwrap().unwrap();
    assert_eq!(a_now.valid_until, Some(Revision("rev2".to_string())));

    // The current (no-revision) view shows only the live record, B.
    let live = store.query(&pool, &[scope], None).await.unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, b.id);
}

// --------------------------------------------------------------------------
// Cross-repository scope isolation (absolute)
// --------------------------------------------------------------------------

#[tokio::test]
async fn scope_isolation_never_leaks_across_repositories() {
    let (_tmp, pool) = temp_pool().await;
    let store = MemoryStore::new();
    let repo_a = RepositoryId::new();
    let repo_b = RepositoryId::new();

    // Identical statement and class, different repositories.
    let statement = "the framework is Rust with tokio";
    let in_a = record(
        Scope::Repository(repo_a),
        MemoryClass::Semantic,
        statement,
        "rev1",
    );
    let in_b = record(
        Scope::Repository(repo_b),
        MemoryClass::Semantic,
        statement,
        "rev1",
    );
    store.insert(&pool, &in_a).await.unwrap();
    store.insert(&pool, &in_b).await.unwrap();

    // A query for repo A returns only A's memory — never B's, despite identity.
    let a_view = store
        .query(&pool, &[Scope::Repository(repo_a)], None)
        .await
        .unwrap();
    assert_eq!(a_view.len(), 1);
    assert_eq!(a_view[0].id, in_a.id);
    assert!(a_view.iter().all(|m| m.id != in_b.id));

    // And symmetrically for repo B.
    let b_view = store
        .query(&pool, &[Scope::Repository(repo_b)], None)
        .await
        .unwrap();
    assert_eq!(b_view.len(), 1);
    assert_eq!(b_view[0].id, in_b.id);
    assert!(b_view.iter().all(|m| m.id != in_a.id));
}

// --------------------------------------------------------------------------
// Forget + tombstone
// --------------------------------------------------------------------------

#[tokio::test]
async fn forget_removes_the_row_writes_a_tombstone_and_hides_content() {
    let (_tmp, pool) = temp_pool().await;
    let store = MemoryStore::new();
    let scope = Scope::Repository(RepositoryId::new());

    let statement = "deploy uses a blue-green strategy";
    let mem = record(scope, MemoryClass::Semantic, statement, "rev1");
    store.insert(&pool, &mem).await.unwrap();

    let audit = store.forget(&pool, mem.id).await.unwrap();

    // The row is gone.
    assert!(store.get(&pool, mem.id).await.unwrap().is_none());

    // The audit summary names the id but not the statement text.
    assert_eq!(audit.forgotten, vec![mem.id]);
    assert_eq!(audit.count(), 1);
    let debug = format!("{audit:?}");
    assert!(
        !debug.contains("blue-green"),
        "audit must not retain deleted content: {debug}"
    );

    // A memory_changed tombstone was enqueued (insert=1, forget=1).
    let outbox = codypendent_knowledge::outbox::unprocessed(&pool, 10)
        .await
        .unwrap();
    assert_eq!(outbox.len(), 2);
    assert!(outbox.iter().all(|r| r.event_kind == "memory_changed"));
    assert_eq!(outbox.last().unwrap().entity_id, mem.id.to_string());
}

#[tokio::test]
async fn forget_scope_removes_every_memory_in_the_scope_only() {
    let (_tmp, pool) = temp_pool().await;
    let store = MemoryStore::new();
    let repo_a = RepositoryId::new();
    let repo_b = RepositoryId::new();

    store
        .insert(
            &pool,
            &record(
                Scope::Repository(repo_a),
                MemoryClass::Semantic,
                "a1",
                "rev1",
            ),
        )
        .await
        .unwrap();
    store
        .insert(
            &pool,
            &record(
                Scope::Repository(repo_a),
                MemoryClass::Failure,
                "a2",
                "rev1",
            ),
        )
        .await
        .unwrap();
    store
        .insert(
            &pool,
            &record(
                Scope::Repository(repo_b),
                MemoryClass::Semantic,
                "b1",
                "rev1",
            ),
        )
        .await
        .unwrap();

    let audit = store
        .forget_scope(&pool, &Scope::Repository(repo_a))
        .await
        .unwrap();
    assert_eq!(audit.count(), 2);
    assert_eq!(audit.scope, Some(Scope::Repository(repo_a)));

    // Repo A is emptied; repo B is untouched.
    assert!(store
        .query(&pool, &[Scope::Repository(repo_a)], None)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .query(&pool, &[Scope::Repository(repo_b)], None)
            .await
            .unwrap()
            .len(),
        1
    );
}

// --------------------------------------------------------------------------
// Provenance cards
// --------------------------------------------------------------------------

#[tokio::test]
async fn provenance_cards_open_every_source() {
    let scope = Scope::Repository(RepositoryId::new());
    let mut mem = record(
        scope.clone(),
        MemoryClass::Semantic,
        "rust-toolchain pins nightly",
        "abc123",
    );
    mem.provenance = vec![
        EvidenceRef::EventRange {
            session_id: SessionId::new(),
            from_sequence: 4,
            to_sequence: 9,
        },
        EvidenceRef::Artifact {
            artifact: artifact(),
            source_path: Some("rust-toolchain.toml".to_string()),
        },
    ];

    let cards = provenance_cards(&mem);
    assert_eq!(cards.len(), 2, "one card per evidence ref");
    for card in &cards {
        assert_eq!(card.statement, "rust-toolchain pins nightly");
        assert_eq!(card.revision, Revision("abc123".to_string()));
        assert_eq!(card.scope, scope);
        assert!((card.confidence - 0.9).abs() < f32::EPSILON);
    }
}

// --------------------------------------------------------------------------
// Observer candidate extraction
// --------------------------------------------------------------------------

#[tokio::test]
async fn extract_candidates_yields_provenance_bearing_candidates() {
    let session = SessionId::new();
    let run = RunId::new();

    let events = vec![
        // A repeated, successful shell command (same digest twice) → Procedural.
        event(
            1,
            EventBody::ToolStarted {
                run_id: run,
                tool: "shell.run".to_string(),
                args_digest: "digest-1".to_string(),
            },
        ),
        event(
            2,
            EventBody::ToolCompleted {
                run_id: run,
                tool: "shell.run".to_string(),
                outcome: ToolOutcome::Succeeded,
                artifact: None,
            },
        ),
        event(
            3,
            EventBody::ToolStarted {
                run_id: run,
                tool: "shell.run".to_string(),
                args_digest: "digest-1".to_string(),
            },
        ),
        event(
            4,
            EventBody::ToolCompleted {
                run_id: run,
                tool: "shell.run".to_string(),
                outcome: ToolOutcome::Succeeded,
                artifact: None,
            },
        ),
        // A completed run with a chronicle → Episodic, cited by the artifact.
        event(
            5,
            EventBody::RunCompleted {
                run_id: run,
                disposition: RunDisposition::Completed {
                    summary: Some("green build".to_string()),
                },
                chronicle: artifact(),
            },
        ),
        // An explicit proposal note → Semantic.
        event(
            6,
            EventBody::NoteAppended {
                text: "memory.propose: CI runs on ubuntu-latest".to_string(),
                run_id: None,
            },
        ),
    ];

    let candidates = extract_candidates(&events, Scope::Session(session));

    // Every candidate must carry at least one evidence ref.
    assert!(!candidates.is_empty());
    assert!(
        candidates.iter().all(|c| !c.provenance.is_empty()),
        "observer candidates must be provenance-bearing"
    );

    // A procedural candidate cites the event range spanning the repeats.
    let procedural = candidates
        .iter()
        .find(|c| c.class == MemoryClass::Procedural)
        .expect("a repeated command yields a procedural candidate");
    assert!(matches!(
        procedural.provenance.first(),
        Some(EvidenceRef::EventRange {
            session_id,
            from_sequence: 1,
            to_sequence: 4,
        }) if *session_id == session
    ));

    // The run-completion candidate cites the chronicle artifact.
    let episodic = candidates
        .iter()
        .find(|c| c.class == MemoryClass::Episodic)
        .expect("a completed run yields an episodic candidate");
    assert!(matches!(
        episodic.provenance.first(),
        Some(EvidenceRef::Artifact { .. })
    ));

    // The explicit note becomes a semantic candidate with the marker stripped.
    let semantic = candidates
        .iter()
        .find(|c| c.class == MemoryClass::Semantic)
        .expect("a memory.propose note yields a semantic candidate");
    assert_eq!(semantic.statement, "CI runs on ubuntu-latest");
}

#[tokio::test]
async fn observer_candidates_flow_through_the_curator() {
    let (_tmp, pool) = temp_pool().await;
    let store = MemoryStore::new();
    let session = SessionId::new();
    let run = RunId::new();

    let events = vec![event(
        1,
        EventBody::RunCompleted {
            run_id: run,
            disposition: RunDisposition::Failed {
                reason: "clippy denied 3 lints".to_string(),
            },
            chronicle: artifact(),
        },
    )];

    let candidates = extract_candidates(&events, Scope::Session(session));
    assert_eq!(candidates.len(), 1);
    // A failed run is a Failure-class candidate, and the curator accepts it
    // (it has artifact provenance and no secret).
    assert_eq!(candidates[0].class, MemoryClass::Failure);
    let outcome = store.curate(&pool, candidates[0].clone()).await.unwrap();
    assert!(matches!(outcome, Curation::Accepted(_)), "got {outcome:?}");
}
