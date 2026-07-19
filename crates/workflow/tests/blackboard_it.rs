//! STEP 5.3: the blackboard enforces evidence on claim-like artifacts, supersedes
//! rather than deletes, and isolates items per workflow run.

use codypendent_workflow::{
    compile_yaml, db, BlackboardError, BlackboardKind, BlackboardStore, NewBlackboardItem,
    WorkflowStore,
};
use serde_json::json;

const MANIFEST: &str = "\
schema_version: 1
id: wf
version: 1
steps:
  - id: a
    tool: repository.test
";

async fn seed_run(pool: &sqlx::SqlitePool) -> String {
    let compiled = compile_yaml(MANIFEST).unwrap();
    WorkflowStore::new()
        .create_run(pool, &compiled, None, &json!({}))
        .await
        .unwrap()
}

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

fn finding(text: &str, evidence: Vec<serde_json::Value>) -> NewBlackboardItem {
    NewBlackboardItem {
        kind: BlackboardKind::Finding,
        payload: json!({ "text": text }),
        author: json!({ "agent": "investigator" }),
        confidence: Some(0.8),
        evidence,
    }
}

#[tokio::test]
async fn claim_kinds_require_evidence_but_questions_do_not() {
    let (_tmp, pool) = temp_pool().await;
    let run = seed_run(&pool).await;
    let board = BlackboardStore::new();

    // A finding without evidence is refused.
    let err = board
        .post(
            &pool,
            &run,
            finding("the parser panics on empty input", vec![]),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, BlackboardError::EvidenceRequired("finding")));

    // With evidence it posts.
    let posted = board
        .post(
            &pool,
            &run,
            finding(
                "panics on empty input",
                vec![json!({ "artifact": "log-1" })],
            ),
        )
        .await
        .unwrap();
    assert_eq!(posted.revision, 1);

    // An open question needs no evidence.
    board
        .post(
            &pool,
            &run,
            NewBlackboardItem {
                kind: BlackboardKind::OpenQuestion,
                payload: json!({ "text": "why SQLite over RocksDB?" }),
                author: json!({ "agent": "reviewer" }),
                confidence: None,
                evidence: vec![],
            },
        )
        .await
        .unwrap();

    let all = board.query(&pool, &run, None, false).await.unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn supersede_replaces_and_keeps_the_chain() {
    let (_tmp, pool) = temp_pool().await;
    let run = seed_run(&pool).await;
    let board = BlackboardStore::new();

    let v1 = board
        .post(&pool, &run, finding("v1", vec![json!({ "artifact": "a" })]))
        .await
        .unwrap();
    let v2 = board
        .supersede(
            &pool,
            &run,
            &v1.id,
            finding("v2 corrected", vec![json!({ "artifact": "b" })]),
        )
        .await
        .unwrap();
    assert_eq!(v2.revision, 2);

    // The default view shows only the live item.
    let live = board
        .query(&pool, &run, Some(BlackboardKind::Finding), false)
        .await
        .unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, v2.id);
    assert_eq!(live[0].payload, json!({ "text": "v2 corrected" }));

    // Including superseded shows the whole chain, with v1 pointing at v2.
    let full = board
        .query(&pool, &run, Some(BlackboardKind::Finding), true)
        .await
        .unwrap();
    assert_eq!(full.len(), 2);
    let old = full.iter().find(|i| i.id == v1.id).unwrap();
    assert_eq!(old.superseded_by.as_deref(), Some(v2.id.as_str()));
}

#[tokio::test]
async fn superseding_an_already_superseded_item_is_refused() {
    // Two supersedes of the same item must not both succeed — the loser would
    // fork the chain into two live replacements. The second is rejected.
    let (_tmp, pool) = temp_pool().await;
    let run = seed_run(&pool).await;
    let board = BlackboardStore::new();

    let v1 = board
        .post(&pool, &run, finding("v1", vec![json!({ "artifact": "a" })]))
        .await
        .unwrap();
    board
        .supersede(
            &pool,
            &run,
            &v1.id,
            finding("v2", vec![json!({ "artifact": "b" })]),
        )
        .await
        .unwrap();

    // A second supersede of v1 (now already superseded) is refused, and the
    // live view still shows exactly one finding.
    let err = board
        .supersede(
            &pool,
            &run,
            &v1.id,
            finding("v2b", vec![json!({ "artifact": "c" })]),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, BlackboardError::AlreadySuperseded(_)));

    let live = board
        .query(&pool, &run, Some(BlackboardKind::Finding), false)
        .await
        .unwrap();
    assert_eq!(live.len(), 1, "the chain never forked");
}

#[tokio::test]
async fn get_returns_an_item_scoped_to_its_run() {
    let (_tmp, pool) = temp_pool().await;
    let run_a = seed_run(&pool).await;
    let run_b = seed_run(&pool).await;
    let board = BlackboardStore::new();

    let posted = board
        .post(&pool, &run_a, finding("in A", vec![json!({ "e": 1 })]))
        .await
        .unwrap();

    // Fetchable within its own run…
    let got = board.get(&pool, &run_a, &posted.id).await.unwrap();
    assert_eq!(got.map(|i| i.id), Some(posted.id.clone()));
    // …never visible from another run's board, and an unknown id is None.
    assert!(board
        .get(&pool, &run_b, &posted.id)
        .await
        .unwrap()
        .is_none());
    assert!(board.get(&pool, &run_a, "ghost").await.unwrap().is_none());
}

#[tokio::test]
async fn history_walks_the_full_supersession_chain() {
    let (_tmp, pool) = temp_pool().await;
    let run = seed_run(&pool).await;
    let board = BlackboardStore::new();

    // v1 → v2 → v3, a three-link correction chain.
    let v1 = board
        .post(&pool, &run, finding("v1", vec![json!({ "artifact": "a" })]))
        .await
        .unwrap();
    let v2 = board
        .supersede(
            &pool,
            &run,
            &v1.id,
            finding("v2", vec![json!({ "artifact": "b" })]),
        )
        .await
        .unwrap();
    let v3 = board
        .supersede(
            &pool,
            &run,
            &v2.id,
            finding("v3", vec![json!({ "artifact": "c" })]),
        )
        .await
        .unwrap();

    let expected = [v1.id.as_str(), v2.id.as_str(), v3.id.as_str()];

    // The chain is identical whichever link is used as the anchor, always oldest
    // → newest, and the live head's `superseded_by` is None.
    for anchor in [&v1.id, &v2.id, &v3.id] {
        let chain = board.history(&pool, &run, anchor).await.unwrap();
        let ids: Vec<&str> = chain.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids, expected,
            "chain from {anchor} must be the full lineage"
        );
        let revisions: Vec<u32> = chain.iter().map(|i| i.revision).collect();
        assert_eq!(revisions, vec![1, 2, 3]);
        assert_eq!(chain.last().unwrap().superseded_by, None);
    }

    // An id not on this board has no history.
    assert!(board
        .history(&pool, &run, "ghost")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn blackboards_are_isolated_per_workflow_run() {
    let (_tmp, pool) = temp_pool().await;
    let run_a = seed_run(&pool).await;
    let run_b = seed_run(&pool).await;
    let board = BlackboardStore::new();

    board
        .post(&pool, &run_a, finding("only in A", vec![json!({ "e": 1 })]))
        .await
        .unwrap();

    assert_eq!(
        board.query(&pool, &run_a, None, false).await.unwrap().len(),
        1
    );
    assert!(board
        .query(&pool, &run_b, None, false)
        .await
        .unwrap()
        .is_empty());
}
