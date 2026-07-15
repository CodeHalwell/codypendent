//! The structured error contract.
//!
//! Chapter 14: errors are structured so a client never has to parse human text
//! to decide whether a retry or an approval is possible. [`CodypendentError`]
//! is the full shape; the Phase 0 [`crate::envelope::ProtocolError`] remains for
//! the transport-level errors it already carries.

use serde::{Deserialize, Serialize};

use crate::ids::CorrelationId;

/// A machine-readable, correlated error.
///
/// `code` is a stable dotted identifier (for example
/// `protocol.unsupported-payload` or `policy.write-denied`) that receivers
/// branch on; `message` is for humans only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodypendentError {
    /// Stable machine-readable code. Never parse `message` to decide behaviour.
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
    /// Whether an identical retry could succeed.
    pub retryable: bool,
    /// A suggested next step the client can surface as an affordance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_action: Option<UserAction>,
    /// Free-form structured context (offending path, limits, ...). Defaults to
    /// JSON `null` when there is nothing to add.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
    pub correlation_id: CorrelationId,
}

impl CodypendentError {
    /// Build an error with no `user_action` and empty `details`.
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
            user_action: None,
            details: serde_json::Value::Null,
            correlation_id: CorrelationId::new(),
        }
    }
}

/// A machine-readable hint about how the user could resolve an error, so the
/// client can render the right affordance instead of parsing `message`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum UserAction {
    /// Retry the same request.
    Retry,
    /// Re-establish identity or re-authorise.
    Reauthenticate,
    /// An approval is required before the action can proceed.
    GrantApproval,
    /// The active policy must be widened or changed.
    AdjustPolicy,
    /// The model or provider configuration needs attention.
    ReconfigureModel,
    /// Nothing automatic; escalate to a human/support.
    ContactSupport,
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_round_trips_with_all_fields() {
        let original = CodypendentError {
            code: "policy.write-denied".to_string(),
            message: "writes are denied in Explore mode".to_string(),
            retryable: false,
            user_action: Some(UserAction::AdjustPolicy),
            details: serde_json::json!({ "path": "/etc/passwd" }),
            correlation_id: CorrelationId::new(),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: CodypendentError = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, parsed);
    }

    #[test]
    fn error_round_trips_minimal() {
        let original = CodypendentError::new("protocol.unsupported-payload", "unknown", false);
        let json = serde_json::to_string(&original).expect("serialize");
        // The minimal form omits the absent optional fields on the wire.
        assert!(!json.contains("user_action"));
        assert!(!json.contains("details"));
        let parsed: CodypendentError = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, parsed);
    }

    #[test]
    fn unknown_user_action_tag_deserializes_to_unknown() {
        let parsed: UserAction =
            serde_json::from_value(serde_json::json!({ "type": "DoSomethingNew" }))
                .expect("unknown tag must parse, not error");
        assert!(matches!(parsed, UserAction::Unknown));
    }
}
