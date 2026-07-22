//! STEP 7.5 daemon wiring: `PromotionStore` persists the promotion pipeline
//! across restarts, and — the non-negotiable property — provides no back door
//! to `Promoted` outside a real human `approve()`.

use codypendent_eval::{
    db, ArtifactKind, ArtifactVersion, CanaryOutcome, PromotionError, PromotionStage,
    PromotionStore, PromotionStoreError,
};
use codypendent_protocol::events::Actor;
use codypendent_protocol::ids::UserId;

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

fn human() -> Actor {
    Actor::Human {
        user_id: UserId("dana".into()),
    }
}

fn agent() -> Actor {
    Actor::Agent {
        agent_id: codypendent_protocol::ids::AgentId::new(),
        run_id: codypendent_protocol::ids::RunId::new(),
        model: codypendent_protocol::ids::ModelId("claude-sonnet-5".into()),
    }
}

fn artifact() -> ArtifactVersion {
    ArtifactVersion::new(ArtifactKind::Router, "tool-selection", 7)
}

#[tokio::test]
async fn migration_0015_applies_fresh_and_on_reopen() {
    // Fresh: sqlx::migrate! runs 0015 on a brand-new database.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("codypendent.db");
    let pool = db::open(&path).await.expect("fresh open + migrate");
    drop(pool);
    // Existing: reopening the SAME database file re-runs migrate! against an
    // already-migrated schema without erroring (sqlx's migration tracking
    // table makes this a no-op replay).
    let pool = db::open(&path)
        .await
        .expect("reopen an already-migrated db");
    let store = PromotionStore::new();
    let id = store
        .propose(&pool, artifact(), &human(), false)
        .await
        .expect("the table exists and accepts a write after reopen");
    assert!(store.get(&pool, &id).await.unwrap().is_some());
}

#[tokio::test]
async fn a_candidate_round_trips_through_every_legal_stage() {
    let (_tmp, pool) = temp_pool().await;
    let store = PromotionStore::new();

    let id = store
        .propose(&pool, artifact(), &human(), false)
        .await
        .unwrap();
    let snapshot = store.get(&pool, &id).await.unwrap().unwrap();
    assert_eq!(snapshot.candidate.stage(), PromotionStage::Draft);
    assert_eq!(snapshot.candidate.artifact(), &artifact());

    store.run_regression(&pool, &id, false).await.unwrap();
    assert_eq!(
        store
            .get(&pool, &id)
            .await
            .unwrap()
            .unwrap()
            .candidate
            .stage(),
        PromotionStage::RegressionPassed
    );

    store.start_shadow(&pool, &id).await.unwrap();
    assert_eq!(
        store
            .get(&pool, &id)
            .await
            .unwrap()
            .unwrap()
            .candidate
            .stage(),
        PromotionStage::Shadow
    );

    store.start_canary(&pool, &id).await.unwrap();
    assert_eq!(
        store
            .get(&pool, &id)
            .await
            .unwrap()
            .unwrap()
            .candidate
            .stage(),
        PromotionStage::Canary
    );

    // A clean observation keeps the canary going.
    let outcome = store.observe_canary(&pool, &id, false).await.unwrap();
    assert_eq!(outcome, CanaryOutcome::Continuing);

    store.finish_canary(&pool, &id).await.unwrap();
    assert_eq!(
        store
            .get(&pool, &id)
            .await
            .unwrap()
            .unwrap()
            .candidate
            .stage(),
        PromotionStage::ComparisonReady
    );

    let record = store.approve(&pool, &id, &human()).await.unwrap();
    assert_eq!(record.stage(), PromotionStage::Promoted);
    assert_eq!(record.actor_kind(), "human");
    assert_eq!(
        store
            .get(&pool, &id)
            .await
            .unwrap()
            .unwrap()
            .candidate
            .stage(),
        PromotionStage::Promoted
    );
    assert_eq!(
        store
            .active_version(&pool, &artifact().stem())
            .await
            .unwrap(),
        Some(7)
    );

    // Rollback restores no predecessor (this is the artifact's first version)
    // but the candidate itself still transitions and is attributed.
    let rollback_record = store.rollback(&pool, &id, &human()).await.unwrap();
    assert_eq!(rollback_record.stage(), PromotionStage::RolledBack);
    assert_eq!(rollback_record.actor_kind(), "human");
    assert_eq!(
        store
            .get(&pool, &id)
            .await
            .unwrap()
            .unwrap()
            .candidate
            .stage(),
        PromotionStage::RolledBack
    );
}

#[tokio::test]
async fn there_is_no_persisted_back_door_to_promoted() {
    // The exit-criterion-2 test at the persistence layer: drive a candidate to
    // ComparisonReady, then have an AGENT try to approve through the store. It
    // must fail — and critically, the STORED ROW must still read
    // ComparisonReady afterwards (a partial/side-channel write would be the
    // back door this task must not introduce).
    let (_tmp, pool) = temp_pool().await;
    let store = PromotionStore::new();

    let id = store
        .propose(&pool, artifact(), &agent(), false)
        .await
        .unwrap();
    store.run_regression(&pool, &id, false).await.unwrap();
    store.start_shadow(&pool, &id).await.unwrap();
    store.start_canary(&pool, &id).await.unwrap();
    store.observe_canary(&pool, &id, false).await.unwrap();
    store.finish_canary(&pool, &id).await.unwrap();

    let err = store.approve(&pool, &id, &agent()).await.unwrap_err();
    assert!(matches!(
        err,
        PromotionStoreError::Promotion(PromotionError::RequiresHumanApproval { actor: "agent" })
    ));

    // The row was NOT silently mutated by the failed attempt.
    let snapshot = store.get(&pool, &id).await.unwrap().unwrap();
    assert_eq!(snapshot.candidate.stage(), PromotionStage::ComparisonReady);
    assert_eq!(
        store
            .active_version(&pool, &artifact().stem())
            .await
            .unwrap(),
        None,
        "nothing was activated by the refused attempt"
    );

    // The ONLY path that reaches Promoted is a human approval.
    let record = store.approve(&pool, &id, &human()).await.unwrap();
    assert_eq!(record.stage(), PromotionStage::Promoted);
}

#[tokio::test]
async fn a_canary_cannot_finish_unobserved() {
    // P7-2 at the persistence layer: finish_canary with zero recorded
    // observations must fail, not silently "pass".
    let (_tmp, pool) = temp_pool().await;
    let store = PromotionStore::new();
    let id = store
        .propose(&pool, artifact(), &human(), false)
        .await
        .unwrap();
    store.run_regression(&pool, &id, false).await.unwrap();
    store.start_shadow(&pool, &id).await.unwrap();
    store.start_canary(&pool, &id).await.unwrap();

    let err = store.finish_canary(&pool, &id).await.unwrap_err();
    assert!(matches!(
        err,
        PromotionStoreError::Promotion(PromotionError::CanaryUnobserved)
    ));
    assert_eq!(
        store
            .get(&pool, &id)
            .await
            .unwrap()
            .unwrap()
            .candidate
            .stage(),
        PromotionStage::Canary,
        "an unobserved canary does not persist as ComparisonReady"
    );
}

#[tokio::test]
async fn a_canary_regression_auto_rolls_back_and_persists_system_plus_reason() {
    let (_tmp, pool) = temp_pool().await;
    let store = PromotionStore::new();
    let id = store
        .propose(&pool, artifact(), &human(), false)
        .await
        .unwrap();
    store.run_regression(&pool, &id, false).await.unwrap();
    store.start_shadow(&pool, &id).await.unwrap();
    store.start_canary(&pool, &id).await.unwrap();

    let outcome = store.observe_canary(&pool, &id, true).await.unwrap();
    let CanaryOutcome::AutoRolledBack(record) = outcome else {
        panic!("expected an auto-rollback");
    };
    assert_eq!(record.actor_kind(), "system");
    assert!(record.reason().is_some());
    assert_eq!(
        store
            .get(&pool, &id)
            .await
            .unwrap()
            .unwrap()
            .candidate
            .stage(),
        PromotionStage::RolledBack
    );
    // approve() can never be reached now — the stage guard refuses it.
    let err = store.approve(&pool, &id, &human()).await.unwrap_err();
    assert!(matches!(
        err,
        PromotionStoreError::Promotion(PromotionError::IllegalTransition {
            action: "approve",
            ..
        })
    ));
}

#[tokio::test]
async fn list_by_stage_and_by_artifact() {
    let (_tmp, pool) = temp_pool().await;
    let store = PromotionStore::new();

    let a = store
        .propose(
            &pool,
            ArtifactVersion::new(ArtifactKind::Skill, "rust-ci", 1),
            &human(),
            false,
        )
        .await
        .unwrap();
    let b = store
        .propose(
            &pool,
            ArtifactVersion::new(ArtifactKind::Skill, "rust-ci", 2),
            &human(),
            false,
        )
        .await
        .unwrap();
    let c = store
        .propose(
            &pool,
            ArtifactVersion::new(ArtifactKind::Prompt, "coding-agent", 1),
            &human(),
            false,
        )
        .await
        .unwrap();

    let drafts = store
        .list_by_stage(&pool, PromotionStage::Draft)
        .await
        .unwrap();
    let draft_ids: Vec<&str> = drafts.iter().map(|s| s.id.as_str()).collect();
    assert!(draft_ids.contains(&a.as_str()));
    assert!(draft_ids.contains(&b.as_str()));
    assert!(draft_ids.contains(&c.as_str()));

    store.run_regression(&pool, &a, false).await.unwrap();
    let drafts_after = store
        .list_by_stage(&pool, PromotionStage::Draft)
        .await
        .unwrap();
    assert!(!drafts_after.iter().any(|s| s.id == a));
    let regressed = store
        .list_by_stage(&pool, PromotionStage::RegressionPassed)
        .await
        .unwrap();
    assert!(regressed.iter().any(|s| s.id == a));

    let rust_ci_history = store
        .list_by_artifact(&pool, ArtifactKind::Skill, "rust-ci")
        .await
        .unwrap();
    let history_ids: Vec<&str> = rust_ci_history.iter().map(|s| s.id.as_str()).collect();
    assert!(history_ids.contains(&a.as_str()));
    assert!(history_ids.contains(&b.as_str()));
    assert!(!history_ids.contains(&c.as_str()));
}

#[tokio::test]
async fn propose_idempotent_resolves_a_duplicate_key_to_the_same_candidate() {
    let (_tmp, pool) = temp_pool().await;
    let store = PromotionStore::new();

    let first = store
        .propose_idempotent(&pool, "retry-key-1", artifact(), &human(), false)
        .await
        .unwrap();
    let second = store
        .propose_idempotent(&pool, "retry-key-1", artifact(), &human(), false)
        .await
        .unwrap();
    assert_eq!(
        first, second,
        "the same idempotency key resolves to one candidate"
    );

    // Advancing the first does not create a phantom second row for the same key.
    store.run_regression(&pool, &first, false).await.unwrap();
    let again = store
        .propose_idempotent(&pool, "retry-key-1", artifact(), &human(), false)
        .await
        .unwrap();
    assert_eq!(again, first);
    assert_eq!(
        store
            .get(&pool, &again)
            .await
            .unwrap()
            .unwrap()
            .candidate
            .stage(),
        PromotionStage::RegressionPassed,
        "the retried propose did not reset progress already made"
    );
}

#[tokio::test]
async fn rollback_without_a_predecessor_leaves_the_active_version_in_place() {
    let (_tmp, pool) = temp_pool().await;
    let store = PromotionStore::new();
    let id = store
        .propose(&pool, artifact(), &human(), false)
        .await
        .unwrap();
    store.run_regression(&pool, &id, false).await.unwrap();
    store.start_shadow(&pool, &id).await.unwrap();
    store.start_canary(&pool, &id).await.unwrap();
    store.observe_canary(&pool, &id, false).await.unwrap();
    store.finish_canary(&pool, &id).await.unwrap();
    store.approve(&pool, &id, &human()).await.unwrap();

    store.rollback(&pool, &id, &human()).await.unwrap();
    // Only one version was ever activated — rollback has no predecessor to
    // restore, mirroring `ActiveVersions::rollback`'s no-op-with-no-predecessor
    // rule; the stem stays active at the same (only) version.
    assert_eq!(
        store
            .active_version(&pool, &artifact().stem())
            .await
            .unwrap(),
        Some(7)
    );
}

#[tokio::test]
async fn permission_review_gates_a_synthesized_candidate_through_the_store() {
    let (_tmp, pool) = temp_pool().await;
    let store = PromotionStore::new();
    let id = store
        .propose(
            &pool,
            ArtifactVersion::new(ArtifactKind::Skill, "synth-skill", 1),
            &agent(),
            true,
        )
        .await
        .unwrap();

    let err = store.run_regression(&pool, &id, false).await.unwrap_err();
    assert!(matches!(
        err,
        PromotionStoreError::Promotion(PromotionError::PermissionReviewRequired)
    ));

    store.mark_permission_reviewed(&pool, &id).await.unwrap();
    store.run_regression(&pool, &id, false).await.unwrap();
    assert_eq!(
        store
            .get(&pool, &id)
            .await
            .unwrap()
            .unwrap()
            .candidate
            .stage(),
        PromotionStage::RegressionPassed
    );
}
