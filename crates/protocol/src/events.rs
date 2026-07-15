//! Durable session events.
//!
//! Events record accepted state changes or observations. They are persisted
//! in the event ledger before any client observes them, and original events
//! are immutable evidence (invariant 5). The `EventBody` set here is the
//! Phase 0 seed; later phases add run, tool, approval, patch, workflow, and
//! document events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, ClientId, CommandId, CorrelationId, ModelId, RunId, UserId};

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum EventBody {
    SessionCreated { title: String },
    NoteAppended { text: String },
    SessionClosed,
}
