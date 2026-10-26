//! Memory seam (dependency inversion), outcome 17.
//!
//! `InspectMemory`/`CorrectMemory`/`ForgetMemory`/`ForgetMemoryScope`/
//! `OpenMemoryEvidence` act on the curated-memory store, which lives *outside*
//! the session ledger and inside `codypendent-knowledge` — a crate this one
//! cannot name (the daemon is below knowledge in the graph). So, exactly like
//! [`PromotionGateway`](crate::promotion::PromotionGateway), the daemon
//! declares the seam and the `codypendentd` assembly fills it. The default-`None`
//! [`ServerState::memory`](crate::server::ServerState::memory) leaves it
//! unwired, and the lib-only / test server then rejects every memory command
//! with `memory.transport-unavailable`.
//!
//! # What this seam does NOT decide
//!
//! Scope visibility. The implementation resolves `repository` to a repository
//! identity with the daemon's own single source of truth and asks
//! `codypendentd::memory_ops`, whose whole job is refusing an out-of-scope
//! target **identically** to an absent one. Nothing on the wire lets a caller
//! name a scope key, so the set of scopes a command can reach is derived, never
//! supplied — the same shape as [`ApprovePromotionRequest`'s
//! actor](crate::promotion::ApprovePromotionRequest).

use std::future::Future;
use std::pin::Pin;

use codypendent_protocol::{CodypendentError, MemoryEvidence, MemoryId, MemoryScopeTier};

/// Read one memory. `repository` is the checkout whose scopes are in view.
#[derive(Debug, Clone)]
pub struct InspectMemoryRequest {
    pub id: MemoryId,
    pub repository: String,
}

/// Replace a memory's statement with a corrected one. The store supersedes
/// rather than overwrites; the implementation supplies the correction's own
/// evidence (the edit action itself), never the caller.
#[derive(Debug, Clone)]
pub struct CorrectMemoryRequest {
    pub id: MemoryId,
    pub repository: String,
    pub statement: String,
    pub structured_value: Option<serde_json::Value>,
    pub confidence: f32,
}

/// Remove one memory, or every memory in one visible scope tier.
#[derive(Debug, Clone)]
pub struct ForgetMemoryRequest {
    /// `Some(id)` forgets exactly that memory; `None` forgets the whole `tier`.
    pub id: Option<MemoryId>,
    pub repository: String,
    /// Only read when `id` is `None`.
    pub tier: MemoryScopeTier,
}

/// Fetch the content behind one of a memory's evidence refs.
#[derive(Debug, Clone)]
pub struct OpenMemoryEvidenceRequest {
    pub id: MemoryId,
    pub repository: String,
    /// A position in [`MemoryView::evidence`](codypendent_protocol::MemoryView::evidence).
    pub evidence_index: u32,
}

/// The future the read/write methods returning a record share. Boxed so the
/// trait stays object-safe without an `async-trait` dependency, matching
/// [`PromotionProposeFuture`](crate::promotion::PromotionProposeFuture).
pub type MemoryViewFuture<'a> = Pin<
    Box<
        dyn Future<Output = Result<codypendent_protocol::MemoryView, CodypendentError>> + Send + 'a,
    >,
>;

/// The future a forget returns: the ids actually removed.
pub type MemoryForgetFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<MemoryId>, CodypendentError>> + Send + 'a>>;

/// The future an evidence fetch returns.
pub type MemoryEvidenceFuture<'a> =
    Pin<Box<dyn Future<Output = Result<MemoryEvidence, CodypendentError>> + Send + 'a>>;

pub struct ReadKnowledgeRequest {
    pub session_id: codypendent_protocol::SessionId,
    pub section: codypendent_protocol::knowledge::KnowledgeSection,
    pub offset: u32,
    pub limit: u32,
}
pub type KnowledgePageFuture<'a> = Pin<
    Box<
        dyn Future<
                Output = Result<codypendent_protocol::knowledge::KnowledgePage, CodypendentError>,
            > + Send
            + 'a,
    >,
>;

/// The daemon's seam for the curated-memory store (outcome 17).
///
/// Every method surfaces the underlying store error verbatim as a
/// `CommandRejected`. A target the caller may not see and a target that does
/// not exist must both surface as `memory.not-found` — the implementation is
/// where that collapse happens, and it is the whole point of the seam.
pub trait MemoryGateway: Send + Sync {
    fn read_knowledge(&self, _request: ReadKnowledgeRequest) -> KnowledgePageFuture<'_> {
        Box::pin(async {
            Err(CodypendentError::new(
                "knowledge.transport-unavailable",
                "Knowledge browsing is not enabled on this daemon",
                false,
            ))
        })
    }
    fn inspect(&self, request: InspectMemoryRequest) -> MemoryViewFuture<'_>;
    fn correct(&self, request: CorrectMemoryRequest) -> MemoryViewFuture<'_>;
    fn forget(&self, request: ForgetMemoryRequest) -> MemoryForgetFuture<'_>;
    fn open_evidence(&self, request: OpenMemoryEvidenceRequest) -> MemoryEvidenceFuture<'_>;
}
