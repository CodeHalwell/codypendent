//! The message envelope and the Phase 0 payload set.
//!
//! Every frame on the wire is one serialized `Envelope`. The payload enum
//! grows in later phases (sessions, runs, subscriptions, approvals, ...);
//! Phase 0 ships only daemon lifecycle messages.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::catchup::Catchup;
use crate::command::Command;
use crate::error::CodypendentError;
use crate::events::SessionEvent;
use crate::handshake::{ClientHello, ServerHello};
use crate::ids::{ClientId, CommandId, DaemonInstanceId, MessageId, SessionId, WorkspaceId};
use crate::version::{ProtocolVersion, PROTOCOL_V1};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol_version: ProtocolVersion,
    pub message_id: MessageId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<MessageId>,
    pub client_id: ClientId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    pub payload: Payload,
}

impl Envelope {
    /// Build a new request envelope from a client.
    pub fn request(client_id: ClientId, payload: Payload) -> Self {
        Self {
            protocol_version: PROTOCOL_V1,
            message_id: MessageId::new(),
            correlation_id: None,
            client_id,
            workspace_id: None,
            session_id: None,
            sequence: None,
            payload,
        }
    }

    /// Build a reply correlated to `request`.
    pub fn reply_to(request: &Envelope, payload: Payload) -> Self {
        Self {
            protocol_version: PROTOCOL_V1,
            message_id: MessageId::new(),
            correlation_id: Some(request.message_id),
            client_id: request.client_id,
            workspace_id: request.workspace_id,
            session_id: request.session_id,
            sequence: None,
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Payload {
    /// Liveness probe.
    Ping,
    Pong,
    /// Ask the daemon to describe itself.
    DaemonStatusRequest,
    DaemonStatusResponse(DaemonStatus),
    /// Ask the daemon to shut down gracefully.
    Shutdown,
    ShutdownAck,
    /// Structured protocol-level error (never parse human text to decide
    /// behaviour).
    Error(ProtocolError),

    // --- Phase 1: handshake, commands, events, catch-up ---
    /// Client's opening handshake message.
    ClientHello(ClientHello),
    /// Daemon's handshake reply.
    ServerHello(ServerHello),
    /// A client request for a state change (idempotent).
    Command(Command),
    /// The command was accepted and applied; carries the resulting ledger
    /// sequence when the command produced events.
    CommandAccepted {
        command_id: CommandId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sequence: Option<u64>,
    },
    /// The command was rejected; carries the full structured error.
    CommandRejected(CodypendentError),
    /// A persisted session event published to a subscribed client.
    Event(SessionEvent),
    /// Attach-time catch-up (missed events or a snapshot). Wrapped in a named
    /// field so its internal `type` tag never collides with the payload tag.
    Catchup {
        catchup: Catchup,
    },

    /// Forward-compatibility fallback: a payload tag this build does not know
    /// deserializes to `Unknown` instead of failing the whole frame, so the
    /// receiver can reject it structurally and keep the connection alive
    /// (additive 1.x payloads must never break an older peer).
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::ClientCapabilities;
    use crate::command::CommandBody;
    use crate::ids::CommandId;
    use crate::run::AgentMode;

    fn round_trip_payload(payload: Payload) -> Payload {
        let envelope = Envelope::request(ClientId::new(), payload);
        let json = serde_json::to_string(&envelope).expect("serialize");
        let parsed: Envelope = serde_json::from_str(&json).expect("deserialize");
        parsed.payload
    }

    #[test]
    fn unknown_payload_tag_deserializes_to_unknown() {
        let request = Envelope::request(ClientId::new(), Payload::Ping);
        let mut value = serde_json::to_value(&request).expect("serialize");
        value["payload"] = serde_json::json!({ "type": "FromTheFuture", "detail": 42 });
        let parsed: Envelope = serde_json::from_value(value).expect("future payloads must parse");
        assert!(matches!(parsed.payload, Payload::Unknown));
    }

    #[test]
    fn phase0_payloads_still_round_trip() {
        assert!(matches!(round_trip_payload(Payload::Ping), Payload::Ping));
        assert!(matches!(round_trip_payload(Payload::Pong), Payload::Pong));
        assert!(matches!(
            round_trip_payload(Payload::DaemonStatusRequest),
            Payload::DaemonStatusRequest
        ));
        assert!(matches!(
            round_trip_payload(Payload::Shutdown),
            Payload::Shutdown
        ));
        assert!(matches!(
            round_trip_payload(Payload::ShutdownAck),
            Payload::ShutdownAck
        ));
    }

    #[test]
    fn phase1_handshake_payloads_round_trip() {
        let hello = Payload::ClientHello(ClientHello {
            client_name: "cli".to_string(),
            client_version: "0.1.0".to_string(),
            supported_protocols: vec![PROTOCOL_V1],
            capabilities: ClientCapabilities::default(),
            resume_token: None,
        });
        assert!(matches!(round_trip_payload(hello), Payload::ClientHello(_)));

        let server_hello = Payload::ServerHello(ServerHello {
            selected_protocol: PROTOCOL_V1,
            daemon_version: "0.1.0".to_string(),
            daemon_instance: DaemonInstanceId::new(),
            heartbeat_interval_ms: 15_000,
        });
        assert!(matches!(
            round_trip_payload(server_hello),
            Payload::ServerHello(_)
        ));
    }

    #[test]
    fn phase1_command_payloads_round_trip() {
        let command = Payload::Command(Command {
            command_id: CommandId::new(),
            idempotency_key: "idem".to_string(),
            expected_revision: None,
            body: CommandBody::StartRun {
                session_id: SessionId::new(),
                objective: "fix it".to_string(),
                mode: AgentMode::Build,
                repository: None,
            },
        });
        match round_trip_payload(command) {
            Payload::Command(cmd) => {
                assert!(matches!(cmd.body, CommandBody::StartRun { .. }));
            }
            other => panic!("expected Command, got {other:?}"),
        }

        let accepted = Payload::CommandAccepted {
            command_id: CommandId::new(),
            sequence: Some(7),
        };
        assert!(matches!(
            round_trip_payload(accepted),
            Payload::CommandAccepted {
                sequence: Some(7),
                ..
            }
        ));

        let rejected = Payload::CommandRejected(CodypendentError::new(
            "protocol.role-denied",
            "observers may not start runs",
            false,
        ));
        match round_trip_payload(rejected) {
            Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
            other => panic!("expected CommandRejected, got {other:?}"),
        }
    }

    #[test]
    fn phase1_event_and_catchup_payloads_round_trip() {
        use crate::events::{Actor, EventBody, SessionEvent};
        use chrono::Utc;

        let event = SessionEvent {
            sequence: 3,
            occurred_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body: EventBody::SessionClosed,
        };
        match round_trip_payload(Payload::Event(event)) {
            Payload::Event(ev) => assert!(matches!(ev.body, EventBody::SessionClosed)),
            other => panic!("expected Event, got {other:?}"),
        }

        let catchup = Payload::Catchup {
            catchup: Catchup::Events {
                from: 1,
                through: 3,
                events: vec![],
            },
        };
        match round_trip_payload(catchup) {
            Payload::Catchup { catchup } => {
                assert!(matches!(catchup, Catchup::Events { from: 1, .. }));
            }
            other => panic!("expected Catchup, got {other:?}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub daemon_version: String,
    pub protocol_version: ProtocolVersion,
    pub instance_id: DaemonInstanceId,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub uptime_seconds: u64,
    pub boot_count: i64,
    pub database_path: String,
    pub socket_path: String,
    pub session_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
