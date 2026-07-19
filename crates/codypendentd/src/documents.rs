//! The concrete [`DocumentMutator`]: applies a client's `MutateDocument` onto the
//! authoritative collaborative document (Phase 4 STEP 4.3 client transport).
//!
//! Like [`RuntimeExecutor`](crate::executor::RuntimeExecutor), this lives in the
//! assembly binary because it bridges the daemon (which defines the
//! [`DocumentMutator`] seam) and `codypendent-knowledge` (which owns the
//! authoritative Loro document, its collaboration-mode gate, and its edit-lease
//! store). The daemon crate cannot name knowledge, so the composition happens
//! here.
//!
//! For one accepted mutation it:
//! 1. derives the document's default [`CollaborationMode`] from its **scope**
//!    (organization docs default to *Suggest*, personal scopes to *Edit*) — read
//!    cheaply via [`DocumentStore::scope`], without reconstructing the CRDT;
//! 2. **enforces single-writer** over the mutation's target range through the
//!    edit-lease store's `require` (a different holder ⇒ rejected); and
//! 3. applies the mutation via [`apply_mutation`], which mode-gates content edits
//!    (direct / suggestion / denied) and returns the [`DocumentSync`] the server
//!    broadcasts to the document's subscribers.
//!
//! Every failure is mapped to a **structured** [`CodypendentError`] the client
//! branches on by code, never by message text.

use std::time::Duration;

use codypendent_daemon::documents::{
    DocumentLeaseFuture, DocumentLeaseReleaseRequest, DocumentLeaseRequest, DocumentLeaser,
    DocumentMutationFuture, DocumentMutationRequest, DocumentMutator, DocumentReleaseFuture,
};
use codypendent_knowledge::{
    apply_mutation, ApplyError, CollaborationMode, DocStoreError, DocumentAuthor,
    DocumentLeaseStore, DocumentStore, LeaseError,
};
use codypendent_protocol::document::DocumentMutation;
use codypendent_protocol::{ClientId, CodypendentError, DocumentId, DocumentLeaseGrant, UserId};
use sqlx::SqlitePool;

/// The default lifetime for a lease acquired without an explicit TTL: long enough
/// for an active editing burst, short enough that a crashed holder's range is
/// reclaimed promptly (leases expire lazily on the next acquire/require).
const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(300);

/// Applies `MutateDocument` commands over the knowledge document engine. Cheap to
/// clone — a pool handle plus a stateless lease store.
#[derive(Clone)]
pub struct KnowledgeDocumentMutator {
    pool: SqlitePool,
    leases: DocumentLeaseStore,
}

impl KnowledgeDocumentMutator {
    /// Build a mutator over the daemon's pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            leases: DocumentLeaseStore::new(),
        }
    }
}

impl DocumentMutator for KnowledgeDocumentMutator {
    fn apply_mutation(&self, request: DocumentMutationRequest) -> DocumentMutationFuture<'_> {
        let pool = self.pool.clone();
        let leases = self.leases;
        Box::pin(async move {
            let DocumentMutationRequest {
                document_id,
                mutation,
                client_id,
            } = request;

            // A `MutateDocument` command is a human client edit; an agent authors
            // through the runtime, not this path. The client id doubles as the
            // lease-holder identity, so the same client that acquired a block lease
            // is the one this mutation is attributed to.
            let author = human_author(client_id);

            // Derive the collaboration mode from the document's scope. Absent ⇒
            // the document does not exist; reject before touching leases.
            let scope = DocumentStore::new()
                .scope(&pool, document_id)
                .await
                .map_err(map_store_error)?
                .ok_or_else(|| not_found(document_id))?;
            let mode = CollaborationMode::default_for_scope(&scope);

            // Enforce single-writer over the target range before applying.
            leases
                .require(
                    &pool,
                    document_id,
                    mutation_block_target(&mutation),
                    &author,
                )
                .await
                .map_err(map_lease_error)?;

            let outcome = apply_mutation(&pool, document_id, &mutation, mode, &author)
                .await
                .map_err(map_apply_error)?;
            Ok(outcome.sync)
        })
    }
}

impl DocumentLeaser for KnowledgeDocumentMutator {
    fn acquire(&self, request: DocumentLeaseRequest) -> DocumentLeaseFuture<'_> {
        let pool = self.pool.clone();
        let leases = self.leases;
        Box::pin(async move {
            let DocumentLeaseRequest {
                document_id,
                block_id,
                ttl,
                client_id,
            } = request;

            // The lease holder is the acquiring client, identified exactly as the
            // mutation author is — so the lease this takes is the one the
            // `MutateDocument` `require` later recognises as the same writer.
            let holder = human_author(client_id);

            // Reject a lease on a document that does not exist, before writing a
            // lease row that could never gate a real mutation.
            if DocumentStore::new()
                .scope(&pool, document_id)
                .await
                .map_err(map_store_error)?
                .is_none()
            {
                return Err(not_found(document_id));
            }

            let lease = leases
                .acquire(
                    &pool,
                    document_id,
                    block_id.as_deref(),
                    &holder,
                    ttl.unwrap_or(DEFAULT_LEASE_TTL),
                )
                .await
                .map_err(map_lease_error)?;

            Ok(DocumentLeaseGrant {
                lease_id: lease.id,
                document_id: lease.document_id,
                block_id: lease.block_id,
                expires_at: lease.expires_at,
            })
        })
    }

    fn release(&self, request: DocumentLeaseReleaseRequest) -> DocumentReleaseFuture<'_> {
        let pool = self.pool.clone();
        let leases = self.leases;
        Box::pin(async move {
            // Release is idempotent (releasing an unknown/already-released lease is
            // a no-op); the lease id is the bearer capability the grant returned to
            // the acquirer.
            leases
                .release(&pool, &request.lease_id)
                .await
                .map_err(map_lease_error)
        })
    }
}

/// A `MutateDocument`/lease client identity as a document author. Both paths use
/// the same mapping so a lease acquired by a client is recognised as that same
/// writer when its mutation runs the `require` pre-check.
fn human_author(client_id: ClientId) -> DocumentAuthor {
    DocumentAuthor::Human {
        user: UserId(client_id.to_string()),
    }
}

/// The block range a mutation writes, for lease enforcement. A **structural** edit
/// (block insert/delete) takes the whole-document lease (`None`) — it reshapes the
/// block list, so it must not proceed while any block is being written. A text
/// edit or annotation is scoped to its target block. Accepting/rejecting a
/// suggestion is a *resolution* action (a role decision, not a fresh content
/// write), so it takes no lease here.
fn mutation_block_target(mutation: &DocumentMutation) -> Option<&str> {
    match mutation {
        DocumentMutation::EditText { block_id, .. } => Some(block_id),
        DocumentMutation::Annotate { suggestion } => Some(&suggestion.block_id),
        DocumentMutation::Insert { .. }
        | DocumentMutation::Delete { .. }
        | DocumentMutation::AcceptSuggestion { .. }
        | DocumentMutation::RejectSuggestion { .. } => None,
        // A newer client's op — apply_mutation rejects it as unsupported.
        _ => None,
    }
}

/// The `document.not-found` error (a document the client named does not exist).
fn not_found(document_id: DocumentId) -> CodypendentError {
    CodypendentError::new(
        "document.not-found",
        format!("no document {document_id}"),
        false,
    )
}

/// Map an edit-lease failure to a structured error. A range held by a different
/// writer is `document.range-leased` (not retryable — the client must wait for the
/// holder to release); infrastructure failures collapse to a retryable
/// `document.apply-failed`.
fn map_lease_error(error: LeaseError) -> CodypendentError {
    match error {
        LeaseError::Conflict { holder_key } => CodypendentError::new(
            "document.range-leased",
            format!("document range is being edited by {holder_key}"),
            false,
        ),
        other => CodypendentError::new("document.apply-failed", other.to_string(), true),
    }
}

/// Map a mutation-application failure to a structured error the client branches on
/// by code. A stale revision is retryable (reload + retry); a mode denial, invalid
/// content, drifted/settled suggestion, or unknown op is not; other store/CRDT
/// failures collapse to a retryable `document.apply-failed`.
fn map_apply_error(error: ApplyError) -> CodypendentError {
    match error {
        ApplyError::NoSuchDocument(id) => not_found(id),
        ApplyError::Denied { mode, reason } => CodypendentError::new(
            "document.mode-denied",
            format!("collaboration mode {mode:?} forbids this mutation: {reason}"),
            false,
        ),
        ApplyError::InvalidContent(err) => CodypendentError::new(
            "document.invalid-content",
            format!("invalid block content: {err}"),
            false,
        ),
        ApplyError::Unsupported => CodypendentError::new(
            "protocol.unsupported-payload",
            "unsupported document mutation".to_string(),
            false,
        ),
        ApplyError::Store(store) => map_store_error(store),
    }
}

/// Map a document-store failure to a structured error.
fn map_store_error(error: DocStoreError) -> CodypendentError {
    match error {
        DocStoreError::NoSuchDocument(id) => not_found(id),
        DocStoreError::StaleRevision { .. } => CodypendentError::new(
            "document.stale-revision",
            format!("{error}; reload and retry"),
            true,
        ),
        DocStoreError::SuggestionNotPending(_) => {
            CodypendentError::new("document.suggestion-not-pending", error.to_string(), false)
        }
        DocStoreError::SuggestionRangeDrifted(_) => {
            CodypendentError::new("document.suggestion-drifted", error.to_string(), false)
        }
        // Database / serde / CRDT / corrupt-row: transient or internal — retryable.
        other => CodypendentError::new("document.apply-failed", other.to_string(), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_daemon::db;
    use codypendent_knowledge::{
        BlockContent, DocumentBlock, DocumentMetadata, DocumentStore, NewDocument, Scope,
    };

    async fn temp_pool() -> (tempfile::TempDir, SqlitePool) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::open_database(&tmp.path().join("codypendent.db"))
            .await
            .expect("open db");
        (tmp, pool)
    }

    async fn seed_document(pool: &SqlitePool) -> DocumentId {
        DocumentStore::new()
            .create(
                pool,
                NewDocument {
                    title: "Doc".into(),
                    scope: Scope::System,
                    metadata: DocumentMetadata::default(),
                    blocks: vec![DocumentBlock::with_id(
                        "p",
                        BlockContent::Paragraph {
                            text: "hello".into(),
                        },
                    )],
                },
                &human_author(ClientId::new()),
            )
            .await
            .expect("create document")
            .id
    }

    fn acquire(
        document_id: DocumentId,
        block: Option<&str>,
        client: ClientId,
    ) -> DocumentLeaseRequest {
        DocumentLeaseRequest {
            document_id,
            block_id: block.map(str::to_owned),
            ttl: None,
            client_id: client,
        }
    }

    #[tokio::test]
    async fn acquire_grants_a_lease_and_a_second_writer_is_refused() {
        let (_tmp, pool) = temp_pool().await;
        let doc = seed_document(&pool).await;
        let leaser = KnowledgeDocumentMutator::new(pool.clone());

        let a = ClientId::new();
        let grant = leaser
            .acquire(acquire(doc, Some("p"), a))
            .await
            .expect("first writer acquires the block");
        assert!(!grant.lease_id.is_empty());
        assert_eq!(grant.block_id.as_deref(), Some("p"));
        assert_eq!(grant.document_id, doc);

        // A different client is refused the same block: `document.range-leased`.
        let b = ClientId::new();
        let err = leaser
            .acquire(acquire(doc, Some("p"), b))
            .await
            .expect_err("a second writer must be refused");
        assert_eq!(err.code, "document.range-leased");

        // The holder renews its own lease rather than conflicting with itself.
        leaser
            .acquire(acquire(doc, Some("p"), a))
            .await
            .expect("the holder renews in place");
    }

    #[tokio::test]
    async fn releasing_frees_the_range_for_another_writer() {
        let (_tmp, pool) = temp_pool().await;
        let doc = seed_document(&pool).await;
        let leaser = KnowledgeDocumentMutator::new(pool.clone());

        let a = ClientId::new();
        let grant = leaser.acquire(acquire(doc, Some("p"), a)).await.unwrap();

        // Release, then a different writer can take the same block.
        leaser
            .release(DocumentLeaseReleaseRequest {
                lease_id: grant.lease_id.clone(),
                client_id: a,
            })
            .await
            .expect("release succeeds");
        // Releasing again is an idempotent no-op.
        leaser
            .release(DocumentLeaseReleaseRequest {
                lease_id: grant.lease_id,
                client_id: a,
            })
            .await
            .expect("release is idempotent");

        let b = ClientId::new();
        leaser
            .acquire(acquire(doc, Some("p"), b))
            .await
            .expect("the range is free after release");
    }

    #[tokio::test]
    async fn a_whole_document_lease_conflicts_with_a_block_lease() {
        let (_tmp, pool) = temp_pool().await;
        let doc = seed_document(&pool).await;
        let leaser = KnowledgeDocumentMutator::new(pool.clone());

        // A holds block "p"; B's whole-document (structural) lease overlaps it.
        let a = ClientId::new();
        leaser.acquire(acquire(doc, Some("p"), a)).await.unwrap();

        let b = ClientId::new();
        let err = leaser
            .acquire(acquire(doc, None, b))
            .await
            .expect_err("a structural lease conflicts with any block lease");
        assert_eq!(err.code, "document.range-leased");
    }

    #[tokio::test]
    async fn acquiring_a_lease_on_a_missing_document_is_not_found() {
        let (_tmp, pool) = temp_pool().await;
        let leaser = KnowledgeDocumentMutator::new(pool);
        let err = leaser
            .acquire(acquire(DocumentId::new(), Some("p"), ClientId::new()))
            .await
            .expect_err("a phantom document cannot be leased");
        assert_eq!(err.code, "document.not-found");
    }
}
