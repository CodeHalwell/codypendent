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
