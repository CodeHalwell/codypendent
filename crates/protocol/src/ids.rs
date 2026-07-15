//! Opaque, sortable identifiers (UUIDv7) for every domain entity.
//!
//! See "Core Data Contracts". IDs are newtypes so they can never be confused
//! with one another at compile time.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Create a new time-ordered (UUIDv7) identifier.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }
    };
}

uuid_id!(SessionId);
uuid_id!(RunId);
uuid_id!(TaskId);
uuid_id!(AgentId);
uuid_id!(ArtifactId);
uuid_id!(WorkflowId);
uuid_id!(ToolId);
uuid_id!(SkillId);
uuid_id!(PluginId);
uuid_id!(DocumentId);
uuid_id!(WorkspaceId);
uuid_id!(ClientId);
uuid_id!(MessageId);
uuid_id!(CommandId);
uuid_id!(CorrelationId);
uuid_id!(ApprovalId);
uuid_id!(DaemonInstanceId);

/// Model identifiers are provider strings such as `"claude-sonnet-5"` or
/// `"qwen2.5-coder:32b"`, not UUIDs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// User identifiers are strings in the personal product (OS user or configured
/// identity), not UUIDs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub String);

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
