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

use codypendent_daemon::documents::{
    DocumentMutationFuture, DocumentMutationRequest, DocumentMutator,
};
use codypendent_knowledge::{
    apply_mutation, ApplyError, CollaborationMode, DocStoreError, DocumentAuthor, DocumentLeaseStore,
    DocumentStore, LeaseError,
};
use codypendent_protocol::document::DocumentMutation;
use codypendent_protocol::{CodypendentError, DocumentId, UserId};
use sqlx::SqlitePool;

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
            let author = DocumentAuthor::Human {
                user: UserId(client_id.to_string()),
            };

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
                .require(&pool, document_id, mutation_block_target(&mutation), &author)
                .await
                .map_err(map_lease_error)?;

            let outcome = apply_mutation(&pool, document_id, &mutation, mode, &author)
                .await
                .map_err(map_apply_error)?;
            Ok(outcome.sync)
        })
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
        DocStoreError::SuggestionNotPending(_) => CodypendentError::new(
            "document.suggestion-not-pending",
            error.to_string(),
            false,
        ),
        DocStoreError::SuggestionRangeDrifted(_) => CodypendentError::new(
            "document.suggestion-drifted",
            error.to_string(),
            false,
        ),
        // Database / serde / CRDT / corrupt-row: transient or internal — retryable.
        other => CodypendentError::new("document.apply-failed", other.to_string(), true),
    }
}
