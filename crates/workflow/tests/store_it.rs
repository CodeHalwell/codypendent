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
async fn manifest_round_trips_and_is_none_when_absent() {
    // The manifest is stored so a daemon can recompile-and-resume after a restart
    // (STEP 5.2 startup recovery). A run created with one returns it verbatim; one
    // created without returns None (recovery skips such a run).
    let (_tmp, pool) = temp_pool().await;
    let compiled = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();

    let with_manifest = store
        .create_run(&pool, &compiled, None, &json!({}), Some(MANIFEST))
        .await
        .unwrap();
    assert_eq!(
        store
            .manifest(&pool, &with_manifest)
            .await
            .unwrap()
            .as_deref(),
        Some(MANIFEST)
    );

    let without = store
        .create_run(&pool, &compiled, None, &json!({}), None)
        .await
        .unwrap();
    assert_eq!(store.manifest(&pool, &without).await.unwrap(), None);

    // An unknown run id is also None, never an error.
    assert_eq!(store.manifest(&pool, "nope").await.unwrap(), None);
}

#[tokio::test]
async fn create_run_seeds_pending_nodes_in_topological_order() {
    let (_tmp, pool) = temp_pool().await;
    let compiled = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();

    let id = store
        .create_run(&pool, &compiled, Some("run-1"), &json!({}), None)
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
async fn create_run_idempotent_dedups_by_key() {
    let (_tmp, pool) = temp_pool().await;
    let compiled = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();

    // Two creations with the same idempotency key resolve to one run (a duplicate
    // StartWorkflow delivery after a lost ack does not fork a second run).
    let first = store
        .create_run_idempotent(&pool, &compiled, "cmd-42", &json!({ "x": 1 }), None)
        .await
        .unwrap();
    let second = store
        .create_run_idempotent(&pool, &compiled, "cmd-42", &json!({ "x": 1 }), None)
        .await
        .unwrap();
    assert_eq!(first, second, "same key ⇒ same run id");

    // Exactly one run, with its three pending nodes (not six).
    assert_eq!(store.list_incomplete_runs(&pool).await.unwrap().len(), 1);
    let snap = store.snapshot(&pool, &first).await.unwrap().unwrap();
    assert_eq!(snap.nodes.len(), 3);

    // A different key is a different run.
    let other = store
        .create_run_idempotent(&pool, &compiled, "cmd-99", &json!({ "x": 1 }), None)
        .await
        .unwrap();
    assert_ne!(first, other);
    assert_eq!(store.list_incomplete_runs(&pool).await.unwrap().len(), 2);
}

#[tokio::test]
async fn transitions_record_state_attempt_and_cost() {
    let (_tmp, pool) = temp_pool().await;
    let compiled = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();
    let id = store
        .create_run(&pool, &compiled, None, &json!({}), None)
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
        .create_run(&pool, &compiled, None, &json!({}), None)
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
        .create_run(&pool, &original, None, &json!({}), None)
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
async fn retry_from_node_resets_the_node_and_everything_downstream() {
    let (_tmp, pool) = temp_pool().await;
    let compiled = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();
    let id = store
        .create_run(&pool, &compiled, None, &json!({}), None)
        .await
        .unwrap();

    // Drive the whole pipeline to completion (a → b → c), recording cost + an
    // agent-run id along the way.
    for node in ["a", "b", "c"] {
        store
            .transition_node(
                &pool,
                &id,
                node,
                NodeState::Running,
                1,
                Some("agent-x"),
                None,
            )
            .await
            .unwrap();
        store
            .transition_node(
                &pool,
                &id,
                node,
                NodeState::Completed,
                1,
                None,
                Some(&json!({ "usd": 0.2 })),
            )
            .await
            .unwrap();
    }
    store
        .set_run_state(&pool, &id, WorkflowRunState::Completed)
        .await
        .unwrap();

    // Retry from `b`: `b` and its downstream `c` reset; `a` (upstream) is untouched.
    let reset = store
        .retry_from_node(&pool, &id, "b", &compiled)
        .await
        .unwrap();
    assert_eq!(reset, vec!["b".to_string(), "c".to_string()]);

    let snap = store.snapshot(&pool, &id).await.unwrap().unwrap();
    // The run is Running again so the executor picks it back up.
    assert_eq!(snap.run.state, WorkflowRunState::Running);

    let node = |n: &str| snap.nodes.iter().find(|x| x.node_id == n).unwrap().clone();
    // `a` kept its terminal state and provenance.
    assert_eq!(node("a").state, NodeState::Completed);
    assert_eq!(node("a").cost, Some(json!({ "usd": 0.2 })));
    // `b` and `c` are fresh Pending: state, attempt, cost, and agent-run id cleared.
    for n in ["b", "c"] {
        let r = node(n);
        assert_eq!(r.state, NodeState::Pending, "{n} reset to pending");
        assert_eq!(r.attempt, 0, "{n} attempt cleared");
        assert_eq!(r.cost, None, "{n} cost cleared");
        assert_eq!(r.agent_run_id, None, "{n} agent-run id cleared");
    }

    // Composes with resume: the first incomplete node is now `b`.
    let plan = store.resume(&pool, &id, &compiled).await.unwrap();
    assert_eq!(plan.next_node.as_deref(), Some("b"));
}

#[tokio::test]
async fn retry_from_node_refuses_a_changed_graph_or_unknown_node() {
    let (_tmp, pool) = temp_pool().await;
    let original = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();
    let id = store
        .create_run(&pool, &original, None, &json!({}), None)
        .await
        .unwrap();

    // A changed graph is refused, exactly like `resume`.
    let changed_manifest =
        format!("{MANIFEST}  - id: d\n    depends_on: [c]\n    tool: repository.test\n");
    let changed = compile_yaml(&changed_manifest).unwrap();
    let err = store
        .retry_from_node(&pool, &id, "b", &changed)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        WorkflowStoreError::GraphSignatureChanged { .. }
    ));

    // A node absent from the graph is NotFound (not a silent no-op).
    let err = store
        .retry_from_node(&pool, &id, "ghost", &original)
        .await
        .unwrap_err();
    assert!(matches!(err, WorkflowStoreError::NotFound(_)));
}

/// A diamond: `a` fans out to `b` and `c`, which both feed `d`.
const DIAMOND: &str = "\
schema_version: 1
id: diamond
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
    depends_on: [a]
    tool: repository.test
  - id: d
    depends_on: [b, c]
    tool: repository.test
";

#[tokio::test]
async fn ready_nodes_is_the_parallel_frontier() {
    let (_tmp, pool) = temp_pool().await;
    let compiled = compile_yaml(DIAMOND).unwrap();
    let store = WorkflowStore::new();
    let id = store
        .create_run(&pool, &compiled, None, &json!({}), None)
        .await
        .unwrap();

    let complete = |node: &'static str| {
        let store = &store;
        let pool = &pool;
        let id = &id;
        async move {
            store
                .transition_node(pool, id, node, NodeState::Completed, 1, None, None)
                .await
                .unwrap();
        }
    };

    // Fresh: only the source `a` is ready.
    assert_eq!(
        store.ready_nodes(&pool, &id, &compiled).await.unwrap(),
        vec!["a"]
    );

    // After `a`, both `b` and `c` are ready at once — the parallel frontier `resume`
    // (which returns a single node) cannot express.
    complete("a").await;
    assert_eq!(
        store.ready_nodes(&pool, &id, &compiled).await.unwrap(),
        vec!["b", "c"]
    );

    // Completing only `b` leaves `c` ready; `d` still waits on `c`.
    complete("b").await;
    assert_eq!(
        store.ready_nodes(&pool, &id, &compiled).await.unwrap(),
        vec!["c"]
    );

    // Once both `b` and `c` are done, `d` becomes ready.
    complete("c").await;
    assert_eq!(
        store.ready_nodes(&pool, &id, &compiled).await.unwrap(),
        vec!["d"]
    );

    // With everything terminal, the frontier is empty.
    complete("d").await;
    assert!(store
        .ready_nodes(&pool, &id, &compiled)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_failed_dependency_blocks_the_dependent() {
    let (_tmp, pool) = temp_pool().await;
    let compiled = compile_yaml(DIAMOND).unwrap();
    let store = WorkflowStore::new();
    let id = store
        .create_run(&pool, &compiled, None, &json!({}), None)
        .await
        .unwrap();

    // `a` completes; `b` fails; `c` completes. `d` depends on both `b` and `c`, so
    // a failed `b` keeps `d` off the frontier — it is not ready and never blocks a
    // sibling.
    store
        .transition_node(&pool, &id, "a", NodeState::Completed, 1, None, None)
        .await
        .unwrap();
    store
        .transition_node(&pool, &id, "b", NodeState::Failed, 1, None, None)
        .await
        .unwrap();
    store
        .transition_node(&pool, &id, "c", NodeState::Completed, 1, None, None)
        .await
        .unwrap();

    assert!(
        store
            .ready_nodes(&pool, &id, &compiled)
            .await
            .unwrap()
            .is_empty(),
        "d must stay blocked while its dependency b is failed"
    );

    // `resume` must agree with the frontier: `d` is non-terminal but stranded
    // behind failed `b`, so it is NOT the next node — the run has no
    // schedulable work, and the blocked set names the stranded node with its
    // blocker. (The old behavior reported `d` as `next_node`, so a recovery
    // loop composing list_incomplete_runs + resume livelocked here.)
    let plan = store.resume(&pool, &id, &compiled).await.unwrap();
    assert_eq!(
        plan.next_node, None,
        "a node stranded behind a failure is not resumable"
    );
    assert_eq!(
        plan.blocked_nodes,
        vec![("d".to_string(), "b".to_string())],
        "the stranded node and its blocking ancestor are reported"
    );
}

#[tokio::test]
async fn ready_nodes_refuses_a_changed_graph_signature() {
    let (_tmp, pool) = temp_pool().await;
    let original = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();
    let id = store
        .create_run(&pool, &original, None, &json!({}), None)
        .await
        .unwrap();

    let changed = compile_yaml(DIAMOND).unwrap();
    let err = store.ready_nodes(&pool, &id, &changed).await.unwrap_err();
    assert!(matches!(
        err,
        WorkflowStoreError::GraphSignatureChanged { .. }
    ));
}

#[tokio::test]
async fn list_incomplete_runs_returns_only_non_terminal_runs() {
    let (_tmp, pool) = temp_pool().await;
    let compiled = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();

    // Three runs: leave one pending, drive one to running, finish one.
    let pending = store
        .create_run(&pool, &compiled, Some("pending"), &json!({}), None)
        .await
        .unwrap();
    let running = store
        .create_run(&pool, &compiled, Some("running"), &json!({}), None)
        .await
        .unwrap();
    let completed = store
        .create_run(&pool, &compiled, Some("done"), &json!({}), None)
        .await
        .unwrap();
    store
        .set_run_state(&pool, &running, WorkflowRunState::Running)
        .await
        .unwrap();
    store
        .set_run_state(&pool, &completed, WorkflowRunState::Completed)
        .await
        .unwrap();

    // Startup recovery sees the pending + running runs (oldest first), not the
    // completed one.
    let incomplete = store.list_incomplete_runs(&pool).await.unwrap();
    let ids: Vec<&str> = incomplete.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec![pending.as_str(), running.as_str()]);
    // The records carry enough to recompile + resume (run_id, signature preserved).
    assert_eq!(incomplete[1].run_id.as_deref(), Some("running"));
    assert_eq!(incomplete[1].graph_signature, compiled.signature());
}

#[tokio::test]
async fn run_state_transitions_persist() {
    let (_tmp, pool) = temp_pool().await;
    let compiled = compile_yaml(MANIFEST).unwrap();
    let store = WorkflowStore::new();
    let id = store
        .create_run(&pool, &compiled, None, &json!({}), None)
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
