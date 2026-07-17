//! Normalizing raw GitHub webhook payloads into small internal events.
//!
//! GitHub's payloads are large and event-specific. Ingestion only needs a few
//! fields, so each verified delivery is reduced to a [`NormalizedEvent`] — the
//! event kind plus the handful of fields the runtime acts on. Event types the
//! runtime does not model degrade to [`NormalizedEvent::Other`] rather than
//! failing, mirroring the protocol crate's `Unknown` fallbacks.

use serde_json::Value;

use super::WebhookError;

/// A GitHub webhook delivery reduced to the fields ingestion cares about.
#[derive(Debug, Clone, PartialEq)]
pub enum NormalizedEvent {
    /// GitHub's `ping`, sent once when a webhook is first configured.
    Ping,
    /// A `pull_request` event (opened, synchronized, closed, …).
    PullRequest {
        action: String,
        number: u64,
        repository: String,
    },
    /// A `check_run` event carrying CI status.
    CheckRun {
        action: String,
        name: String,
        status: String,
        repository: String,
    },
    /// A `push` event; `git_ref` is the fully-qualified ref (`refs/heads/main`).
    Push { git_ref: String, repository: String },
    /// Any event type the runtime does not model.
    Other { event_type: String },
}

/// Normalize a verified delivery. `event_type` is the `X-GitHub-Event` header;
/// `body` is the raw JSON payload.
///
/// A body that is not valid JSON is a [`WebhookError::Malformed`]. Missing
/// fields default to empty/zero rather than failing, since GitHub payload shapes
/// vary across event actions.
pub fn normalize(event_type: &str, body: &[u8]) -> Result<NormalizedEvent, WebhookError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| WebhookError::Malformed(format!("invalid JSON body: {error}")))?;

    let event = match event_type {
        "ping" => NormalizedEvent::Ping,
        "pull_request" => NormalizedEvent::PullRequest {
            action: string_field(&value, "action"),
            number: value
                .get("pull_request")
                .and_then(|pr| pr.get("number"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            repository: repository_slug(&value),
        },
        "check_run" => {
            let check_run = value.get("check_run");
            NormalizedEvent::CheckRun {
                action: string_field(&value, "action"),
                name: nested_string(check_run, "name"),
                status: nested_string(check_run, "status"),
                repository: repository_slug(&value),
            }
        }
        "push" => NormalizedEvent::Push {
            git_ref: string_field(&value, "ref"),
            repository: repository_slug(&value),
        },
        other => NormalizedEvent::Other {
            event_type: other.to_string(),
        },
    };
    Ok(event)
}

/// The string at `key` on the top-level object, or empty when absent.
fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The string at `key` on a nested optional object, or empty when absent.
fn nested_string(object: Option<&Value>, key: &str) -> String {
    object
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The `owner/repo` slug from `repository.full_name`, or empty when absent.
fn repository_slug(value: &Value) -> String {
    value
        .get("repository")
        .and_then(|repository| repository.get("full_name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_request_normalizes() {
        let body = br#"{
            "action": "opened",
            "pull_request": { "number": 42 },
            "repository": { "full_name": "octocat/hello-world" }
        }"#;
        let event = normalize("pull_request", body).expect("normalizes");
        assert_eq!(
            event,
            NormalizedEvent::PullRequest {
                action: "opened".to_string(),
                number: 42,
                repository: "octocat/hello-world".to_string(),
            }
        );
    }

    #[test]
    fn check_run_normalizes() {
        let body = br#"{
            "action": "completed",
            "check_run": { "name": "build", "status": "completed" },
            "repository": { "full_name": "octocat/hello-world" }
        }"#;
        let event = normalize("check_run", body).expect("normalizes");
        assert_eq!(
            event,
            NormalizedEvent::CheckRun {
                action: "completed".to_string(),
                name: "build".to_string(),
                status: "completed".to_string(),
                repository: "octocat/hello-world".to_string(),
            }
        );
    }

    #[test]
    fn ping_normalizes() {
        let event = normalize("ping", br#"{ "zen": "Design for failure." }"#).expect("normalizes");
        assert_eq!(event, NormalizedEvent::Ping);
    }

    #[test]
    fn unknown_event_maps_to_other() {
        let event = normalize("issues", br#"{}"#).expect("normalizes");
        assert_eq!(
            event,
            NormalizedEvent::Other {
                event_type: "issues".to_string(),
            }
        );
    }

    #[test]
    fn malformed_body_errors() {
        let error = normalize("pull_request", b"not json at all").expect_err("must fail");
        assert!(matches!(error, WebhookError::Malformed(_)));
    }
}
