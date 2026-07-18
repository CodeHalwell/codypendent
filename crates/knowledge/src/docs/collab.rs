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
use super::store::{insert_authorship, write_document_tx, DocStoreError, Document};

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
    /// The document revision this suggestion was proposed against. Accept refuses
    /// if the document has advanced, so a zero-length (insertion) suggestion —
    /// whose empty `original` cannot detect a shift on its own — can never be
    /// applied at a stale offset.
    pub source_revision: u64,
    /// The text the proposer saw at `[range_start, range_end)`. Accept refuses if
    /// the block has since drifted so these offsets no longer cover it.
    pub original: String,
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
    /// The document revision the proposer computed these offsets against. `propose`
    /// refuses if the document has already advanced past it (the offsets would be
    /// stale on arrival), and it is stored so accept can refuse later drift — this
    /// is the caller's *observed* revision, never the latest row read at write time.
    pub source_revision: u64,
    /// The text currently at `[range_start, range_end)` as the proposer sees it —
    /// the anchor accept verifies before applying (empty for an insertion).
    pub original: String,
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
        // The suggestion is anchored to the revision the *proposer* observed, not
        // whatever is current now: if the document advanced between the client
        // computing the offsets and this write, the offsets are already stale, so
        // refuse rather than store a suggestion whose accept-time guard would
        // wrongly pass. (Also validates the document exists.)
        let current_revision: i64 =
            sqlx::query_scalar("SELECT revision FROM documents WHERE id = ?")
                .bind(document_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DocStoreError::NoSuchDocument(document_id))?;
        if current_revision as u64 != new.source_revision {
            return Err(DocStoreError::StaleRevision {
                id: document_id,
                expected: new.source_revision,
            });
        }
        let source_revision = new.source_revision as i64;
        sqlx::query(
            "INSERT INTO document_suggestions \
             (id, document_id, block_id, range_start, range_end, source_revision, original, \
              replacement, author_json, rationale, status, created_at, resolved_at, \
              resolved_by_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, NULL, NULL)",
        )
        .bind(&id)
        .bind(document_id.to_string())
        .bind(&new.block_id)
        .bind(new.range_start as i64)
        .bind(new.range_end as i64)
        .bind(source_revision)
        .bind(&new.original)
        .bind(&new.replacement)
        .bind(serde_json::to_string(&new.author)?)
        .bind(new.rationale.as_deref())
        .bind(now.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        // Record the proposal in the authorship log (same transaction) so
        // `authorship()` can audit who proposed generated text — the content is
        // unchanged, so this stamps the current revision, not a new one.
        insert_authorship(
            &mut tx,
            document_id,
            Some(&new.block_id),
            &new.author,
            MutationKind::Suggest,
            new.source_revision,
            &now.to_rfc3339(),
        )
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
            source_revision: source_revision as u64,
            original: new.original,
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
            "SELECT id, block_id, range_start, range_end, source_revision, original, replacement, author_json, \
             rationale, status FROM document_suggestions WHERE document_id = ? AND status = 'pending' \
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
        // Guard against range drift. First, refuse if the document has advanced
        // since the suggestion was proposed: any saved edit can shift the stored
        // offsets, and for a zero-length *insertion* the empty-text check below
        // cannot detect that on its own. `doc` is the caller's current replica; a
        // stale replica is additionally caught by the revision guard in
        // `write_document_tx`.
        if suggestion.source_revision != doc.revision {
            return Err(DocStoreError::SuggestionRangeDrifted(
                suggestion_id.to_string(),
            ));
        }
        // Second, verify the text now under the stored offsets is what the
        // proposer saw (catches unsaved in-place edits at the same revision). An
        // out-of-range range surfaces as OutOfBounds via `text_range`.
        let current = doc.crdt.text_range(
            &suggestion.block_id,
            suggestion.range_start,
            suggestion.range_end,
        )?;
        if current != suggestion.original {
            return Err(DocStoreError::SuggestionRangeDrifted(
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
    ///
    /// The status stamp and a `DocumentChanged` outbox row commit **together**, so
    /// subscribers and index workers learn the review rail changed — just as
    /// `propose` and `accept` do — rather than the rejection staying invisible
    /// until some later document mutation.
    pub async fn reject(
        &self,
        pool: &SqlitePool,
        document_id: DocumentId,
        suggestion_id: &str,
        resolver: &DocumentAuthor,
    ) -> Result<(), DocStoreError> {
        // Ensure it exists and belongs to the document; keep it for the block the
        // rejection is attributed against in the authorship log.
        let suggestion = self.get(pool, document_id, suggestion_id).await?;
        let now = Utc::now();
        let mut tx = pool.begin().await?;
        let claimed = resolve_pending(
            &mut *tx,
            suggestion_id,
            SuggestionStatus::Rejected,
            resolver,
        )
        .await?;
        if claimed == 0 {
            return Err(DocStoreError::SuggestionNotPending(
                suggestion_id.to_string(),
            ));
        }
        // Record the rejection in the authorship log (same transaction) so
        // `authorship()` can audit who rejected the suggestion, mirroring how
        // accept records `AcceptSuggestion`. No content changed, so this stamps the
        // document's current revision rather than a new one.
        let current_revision: i64 =
            sqlx::query_scalar("SELECT revision FROM documents WHERE id = ?")
                .bind(document_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DocStoreError::NoSuchDocument(document_id))?;
        insert_authorship(
            &mut tx,
            document_id,
            Some(&suggestion.block_id),
            resolver,
            MutationKind::RejectSuggestion,
            current_revision as u64,
            &now.to_rfc3339(),
        )
        .await?;
        outbox::enqueue(
            &mut *tx,
            &KnowledgeIndexEvent::DocumentChanged(document_id),
            now,
        )
        .await?;
        tx.commit().await?;
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
            "SELECT id, block_id, range_start, range_end, source_revision, original, replacement, author_json, \
             rationale, status FROM document_suggestions WHERE id = ? AND document_id = ?",
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
        source_revision: row.get::<i64, _>("source_revision") as u64,
        original: row.get("original"),
        replacement: row.get("replacement"),
        author: serde_json::from_str(row.get::<String, _>("author_json").as_str())?,
        rationale: row.get("rationale"),
        status,
    })
}
