//! Run-domain wire types: modes, states, actions, approvals, budgets.
//!
//! These describe an agent run as it crosses the wire in commands and events.
//! Every enum here is internally tagged (`#[serde(tag = "type")]`) and carries a
//! `#[serde(other)] Unknown` fallback, so a value produced by a newer peer
//! deserializes to `Unknown` rather than failing the whole frame.

use serde::{Deserialize, Serialize};

use crate::ids::ArtifactId;

/// A mode preset: a bundle of policy and interaction defaults, not merely a
/// prompt (Chapter 20). Modes are enforced by the policy engine — an `Explore`
/// run proposing a write is denied regardless of what the model says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum AgentMode {
    /// Explain, answer, retrieve. Writes and commands denied.
    Ask,
    /// Investigate the repository. Read-only tools; writes denied.
    Explore,
    /// Produce an execution plan. May write plan artifacts only.
    Plan,
    /// Implement approved work in the worktree write scope.
    Build,
    /// Inspect code or a change set. Read plus comment.
    Review,
    #[serde(other)]
    Unknown,
}

/// The lifecycle state of a run (Chapter 04). Transitions are persisted before
/// they are exposed to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum RunState {
    Queued,
    Preparing,
    Running,
    WaitingForApproval,
    WaitingForUserInput,
    Paused,
    Recovering,
    Completed,
    Failed,
    Cancelled,
    #[serde(other)]
    Unknown,
}

/// The terminal outcome of a run, carried by `RunCompleted`.
///
/// Chapter 04 names the terminal `RunState`s but leaves the disposition detail
/// open at Phase 1; this is the minimal reasonable shape — the terminal kind
/// plus a short human-readable summary or reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum RunDisposition {
    Completed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    Failed {
        reason: String,
    },
    Cancelled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

/// A side-effecting action an agent proposes, pending policy evaluation and
/// possibly approval.
///
/// This started as the Phase 1 minimal subset of the Chapter 14 shape; Phase 3
/// adds `GitHubMutation` for remote GitHub writes. Further variants
/// (`InstallPlugin`, structured `CommandRequest` / `NetworkDestination`) arrive
/// in later phases. Paths and destinations are carried as strings on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum ProposedAction {
    ReadFiles {
        paths: Vec<String>,
    },
    WritePatch {
        patch: ArtifactId,
    },
    ExecuteCommand {
        program: String,
        args: Vec<String>,
    },
    NetworkRequest {
        destination: String,
    },
    GitCommit {
        repository: String,
    },
    GitPush {
        remote: String,
        branch: String,
    },
    /// A write to a remote GitHub resource (draft PR, review comment, PR
    /// update, check-run summary) via the GitHub API (Phase 3 STEP 3.1). Every
    /// such write is approval-gated and network-scoped to the GitHub API
    /// endpoint by the policy engine.
    GitHubMutation {
        /// The `owner/repo` slug the mutation targets.
        repository: String,
        /// A short human-readable description of the write, rendered on the
        /// approval card (e.g. `create draft PR on owner/repo`).
        summary: String,
    },
    #[serde(other)]
    Unknown,
}

/// A structured risk assessment attached to a proposed action or approval
/// request. Chapter 14 leaves the exact shape open at Phase 1; this is the
/// minimal reasonable form — a severity level plus human-readable reasons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Risk {
    pub level: RiskLevel,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

/// Severity buckets for a [`Risk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
    #[serde(other)]
    Unknown,
}

/// The decision an approver returns for a proposed action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum ApprovalDecision {
    Approve,
    Reject,
    #[serde(other)]
    Unknown,
}

/// How widely an approval applies (Chapter 04 / STEP 1.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum ApprovalScope {
    /// This single proposal only.
    Once,
    /// Every identical proposal for the remainder of the run.
    Run,
    /// A recorded pattern of similar proposals.
    Pattern,
    /// Any matching proposal in this repository.
    Repository,
    #[serde(other)]
    Unknown,
}

/// Which budget a `BudgetWarning` is about. The unit of the reported
/// `used`/`limit` is implied by the dimension (tokens, minor currency units,
/// seconds, or a count of calls).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum BudgetDimension {
    Tokens,
    Cost,
    WallClock,
    ToolCalls,
    #[serde(other)]
    Unknown,
}

/// The outcome of a completed tool call, carried by `ToolCompleted`.
///
/// Chapter 03 lists tool-completed as an event category without fixing its
/// payload; this is the minimal reasonable shape — success, or failure with a
/// short message. Bulk output travels as an `ArtifactRef`, never here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum ToolOutcome {
    Succeeded,
    Failed {
        message: String,
    },
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a value through JSON and assert it is unchanged.
    fn round_trip<T>(value: T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(&value).expect("serialize");
        let parsed: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(value, parsed);
    }

    #[test]
    fn run_domain_types_round_trip() {
        round_trip(AgentMode::Build);
        round_trip(RunState::WaitingForApproval);
        round_trip(RunDisposition::Completed {
            summary: Some("done".to_string()),
        });
        round_trip(RunDisposition::Failed {
            reason: "daemon restart".to_string(),
        });
        round_trip(RunDisposition::Cancelled { reason: None });
        round_trip(ProposedAction::ExecuteCommand {
            program: "cargo".to_string(),
            args: vec!["test".to_string()],
        });
        round_trip(ProposedAction::WritePatch {
            patch: ArtifactId::new(),
        });
        round_trip(ProposedAction::GitHubMutation {
            repository: "octocat/hello-world".to_string(),
            summary: "create draft PR on octocat/hello-world".to_string(),
        });
        round_trip(Risk {
            level: RiskLevel::High,
            reasons: vec!["writes outside the worktree".to_string()],
        });
        round_trip(ApprovalDecision::Approve);
        round_trip(ApprovalScope::Run);
        round_trip(BudgetDimension::Tokens);
        round_trip(ToolOutcome::Failed {
            message: "exit 1".to_string(),
        });
    }

    #[test]
    fn unknown_tags_deserialize_to_unknown() {
        let future = serde_json::json!({ "type": "FromTheFuture", "extra": 1 });
        assert!(matches!(
            serde_json::from_value::<AgentMode>(future.clone()).expect("mode"),
            AgentMode::Unknown
        ));
        assert!(matches!(
            serde_json::from_value::<RunState>(future.clone()).expect("state"),
            RunState::Unknown
        ));
        assert!(matches!(
            serde_json::from_value::<RunDisposition>(future.clone()).expect("disposition"),
            RunDisposition::Unknown
        ));
        assert!(matches!(
            serde_json::from_value::<ProposedAction>(future.clone()).expect("action"),
            ProposedAction::Unknown
        ));
        assert!(matches!(
            serde_json::from_value::<RiskLevel>(future.clone()).expect("risk"),
            RiskLevel::Unknown
        ));
        assert!(matches!(
            serde_json::from_value::<ApprovalDecision>(future.clone()).expect("decision"),
            ApprovalDecision::Unknown
        ));
        assert!(matches!(
            serde_json::from_value::<ApprovalScope>(future.clone()).expect("scope"),
            ApprovalScope::Unknown
        ));
        assert!(matches!(
            serde_json::from_value::<BudgetDimension>(future.clone()).expect("dimension"),
            BudgetDimension::Unknown
        ));
        assert!(matches!(
            serde_json::from_value::<ToolOutcome>(future).expect("outcome"),
            ToolOutcome::Unknown
        ));
    }
}
