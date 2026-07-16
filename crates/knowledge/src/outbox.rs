//! The index-update outbox (Chapter 06).
//!
//! Every authoritative write to `registry_items` / `memories` / `code_*` also
//! appends one [`KnowledgeIndexEvent`] row here **inside the same transaction**.
//! Indexer workers later claim unprocessed rows, update their derived index
//! (Tantivy, the vector index, …), and stamp `processed_at`. Because the derived
//! indexes are never written in the authoritative transaction, an indexer crash
//! can never corrupt the source rows — and deleting the indexes and replaying
//! the outbox is exactly `codypendent index rebuild`.

use chrono::{DateTime, Utc};
use codypendent_protocol::{ArtifactId, CodeNodeId, DocumentId, MemoryId, RegistryItemId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A change to an authoritative entity that derived indexes must react to
/// (Chapter 06 `KnowledgeIndexEvent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KnowledgeIndexEvent {
    RegistryItemChanged(RegistryItemId),
    MemoryChanged(MemoryId),
    SymbolChanged(CodeNodeId),
    DocumentChanged(DocumentId),
    ArtifactCreated(ArtifactId),
}

impl KnowledgeIndexEvent {
    /// The scalar `event_kind` string stored in the row.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            KnowledgeIndexEvent::RegistryItemChanged(_) => "registry_item_changed",
            KnowledgeIndexEvent::MemoryChanged(_) => "memory_changed",
            KnowledgeIndexEvent::SymbolChanged(_) => "symbol_changed",
            KnowledgeIndexEvent::DocumentChanged(_) => "document_changed",
            KnowledgeIndexEvent::ArtifactCreated(_) => "artifact_created",
        }
    }

    /// The affected entity's id, as a string.
    #[must_use]
    pub fn entity_id(&self) -> String {
        match self {
            KnowledgeIndexEvent::RegistryItemChanged(id) => id.to_string(),
            KnowledgeIndexEvent::MemoryChanged(id) => id.to_string(),
            KnowledgeIndexEvent::SymbolChanged(id) => id.to_string(),
            KnowledgeIndexEvent::DocumentChanged(id) => id.to_string(),
            KnowledgeIndexEvent::ArtifactCreated(id) => id.to_string(),
        }
    }
}

/// Append an outbox row within the caller's executor — pass the **same
/// transaction** as the authoritative write so the pair is atomic.
pub async fn enqueue(
    executor: impl sqlx::SqliteExecutor<'_>,
    event: &KnowledgeIndexEvent,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO index_outbox (id, event_kind, entity_id, created_at, processed_at) \
         VALUES (?, ?, ?, ?, NULL)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(event.kind())
    .bind(event.entity_id())
    .bind(now.to_rfc3339())
    .execute(executor)
    .await?;
    Ok(())
}

/// One unprocessed outbox row, claimed by an indexer worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub id: String,
    pub event_kind: String,
    pub entity_id: String,
    pub created_at: String,
}

/// Read the oldest unprocessed rows (`processed_at IS NULL`), up to `limit`.
pub async fn unprocessed(
    pool: &sqlx::SqlitePool,
    limit: i64,
) -> Result<Vec<OutboxRow>, sqlx::Error> {
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT id, event_kind, entity_id, created_at FROM index_outbox \
         WHERE processed_at IS NULL ORDER BY created_at ASC, id ASC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, event_kind, entity_id, created_at)| OutboxRow {
            id,
            event_kind,
            entity_id,
            created_at,
        })
        .collect())
}

/// Stamp a row processed, so it is not re-claimed.
pub async fn mark_processed(
    pool: &sqlx::SqlitePool,
    id: &str,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE index_outbox SET processed_at = ? WHERE id = ?")
        .bind(now.to_rfc3339())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Reset every outbox row to unprocessed — the first move of `index rebuild`,
/// which then deletes the derived indexes and lets the workers replay authority.
pub async fn reset_all(pool: &sqlx::SqlitePool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("UPDATE index_outbox SET processed_at = NULL")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
