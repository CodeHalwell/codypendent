//! Attach-time catch-up: missed events or a projection snapshot.
//!
//! When a client attaches (or reconnects), the daemon replies with a
//! [`Catchup`] (Chapter 03): if the client is at most ~500 events behind it
//! receives the missed [`SessionEvent`]s directly, otherwise a compact
//! [`SessionProjection`] snapshot it can render immediately and then live-tail.

use serde::{Deserialize, Serialize};

use crate::events::SessionEvent;
use crate::ids::{ApprovalId, RunId, SessionId};
use crate::{PendingPromptView, ProposedAction, Risk};

/// The daemon's answer to an attach: replay or snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum Catchup {
    /// The client was close enough to replay the gap event-by-event.
    Events {
        from: u64,
        through: u64,
        events: Vec<SessionEvent>,
    },
    /// The client was too far behind; here is a snapshot as of `through`.
    Snapshot {
        through: u64,
        projection: SessionProjection,
    },
    #[serde(other)]
    Unknown,
}

/// A compact summary of session state sent in place of a long event history.
///
/// Chapter 03 references a `SessionProjection` without fixing its fields; this
/// is the minimal reasonable Phase 1 shape — enough for a reconnecting client to
/// render a session's identity and live runs before it resumes live-tailing.
/// Richer per-view projections arrive with their subscriptions.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct SessionProjection {
    pub session_id: SessionId,
    pub title: String,
    /// The highest event sequence folded into this snapshot.
    pub last_sequence: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_runs: Vec<RunId>,
    /// Approvals which are still actionable at the snapshot watermark. A
    /// compacted catch-up must preserve workflow state, not merely run ids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_approvals: Vec<PendingApprovalProjection>,
    /// Pending queued prompts at the snapshot watermark, so a >500-event
    /// catch-up still shows the queue (mirrors `pending_approvals`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_prompts: Vec<PendingPromptView>,
    pub closed: bool,
}

/// The actionable part of a pending approval carried in a compact snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct PendingApprovalProjection {
    pub approval_id: ApprovalId,
    pub run_id: RunId,
    pub action: ProposedAction,
    pub risk: Risk,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Actor, EventBody, SessionEvent};
    use chrono::Utc;

    fn sample_event() -> SessionEvent {
        SessionEvent {
            sequence: 1,
            occurred_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body: EventBody::SessionCreated {
                title: "fixture".to_string(),
            },
        }
    }

    #[test]
    fn catchup_events_round_trips() {
        let original = Catchup::Events {
            from: 1,
            through: 1,
            events: vec![sample_event()],
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: Catchup = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, parsed);
    }

    #[test]
    fn catchup_snapshot_round_trips() {
        use crate::ids::PromptId;
        use crate::run::{AgentMode, PromptDelivery};

        let original = Catchup::Snapshot {
            through: 512,
            projection: SessionProjection {
                session_id: SessionId::new(),
                title: "long session".to_string(),
                last_sequence: 512,
                active_runs: vec![RunId::new()],
                pending_approvals: Vec::new(),
                pending_prompts: vec![PendingPromptView {
                    id: PromptId::new(),
                    text: "queued prompt".to_string(),
                    mode: AgentMode::Build,
                    delivery: PromptDelivery::Queue,
                }],
                closed: false,
            },
        };
        let json = serde_json::to_string(&original).expect("serialize");
        assert!(json.contains("queued prompt"));
        let parsed: Catchup = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, parsed);

        // Empty pending_prompts is omitted in json and deserializes to empty vec
        let empty_proj = SessionProjection {
            session_id: SessionId::new(),
            title: "empty".to_string(),
            last_sequence: 1,
            active_runs: Vec::new(),
            pending_approvals: Vec::new(),
            pending_prompts: Vec::new(),
            closed: false,
        };
        let empty_json = serde_json::to_string(&empty_proj).expect("serialize");
        assert!(!empty_json.contains("pending_prompts"));
        let parsed_empty: SessionProjection =
            serde_json::from_str(&empty_json).expect("deserialize");
        assert_eq!(
            parsed_empty.pending_prompts,
            Vec::<PendingPromptView>::new()
        );
    }

    #[test]
    fn unknown_catchup_tag_deserializes_to_unknown() {
        let parsed: Catchup =
            serde_json::from_value(serde_json::json!({ "type": "TimeTravel", "to": 0 }))
                .expect("unknown tag must parse, not error");
        assert!(matches!(parsed, Catchup::Unknown));
    }
}
