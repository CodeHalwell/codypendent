//! Commands: client requests for state changes.
//!
//! A [`Command`] carries an `idempotency_key` so a duplicate delivery produces
//! exactly one effect (the daemon records the first application and replays its
//! recorded result on a repeat — STEP 1.3). Commands request change; the daemon
//! decides, persists, and only then emits the resulting events.

use serde::{Deserialize, Serialize};

use crate::handshake::{ClientRole, Subscription};
use crate::ids::{ApprovalId, CommandId, RunId, SessionId, WorkspaceId};
use crate::run::{AgentMode, ApprovalDecision, ApprovalScope};

/// An idempotent, optionally revision-guarded request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub command_id: CommandId,
    /// Client-chosen key; the same key must never apply twice.
    pub idempotency_key: String,
    /// Optimistic-concurrency guard: apply only if the session is still at this
    /// revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    pub body: CommandBody,
}

/// The specific change a command requests. A wire enum: internally tagged with
/// an [`CommandBody::Unknown`] fallback so a command from a newer client
/// deserializes and is rejected structurally rather than crashing the peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum CommandBody {
    CreateSession {
        workspace: WorkspaceId,
        title: String,
    },
    AttachSession {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_seen_sequence: Option<u64>,
        subscriptions: Vec<Subscription>,
        requested_role: ClientRole,
    },
    SubmitUserInput {
        session_id: SessionId,
        text: String,
        mode: AgentMode,
    },
    StartRun {
        session_id: SessionId,
        objective: String,
        mode: AgentMode,
        /// The canonical filesystem root of the repository this run operates on.
        /// A per-user daemon can serve several checkouts over one socket, so the
        /// run — not the daemon's startup working directory — must decide which
        /// repository its context map and curated memories are attributed to
        /// (issue #6 item 1). `#[serde(default)]` keeps an older client (which
        /// sends none) working: the daemon then falls back to its own directory,
        /// exactly as before this field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository: Option<String>,
    },
    ResolveApproval {
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        scope: ApprovalScope,
    },
    CancelRun {
        run_id: RunId,
    },
    PauseRun {
        run_id: RunId,
    },
    ResumeRun {
        run_id: RunId,
    },
    QueueSteering {
        run_id: RunId,
        text: String,
    },
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(body: CommandBody) {
        let command = Command {
            command_id: CommandId::new(),
            idempotency_key: "idem-1".to_string(),
            expected_revision: Some(7),
            body,
        };
        let json = serde_json::to_string(&command).expect("serialize");
        let parsed: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(command, parsed);
    }

    #[test]
    fn start_run_repository_is_omitted_when_absent_and_reparses_to_none() {
        // The per-run repository (issue #6 item 1) is optional on the wire: a
        // client that sends none produces JSON without the key, and such a
        // payload (also what an older client emits) parses back to `None` so the
        // daemon falls back to its own directory.
        let body = CommandBody::StartRun {
            session_id: SessionId::new(),
            objective: "diagnose".to_string(),
            mode: AgentMode::Build,
            repository: None,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(
            !json.contains("repository"),
            "an absent repository is skipped on the wire: {json}"
        );
        let parsed: CommandBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, body, "a payload without the key defaults to None");
    }

    #[test]
    fn every_command_body_round_trips() {
        round_trip(CommandBody::CreateSession {
            workspace: WorkspaceId::new(),
            title: "fix the failing test".to_string(),
        });
        round_trip(CommandBody::AttachSession {
            session_id: SessionId::new(),
            last_seen_sequence: Some(42),
            subscriptions: vec![Subscription::SessionSummary],
            requested_role: ClientRole::Contributor,
        });
        round_trip(CommandBody::SubmitUserInput {
            session_id: SessionId::new(),
            text: "try again".to_string(),
            mode: AgentMode::Build,
        });
        round_trip(CommandBody::StartRun {
            session_id: SessionId::new(),
            objective: "diagnose the failing test".to_string(),
            mode: AgentMode::Build,
            repository: Some("/home/user/project".to_string()),
        });
        round_trip(CommandBody::ResolveApproval {
            approval_id: ApprovalId::new(),
            decision: ApprovalDecision::Approve,
            scope: ApprovalScope::Run,
        });
        round_trip(CommandBody::CancelRun {
            run_id: RunId::new(),
        });
        round_trip(CommandBody::PauseRun {
            run_id: RunId::new(),
        });
        round_trip(CommandBody::ResumeRun {
            run_id: RunId::new(),
        });
        round_trip(CommandBody::QueueSteering {
            run_id: RunId::new(),
            text: "focus on the parser".to_string(),
        });
    }

    #[test]
    fn attach_session_omits_absent_sequence() {
        let command = Command {
            command_id: CommandId::new(),
            idempotency_key: "idem-2".to_string(),
            expected_revision: None,
            body: CommandBody::AttachSession {
                session_id: SessionId::new(),
                last_seen_sequence: None,
                subscriptions: vec![],
                requested_role: ClientRole::Observer,
            },
        };
        let json = serde_json::to_string(&command).expect("serialize");
        assert!(!json.contains("last_seen_sequence"));
        assert!(!json.contains("expected_revision"));
        let parsed: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(command, parsed);
    }

    #[test]
    fn unknown_command_tag_deserializes_to_unknown() {
        let parsed: CommandBody = serde_json::from_value(
            serde_json::json!({ "type": "TeleportRepository", "coords": [1, 2] }),
        )
        .expect("unknown tag must parse, not error");
        assert!(matches!(parsed, CommandBody::Unknown));
    }
}
