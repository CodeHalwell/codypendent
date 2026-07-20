//! The promotion pipeline (STEP 7.5): nothing promotes itself.
//!
//! Every learnable artifact — retrieval weights, skill versions, prompt policies,
//! routing policies, workflow versions, model execution profiles — is promoted
//! through one auditable pipeline
//! ([Chapter 13](../../docs/docs/13-observability-evaluation-learning.md)):
//!
//! ```text
//! candidate (draft, versioned, attributed)
//! → offline regression suite (must not regress)
//! → shadow run (compare, don't affect)
//! → limited canary (budget-capped, auto-rollback on signal regression)
//! → statistical + safety comparison
//! → HUMAN approval → promotion (version activated)
//! → rollback = normal operation (one command; previous version reactivates)
//! ```
//!
//! **The invariant (ADR-010, exit criterion 2): no self-promotion.** The
//! [`Candidate::approve`] transition requires an [`Actor::Human`] — there is no
//! method on a [`Candidate`] that reaches [`PromotionStage::Promoted`] without
//! one. A grader, an agent, or the canary itself can drive a candidate all the
//! way to `ComparisonReady`, but the last step is a human's alone.

use std::collections::BTreeMap;
use std::fmt;

use codypendent_protocol::events::Actor;
use serde::{Deserialize, Serialize};

/// A class of learnable artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    RetrievalWeights,
    Skill,
    Prompt,
    Router,
    Workflow,
    ModelProfile,
}

impl ArtifactKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::RetrievalWeights => "retrieval",
            ArtifactKind::Skill => "skill",
            ArtifactKind::Prompt => "prompt",
            ArtifactKind::Router => "router",
            ArtifactKind::Workflow => "workflow",
            ArtifactKind::ModelProfile => "model-profile",
        }
    }
}

/// A versioned artifact identity, e.g. `router/tool-selection/12`. This string
/// appears in every trace that used the artifact (exit criterion 4: attributable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactVersion {
    pub kind: ArtifactKind,
    pub name: String,
    pub version: u32,
}

impl ArtifactVersion {
    #[must_use]
    pub fn new(kind: ArtifactKind, name: impl Into<String>, version: u32) -> Self {
        Self {
            kind,
            name: name.into(),
            version,
        }
    }

    /// The identity without the version (`router/tool-selection`), the key under
    /// which versions of one artifact are tracked.
    #[must_use]
    pub fn stem(&self) -> String {
        format!("{}/{}", self.kind.as_str(), self.name)
    }
}

impl fmt::Display for ArtifactVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.kind.as_str(), self.name, self.version)
    }
}

/// Where a candidate sits in the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromotionStage {
    /// Authored, versioned, attributed — not yet evaluated.
    Draft,
    /// Passed the offline regression suite.
    RegressionPassed,
    /// Running in shadow (compared, no production effect).
    Shadow,
    /// Running as a limited, budget-capped canary.
    Canary,
    /// Canary complete and comparison assembled; awaiting human approval.
    ComparisonReady,
    /// Promoted (a human approved it); the version is activatable.
    Promoted,
    /// Rolled back (auto from a canary regression, or a manual rollback).
    RolledBack,
    /// Rejected (regressed offline, or declined).
    Rejected,
}

/// The outcome of a canary observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanaryOutcome {
    /// No regression — the canary continues.
    Continuing,
    /// A signal regression was detected; the candidate auto-rolled back.
    AutoRolledBack,
}

/// A promotion-pipeline failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromotionError {
    /// The **no-self-promotion** guard: only a human may approve a promotion.
    #[error("promotion requires a human approver; {actor} is not permitted to promote")]
    RequiresHumanApproval { actor: &'static str },
    /// The candidate regressed against the offline suite and cannot advance.
    #[error("candidate regressed against the offline suite; it may not be promoted")]
    RegressedOffline,
    /// The transition is not legal from the current stage.
    #[error("cannot {action} a candidate in stage {stage:?}")]
    IllegalTransition {
        action: &'static str,
        stage: PromotionStage,
    },
    /// A synthesized artifact needs permission review before it can be evaluated.
    #[error("candidate needs permission review before evaluation")]
    PermissionReviewRequired,
    /// A version was submitted for activation without a human-approved promotion
    /// receipt — the activation-bypass guard (exit criterion 2).
    #[error(
        "cannot activate a version without a completed promotion (record stage was {stage:?})"
    )]
    NotPromoted { stage: PromotionStage },
}

/// A record of a promotion or rollback, for the audit trail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionRecord {
    pub artifact: ArtifactVersion,
    /// Who performed the action (a human for a promotion).
    pub actor_kind: String,
    /// The stage transitioned into.
    pub stage: PromotionStage,
}

/// A candidate moving through the pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub artifact: ArtifactVersion,
    /// Who authored the candidate (attribution). Non-human authors are fine — a
    /// grader/agent may *draft* a candidate; it just cannot *promote* one.
    pub author_kind: String,
    pub stage: PromotionStage,
    /// Synthesized artifacts (e.g. skills from trace clusters) must pass a
    /// permission review before entering evaluation.
    #[serde(default)]
    pub requires_permission_review: bool,
    #[serde(default)]
    pub permission_reviewed: bool,
}

impl Candidate {
    /// A fresh draft, attributed to its author. A draft can be authored by anyone
    /// (including an agent/grader synthesizing an improvement).
    #[must_use]
    pub fn draft(artifact: ArtifactVersion, author: &Actor) -> Self {
        Self {
            artifact,
            author_kind: actor_kind(author).to_string(),
            stage: PromotionStage::Draft,
            requires_permission_review: false,
            permission_reviewed: false,
        }
    }

    /// Mark that this candidate was synthesized and needs permission review.
    #[must_use]
    pub fn needs_permission_review(mut self) -> Self {
        self.requires_permission_review = true;
        self
    }

    /// Record that a permission review passed (a prerequisite for a synthesized
    /// candidate to be evaluated).
    pub fn mark_permission_reviewed(&mut self) {
        self.permission_reviewed = true;
    }

    /// Run the offline regression suite. `regressed` = did the candidate regress
    /// any historical case. On success advances to `RegressionPassed`; a
    /// regression rejects the candidate (it may not be promoted).
    pub fn run_regression(&mut self, regressed: bool) -> Result<(), PromotionError> {
        self.expect_stage(PromotionStage::Draft, "run-regression")?;
        if self.requires_permission_review && !self.permission_reviewed {
            return Err(PromotionError::PermissionReviewRequired);
        }
        if regressed {
            self.stage = PromotionStage::Rejected;
            return Err(PromotionError::RegressedOffline);
        }
        self.stage = PromotionStage::RegressionPassed;
        Ok(())
    }

    /// Begin the shadow run.
    pub fn start_shadow(&mut self) -> Result<(), PromotionError> {
        self.expect_stage(PromotionStage::RegressionPassed, "start-shadow")?;
        self.stage = PromotionStage::Shadow;
        Ok(())
    }

    /// Begin the limited canary.
    pub fn start_canary(&mut self) -> Result<(), PromotionError> {
        self.expect_stage(PromotionStage::Shadow, "start-canary")?;
        self.stage = PromotionStage::Canary;
        Ok(())
    }

    /// Feed a canary signal observation. A regression **auto-rolls-back** the
    /// candidate immediately (no human needed to *stop* a bad change — only to
    /// *promote* a good one).
    pub fn observe_canary(&mut self, regressed: bool) -> Result<CanaryOutcome, PromotionError> {
        self.expect_stage(PromotionStage::Canary, "observe-canary")?;
        if regressed {
            self.stage = PromotionStage::RolledBack;
            Ok(CanaryOutcome::AutoRolledBack)
        } else {
            Ok(CanaryOutcome::Continuing)
        }
    }

    /// Finish the canary and assemble the comparison — the candidate now awaits a
    /// human decision.
    pub fn finish_canary(&mut self) -> Result<(), PromotionError> {
        self.expect_stage(PromotionStage::Canary, "finish-canary")?;
        self.stage = PromotionStage::ComparisonReady;
        Ok(())
    }

    /// **Approve and promote.** The only path to [`PromotionStage::Promoted`], and
    /// it requires an [`Actor::Human`]. Any other actor — an agent, the system, an
    /// integration — is refused: there is no code path from a non-human to
    /// activation (exit criterion 2). Returns the audit record on success.
    pub fn approve(&mut self, approver: &Actor) -> Result<PromotionRecord, PromotionError> {
        self.expect_stage(PromotionStage::ComparisonReady, "approve")?;
        if !matches!(approver, Actor::Human { .. }) {
            return Err(PromotionError::RequiresHumanApproval {
                actor: actor_kind(approver),
            });
        }
        self.stage = PromotionStage::Promoted;
        Ok(PromotionRecord {
            artifact: self.artifact.clone(),
            actor_kind: actor_kind(approver).to_string(),
            stage: PromotionStage::Promoted,
        })
    }

    /// Manually roll back a promoted candidate.
    pub fn rollback(&mut self) -> Result<PromotionRecord, PromotionError> {
        self.expect_stage(PromotionStage::Promoted, "rollback")?;
        self.stage = PromotionStage::RolledBack;
        Ok(PromotionRecord {
            artifact: self.artifact.clone(),
            actor_kind: "system".to_string(),
            stage: PromotionStage::RolledBack,
        })
    }

    fn expect_stage(
        &self,
        expected: PromotionStage,
        action: &'static str,
    ) -> Result<(), PromotionError> {
        if self.stage == expected {
            Ok(())
        } else {
            Err(PromotionError::IllegalTransition {
                action,
                stage: self.stage,
            })
        }
    }
}

/// The active version of each artifact, with rollback to the predecessor.
/// `codypendent versions rollback <id>` restores the prior version — also traced
/// (exit criterion 4: reversible).
#[derive(Debug, Clone, Default)]
pub struct ActiveVersions {
    /// The activation stack per artifact stem (`router/tool-selection` → [1, 4, 12]).
    history: BTreeMap<String, Vec<u32>>,
}

impl ActiveVersions {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Activate a version, pushing it onto the artifact's stack. Activation
    /// **requires a promotion receipt** — a [`PromotionRecord`] whose stage is
    /// [`PromotionStage::Promoted`], which only [`Candidate::approve`] (an
    /// `Actor::Human`) produces. There is no way to make a bare version active
    /// without such a record, so an agent/system caller cannot activate its own
    /// draft and bypass the human-approval gate (ADR-010, exit criterion 2).
    pub fn activate(&mut self, record: &PromotionRecord) -> Result<(), PromotionError> {
        if record.stage != PromotionStage::Promoted {
            return Err(PromotionError::NotPromoted {
                stage: record.stage,
            });
        }
        self.history
            .entry(record.artifact.stem())
            .or_default()
            .push(record.artifact.version);
        Ok(())
    }

    /// The currently active version of an artifact stem, if any.
    #[must_use]
    pub fn active(&self, stem: &str) -> Option<u32> {
        self.history.get(stem).and_then(|v| v.last().copied())
    }

    /// Roll back to the predecessor version: pop the current version and return
    /// the one now active. Returns `None` if there is no predecessor to restore.
    pub fn rollback(&mut self, stem: &str) -> Option<u32> {
        let stack = self.history.get_mut(stem)?;
        if stack.len() < 2 {
            // Nothing to restore to — a rollback needs a predecessor.
            return None;
        }
        stack.pop();
        stack.last().copied()
    }
}

/// The kind label of an actor, for attribution.
fn actor_kind(actor: &Actor) -> &'static str {
    match actor {
        Actor::Human { .. } => "human",
        Actor::Agent { .. } => "agent",
        Actor::Client { .. } => "client",
        Actor::Integration { .. } => "integration",
        Actor::System => "system",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::ids::{AgentId, ModelId, RunId, UserId};

    fn human() -> Actor {
        Actor::Human {
            user_id: UserId("danielhalwell".into()),
        }
    }

    fn agent() -> Actor {
        Actor::Agent {
            agent_id: AgentId::new(),
            run_id: RunId::new(),
            model: ModelId("claude-sonnet-5".into()),
        }
    }

    fn artifact() -> ArtifactVersion {
        ArtifactVersion::new(ArtifactKind::Router, "tool-selection", 12)
    }

    /// A promotion receipt for a version — as `approve()` would produce for a
    /// human-approved candidate. Used to activate a version in tests.
    fn promoted(version: u32) -> PromotionRecord {
        PromotionRecord {
            artifact: ArtifactVersion::new(ArtifactKind::Router, "tool-selection", version),
            actor_kind: "human".into(),
            stage: PromotionStage::Promoted,
        }
    }

    fn drive_to_comparison(c: &mut Candidate) {
        c.run_regression(false).unwrap();
        c.start_shadow().unwrap();
        c.start_canary().unwrap();
        assert_eq!(c.observe_canary(false).unwrap(), CanaryOutcome::Continuing);
        c.finish_canary().unwrap();
        assert_eq!(c.stage, PromotionStage::ComparisonReady);
    }

    #[test]
    fn artifact_version_renders_the_registry_id() {
        assert_eq!(artifact().to_string(), "router/tool-selection/12");
        assert_eq!(artifact().stem(), "router/tool-selection");
    }

    #[test]
    fn a_human_can_promote_through_the_full_pipeline() {
        let mut c = Candidate::draft(artifact(), &human());
        drive_to_comparison(&mut c);
        let record = c.approve(&human()).unwrap();
        assert_eq!(c.stage, PromotionStage::Promoted);
        assert_eq!(record.actor_kind, "human");
        assert_eq!(record.artifact.to_string(), "router/tool-selection/12");
    }

    #[test]
    fn an_agent_cannot_promote_itself() {
        // The exit-criterion-2 test: drive a candidate all the way to the decision
        // point, then have an AGENT try to approve. It must fail structurally.
        let mut c = Candidate::draft(artifact(), &agent());
        drive_to_comparison(&mut c);
        let err = c.approve(&agent()).unwrap_err();
        assert!(matches!(
            err,
            PromotionError::RequiresHumanApproval { actor: "agent" }
        ));
        // The candidate did NOT promote.
        assert_eq!(c.stage, PromotionStage::ComparisonReady);
    }

    #[test]
    fn system_and_integration_actors_also_cannot_promote() {
        for actor in [
            Actor::System,
            Actor::Integration {
                integration_id: "ci".into(),
            },
        ] {
            let mut c = Candidate::draft(artifact(), &human());
            drive_to_comparison(&mut c);
            assert!(matches!(
                c.approve(&actor),
                Err(PromotionError::RequiresHumanApproval { .. })
            ));
        }
    }

    #[test]
    fn there_is_no_path_to_promoted_that_skips_approval() {
        // Every transition method is exercised; none but approve() reaches Promoted.
        let mut c = Candidate::draft(artifact(), &agent());
        c.run_regression(false).unwrap();
        assert_ne!(c.stage, PromotionStage::Promoted);
        c.start_shadow().unwrap();
        assert_ne!(c.stage, PromotionStage::Promoted);
        c.start_canary().unwrap();
        assert_ne!(c.stage, PromotionStage::Promoted);
        c.observe_canary(false).unwrap();
        assert_ne!(c.stage, PromotionStage::Promoted);
        c.finish_canary().unwrap();
        assert_ne!(c.stage, PromotionStage::Promoted, "only approve() promotes");
    }

    #[test]
    fn an_offline_regression_rejects_the_candidate() {
        let mut c = Candidate::draft(artifact(), &human());
        let err = c.run_regression(true).unwrap_err();
        assert_eq!(err, PromotionError::RegressedOffline);
        assert_eq!(c.stage, PromotionStage::Rejected);
    }

    #[test]
    fn a_canary_regression_auto_rolls_back_without_a_human() {
        let mut c = Candidate::draft(artifact(), &human());
        c.run_regression(false).unwrap();
        c.start_shadow().unwrap();
        c.start_canary().unwrap();
        // A regression signal during canary rolls back immediately — stopping a bad
        // change needs no human, only promoting a good one does.
        assert_eq!(
            c.observe_canary(true).unwrap(),
            CanaryOutcome::AutoRolledBack
        );
        assert_eq!(c.stage, PromotionStage::RolledBack);
    }

    #[test]
    fn a_synthesized_candidate_needs_permission_review_first() {
        let mut c = Candidate::draft(
            ArtifactVersion::new(ArtifactKind::Skill, "rust-ci", 4),
            &agent(),
        )
        .needs_permission_review();
        // Without review, it cannot enter evaluation.
        assert_eq!(
            c.run_regression(false),
            Err(PromotionError::PermissionReviewRequired)
        );
        // After review, it proceeds.
        c.mark_permission_reviewed();
        assert!(c.run_regression(false).is_ok());
    }

    #[test]
    fn rollback_restores_the_predecessor_version() {
        let mut active = ActiveVersions::new();
        active.activate(&promoted(11)).unwrap();
        active.activate(&promoted(12)).unwrap();
        assert_eq!(active.active("router/tool-selection"), Some(12));
        // One command restores the predecessor.
        let restored = active.rollback("router/tool-selection");
        assert_eq!(restored, Some(11));
        assert_eq!(active.active("router/tool-selection"), Some(11));
    }

    #[test]
    fn rollback_without_a_predecessor_is_a_noop() {
        let mut active = ActiveVersions::new();
        active.activate(&promoted(12)).unwrap();
        assert_eq!(active.rollback("router/tool-selection"), None);
        assert_eq!(active.active("router/tool-selection"), Some(12));
    }

    #[test]
    fn activation_requires_a_promoted_record() {
        // The exit-criterion-2 activation-bypass guard: a record that is NOT a
        // completed human promotion (e.g. a rollback receipt, or a fabricated
        // non-promoted record) cannot make a version active.
        let mut active = ActiveVersions::new();
        let not_promoted = PromotionRecord {
            artifact: ArtifactVersion::new(ArtifactKind::Router, "tool-selection", 99),
            actor_kind: "agent".into(),
            stage: PromotionStage::ComparisonReady,
        };
        let err = active.activate(&not_promoted).unwrap_err();
        assert!(matches!(err, PromotionError::NotPromoted { .. }));
        assert_eq!(
            active.active("router/tool-selection"),
            None,
            "nothing was activated"
        );
    }

    #[test]
    fn a_human_approval_record_activates() {
        // The genuine path: approve() (a human) produces a Promoted record, which
        // is exactly what activate() accepts.
        let mut c = Candidate::draft(artifact(), &human());
        drive_to_comparison(&mut c);
        let record = c.approve(&human()).unwrap();
        let mut active = ActiveVersions::new();
        active.activate(&record).unwrap();
        assert_eq!(active.active("router/tool-selection"), Some(12));
    }

    #[test]
    fn transitions_are_stage_guarded() {
        let mut c = Candidate::draft(artifact(), &human());
        // Cannot approve straight from Draft.
        assert!(matches!(
            c.approve(&human()),
            Err(PromotionError::IllegalTransition {
                action: "approve",
                ..
            })
        ));
    }
}
