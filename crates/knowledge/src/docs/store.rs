//! Document persistence (STEP 4.2) — the `documents` table and its attribution
//! log, mirroring the fabric's house conventions ([`crate::memory`],
//! [`crate::codegraph`]): a stateless struct whose methods take `pool`, JSON/BLOB
//! columns, and every authoritative write appending an index-outbox row in the
//! **same transaction** so the write and its `DocumentChanged` event are atomic.
//!
//! The Loro CRDT snapshot is authoritative for the draft (ADR-016). It is stored
//! inline as the `crdt_snapshot` BLOB — the draft's durable home — and the
//! block-structured read model ([`KnowledgeDocument`]) is projected out of it, so
//! what is persisted and what is edited never drift.

use chrono::Utc;
use codypendent_protocol::DocumentId;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::outbox::{self, KnowledgeIndexEvent};
use crate::types::Scope;

use super::crdt::{DocCrdtError, DocumentCrdt};
use super::model::{
    AuthorshipRecord, Citation, DocumentAuthor, DocumentBlock, DocumentLink, DocumentMetadata,
    DocumentStatus, KnowledgeDocument, MutationKind,
};

/// Errors from the document store.
#[derive(Debug, thiserror::Error)]
pub enum DocStoreError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    #[error(transparent)]
    Crdt(#[from] DocCrdtError),
    /// A stored row could not be decoded (should never happen; the store wrote it).
    #[error("corrupt document row: {0}")]
    Corrupt(String),
    #[error("no such document: {0}")]
    NoSuchDocument(DocumentId),
    /// The document was modified since this replica loaded it — an optimistic
    /// concurrency conflict. The caller should reload (merging the CRDT
    /// snapshots) and retry rather than silently clobber the other writer.
    #[error("stale document revision for {id}: expected {expected}")]
    StaleRevision { id: DocumentId, expected: u64 },
    /// A suggestion is no longer pending (already accepted/rejected) — guards
    /// against a retried or concurrent accept re-applying its range.
    #[error("suggestion {0} is not pending")]
    SuggestionNotPending(String),
    /// A suggestion's target range no longer covers the text the proposer saw —
    /// the block was edited between propose and accept, so applying the stored
    /// offsets would corrupt the wrong characters. The proposer must re-propose.
    #[error("suggestion {0} no longer matches the block text (range drifted)")]
    SuggestionRangeDrifted(String),
}

/// A new document to create.
#[derive(Debug, Clone)]
pub struct NewDocument {
    pub title: String,
    pub scope: Scope,
    pub metadata: DocumentMetadata,
    pub blocks: Vec<DocumentBlock>,
}

/// A live, persisted document: its row fields plus the authoritative CRDT handle.
/// Edit via the [`crdt`](Document::crdt) methods, then [`DocumentStore::save`].
pub struct Document {
    pub id: DocumentId,
    pub title: String,
    pub scope: Scope,
    pub status: DocumentStatus,
    pub metadata: DocumentMetadata,
    pub links: Vec<DocumentLink>,
    pub citations: Vec<Citation>,
    pub revision: u64,
    pub crdt: DocumentCrdt,
}

impl Document {
    /// The current block projection of the CRDT.
    pub fn blocks(&self) -> Result<Vec<DocumentBlock>, DocStoreError> {
        Ok(self.crdt.to_blocks()?)
    }
}

/// The `documents`/`document_authorship` store. Stateless; the pool is passed to
/// each method (mirrors [`crate::memory::MemoryStore`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct DocumentStore;

impl DocumentStore {
    /// A document-store handle.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Create a document at revision 1 from `new.blocks`, attributing the creation
    /// to `author`. Inserts the row, its first authorship record, and a
    /// `DocumentChanged` outbox row — all in one transaction.
    pub async fn create(
        &self,
        pool: &SqlitePool,
        new: NewDocument,
        author: &DocumentAuthor,
    ) -> Result<Document, DocStoreError> {
        let id = DocumentId::new();
        let crdt = DocumentCrdt::from_blocks(&new.blocks)?;
        let snapshot = crdt.snapshot()?;
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let revision: u64 = 1;

        let scope_json = serde_json::to_string(&new.scope)?;
        let metadata_json = serde_json::to_string(&new.metadata)?;

        let mut tx = pool.begin().await?;
        sqlx::query(
            "INSERT INTO documents \
             (id, title, scope_json, scope_tier, scope_key, status, metadata_json, \
              crdt_snapshot, links_json, citations_json, revision, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(&new.title)
        .bind(&scope_json)
        .bind(new.scope.tier())
        .bind(new.scope.key())
        .bind(DocumentStatus::Draft.as_str())
        .bind(&metadata_json)
        .bind(&snapshot)
        .bind("[]")
        .bind("[]")
        .bind(revision as i64)
        .bind(&now_str)
        .bind(&now_str)
        .execute(&mut *tx)
        .await?;

        insert_authorship(
            &mut tx,
            id,
            None,
            author,
            MutationKind::InsertBlock,
            revision,
            &now_str,
        )
        .await?;
        outbox::enqueue(&mut *tx, &KnowledgeIndexEvent::DocumentChanged(id), now).await?;
        tx.commit().await?;

        Ok(Document {
            id,
            title: new.title,
            scope: new.scope,
            status: DocumentStatus::Draft,
            metadata: new.metadata,
            links: Vec::new(),
            citations: Vec::new(),
            revision,
            crdt,
        })
    }

    /// Load a document (its row + reconstructed CRDT). `None` if absent.
    pub async fn load(
        &self,
        pool: &SqlitePool,
        id: DocumentId,
    ) -> Result<Option<Document>, DocStoreError> {
        let Some(row) = sqlx::query(
            "SELECT title, scope_json, status, metadata_json, crdt_snapshot, links_json, \
             citations_json, revision FROM documents WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?
        else {
            return Ok(None);
        };

        let scope: Scope = serde_json::from_str(row.get::<String, _>("scope_json").as_str())?;
        let status: DocumentStatus = parse_status(row.get::<String, _>("status").as_str())?;
        let metadata: DocumentMetadata =
            serde_json::from_str(row.get::<String, _>("metadata_json").as_str())?;
        let links: Vec<DocumentLink> =
            serde_json::from_str(row.get::<String, _>("links_json").as_str())?;
        let citations: Vec<Citation> =
            serde_json::from_str(row.get::<String, _>("citations_json").as_str())?;
        let snapshot: Vec<u8> = row.get("crdt_snapshot");
        let revision = row.get::<i64, _>("revision") as u64;
        let crdt = DocumentCrdt::from_snapshot(&snapshot)?;

        Ok(Some(Document {
            id,
            title: row.get("title"),
            scope,
            status,
            metadata,
            links,
            citations,
            revision,
            crdt,
        }))
    }

    /// Persist the current CRDT state of `doc`, bumping the revision and recording
    /// one authorship entry for `(mutation, block_id)` attributed to `author`.
    /// Snapshot write, authorship, and the `DocumentChanged` outbox row land in
    /// one transaction. Returns and updates the new revision on `doc`.
    ///
    /// **Optimistic concurrency:** the write is guarded on `doc.revision`, so if
    /// another replica advanced the document since this one loaded it, the save
    /// fails with [`DocStoreError::StaleRevision`] instead of silently
    /// overwriting the other writer's content (the lost-update anomaly).
    pub async fn save(
        &self,
        pool: &SqlitePool,
        doc: &mut Document,
        author: &DocumentAuthor,
        mutation: MutationKind,
        block_id: Option<&str>,
    ) -> Result<u64, DocStoreError> {
        let mut tx = pool.begin().await?;
        let revision =
            write_document_tx(&mut tx, doc, author, mutation, block_id, Utc::now()).await?;
        tx.commit().await?;
        doc.revision = revision;
        Ok(revision)
    }

    /// Replace a document's knowledge-graph links (e.g. after resolving
    /// `{{ symbol:… }}` references against the code graph, STEP 4.6). This is a
    /// metadata update — it does **not** bump the document revision or record an
    /// authorship entry, since it changes no content. Updates the in-memory `doc`
    /// too.
    ///
    /// **Guarded on `doc.revision`:** link resolution runs from a document
    /// snapshot, so if the document was edited concurrently (its markers may have
    /// moved, appeared, or disappeared), this stale write fails with
    /// [`DocStoreError::StaleRevision`] rather than persisting links for a version
    /// that no longer exists. The caller reloads and re-resolves.
    pub async fn set_links(
        &self,
        pool: &SqlitePool,
        doc: &mut Document,
        links: Vec<DocumentLink>,
    ) -> Result<(), DocStoreError> {
        let links_json = serde_json::to_string(&links)?;
        let mut tx = pool.begin().await?;
        let affected = sqlx::query(
            "UPDATE documents SET links_json = ?, updated_at = ? WHERE id = ? AND revision = ?",
        )
        .bind(&links_json)
        .bind(Utc::now().to_rfc3339())
        .bind(doc.id.to_string())
        .bind(doc.revision as i64)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if affected == 0 {
            let exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM documents WHERE id = ?")
                .bind(doc.id.to_string())
                .fetch_optional(&mut *tx)
                .await?;
            return Err(if exists.is_some() {
                DocStoreError::StaleRevision {
                    id: doc.id,
                    expected: doc.revision,
                }
            } else {
                DocStoreError::NoSuchDocument(doc.id)
            });
        }
        // Replacing links is an index-relevant change (stale-link detection,
        // link-backed indexes), so notify subscribers/index workers — in the same
        // transaction as the write — just like content mutations do.
        outbox::enqueue(
            &mut *tx,
            &KnowledgeIndexEvent::DocumentChanged(doc.id),
            Utc::now(),
        )
        .await?;
        tx.commit().await?;
        doc.links = links;
        Ok(())
    }

    /// The attribution log for a document, oldest first.
    pub async fn authorship(
        &self,
        pool: &SqlitePool,
        id: DocumentId,
    ) -> Result<Vec<AuthorshipRecord>, DocStoreError> {
        let rows = sqlx::query(
            "SELECT block_id, author_json, mutation, revision, at FROM document_authorship \
             WHERE document_id = ? ORDER BY at ASC, id ASC",
        )
        .bind(id.to_string())
        .fetch_all(pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(AuthorshipRecord {
                    author: serde_json::from_str(row.get::<String, _>("author_json").as_str())?,
                    block_id: row.get("block_id"),
                    mutation: parse_mutation(row.get::<String, _>("mutation").as_str())?,
                    revision: row.get::<i64, _>("revision") as u64,
                    at: chrono::DateTime::parse_from_rfc3339(row.get::<String, _>("at").as_str())
                        .map_err(|e| DocStoreError::Corrupt(e.to_string()))?
                        .with_timezone(&Utc),
                })
            })
            .collect()
    }

    /// Assemble the full [`KnowledgeDocument`] read model (row + blocks +
    /// authorship). Used for inspection, export, and indexing.
    pub async fn snapshot_document(
        &self,
        pool: &SqlitePool,
        id: DocumentId,
    ) -> Result<Option<KnowledgeDocument>, DocStoreError> {
        let Some(doc) = self.load(pool, id).await? else {
            return Ok(None);
        };
        let authorship = self.authorship(pool, id).await?;
        let blocks = doc.blocks()?;
        Ok(Some(KnowledgeDocument {
            id: doc.id,
            title: doc.title,
            scope: doc.scope,
            status: doc.status,
            metadata: doc.metadata,
            blocks,
            links: doc.links,
            citations: doc.citations,
            authorship,
            revision: doc.revision,
        }))
    }

    /// List documents visible in any of `scopes`, newest first. Enforces
    /// cross-scope isolation in SQL (an empty slice matches nothing).
    pub async fn list(
        &self,
        pool: &SqlitePool,
        scopes: &[Scope],
    ) -> Result<Vec<DocumentSummary>, DocStoreError> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = String::from("SELECT id, title, status, revision FROM documents WHERE (");
        for (i, scope) in scopes.iter().enumerate() {
            if i > 0 {
                sql.push_str(" OR ");
            }
            match scope.key() {
                Some(_) => sql.push_str("(scope_tier = ? AND scope_key = ?)"),
                None => sql.push_str("(scope_tier = ? AND scope_key IS NULL)"),
            }
        }
        sql.push_str(") ORDER BY created_at DESC, id DESC");

        let mut q = sqlx::query(&sql);
        for scope in scopes {
            q = q.bind(scope.tier());
            if let Some(key) = scope.key() {
                q = q.bind(key);
            }
        }
        let rows = q.fetch_all(pool).await?;
        rows.iter()
            .map(|row| {
                Ok(DocumentSummary {
                    id: row
                        .get::<String, _>("id")
                        .parse()
                        .map_err(|e: uuid::Error| DocStoreError::Corrupt(e.to_string()))?,
                    title: row.get("title"),
                    status: parse_status(row.get::<String, _>("status").as_str())?,
                    revision: row.get::<i64, _>("revision") as u64,
                })
            })
            .collect()
    }
}

/// A compact document listing entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSummary {
    pub id: DocumentId,
    pub title: String,
    pub status: DocumentStatus,
    pub revision: u64,
}

/// Write `doc`'s current CRDT snapshot inside the caller's transaction, guarded
/// on the document's loaded revision (optimistic concurrency), and record the
/// authorship + `DocumentChanged` outbox row. Returns the new revision.
///
/// Shared by [`DocumentStore::save`] and the suggestion-accept flow so the
/// document write and a suggestion's resolution can commit atomically in one
/// transaction. Does **not** mutate `doc.revision` — the caller does that only
/// after the transaction commits.
///
/// **Does not write `links_json`.** Knowledge-graph links are machine-resolved
/// against the code graph and owned exclusively by [`DocumentStore::set_links`]
/// (STEP 4.6). If a content save also wrote its in-memory `doc.links`, an editor
/// who loaded before a concurrent resolver ran would silently overwrite the
/// resolved links; keeping links out of the content-save path prevents that.
pub(crate) async fn write_document_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    doc: &Document,
    author: &DocumentAuthor,
    mutation: MutationKind,
    block_id: Option<&str>,
    now: chrono::DateTime<Utc>,
) -> Result<u64, DocStoreError> {
    let snapshot = doc.crdt.snapshot()?;
    let now_str = now.to_rfc3339();
    let revision = doc.revision + 1;
    let citations_json = serde_json::to_string(&doc.citations)?;
    let metadata_json = serde_json::to_string(&doc.metadata)?;

    let affected = sqlx::query(
        "UPDATE documents SET crdt_snapshot = ?, status = ?, metadata_json = ?, \
         citations_json = ?, revision = ?, updated_at = ? \
         WHERE id = ? AND revision = ?",
    )
    .bind(&snapshot)
    .bind(doc.status.as_str())
    .bind(&metadata_json)
    .bind(&citations_json)
    .bind(revision as i64)
    .bind(&now_str)
    .bind(doc.id.to_string())
    .bind(doc.revision as i64)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if affected == 0 {
        // Nothing matched (id, revision): either the row is gone or another
        // writer advanced the revision. Distinguish the two.
        let exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM documents WHERE id = ?")
            .bind(doc.id.to_string())
            .fetch_optional(&mut **tx)
            .await?;
        return Err(if exists.is_some() {
            DocStoreError::StaleRevision {
                id: doc.id,
                expected: doc.revision,
            }
        } else {
            DocStoreError::NoSuchDocument(doc.id)
        });
    }

    insert_authorship(tx, doc.id, block_id, author, mutation, revision, &now_str).await?;
    outbox::enqueue(
        &mut **tx,
        &KnowledgeIndexEvent::DocumentChanged(doc.id),
        now,
    )
    .await?;
    Ok(revision)
}

/// Insert one authorship row inside the caller's transaction.
pub(crate) async fn insert_authorship(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    document_id: DocumentId,
    block_id: Option<&str>,
    author: &DocumentAuthor,
    mutation: MutationKind,
    revision: u64,
    at: &str,
) -> Result<(), DocStoreError> {
    sqlx::query(
        "INSERT INTO document_authorship \
         (id, document_id, block_id, author_json, mutation, revision, at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(document_id.to_string())
    .bind(block_id)
    .bind(serde_json::to_string(author)?)
    .bind(mutation.as_str())
    .bind(revision as i64)
    .bind(at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Decode a `DocumentStatus` scalar.
fn parse_status(s: &str) -> Result<DocumentStatus, DocStoreError> {
    Ok(serde_json::from_value(serde_json::Value::String(
        s.to_owned(),
    ))?)
}

/// Decode a `MutationKind` scalar.
fn parse_mutation(s: &str) -> Result<MutationKind, DocStoreError> {
    Ok(serde_json::from_value(serde_json::Value::String(
        s.to_owned(),
    ))?)
}
