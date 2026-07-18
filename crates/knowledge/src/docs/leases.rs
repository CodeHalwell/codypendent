//! Block-range edit leases for collaborative documents (STEP 4.3 transport).
//!
//! Enforces **one writer per block-range**: a client that intends to edit a block
//! acquires a lease over it (the wire request is [`DocumentEditLease`]); a
//! whole-document lease (`block_id = None`) covers structural edits (block
//! insert/delete/reorder) and conflicts with any block lease on that document.
//! Readers take no lease and are unlimited. Leases carry a TTL and expire, so a
//! crashed holder never blocks a document forever — an expired lease is reclaimed
//! lazily on the next acquire/require (the Phase-1 workspace-lease approach).
//!
//! The daemon owns the orchestration: on a [`DocumentEditLease`] request it
//! [`acquire`](DocumentLeaseStore::acquire)s, and before applying a
//! [`MutateDocument`](codypendent_protocol::CommandBody) it
//! [`require`](DocumentLeaseStore::require)s that the writer is not blocked. This
//! store is that enforcement engine — it is daemon-agnostic and testable on its
//! own.
//!
//! [`DocumentEditLease`]: codypendent_protocol::DocumentEditLease

use std::time::Duration;

use chrono::{DateTime, Utc};
use codypendent_protocol::DocumentId;
use sqlx::{SqliteExecutor, SqlitePool};
use uuid::Uuid;

use super::model::DocumentAuthor;

/// An error from the lease store.
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    /// A **different** writer holds an active, unexpired lease over an
    /// overlapping range. The payload is that writer's stable identity.
    #[error("document range is leased by {holder_key}")]
    Conflict { holder_key: String },
}

/// A granted lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentLease {
    pub id: String,
    pub document_id: DocumentId,
    /// The block leased, or `None` for a whole-document (structural) lease.
    pub block_id: Option<String>,
    pub holder: DocumentAuthor,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// The `document_leases` store. Stateless; the pool is passed per method.
#[derive(Debug, Clone, Copy, Default)]
pub struct DocumentLeaseStore;

impl DocumentLeaseStore {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// A stable identity for a holder, so a re-acquire by the same writer renews
    /// rather than conflicts.
    fn holder_key(author: &DocumentAuthor) -> String {
        match author {
            DocumentAuthor::Human { user } => format!("human:{}", user.0),
            DocumentAuthor::Agent { run_id, .. } => format!("agent:{run_id}"),
            DocumentAuthor::Integration { integration } => format!("integration:{integration}"),
        }
    }

    /// Acquire (or renew) a lease over `block_id` (`None` = the whole document)
    /// for `holder`, valid for `ttl`. Fails with [`LeaseError::Conflict`] if a
    /// *different* writer holds an active, unexpired lease over an overlapping
    /// range. A re-acquire by the same holder renews the expiry in place.
    pub async fn acquire(
        &self,
        pool: &SqlitePool,
        document_id: DocumentId,
        block_id: Option<&str>,
        holder: &DocumentAuthor,
        ttl: Duration,
    ) -> Result<DocumentLease, LeaseError> {
        let now = Utc::now();
        let key = Self::holder_key(holder);
        let mut tx = pool.begin().await?;

        // Someone else holding an overlapping active, unexpired lease blocks us.
        if let Some(other) = conflicting_holder(&mut *tx, document_id, block_id, &key, now).await? {
            // Nothing was written; drop the (read-only) transaction.
            drop(tx);
            return Err(LeaseError::Conflict { holder_key: other });
        }

        // Already hold this exact range? Renew it.
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT id FROM document_leases \
             WHERE document_id = ? AND state = 'active' AND holder_key = ? \
               AND ((block_id IS NULL AND ? IS NULL) OR block_id = ?) LIMIT 1",
        )
        .bind(document_id.to_string())
        .bind(&key)
        .bind(block_id)
        .bind(block_id)
        .fetch_optional(&mut *tx)
        .await?;

        let expires =
            now + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::seconds(0));
        let id = match existing {
            Some(id) => {
                sqlx::query(
                    "UPDATE document_leases SET expires_at = ?, acquired_at = ? WHERE id = ?",
                )
                .bind(expires.to_rfc3339())
                .bind(now.to_rfc3339())
                .bind(&id)
                .execute(&mut *tx)
                .await?;
                id
            }
            None => {
                let id = Uuid::now_v7().to_string();
                sqlx::query(
                    "INSERT INTO document_leases \
                     (id, document_id, block_id, holder_json, holder_key, state, acquired_at, \
                      expires_at) \
                     VALUES (?, ?, ?, ?, ?, 'active', ?, ?)",
                )
                .bind(&id)
                .bind(document_id.to_string())
                .bind(block_id)
                .bind(serde_json::to_string(holder)?)
                .bind(&key)
                .bind(now.to_rfc3339())
                .bind(expires.to_rfc3339())
                .execute(&mut *tx)
                .await?;
                id
            }
        };
        tx.commit().await?;
        Ok(DocumentLease {
            id,
            document_id,
            block_id: block_id.map(str::to_owned),
            holder: holder.clone(),
            acquired_at: now,
            expires_at: expires,
        })
    }

    /// Enforce single-writer **without acquiring**: `Ok` if `holder` may write
    /// `block_id` right now — either no one holds a conflicting lease, or `holder`
    /// is the writer who does — and [`LeaseError::Conflict`] if a different writer
    /// holds an overlapping range. This is the pre-check the daemon runs before
    /// applying a mutation.
    pub async fn require(
        &self,
        pool: &SqlitePool,
        document_id: DocumentId,
        block_id: Option<&str>,
        holder: &DocumentAuthor,
    ) -> Result<(), LeaseError> {
        let key = Self::holder_key(holder);
        match conflicting_holder(pool, document_id, block_id, &key, Utc::now()).await? {
            Some(other) => Err(LeaseError::Conflict { holder_key: other }),
            None => Ok(()),
        }
    }

    /// Release a lease by id (idempotent — releasing an already-released or
    /// unknown lease is a no-op).
    pub async fn release(&self, pool: &SqlitePool, lease_id: &str) -> Result<(), LeaseError> {
        sqlx::query(
            "UPDATE document_leases SET state = 'released', released_at = ? \
             WHERE id = ? AND state = 'active'",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(lease_id)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// The current holder of an active, unexpired lease over `block_id`, if any.
    pub async fn active_holder(
        &self,
        pool: &SqlitePool,
        document_id: DocumentId,
        block_id: Option<&str>,
    ) -> Result<Option<DocumentAuthor>, LeaseError> {
        let holder: Option<String> = sqlx::query_scalar(
            "SELECT holder_json FROM document_leases \
             WHERE document_id = ? AND state = 'active' AND expires_at > ? \
               AND ((block_id IS NULL AND ? IS NULL) OR block_id = ?) \
             ORDER BY acquired_at DESC LIMIT 1",
        )
        .bind(document_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind(block_id)
        .bind(block_id)
        .fetch_optional(pool)
        .await?;
        holder
            .map(|json| serde_json::from_str(&json).map_err(LeaseError::from))
            .transpose()
    }
}

/// The stable identity of a *different* writer holding an active, unexpired lease
/// that overlaps the requested range, if one exists. Overlap rules: a
/// whole-document request (`block_id = None`) conflicts with **any** active lease
/// on the document; a block request conflicts with a lease on the same block or a
/// whole-document lease.
async fn conflicting_holder<'e, E: SqliteExecutor<'e>>(
    exec: E,
    document_id: DocumentId,
    block_id: Option<&str>,
    self_key: &str,
    now: DateTime<Utc>,
) -> Result<Option<String>, LeaseError> {
    let now_s = now.to_rfc3339();
    let holder: Option<String> = if block_id.is_none() {
        sqlx::query_scalar(
            "SELECT holder_key FROM document_leases \
             WHERE document_id = ? AND state = 'active' AND expires_at > ? AND holder_key <> ? \
             LIMIT 1",
        )
        .bind(document_id.to_string())
        .bind(&now_s)
        .bind(self_key)
        .fetch_optional(exec)
        .await?
    } else {
        sqlx::query_scalar(
            "SELECT holder_key FROM document_leases \
             WHERE document_id = ? AND state = 'active' AND expires_at > ? AND holder_key <> ? \
               AND (block_id IS NULL OR block_id = ?) \
             LIMIT 1",
        )
        .bind(document_id.to_string())
        .bind(&now_s)
        .bind(self_key)
        .bind(block_id)
        .fetch_optional(exec)
        .await?
    };
    Ok(holder)
}
