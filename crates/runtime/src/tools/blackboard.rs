//! `blackboard.post` / `blackboard.query` — the two registry tools a workflow
//! agent reads and writes its run's typed artifact channel through (STEP 5.3).
//!
//! These are the *only* way an agent communicates a durable result to the rest of
//! its workflow (Chapter 04: agents share findings via blackboard artifacts and
//! declared outputs, never raw transcripts). Both are meaningful only inside a
//! workflow node's agent run — they need the ambient `workflow_run_id`, which the
//! node executor threads onto the [`RunContext`](crate::agent::RunContext); a plain
//! single-agent run is never offered them (the agent loop gates them on the run's
//! workflow binding, so a call in a non-workflow run reads as an unknown tool).
//!
//! Unlike the filesystem/command tools, a blackboard access needs no path/command
//! scope: it targets only the run's own board, so its [`ProposedAction`] is
//! policy-allowed unconditionally and never reaches the approval gate — it is
//! recorded purely so the access is traced like every other tool call. The actual
//! store read/write happens behind the
//! [`BlackboardChannel`](crate::blackboard::BlackboardChannel) seam (the agent-loop
//! layer owns argument parsing + author attribution; this module owns the tool
//! identities, argument shapes, and proposed actions).

use codypendent_protocol::ProposedAction;
use serde_json::Value;

/// The `blackboard.post` tool: post (or supersede) a typed artifact on the run's
/// board.
pub struct BlackboardPostTool;

impl BlackboardPostTool {
    /// The stable dotted tool name.
    pub const NAME: &'static str = "blackboard.post";

    /// The action policy evaluates: a board write scoped to the run (never the
    /// filesystem/remote), always allowed within a workflow run.
    #[must_use]
    pub fn proposed_action(workflow_run_id: &str, kind: &str) -> ProposedAction {
        ProposedAction::BlackboardPost {
            workflow_run_id: workflow_run_id.to_string(),
            kind: kind.to_string(),
        }
    }
}

/// The parsed, model-supplied arguments of a `blackboard.post` call. The author is
/// **not** here — the agent loop builds it server-side from the run context, never
/// trusting a model-supplied identity.
#[derive(Debug, Clone, PartialEq)]
pub struct BlackboardPostInput {
    /// The artifact kind (`finding`, `decision`, …).
    pub kind: String,
    /// The artifact body.
    pub payload: Value,
    /// The author's confidence in `[0, 1]`, if given.
    pub confidence: Option<f64>,
    /// Evidence references grounding the artifact.
    pub evidence: Vec<Value>,
    /// The id of a prior item this post supersedes (a correction), if any.
    pub supersedes: Option<String>,
}

/// Parse `blackboard.post` arguments. `kind` and `payload` are required; the model
/// may pass `evidence` as an array, `confidence` as a number, and `supersedes` as a
/// prior item id. A bare non-array `evidence` is treated as a single reference so a
/// model that passes one object still grounds its claim.
pub fn parse_blackboard_post(args: &Value) -> Result<BlackboardPostInput, String> {
    let kind = args
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("blackboard.post requires a string `kind`")?
        .to_string();
    let payload = args
        .get("payload")
        .cloned()
        .ok_or("blackboard.post requires a `payload`")?;
    let confidence = args.get("confidence").and_then(Value::as_f64);
    let evidence = match args.get("evidence") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        // A single reference passed bare is still one piece of evidence.
        Some(other) => vec![other.clone()],
    };
    let supersedes = args
        .get("supersedes")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(BlackboardPostInput {
        kind,
        payload,
        confidence,
        evidence,
        supersedes,
    })
}

/// The `blackboard.query` tool: read the run's board, optionally filtered by kind.
pub struct BlackboardQueryTool;

impl BlackboardQueryTool {
    /// The stable dotted tool name.
    pub const NAME: &'static str = "blackboard.query";

    /// The action policy evaluates: a board read scoped to the run.
    #[must_use]
    pub fn proposed_action(workflow_run_id: &str) -> ProposedAction {
        ProposedAction::BlackboardQuery {
            workflow_run_id: workflow_run_id.to_string(),
        }
    }
}

/// The parsed arguments of a `blackboard.query` call.
#[derive(Debug, Clone, PartialEq)]
pub struct BlackboardQueryInput {
    /// A kind to filter by, or all kinds when `None`.
    pub kind: Option<String>,
    /// Include superseded revisions too; the default (`false`) is the live board.
    pub include_superseded: bool,
}

/// Parse `blackboard.query` arguments. Both are optional: no `kind` reads every
/// kind, and `include_superseded` defaults to `false` (the live board only).
pub fn parse_blackboard_query(args: &Value) -> BlackboardQueryInput {
    let kind = args.get("kind").and_then(Value::as_str).map(str::to_string);
    let include_superseded = args
        .get("include_superseded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    BlackboardQueryInput {
        kind,
        include_superseded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn post_requires_kind_and_payload() {
        assert!(parse_blackboard_post(&json!({ "payload": {} })).is_err());
        assert!(parse_blackboard_post(&json!({ "kind": "finding" })).is_err());
    }

    #[test]
    fn post_parses_evidence_forms_and_supersedes() {
        // An array of references stays a list.
        let parsed = parse_blackboard_post(&json!({
            "kind": "finding",
            "payload": { "summary": "x" },
            "confidence": 0.7,
            "evidence": [{ "path": "a.rs" }, { "path": "b.rs" }],
            "supersedes": "0192-old",
        }))
        .expect("parses");
        assert_eq!(parsed.kind, "finding");
        assert_eq!(parsed.confidence, Some(0.7));
        assert_eq!(parsed.evidence.len(), 2);
        assert_eq!(parsed.supersedes.as_deref(), Some("0192-old"));

        // A single bare reference is wrapped as one piece of evidence.
        let one = parse_blackboard_post(&json!({
            "kind": "decision",
            "payload": "go",
            "evidence": { "path": "c.rs" },
        }))
        .expect("parses");
        assert_eq!(one.evidence.len(), 1);
        assert!(one.supersedes.is_none());

        // No evidence key yields an empty list (the store then enforces the
        // claim-kind requirement, surfaced back to the agent).
        let none =
            parse_blackboard_post(&json!({ "kind": "finding", "payload": {} })).expect("parses");
        assert!(none.evidence.is_empty());
    }

    #[test]
    fn query_defaults_to_live_all_kinds() {
        let all = parse_blackboard_query(&json!({}));
        assert_eq!(all.kind, None);
        assert!(!all.include_superseded);

        let filtered = parse_blackboard_query(&json!({
            "kind": "finding",
            "include_superseded": true,
        }));
        assert_eq!(filtered.kind.as_deref(), Some("finding"));
        assert!(filtered.include_superseded);
    }
}
