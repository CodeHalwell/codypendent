use agent_client_protocol::schema::v1::{ContentBlock, ToolCall, ToolCallContent, ToolCallUpdate};
use serde::Deserialize;
use serde_json::Value;
use std::sync::OnceLock;

const MAX_TEXT: usize = 2048;
const MAX_MESSAGES: usize = 8;

pub fn format_tool_failure(diagnostic: &str) -> String {
    let mut failure = AcpToolFailure::default();
    failure.capture_text(diagnostic);
    failure.message()
}

#[derive(Clone, Default)]
pub(crate) struct AcpToolFailure {
    messages: Vec<String>,
    exit_code: Option<i64>,
    signal: Option<String>,
}

impl AcpToolFailure {
    pub(crate) fn capture_call(&mut self, call: &ToolCall) {
        self.capture(&call.content, call.raw_output.as_ref());
        if let Some(terminal) = call
            .meta
            .as_ref()
            .and_then(|meta| meta.get("terminal_exit"))
        {
            self.capture_raw(terminal, 0);
        }
    }

    pub(crate) fn capture_update(&mut self, update: &ToolCallUpdate) {
        self.capture(
            update.fields.content.as_deref().unwrap_or_default(),
            update.fields.raw_output.as_ref(),
        );
        if let Some(terminal) = update
            .meta
            .as_ref()
            .and_then(|meta| meta.get("terminal_exit"))
        {
            self.capture_raw(terminal, 0);
        }
    }

    fn capture(&mut self, content: &[ToolCallContent], raw: Option<&Value>) {
        for item in content.iter().take(16) {
            if let ToolCallContent::Content(content) = item {
                if let ContentBlock::Text(text) = &content.content {
                    self.capture_text(&text.text);
                }
            }
        }
        if let Some(raw) = raw {
            self.capture_raw(raw, 0);
        }
    }

    fn capture_text(&mut self, text: &str) {
        let bounded: String = text.chars().take(MAX_TEXT).collect();
        if let Ok(value) = serde_json::from_str::<Value>(&bounded) {
            self.capture_raw(&value, 0);
            return;
        }
        if bounded.trim_start().starts_with('{') {
            return;
        }
        let message = safe_text(&bounded);
        if !message.is_empty() && !self.messages.contains(&message) {
            if self.messages.len() == MAX_MESSAGES {
                self.messages.remove(0);
            }
            self.messages.push(message);
        }
    }

    fn capture_raw(&mut self, raw: &Value, depth: usize) {
        if depth > 4 {
            return;
        }
        let Value::Object(fields) = raw else {
            if let Value::String(text) = raw {
                self.capture_text(text);
            }
            return;
        };
        for (key, value) in fields.iter().take(32) {
            if key.len() > 64 {
                continue;
            }
            match key.replace(['_', '-'], "").to_ascii_lowercase().as_str() {
                "content" => {
                    for block in value.as_array().into_iter().flatten().take(16) {
                        if block.get("type").and_then(Value::as_str) == Some("text") {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                self.capture_text(text);
                            }
                        }
                    }
                }
                "exitcode" => {
                    if let Some(code) = value.as_i64().or_else(|| value.as_str()?.parse().ok()) {
                        self.exit_code = Some(code);
                    }
                }
                "signal" => {
                    let signal = match value {
                        Value::String(signal) if signal.len() <= 32 => signal.clone(),
                        Value::Number(signal) => signal.to_string(),
                        _ => continue,
                    };
                    if !signal.is_empty()
                        && signal.len() <= 32
                        && signal.chars().all(|c| c.is_ascii_alphanumeric())
                    {
                        self.signal = Some(signal);
                    }
                }
                "stderr" | "error" | "message" | "details" | "aggregatedoutput"
                | "formattedoutput" | "result" | "output" | "data" => {
                    self.capture_raw(value, depth + 1);
                }
                _ => {}
            }
        }
    }

    pub(crate) fn message(&self) -> String {
        let status = match (&self.signal, self.exit_code) {
            (Some(signal), Some(code)) => {
                format!("Process ended with signal {signal} (exit code {code}).")
            }
            (Some(signal), None) => format!("Process ended with signal {signal}."),
            (None, Some(137)) => "Process exited with code 137 (SIGKILL).".to_owned(),
            (None, Some(code)) => format!("Process exited with code {code}."),
            (None, None) => String::new(),
        };
        let text = std::iter::once(status.as_str())
            .chain(self.messages.iter().map(String::as_str))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            "The tool failed without reporting a reason.".to_owned()
        } else {
            let mut bounded: String = text.chars().take(MAX_TEXT).collect();
            if text.chars().count() > MAX_TEXT {
                bounded.pop();
                bounded.push('…');
            }
            bounded
        }
    }
}

#[derive(Deserialize)]
struct SensitiveMarkers {
    keys: Vec<String>,
    prefixes: Vec<String>,
}

fn safe_text(text: &str) -> String {
    static MARKERS: OnceLock<SensitiveMarkers> = OnceLock::new();
    let markers = MARKERS.get_or_init(|| {
        serde_json::from_str(include_str!("../../../testdata/failure-sanitization.json"))
            .expect("embedded failure sanitization contract")
    });
    let mut chars = text.chars().peekable();
    let mut clean = String::new();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            if c == '\u{1b}' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if (!c.is_control() || matches!(c, '\n' | '\t'))
            && !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            clean.push(c);
        }
    }
    clean
        .lines()
        .filter_map(|line| {
            let lower = line.to_ascii_lowercase();
            let sensitive = markers.prefixes.iter().any(|prefix| lower.contains(prefix))
                || markers.keys.iter().any(|key| {
                    lower.match_indices(key).any(|(index, key)| {
                        let tail =
                            lower[index + key.len()..].trim_start_matches([' ', '\t', '\'', '"']);
                        tail.starts_with([':', '='])
                            || (matches!(key, "authorization" | "bearer" | "password" | "secret")
                                && lower[index + key.len()..].starts_with(char::is_whitespace))
                    })
                });
            (!sensitive && !line.trim().is_empty()).then_some(line)
        })
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{Content, ToolCallUpdateFields};
    use serde_json::json;

    #[test]
    fn all_text_blocks_and_structured_exit_survive_empty_terminal_update() {
        let mut failure = AcpToolFailure::default();
        failure.capture_update(&ToolCallUpdate::new(
            "command",
            ToolCallUpdateFields::new()
                .content(vec![
                    ToolCallContent::Content(Content::new("permission denied")),
                    ToolCallContent::Content(Content::new("try another checkout")),
                ])
                .raw_output(json!({"formatted_output": "", "exit_code": 137})),
        ));
        failure.capture_update(&ToolCallUpdate::new("command", ToolCallUpdateFields::new()));
        assert_eq!(
            failure.message(),
            "Process exited with code 137 (SIGKILL). permission denied try another checkout"
        );
    }

    #[test]
    fn initial_failed_call_preserves_nested_error_and_signal() {
        let mut failure = AcpToolFailure::default();
        failure.capture_call(&ToolCall::new("command", "Search").raw_output(json!({
            "result": {"exit_code": 2, "signal": "SIGTERM", "error": {"message": "file is missing"}},
            "unrelated": "do not display arbitrary metadata"
        })));
        assert_eq!(
            failure.message(),
            "Process ended with signal SIGTERM (exit code 2). file is missing"
        );
    }

    #[test]
    fn codex_output_and_terminal_exit_metadata_are_projected_without_metadata_dump() {
        let mut failure = AcpToolFailure::default();
        failure.capture_call(
            &ToolCall::new("command", "Search")
                .raw_output(json!({
                    "formatted_output": "search process was terminated", "exit_code": 137
                }))
                .meta(
                    json!({
                        "terminal_exit": {"exit_code": 137, "signal": "SIGKILL"},
                        "unrelated": "private adapter metadata"
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
        );
        assert_eq!(
            failure.message(),
            "Process ended with signal SIGKILL (exit code 137). search process was terminated"
        );
        let mut empty = AcpToolFailure::default();
        empty.capture_update(&ToolCallUpdate::new(
            "command",
            ToolCallUpdateFields::new()
                .raw_output(json!({"formatted_output": "", "exit_code": 137})),
        ));
        assert_eq!(empty.message(), "Process exited with code 137 (SIGKILL).");
    }

    #[test]
    fn codex_mcp_failure_retains_text_result_for_initial_and_streamed_calls() {
        let result = json!({
            "content": [{
                "type": "text",
                "text": "workspace.search error: Search failed (exit status: 2): rg: regex parse error:\n    (?:[)\n       ^\nerror: unclosed character class"
            }],
            "isError": true
        });
        let expected = "workspace.search error: Search failed (exit status: 2): rg: regex parse error: (?:[) ^ error: unclosed character class";
        let mut initial = AcpToolFailure::default();
        initial.capture_call(
            &ToolCall::new("exec-d2a637bd-e4dc-4761-8ce5-230d9dcfc741", "Search")
                .raw_output(result.clone()),
        );
        assert_eq!(initial.message(), expected);
        let mut streamed = AcpToolFailure::default();
        streamed.capture_update(&ToolCallUpdate::new(
            "exec-d2a637bd-e4dc-4761-8ce5-230d9dcfc741",
            ToolCallUpdateFields::new().raw_output(result),
        ));
        streamed.capture_update(&ToolCallUpdate::new(
            "exec-d2a637bd-e4dc-4761-8ce5-230d9dcfc741",
            ToolCallUpdateFields::new(),
        ));
        assert_eq!(streamed.message(), expected);
    }

    #[test]
    fn raw_mcp_content_only_reads_bounded_sanitised_text_blocks() {
        let mut failure = AcpToolFailure::default();
        failure.capture_update(&ToolCallUpdate::new(
            "mcp",
            ToolCallUpdateFields::new().raw_output(json!({
                "content": [
                    {"type": "image", "text": "not a text result", "data": "private image data"},
                    {"type": "text", "text": "Authorization: Bearer hidden\nrequest denied"},
                    {"type": "text", "text": "retry after signing in"}
                ],
                "isError": true,
                "unrelated": {"text": "private adapter metadata"}
            })),
        ));
        assert_eq!(failure.message(), "request denied retry after signing in");
    }

    #[test]
    fn raw_json_and_credentials_are_never_dumped() {
        let mut failure = AcpToolFailure::default();
        failure.capture_call(&ToolCall::new("command", "Search").raw_output(json!({
            "stderr": "Authorization: Bearer hidden\nrequest denied\napi_key=private\ninvalid sk-secret\nretry later",
            "token": "hidden", "other": {"message": "not a diagnostic field"}
        })));
        assert_eq!(failure.message(), "request denied retry later");
        failure.capture_text(r#"{"api\u005fkey":"private","message":"sign in again"}"#);
        assert!(failure.message().ends_with("sign in again"));
        assert!(!failure.message().contains("private"));
    }

    #[test]
    fn failure_projection_is_bounded_and_unicode_safe() {
        let mut failure = AcpToolFailure::default();
        for index in 0..100 {
            failure.capture_text(&format!("{index} {}", "é".repeat(10_000)));
        }
        assert_eq!(failure.messages.len(), MAX_MESSAGES);
        assert!(failure.message().chars().count() <= MAX_TEXT);
        assert!(failure.message().ends_with('…'));
        assert_eq!(
            AcpToolFailure::default().message(),
            "The tool failed without reporting a reason."
        );
    }

    #[test]
    fn controls_and_incomplete_json_do_not_reach_the_message() {
        let mut failure = AcpToolFailure::default();
        failure.capture_text("failed\u{1b}[31m hard\u{202e} text\u{1b}]0;hidden title\u{7}");
        failure.capture_text(&format!("{{\"unrelated\":\"{}\"}}", "x".repeat(MAX_TEXT)));
        assert_eq!(failure.message(), "failed hard text");
    }
}
