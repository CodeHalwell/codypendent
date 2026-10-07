//! Bounded, curated learning that improves future runs without retaining the
//! transcript that produced it.
//!
//! This module deliberately separates compact factual memory from reusable
//! procedural knowledge. Every record is scoped, provenance-bearing,
//! confidence-rated, expirable, and reviewable. Capturing a candidate is not
//! equivalent to trusting it: externally controlled text, tool output, council
//! synthesis, and agent inference can only enter as [`LearningState::Proposed`].
//! Activation requires direct user authorship, an attested successful check, or
//! an explicit later [`Verification`].
//!
//! The ledger is additive to the legacy `memories` table. Existing rows and APIs
//! remain unchanged; callers can adopt this stricter lifecycle incrementally.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use codypendent_protocol::{LearningId, RepositoryId, SessionId, UserId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::memory::detect_secret;

/// Maximum non-rejected records retained in one scope. A bounded ledger keeps
/// context and review surfaces useful instead of turning them into transcripts.
pub const MAX_LEARNINGS_PER_SCOPE: i64 = 256;
/// Maximum bytes in one compact fact after whitespace normalization.
pub const MAX_FACT_BYTES: usize = 600;
/// Maximum serialized textual bytes in one reusable procedure.
pub const MAX_PROCEDURE_BYTES: usize = 12_000;
/// Maximum number of ordered steps in a procedure.
pub const MAX_PROCEDURE_STEPS: usize = 40;

const LEARNING_COLUMNS: &str = "id, kind, scope_kind, scope_key, content_json, \
    conflict_key, provenance_json, confidence, state, created_at, updated_at, verified_at, \
    expires_at, pinned, rejection_reason, revision";

/// A learned item's deliberately small domain: a fact or a reusable procedure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningKind {
    /// A compact preference, convention, decision, or validated environment fact.
    Fact,
    /// A reusable workflow with prerequisites, checks, and known pitfalls.
    Procedure,
}

/// Explicit visibility for learned material. Provider and council scopes are
/// strings because their stable identities come from adapter/profile catalogs,
/// not the repository UUID domain.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "scope", content = "key", rename_all = "snake_case")]
pub enum LearningScope {
    /// Preferences that follow one configured user.
    User(UserId),
    /// Conventions and facts isolated to one repository.
    Repository(RepositoryId),
    /// Adapter/model-provider quirks isolated to a provider id.
    Provider(String),
    /// Collaboration recipes isolated to a council/template id.
    Council(String),
}

impl LearningScope {
    fn kind(&self) -> &'static str {
        match self {
            Self::User(_) => "user",
            Self::Repository(_) => "repository",
            Self::Provider(_) => "provider",
            Self::Council(_) => "council",
        }
    }

    fn key(&self) -> String {
        match self {
            Self::User(id) => id.to_string(),
            Self::Repository(id) => id.to_string(),
            Self::Provider(id) | Self::Council(id) => id.clone(),
        }
    }
}

/// Structured procedural knowledge. Procedures are not disguised chat notes:
/// they carry the information needed to repeat and validate a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningProcedure {
    /// Short stable name shown in skill/learning pickers.
    pub name: String,
    /// What this procedure accomplishes and when it applies.
    pub summary: String,
    /// Ordered implementation or operational steps.
    pub steps: Vec<String>,
    /// Conditions or resources that must exist first.
    #[serde(default)]
    pub prerequisites: Vec<String>,
    /// Safe commands or checks proving the procedure succeeded.
    #[serde(default)]
    pub verification: Vec<String>,
    /// Known failure modes worth avoiding on a later run.
    #[serde(default)]
    pub pitfalls: Vec<String>,
}

/// Content contract for the two learning domains.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LearningContent {
    /// One compact factual statement, optionally with a machine-readable value.
    Fact {
        /// Human-readable fact or preference.
        statement: String,
        /// Optional typed value used for precise conflict detection.
        structured_value: Option<serde_json::Value>,
    },
    /// A reusable workflow that can later be promoted into a skill.
    Procedure(LearningProcedure),
}

impl LearningContent {
    /// The content's domain kind.
    #[must_use]
    pub fn kind(&self) -> LearningKind {
        match self {
            Self::Fact { .. } => LearningKind::Fact,
            Self::Procedure(_) => LearningKind::Procedure,
        }
    }

    /// A concise display string without provenance or transcript material.
    #[must_use]
    pub fn summary(&self) -> &str {
        match self {
            Self::Fact { statement, .. } => statement,
            Self::Procedure(procedure) => &procedure.summary,
        }
    }
}

/// Where a learning candidate came from. Source-controlled or network text is
/// explicitly untrusted even when it was delivered through a trusted tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum LearningProvenance {
    /// A user explicitly stated or edited this learning.
    UserStatement { user: UserId },
    /// A repository observation. File contents may be attacker-controlled, so
    /// this source proposes but never auto-activates a record.
    RepositoryObservation {
        repository: RepositoryId,
        source_path: Option<String>,
        revision: Option<String>,
    },
    /// A zero-exit verification performed by Codypendent itself.
    SuccessfulCommand {
        session: SessionId,
        command_summary: String,
    },
    /// A model-derived inference requiring human or execution verification.
    AgentInference { model: String },
    /// Raw or summarized output from a tool. Never an auto-activation authority.
    ToolOutput { tool: String },
    /// Web, MCP, document, or other externally controlled content.
    ExternalContent { source_uri: String },
    /// A synthesized council result, proposed for review rather than trusted.
    CouncilResult { council_id: String },
}

impl LearningProvenance {
    fn permits_auto_activation(&self) -> bool {
        matches!(
            self,
            Self::UserStatement { .. } | Self::SuccessfulCommand { .. }
        )
    }
}

/// Review lifecycle. Proposed records cannot be injected into future context;
/// rejected records remain visible for audit/dedupe until explicitly deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningState {
    /// Awaiting user or execution verification.
    Proposed,
    /// Verified and eligible for retrieval into future runs.
    Active,
    /// Declined or invalidated; never eligible for retrieval.
    Rejected,
}

/// Whether capture should merely propose or activate when its provenance is
/// strong enough. `ActivateIfTrusted` still fails closed to `Proposed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationIntent {
    /// Always create a reviewable proposal.
    Propose,
    /// Activate only with qualifying provenance and confidence.
    ActivateIfTrusted,
}

/// One governed fact or procedure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningRecord {
    /// Stable UUIDv7 identity.
    pub id: LearningId,
    /// Explicit learning scope.
    pub scope: LearningScope,
    /// Compact fact or reusable procedure.
    pub content: LearningContent,
    /// Optional normalized subject/name used to surface conflicts.
    pub conflict_key: Option<String>,
    /// One or more sources explaining why the item exists.
    pub provenance: Vec<LearningProvenance>,
    /// Calibrated confidence in `[0, 1]`.
    pub confidence: f32,
    /// Review/retrieval lifecycle.
    pub state: LearningState,
    /// First persisted time.
    pub created_at: DateTime<Utc>,
    /// Last mutation time.
    pub updated_at: DateTime<Utc>,
    /// Most recent explicit verification, if any.
    pub verified_at: Option<DateTime<Utc>>,
    /// Time after which the item is no longer eligible for active retrieval.
    pub expires_at: Option<DateTime<Utc>>,
    /// Pinned records are retained prominently but still obey expiry and state.
    pub pinned: bool,
    /// Human-readable reason attached to a rejected record.
    pub rejection_reason: Option<String>,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

impl LearningRecord {
    /// Whether this record may be injected into a run at `now`.
    #[must_use]
    pub fn is_retrievable(&self, now: DateTime<Utc>) -> bool {
        self.state == LearningState::Active
            && self.expires_at.as_ref().is_none_or(|expiry| expiry > &now)
    }
}

/// Candidate handed to the strict capture policy.
#[derive(Debug, Clone, PartialEq)]
pub struct NewLearning {
    /// Explicit target scope.
    pub scope: LearningScope,
    /// Fact or procedure content.
    pub content: LearningContent,
    /// Optional subject/name override used for conflict grouping.
    pub conflict_key: Option<String>,
    /// Source evidence. Empty provenance is rejected.
    pub provenance: Vec<LearningProvenance>,
    /// Confidence in `[0, 1]`.
    pub confidence: f32,
    /// Optional expiry for environment-dependent knowledge.
    pub expires_at: Option<DateTime<Utc>>,
    /// Requested activation behavior.
    pub activation: ActivationIntent,
}

/// A partial user/API edit. Nested `Option`s distinguish "leave unchanged"
/// from "clear the optional value".
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LearningPatch {
    /// Replacement content.
    pub content: Option<LearningContent>,
    /// Replacement conflict key; `Some(None)` clears it.
    pub conflict_key: Option<Option<String>>,
    /// Replacement confidence.
    pub confidence: Option<f32>,
    /// Replacement expiry; `Some(None)` clears it.
    pub expires_at: Option<Option<DateTime<Utc>>>,
}

/// Query controls. Empty filters mean all values for that dimension.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LearningQuery {
    /// Exact scopes visible to the caller.
    pub scopes: Vec<LearningScope>,
    /// Optional kind filter.
    pub kinds: Vec<LearningKind>,
    /// Optional state filter.
    pub states: Vec<LearningState>,
    /// Include records whose expiry is at or before now.
    pub include_expired: bool,
}

/// Result of policy, dedupe, and conflict evaluation during capture.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureOutcome {
    /// Stored without a conflict.
    Stored(LearningRecord),
    /// An equivalent live item already exists; no row was written.
    Duplicate { existing_id: LearningId },
    /// Stored as a proposal because another live item shares its subject/name.
    Conflict {
        record: LearningRecord,
        conflicts_with: Vec<LearningId>,
    },
    /// Rejected by the strict quality/secret policy; no content was persisted.
    PolicyRejected { reason: String },
}

/// Result of editing an existing learning.
#[derive(Debug, Clone, PartialEq)]
pub enum MutationOutcome {
    /// Mutation succeeded.
    Updated(LearningRecord),
    /// Mutation would duplicate another live item; no change was written.
    Duplicate { existing_id: LearningId },
    /// Mutation succeeded but was downgraded to proposed due to conflicts.
    Conflict {
        record: LearningRecord,
        conflicts_with: Vec<LearningId>,
    },
}

/// Trusted evidence supplied when explicitly activating a proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// A user reviewed and accepted the record.
    UserConfirmed { user: UserId },
    /// A successful targeted check validated the record.
    SuccessfulCheck {
        session: SessionId,
        command_summary: String,
    },
}

/// Explicit activation can still be blocked by a conflicting active record.
#[derive(Debug, Clone, PartialEq)]
pub enum ActivationOutcome {
    /// Record is now active and verified.
    Activated(Box<LearningRecord>),
    /// An active record with the same conflict key must be resolved first.
    Conflict { conflicts_with: Vec<LearningId> },
}

/// Content-free deletion audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedLearning {
    /// Deleted identity, or `None` when it did not exist.
    pub id: Option<LearningId>,
    /// Deletion time; never accompanied by the deleted content.
    pub deleted_at: DateTime<Utc>,
}

/// Unrecoverable learning-store failures. Policy outcomes remain typed values.
#[derive(Debug, thiserror::Error)]
pub enum LearningError {
    /// Requested record does not exist.
    #[error("learning {0} was not found")]
    NotFound(LearningId),
    /// Optimistic revision did not match the current row.
    #[error("learning {id} revision changed (expected {expected})")]
    RevisionConflict { id: LearningId, expected: u64 },
    /// A stored row violated its schema.
    #[error("corrupt learning row: {0}")]
    Corrupt(String),
    /// Scope has reached the bounded live-record cap.
    #[error("learning scope is full ({MAX_LEARNINGS_PER_SCOPE} live records)")]
    ScopeFull,
    /// A requested edit itself violated capture policy.
    #[error("learning policy rejected the mutation: {0}")]
    Policy(String),
    /// Database error.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    /// JSON serialization error.
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

/// Stateless governed learning ledger.
#[derive(Debug, Clone, Copy, Default)]
pub struct LearningStore;

impl LearningStore {
    /// Construct a store handle.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Apply secret, quality, bounds, trust, dedupe, and conflict gates, then
    /// persist a candidate. Untrusted provenance can never auto-activate.
    pub async fn capture(
        &self,
        pool: &SqlitePool,
        mut candidate: NewLearning,
    ) -> Result<CaptureOutcome, LearningError> {
        // Inspect the original shape first: normalization deliberately collapses
        // whitespace, which must not let a multi-line raw log evade the policy.
        if let Some(reason) = capture_policy_reason(&candidate) {
            return Ok(CaptureOutcome::PolicyRejected { reason });
        }
        normalize_content(&mut candidate.content);
        if let Some(reason) = capture_policy_reason(&candidate) {
            return Ok(CaptureOutcome::PolicyRejected { reason });
        }

        let now = Utc::now();
        let expires_at = candidate.expires_at;
        let hash = normalized_hash(&candidate.content)?;
        let conflict_key = candidate
            .conflict_key
            .as_deref()
            .and_then(|value| normalize_conflict_key(Some(value)))
            .or_else(|| inferred_conflict_key(&candidate.content));
        // BEGIN IMMEDIATE, not the deferred default: every path below READS (the
        // duplicate / conflict / count checks) and then WRITES. A deferred
        // transaction takes its read snapshot at the first SELECT, and in WAL mode
        // a writer that committed after that snapshot makes the later write fail
        // at once with SQLITE_BUSY_SNAPSHOT — `busy_timeout` cannot help, because
        // the snapshot is stale, not the lock held. Taking the write lock up front
        // serialises the callers instead. (The rest of the daemon already does
        // this for read-then-write; see `approvals.rs`.)
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

        if let Some(existing_id) = find_duplicate(
            &mut *tx,
            &candidate.scope,
            candidate.content.kind(),
            &hash,
            None,
        )
        .await?
        {
            tx.rollback().await?;
            return Ok(CaptureOutcome::Duplicate { existing_id });
        }

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM learning_records \
             WHERE scope_kind = ? AND scope_key = ? AND state != 'rejected' \
             AND (expires_at IS NULL OR expires_at > ?)",
        )
        .bind(candidate.scope.kind())
        .bind(candidate.scope.key())
        .bind(now.to_rfc3339())
        .fetch_one(&mut *tx)
        .await?;
        if count >= MAX_LEARNINGS_PER_SCOPE {
            tx.rollback().await?;
            return Err(LearningError::ScopeFull);
        }

        let conflicts = find_conflicts(
            &mut *tx,
            &candidate.scope,
            candidate.content.kind(),
            conflict_key.as_deref(),
            None,
            false,
        )
        .await?;
        let trusted = candidate
            .provenance
            .iter()
            .any(LearningProvenance::permits_auto_activation);
        let state = if conflicts.is_empty()
            && candidate.activation == ActivationIntent::ActivateIfTrusted
            && trusted
            && candidate.confidence >= 0.8
        {
            LearningState::Active
        } else {
            LearningState::Proposed
        };
        let record = LearningRecord {
            id: LearningId::new(),
            scope: candidate.scope,
            content: candidate.content,
            conflict_key,
            provenance: candidate.provenance,
            confidence: candidate.confidence,
            state,
            created_at: now,
            updated_at: now,
            verified_at: (state == LearningState::Active).then_some(now),
            expires_at,
            pinned: false,
            rejection_reason: None,
            revision: 1,
        };
        insert_record(&mut *tx, &record, &hash).await?;
        tx.commit().await?;

        if conflicts.is_empty() {
            Ok(CaptureOutcome::Stored(record))
        } else {
            Ok(CaptureOutcome::Conflict {
                record,
                conflicts_with: conflicts,
            })
        }
    }

    /// Fetch one record regardless of state or expiry.
    pub async fn get(
        &self,
        pool: &SqlitePool,
        id: LearningId,
    ) -> Result<Option<LearningRecord>, LearningError> {
        let row = sqlx::query(&format!(
            "SELECT {LEARNING_COLUMNS} FROM learning_records WHERE id = ?"
        ))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?;
        row.as_ref().map(record_from_row).transpose()
    }

    /// Query exactly the caller-visible scopes. Empty scopes fail closed and
    /// return no records. Expired records are excluded by default.
    pub async fn query(
        &self,
        pool: &SqlitePool,
        query: &LearningQuery,
    ) -> Result<Vec<LearningRecord>, LearningError> {
        if query.scopes.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = format!("SELECT {LEARNING_COLUMNS} FROM learning_records WHERE (");
        for (index, _) in query.scopes.iter().enumerate() {
            if index > 0 {
                sql.push_str(" OR ");
            }
            sql.push_str("(scope_kind = ? AND scope_key = ?)");
        }
        sql.push(')');
        if !query.kinds.is_empty() {
            sql.push_str(" AND kind IN (");
            sql.push_str(&vec!["?"; query.kinds.len()].join(", "));
            sql.push(')');
        }
        if !query.states.is_empty() {
            sql.push_str(" AND state IN (");
            sql.push_str(&vec!["?"; query.states.len()].join(", "));
            sql.push(')');
        }
        if !query.include_expired {
            sql.push_str(" AND (expires_at IS NULL OR expires_at > ?)");
        }
        sql.push_str(" ORDER BY pinned DESC, updated_at DESC, id DESC");

        let mut statement = sqlx::query(&sql);
        for scope in &query.scopes {
            statement = statement.bind(scope.kind()).bind(scope.key());
        }
        for kind in &query.kinds {
            statement = statement.bind(enum_to_db(kind)?);
        }
        for state in &query.states {
            statement = statement.bind(enum_to_db(state)?);
        }
        if !query.include_expired {
            statement = statement.bind(Utc::now().to_rfc3339());
        }
        statement
            .fetch_all(pool)
            .await?
            .iter()
            .map(record_from_row)
            .collect()
    }

    /// Edit a record using optimistic concurrency. Content edits are re-run
    /// through the strict policy and may downgrade an active record to proposed
    /// when they introduce a conflict.
    pub async fn edit(
        &self,
        pool: &SqlitePool,
        id: LearningId,
        expected_revision: u64,
        patch: LearningPatch,
    ) -> Result<MutationOutcome, LearningError> {
        let LearningPatch {
            content,
            conflict_key,
            confidence,
            expires_at,
        } = patch;
        let mut current = self
            .get(pool, id)
            .await?
            .ok_or(LearningError::NotFound(id))?;
        if current.revision != expected_revision {
            return Err(LearningError::RevisionConflict {
                id,
                expected: expected_revision,
            });
        }
        let content_changed = content.is_some();
        if let Some(mut replacement) = content {
            normalize_content(&mut replacement);
            let probe = NewLearning {
                scope: current.scope.clone(),
                content: replacement.clone(),
                conflict_key: None,
                provenance: current.provenance.clone(),
                confidence: confidence.unwrap_or(current.confidence),
                expires_at: expires_at.unwrap_or(current.expires_at),
                activation: ActivationIntent::Propose,
            };
            if let Some(reason) = capture_policy_reason(&probe) {
                return Err(LearningError::Policy(reason));
            }
            current.content = replacement;
        }
        if let Some(replacement) = conflict_key {
            current.conflict_key = normalize_conflict_key(replacement.as_deref());
        } else if content_changed {
            current.conflict_key = inferred_conflict_key(&current.content);
        }
        if let Some(replacement) = confidence {
            validate_confidence(replacement).map_err(LearningError::Policy)?;
            current.confidence = replacement;
        }
        if let Some(replacement) = expires_at {
            current.expires_at = replacement;
        }

        let hash = normalized_hash(&current.content)?;
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(existing_id) = find_duplicate(
            &mut *tx,
            &current.scope,
            current.content.kind(),
            &hash,
            Some(id),
        )
        .await?
        {
            tx.rollback().await?;
            return Ok(MutationOutcome::Duplicate { existing_id });
        }
        let conflicts = find_conflicts(
            &mut *tx,
            &current.scope,
            current.content.kind(),
            current.conflict_key.as_deref(),
            Some(id),
            false,
        )
        .await?;
        if !conflicts.is_empty() {
            current.state = LearningState::Proposed;
            current.verified_at = None;
        }
        current.updated_at = Utc::now();
        current.revision += 1;
        update_record(&mut *tx, &current, &hash, expected_revision).await?;
        tx.commit().await?;
        if conflicts.is_empty() {
            Ok(MutationOutcome::Updated(current))
        } else {
            Ok(MutationOutcome::Conflict {
                record: current,
                conflicts_with: conflicts,
            })
        }
    }

    /// Explicitly verify and activate a proposal. Conflicting active records are
    /// surfaced instead of being silently superseded.
    pub async fn activate(
        &self,
        pool: &SqlitePool,
        id: LearningId,
        expected_revision: u64,
        verification: Verification,
    ) -> Result<ActivationOutcome, LearningError> {
        let mut record = self
            .get(pool, id)
            .await?
            .ok_or(LearningError::NotFound(id))?;
        ensure_revision(&record, expected_revision)?;
        if record.expires_at.is_some_and(|expiry| expiry <= Utc::now()) {
            return Err(LearningError::Policy(
                "expired learning must be revalidated with a future expiry before activation"
                    .to_owned(),
            ));
        }
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let conflicts = find_conflicts(
            &mut *tx,
            &record.scope,
            record.content.kind(),
            record.conflict_key.as_deref(),
            Some(id),
            true,
        )
        .await?;
        if !conflicts.is_empty() {
            tx.rollback().await?;
            return Ok(ActivationOutcome::Conflict {
                conflicts_with: conflicts,
            });
        }
        record.provenance.push(match verification {
            Verification::UserConfirmed { user } => LearningProvenance::UserStatement { user },
            Verification::SuccessfulCheck {
                session,
                command_summary,
            } => LearningProvenance::SuccessfulCommand {
                session,
                command_summary,
            },
        });
        let now = Utc::now();
        record.state = LearningState::Active;
        record.updated_at = now;
        record.verified_at = Some(now);
        record.rejection_reason = None;
        record.revision += 1;
        let hash = normalized_hash(&record.content)?;
        update_record(&mut *tx, &record, &hash, expected_revision).await?;
        tx.commit().await?;
        Ok(ActivationOutcome::Activated(Box::new(record)))
    }

    /// Reject a proposal or active record with an explicit reason.
    pub async fn reject(
        &self,
        pool: &SqlitePool,
        id: LearningId,
        expected_revision: u64,
        reason: impl Into<String>,
    ) -> Result<LearningRecord, LearningError> {
        let mut record = self
            .get(pool, id)
            .await?
            .ok_or(LearningError::NotFound(id))?;
        ensure_revision(&record, expected_revision)?;
        let reason = normalize_text(&reason.into());
        if reason.is_empty() {
            return Err(LearningError::Policy(
                "rejection reason must not be empty".to_owned(),
            ));
        }
        record.state = LearningState::Rejected;
        record.rejection_reason = Some(reason);
        record.verified_at = None;
        record.updated_at = Utc::now();
        record.revision += 1;
        write_existing(pool, &record, expected_revision).await?;
        Ok(record)
    }

    /// Pin or unpin a record without altering its trust state.
    pub async fn set_pinned(
        &self,
        pool: &SqlitePool,
        id: LearningId,
        expected_revision: u64,
        pinned: bool,
    ) -> Result<LearningRecord, LearningError> {
        let mut record = self
            .get(pool, id)
            .await?
            .ok_or(LearningError::NotFound(id))?;
        ensure_revision(&record, expected_revision)?;
        record.pinned = pinned;
        record.updated_at = Utc::now();
        record.revision += 1;
        write_existing(pool, &record, expected_revision).await?;
        Ok(record)
    }

    /// Permanently delete one record, returning an audit that contains no
    /// deleted content.
    pub async fn delete(
        &self,
        pool: &SqlitePool,
        id: LearningId,
    ) -> Result<DeletedLearning, LearningError> {
        let result = sqlx::query("DELETE FROM learning_records WHERE id = ?")
            .bind(id.to_string())
            .execute(pool)
            .await?;
        Ok(DeletedLearning {
            id: (result.rows_affected() > 0).then_some(id),
            deleted_at: Utc::now(),
        })
    }
}

fn capture_policy_reason(candidate: &NewLearning) -> Option<String> {
    if candidate.provenance.is_empty() {
        return Some("provenance is required".to_owned());
    }
    if let Err(reason) = validate_scope(&candidate.scope) {
        return Some(reason);
    }
    if let Err(reason) = validate_confidence(candidate.confidence) {
        return Some(reason);
    }
    if candidate
        .expires_at
        .is_some_and(|expiry| expiry <= Utc::now())
    {
        return Some("expiry must be in the future".to_owned());
    }
    let texts = content_texts(&candidate.content);
    for text in &texts {
        if let Some(reason) = detect_secret(text) {
            return Some(format!("secret-bearing content: {reason}"));
        }
        if let Some(reason) = strict_text_rejection_reason(text) {
            return Some(reason);
        }
    }
    match &candidate.content {
        LearningContent::Fact {
            statement,
            structured_value,
        } => {
            if statement.len() > MAX_FACT_BYTES {
                return Some(format!("fact exceeds {MAX_FACT_BYTES} bytes"));
            }
            if let Some(value) = structured_value {
                let serialized = value.to_string();
                if let Some(reason) = detect_secret(&serialized) {
                    return Some(format!("secret-bearing structured value: {reason}"));
                }
            }
        }
        LearningContent::Procedure(procedure) => {
            if procedure.name.is_empty()
                || procedure.summary.is_empty()
                || procedure.steps.is_empty()
            {
                return Some(
                    "procedure requires a name, summary, and at least one step".to_owned(),
                );
            }
            if procedure.steps.len() > MAX_PROCEDURE_STEPS {
                return Some(format!(
                    "procedure exceeds {MAX_PROCEDURE_STEPS} ordered steps"
                ));
            }
            let bytes: usize = texts.iter().map(|text| text.len()).sum();
            if bytes > MAX_PROCEDURE_BYTES {
                return Some(format!("procedure exceeds {MAX_PROCEDURE_BYTES} bytes"));
            }
        }
    }
    None
}

fn validate_scope(scope: &LearningScope) -> Result<(), String> {
    match scope {
        LearningScope::Provider(key) | LearningScope::Council(key) if key.trim().is_empty() => {
            Err("provider/council scope key must not be empty".to_owned())
        }
        _ => Ok(()),
    }
}

fn validate_confidence(confidence: f32) -> Result<(), String> {
    if confidence.is_finite() && (0.0..=1.0).contains(&confidence) {
        Ok(())
    } else {
        Err("confidence must be finite and in [0, 1]".to_owned())
    }
}

fn content_texts(content: &LearningContent) -> Vec<&str> {
    match content {
        LearningContent::Fact { statement, .. } => vec![statement],
        LearningContent::Procedure(procedure) => std::iter::once(procedure.name.as_str())
            .chain(std::iter::once(procedure.summary.as_str()))
            .chain(procedure.steps.iter().map(String::as_str))
            .chain(procedure.prerequisites.iter().map(String::as_str))
            .chain(procedure.verification.iter().map(String::as_str))
            .chain(procedure.pitfalls.iter().map(String::as_str))
            .collect(),
    }
}

pub(crate) fn strict_text_rejection_reason(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    let greeting = [
        "hello",
        "hi",
        "hey",
        "good morning",
        "good afternoon",
        "what can i help you with",
        "what would you like to work on",
        "how can i help",
        "i'm ready to help",
        "i am ready to help",
    ]
    .iter()
    .any(|phrase| lower == *phrase || lower.starts_with(&format!("{phrase}!")));
    if greeting {
        return Some("greetings are not durable knowledge".to_owned());
    }
    let generic_completion = [
        "done",
        "completed",
        "task completed",
        "all done",
        "finished",
        "success",
        "successfully completed",
    ]
    .iter()
    .any(|phrase| lower.trim_matches(|ch: char| ch.is_ascii_punctuation()) == *phrase);
    if generic_completion {
        return Some("generic completion messages are not durable knowledge".to_owned());
    }
    if contains_temporary_path(&lower) {
        return Some("temporary paths are session context, not durable knowledge".to_owned());
    }
    if looks_like_raw_log(trimmed) {
        return Some("raw logs belong in artifacts, not durable learning".to_owned());
    }
    None
}

fn contains_temporary_path(lower: &str) -> bool {
    lower.contains("/tmp/")
        || lower.contains("/private/tmp/")
        || lower.contains("/var/folders/")
        || lower.contains("\\appdata\\local\\temp\\")
        || lower.contains("c:\\temp\\")
}

fn looks_like_raw_log(text: &str) -> bool {
    if text.contains('\u{1b}') {
        return true;
    }
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() < 3 {
        return false;
    }
    let log_lines = lines
        .iter()
        .filter(|line| {
            let line = line.trim_start();
            let upper = line.to_ascii_uppercase();
            upper.starts_with("DEBUG ")
                || upper.starts_with("INFO ")
                || upper.starts_with("WARN ")
                || upper.starts_with("ERROR ")
                || upper.starts_with("TRACE ")
                || line.starts_with("at ")
                || line.starts_with("thread '")
                || starts_with_timestamp(line)
        })
        .count();
    log_lines * 2 >= lines.len()
}

fn starts_with_timestamp(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.len() >= 10
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}

fn normalize_content(content: &mut LearningContent) {
    match content {
        LearningContent::Fact { statement, .. } => *statement = normalize_text(statement),
        LearningContent::Procedure(procedure) => {
            procedure.name = normalize_text(&procedure.name);
            procedure.summary = normalize_text(&procedure.summary);
            for collection in [
                &mut procedure.steps,
                &mut procedure.prerequisites,
                &mut procedure.verification,
                &mut procedure.pitfalls,
            ] {
                for item in collection.iter_mut() {
                    *item = normalize_text(item);
                }
                collection.retain(|item| !item.is_empty());
            }
        }
    }
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalized_hash(content: &LearningContent) -> Result<String, LearningError> {
    let canonical = match content {
        LearningContent::Fact {
            statement,
            structured_value,
        } => serde_json::json!({
            "kind": "fact",
            "statement": normalize_for_compare(statement),
            "structured_value": structured_value,
        }),
        LearningContent::Procedure(procedure) => serde_json::json!({
            "kind": "procedure",
            "name": normalize_for_compare(&procedure.name),
            "summary": normalize_for_compare(&procedure.summary),
            "steps": procedure.steps.iter().map(|value| normalize_for_compare(value)).collect::<Vec<_>>(),
            "prerequisites": procedure.prerequisites.iter().map(|value| normalize_for_compare(value)).collect::<Vec<_>>(),
            "verification": procedure.verification.iter().map(|value| normalize_for_compare(value)).collect::<Vec<_>>(),
            "pitfalls": procedure.pitfalls.iter().map(|value| normalize_for_compare(value)).collect::<Vec<_>>(),
        }),
    };
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&canonical)?)))
}

fn normalize_for_compare(value: &str) -> String {
    normalize_text(value).to_lowercase()
}

fn inferred_conflict_key(content: &LearningContent) -> Option<String> {
    match content {
        LearningContent::Procedure(procedure) => normalize_conflict_key(Some(&procedure.name)),
        LearningContent::Fact { statement, .. } => {
            let normalized = normalize_for_compare(statement);
            for separator in [" is ", ": ", " = ", "="] {
                if let Some(index) = normalized.find(separator) {
                    return normalize_conflict_key(Some(&normalized[..index]));
                }
            }
            None
        }
    }
}

fn normalize_conflict_key(value: Option<&str>) -> Option<String> {
    value
        .map(normalize_for_compare)
        .filter(|value| value.len() >= 3)
}

async fn find_duplicate(
    executor: impl sqlx::SqliteExecutor<'_>,
    scope: &LearningScope,
    kind: LearningKind,
    hash: &str,
    excluding: Option<LearningId>,
) -> Result<Option<LearningId>, LearningError> {
    let row: Option<String> = sqlx::query_scalar(
        "SELECT id FROM learning_records \
         WHERE scope_kind = ? AND scope_key = ? AND kind = ? AND normalized_hash = ? \
         AND state != 'rejected' AND (? IS NULL OR id != ?) LIMIT 1",
    )
    .bind(scope.kind())
    .bind(scope.key())
    .bind(enum_to_db(&kind)?)
    .bind(hash)
    .bind(excluding.map(|id| id.to_string()))
    .bind(excluding.map(|id| id.to_string()))
    .fetch_optional(executor)
    .await?;
    row.map(|id| parse_id(&id)).transpose()
}

async fn find_conflicts(
    executor: impl sqlx::SqliteExecutor<'_>,
    scope: &LearningScope,
    kind: LearningKind,
    conflict_key: Option<&str>,
    excluding: Option<LearningId>,
    active_only: bool,
) -> Result<Vec<LearningId>, LearningError> {
    let Some(conflict_key) = conflict_key else {
        return Ok(Vec::new());
    };
    let state_clause = if active_only {
        "state = 'active'"
    } else {
        "state != 'rejected'"
    };
    let sql = format!(
        "SELECT id FROM learning_records WHERE scope_kind = ? AND scope_key = ? \
         AND kind = ? AND conflict_key = ? AND {state_clause} \
         AND (? IS NULL OR id != ?) ORDER BY created_at ASC"
    );
    let ids: Vec<String> = sqlx::query_scalar(&sql)
        .bind(scope.kind())
        .bind(scope.key())
        .bind(enum_to_db(&kind)?)
        .bind(conflict_key)
        .bind(excluding.map(|id| id.to_string()))
        .bind(excluding.map(|id| id.to_string()))
        .fetch_all(executor)
        .await?;
    ids.iter().map(|id| parse_id(id)).collect()
}

async fn insert_record(
    executor: impl sqlx::SqliteExecutor<'_>,
    record: &LearningRecord,
    hash: &str,
) -> Result<(), LearningError> {
    sqlx::query(
        "INSERT INTO learning_records \
         (id, kind, scope_kind, scope_key, content_json, normalized_hash, conflict_key, \
          provenance_json, confidence, state, created_at, updated_at, verified_at, expires_at, \
          pinned, rejection_reason, revision) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(record.id.to_string())
    .bind(enum_to_db(&record.content.kind())?)
    .bind(record.scope.kind())
    .bind(record.scope.key())
    .bind(serde_json::to_string(&record.content)?)
    .bind(hash)
    .bind(&record.conflict_key)
    .bind(serde_json::to_string(&record.provenance)?)
    .bind(f64::from(record.confidence))
    .bind(enum_to_db(&record.state)?)
    .bind(record.created_at.to_rfc3339())
    .bind(record.updated_at.to_rfc3339())
    .bind(record.verified_at.as_ref().map(DateTime::to_rfc3339))
    .bind(record.expires_at.as_ref().map(DateTime::to_rfc3339))
    .bind(record.pinned)
    .bind(&record.rejection_reason)
    .bind(record.revision as i64)
    .execute(executor)
    .await?;
    Ok(())
}

async fn update_record(
    executor: impl sqlx::SqliteExecutor<'_>,
    record: &LearningRecord,
    hash: &str,
    expected_revision: u64,
) -> Result<(), LearningError> {
    let result = sqlx::query(
        "UPDATE learning_records SET kind = ?, content_json = ?, normalized_hash = ?, \
         conflict_key = ?, provenance_json = ?, confidence = ?, state = ?, updated_at = ?, \
         verified_at = ?, expires_at = ?, pinned = ?, rejection_reason = ?, revision = ? \
         WHERE id = ? AND revision = ?",
    )
    .bind(enum_to_db(&record.content.kind())?)
    .bind(serde_json::to_string(&record.content)?)
    .bind(hash)
    .bind(&record.conflict_key)
    .bind(serde_json::to_string(&record.provenance)?)
    .bind(f64::from(record.confidence))
    .bind(enum_to_db(&record.state)?)
    .bind(record.updated_at.to_rfc3339())
    .bind(record.verified_at.as_ref().map(DateTime::to_rfc3339))
    .bind(record.expires_at.as_ref().map(DateTime::to_rfc3339))
    .bind(record.pinned)
    .bind(&record.rejection_reason)
    .bind(record.revision as i64)
    .bind(record.id.to_string())
    .bind(expected_revision as i64)
    .execute(executor)
    .await?;
    if result.rows_affected() == 0 {
        return Err(LearningError::RevisionConflict {
            id: record.id,
            expected: expected_revision,
        });
    }
    Ok(())
}

async fn write_existing(
    pool: &SqlitePool,
    record: &LearningRecord,
    expected_revision: u64,
) -> Result<(), LearningError> {
    let hash = normalized_hash(&record.content)?;
    update_record(pool, record, &hash, expected_revision).await
}

fn ensure_revision(record: &LearningRecord, expected: u64) -> Result<(), LearningError> {
    if record.revision == expected {
        Ok(())
    } else {
        Err(LearningError::RevisionConflict {
            id: record.id,
            expected,
        })
    }
}

fn record_from_row(row: &SqliteRow) -> Result<LearningRecord, LearningError> {
    let id: String = row.try_get("id")?;
    let kind: String = row.try_get("kind")?;
    let scope_kind: String = row.try_get("scope_kind")?;
    let scope_key: String = row.try_get("scope_key")?;
    let content_json: String = row.try_get("content_json")?;
    let provenance_json: String = row.try_get("provenance_json")?;
    let state: String = row.try_get("state")?;
    let created_at: String = row.try_get("created_at")?;
    let updated_at: String = row.try_get("updated_at")?;
    let verified_at: Option<String> = row.try_get("verified_at")?;
    let expires_at: Option<String> = row.try_get("expires_at")?;
    let confidence: f64 = row.try_get("confidence")?;
    let revision: i64 = row.try_get("revision")?;
    let content: LearningContent = serde_json::from_str(&content_json)?;
    let stored_kind: LearningKind = enum_from_db(&kind)?;
    if content.kind() != stored_kind {
        return Err(LearningError::Corrupt(format!(
            "content kind {:?} does not match stored kind {stored_kind:?}",
            content.kind()
        )));
    }
    Ok(LearningRecord {
        id: parse_id(&id)?,
        scope: scope_from_parts(&scope_kind, &scope_key)?,
        content,
        conflict_key: row.try_get("conflict_key")?,
        provenance: serde_json::from_str(&provenance_json)?,
        confidence: confidence as f32,
        state: enum_from_db(&state)?,
        created_at: parse_time(&created_at, "created_at")?,
        updated_at: parse_time(&updated_at, "updated_at")?,
        verified_at: verified_at
            .as_deref()
            .map(|value| parse_time(value, "verified_at"))
            .transpose()?,
        expires_at: expires_at
            .as_deref()
            .map(|value| parse_time(value, "expires_at"))
            .transpose()?,
        pinned: row.try_get("pinned")?,
        rejection_reason: row.try_get("rejection_reason")?,
        revision: u64::try_from(revision)
            .map_err(|_| LearningError::Corrupt(format!("invalid revision {revision}")))?,
    })
}

fn scope_from_parts(kind: &str, key: &str) -> Result<LearningScope, LearningError> {
    match kind {
        "user" => Ok(LearningScope::User(UserId(key.to_owned()))),
        "repository" => RepositoryId::from_str(key)
            .map(LearningScope::Repository)
            .map_err(|error| LearningError::Corrupt(format!("repository scope `{key}`: {error}"))),
        "provider" => Ok(LearningScope::Provider(key.to_owned())),
        "council" => Ok(LearningScope::Council(key.to_owned())),
        other => Err(LearningError::Corrupt(format!(
            "unknown learning scope `{other}`"
        ))),
    }
}

fn enum_to_db<T: Serialize>(value: &T) -> Result<String, LearningError> {
    Ok(serde_json::to_string(value)?.trim_matches('"').to_owned())
}

fn enum_from_db<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, LearningError> {
    Ok(serde_json::from_str(&format!("\"{value}\""))?)
}

fn parse_id(value: &str) -> Result<LearningId, LearningError> {
    LearningId::from_str(value)
        .map_err(|error| LearningError::Corrupt(format!("learning id `{value}`: {error}")))
}

fn parse_time(value: &str, field: &str) -> Result<DateTime<Utc>, LearningError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| LearningError::Corrupt(format!("{field} `{value}`: {error}")))
}

#[cfg(test)]
mod policy_tests {
    use super::{looks_like_raw_log, strict_text_rejection_reason};

    #[test]
    fn raw_log_detection_is_conservative_but_catches_transcripts() {
        assert!(looks_like_raw_log(
            "2026-08-12 INFO start\n2026-08-12 WARN retry\n2026-08-12 ERROR failed"
        ));
        assert!(!looks_like_raw_log(
            "Run the tests\nInspect failures\nPatch the implementation"
        ));
    }

    #[test]
    fn rejects_low_value_chat_and_temporary_paths() {
        assert!(strict_text_rejection_reason("Hello!").is_some());
        assert!(strict_text_rejection_reason("Done").is_some());
        assert!(strict_text_rejection_reason("output lives in /var/folders/x/result").is_some());
        assert!(strict_text_rejection_reason("Use cargo nextest for repository tests").is_none());
    }
}
