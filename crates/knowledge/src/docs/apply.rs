//! Applying a semantic document mutation (STEP 4.3 transport).
//!
//! [`apply_mutation`] is the daemon-facing seam of the collaborative-document
//! transport: it maps a protocol [`DocumentMutation`] onto the authoritative Loro
//! document and suggestion store, gated by the document's [`CollaborationMode`],
//! and returns the resulting [`DocumentSync`] to broadcast to `Document`
//! subscribers. The daemon does not depend on this crate directly — it reaches
//! this engine through the `codypendentd` assembly seam, exactly as it reaches
//! the agent runtime through `RunExecutor` (dependency inversion).
//!
//! # The mode gate (engine level)
//!
//! The [`CollaborationMode`] governs how an agent *authors* content, so it gates
//! only the content-editing mutations:
//!
//! - **Ask / Review** ([`EditDisposition::Denied`]) reject content edits outright.
//! - **Suggest / Co-author / Maintain** ([`EditDisposition::Suggest`]) route a
//!   text edit or annotation to the suggestion store as a *pending suggestion*;
//!   a block insert/delete has no suggestion form, so it is refused.
//! - **Edit** ([`EditDisposition::Direct`]) applies text and block edits straight
//!   to the CRDT.
//!
//! Accepting and rejecting a suggestion are *resolution* actions, not authoring,
//! so they are **not** mode-gated here — who may resolve a suggestion is a
//! client-role decision the daemon owns. This keeps the two axes (how you author
//! vs. who may review) cleanly separated.

use codypendent_protocol::document::{DocumentMutation, DocumentSync, SuggestionInput};
use codypendent_protocol::DocumentId;
use sqlx::SqlitePool;

use super::collab::{CollaborationMode, EditDisposition, NewSuggestion, SuggestionStore};
use super::crdt::DocCrdtError;
use super::model::{BlockContent, DocumentAuthor, DocumentBlock, MutationKind};
use super::store::{DocStoreError, Document, DocumentStore};

/// An error from applying a document mutation.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// A document-store or CRDT operation failed (missing block, drifted range,
    /// stale revision, database error).
    #[error(transparent)]
    Store(#[from] DocStoreError),
    /// No document with this id exists.
    #[error("no such document: {0}")]
    NoSuchDocument(DocumentId),
    /// The document's collaboration mode forbids this mutation. `reason` is a
    /// human explanation; callers branch on the variant, never the text.
    #[error("collaboration mode {mode:?} forbids this mutation: {reason}")]
    Denied {
        mode: CollaborationMode,
        reason: &'static str,
    },
    /// An `Insert` mutation carried block content that did not deserialize into a
    /// [`BlockContent`].
    #[error("invalid block content: {0}")]
    InvalidContent(#[source] serde_json::Error),
    /// The mutation was an unknown/newer-client op this build cannot model.
    #[error("unsupported document mutation")]
    Unsupported,
}

impl From<DocCrdtError> for ApplyError {
    fn from(error: DocCrdtError) -> Self {
        ApplyError::Store(DocStoreError::from(error))
    }
}

/// What a successful [`apply_mutation`] did to the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationEffect {
    /// The mutation was applied directly to document content (Edit mode).
    Applied(MutationKind),
    /// The edit was recorded as a pending suggestion, carrying its id (a
    /// suggest-disposition mode, or any `Annotate`).
    Suggested(String),
    /// A pending suggestion was accepted and its range applied, carrying its id.
    Accepted(String),
    /// A pending suggestion was rejected without applying, carrying its id.
    Rejected(String),
}

/// The result of applying a mutation: what happened, plus the [`DocumentSync`] to
/// broadcast to the document's subscribers.
#[derive(Debug, Clone)]
pub struct MutationOutcome {
    /// What the mutation did.
    pub effect: MutationEffect,
    /// The authoritative CRDT sync to publish to `Document` subscribers. Its
    /// `revision` is the document's revision after the mutation (unchanged for a
    /// proposed or rejected suggestion, which touch no content), and `update`
    /// carries the current CRDT snapshot — a receiver merges it into its replica.
    pub sync: DocumentSync,
}

/// Apply a semantic `mutation` to `document_id`, gated by `mode` and attributed
/// to `author`, and return the sync to broadcast. See the module docs for the
/// mode gate. Errors leave the document unchanged (each underlying store op is
/// transactional and revision-guarded).
pub async fn apply_mutation(
    pool: &SqlitePool,
    document_id: DocumentId,
    mutation: &DocumentMutation,
    mode: CollaborationMode,
    author: &DocumentAuthor,
) -> Result<MutationOutcome, ApplyError> {
    let store = DocumentStore::new();
    let mut doc = store
        .load(pool, document_id)
        .await?
        .ok_or(ApplyError::NoSuchDocument(document_id))?;

    let effect = match mutation {
        DocumentMutation::Insert {
            index,
            block_id,
            content,
        } => {
            require_direct(mode, "a block insert has no suggestion form")?;
            let content: BlockContent =
                serde_json::from_value(content.clone()).map_err(ApplyError::InvalidContent)?;
            let block = DocumentBlock::with_id(block_id.clone(), content);
            doc.crdt.insert_block(*index as usize, &block)?;
            store
                .save(
                    pool,
                    &mut doc,
                    author,
                    MutationKind::InsertBlock,
                    Some(block_id),
                )
                .await?;
            MutationEffect::Applied(MutationKind::InsertBlock)
        }
        DocumentMutation::Delete { block_id } => {
            require_direct(mode, "a block delete has no suggestion form")?;
            doc.crdt.delete_block(block_id)?;
            store
                .save(
                    pool,
                    &mut doc,
                    author,
                    MutationKind::DeleteBlock,
                    Some(block_id),
                )
                .await?;
            MutationEffect::Applied(MutationKind::DeleteBlock)
        }
        DocumentMutation::EditText {
            block_id,
            position,
            delete_len,
            insert,
        } => match mode.disposition() {
            EditDisposition::Direct => {
                let pos = *position as usize;
                let del = *delete_len as usize;
                if del > 0 {
                    doc.crdt.delete_text(block_id, pos, del)?;
                }
                if !insert.is_empty() {
                    doc.crdt.insert_text(block_id, pos, insert)?;
                }
                store
                    .save(
                        pool,
                        &mut doc,
                        author,
                        MutationKind::EditText,
                        Some(block_id),
                    )
                    .await?;
                MutationEffect::Applied(MutationKind::EditText)
            }
            EditDisposition::Suggest => {
                let end = position.saturating_add(*delete_len);
                let id = propose_range(
                    pool,
                    &doc,
                    block_id,
                    u64::from(*position),
                    u64::from(end),
                    insert.clone(),
                    None,
                    author,
                )
                .await?;
                MutationEffect::Suggested(id)
            }
            EditDisposition::Denied => {
                return Err(ApplyError::Denied {
                    mode,
                    reason: "this mode may not edit document content",
                });
            }
        },
        DocumentMutation::Annotate { suggestion } => {
            if mode.disposition() == EditDisposition::Denied {
                return Err(ApplyError::Denied {
                    mode,
                    reason: "this mode may not propose content changes",
                });
            }
            let SuggestionInput {
                block_id,
                range_start,
                range_end,
                replacement,
                rationale,
            } = suggestion;
            let id = propose_range(
                pool,
                &doc,
                block_id,
                u64::from(*range_start),
                u64::from(*range_end),
                replacement.clone(),
                rationale.clone(),
                author,
            )
            .await?;
            MutationEffect::Suggested(id)
        }
        DocumentMutation::AcceptSuggestion { suggestion_id } => {
            SuggestionStore::new()
                .accept(pool, &mut doc, suggestion_id, author)
                .await?;
            MutationEffect::Accepted(suggestion_id.clone())
        }
        DocumentMutation::RejectSuggestion { suggestion_id } => {
            SuggestionStore::new()
                .reject(pool, doc.id, suggestion_id, author)
                .await?;
            MutationEffect::Rejected(suggestion_id.clone())
        }
        // `DocumentMutation` is `#[non_exhaustive]` and carries a `serde(other)`
        // `Unknown`: a newer client's op deserializes here and is rejected
        // structurally rather than crashing the daemon.
        _ => return Err(ApplyError::Unsupported),
    };

    let snapshot = doc.crdt.snapshot()?;
    Ok(MutationOutcome {
        effect,
        sync: DocumentSync {
            document_id,
            revision: doc.revision,
            update: snapshot,
        },
    })
}

/// Refuse a mutation that only has a direct form when the mode does not permit
/// direct edits. `suggest_reason` explains why the op cannot be a suggestion.
fn require_direct(mode: CollaborationMode, suggest_reason: &'static str) -> Result<(), ApplyError> {
    match mode.disposition() {
        EditDisposition::Direct => Ok(()),
        EditDisposition::Suggest => Err(ApplyError::Denied {
            mode,
            reason: suggest_reason,
        }),
        EditDisposition::Denied => Err(ApplyError::Denied {
            mode,
            reason: "this mode may not edit document content",
        }),
    }
}

/// Record a range replacement as a pending suggestion, anchoring it to the text
/// the proposer currently sees at `[start, end)` (so accept can detect later
/// drift) and to the document's current revision.
#[allow(clippy::too_many_arguments)]
async fn propose_range(
    pool: &SqlitePool,
    doc: &Document,
    block_id: &str,
    start: u64,
    end: u64,
    replacement: String,
    rationale: Option<String>,
    author: &DocumentAuthor,
) -> Result<String, ApplyError> {
    let range_start = start as usize;
    let range_end = end as usize;
    // Read the anchor text under the range (an inverted or out-of-range range
    // surfaces as a recoverable error rather than a panic).
    let original = doc.crdt.text_range(block_id, range_start, range_end)?;
    let suggestion = SuggestionStore::new()
        .propose(
            pool,
            doc.id,
            NewSuggestion {
                block_id: block_id.to_owned(),
                range_start,
                range_end,
                source_revision: doc.revision,
                original,
                replacement,
                author: author.clone(),
                rationale,
            },
        )
        .await?;
    Ok(suggestion.id)
}
