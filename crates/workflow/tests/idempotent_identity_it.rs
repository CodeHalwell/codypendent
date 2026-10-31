//! Concurrent duplicate deliveries must identify the entire accepted request,
//! including repository and parsed definition fields outside the graph signature.

use codypendent_workflow::{
    compile_yaml, db, WorkflowRunAttribution, WorkflowStore, WorkflowStoreError,
};
use serde_json::json;

const MANIFEST: &str = "\
schema_version: 1
id: identity-check
version: 1
inputs:
  query:
    type: string
    required: false
budget:
  maximum_cost_usd: 5.0
steps:
  - id: inspect
    tool: repository.test
  - id: verify
    depends_on: [inspect]
    tool: repository.test
";

type Creation = Result<String, WorkflowStoreError>;

async fn race(
    left_manifest: Option<&str>,
    right_manifest: Option<&str>,
    left_repository: Option<&str>,
    right_repository: Option<&str>,
) -> (tempfile::TempDir, sqlx::SqlitePool, Creation, Creation) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("identity.db")).await.unwrap();
    let store = WorkflowStore::new();
    let left = compile_yaml(left_manifest.unwrap_or(MANIFEST)).unwrap();
    let right = compile_yaml(right_manifest.unwrap_or(MANIFEST)).unwrap();
    // These regressions deliberately exercise identity missing from this hash.
    assert_eq!(left.signature(), right.signature());
    let inputs = json!({ "query": "status" });

    let (left_result, right_result) = tokio::join!(
        store.create_run_idempotent_owned(
            &pool,
            &left,
            "same-client-key",
            &inputs,
            WorkflowRunAttribution {
                manifest: left_manifest,
                repository: left_repository,
                owner_uid: 1_001,
            },
        ),
        store.create_run_idempotent_owned(
            &pool,
            &right,
            "same-client-key",
            &inputs,
            WorkflowRunAttribution {
                manifest: right_manifest,
                repository: right_repository,
                owner_uid: 1_001,
            },
        ),
    );

    (tmp, pool, left_result, right_result)
}

fn assert_one_winner(left: Creation, right: Creation) -> String {
    match (left, right) {
        (Ok(id), Err(error)) | (Err(error), Ok(id)) => {
            assert!(
                matches!(error, WorkflowStoreError::IdempotencyKeyReused { .. }),
                "a conflicting duplicate must be rejected, got {error:?}"
            );
            id
        }
        other => panic!("expected exactly one accepted request, got {other:?}"),
    }
}

async fn assert_single_persisted_graph(pool: &sqlx::SqlitePool, id: &str) {
    let run_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workflow_runs")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(run_count, 1);
    let node_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workflow_nodes")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(node_count, 2, "duplicate delivery must not seed more nodes");
    let snapshot = WorkflowStore::new()
        .snapshot(pool, id)
        .await
        .unwrap()
        .unwrap();
    let node_ids: Vec<&str> = snapshot
        .nodes
        .iter()
        .map(|node| node.node_id.as_str())
        .collect();
    assert_eq!(node_ids, ["inspect", "verify"]);
}

#[tokio::test]
async fn concurrent_same_key_rejects_a_different_repository() {
    let (_tmp, pool, left, right) = race(
        Some(MANIFEST),
        Some(MANIFEST),
        Some("/repositories/first"),
        Some("/repositories/second"),
    )
    .await;
    let winner = assert_one_winner(left, right);
    assert_single_persisted_graph(&pool, &winner).await;
}

#[tokio::test]
async fn concurrent_same_key_rejects_a_budget_only_definition_change() {
    let changed = MANIFEST.replace("maximum_cost_usd: 5.0", "maximum_cost_usd: 7.0");
    let (_tmp, pool, left, right) = race(
        Some(MANIFEST),
        Some(&changed),
        Some("/repositories/project"),
        Some("/repositories/project"),
    )
    .await;
    let winner = assert_one_winner(left, right);
    assert_single_persisted_graph(&pool, &winner).await;
}

#[tokio::test]
async fn concurrent_same_key_rejects_an_input_schema_only_definition_change() {
    let changed = MANIFEST.replace("required: false", "required: true");
    let (_tmp, pool, left, right) = race(
        Some(MANIFEST),
        Some(&changed),
        Some("/repositories/project"),
        Some("/repositories/project"),
    )
    .await;
    let winner = assert_one_winner(left, right);
    assert_single_persisted_graph(&pool, &winner).await;
}

#[tokio::test]
async fn concurrent_same_key_deduplicates_semantically_identical_yaml() {
    let formatted = format!(
        "# The same definition, with presentation changes.\n{}\n# End.\n",
        MANIFEST.replace("type: string", "type: 'string'")
    );
    assert_ne!(MANIFEST, formatted);
    let (_tmp, pool, left, right) = race(
        Some(MANIFEST),
        Some(&formatted),
        Some("/repositories/project"),
        Some("/repositories/project"),
    )
    .await;
    let left = left.unwrap();
    assert_eq!(left, right.unwrap());
    assert_single_persisted_graph(&pool, &left).await;
}

#[tokio::test]
async fn concurrent_legacy_requests_without_manifests_still_deduplicate() {
    let (_tmp, pool, left, right) = race(
        None,
        None,
        Some("/repositories/project"),
        Some("/repositories/project"),
    )
    .await;
    let left = left.unwrap();
    assert_eq!(left, right.unwrap());
    assert_single_persisted_graph(&pool, &left).await;
}

#[tokio::test]
async fn concurrent_same_key_fails_closed_for_missing_versus_present_definition() {
    let (_tmp, pool, left, right) = race(
        None,
        Some(MANIFEST),
        Some("/repositories/project"),
        Some("/repositories/project"),
    )
    .await;
    let winner = assert_one_winner(left, right);
    assert_single_persisted_graph(&pool, &winner).await;
}

#[tokio::test]
async fn provided_manifest_must_parse_compile_and_match_the_compiled_graph_before_insert() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("manifest-validation.db"))
        .await
        .unwrap();
    let store = WorkflowStore::new();
    let compiled = compile_yaml(MANIFEST).unwrap();
    let other_graph =
        format!("{MANIFEST}  - id: extra\n    depends_on: [verify]\n    tool: repository.test\n");
    assert_ne!(
        compiled.signature(),
        compile_yaml(&other_graph).unwrap().signature()
    );
    let inputs = json!({ "query": "status" });

    for (case, manifest, expected_message) in [
        (
            "invalid-yaml",
            "schema_version: [",
            "workflow request manifest cannot be parsed",
        ),
        (
            "invalid-graph",
            "schema_version: 1\nid: identity-check\nversion: 1\nsteps: []\n",
            "workflow request manifest cannot be compiled",
        ),
        (
            "different-graph",
            other_graph.as_str(),
            "workflow request manifest does not match its compiled graph",
        ),
    ] {
        let error = store
            .create_run_idempotent_owned(
                &pool,
                &compiled,
                case,
                &inputs,
                WorkflowRunAttribution {
                    manifest: Some(manifest),
                    repository: Some("/repositories/project"),
                    owner_uid: 1_001,
                },
            )
            .await
            .expect_err(case);
        assert!(
            matches!(error, WorkflowStoreError::Corrupt(ref message) if message == expected_message),
            "{case}: expected fail-closed manifest validation, got {error:?}"
        );
        for table in ["workflow_runs", "workflow_nodes"] {
            let query = format!("SELECT COUNT(*) FROM {table}");
            let count: i64 = sqlx::query_scalar(&query).fetch_one(&pool).await.unwrap();
            assert_eq!(
                count, 0,
                "{case}: invalid attribution must not write {table}"
            );
        }
    }
}
