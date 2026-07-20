//! The concrete [`WorkflowStarter`]: compiles a client's `StartWorkflow` manifest
//! and creates a durable run (Phase 5 STEP 5.2).
//!
//! Like [`KnowledgeDocumentMutator`](crate::documents::KnowledgeDocumentMutator),
//! this lives in the assembly binary because it bridges the daemon (which declares
//! the [`WorkflowStarter`] seam) and `codypendent-workflow` (which owns the
//! compiler and the durable [`WorkflowStore`]). The daemon crate cannot name the
//! workflow crate, so the composition happens here.
//!
//! For one accepted `StartWorkflow` it compiles the manifest (structural
//! validation — the tool/skill/role registry cross-check is a later wiring step)
//! and creates a durable run: state `pending`, one `pending` node per step, the
//! inputs recorded with the run. **Driving the created run is a later step** — this
//! seam only makes runs durably creatable, so a client (or a future startup-recovery
//! pass over `list_incomplete_runs`) has a run row to advance. Every failure is a
//! structured [`CodypendentError`] the client branches on by code, never by text.

use codypendent_daemon::workflows::{StartWorkflowRequest, WorkflowStartFuture, WorkflowStarter};
use codypendent_protocol::CodypendentError;
use codypendent_workflow::{compile_yaml, WorkflowStore};
use sqlx::SqlitePool;

/// Creates durable workflow runs from `StartWorkflow` commands over the daemon's
/// pool. Cheap to clone — a pool handle plus a stateless store.
#[derive(Clone)]
pub struct WorkflowRunStarter {
    pool: SqlitePool,
}

impl WorkflowRunStarter {
    /// Build a starter over the daemon's pool (the workflow tables share it — the
    /// migrations are workspace-wide).
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl WorkflowStarter for WorkflowRunStarter {
    fn start(&self, request: StartWorkflowRequest) -> WorkflowStartFuture<'_> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let StartWorkflowRequest {
                manifest, inputs, ..
            } = request;

            // Compile the manifest — a malformed workflow is the client's to fix,
            // surfaced verbatim. Non-retryable: recompiling the same text fails the
            // same way.
            let compiled = compile_yaml(&manifest).map_err(|error| {
                CodypendentError::new(
                    "workflow.invalid-manifest",
                    format!("workflow manifest does not compile: {error}"),
                    false,
                )
            })?;

            // Create the durable run (one `pending` node per step). A store error
            // may be transient (a busy database), so mark it retryable.
            WorkflowStore::new()
                .create_run(&pool, &compiled, None, &inputs)
                .await
                .map_err(|error| {
                    CodypendentError::new(
                        "workflow.store-error",
                        format!("could not create the workflow run: {error}"),
                        true,
                    )
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::ClientId;
    use serde_json::json;

    const MANIFEST: &str = "\
schema_version: 1
id: repair-github-check
version: 1
orchestration_reason: independent-review
budget:
  maximum_agents: 2
steps:
  - id: inspect
    agent:
      role: investigator
    outputs: [finding]
  - id: verify
    depends_on: [inspect]
    tool: repository.test
    outputs: [test_result]
";

    async fn temp_pool() -> (tempfile::TempDir, SqlitePool) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = codypendent_workflow::db::open(&tmp.path().join("codypendent.db"))
            .await
            .unwrap();
        (tmp, pool)
    }

    #[tokio::test]
    async fn start_compiles_the_manifest_and_creates_a_durable_run() {
        let (_tmp, pool) = temp_pool().await;
        let starter = WorkflowRunStarter::new(pool.clone());

        let run_id = starter
            .start(StartWorkflowRequest {
                manifest: MANIFEST.to_owned(),
                inputs: json!({ "pull_request": 7 }),
                client_id: ClientId::new(),
            })
            .await
            .expect("a valid manifest starts a run");

        // The run and a pending node per step are durable in the store.
        let snapshot = WorkflowStore::new()
            .snapshot(&pool, &run_id)
            .await
            .unwrap()
            .expect("the run row exists");
        assert_eq!(snapshot.run.workflow_id, "repair-github-check");
        assert_eq!(snapshot.run.inputs, json!({ "pull_request": 7 }));
        assert_eq!(snapshot.nodes.len(), 2);
        assert!(snapshot
            .nodes
            .iter()
            .all(|n| n.state == codypendent_workflow::NodeState::Pending));
    }

    #[tokio::test]
    async fn start_rejects_an_uncompilable_manifest_without_creating_a_run() {
        let (_tmp, pool) = temp_pool().await;
        let starter = WorkflowRunStarter::new(pool.clone());

        let error = starter
            .start(StartWorkflowRequest {
                // Valid header but no steps → a compile error, not a panic.
                manifest: "schema_version: 1\nid: empty\nversion: 1\nsteps: []\n".to_owned(),
                inputs: json!(null),
                client_id: ClientId::new(),
            })
            .await
            .expect_err("an uncompilable manifest is rejected");
        assert_eq!(error.code, "workflow.invalid-manifest");

        // Nothing was created.
        assert!(WorkflowStore::new()
            .list_incomplete_runs(&pool)
            .await
            .unwrap()
            .is_empty());
    }
}
