//! Daemon-side handlers for inspecting, correcting, forgetting, and opening
//! the source of a memory (2026-08-13 review, memory-docs vertical, F3/F4).
//!
//! `MemoryStore::forget`/`forget_scope`/`correct` (`codypendent-knowledge`)
//! already implement the Chapter 06 inspect/edit/delete triad in full, and
//! [`provenance_cards`](codypendent_knowledge::provenance_cards) already
//! projects "every retrieved memory opens its source" — but nothing a client
//! can reach ever called any of them: `crates/protocol/src/command.rs` has no
//! memory command at all, and the TUI's memory surface reads SQLite directly,
//! read-only.
//!
//! This module is the daemon-side half a protocol command can call once one
//! exists: it adds the ONE thing the bare store methods deliberately do
//! **not** do — verifying the target memory (or scope) actually falls inside
//! the caller's visible scopes before acting — and resolves an
//! [`EvidenceRef`] to the real content behind a provenance card. Protocol and
//! TUI wiring are proposed in
//! `.impl/proposals/agent-security-from-agent-memory.md` and
//! `.impl/proposals/agent-tui-from-agent-memory.md`.
//!
//! ## No enumeration oracle
//!
//! A memory outside the caller's visible scopes is refused **identically** to
//! one that does not exist at all (`MemoryError::NotFound`), for
//! [`inspect`], [`correct`], and [`forget`] alike — a client must never be
//! able to use these calls as an oracle for "does memory X exist in a
//! repository I cannot see" (brief rule 2: a check that gates access must be
//! enforced where the resource is fetched, and must fail identically for "not
//! allowed" and "does not exist"). [`forget_scope`] is the one exception: the
//! caller already names the scope outright, so refusing a scope it may not
//! touch leaks nothing new.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use codypendent_daemon::artifacts::{ArtifactStore, Provenance};
use codypendent_daemon::ledger;
use codypendent_daemon::memory::{
    CorrectMemoryRequest, ForgetMemoryRequest, InspectMemoryRequest, MemoryEvidenceFuture,
    MemoryForgetFuture, MemoryGateway, MemoryViewFuture, OpenMemoryEvidenceRequest,
};
use codypendent_knowledge::{
    local_user_scope, EvidenceRef, ForgetAudit, MemoryCorrection, MemoryError, MemoryRecord,
    MemoryStore, Scope,
};
use codypendent_protocol::{
    CodypendentError, DataClassification, MemoryEvidence as WireEvidence, MemoryId, MemoryScope,
    MemoryScopeTier, MemoryView, RepositoryId, SessionEvent,
};
use sqlx::SqlitePool;
use tokio::io::AsyncReadExt;

/// The scopes a memory inspect/edit/delete request may act on for a daemon
/// serving `repository` — byte-for-byte the same widened set
/// [`emit_context`](crate::executor::RuntimeExecutor) builds
/// (`[System, local_user_scope(), Repository(repository)]`). Callers use this
/// rather than re-deriving their own scope list specifically to avoid the
/// class of bug this same review found one layer up (F1: two call sites
/// computing a repository identity two different ways and silently
/// disagreeing) — every caller of this module gets "what may I touch" from
/// this one function.
#[must_use]
pub fn visible_scopes(repository: RepositoryId) -> Vec<Scope> {
    vec![
        Scope::System,
        local_user_scope(),
        Scope::Repository(repository),
    ]
}

/// Whether `record` is inside `visible_scopes` — the one check a bare
/// `MemoryStore` method cannot make on its own (the store has no notion of
/// "caller").
fn in_scope(record: &MemoryRecord, visible_scopes: &[Scope]) -> bool {
    visible_scopes.contains(&record.scope)
}

/// Inspect one memory, if it is visible to the caller. `Ok(None)` for both
/// "does not exist" and "exists outside `visible_scopes`" — see the module
/// docs.
pub async fn inspect(
    pool: &SqlitePool,
    id: MemoryId,
    visible_scopes: &[Scope],
) -> Result<Option<MemoryRecord>, MemoryError> {
    Ok(MemoryStore::new()
        .get(pool, id)
        .await?
        .filter(|record| in_scope(record, visible_scopes)))
}

/// Correct a live, in-scope memory (see
/// [`MemoryStore::correct`](codypendent_knowledge::MemoryStore::correct)).
/// Refuses with [`MemoryError::NotFound`] identically whether `id` is absent,
/// historical (already superseded), or simply outside `visible_scopes`.
pub async fn correct(
    pool: &SqlitePool,
    id: MemoryId,
    visible_scopes: &[Scope],
    correction: MemoryCorrection,
) -> Result<MemoryRecord, MemoryError> {
    let store = MemoryStore::new();
    let visible = store
        .get(pool, id)
        .await?
        .is_some_and(|record| in_scope(&record, visible_scopes));
    if !visible {
        return Err(MemoryError::NotFound(id));
    }
    store.correct(pool, id, correction).await
}

/// Forget one memory (see
/// [`MemoryStore::forget`](codypendent_knowledge::MemoryStore::forget)).
/// Refuses with [`MemoryError::NotFound`] identically whether `id` is absent
/// or simply outside `visible_scopes` — never distinguished.
pub async fn forget(
    pool: &SqlitePool,
    id: MemoryId,
    visible_scopes: &[Scope],
) -> Result<ForgetAudit, MemoryError> {
    let store = MemoryStore::new();
    let visible = store
        .get(pool, id)
        .await?
        .is_some_and(|record| in_scope(&record, visible_scopes));
    if !visible {
        return Err(MemoryError::NotFound(id));
    }
    store.forget(pool, id).await
}

/// Forget every memory in `scope` (see
/// [`MemoryStore::forget_scope`](codypendent_knowledge::MemoryStore::forget_scope)).
/// Refuses a `scope` the caller may not touch with [`MemoryError::Policy`] —
/// the caller already names the scope outright, so refusing accurately here
/// leaks nothing a client did not already assert.
pub async fn forget_scope(
    pool: &SqlitePool,
    scope: Scope,
    visible_scopes: &[Scope],
) -> Result<ForgetAudit, MemoryError> {
    if !visible_scopes.contains(&scope) {
        return Err(MemoryError::Policy(
            "scope is not visible to this caller".to_string(),
        ));
    }
    MemoryStore::new().forget_scope(pool, &scope).await
}

/// What [`open_evidence`] resolves an [`EvidenceRef`] to — the content behind
/// a provenance card. Closes Chapter 06's "every retrieved memory opens its
/// source" exit criterion (2026-08-13 review F4):
/// [`provenance_cards`](codypendent_knowledge::provenance_cards) already
/// projects the card, but nothing ever fetched the artifact/event range it
/// names — both existing display surfaces stopped at an opaque id string.
#[derive(Debug, Clone, PartialEq)]
pub enum EvidenceContent {
    /// The session ledger events in `[from_sequence, to_sequence]` (both ends
    /// inclusive, matching how `EvidenceRef::EventRange` is constructed
    /// throughout the fabric — see `crate::observer`).
    Events(Vec<SessionEvent>),
    /// The stored artifact's raw bytes and media type.
    Artifact { media_type: String, bytes: Vec<u8> },
}

/// Resolve one [`EvidenceRef`] to its actual content: the session ledger
/// events it names, or the artifact bytes it names. This is the fetch a
/// client's "open source" action performs once wired to a protocol command —
/// today nothing calls it, so a `RevealSource` action can render only the
/// evidence ref's own opaque ids.
pub async fn open_evidence(
    pool: &SqlitePool,
    artifacts: &ArtifactStore,
    evidence: &EvidenceRef,
) -> anyhow::Result<EvidenceContent> {
    match evidence {
        EvidenceRef::EventRange {
            session_id,
            from_sequence,
            to_sequence,
        } => {
            // `load_events_between`'s contract is `after < sequence <= through`
            // (exclusive start); `from_sequence` is inclusive, so the lower
            // bound is shifted down by one to include it.
            let events = ledger::load_events_between(
                pool,
                *session_id,
                from_sequence.saturating_sub(1),
                *to_sequence,
            )
            .await?;
            Ok(EvidenceContent::Events(events))
        }
        EvidenceRef::Artifact { artifact, .. } => {
            let mut file = artifacts.open(pool, artifact.id).await?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).await?;
            Ok(EvidenceContent::Artifact {
                media_type: artifact.media_type.clone(),
                bytes,
            })
        }
        // An agent assertion carries its own content — the rationale IS the
        // evidence, and the run that gave it is named right here. Opening it
        // resolves to that run's turn in the session ledger, so "open source"
        // lands on the transcript where the claim was made rather than on an
        // opaque id.
        EvidenceRef::AgentAssertion {
            session_id, run_id, ..
        } => {
            let events = ledger::load_events(pool, *session_id).await?;
            Ok(EvidenceContent::Events(
                events
                    .into_iter()
                    .filter(|event| event_names_run(event, *run_id))
                    .collect(),
            ))
        }
    }
}

/// Whether a ledger event belongs to `run` — the filter that turns "the session
/// that asserted this" into "the turns that asserted it". Matched on the
/// serialized body rather than on an exhaustive walk of every `EventBody`
/// variant that carries a run id, because that list grows and a missed variant
/// here silently narrows the evidence a user is shown.
fn event_names_run(event: &SessionEvent, run: codypendent_protocol::RunId) -> bool {
    serde_json::to_string(&event.body).is_ok_and(|body| body.contains(&run.to_string()))
}

// ---------------------------------------------------------------------------
// The protocol seam (outcome 17): `codypendent_daemon::memory::MemoryGateway`
// ---------------------------------------------------------------------------

/// Fills the daemon's [`MemoryGateway`] seam over the handlers above.
///
/// Everything a caller could get wrong is derived here rather than accepted:
/// the repository identity comes from [`crate::scan::repository_id_for`] (the
/// daemon's one derivation of it — the review's F1 was two call sites computing
/// it two different ways), the visible scope set comes from [`visible_scopes`],
/// and a bulk forget's target is one of those scopes chosen by TIER, so no
/// scope key ever crosses the wire.
#[derive(Clone)]
pub struct MemoryStoreGateway {
    pool: SqlitePool,
    artifacts: ArtifactStore,
}

impl MemoryStoreGateway {
    #[must_use]
    pub fn new(pool: SqlitePool, artifacts: ArtifactStore) -> Self {
        Self { pool, artifacts }
    }

    /// The scopes a command naming `repository` may act on.
    fn scopes_for(repository: &str) -> Vec<Scope> {
        visible_scopes(crate::scan::repository_id_for(std::path::Path::new(
            repository,
        )))
    }
}

/// Every store failure becomes a wire error here, and `NotFound` keeps its
/// double duty: absent and out-of-scope produce the SAME code and the SAME
/// message, so the reply cannot be read as an existence oracle.
fn memory_error_to_protocol(error: MemoryError) -> CodypendentError {
    match error {
        MemoryError::NotFound(id) => CodypendentError::new(
            "memory.not-found",
            format!("memory {id} was not found"),
            false,
        ),
        MemoryError::Policy(message) => {
            CodypendentError::new("memory.policy-denied", message, false)
        }
        other => CodypendentError::new("memory.store-error", other.to_string(), true),
    }
}

/// Project a stored record onto the wire view. Evidence becomes an ordered list
/// of labels: the index into that list is what `OpenMemoryEvidence` addresses,
/// so a client never has to reconstruct an `EvidenceRef` it cannot type.
fn to_view(record: &MemoryRecord) -> MemoryView {
    MemoryView {
        id: record.id,
        scope: MemoryScope {
            tier: record.scope.tier().to_string(),
            key: record.scope.key(),
        },
        class: serde_json::to_value(record.class)
            .ok()
            .and_then(|value| value.as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "unknown".to_string()),
        statement: record.statement.clone(),
        structured_value: record.structured_value.clone(),
        confidence: record.confidence,
        observed_at: record.observed_at,
        sensitivity: record.sensitivity,
        supersedes: record.supersedes.clone(),
        evidence: record.provenance.iter().map(evidence_label).collect(),
    }
}

/// A bounded, human-legible name for one evidence ref — never its content.
fn evidence_label(evidence: &EvidenceRef) -> String {
    match evidence {
        EvidenceRef::EventRange {
            session_id,
            from_sequence,
            to_sequence,
        } => format!("session {session_id} events {from_sequence}..{to_sequence}"),
        EvidenceRef::Artifact {
            artifact,
            source_path,
        } => match source_path {
            Some(path) => format!("artifact {} ({path})", artifact.id),
            None => format!("artifact {}", artifact.id),
        },
        // The rationale is the label here: it is short by construction (the
        // asserting tool caps it) and it is the ONLY thing that distinguishes
        // one agent assertion from another, which is the whole point of a label.
        EvidenceRef::AgentAssertion {
            run_id, rationale, ..
        } => format!("asserted by run {run_id}: {rationale}"),
    }
}

impl MemoryGateway for MemoryStoreGateway {
    fn read_knowledge(
        &self,
        request: codypendent_daemon::memory::ReadKnowledgeRequest,
    ) -> codypendent_daemon::memory::KnowledgePageFuture<'_> {
        Box::pin(async move {
            codypendent_knowledge::client_views::read_page(
                &self.pool,
                request.session_id,
                request.section,
                request.offset,
                request.limit,
            )
            .await
            .map_err(|error| {
                CodypendentError::new("knowledge.read-failed", error.to_string(), false)
            })
        })
    }
    fn inspect(&self, request: InspectMemoryRequest) -> MemoryViewFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            let scopes = MemoryStoreGateway::scopes_for(&request.repository);
            match inspect(&host.pool, request.id, &scopes).await {
                Ok(Some(record)) => Ok(to_view(&record)),
                // Absent and out-of-scope arrive here as the same `None`, and
                // leave as the same rejection.
                Ok(None) => Err(memory_error_to_protocol(MemoryError::NotFound(request.id))),
                Err(error) => Err(memory_error_to_protocol(error)),
            }
        })
    }

    fn correct(&self, request: CorrectMemoryRequest) -> MemoryViewFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            let scopes = MemoryStoreGateway::scopes_for(&request.repository);
            // `MemoryStore::correct` requires fresh evidence, and rightly so: a
            // correction with no source is just an assertion. The edit action
            // IS the evidence, so the daemon stores the request that made it —
            // content-addressed, durable, and openable later through
            // `OpenMemoryEvidence` like any other provenance.
            let receipt = serde_json::json!({
                "correction_of": request.id.to_string(),
                "statement": request.statement,
                "confidence": request.confidence,
            });
            // Validate BEFORE minting the receipt. `correct` re-checks
            // authoritatively below — this pre-check exists only so a rejected
            // request (absent, already superseded, or out of scope) does not
            // leave an orphan artifact row and blob behind. Without it every
            // invalid CorrectMemory grew storage permanently, and a caller
            // could grow it without bound by repeating one.
            let visible = MemoryStore::new()
                .get(&host.pool, request.id)
                .await
                .map_err(memory_error_to_protocol)?
                .is_some_and(|record| in_scope(&record, &scopes));
            if !visible {
                return Err(memory_error_to_protocol(MemoryError::NotFound(request.id)));
            }

            let stored = host
                .artifacts
                .put(
                    &host.pool,
                    "application/json",
                    DataClassification::Internal,
                    Provenance::system("memory-correction"),
                    &serde_json::to_vec(&receipt).map_err(|error| {
                        CodypendentError::new("memory.store-error", error.to_string(), true)
                    })?,
                )
                .await
                .map_err(|error| {
                    CodypendentError::new("memory.store-error", error.to_string(), true)
                })?;
            let correction = MemoryCorrection {
                statement: request.statement,
                structured_value: request.structured_value,
                provenance: vec![EvidenceRef::Artifact {
                    artifact: stored,
                    source_path: None,
                }],
                confidence: request.confidence,
            };
            correct(&host.pool, request.id, &scopes, correction)
                .await
                .map(|record| to_view(&record))
                .map_err(memory_error_to_protocol)
        })
    }

    fn forget(&self, request: ForgetMemoryRequest) -> MemoryForgetFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            let scopes = MemoryStoreGateway::scopes_for(&request.repository);
            let audit = match request.id {
                Some(id) => forget(&host.pool, id, &scopes).await,
                None => {
                    // The tier picks one of the caller's OWN visible scopes.
                    // There is deliberately no path from the wire to a scope
                    // key, so a bulk delete cannot be aimed at a repository the
                    // caller cannot already see.
                    let Some(scope) = scopes
                        .iter()
                        .find(|scope| tier_matches(scope, request.tier))
                        .cloned()
                    else {
                        return Err(CodypendentError::new(
                            "memory.unknown-scope-tier",
                            "unrecognized memory scope tier".to_string(),
                            false,
                        ));
                    };
                    forget_scope(&host.pool, scope, &scopes).await
                }
            };
            audit
                .map(|audit| audit.forgotten)
                .map_err(memory_error_to_protocol)
        })
    }

    fn open_evidence(&self, request: OpenMemoryEvidenceRequest) -> MemoryEvidenceFuture<'_> {
        let host = self.clone();
        Box::pin(async move {
            let scopes = MemoryStoreGateway::scopes_for(&request.repository);
            // The gate is HERE, at the fetch — the memory is re-read under the
            // caller's scopes before any artifact or event range behind it is
            // opened, so naming a memory you cannot see never yields its bytes.
            let record = inspect(&host.pool, request.id, &scopes)
                .await
                .map_err(memory_error_to_protocol)?
                .ok_or_else(|| memory_error_to_protocol(MemoryError::NotFound(request.id)))?;
            let Some(evidence) = record.provenance.get(request.evidence_index as usize) else {
                return Err(CodypendentError::new(
                    "memory.evidence-not-found",
                    format!(
                        "memory {} has no evidence at index {}",
                        request.id, request.evidence_index
                    ),
                    false,
                ));
            };
            match open_evidence(&host.pool, &host.artifacts, evidence).await {
                Ok(EvidenceContent::Events(events)) => Ok(WireEvidence::Events { events }),
                Ok(EvidenceContent::Artifact { media_type, bytes }) => Ok(WireEvidence::Artifact {
                    media_type,
                    bytes_base64: BASE64.encode(bytes),
                }),
                Err(error) => Err(CodypendentError::new(
                    "memory.evidence-unreadable",
                    error.to_string(),
                    false,
                )),
            }
        })
    }
}

/// Whether `scope` is the one the wire tier names. `Unknown` matches nothing —
/// a tier this build does not understand must never fall through to a scope.
fn tier_matches(scope: &Scope, tier: MemoryScopeTier) -> bool {
    match tier {
        MemoryScopeTier::System => matches!(scope, Scope::System),
        MemoryScopeTier::User => matches!(scope, Scope::User(_)),
        MemoryScopeTier::Repository => matches!(scope, Scope::Repository(_)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_daemon::artifacts::Provenance;
    use codypendent_daemon::db;
    use codypendent_knowledge::{CandidateMemory, Curation, MemoryClass};
    use codypendent_protocol::{Actor, DataClassification, EventBody, SessionId};

    async fn temp_pool() -> (tempfile::TempDir, SqlitePool) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = db::open_database(&tmp.path().join("codypendent.db"))
            .await
            .expect("open db");
        (tmp, pool)
    }

    fn some_evidence() -> Vec<codypendent_knowledge::EvidenceRef> {
        vec![EvidenceRef::EventRange {
            session_id: SessionId::new(),
            from_sequence: 1,
            to_sequence: 1,
        }]
    }

    /// Insert a live memory directly in `scope` and return its id.
    async fn seed_memory(pool: &SqlitePool, scope: Scope, statement: &str) -> MemoryId {
        let candidate = CandidateMemory {
            class: MemoryClass::Semantic,
            scope: Some(scope),
            statement: statement.to_string(),
            structured_value: None,
            provenance: some_evidence(),
            confidence: 0.8,
            observed_at: chrono::Utc::now(),
            valid_from: codypendent_knowledge::Revision::sequence(1),
            sensitivity: DataClassification::Internal,
            retention: None,
        };
        match MemoryStore::new().curate(pool, candidate).await.unwrap() {
            Curation::Accepted(record) => record.id,
            other => panic!("expected the seed memory to be accepted, got {other:?}"),
        }
    }

    /// A rejected correction used to mint its receipt artifact BEFORE the
    /// target was validated, so every invalid `CorrectMemory` left an orphan
    /// metadata row (and, for a unique statement, an orphan blob) behind. A
    /// caller could grow the artifact store without bound by repeating one.
    #[tokio::test]
    async fn a_rejected_correction_persists_no_receipt_artifact() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = db::open_database(&tmp.path().join("codypendent.db"))
            .await
            .expect("open db");
        let gateway = MemoryStoreGateway::new(
            pool.clone(),
            ArtifactStore::new(tmp.path().join("artifacts")),
        );

        let artifacts_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts")
            .fetch_one(&pool)
            .await
            .expect("count artifacts");

        // Out of scope for the repository this request names, so `correct`
        // refuses it — the same refusal an absent id gets.
        let hidden = seed_memory(
            &pool,
            Scope::Repository(RepositoryId::new()),
            "another checkout's secret",
        )
        .await;

        for id in [hidden, MemoryId::new()] {
            let refused = gateway
                .correct(CorrectMemoryRequest {
                    id,
                    repository: tmp.path().to_string_lossy().into_owned(),
                    statement: "a correction that must not be recorded".to_string(),
                    structured_value: None,
                    confidence: 0.9,
                })
                .await;
            assert!(
                refused.is_err(),
                "a hidden or absent memory must be refused"
            );
        }

        let artifacts_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts")
            .fetch_one(&pool)
            .await
            .expect("count artifacts");
        assert_eq!(
            artifacts_before, artifacts_after,
            "a refused correction must leave no receipt behind"
        );
    }

    #[tokio::test]
    async fn inspect_is_identical_for_absent_and_out_of_scope() {
        let (_tmp, pool) = temp_pool().await;
        let visible_repo = RepositoryId::new();
        let other_repo = RepositoryId::new();
        let visible = visible_scopes(visible_repo);

        let in_scope_id = seed_memory(&pool, Scope::Repository(visible_repo), "a").await;
        let out_of_scope_id = seed_memory(&pool, Scope::Repository(other_repo), "b").await;

        assert!(inspect(&pool, in_scope_id, &visible)
            .await
            .unwrap()
            .is_some());
        assert!(inspect(&pool, out_of_scope_id, &visible)
            .await
            .unwrap()
            .is_none());
        assert!(inspect(&pool, MemoryId::new(), &visible)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn forget_refuses_an_out_of_scope_memory_and_deletes_nothing() {
        let (_tmp, pool) = temp_pool().await;
        let visible_repo = RepositoryId::new();
        let other_repo = RepositoryId::new();
        let visible = visible_scopes(visible_repo);
        let id = seed_memory(
            &pool,
            Scope::Repository(other_repo),
            "secret to another repo",
        )
        .await;

        let error = forget(&pool, id, &visible).await.unwrap_err();
        assert!(matches!(error, MemoryError::NotFound(refused) if refused == id));

        // Nothing was deleted — a scope check that ran AFTER the delete would
        // be a much worse bug than one that never ran at all.
        assert!(MemoryStore::new().get(&pool, id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn forget_removes_an_in_scope_memory() {
        let (_tmp, pool) = temp_pool().await;
        let repo = RepositoryId::new();
        let visible = visible_scopes(repo);
        let id = seed_memory(&pool, Scope::Repository(repo), "safe to forget").await;

        let audit = forget(&pool, id, &visible).await.unwrap();
        assert_eq!(audit.forgotten, vec![id]);
        assert!(MemoryStore::new().get(&pool, id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn correct_refuses_an_out_of_scope_memory() {
        let (_tmp, pool) = temp_pool().await;
        let visible_repo = RepositoryId::new();
        let other_repo = RepositoryId::new();
        let visible = visible_scopes(visible_repo);
        let id = seed_memory(&pool, Scope::Repository(other_repo), "not yours").await;

        let error = correct(
            &pool,
            id,
            &visible,
            MemoryCorrection {
                statement: "rewritten".to_string(),
                structured_value: None,
                provenance: some_evidence(),
                confidence: 0.9,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, MemoryError::NotFound(refused) if refused == id));
    }

    #[tokio::test]
    async fn forget_scope_refuses_a_scope_the_caller_cannot_see() {
        let (_tmp, pool) = temp_pool().await;
        let visible_repo = RepositoryId::new();
        let other_repo = RepositoryId::new();
        let visible = visible_scopes(visible_repo);
        seed_memory(&pool, Scope::Repository(other_repo), "not yours").await;

        let error = forget_scope(&pool, Scope::Repository(other_repo), &visible)
            .await
            .unwrap_err();
        assert!(matches!(error, MemoryError::Policy(_)));
        // The other repository's memory survives untouched.
        assert_eq!(
            MemoryStore::new()
                .query(&pool, &[Scope::Repository(other_repo)], None)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// The gateway is the layer a socket command reaches, so the no-oracle
    /// property has to hold THERE, not only in the bare handler: a memory in a
    /// repository the caller did not name and one that never existed must come
    /// back as the same rejection, byte for byte.
    #[tokio::test]
    async fn the_gateway_refuses_an_out_of_scope_memory_exactly_like_an_absent_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = db::open_database(&tmp.path().join("codypendent.db"))
            .await
            .expect("open db");
        let gateway = MemoryStoreGateway::new(
            pool.clone(),
            ArtifactStore::new(tmp.path().join("artifacts")),
        );
        // A repository the caller never names, so its memory is out of scope for
        // every request below.
        let hidden = seed_memory(
            &pool,
            Scope::Repository(RepositoryId::new()),
            "another checkout's secret",
        )
        .await;

        let out_of_scope = gateway
            .inspect(InspectMemoryRequest {
                id: hidden,
                repository: tmp.path().to_string_lossy().into_owned(),
            })
            .await
            .expect_err("a memory outside the caller's scopes must not be readable");
        let absent = gateway
            .inspect(InspectMemoryRequest {
                id: MemoryId::new(),
                repository: tmp.path().to_string_lossy().into_owned(),
            })
            .await
            .expect_err("an absent memory must not be readable either");
        assert_eq!(out_of_scope.code, "memory.not-found");
        assert_eq!(
            out_of_scope.code, absent.code,
            "an out-of-scope memory must be indistinguishable from an absent one"
        );

        // …and the same gate applies where the EVIDENCE is fetched, not only
        // where the memory is listed: naming a memory you cannot see must not
        // yield the artifact or events behind it.
        let evidence = gateway
            .open_evidence(OpenMemoryEvidenceRequest {
                id: hidden,
                repository: tmp.path().to_string_lossy().into_owned(),
                evidence_index: 0,
            })
            .await
            .expect_err("evidence behind an invisible memory must stay invisible");
        assert_eq!(evidence.code, "memory.not-found");
    }

    /// A correction supersedes rather than overwrites, and the daemon — not the
    /// caller — supplies the correction's evidence.
    #[tokio::test]
    async fn the_gateway_corrects_by_superseding_and_supplies_its_own_evidence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = db::open_database(&tmp.path().join("codypendent.db"))
            .await
            .expect("open db");
        let repository = tmp.path().to_string_lossy().into_owned();
        let gateway = MemoryStoreGateway::new(
            pool.clone(),
            ArtifactStore::new(tmp.path().join("artifacts")),
        );
        let scope = crate::scan::repository_id_for(tmp.path());
        let original =
            seed_memory(&pool, Scope::Repository(scope), "the parser is generated").await;

        let corrected = gateway
            .correct(CorrectMemoryRequest {
                id: original,
                repository: repository.clone(),
                statement: "the parser is hand-written".to_string(),
                structured_value: None,
                confidence: 0.9,
            })
            .await
            .expect("an in-scope correction is accepted");
        assert_ne!(corrected.id, original, "a correction is a NEW record");
        assert_eq!(corrected.supersedes, vec![original]);
        assert_eq!(
            corrected.evidence.len(),
            1,
            "the daemon attaches the edit's own receipt as evidence: {:?}",
            corrected.evidence
        );

        // The receipt is real, openable content — not a placeholder id.
        match gateway
            .open_evidence(OpenMemoryEvidenceRequest {
                id: corrected.id,
                repository,
                evidence_index: 0,
            })
            .await
            .expect("the correction receipt opens")
        {
            WireEvidence::Artifact { media_type, .. } => assert_eq!(media_type, "application/json"),
            other => panic!("expected the correction receipt artifact, got {other:?}"),
        }
    }

    /// A bulk forget can only name a TIER, and the tier resolves against the
    /// caller's own visible scopes — so there is no wire path to another
    /// repository's memories at all.
    #[tokio::test]
    async fn a_tier_forget_reaches_only_the_callers_own_repository() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = db::open_database(&tmp.path().join("codypendent.db"))
            .await
            .expect("open db");
        let repository = tmp.path().to_string_lossy().into_owned();
        let gateway = MemoryStoreGateway::new(
            pool.clone(),
            ArtifactStore::new(tmp.path().join("artifacts")),
        );
        let mine = crate::scan::repository_id_for(tmp.path());
        let theirs = RepositoryId::new();
        let ours = seed_memory(&pool, Scope::Repository(mine), "mine").await;
        seed_memory(&pool, Scope::Repository(theirs), "theirs").await;

        let forgotten = gateway
            .forget(ForgetMemoryRequest {
                id: None,
                repository,
                tier: MemoryScopeTier::Repository,
            })
            .await
            .expect("forgetting my own repository tier is allowed");
        assert_eq!(forgotten, vec![ours]);
        assert_eq!(
            MemoryStore::new()
                .query(&pool, &[Scope::Repository(theirs)], None)
                .await
                .unwrap()
                .len(),
            1,
            "another repository's memories are untouched"
        );
    }

    /// A tier this build does not understand must reach no scope at all —
    /// falling through to "the first visible scope" would make an unknown wire
    /// value delete something.
    #[tokio::test]
    async fn an_unknown_tier_forgets_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = db::open_database(&tmp.path().join("codypendent.db"))
            .await
            .expect("open db");
        let gateway = MemoryStoreGateway::new(
            pool.clone(),
            ArtifactStore::new(tmp.path().join("artifacts")),
        );
        let mine = crate::scan::repository_id_for(tmp.path());
        seed_memory(&pool, Scope::Repository(mine), "mine").await;

        let refused = gateway
            .forget(ForgetMemoryRequest {
                id: None,
                repository: tmp.path().to_string_lossy().into_owned(),
                tier: MemoryScopeTier::Unknown,
            })
            .await
            .expect_err("an unrecognized tier must refuse, not guess");
        assert_eq!(refused.code, "memory.unknown-scope-tier");
        assert_eq!(
            MemoryStore::new()
                .query(&pool, &[Scope::Repository(mine)], None)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn open_evidence_reads_back_the_real_event_range_and_artifact_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::open_database(&tmp.path().join("codypendent.db"))
            .await
            .unwrap();
        let artifacts = ArtifactStore::new(tmp.path().join("artifacts"));
        let session_id = SessionId::new();
        ledger::create_session(&pool, session_id, "evidence")
            .await
            .unwrap();

        let e1 = ledger::append_next_event(
            &pool,
            session_id,
            &Actor::System,
            &EventBody::NoteAppended {
                text: "first note".to_string(),
                run_id: None,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        let e2 = ledger::append_next_event(
            &pool,
            session_id,
            &Actor::System,
            &EventBody::NoteAppended {
                text: "second note".to_string(),
                run_id: None,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        // A third event OUTSIDE the requested range must not come back.
        ledger::append_next_event(
            &pool,
            session_id,
            &Actor::System,
            &EventBody::NoteAppended {
                text: "third note, out of range".to_string(),
                run_id: None,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();

        let range = EvidenceRef::EventRange {
            session_id,
            from_sequence: e1.sequence,
            to_sequence: e2.sequence,
        };
        match open_evidence(&pool, &artifacts, &range).await.unwrap() {
            EvidenceContent::Events(events) => {
                assert_eq!(
                    events.len(),
                    2,
                    "exactly the requested [from, to]: {events:?}"
                );
                assert_eq!(events[0].sequence, e1.sequence);
                assert_eq!(events[1].sequence, e2.sequence);
            }
            other => panic!("expected Events, got {other:?}"),
        }

        let stored = artifacts
            .put(
                &pool,
                "text/plain",
                DataClassification::Internal,
                Provenance::system("test"),
                b"the real evidence bytes",
            )
            .await
            .unwrap();
        let artifact_ref = EvidenceRef::Artifact {
            artifact: stored,
            source_path: Some("notes.txt".to_string()),
        };
        match open_evidence(&pool, &artifacts, &artifact_ref)
            .await
            .unwrap()
        {
            EvidenceContent::Artifact { media_type, bytes } => {
                assert_eq!(media_type, "text/plain");
                assert_eq!(bytes, b"the real evidence bytes");
            }
            other => panic!("expected Artifact, got {other:?}"),
        }
    }
}
