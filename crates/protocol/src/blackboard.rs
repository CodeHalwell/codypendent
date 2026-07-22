//! Blackboard wire types (Phase 5 STEP 5.3): the client-facing projection of a
//! workflow run's typed artifact board.
//!
//! Agents in a multi-agent workflow communicate **only** via blackboard artifacts
//! and declared node outputs (Chapter 04). The authoritative board lives in
//! `codypendent-workflow`'s `BlackboardStore`; this crate carries the *view* of one
//! stored artifact across the wire — the shape the daemon's read command
//! ([`CommandBody::ReadBlackboard`](crate::command::CommandBody::ReadBlackboard))
//! returns and the per-run subscription
//! ([`Subscription::Blackboard`](crate::handshake::Subscription::Blackboard))
//! delivers.
//!
//! Payload, author, and evidence ride as opaque JSON [`Value`]s so the protocol
//! stays decoupled from the workflow domain types — a client renders them, never
//! branches structurally on them (and treats them as evidence, not instructions).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One stored blackboard artifact, projected for a client.
///
/// A read-command reply carries a `Vec` of these (the run's board, kind-filtered);
/// a subscription delivers one as each post/supersede lands. The `workflow_run_id`
/// travels with the item so a client routes a live
/// [`Payload::BlackboardPosted`](crate::envelope::Payload::BlackboardPosted) to the
/// right board without consulting the enclosing frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlackboardItemView {
    /// The artifact's stable id (a UUIDv7 string).
    pub id: String,
    /// The workflow run whose board holds it.
    pub workflow_run_id: String,
    /// The typed artifact kind (`finding`, `decision`, `hypothesis`, …), as the
    /// manifest-facing string the `BlackboardStore` records.
    pub kind: String,
    /// The artifact body (opaque JSON — a client renders it).
    pub payload: Value,
    /// Who produced it — the daemon builds this server-side from the authoring
    /// node's run context (`{role, run_id, node_id, workflow_run_id}`), never from
    /// model-supplied identity.
    pub author: Value,
    /// The author's self-reported confidence in `[0, 1]`, if given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Evidence references grounding the artifact (opaque JSON). Claim-like kinds
    /// require at least one; the store enforces it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Value>,
    /// The artifact's revision within its supersession chain (1 for an original).
    pub revision: u32,
    /// The id of the item that superseded this one, if any — a live item has
    /// `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view() -> BlackboardItemView {
        BlackboardItemView {
            id: "0192-item".to_string(),
            workflow_run_id: "wfrun-abc".to_string(),
            kind: "finding".to_string(),
            payload: json!({ "summary": "the parser drops trailing commas" }),
            author: json!({ "role": "investigator", "node_id": "diagnose" }),
            confidence: Some(0.8),
            evidence: vec![json!({ "path": "src/parse.rs", "line": 42 })],
            revision: 1,
            superseded_by: None,
        }
    }

    #[test]
    fn blackboard_item_view_round_trips() {
        let original = view();
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: BlackboardItemView = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, parsed);
    }

    #[test]
    fn absent_optionals_are_skipped_and_default_back() {
        // A live item with no confidence and no evidence omits both keys, and such
        // a payload reparses with them defaulted (an older client that sends none
        // still round-trips).
        let mut item = view();
        item.confidence = None;
        item.evidence = Vec::new();
        item.superseded_by = None;
        let json = serde_json::to_string(&item).expect("serialize");
        assert!(!json.contains("confidence"), "confidence skipped: {json}");
        assert!(!json.contains("evidence"), "evidence skipped: {json}");
        assert!(
            !json.contains("superseded_by"),
            "superseded_by skipped: {json}"
        );
        let parsed: BlackboardItemView = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, item);
    }
}
