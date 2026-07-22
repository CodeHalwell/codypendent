//! Connection handshake, roles, and projection subscriptions.
//!
//! The handshake is the first exchange on every connection (Chapter 03): the
//! client sends a [`ClientHello`], the daemon replies with a [`ServerHello`].
//! After that, a client attaches to a session with a [`ClientRole`] and a set
//! of [`Subscription`]s selecting which projections it wants to receive.

use serde::{Deserialize, Serialize};

use crate::capabilities::ClientCapabilities;
use crate::ids::{DaemonInstanceId, DocumentId, RunId};
use crate::version::ProtocolVersion;

/// An opaque, daemon-signed reconnection token (STEP 1.11).
///
/// Clients treat it as an opaque blob: they store the token from a prior
/// session and present it on the next `ClientHello`, never parsing or minting
/// it themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResumeToken(pub String);

/// The client's opening message: who it is, what it speaks, and what it can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub client_name: String,
    pub client_version: String,
    /// Protocol versions the client can speak, best first.
    pub supported_protocols: Vec<ProtocolVersion>,
    pub capabilities: ClientCapabilities,
    /// Present when resuming a prior connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_token: Option<ResumeToken>,
}

/// The daemon's reply: the negotiated protocol plus liveness parameters.
///
/// The Chapter 03 sketch also carries an `authentication` result; local
/// authentication in Phase 1 is enforced by socket permissions and OS peer
/// identity, so this Phase 1 shape omits that field (it can be added additively
/// when remote transports arrive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub selected_protocol: ProtocolVersion,
    pub daemon_version: String,
    pub daemon_instance: DaemonInstanceId,
    /// How often the client should expect (and send) heartbeats.
    pub heartbeat_interval_ms: u64,
    /// A fresh [`ResumeToken`] for this connection's identity. The client stores
    /// it opaquely and presents it on its next `ClientHello`, so a reconnect
    /// resumes the same client identity even across a client-process restart.
    /// Optional and defaulted for wire compatibility with older daemons/clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_token: Option<ResumeToken>,
}

/// A client's authority over a session it observes (Chapter 03). Exclusivity is
/// attached to specific resources (leases), not to the whole session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum ClientRole {
    /// Read-only.
    Observer,
    /// May submit input and steer.
    Contributor,
    /// May control runs (cancel, pause, resume).
    Controller,
    /// May resolve approvals.
    Approver,
    #[serde(other)]
    Unknown,
}

/// A projection view a client subscribes to, rather than receiving every
/// internal event (Chapter 03). This is the Phase 1 subset; document, workflow,
/// and GitHub views arrive with their features.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum Subscription {
    /// High-level session lifecycle and summary.
    SessionSummary,
    /// The detailed trace of one run.
    RunTrace { run_id: RunId },
    /// Agent activity across the session.
    AgentActivity,
    /// Repository/worktree status.
    RepositoryStatus,
    /// Budget usage and warnings.
    BudgetState,
    /// A collaborative document's CRDT sync stream (Phase 4 STEP 4.3): as the
    /// authoritative replica advances, the daemon fans each `DocumentSync` out
    /// to subscribers over a per-document hub, delivered as
    /// [`Payload::DocumentSync`](crate::envelope::Payload). A subscriber's
    /// baseline comes from the document read path; this stream carries the
    /// post-subscribe updates it merges (idempotent CRDT merge, so no
    /// watermark is needed). Re-attaching with a different `Document` set
    /// replaces the previous forwarders.
    Document { document_id: DocumentId },
    /// A workflow run's blackboard stream (Phase 5 STEP 5.3): as the run's agents
    /// post (and supersede) typed artifacts, the daemon fans each
    /// [`BlackboardItemView`](crate::blackboard::BlackboardItemView) out to
    /// subscribers over a per-run hub, delivered as
    /// [`Payload::BlackboardPosted`](crate::envelope::Payload). A subscriber's
    /// baseline comes from the blackboard read command
    /// ([`ReadBlackboard`](crate::command::CommandBody::ReadBlackboard)); this
    /// stream carries the post-subscribe artifacts it merges by id (a superseding
    /// revision arrives as its own delivery, so no watermark is needed). Mirrors
    /// [`Document`](Subscription::Document)'s per-id hub, keyed by workflow run.
    Blackboard { workflow_run_id: String },
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::PROTOCOL_V1;

    #[test]
    fn client_hello_round_trips() {
        let original = ClientHello {
            client_name: "codypendent-tui".to_string(),
            client_version: "0.1.0".to_string(),
            supported_protocols: vec![PROTOCOL_V1],
            capabilities: ClientCapabilities {
                rich_text: true,
                mouse: true,
                unicode: true,
                true_color: true,
                ..ClientCapabilities::default()
            },
            resume_token: Some(ResumeToken("opaque-token".to_string())),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: ClientHello = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, parsed);
    }

    #[test]
    fn server_hello_round_trips() {
        let original = ServerHello {
            selected_protocol: PROTOCOL_V1,
            daemon_version: "0.1.0".to_string(),
            daemon_instance: DaemonInstanceId::new(),
            heartbeat_interval_ms: 15_000,
            resume_token: Some(ResumeToken("opaque".to_string())),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: ServerHello = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, parsed);

        // An older daemon's hello (no resume_token on the wire) still parses.
        let legacy = serde_json::json!({
            "selected_protocol": PROTOCOL_V1,
            "daemon_version": "0.1.0",
            "daemon_instance": DaemonInstanceId::new(),
            "heartbeat_interval_ms": 15_000u64,
        });
        let parsed: ServerHello = serde_json::from_value(legacy).expect("legacy hello parses");
        assert_eq!(parsed.resume_token, None);
    }

    #[test]
    fn role_and_subscription_round_trip() {
        for role in [
            ClientRole::Observer,
            ClientRole::Contributor,
            ClientRole::Controller,
            ClientRole::Approver,
        ] {
            let json = serde_json::to_string(&role).expect("serialize");
            let parsed: ClientRole = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(role, parsed);
        }
        let subs = [
            Subscription::SessionSummary,
            Subscription::RunTrace {
                run_id: RunId::new(),
            },
            Subscription::AgentActivity,
            Subscription::RepositoryStatus,
            Subscription::BudgetState,
        ];
        for sub in subs {
            let json = serde_json::to_string(&sub).expect("serialize");
            let parsed: Subscription = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(sub, parsed);
        }
        // The per-run blackboard subscription (STEP 5.3) round-trips its run id.
        let blackboard = Subscription::Blackboard {
            workflow_run_id: "wfrun-abc123".to_string(),
        };
        let json = serde_json::to_string(&blackboard).expect("serialize");
        assert_eq!(
            serde_json::from_str::<Subscription>(&json).expect("deserialize"),
            blackboard
        );
    }

    #[test]
    fn unknown_tags_deserialize_to_unknown() {
        let future = serde_json::json!({ "type": "FromTheFuture" });
        assert!(matches!(
            serde_json::from_value::<ClientRole>(future.clone()).expect("role"),
            ClientRole::Unknown
        ));
        assert!(matches!(
            serde_json::from_value::<Subscription>(future).expect("subscription"),
            Subscription::Unknown
        ));
    }
}
