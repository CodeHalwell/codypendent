//! ACP (Agent Client Protocol) *client* — the inverse of `acp.rs`.
//!
//! `acp.rs` is the SERVER role (Codypendent serves ACP to Zed). This module is
//! the CLIENT/host role: Codypendent spawns an external ACP agent
//! (`gemini --acp`, `npx @agentclientprotocol/claude-agent-acp`, ...), does the
//! initialize/session handshake, delegates a run's objective as an ACP prompt,
//! and maps the agent's streamed `session/update`s onto Codypendent's existing
//! `EventBody` model. The agent owns its model; we send no model id.

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionUpdate, ToolCall, ToolCallContent, ToolCallStatus,
    ToolCallUpdate,
};
use codypendent_protocol::{EventBody, RunId, ToolOutcome};

/// Map one ACP `session/update` payload onto zero or more Codypendent events
/// for the run it belongs to.
///
/// Pure and deterministic (no I/O, no clock): the same `SessionUpdate` always
/// produces the same `Vec<EventBody>`. This takes just the `update` half of
/// the wire `SessionNotification` — never its `session_id` — because mapping
/// the ACP `SessionId` an update arrived on to Codypendent's own `RunId` is
/// the session driver's job (Task 7), not this function's; the caller passes
/// the already-resolved `run_id` in.
///
/// ACP updates with no Codypendent `EventBody` equivalent produce no events
/// rather than a fabricated one — additive, so an ACP-backed turn renders
/// from exactly the same event vocabulary as a native one:
/// - `UserMessageChunk` echoes the user's own prompt back; it is not model
///   output.
/// - `Plan`, `AvailableCommandsUpdate`, `CurrentModeUpdate`,
///   `ConfigOptionUpdate`, and `SessionInfoUpdate` are ACP session/UI concepts
///   with no Codypendent parallel.
/// - `UsageUpdate` carries token/cost accounting; turning it into an
///   `EventBody::BudgetWarning` would fabricate a threshold breach that never
///   happened — the same cost-honesty rule that keeps the provider catalog's
///   cost metadata display-only and out of any budget sum.
///
/// The inverse of the server-side bridge in `crates/cli/src/acp.rs`.
#[must_use]
pub fn session_update_to_events(update: &SessionUpdate, run_id: RunId) -> Vec<EventBody> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) | SessionUpdate::AgentThoughtChunk(chunk) => {
            model_stream_delta(chunk, run_id)
        }
        SessionUpdate::ToolCall(tool_call) => tool_started(tool_call, run_id),
        SessionUpdate::ToolCallUpdate(tool_call_update) => tool_completed(tool_call_update, run_id),
        // No Codypendent `EventBody` equivalent (see the doc comment above) —
        // covers `UserMessageChunk`, `Plan`, `AvailableCommandsUpdate`,
        // `CurrentModeUpdate`, `ConfigOptionUpdate`, `SessionInfoUpdate`,
        // `UsageUpdate`, and any variant a future ACP schema bump adds that
        // this build does not know yet (`SessionUpdate` is `#[non_exhaustive]`
        // — RULE 1: unknown wire content is handled safely, not a hard
        // error).
        _ => Vec::new(),
    }
}

/// A chunk of the agent's reply or internal reasoning, streamed as
/// `EventBody::ModelStreamDelta`. Codypendent has no separate "thinking"
/// event, so both `AgentMessageChunk` and `AgentThoughtChunk` land here — the
/// same event kind the TUI already renders incrementally, so an ACP turn's
/// stream looks identical to a native one. Non-text content (image, audio,
/// resource) and empty text produce no event: there is nothing to append to
/// the transcript.
fn model_stream_delta(chunk: &ContentChunk, run_id: RunId) -> Vec<EventBody> {
    let ContentBlock::Text(text) = &chunk.content else {
        return Vec::new();
    };
    if text.text.is_empty() {
        return Vec::new();
    }
    vec![EventBody::ModelStreamDelta {
        run_id,
        text: text.text.clone(),
    }]
}

/// A newly-initiated tool call maps to `EventBody::ToolStarted`. `args_digest`
/// stays empty: the agent built these arguments, not Codypendent's own tool
/// executor, so there is no digest comparable to the native path's
/// `hash_json` (`crates/runtime/src/agent.rs`) to record here — never
/// fabricate one.
fn tool_started(tool_call: &ToolCall, run_id: RunId) -> Vec<EventBody> {
    vec![EventBody::ToolStarted {
        run_id,
        tool: tool_call.title.clone(),
        args_digest: String::new(),
    }]
}

/// A tool call update maps to `EventBody::ToolCompleted` only once it reaches
/// a terminal status. `Pending`/`InProgress` — or an update that does not
/// touch `status` at all — is not terminal yet and produces no event (ACP
/// reports progress this way; Codypendent has no "tool progressed" event).
fn tool_completed(update: &ToolCallUpdate, run_id: RunId) -> Vec<EventBody> {
    let outcome = match update.fields.status {
        Some(ToolCallStatus::Completed) => ToolOutcome::Succeeded,
        Some(ToolCallStatus::Failed) => ToolOutcome::Failed {
            message: failure_message(update),
        },
        _ => return Vec::new(),
    };
    vec![EventBody::ToolCompleted {
        run_id,
        tool: tool_label(update),
        outcome,
        artifact: None,
    }]
}

/// The update's own title, else the tool call id it targets — always
/// something, since `tool_call_id` is required on every `ToolCallUpdate`.
fn tool_label(update: &ToolCallUpdate) -> String {
    update
        .fields
        .title
        .clone()
        .unwrap_or_else(|| update.tool_call_id.to_string())
}

/// The first text content block reported alongside a failed tool call, else
/// a generic message. ACP has no field dedicated to "why did this fail"
/// distinct from the call's reported content, so this is the closest real
/// signal to a failure message — never a placeholder when the agent actually
/// told us something.
fn failure_message(update: &ToolCallUpdate) -> String {
    update
        .fields
        .content
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find_map(|item| match item {
            ToolCallContent::Content(content) => match &content.content {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_else(|| "ACP tool call failed".to_string())
}

#[cfg(test)]
mod mapping_tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        Content, ImageContent, Plan, TextContent, ToolCallUpdateFields,
    };

    fn rid() -> RunId {
        RunId::new()
    }

    /// A `ContentChunk` wrapping a single text block, the common case for
    /// both `AgentMessageChunk` and `AgentThoughtChunk`.
    fn text_chunk(text: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
    }

    #[test]
    fn agent_message_chunk_maps_to_a_model_stream_delta() {
        let run_id = rid();
        let update = SessionUpdate::AgentMessageChunk(text_chunk("hello"));
        let events = session_update_to_events(&update, run_id);
        assert_eq!(
            events,
            vec![EventBody::ModelStreamDelta {
                run_id,
                text: "hello".to_string()
            }]
        );
    }

    #[test]
    fn agent_thought_chunk_also_streams_as_text() {
        let run_id = rid();
        let update = SessionUpdate::AgentThoughtChunk(text_chunk("thinking"));
        let events = session_update_to_events(&update, run_id);
        assert_eq!(
            events,
            vec![EventBody::ModelStreamDelta {
                run_id,
                text: "thinking".to_string()
            }]
        );
    }

    #[test]
    fn agent_message_chunk_with_empty_text_produces_no_events() {
        let run_id = rid();
        let update = SessionUpdate::AgentMessageChunk(text_chunk(""));
        assert!(session_update_to_events(&update, run_id).is_empty());
    }

    #[test]
    fn agent_message_chunk_with_non_text_content_produces_no_events() {
        let run_id = rid();
        let image = ContentBlock::Image(ImageContent::new("base64data", "image/png"));
        let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(image));
        assert!(session_update_to_events(&update, run_id).is_empty());
    }

    #[test]
    fn tool_call_maps_to_tool_started() {
        let run_id = rid();
        let update = SessionUpdate::ToolCall(ToolCall::new("t1", "read_file"));
        let events = session_update_to_events(&update, run_id);
        assert_eq!(
            events,
            vec![EventBody::ToolStarted {
                run_id,
                tool: "read_file".to_string(),
                args_digest: String::new(),
            }]
        );
    }

    #[test]
    fn completed_tool_call_update_maps_to_tool_completed_succeeded() {
        let run_id = rid();
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "t1",
            ToolCallUpdateFields::new()
                .title("read_file")
                .status(ToolCallStatus::Completed),
        ));
        let events = session_update_to_events(&update, run_id);
        assert_eq!(
            events,
            vec![EventBody::ToolCompleted {
                run_id,
                tool: "read_file".to_string(),
                outcome: ToolOutcome::Succeeded,
                artifact: None,
            }]
        );
    }

    #[test]
    fn failed_tool_call_update_maps_to_tool_completed_failed() {
        let run_id = rid();
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "shell-1",
            ToolCallUpdateFields::new()
                .title("shell")
                .status(ToolCallStatus::Failed),
        ));
        let events = session_update_to_events(&update, run_id);
        assert!(matches!(
            events.as_slice(),
            [EventBody::ToolCompleted {
                outcome: ToolOutcome::Failed { .. },
                ..
            }]
        ));
    }

    #[test]
    fn failed_tool_call_update_uses_reported_content_as_the_failure_message() {
        let run_id = rid();
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "shell-1",
            ToolCallUpdateFields::new()
                .title("shell")
                .status(ToolCallStatus::Failed)
                .content(vec![ToolCallContent::Content(Content::new(
                    "permission denied",
                ))]),
        ));
        let events = session_update_to_events(&update, run_id);
        assert_eq!(
            events,
            vec![EventBody::ToolCompleted {
                run_id,
                tool: "shell".to_string(),
                outcome: ToolOutcome::Failed {
                    message: "permission denied".to_string()
                },
                artifact: None,
            }]
        );
    }

    #[test]
    fn tool_call_update_without_a_title_falls_back_to_the_tool_call_id() {
        let run_id = rid();
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "t-42",
            ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
        ));
        let events = session_update_to_events(&update, run_id);
        assert_eq!(
            events,
            vec![EventBody::ToolCompleted {
                run_id,
                tool: "t-42".to_string(),
                outcome: ToolOutcome::Succeeded,
                artifact: None,
            }]
        );
    }

    #[test]
    fn an_incomplete_tool_call_update_produces_no_events() {
        let run_id = rid();
        let in_progress = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "t1",
            ToolCallUpdateFields::new()
                .title("x")
                .status(ToolCallStatus::InProgress),
        ));
        assert!(session_update_to_events(&in_progress, run_id).is_empty());

        let no_status_change = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "t1",
            ToolCallUpdateFields::new().title("renamed"),
        ));
        assert!(session_update_to_events(&no_status_change, run_id).is_empty());
    }

    #[test]
    fn plan_update_produces_no_events() {
        let run_id = rid();
        let update = SessionUpdate::Plan(Plan::new(vec![]));
        assert!(session_update_to_events(&update, run_id).is_empty());
    }

    #[test]
    fn user_message_chunk_produces_no_events() {
        let run_id = rid();
        let update = SessionUpdate::UserMessageChunk(text_chunk("what does this do?"));
        assert!(session_update_to_events(&update, run_id).is_empty());
    }

    #[test]
    fn usage_update_produces_no_events() {
        use agent_client_protocol::schema::v1::UsageUpdate;

        let run_id = rid();
        let update = SessionUpdate::UsageUpdate(UsageUpdate::new(100, 1000));
        assert!(session_update_to_events(&update, run_id).is_empty());
    }
}
