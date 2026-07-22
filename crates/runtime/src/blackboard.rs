//! The blackboard write/read seam for the agent loop (Phase 5 STEP 5.3).
//!
//! The `blackboard.post` / `blackboard.query` tools let a workflow agent read and
//! write its run's typed artifact channel. The authoritative store lives in
//! `codypendent-workflow` (`BlackboardStore`, over the SQLite pool), which this
//! crate cannot name — `sqlx` is not a dependency (ADR-009) and neither is the
//! workflow crate. So, exactly as the tool layer reaches the artifact store through
//! the pool-erased [`ArtifactSink`](crate::tools::ArtifactSink) and the loop reaches
//! the ledger through the [`RunJournal`](crate::agent::RunJournal), the loop reaches
//! the blackboard through this trait: the `codypendentd` assembly implements it over
//! a real `BlackboardStore` + pool + the daemon's per-run fan-out hub, and injects it
//! into the runtime (see [`FrameworkAgentRuntime::with_blackboard`]).
//!
//! The seam is **workflow-type-erased**: a kind is a plain string (the assembly
//! parses it against `BlackboardKind`), and payload/author/evidence ride as opaque
//! JSON — so this crate stays decoupled from the workflow domain types. It returns
//! the protocol [`BlackboardItemView`] (which this crate *can* name) so a posted or
//! queried item is described once, wire-ready.
//!
//! [`FrameworkAgentRuntime::with_blackboard`]: crate::agent::FrameworkAgentRuntime::with_blackboard

use async_trait::async_trait;
use codypendent_protocol::BlackboardItemView;
use serde_json::Value;

/// An artifact an agent asks to post (or supersede) on its run's board. The
/// `author` is built **server-side** by the runtime from the run context, never
/// from model-supplied identity — the tool overwrites whatever the model sent.
#[derive(Debug, Clone)]
pub struct BlackboardPost {
    /// The artifact kind (`finding`, `decision`, …) — the assembly validates it.
    pub kind: String,
    /// The artifact body (opaque JSON).
    pub payload: Value,
    /// Attribution built from the authoring node's run context
    /// (`{role, run_id, node_id, workflow_run_id}`).
    pub author: Value,
    /// The author's confidence in `[0, 1]`, if given.
    pub confidence: Option<f64>,
    /// Evidence references grounding the artifact. Claim-like kinds require at
    /// least one; the store enforces it and the refusal surfaces to the agent.
    pub evidence: Vec<Value>,
    /// When set, this post *supersedes* the identified prior item (a correction):
    /// the store posts the replacement at the next revision and stamps the old one.
    pub supersedes: Option<String>,
}

/// A structured blackboard failure, mapped by the assembly from the store's error.
///
/// Every variant carries a stable dotted [`code`](BlackboardChannelError::code) and
/// a legible `Display`, so the tool can feed the reason back to the agent as a
/// **correctable** observation — most importantly [`EvidenceRequired`], which the
/// agent fixes by re-posting with evidence.
///
/// [`EvidenceRequired`]: BlackboardChannelError::EvidenceRequired
#[derive(Debug, thiserror::Error)]
pub enum BlackboardChannelError {
    /// A claim-like artifact was posted without evidence — the agent should retry
    /// with at least one evidence reference.
    #[error("a {0} must carry at least one evidence reference — retry with evidence")]
    EvidenceRequired(String),
    /// The item to supersede does not exist on this run's board.
    #[error("no such blackboard item to supersede: {0}")]
    NotFound(String),
    /// The item to supersede was already superseded by a concurrent correction.
    #[error("blackboard item {0} has already been superseded")]
    AlreadySuperseded(String),
    /// The posted/queried kind is not a known blackboard artifact kind.
    #[error("`{0}` is not a known blackboard artifact kind")]
    UnknownKind(String),
    /// The blackboard is not available for this run (no channel, or not a workflow
    /// run) — the tool should not have been offered.
    #[error("the blackboard is not available for this run")]
    Unavailable,
    /// An underlying store/backend failure (surfaced without leaking internals).
    #[error("blackboard backend error: {0}")]
    Backend(String),
}

impl BlackboardChannelError {
    /// A stable, dotted machine code for this failure, for a `ToolCompleted`
    /// payload's `Failed` message.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            BlackboardChannelError::EvidenceRequired(_) => "blackboard.evidence-required",
            BlackboardChannelError::NotFound(_) => "blackboard.item-not-found",
            BlackboardChannelError::AlreadySuperseded(_) => "blackboard.already-superseded",
            BlackboardChannelError::UnknownKind(_) => "blackboard.unknown-kind",
            BlackboardChannelError::Unavailable => "blackboard.unavailable",
            BlackboardChannelError::Backend(_) => "blackboard.backend-error",
        }
    }
}

/// The pool-erased seam the agent loop posts to and queries the run's board
/// through. Implemented by the `codypendentd` assembly over a real
/// `BlackboardStore` + pool + the daemon's per-run fan-out hub.
#[async_trait]
pub trait BlackboardChannel: Send + Sync {
    /// Post (or supersede) an artifact on `workflow_run_id`'s board, returning the
    /// stored item's view. A successful post is fanned out to the run's
    /// subscribers by the implementation.
    async fn post(
        &self,
        workflow_run_id: &str,
        post: BlackboardPost,
    ) -> Result<BlackboardItemView, BlackboardChannelError>;

    /// Query `workflow_run_id`'s board, optionally filtered by `kind`; superseded
    /// items are excluded unless `include_superseded`. Newest first.
    async fn query(
        &self,
        workflow_run_id: &str,
        kind: Option<String>,
        include_superseded: bool,
    ) -> Result<Vec<BlackboardItemView>, BlackboardChannelError>;
}
