use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{SessionUpdate, ToolCallStatus};
use codypendent_protocol::{EventBody, RunId, ToolOutcome};

use crate::{acp_client::session_update_to_events, acp_tool_failure::AcpToolFailure};

const MAX_ACTIVE_TOOLS: usize = 256;

#[derive(Default)]
pub(crate) struct AcpToolHistory {
    failures: BTreeMap<String, AcpToolFailure>,
}

impl AcpToolHistory {
    pub(crate) fn events(&mut self, update: &SessionUpdate, run: RunId) -> Vec<EventBody> {
        let (id, status) = match update {
            SessionUpdate::ToolCall(call) => (call.tool_call_id.to_string(), Some(call.status)),
            SessionUpdate::ToolCallUpdate(call) => {
                (call.tool_call_id.to_string(), call.fields.status)
            }
            _ => return session_update_to_events(update, run),
        };
        if self.failures.len() >= MAX_ACTIVE_TOOLS && !self.failures.contains_key(&id) {
            return session_update_to_events(update, run);
        }
        let failure = self.failures.entry(id.clone()).or_default();
        match update {
            SessionUpdate::ToolCall(call) => failure.capture_call(call),
            SessionUpdate::ToolCallUpdate(call) => failure.capture_update(call),
            _ => {}
        }
        let mut events = session_update_to_events(update, run);
        if status == Some(ToolCallStatus::Failed) {
            for event in &mut events {
                if let EventBody::ToolCompleted {
                    outcome: ToolOutcome::Failed { message },
                    ..
                } = event
                {
                    *message = failure.message();
                }
            }
        }
        if matches!(
            status,
            Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
        ) {
            self.failures.remove(&id);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        Content, ToolCall, ToolCallContent, ToolCallUpdate, ToolCallUpdateFields,
    };
    use serde_json::json;

    #[test]
    fn terminal_status_keeps_earlier_diagnostics_for_its_own_call() {
        let mut history = AcpToolHistory::default();
        let run = RunId::new();
        history.events(
            &SessionUpdate::ToolCall(ToolCall::new("search", "Search")),
            run,
        );
        history.events(
            &SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "search",
                ToolCallUpdateFields::new()
                    .raw_output(json!({"exit_code":137}))
                    .content(vec![ToolCallContent::Content(Content::new(
                        "search interrupted",
                    ))]),
            )),
            run,
        );
        history.events(
            &SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "other",
                ToolCallUpdateFields::new().content(vec![ToolCallContent::Content(Content::new(
                    "unrelated failure",
                ))]),
            )),
            run,
        );
        let events = history.events(
            &SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "search",
                ToolCallUpdateFields::new().status(ToolCallStatus::Failed),
            )),
            run,
        );
        assert!(
            matches!(events.as_slice(), [EventBody::ToolCompleted { outcome: ToolOutcome::Failed { message }, .. }]
            if message.contains("137") && message.contains("search interrupted") && !message.contains("unrelated"))
        );
        assert!(!history.failures.contains_key("search"));
    }

    #[test]
    fn completed_calls_release_history_and_do_not_pollute_later_calls() {
        let mut history = AcpToolHistory::default();
        let run = RunId::new();
        history.events(
            &SessionUpdate::ToolCall(
                ToolCall::new("search", "Search")
                    .raw_output(json!({"error":"old message"}))
                    .status(ToolCallStatus::Completed),
            ),
            run,
        );
        assert!(history.failures.is_empty());
        let events = history.events(
            &SessionUpdate::ToolCall(
                ToolCall::new("search", "Search").status(ToolCallStatus::Failed),
            ),
            run,
        );
        assert!(
            matches!(events.last(), Some(EventBody::ToolCompleted { outcome: ToolOutcome::Failed { message }, .. })
            if !message.contains("old message"))
        );
    }

    #[test]
    fn unfinished_calls_have_a_fixed_memory_bound() {
        let mut history = AcpToolHistory::default();
        let run = RunId::new();
        for index in 0..MAX_ACTIVE_TOOLS + 10 {
            history.events(
                &SessionUpdate::ToolCall(ToolCall::new(index.to_string(), "Search")),
                run,
            );
        }
        assert_eq!(history.failures.len(), MAX_ACTIVE_TOOLS);
        let events = history.events(
            &SessionUpdate::ToolCall(
                ToolCall::new("overflow", "Search")
                    .status(ToolCallStatus::Failed)
                    .raw_output(json!({"exit_code":2,"stderr":"invalid pattern"})),
            ),
            run,
        );
        assert!(
            matches!(events.last(), Some(EventBody::ToolCompleted { outcome: ToolOutcome::Failed { message }, .. })
            if message.contains("invalid pattern"))
        );
    }
}
