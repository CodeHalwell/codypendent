//! Attach-time catch-up: missed events or a projection snapshot.
//!
//! When a client attaches (or reconnects), the daemon replies with a
//! [`Catchup`] (Chapter 03): if the client is at most ~500 events behind it
//! receives the missed [`SessionEvent`]s directly, otherwise a compact
//! [`SessionProjection`] snapshot it can render immediately and then live-tail.

use serde::{Deserialize, Serialize};

use crate::events::SessionEvent;
use crate::ids::{RunId, SessionId};

/// The daemon's answer to an attach: replay or snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
pub struct SessionProjection {
    pub session_id: SessionId,
    pub title: String,
    /// The highest event sequence folded into this snapshot.
    pub last_sequence: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_runs: Vec<RunId>,
    pub closed: bool,
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
        let original = Catchup::Snapshot {
            through: 512,
            projection: SessionProjection {
                session_id: SessionId::new(),
                title: "long session".to_string(),
                last_sequence: 512,
                active_runs: vec![RunId::new()],
                closed: false,
            },
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: Catchup = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, parsed);
    }

    #[test]
    fn unknown_catchup_tag_deserializes_to_unknown() {
        let parsed: Catchup =
            serde_json::from_value(serde_json::json!({ "type": "TimeTravel", "to": 0 }))
                .expect("unknown tag must parse, not error");
        assert!(matches!(parsed, Catchup::Unknown));
    }
}
