//! The message envelope and the Phase 0 payload set.
//!
//! Every frame on the wire is one serialized `Envelope`. The payload enum
//! grows in later phases (sessions, runs, subscriptions, approvals, ...);
//! Phase 0 ships only daemon lifecycle messages.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{ClientId, DaemonInstanceId, MessageId, SessionId, WorkspaceId};
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

    #[test]
    fn unknown_payload_tag_deserializes_to_unknown() {
        let request = Envelope::request(ClientId::new(), Payload::Ping);
        let mut value = serde_json::to_value(&request).expect("serialize");
        value["payload"] = serde_json::json!({ "type": "FromTheFuture", "detail": 42 });
        let parsed: Envelope = serde_json::from_value(value).expect("future payloads must parse");
        assert!(matches!(parsed.payload, Payload::Unknown));
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
