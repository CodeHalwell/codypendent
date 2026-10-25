#[cfg(feature = "provider-openai")]
use agent_framework_core::types::{Content, Message};
use sha2::{Digest, Sha256};

#[cfg(feature = "provider-openai")]
use crate::agent::{ModelStep, StepOutcome, ToolDefinition};

pub fn encode(name: &str) -> String {
    if !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return name.into();
    }
    let readable: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .take(36)
        .collect();
    format!(
        "cpd_{readable}_{}",
        &hex::encode(Sha256::digest(name.as_bytes()))[..20]
    )
}

#[cfg(feature = "provider-openai")]
pub fn encode_history(messages: &mut [Message]) {
    for message in messages {
        for content in &mut message.contents {
            if let Content::FunctionCall(call) = content {
                call.name = encode(&call.name);
            }
        }
    }
}

#[cfg(feature = "provider-openai")]
pub fn decode_step(mut outcome: StepOutcome, tools: &[ToolDefinition]) -> StepOutcome {
    let restore = |name: &mut String| {
        if let Some(tool) = tools.iter().find(|tool| encode(&tool.name) == *name) {
            name.clone_from(&tool.name);
        }
    };
    if let ModelStep::CallTool { tool, .. } = &mut outcome.step {
        restore(tool);
    }
    for call in &mut outcome.extra_calls {
        restore(&mut call.tool);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_framework_core::tools::{ApprovalMode, ToolKind};
    use serde_json::json;

    #[test]
    fn hosted_tool_names_are_bounded_and_do_not_collide_after_punctuation_replacement() {
        let long = "x".repeat(100);
        let names = [
            "desktop.custom.foo",
            "desktop_custom_foo",
            "desktop.custom_foo",
            &long,
        ];
        let encoded: std::collections::HashSet<String> =
            names.iter().map(|name| encode(name)).collect();
        assert_eq!(encoded.len(), names.len());
        assert!(encoded.iter().all(|name| name.len() <= 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))));
    }

    #[test]
    fn hosted_tool_names_restore_every_parallel_call_to_the_canonical_runtime_name() {
        let tool = ToolDefinition {
            name: "desktop.custom.double".into(),
            description: "Double".into(),
            parameters: json!({"type":"object"}),
            kind: ToolKind::Function,
            approval_mode: ApprovalMode::NeverRequire,
            executor: None,
        };
        let wire = encode(&tool.name);
        let outcome = StepOutcome::unmeasured(ModelStep::CallTool {
            tool: wire.clone(),
            args: json!({}),
        })
        .with_extra_calls(vec![crate::agent::ToolCallRequest {
            tool: wire,
            args: json!({}),
        }]);
        let decoded = decode_step(outcome, &[tool]);
        assert!(
            matches!(decoded.step, ModelStep::CallTool { tool, .. } if tool == "desktop.custom.double")
        );
        assert_eq!(decoded.extra_calls[0].tool, "desktop.custom.double");
    }
}
