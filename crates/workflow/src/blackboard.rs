//! The workflow blackboard (STEP 5.3): typed, attributed artifacts agents share
//! within a workflow run.
//!
//! Agents in a multi-agent workflow communicate **only** via blackboard artifacts
//! and declared node outputs — never by exchanging raw transcripts (Chapter 04).
//! Each item is a typed artifact ([`BlackboardKind`]) carrying its author,
//! confidence, and evidence, scoped to one workflow run, with a revision and a
//! supersession pointer so a corrected finding replaces — never silently
//! deletes — the one it supersedes.
//!
//! Payload, author, and evidence ride as opaque JSON so this crate stays
//! decoupled from the protocol/knowledge domain types (the daemon supplies typed
//! values); what this store owns is the *discipline*: evidence-required kinds,
//! per-run isolation, and supersession chains. Reads are
//! [`query`](BlackboardStore::query) (the live or full board, filtered by kind),
//! [`get`](BlackboardStore::get) (one item by id), and
//! [`history`](BlackboardStore::history) (an artifact's full revision lineage,
//! oldest first) — the surface the daemon's read command projects.

use chrono::Utc;
use serde_json::Value;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

/// An error from the blackboard store.
#[derive(Debug, thiserror::Error)]
pub enum BlackboardError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    /// A claim-like artifact was posted without evidence.
    #[error("a {0} must carry at least one evidence reference")]
    EvidenceRequired(&'static str),
    /// The item to supersede does not exist in this workflow run.
    #[error("no such blackboard item: {0}")]
    NotFound(String),
    /// The item to supersede has already been superseded — a concurrent supersede
    /// won the race, so this one is refused rather than forking the chain.
    #[error("blackboard item {0} has already been superseded")]
    AlreadySuperseded(String),
}

/// The typed artifacts the blackboard holds (Chapter 04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlackboardKind {
    Finding,
    Hypothesis,
    Decision,
    CodeLocation,
    ProposedPatch,
    TestResult,
    DocumentDraft,
    OpenQuestion,
}

impl BlackboardKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BlackboardKind::Finding => "finding",
            BlackboardKind::Hypothesis => "hypothesis",
            BlackboardKind::Decision => "decision",
            BlackboardKind::CodeLocation => "code_location",
            BlackboardKind::ProposedPatch => "proposed_patch",
            BlackboardKind::TestResult => "test_result",
            BlackboardKind::DocumentDraft => "document_draft",
            BlackboardKind::OpenQuestion => "open_question",
        }
    }

    /// Whether an artifact of this kind must carry evidence. Claim-like artifacts
    /// (a finding, a decision, a test result, a proposed patch, a located symbol)
    /// need grounding; a hypothesis, a draft, or an open question do not.
    #[must_use]
    pub fn requires_evidence(self) -> bool {
        matches!(
            self,
            BlackboardKind::Finding
                | BlackboardKind::Decision
                | BlackboardKind::TestResult
                | BlackboardKind::ProposedPatch
                | BlackboardKind::CodeLocation
        )
    }

    /// Parse a manifest-facing kind string (the inverse of
    /// [`as_str`](Self::as_str)); `None` for an unknown kind. The workflow
    /// compiler validates declared step `outputs` against this.
    #[must_use]
    pub fn parse_kind(s: &str) -> Option<Self> {
        Some(match s {
            "finding" => BlackboardKind::Finding,
            "hypothesis" => BlackboardKind::Hypothesis,
            "decision" => BlackboardKind::Decision,
            "code_location" => BlackboardKind::CodeLocation,
            "proposed_patch" => BlackboardKind::ProposedPatch,
            "test_result" => BlackboardKind::TestResult,
            "document_draft" => BlackboardKind::DocumentDraft,
            "open_question" => BlackboardKind::OpenQuestion,
            _ => return None,
        })
    }

    fn parse(s: &str) -> Result<Self, BlackboardError> {
        Self::parse_kind(s).ok_or_else(|| BlackboardError::NotFound(format!("kind {s}")))
    }
}

/// An artifact to post (before it gets an id / revision).
#[derive(Debug, Clone)]
pub struct NewBlackboardItem {
    pub kind: BlackboardKind,
    /// The artifact body (opaque to this crate).
    pub payload: Value,
    /// Who produced it — an agent/run/task (opaque to this crate).
    pub author: Value,
    pub confidence: Option<f64>,
    /// Evidence references (opaque). Required for claim-like kinds.
    pub evidence: Vec<Value>,
}

/// A stored blackboard artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct BlackboardItem {
    pub id: String,
    pub kind: BlackboardKind,
    pub payload: Value,
    pub author: Value,
    pub confidence: Option<f64>,
    pub evidence: Vec<Value>,
    pub revision: u32,
    /// The id of the item that superseded this one, if any.
    pub superseded_by: Option<String>,
}

/// Every column [`row_to_item`] decodes, in a fixed order shared by the SELECT
/// statements.
const ITEM_COLUMNS: &str =
    "id, kind, payload_json, author_json, confidence, evidence_json, revision, superseded_by";

/// The `blackboard_items` store, scoped to a workflow run.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlackboardStore;

impl BlackboardStore {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Post an artifact to a workflow run's blackboard. Refuses a claim-like kind
    /// with no evidence ([`BlackboardError::EvidenceRequired`]).
    pub async fn post(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        new: NewBlackboardItem,
    ) -> Result<BlackboardItem, BlackboardError> {
        if new.kind.requires_evidence() && new.evidence.is_empty() {
            return Err(BlackboardError::EvidenceRequired(new.kind.as_str()));
        }
        let id = Uuid::now_v7().to_string();
        insert_item(pool, workflow_run_id, &id, &new, 1).await?;
        Ok(BlackboardItem {
            id,
            kind: new.kind,
            payload: new.payload,
            author: new.author,
            confidence: new.confidence,
            evidence: new.evidence,
            revision: 1,
            superseded_by: None,
        })
    }

    /// Supersede `old_id` with a new artifact: the replacement is posted at the
    /// next revision and the old item is stamped `superseded_by` the new id — both
    /// in one transaction, so the chain is never torn. Returns the new item.
    pub async fn supersede(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        old_id: &str,
        new: NewBlackboardItem,
    ) -> Result<BlackboardItem, BlackboardError> {
        if new.kind.requires_evidence() && new.evidence.is_empty() {
            return Err(BlackboardError::EvidenceRequired(new.kind.as_str()));
        }
        // Read the old item's revision + supersession state, insert the
        // replacement, and stamp the old row — all in ONE immediate (write-locked)
        // transaction. A second concurrent supersede of the same item blocks at
        // `begin`, then reads `superseded_by` already set and is refused, so the
        // chain can never fork into two live replacements at the same revision.
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let old: Option<(i64, Option<String>)> = sqlx::query_as(
            "SELECT revision, superseded_by FROM blackboard_items \
             WHERE id = ? AND workflow_run_id = ?",
        )
        .bind(old_id)
        .bind(workflow_run_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (old_revision, superseded_by) =
            old.ok_or_else(|| BlackboardError::NotFound(old_id.to_owned()))?;
        if superseded_by.is_some() {
            return Err(BlackboardError::AlreadySuperseded(old_id.to_owned()));
        }

        let new_id = Uuid::now_v7().to_string();
        let revision = old_revision as u32 + 1;
        insert_item_tx(&mut *tx, workflow_run_id, &new_id, &new, revision).await?;
        // Stamp only while still un-superseded; a 0-row result means another
        // supersede slipped in, so abort rather than orphan our replacement.
        let affected = sqlx::query(
            "UPDATE blackboard_items SET superseded_by = ? WHERE id = ? AND superseded_by IS NULL",
        )
        .bind(&new_id)
        .bind(old_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if affected != 1 {
            tx.rollback().await?;
            return Err(BlackboardError::AlreadySuperseded(old_id.to_owned()));
        }
        tx.commit().await?;

        Ok(BlackboardItem {
            id: new_id,
            kind: new.kind,
            payload: new.payload,
            author: new.author,
            confidence: new.confidence,
            evidence: new.evidence,
            revision,
            superseded_by: None,
        })
    }

    /// Query a workflow run's blackboard. Optionally filter by `kind`; superseded
    /// items are excluded unless `include_superseded` is set. Newest first.
    pub async fn query(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        kind: Option<BlackboardKind>,
        include_superseded: bool,
    ) -> Result<Vec<BlackboardItem>, BlackboardError> {
        let mut sql =
            format!("SELECT {ITEM_COLUMNS} FROM blackboard_items WHERE workflow_run_id = ?");
        if !include_superseded {
            sql.push_str(" AND superseded_by IS NULL");
        }
        if kind.is_some() {
            sql.push_str(" AND kind = ?");
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC");

        let mut q = sqlx::query(&sql).bind(workflow_run_id);
        if let Some(kind) = kind {
            q = q.bind(kind.as_str());
        }
        let rows = q.fetch_all(pool).await?;
        rows.into_iter().map(row_to_item).collect()
    }

    /// Fetch one artifact by id within a run, or `None` if the run holds no such
    /// item. (A run scope is required so an id from another run's board is never
    /// returned.)
    pub async fn get(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        id: &str,
    ) -> Result<Option<BlackboardItem>, BlackboardError> {
        let row = sqlx::query(&format!(
            "SELECT {ITEM_COLUMNS} FROM blackboard_items WHERE id = ? AND workflow_run_id = ?"
        ))
        .bind(id)
        .bind(workflow_run_id)
        .fetch_optional(pool)
        .await?;
        row.map(row_to_item).transpose()
    }

    /// The full supersession chain the artifact `id` belongs to, oldest revision
    /// first. A correction *supersedes* rather than deletes, so the lineage is
    /// preserved; this walks it in both directions from `id` — back to the original
    /// and forward to the current live item — and returns every revision in order.
    /// The last element is the live item (its `superseded_by` is `None`) unless the
    /// chain is mid-write. An `id` absent from the run yields an empty vector (the
    /// chain always contains at least its anchor, so empty ⇒ not found).
    pub async fn history(
        &self,
        pool: &SqlitePool,
        workflow_run_id: &str,
        id: &str,
    ) -> Result<Vec<BlackboardItem>, BlackboardError> {
        let Some(anchor) = self.get(pool, workflow_run_id, id).await? else {
            return Ok(Vec::new());
        };

        let mut chain = vec![anchor.clone()];

        // Walk backward: each step finds the single predecessor whose
        // `superseded_by` points at the current item (supersede stamps exactly one
        // old row, so the chain is linear and this terminates at the original).
        let mut cursor = anchor.id.clone();
        while let Some(row) = sqlx::query(&format!(
            "SELECT {ITEM_COLUMNS} FROM blackboard_items \
             WHERE workflow_run_id = ? AND superseded_by = ?"
        ))
        .bind(workflow_run_id)
        .bind(&cursor)
        .fetch_optional(pool)
        .await?
        {
            let item = row_to_item(row)?;
            cursor = item.id.clone();
            chain.push(item);
        }

        // Walk forward: follow `superseded_by` to the live head.
        let mut next = anchor.superseded_by.clone();
        while let Some(next_id) = next {
            let Some(item) = self.get(pool, workflow_run_id, &next_id).await? else {
                break;
            };
            next = item.superseded_by.clone();
            chain.push(item);
        }

        // Revisions are monotonic within a chain, so this yields oldest → newest.
        chain.sort_by_key(|item| item.revision);
        Ok(chain)
    }
}

async fn insert_item(
    pool: &SqlitePool,
    workflow_run_id: &str,
    id: &str,
    new: &NewBlackboardItem,
    revision: u32,
) -> Result<(), BlackboardError> {
    let mut conn = pool.acquire().await?;
    insert_item_tx(&mut *conn, workflow_run_id, id, new, revision).await
}

async fn insert_item_tx<'e, E: sqlx::SqliteExecutor<'e>>(
    exec: E,
    workflow_run_id: &str,
    id: &str,
    new: &NewBlackboardItem,
    revision: u32,
) -> Result<(), BlackboardError> {
    sqlx::query(
        "INSERT INTO blackboard_items \
         (id, workflow_run_id, kind, payload_json, author_json, confidence, evidence_json, \
          revision, superseded_by, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, ?)",
    )
    .bind(id)
    .bind(workflow_run_id)
    .bind(new.kind.as_str())
    .bind(serde_json::to_string(&new.payload)?)
    .bind(serde_json::to_string(&new.author)?)
    .bind(new.confidence)
    .bind(serde_json::to_string(&new.evidence)?)
    .bind(i64::from(revision))
    .bind(Utc::now().to_rfc3339())
    .execute(exec)
    .await?;
    Ok(())
}

fn row_to_item(row: sqlx::sqlite::SqliteRow) -> Result<BlackboardItem, BlackboardError> {
    Ok(BlackboardItem {
        id: row.get("id"),
        kind: BlackboardKind::parse(&row.get::<String, _>("kind"))?,
        payload: serde_json::from_str(&row.get::<String, _>("payload_json"))?,
        author: serde_json::from_str(&row.get::<String, _>("author_json"))?,
        confidence: row.get("confidence"),
        evidence: serde_json::from_str(&row.get::<String, _>("evidence_json"))?,
        revision: row.get::<i64, _>("revision") as u32,
        superseded_by: row.get("superseded_by"),
    })
}
