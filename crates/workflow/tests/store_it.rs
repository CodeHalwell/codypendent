//! STEP 5.2: durable workflow runs recover from a checkpoint, transitions record
//! node-level state/cost, resume continues from the first incomplete node, and a
//! changed graph signature is refused.

use codypendent_workflow::{
    compile_yaml, db, NodeState, WorkflowRunState, WorkflowStore, WorkflowStoreError,
};
use serde_json::json;

const MANIFEST: &str = "\
schema_version: 1
id: pipeline
version: 1
budget:
  maximum_cost_usd: 5.0
steps:
  - id: a
    tool: repository.test
  - id: b
    depends_on: [a]
    tool: repository.test
  - id: c
    depends_on: [b]
    tool: repository.test
";

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

#[tokio::test]
async fn create_run_seeds_pending_nodes_in_topological_order() {
    let (_tmp, pool) = temp_pool().await;
    let compiled = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();

    let id = store
        .create_run(&pool, &compiled, Some("run-1"), &json!({}))
        .await
        .unwrap();

    let snap = store.snapshot(&pool, &id).await.unwrap().unwrap();
    assert_eq!(snap.run.workflow_id, "pipeline");
    assert_eq!(snap.run.state, WorkflowRunState::Pending);
    assert_eq!(snap.run.run_id.as_deref(), Some("run-1"));
    let order: Vec<&str> = snap.nodes.iter().map(|n| n.node_id.as_str()).collect();
    assert_eq!(order, ["a", "b", "c"]);
    assert!(snap.nodes.iter().all(|n| n.state == NodeState::Pending));
}

#[tokio::test]
async fn transitions_record_state_attempt_and_cost() {
    let (_tmp, pool) = temp_pool().await;
    let compiled = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();
    let id = store
        .create_run(&pool, &compiled, None, &json!({}))
        .await
        .unwrap();

    store
        .transition_node(
            &pool,
            &id,
            "a",
            NodeState::Running,
            1,
            Some("agent-run-7"),
            None,
        )
        .await
        .unwrap();
    store
        .transition_node(
            &pool,
            &id,
            "a",
            NodeState::Completed,
            1,
            None,
            Some(&json!({ "usd": 0.4 })),
        )
        .await
        .unwrap();

    let snap = store.snapshot(&pool, &id).await.unwrap().unwrap();
    let a = snap.nodes.iter().find(|n| n.node_id == "a").unwrap();
    assert_eq!(a.state, NodeState::Completed);
    assert_eq!(a.attempt, 1);
    assert_eq!(a.agent_run_id.as_deref(), Some("agent-run-7"));
    assert_eq!(a.cost, Some(json!({ "usd": 0.4 })));
}

#[tokio::test]
async fn resume_continues_from_the_first_incomplete_node() {
    let (_tmp, pool) = temp_pool().await;
    let compiled = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();
    let id = store
        .create_run(&pool, &compiled, None, &json!({}))
        .await
        .unwrap();

    // Finish `a`, checkpoint, then "crash".
    store
        .transition_node(&pool, &id, "a", NodeState::Completed, 1, None, None)
        .await
        .unwrap();
    store
        .record_checkpoint(&pool, &id, &compiled.signature(), Some("artifact-1"))
        .await
        .unwrap();

    // Resume with the same compiled graph: continue from `b`.
    let plan = store.resume(&pool, &id, &compiled).await.unwrap();
    assert_eq!(plan.next_node.as_deref(), Some("b"));
    assert_eq!(
        plan.latest_checkpoint.unwrap().state_artifact_id.as_deref(),
        Some("artifact-1")
    );

    // Once every node is terminal, there is nothing left to resume.
    for node in ["b", "c"] {
        store
            .transition_node(&pool, &id, node, NodeState::Completed, 1, None, None)
            .await
            .unwrap();
    }
    let done = store.resume(&pool, &id, &compiled).await.unwrap();
    assert_eq!(done.next_node, None);
}

#[tokio::test]
async fn resume_refuses_a_changed_graph_signature() {
    let (_tmp, pool) = temp_pool().await;
    let original = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();
    let id = store
        .create_run(&pool, &original, None, &json!({}))
        .await
        .unwrap();

    // A different graph (an extra step) has a different signature.
    let changed_manifest =
        format!("{MANIFEST}  - id: d\n    depends_on: [c]\n    tool: repository.test\n");
    let changed = compile_yaml(&changed_manifest).unwrap();
    assert_ne!(original.signature(), changed.signature());

    let err = store.resume(&pool, &id, &changed).await.unwrap_err();
    assert!(matches!(
        err,
        WorkflowStoreError::GraphSignatureChanged { .. }
    ));
}

#[tokio::test]
async fn run_state_transitions_persist() {
    let (_tmp, pool) = temp_pool().await;
    let compiled = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();
    let id = store
        .create_run(&pool, &compiled, None, &json!({}))
        .await
        .unwrap();

    store
        .set_run_state(&pool, &id, WorkflowRunState::Running)
        .await
        .unwrap();
    let snap = store.snapshot(&pool, &id).await.unwrap().unwrap();
    assert_eq!(snap.run.state, WorkflowRunState::Running);

    // An unknown run id is reported, not silently ignored.
    let err = store
        .set_run_state(&pool, "nope", WorkflowRunState::Failed)
        .await
        .unwrap_err();
    assert!(matches!(err, WorkflowStoreError::NotFound(_)));
}
