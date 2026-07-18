//! Durable session events.
//!
//! Events record accepted state changes or observations. They are persisted
//! in the event ledger before any client observes them, and original events
//! are immutable evidence (invariant 5). The Phase 0 seed (session lifecycle)
//! is joined here by the Phase 1 run, model, tool, approval, patch, steering,
//! and budget events. Bulk content is never inlined — events reference an
//! [`ArtifactRef`] instead.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactRef;
use crate::handshake::ClientRole;
use crate::ids::{
    AgentId, ApprovalId, ChangeSetId, ClientId, CommandId, CorrelationId, ModelId, RunId, UserId,
};
use crate::run::{
    AgentMode, ApprovalDecision, BudgetDimension, ProposedAction, Risk, RunDisposition, RunState,
    ToolOutcome,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub sequence: u64,
    pub occurred_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<CommandId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<CorrelationId>,
    pub actor: Actor,
    pub body: EventBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Actor {
    Human {
        user_id: UserId,
    },
    Agent {
        agent_id: AgentId,
        run_id: RunId,
        model: ModelId,
    },
    Client {
        client_id: ClientId,
    },
    Integration {
        integration_id: String,
    },
    System,
}

/// The body of a persisted event.
///
/// Internally tagged with a `#[serde(other)] Unknown` fallback (RULE 1): an
/// event type produced by a newer daemon deserializes to `Unknown` in an older
/// client instead of failing the whole frame, and the client renders an
/// "unsupported item" placeholder. Phase 0 variants are preserved so old ledger
/// bytes parse forever.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum EventBody {
    // --- Phase 0: session lifecycle ---
    SessionCreated {
        title: String,
    },
    NoteAppended {
        text: String,
        /// The run this note belongs to, when it is run-scoped (a run's context
        /// manifest or a curated-memory note). `None` for a session-level note
        /// (e.g. user input, an effect-reconciliation record), which a client
        /// attaches to whatever run is in focus. Without this, a run's note could
        /// land on the wrong transcript when runs interleave (issue #6 item 3).
        /// `#[serde(default)]` keeps old ledger bytes (which have no `run_id`)
        /// parsing to `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<RunId>,
    },
    SessionClosed,

    // --- Phase 1: run lifecycle and agent activity ---
    RunStarted {
        run_id: RunId,
        objective: String,
        mode: AgentMode,
    },
    RunStateChanged {
        run_id: RunId,
        state: RunState,
    },
    ModelStreamDelta {
        run_id: RunId,
        text: String,
    },
    ToolProposed {
        run_id: RunId,
        approval_id: ApprovalId,
        action: ProposedAction,
    },
    ToolStarted {
        run_id: RunId,
        /// Tool name, e.g. `shell.run`.
        tool: String,
        /// Digest of the tool arguments (not the arguments themselves).
        args_digest: String,
    },
    ToolCompleted {
        run_id: RunId,
        tool: String,
        outcome: ToolOutcome,
        /// Bulk output, if any, as an artifact reference.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact: Option<ArtifactRef>,
    },
    PatchProposed {
        run_id: RunId,
        changeset_id: ChangeSetId,
        /// The patch/diff, stored as an artifact.
        artifact: ArtifactRef,
    },
    ApprovalRequested {
        approval_id: ApprovalId,
        action: ProposedAction,
        risk: Risk,
    },
    ApprovalResolved {
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    },
    SteeringQueued {
        run_id: RunId,
    },
    SteeringApplied {
        run_id: RunId,
    },
    BudgetWarning {
        run_id: RunId,
        dimension: BudgetDimension,
        used: u64,
        limit: u64,
    },
    RunCompleted {
        run_id: RunId,
        disposition: RunDisposition,
        /// The run chronicle, stored as a JSON artifact.
        chronicle: ArtifactRef,
    },

    /// A client attached to or detached from the session (Phase 3 STEP 3.7).
    /// Emitted so every attached client can show who else is present — e.g. the
    /// TUI showing that VS Code has joined the same session during a handoff.
    ClientPresenceChanged {
        client_id: ClientId,
        role: ClientRole,
        /// `true` when the client attached, `false` when it detached.
        present: bool,
    },

    /// Forward-compatibility fallback for an event type this build does not
    /// know (RULE 1). Receivers render a placeholder and continue.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::DataClassification;
    use crate::ids::ArtifactId;
    use crate::run::{ApprovalDecision, BudgetDimension, RiskLevel};

    fn artifact_ref() -> ArtifactRef {
        ArtifactRef {
            id: ArtifactId::new(),
            media_type: "text/x-diff".to_string(),
            byte_length: 128,
            sha256: "0".repeat(64),
            sensitivity: DataClassification::Internal,
        }
    }

    fn event_with(body: EventBody) -> SessionEvent {
        SessionEvent {
            sequence: 9,
            occurred_at: Utc::now(),
            causation_id: Some(CommandId::new()),
            correlation_id: Some(CorrelationId::new()),
            actor: Actor::Agent {
                agent_id: AgentId::new(),
                run_id: RunId::new(),
                model: ModelId("gpt-5.1-codex".to_string()),
            },
            body,
        }
    }

    fn round_trip(body: EventBody) {
        let event = event_with(body);
        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: SessionEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, parsed);
    }

    #[test]
    fn every_phase1_event_body_round_trips() {
        let run_id = RunId::new();
        round_trip(EventBody::RunStarted {
            run_id,
            objective: "diagnose".to_string(),
            mode: AgentMode::Build,
        });
        round_trip(EventBody::RunStateChanged {
            run_id,
            state: RunState::Running,
        });
        round_trip(EventBody::ModelStreamDelta {
            run_id,
            text: "thinking...".to_string(),
        });
        round_trip(EventBody::ToolProposed {
            run_id,
            approval_id: ApprovalId::new(),
            action: ProposedAction::ExecuteCommand {
                program: "cargo".to_string(),
                args: vec!["test".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
        });
        round_trip(EventBody::ToolStarted {
            run_id,
            tool: "shell.run".to_string(),
            args_digest: "abc123".to_string(),
        });
        round_trip(EventBody::ToolCompleted {
            run_id,
            tool: "shell.run".to_string(),
            outcome: ToolOutcome::Succeeded,
            artifact: Some(artifact_ref()),
        });
        round_trip(EventBody::ToolCompleted {
            run_id,
            tool: "workspace.read_file".to_string(),
            outcome: ToolOutcome::Succeeded,
            artifact: None,
        });
        round_trip(EventBody::PatchProposed {
            run_id,
            changeset_id: ChangeSetId::new(),
            artifact: artifact_ref(),
        });
        round_trip(EventBody::ApprovalRequested {
            approval_id: ApprovalId::new(),
            action: ProposedAction::GitCommit {
                repository: "acme/widget".to_string(),
            },
            risk: Risk {
                level: RiskLevel::Medium,
                reasons: vec![],
            },
        });
        round_trip(EventBody::ApprovalResolved {
            approval_id: ApprovalId::new(),
            decision: ApprovalDecision::Approve,
        });
        round_trip(EventBody::SteeringQueued { run_id });
        round_trip(EventBody::SteeringApplied { run_id });
        round_trip(EventBody::BudgetWarning {
            run_id,
            dimension: BudgetDimension::Tokens,
            used: 90_000,
            limit: 100_000,
        });
        round_trip(EventBody::RunCompleted {
            run_id,
            disposition: RunDisposition::Completed {
                summary: Some("fixed".to_string()),
            },
            chronicle: artifact_ref(),
        });
        round_trip(EventBody::ClientPresenceChanged {
            client_id: crate::ids::ClientId::new(),
            role: crate::handshake::ClientRole::Contributor,
            present: true,
        });
    }

    #[test]
    fn tool_completed_omits_absent_artifact() {
        let event = event_with(EventBody::ToolCompleted {
            run_id: RunId::new(),
            tool: "workspace.search".to_string(),
            outcome: ToolOutcome::Succeeded,
            artifact: None,
        });
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(!json.contains("artifact"));
    }

    #[test]
    fn unknown_event_tag_deserializes_to_unknown() {
        // Mirror the Phase 0 `Payload` unknown-tag test at the event layer: a
        // future event type must deserialize to `Unknown`, not error the frame.
        let mut value = serde_json::to_value(event_with(EventBody::SessionClosed)).expect("value");
        value["body"] = serde_json::json!({ "type": "QuantumEvent", "spin": "up" });
        let parsed: SessionEvent =
            serde_json::from_value(value).expect("future events must parse, not error");
        assert!(matches!(parsed.body, EventBody::Unknown));
    }

    /// The exact Phase 0 bytes from `crates/test-support/fixtures/events-basic.jsonl`.
    /// Embedded as a literal so this test never depends on the test-support crate
    /// (that would create a dependency cycle). Old event bytes must parse forever.
    const PHASE0_FIXTURE_JSONL: &str = r#"{"sequence":1,"occurred_at":"2026-07-14T09:00:00Z","actor":{"type":"System"},"body":{"type":"SessionCreated","title":"fixture session"}}
{"sequence":2,"occurred_at":"2026-07-14T09:00:05Z","actor":{"type":"Human","user_id":"dana"},"body":{"type":"NoteAppended","text":"first note"}}
{"sequence":3,"occurred_at":"2026-07-14T09:00:10Z","actor":{"type":"Human","user_id":"dana"},"body":{"type":"NoteAppended","text":"second note"}}
{"sequence":4,"occurred_at":"2026-07-14T09:00:15Z","actor":{"type":"System"},"body":{"type":"SessionClosed"}}"#;

    #[test]
    fn phase0_fixture_bytes_still_deserialize() {
        let events: Vec<SessionEvent> = PHASE0_FIXTURE_JSONL
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("Phase 0 event must parse forever"))
            .collect();

        assert_eq!(events.len(), 4);
        assert_eq!(events[0].sequence, 1);
        assert!(matches!(events[0].body, EventBody::SessionCreated { .. }));
        assert!(matches!(events[1].body, EventBody::NoteAppended { .. }));
        assert!(matches!(events[2].body, EventBody::NoteAppended { .. }));
        assert!(matches!(events[3].body, EventBody::SessionClosed));
        assert!(matches!(events[3].actor, Actor::System));
    }
}
