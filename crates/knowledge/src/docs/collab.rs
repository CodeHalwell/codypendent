//! Collaboration modes and suggestions (STEP 4.3).
//!
//! Chapter 08's agent collaboration modes map to a policy over how an agent may
//! touch a document. A **suggestion** is a proposed replacement over a character
//! range of a block, recorded as data — it mutates nothing until it is accepted,
//! at which point exactly the annotated range is applied to the CRDT. The default
//! for organization-scope documentation is [`CollaborationMode::Suggest`].

use chrono::Utc;
use codypendent_protocol::DocumentId;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::outbox::{self, KnowledgeIndexEvent};
use crate::types::Scope;

use super::model::{DocumentAuthor, MutationKind};
use super::store::{write_document_tx, DocStoreError, Document};

/// How an agent may collaborate on a document (Chapter 08 table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationMode {
    /// Answer without editing.
    Ask,
    /// Create proposed changes (suggestions), never direct edits.
    Suggest,
    /// Apply changes directly, under the run's approval policy.
    Edit,
    /// Continuously propose edits (suggestions).
    CoAuthor,
    /// Add comments and findings, not content.
    Review,
    /// Detect staleness and propose updates (suggestions).
    Maintain,
}

/// What an agent content edit is allowed to become in a given mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditDisposition {
    /// Apply directly to the CRDT (approval-gated by the run policy).
    Direct,
    /// Record as a suggestion; apply only on accept.
    Suggest,
    /// Not permitted to touch content at all.
    Denied,
}

impl CollaborationMode {
    /// The default mode for a document in `scope`. **Organization-scope
    /// documentation defaults to [`CollaborationMode::Suggest`]** (Chapter 08);
    /// personal scopes default to [`CollaborationMode::Edit`].
    #[must_use]
    pub fn default_for_scope(scope: &Scope) -> Self {
        match scope {
            Scope::Organization(_) => CollaborationMode::Suggest,
            _ => CollaborationMode::Edit,
        }
    }

    /// How a content edit is dispositioned in this mode. Only [`Edit`] permits a
    /// direct CRDT mutation; `Suggest`/`CoAuthor`/`Maintain` route through
    /// suggestions; `Ask`/`Review` may not touch content.
    ///
    /// [`Edit`]: CollaborationMode::Edit
    #[must_use]
    pub fn disposition(&self) -> EditDisposition {
        match self {
            CollaborationMode::Edit => EditDisposition::Direct,
            CollaborationMode::Suggest
            | CollaborationMode::CoAuthor
            | CollaborationMode::Maintain => EditDisposition::Suggest,
            CollaborationMode::Ask | CollaborationMode::Review => EditDisposition::Denied,
        }
    }

    /// Whether an agent in this mode may mutate document content directly.
    #[must_use]
    pub fn allows_direct_edit(&self) -> bool {
        self.disposition() == EditDisposition::Direct
    }
}

/// The lifecycle of a suggestion. Stored scalar in `document_suggestions.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionStatus {
    Pending,
    Accepted,
    Rejected,
}

impl SuggestionStatus {
    fn as_str(&self) -> &'static str {
        match self {
            SuggestionStatus::Pending => "pending",
            SuggestionStatus::Accepted => "accepted",
            SuggestionStatus::Rejected => "rejected",
        }
    }
}

/// A proposed replacement over a character range of a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub id: String,
    pub document_id: DocumentId,
    pub block_id: String,
    /// Character offset (inclusive) within the block's text.
    pub range_start: usize,
    /// Character offset (exclusive).
    pub range_end: usize,
    pub replacement: String,
    pub author: DocumentAuthor,
    pub rationale: Option<String>,
    pub status: SuggestionStatus,
}

/// A suggestion to record (before it gets an id/status).
#[derive(Debug, Clone)]
pub struct NewSuggestion {
    pub block_id: String,
    pub range_start: usize,
    pub range_end: usize,
    pub replacement: String,
    pub author: DocumentAuthor,
    pub rationale: Option<String>,
}

/// The `document_suggestions` store (STEP 4.3). Stateless; pool-per-method.
#[derive(Debug, Clone, Copy, Default)]
pub struct SuggestionStore;

impl SuggestionStore {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Record a pending suggestion against `document_id`. Records nothing to the
    /// document's content (a suggestion mutates nothing until accepted) but
    /// enqueues a `DocumentChanged` outbox row so the review rail updates.
    pub async fn propose(
        &self,
        pool: &SqlitePool,
        document_id: DocumentId,
        new: NewSuggestion,
    ) -> Result<Suggestion, DocStoreError> {
        if new.range_end < new.range_start {
            return Err(DocStoreError::Corrupt(
                "suggestion range_end precedes range_start".into(),
            ));
        }
        let id = Uuid::now_v7().to_string();
        let now = Utc::now();
        let mut tx = pool.begin().await?;
        sqlx::query(
            "INSERT INTO document_suggestions \
             (id, document_id, block_id, range_start, range_end, replacement, author_json, \
              rationale, status, created_at, resolved_at, resolved_by_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, NULL, NULL)",
        )
        .bind(&id)
        .bind(document_id.to_string())
        .bind(&new.block_id)
        .bind(new.range_start as i64)
        .bind(new.range_end as i64)
        .bind(&new.replacement)
        .bind(serde_json::to_string(&new.author)?)
        .bind(new.rationale.as_deref())
        .bind(now.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        outbox::enqueue(
            &mut *tx,
            &KnowledgeIndexEvent::DocumentChanged(document_id),
            now,
        )
        .await?;
        tx.commit().await?;

        Ok(Suggestion {
            id,
            document_id,
            block_id: new.block_id,
            range_start: new.range_start,
            range_end: new.range_end,
            replacement: new.replacement,
            author: new.author,
            rationale: new.rationale,
            status: SuggestionStatus::Pending,
        })
    }

    /// Every pending suggestion for a document, oldest first.
    pub async fn pending(
        &self,
        pool: &SqlitePool,
        document_id: DocumentId,
    ) -> Result<Vec<Suggestion>, DocStoreError> {
        let rows = sqlx::query(
            "SELECT id, block_id, range_start, range_end, replacement, author_json, rationale, \
             status FROM document_suggestions WHERE document_id = ? AND status = 'pending' \
             ORDER BY created_at ASC, id ASC",
        )
        .bind(document_id.to_string())
        .fetch_all(pool)
        .await?;
        rows.iter()
            .map(|row| decode_suggestion(row, document_id))
            .collect()
    }

    /// Accept a suggestion: apply **exactly** the annotated range to the CRDT
    /// (replace `text[range_start..range_end]` with `replacement`), mark it
    /// accepted, and persist the document with an `AcceptSuggestion` authorship
    /// record attributed to `resolver`. Returns the new document revision.
    ///
    /// The suggestion must still be **pending**; a retried or concurrent accept
    /// that finds it already resolved fails with
    /// [`DocStoreError::SuggestionNotPending`] rather than re-applying the range
    /// (which would duplicate inserted text or delete already-updated content).
    /// The claim, the CRDT snapshot write, and the authorship/outbox rows all
    /// commit in **one transaction**, so the system never lands in a state where
    /// the content changed but the suggestion is still pending.
    pub async fn accept(
        &self,
        pool: &SqlitePool,
        doc: &mut Document,
        suggestion_id: &str,
        resolver: &DocumentAuthor,
    ) -> Result<u64, DocStoreError> {
        let suggestion = self.get(pool, doc.id, suggestion_id).await?;
        if suggestion.status != SuggestionStatus::Pending {
            return Err(DocStoreError::SuggestionNotPending(
                suggestion_id.to_string(),
            ));
        }
        // Apply exactly the annotated range: delete [start, end), insert at start.
        let len = suggestion
            .range_end
            .checked_sub(suggestion.range_start)
            .ok_or_else(|| DocStoreError::Corrupt("inverted suggestion range".into()))?;

        let now = Utc::now();
        let mut tx = pool.begin().await?;
        // Atomically claim the pending suggestion. If a concurrent accept already
        // claimed it, this affects 0 rows and we abort before mutating anything.
        let claimed = resolve_pending(
            &mut *tx,
            suggestion_id,
            SuggestionStatus::Accepted,
            resolver,
        )
        .await?;
        if claimed == 0 {
            return Err(DocStoreError::SuggestionNotPending(
                suggestion_id.to_string(),
            ));
        }
        if len > 0 {
            doc.crdt
                .delete_text(&suggestion.block_id, suggestion.range_start, len)?;
        }
        if !suggestion.replacement.is_empty() {
            doc.crdt.insert_text(
                &suggestion.block_id,
                suggestion.range_start,
                &suggestion.replacement,
            )?;
        }
        // The document write commits in the SAME transaction as the claim.
        let revision = write_document_tx(
            &mut tx,
            doc,
            resolver,
            MutationKind::AcceptSuggestion,
            Some(&suggestion.block_id),
            now,
        )
        .await?;
        tx.commit().await?;
        doc.revision = revision;
        Ok(revision)
    }

    /// Reject a suggestion — no content changes; just stamps it rejected. Fails
    /// with [`DocStoreError::SuggestionNotPending`] if it was already resolved.
    pub async fn reject(
        &self,
        pool: &SqlitePool,
        document_id: DocumentId,
        suggestion_id: &str,
        resolver: &DocumentAuthor,
    ) -> Result<(), DocStoreError> {
        // Ensure it exists and belongs to the document.
        let _ = self.get(pool, document_id, suggestion_id).await?;
        let claimed =
            resolve_pending(pool, suggestion_id, SuggestionStatus::Rejected, resolver).await?;
        if claimed == 0 {
            return Err(DocStoreError::SuggestionNotPending(
                suggestion_id.to_string(),
            ));
        }
        Ok(())
    }

    /// Fetch a suggestion by id, scoped to its document.
    async fn get(
        &self,
        pool: &SqlitePool,
        document_id: DocumentId,
        suggestion_id: &str,
    ) -> Result<Suggestion, DocStoreError> {
        let row = sqlx::query(
            "SELECT id, block_id, range_start, range_end, replacement, author_json, rationale, \
             status FROM document_suggestions WHERE id = ? AND document_id = ?",
        )
        .bind(suggestion_id)
        .bind(document_id.to_string())
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| DocStoreError::Corrupt(format!("no such suggestion: {suggestion_id}")))?;
        decode_suggestion(&row, document_id)
    }
}

/// Stamp a suggestion's resolution **only if it is still pending**, within the
/// caller's executor (a pool or a transaction). Returns the number of rows
/// affected — `1` when this call claimed the pending suggestion, `0` when it was
/// already resolved (or does not exist). The `status = 'pending'` guard is what
/// makes accept/reject safe against retries and concurrent resolution.
async fn resolve_pending(
    executor: impl sqlx::SqliteExecutor<'_>,
    suggestion_id: &str,
    status: SuggestionStatus,
    resolver: &DocumentAuthor,
) -> Result<u64, DocStoreError> {
    let affected = sqlx::query(
        "UPDATE document_suggestions SET status = ?, resolved_at = ?, resolved_by_json = ? \
         WHERE id = ? AND status = 'pending'",
    )
    .bind(status.as_str())
    .bind(Utc::now().to_rfc3339())
    .bind(serde_json::to_string(resolver)?)
    .bind(suggestion_id)
    .execute(executor)
    .await?
    .rows_affected();
    Ok(affected)
}

/// Decode a `document_suggestions` row into a [`Suggestion`].
fn decode_suggestion(
    row: &sqlx::sqlite::SqliteRow,
    document_id: DocumentId,
) -> Result<Suggestion, DocStoreError> {
    let status: SuggestionStatus =
        serde_json::from_value(serde_json::Value::String(row.get::<String, _>("status")))?;
    Ok(Suggestion {
        id: row.get("id"),
        document_id,
        block_id: row.get("block_id"),
        range_start: row.get::<i64, _>("range_start") as usize,
        range_end: row.get::<i64, _>("range_end") as usize,
        replacement: row.get("replacement"),
        author: serde_json::from_str(row.get::<String, _>("author_json").as_str())?,
        rationale: row.get("rationale"),
        status,
    })
}
