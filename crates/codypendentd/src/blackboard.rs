//! The concrete blackboard seams (Phase 5 STEP 5.3): the bridge between the
//! runtime's [`BlackboardChannel`] tool seam, the daemon's [`BlackboardReader`]
//! read seam, and `codypendent-workflow`'s authoritative
//! [`BlackboardStore`](codypendent_workflow::BlackboardStore).
//!
//! Like [`RuntimeExecutor`](crate::executor::RuntimeExecutor) and
//! [`KnowledgeDocumentMutator`](crate::documents::KnowledgeDocumentMutator), these
//! live in the assembly binary because it alone can name all three layers — the
//! runtime (which defines the tool seam), the daemon (which defines the read seam +
//! the fan-out hub), and the workflow crate (which owns the store). Two pieces:
//!
//! * [`AssemblyBlackboardChannel`] — implements the runtime's
//!   [`BlackboardChannel`]: an agent's `blackboard.post` / `blackboard.query`
//!   applies to the run's board through the `BlackboardStore` on the daemon's pool,
//!   and each posted (or superseded) artifact is fanned out to the run's
//!   subscribers over the daemon's [`BlackboardHub`] (persist-before-publish — the
//!   store commit happens first, then the hub publish).
//!
//! * [`WorkflowBlackboardReader`] — implements the daemon's [`BlackboardReader`]:
//!   projects a run's board (kind-filtered) into [`BlackboardItemView`]s for the
//!   `ReadBlackboard` command reply.
//!
//! Every workflow-store error is mapped to the seam's own structured error (the
//! runtime's [`BlackboardChannelError`] or a protocol [`CodypendentError`]) so no
//! caller branches on message text, and internals never leak.

use async_trait::async_trait;
use codypendent_daemon::blackboard::{
    BlackboardHub, BlackboardReadFuture, BlackboardReader, ReadBlackboardRequest,
};
use codypendent_protocol::{BlackboardItemView, CodypendentError};
use codypendent_runtime::blackboard::{BlackboardChannel, BlackboardChannelError, BlackboardPost};
use codypendent_workflow::{
    BlackboardError, BlackboardItem, BlackboardKind, BlackboardStore, NewBlackboardItem,
};
use sqlx::SqlitePool;

/// Project a stored workflow artifact into its wire/runtime view, carrying the run
/// id with it so a live delivery routes without the enclosing frame.
fn item_to_view(workflow_run_id: &str, item: BlackboardItem) -> BlackboardItemView {
    BlackboardItemView {
        id: item.id,
        workflow_run_id: workflow_run_id.to_string(),
        kind: item.kind.as_str().to_string(),
        payload: item.payload,
        author: item.author,
        confidence: item.confidence,
        evidence: item.evidence,
        revision: item.revision,
        superseded_by: item.superseded_by,
    }
}

/// Map a workflow-store error to the runtime tool seam's structured error, so the
/// agent sees a legible, correctable reason (an evidence-required refusal most of
/// all) rather than an opaque backend failure.
fn map_channel_error(error: BlackboardError) -> BlackboardChannelError {
    match error {
        BlackboardError::EvidenceRequired(kind) => {
            BlackboardChannelError::EvidenceRequired(kind.to_string())
        }
        BlackboardError::NotFound(id) => BlackboardChannelError::NotFound(id),
        BlackboardError::AlreadySuperseded(id) => BlackboardChannelError::AlreadySuperseded(id),
        // Database / serialization failures: surface a backend error without
        // leaking the underlying detail's structure to the model.
        other => BlackboardChannelError::Backend(other.to_string()),
    }
}

/// Parse a manifest-facing kind string for the channel seam, mapping an unknown
/// kind to the seam's structured error.
fn channel_kind(kind: &str) -> Result<BlackboardKind, BlackboardChannelError> {
    BlackboardKind::parse_kind(kind)
        .ok_or_else(|| BlackboardChannelError::UnknownKind(kind.to_string()))
}

/// Implements the runtime's [`BlackboardChannel`] over the workflow store + pool,
/// fanning each posted artifact out over the daemon's per-run hub. Cheap to clone
/// (a pool handle, a stateless store, and an `Arc`-backed hub).
#[derive(Clone)]
pub struct AssemblyBlackboardChannel {
    pool: SqlitePool,
    store: BlackboardStore,
    hub: BlackboardHub,
}

impl AssemblyBlackboardChannel {
    /// Build the channel over the daemon's pool and the run fan-out hub.
    #[must_use]
    pub fn new(pool: SqlitePool, hub: BlackboardHub) -> Self {
        Self {
            pool,
            store: BlackboardStore::new(),
            hub,
        }
    }
}

#[async_trait]
impl BlackboardChannel for AssemblyBlackboardChannel {
    async fn post(
        &self,
        workflow_run_id: &str,
        post: BlackboardPost,
    ) -> Result<BlackboardItemView, BlackboardChannelError> {
        let kind = channel_kind(&post.kind)?;
        let new = NewBlackboardItem {
            kind,
            payload: post.payload,
            author: post.author,
            confidence: post.confidence,
            evidence: post.evidence,
        };
        // A post carrying `supersedes` is a correction (posted at the next revision,
        // stamping the old row in one transaction); otherwise a fresh artifact.
        let item = match post.supersedes {
            Some(old_id) => {
                self.store
                    .supersede(&self.pool, workflow_run_id, &old_id, new)
                    .await
            }
            None => self.store.post(&self.pool, workflow_run_id, new).await,
        }
        .map_err(map_channel_error)?;

        // Persist-before-publish: the store commit above happened; only now fan the
        // artifact out to the run's subscribers (best-effort — the store is the
        // durable record).
        let view = item_to_view(workflow_run_id, item);
        self.hub.publish(workflow_run_id, view.clone());
        Ok(view)
    }

    async fn query(
        &self,
        workflow_run_id: &str,
        kind: Option<String>,
        include_superseded: bool,
    ) -> Result<Vec<BlackboardItemView>, BlackboardChannelError> {
        let kind = kind.as_deref().map(channel_kind).transpose()?;
        let items = self
            .store
            .query(&self.pool, workflow_run_id, kind, include_superseded)
            .await
            .map_err(map_channel_error)?;
        Ok(items
            .into_iter()
            .map(|item| item_to_view(workflow_run_id, item))
            .collect())
    }
}

/// Implements the daemon's [`BlackboardReader`] over the workflow store + pool for
/// the `ReadBlackboard` command. Cheap to clone.
#[derive(Clone)]
pub struct WorkflowBlackboardReader {
    pool: SqlitePool,
    store: BlackboardStore,
}

impl WorkflowBlackboardReader {
    /// Build the reader over the daemon's pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            store: BlackboardStore::new(),
        }
    }
}

impl BlackboardReader for WorkflowBlackboardReader {
    fn read(&self, request: ReadBlackboardRequest) -> BlackboardReadFuture<'_> {
        let pool = self.pool.clone();
        let store = self.store;
        Box::pin(async move {
            let ReadBlackboardRequest {
                workflow_run_id,
                kind,
                include_superseded,
                client_id: _,
            } = request;

            // An explicit kind filter that names no known artifact kind is a client
            // error (a typo like `test-result`), rejected legibly rather than
            // silently returning an empty board.
            let kind = match kind.as_deref() {
                Some(k) => Some(BlackboardKind::parse_kind(k).ok_or_else(|| {
                    CodypendentError::new(
                        "workflow.unknown-blackboard-kind",
                        format!("`{k}` is not a known blackboard artifact kind"),
                        false,
                    )
                })?),
                None => None,
            };

            let items = store
                .query(&pool, &workflow_run_id, kind, include_superseded)
                .await
                .map_err(|error| {
                    CodypendentError::new(
                        "workflow.blackboard-read-failed",
                        format!("could not read the blackboard: {error}"),
                        true,
                    )
                })?;
            Ok(items
                .into_iter()
                .map(|item| item_to_view(&workflow_run_id, item))
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::ClientId;
    use serde_json::json;

    /// A migrated pool on a tempfile (WAL needs a real file, not `:memory:`); the
    /// shared migrations create `blackboard_items` (0010). The returned `TempDir`
    /// must be kept alive for the pool's lifetime.
    async fn temp_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = codypendent_daemon::db::open_database(&dir.path().join("codypendent.db"))
            .await
            .expect("migrated pool");
        (dir, pool)
    }

    /// Seed a minimal `workflow_runs` row — `blackboard_items.workflow_run_id`
    /// references it (FK), so a post/query needs the run to exist.
    async fn seed_run(pool: &SqlitePool, id: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO workflow_runs \
             (id, workflow_id, workflow_version, graph_signature, inputs_json, state, \
              created_at, updated_at) \
             VALUES (?, 'wf', 1, 'sig', 'null', 'running', ?, ?)",
        )
        .bind(id)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await
        .expect("seed workflow run");
    }

    fn finding(with_evidence: bool) -> BlackboardPost {
        BlackboardPost {
            kind: "finding".to_string(),
            payload: json!({ "summary": "the parser drops trailing commas" }),
            author: json!({ "role": "investigator", "node_id": "diagnose" }),
            confidence: Some(0.8),
            evidence: if with_evidence {
                vec![json!({ "path": "src/parse.rs", "line": 42 })]
            } else {
                Vec::new()
            },
            supersedes: None,
        }
    }

    #[tokio::test]
    async fn post_lands_and_fans_out_to_subscribers() {
        let (_dir, pool) = temp_pool().await;
        let hub = BlackboardHub::new();
        let channel = AssemblyBlackboardChannel::new(pool.clone(), hub.clone());
        let run = "wfrun-post";
        seed_run(&pool, run).await;
        let mut rx = hub.subscribe(run);

        let posted = channel.post(run, finding(true)).await.expect("posts");
        assert_eq!(posted.kind, "finding");
        assert_eq!(posted.revision, 1);
        // The author is exactly what the runtime built (server-side).
        assert_eq!(posted.author["node_id"], "diagnose");

        // The subscriber receives the same artifact.
        let delivered = rx.recv().await.expect("delivered");
        assert_eq!(delivered.id, posted.id);
        assert_eq!(delivered.workflow_run_id, run);

        // And it is queryable on the live board.
        let live = channel
            .query(run, Some("finding".to_string()), false)
            .await
            .unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, posted.id);
    }

    #[tokio::test]
    async fn evidence_required_refusal_is_structured_and_correctable() {
        let (_dir, pool) = temp_pool().await;
        let channel = AssemblyBlackboardChannel::new(pool.clone(), BlackboardHub::new());
        let run = "wfrun-evidence";
        seed_run(&pool, run).await;

        // A finding is claim-like: without evidence it is refused with the
        // correctable, structured error (not a backend error).
        let err = channel.post(run, finding(false)).await.unwrap_err();
        assert_eq!(err.code(), "blackboard.evidence-required");

        // Re-posting the same finding *with* evidence lands.
        let ok = channel
            .post(run, finding(true))
            .await
            .expect("second post lands");
        assert_eq!(ok.kind, "finding");
    }

    #[tokio::test]
    async fn supersede_publishes_the_new_revision() {
        let (_dir, pool) = temp_pool().await;
        let hub = BlackboardHub::new();
        let channel = AssemblyBlackboardChannel::new(pool.clone(), hub.clone());
        let run = "wfrun-supersede";
        seed_run(&pool, run).await;
        let mut rx = hub.subscribe(run);

        let first = channel.post(run, finding(true)).await.expect("first");
        let _ = rx.recv().await.expect("first delivered");

        let mut correction = finding(true);
        correction.supersedes = Some(first.id.clone());
        correction.payload = json!({ "summary": "corrected: it is only in nested arrays" });
        let second = channel.post(run, correction).await.expect("supersede");
        assert_eq!(second.revision, 2);

        let delivered = rx.recv().await.expect("supersession delivered");
        assert_eq!(delivered.id, second.id);
        assert_eq!(delivered.revision, 2);

        // The live board now shows only the correction.
        let live = channel.query(run, None, false).await.unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, second.id);
    }

    #[tokio::test]
    async fn reader_projects_the_board_and_rejects_an_unknown_kind() {
        let (_dir, pool) = temp_pool().await;
        let channel = AssemblyBlackboardChannel::new(pool.clone(), BlackboardHub::new());
        let run = "wfrun-read";
        seed_run(&pool, run).await;
        channel.post(run, finding(true)).await.expect("seed");

        let reader = WorkflowBlackboardReader::new(pool);
        let items = reader
            .read(ReadBlackboardRequest {
                workflow_run_id: run.to_string(),
                kind: None,
                include_superseded: false,
                client_id: ClientId::new(),
            })
            .await
            .expect("read");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "finding");

        // A typo'd kind filter is a legible rejection, not a silent empty board.
        let err = reader
            .read(ReadBlackboardRequest {
                workflow_run_id: run.to_string(),
                kind: Some("test-result".to_string()),
                include_superseded: false,
                client_id: ClientId::new(),
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, "workflow.unknown-blackboard-kind");
    }
}
