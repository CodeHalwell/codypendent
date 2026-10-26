//! ACP (Agent Client Protocol) *client* — the inverse of `acp.rs`.
//!
//! `acp.rs` is the SERVER role (Codypendent serves ACP to Zed). This module is
//! the CLIENT/host role: Codypendent spawns an external ACP agent
//! (`gemini --acp`, `npx @agentclientprotocol/claude-agent-acp`, ...), does the
//! initialize/session handshake, delegates a run's objective as an ACP prompt,
//! and maps the agent's streamed `session/update`s onto Codypendent's existing
//! `EventBody` model. The agent owns its model catalog — the handshake's
//! `config_options` advertise it ([`AcpClient::discovered_models`]), and a
//! profile pinned to one of those models switches the session with
//! `session/set_config_option` ([`AcpClient::set_model`]); nothing is ever
//! guessed on the agent's behalf.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use agent_client_protocol::schema::v1::{
    ConfigOptionUpdate, ContentBlock, ContentChunk, CurrentModeUpdate, SessionConfigKind,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelect,
    SessionConfigSelectOptions, SessionUpdate, ToolCall, ToolCallLocation, ToolCallStatus,
    ToolCallUpdate, ToolKind,
};
// The ACP *client* surface (see the client section below). `ContentBlock` above
// is shared with the Task 6 mapping; the rest are client-only.
use agent_client_protocol::schema::v1::{
    AuthenticateRequest, CancelNotification, InitializeRequest, InitializeResponse,
    McpServer as WireMcpServer, McpServerStdio, NewSessionRequest, NewSessionResponse,
    PermissionOption as WirePermissionOption, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigId, SessionConfigOptionValue, SessionNotification,
    SetSessionConfigOptionRequest, StopReason, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectTo, ConnectionTo, Lines,
};

use crate::acp::PermissionOption;
use crate::acp_tool_failure::AcpToolFailure;
use crate::poison::lock_recovering;
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
/// - `Plan`, `AvailableCommandsUpdate`, and `SessionInfoUpdate` are ACP
///   session/UI concepts with no Codypendent parallel.
/// - `UsageUpdate` carries token/cost accounting; turning it into an
///   `EventBody::BudgetWarning` would fabricate a threshold breach that never
///   happened — the same cost-honesty rule that keeps the provider catalog's
///   cost metadata display-only and out of any budget sum.
///
/// `CurrentModeUpdate` and `ConfigOptionUpdate` DO map — to run-scoped
/// `NoteAppended` events — so an agent-initiated model or mode switch surfaces
/// in the transcript instead of silently changing what the run executes on.
///
/// The inverse of the server-side bridge in `crates/cli/src/acp.rs`.
#[must_use]
pub fn session_update_to_events(update: &SessionUpdate, run_id: RunId) -> Vec<EventBody> {
    match update {
        // The two are NOT the same channel and were merged here until v0.12.2.
        // ACP agents that deliberate out loud sent their reasoning as
        // `AgentThoughtChunk`; flattening it into the reply meant the answer
        // arrived buried under the model's own narration, and there was no way
        // for any client to tell the two apart because the distinction was
        // discarded one line from where it arrived.
        SessionUpdate::AgentMessageChunk(chunk) => model_stream_delta(chunk, run_id, false),
        SessionUpdate::AgentThoughtChunk(chunk) => model_stream_delta(chunk, run_id, true),
        SessionUpdate::ToolCall(tool_call) => tool_started(tool_call, run_id),
        SessionUpdate::ToolCallUpdate(tool_call_update) => tool_completed(tool_call_update, run_id),
        SessionUpdate::CurrentModeUpdate(update) => current_mode_note(update, run_id),
        SessionUpdate::ConfigOptionUpdate(update) => config_option_notes(update, run_id),
        // No Codypendent `EventBody` equivalent (see the doc comment above) —
        // covers `UserMessageChunk`, `Plan`, `AvailableCommandsUpdate`,
        // `SessionInfoUpdate`, `UsageUpdate`, and any variant a future ACP
        // schema bump adds that this build does not know yet (`SessionUpdate`
        // is `#[non_exhaustive]` — RULE 1: unknown wire content is handled
        // safely, not a hard error).
        _ => Vec::new(),
    }
}

/// The agent switched its own session mode mid-turn. Codypendent has no mode
/// event, so this surfaces as a run-scoped note — visible in the transcript,
/// never mistaken for model output.
fn current_mode_note(update: &CurrentModeUpdate, run_id: RunId) -> Vec<EventBody> {
    vec![EventBody::NoteAppended {
        text: format!(
            "ACP agent switched session mode to `{}`",
            update.current_mode_id
        ),
        run_id: Some(run_id),
    }]
}

/// The agent re-announced its session configuration (the full set arrives on
/// every change). Only the Model/Mode selector categories are noted — one note
/// per selector, naming the now-current selection — so an agent-initiated
/// model switch surfaces without spamming a note for every unrelated toggle.
fn config_option_notes(update: &ConfigOptionUpdate, run_id: RunId) -> Vec<EventBody> {
    update
        .config_options
        .iter()
        .filter_map(|option| {
            let label = match option.category {
                Some(SessionConfigOptionCategory::Model) => "model",
                Some(SessionConfigOptionCategory::Mode) => "mode",
                // Other categories (thought level, misc model config, unknown
                // future ones — the enum is `#[non_exhaustive]`) stay silent.
                _ => return None,
            };
            let SessionConfigKind::Select(select) = &option.kind else {
                return None;
            };
            let current = select.current_value.to_string();
            let name = select_entries(select)
                .into_iter()
                .find(|(id, _)| *id == current)
                .map(|(_, name)| name);
            let text = match name {
                Some(name) if name != current => {
                    format!("ACP agent {label} is now `{name}` ({current})")
                }
                _ => format!("ACP agent {label} is now `{current}`"),
            };
            Some(EventBody::NoteAppended {
                text,
                run_id: Some(run_id),
            })
        })
        .collect()
}

/// Flatten a select option's `(value id, display name)` pairs, whether the
/// agent grouped them or not. An options shape this build does not know
/// (`SessionConfigSelectOptions` is `#[non_exhaustive]`) flattens to nothing
/// rather than failing.
fn select_entries(select: &SessionConfigSelect) -> Vec<(String, String)> {
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .map(|option| (option.value.to_string(), option.name.clone()))
            .collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .map(|option| (option.value.to_string(), option.name.clone()))
            .collect(),
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
fn model_stream_delta(chunk: &ContentChunk, run_id: RunId, thought: bool) -> Vec<EventBody> {
    let ContentBlock::Text(text) = &chunk.content else {
        return Vec::new();
    };
    if text.text.is_empty() {
        return Vec::new();
    }
    vec![EventBody::ModelStreamDelta {
        run_id,
        text: text.text.clone(),
        thought,
    }]
}

/// A newly-initiated tool call maps to `EventBody::ToolStarted`. `args_digest`
/// stays empty: the agent built these arguments, not Codypendent's own tool
/// executor, so there is no digest comparable to the native path's
/// `hash_json` (`crates/runtime/src/agent.rs`) to record here — never
/// fabricate one. The machine tool name carries the ACP call's stable identity;
/// its human-readable title is the display label. ACP completion updates can
/// omit or change their title, and several calls can share the same title, so
/// matching starts and completions by that title loses their lifecycle.
fn tool_started(tool_call: &ToolCall, run_id: RunId) -> Vec<EventBody> {
    let mut events = vec![EventBody::ToolStarted {
        run_id,
        tool: acp_tool_identity(&tool_call.tool_call_id.to_string()),
        args_digest: String::new(),
        label: Some(acp_tool_display_label(
            &tool_call.title,
            Some(tool_call.kind),
            tool_call.raw_input.as_ref(),
            &tool_call.locations,
        )),
    }];
    // A ToolCall can arrive already completed (for example during an agent's
    // own replay); it must not leave a newly-created running card behind.
    let mut failure = AcpToolFailure::default();
    failure.capture_call(tool_call);
    if let Some(outcome) = tool_outcome(tool_call.status, Some(&failure)) {
        events.push(EventBody::ToolCompleted {
            run_id,
            tool: acp_tool_identity(&tool_call.tool_call_id.to_string()),
            outcome,
            artifact: None,
        });
    }
    events
}

/// A tool call update maps to `EventBody::ToolCompleted` only once it reaches
/// a terminal status. `Pending`/`InProgress` — or an update that does not
/// touch `status` at all — is not terminal yet. An input/title refinement is a
/// new snapshot of the same identified call: clients upsert it rather than
/// append another card. Claude announces "Terminal" before its command has
/// streamed; discarding those refinements hid every command and file target.
fn tool_completed(update: &ToolCallUpdate, run_id: RunId) -> Vec<EventBody> {
    let mut events = Vec::new();
    // Useful details may arrive only in the terminal update. Persist them
    // before the outcome, but do not let a generic title-only update erase an
    // already informative command or path.
    let input = update.fields.raw_input.as_ref();
    let has_target = [
        "command",
        "cmd",
        "file_path",
        "filePath",
        "path",
        "pattern",
        "query",
    ]
    .iter()
    .any(|key| {
        input
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    });
    let has_location = update
        .fields
        .locations
        .as_ref()
        .is_some_and(|locations| !locations.is_empty());
    let title = update.fields.title.as_deref().unwrap_or("Tool");
    let generic_title = title.len() <= 64
        && matches!(
            title.trim().to_ascii_lowercase().as_str(),
            "" | "tool"
                | "terminal"
                | "shell"
                | "bash"
                | "read file"
                | "read_file"
                | "write file"
                | "write_file"
                | "edit file"
                | "edit_file"
                | "file"
                | "search"
                | "delete"
                | "fetch"
                | "think"
        );
    if has_target || has_location || !generic_title {
        events.push(EventBody::ToolUpdated {
            run_id,
            tool: acp_tool_identity(&update.tool_call_id.to_string()),
            label: Some(acp_tool_display_label(
                title,
                update.fields.kind,
                input,
                update.fields.locations.as_deref().unwrap_or_default(),
            )),
        });
    }
    if let Some(outcome) = update.fields.status.and_then(|status| {
        let mut failure = AcpToolFailure::default();
        failure.capture_update(update);
        tool_outcome(status, Some(&failure))
    }) {
        events.push(EventBody::ToolCompleted {
            run_id,
            tool: acp_tool_identity(&update.tool_call_id.to_string()),
            outcome,
            artifact: None,
        });
    }
    events
}

fn acp_tool_identity(tool_call_id: &str) -> String {
    format!("acp:{tool_call_id}")
}

/// Preserve useful action context, never serialize arbitrary arguments or file
/// contents. ACP labels use "kind · action" so every surface can show the same
/// human type and target while keeping the machine call identity private.
fn acp_tool_display_label(
    title: &str,
    kind: Option<ToolKind>,
    input: Option<&Value>,
    locations: &[ToolCallLocation],
) -> String {
    let input_text = |key: &str| {
        input
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
    };
    let command = input_text("command").or_else(|| input_text("cmd"));
    let path = input_text("file_path")
        .or_else(|| input_text("filePath"))
        .or_else(|| input_text("path"));
    let action = command
        .or(path)
        .or_else(|| input_text("pattern"))
        .or_else(|| input_text("query"))
        .map(std::borrow::Cow::Borrowed)
        .or_else(|| {
            locations
                .first()
                .map(|location| location.path.to_string_lossy())
        })
        .unwrap_or(std::borrow::Cow::Borrowed(title));
    let category = match kind {
        Some(ToolKind::Execute) => "Terminal",
        Some(ToolKind::Read) => "Read File",
        Some(ToolKind::Edit) => "Edit File",
        Some(ToolKind::Delete) => "Delete",
        Some(ToolKind::Search) => "Search",
        Some(ToolKind::Fetch) => "Fetch",
        Some(ToolKind::Think) => "Think",
        _ if command.is_some() => "Terminal",
        _ if path.is_some() => "File",
        _ => title,
    };
    const MAX_LABEL_CHARS: usize = 200;
    let category = bounded_label_text(category, MAX_LABEL_CHARS);
    let action = bounded_label_text(&action, MAX_LABEL_CHARS);
    if action.is_empty() || action == category {
        category
    } else {
        bounded_label_text(&format!("{category} · {action}"), MAX_LABEL_CHARS)
    }
}

fn bounded_label_text(text: &str, limit: usize) -> String {
    let mut chars = Vec::with_capacity(limit);
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = !chars.is_empty();
            continue;
        }
        if ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            continue;
        }
        if space {
            chars.push(' ');
            space = false;
        }
        chars.push(ch);
        if chars.len() > limit {
            chars.truncate(limit.saturating_sub(1));
            chars.push('…');
            break;
        }
    }
    chars.into_iter().collect()
}

fn tool_outcome(status: ToolCallStatus, failure: Option<&AcpToolFailure>) -> Option<ToolOutcome> {
    match status {
        ToolCallStatus::Completed => Some(ToolOutcome::Succeeded),
        ToolCallStatus::Failed => Some(ToolOutcome::Failed {
            message: failure.cloned().unwrap_or_default().message(),
        }),
        _ => None,
    }
}

// ===========================================================================
// ACP CLIENT — spawn/connect an external agent, handshake, delegate a prompt.
// ===========================================================================
//
// The real `agent-client-protocol` 2.0.0 client API is closure-scoped: one
// `Client.builder()…connect_with(transport, main_fn)` call owns the connection
// for the whole lifetime of `main_fn`, and the agent's streamed `session/update`
// notifications + `session/request_permission` requests are delivered to
// callbacks registered on the *builder* (not to a `Client` trait we implement,
// and there is no `ClientSideConnection` handle — the plan drafted an older
// shape). To still expose a reusable `connect` / `prompt` split, we run that one
// call on a background task and bridge it with channels: `prompt` sends a
// command in, and drains mapped events / permission asks back out to feed the
// caller's [`AcpEventSink`]. `session_update_to_events` (above) stays the single
// translation point for every streamed update.

/// Bounded depth of the command channel feeding the connection driver. A prompt
/// is delegated one at a time (`prompt` takes `&mut self`), so a shallow queue
/// suffices; it only decouples the caller's `send` from the driver's `recv`.
const PROMPT_QUEUE_DEPTH: usize = 8;
/// Bound agent-to-host updates so a noisy external client is backpressured by
/// durable event/approval processing instead of growing the daemon heap.
const PROMPT_EVENT_QUEUE_DEPTH: usize = 256;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(90);

/// Why an ACP prompt turn ended. Mirrors [`crate::acp::StopReason`] (the server
/// role's type) but is owned by the client role so the two directions stay
/// independent. The ACP wire distinguishes more terminal reasons than
/// Codypendent models: `max_tokens` / `max_turn_requests` collapse into
/// `EndTurn` (the turn simply ended), and any future variant does too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpStopReason {
    /// The agent finished its turn (ACP `end_turn`, `max_tokens`, `max_turn_requests`).
    EndTurn,
    /// The turn was cancelled (ACP `cancelled`).
    Cancelled,
    /// The agent declined to act on the prompt (ACP `refusal`).
    Refusal,
}

/// A failure in the ACP client.
#[derive(Debug, thiserror::Error)]
pub enum AcpClientError {
    /// An I/O failure on the transport.
    #[error("acp client I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The `initialize` / `session/new` handshake did not complete.
    #[error("acp handshake failed: {0}")]
    Handshake(String),
    /// Delegating or streaming a prompt turn failed.
    #[error("acp prompt failed: {0}")]
    Prompt(String),
}

fn streamed_provider_failure(text: &str) -> Option<String> {
    let content = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("Warning: "))
        .collect::<Vec<_>>()
        .join("\n");
    let value: Value = serde_json::from_str(content.trim()).ok()?;
    if value.get("type")?.as_str()? != "error"
        || !(400..600).contains(&value.get("status")?.as_u64()?)
    {
        return None;
    }
    let message = value.pointer("/error/message")?.as_str()?.trim();
    if message.is_empty() {
        return None;
    }
    Some(bounded_label_text(message, 4096))
}

/// One model advertised by a connected ACP agent — a `session/new`
/// `config_options` entry with `category: "model"`, flattened to what a picker
/// needs. `id` is the agent's own stable model id (the `SessionConfigValueId`
/// that `session/set_config_option` accepts back).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpModel {
    /// The agent's stable id for this model.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Whether the agent reports this as the currently-selected model.
    pub current: bool,
}

/// One session mode advertised by a connected ACP agent, from the `session/new`
/// `modes` state (or a Mode-category config option when no dedicated state was
/// sent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpMode {
    /// The agent's stable id for this mode.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Whether the agent reports this as the current mode.
    pub current: bool,
}

/// One authentication method advertised in the agent's `initialize` response.
/// Surfaced verbatim so a failed `session/new` can tell the user exactly what
/// the agent asked for, in the agent's own words, rather than an opaque
/// handshake failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpAuthMethod {
    /// The agent's stable id for this method.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Optional agent-provided instructions.
    pub description: Option<String>,
}

/// A launch spec for one MCP stdio server, forwarded to the agent in
/// `session/new`'s `mcp_servers` so an external agent inherits the same tool
/// servers a native run would see. Deliberately carries NO environment pairs:
/// `mcp.toml`'s `env` is the operator's channel for secrets, and secrets never
/// cross into an external vendor process (see [`forwardable_mcp_servers`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpMcpServer {
    /// Server name (the `mcp.toml` server id).
    pub name: String,
    /// The executable to spawn.
    pub command: String,
    /// Arguments to the executable.
    pub args: Vec<String>,
}

/// Options applied to the ACP session created during the handshake.
#[derive(Debug, Clone, Default)]
pub struct AcpSessionOptions {
    /// MCP servers the agent should connect to (`session/new.mcpServers`).
    pub mcp_servers: Vec<AcpMcpServer>,
    /// Reuse a validated existing vendor session without forwarding its tokens.
    pub cached_auth: Option<AcpCachedAuth>,
    /// Original Google home; the process receives an isolated MCP configuration.
    pub google_home: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct AcpCachedAuth {
    method_id: String,
    profile: PathBuf,
}

pub struct AcpResolvedConnection {
    pub client: AcpClient,
    pub spec: crate::acp_registry::AcpLaunchSpec,
    /// An exact advertised replacement; present only after Google's retirement response.
    pub selected_model: Option<String>,
}

impl AcpSessionOptions {
    /// Only the official Google family requires an explicit cached-sign-in step.
    #[must_use]
    pub fn for_agent(spec: &crate::acp_registry::AcpLaunchSpec) -> Self {
        let google_home = (spec.registry_id == "antigravity-acp")
            .then(|| {
                spec.env
                    .get("GEMINI_HOME")
                    .map(PathBuf::from)
                    .or_else(|| std::env::var_os("GEMINI_HOME").map(PathBuf::from))
                    .or_else(|| {
                        spec.env
                            .get("HOME")
                            .map(PathBuf::from)
                            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                            .map(|home| home.join(".gemini"))
                    })
            })
            .flatten();
        let cached_auth = google_home.as_ref().and_then(|home| {
            [
                home.join("antigravity-acp/acp_token.json"),
                home.join("antigravity-cli/antigravity-oauth-token"),
                home.join("jetski-standalone-oauth-token"),
            ]
            .into_iter()
            .find(|path| cached_google_personal_profile(path))
            .map(|profile| AcpCachedAuth {
                method_id: "oauth-personal".into(),
                profile,
            })
        });
        Self {
            cached_auth,
            google_home,
            ..Self::default()
        }
    }
}

fn bounded_auth_profile(path: &std::path::Path) -> Option<Value> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(65_537)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= 65_536)
        .then(|| serde_json::from_slice(&bytes).ok())
        .flatten()
}

fn cached_google_personal_profile(path: &std::path::Path) -> bool {
    let Some(profile) = bounded_auth_profile(path) else {
        return false;
    };
    if profile.get("auth_method").and_then(Value::as_str) != Some("consumer") {
        return false;
    }
    let Some(token) = profile.get("token").and_then(Value::as_object) else {
        return false;
    };
    let credential = |name: &str| {
        token
            .get(name)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty() && value.len() <= 16_384)
    };
    let Some(expiry) = token
        .get("expiry")
        .and_then(Value::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
    else {
        return false;
    };
    credential("refresh_token") || (credential("access_token") && expiry > chrono::Utc::now())
}

fn google_subject(profile: &Value) -> Option<String> {
    use base64::Engine;
    let token = profile.get("id_token")?.as_str()?;
    if token.len() > 16_384 {
        return None;
    }
    let mut parts = token.split('.');
    let _ = parts.next()?;
    let payload = parts.next()?;
    let _ = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let claims: Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .ok()?,
    )
    .ok()?;
    if !matches!(
        claims.get("iss")?.as_str()?,
        "accounts.google.com" | "https://accounts.google.com"
    ) {
        return None;
    }
    claims
        .get("sub")?
        .as_str()
        .filter(|subject| !subject.is_empty() && subject.len() <= 256)
        .map(str::to_owned)
}

fn same_google_account(auth: &AcpCachedAuth, spec: &crate::acp_registry::AcpLaunchSpec) -> bool {
    let legacy_home = spec
        .env
        .get("GEMINI_CLI_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("GEMINI_CLI_HOME").map(PathBuf::from))
        .or_else(|| {
            spec.env
                .get("HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                .map(|home| home.join(".gemini"))
        });
    let legacy = legacy_home
        .and_then(|home| bounded_auth_profile(&home.join("oauth_creds.json")))
        .and_then(|profile| google_subject(&profile));
    let current = bounded_auth_profile(&auth.profile).and_then(|profile| google_subject(&profile));
    legacy.is_some() && legacy == current
}

fn retired_google_individual_client(error: &AcpClientError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("no longer supported")
        && message.contains("gemini code assist for individuals")
        && message.contains("antigravity.google")
}

fn recovery_gemini_model(models: &[AcpModel], requested: Option<&str>) -> Option<String> {
    let gemini = |model: &&AcpModel| model.id.to_ascii_lowercase().starts_with("gemini-");
    if let Some(requested) = requested {
        return models
            .iter()
            .filter(gemini)
            .find(|model| model.id == requested)
            .map(|model| model.id.clone());
    }
    models
        .iter()
        .filter(gemini)
        .find(|model| model.current)
        .or_else(|| models.iter().filter(gemini).next())
        .map(|model| model.id.clone())
}

fn agent_mcp_launch(
    command: &str,
    args: &[String],
    mut options: AcpSessionOptions,
) -> (Vec<String>, AcpSessionOptions) {
    let mut args = args.to_vec();
    if std::path::Path::new(command)
        .file_stem()
        .is_some_and(|name| name == "copilot")
        && !options.mcp_servers.is_empty()
    {
        let servers: serde_json::Map<String, Value> = options
            .mcp_servers
            .drain(..)
            .map(|server| {
                (server.name, serde_json::json!({"type":"local","command":server.command,"args":server.args,"tools":["*"]}))
            })
            .collect();
        args.extend([
            "--additional-mcp-config".to_string(),
            serde_json::json!({"mcpServers":servers}).to_string(),
        ]);
    }
    (args, options)
}

/// Read `mcp.toml` at `mcp_toml` and derive the launch specs safe to forward to
/// an external ACP agent: name + command + args only, so a delegated session
/// reaches the same tool servers a native run would.
///
/// Two classes of server are withheld rather than forwarded half-configured:
/// one declaring explicit `env` pairs (that is the operator's channel for
/// secrets, and secrets never cross into an external vendor process), and one
/// declaring `inherit_environment = false` (the external agent launches it, so
/// the hermetic environment that server asked for cannot be promised here).
///
/// Forwarding is ON by default; `[acp] forward_mcp_servers = false` in
/// `mcp.toml` — a table the MCP loader itself ignores — turns it off. A missing
/// or malformed file forwards nothing: the daemon already reports the parse
/// error loudly at boot, so this stays quiet rather than double-reporting.
#[must_use]
pub fn forwardable_mcp_servers(mcp_toml: &std::path::Path) -> Vec<AcpMcpServer> {
    #[derive(Deserialize)]
    struct ForwardFile {
        #[serde(default)]
        acp: ForwardTable,
    }
    #[derive(Default, Deserialize)]
    struct ForwardTable {
        #[serde(default)]
        forward_mcp_servers: Option<bool>,
    }

    // The validated loader owns the `[[server]]` shape; this only reads the
    // one extra opt-out key off the same file.
    let Ok(config) = crate::mcp::load_mcp_config(mcp_toml) else {
        return Vec::new();
    };
    let forward = std::fs::read_to_string(mcp_toml)
        .ok()
        .and_then(|text| toml::from_str::<ForwardFile>(&text).ok())
        .and_then(|file| file.acp.forward_mcp_servers)
        .unwrap_or(true);
    if !forward {
        return Vec::new();
    }
    config
        .servers
        .into_iter()
        .filter(|server| server.env.is_empty() && server.inherit_environment)
        .map(|server| AcpMcpServer {
            name: server.name,
            command: server.command,
            args: server.args,
        })
        .collect()
}

/// What the handshake taught us about the agent, shared between the connection
/// driver (which populates it before signalling ready and refreshes it on
/// `ConfigOptionUpdate` / `set_config_option` responses) and the
/// [`AcpClient`]'s accessors.
#[derive(Debug, Clone, Default)]
struct AcpDiscovery {
    auth_methods: Vec<AcpAuthMethod>,
    models: Vec<AcpModel>,
    modes: Vec<AcpMode>,
    /// The `SessionConfigId` of the Model-category selector, kept so
    /// [`AcpClient::set_model`] can target it.
    model_config_id: Option<String>,
    /// Whether `modes` came from the response's dedicated `modes` state (the
    /// richer source); a Mode-category config option never overwrites it then.
    modes_from_state: bool,
    models_from_legacy: bool,
    handshake_phase: Option<&'static str>,
}

impl AcpDiscovery {
    fn apply_initialize(&mut self, response: &InitializeResponse) {
        self.auth_methods = response
            .auth_methods
            .iter()
            .map(|method| AcpAuthMethod {
                id: method.id().to_string(),
                name: method.name().to_string(),
                description: method.description().map(str::to_string),
            })
            .collect();
    }

    fn apply_session(&mut self, response: &NewSessionResponse) {
        if let Some(state) = &response.modes {
            self.modes_from_state = true;
            let current = state.current_mode_id.to_string();
            self.modes = state
                .available_modes
                .iter()
                .map(|mode| AcpMode {
                    current: mode.id.to_string() == current,
                    id: mode.id.to_string(),
                    name: mode.name.clone(),
                })
                .collect();
        }
        if let Some(options) = &response.config_options {
            self.apply_config_options(options);
        }
    }

    fn apply_legacy_models(&mut self, response: &Value) {
        if self.model_config_id.is_some() {
            return;
        }
        let Some(models) = response
            .pointer("/models/availableModels")
            .and_then(Value::as_array)
        else {
            return;
        };
        let current = response
            .pointer("/models/currentModelId")
            .and_then(Value::as_str);
        self.models = models
            .iter()
            .take(256)
            .filter_map(|model| {
                let id = model.get("modelId")?.as_str()?;
                if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
                    return None;
                }
                let name = model
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| name.len() <= 512)
                    .unwrap_or(id);
                Some(AcpModel {
                    id: id.into(),
                    name: name.into(),
                    current: current == Some(id),
                })
            })
            .collect();
        self.models_from_legacy = !self.models.is_empty();
    }

    /// Fold a full config-option set (from `session/new`, a
    /// `set_config_option` response, or a `ConfigOptionUpdate`) into the
    /// discovery state. Every arrival carries the COMPLETE current set, so the
    /// derived selectors are rebuilt from the whole slice rather than merged
    /// entry by entry — an agent that drops its model selector leaves no stale
    /// model list behind to pin a run to.
    fn apply_config_options(&mut self, options: &[SessionConfigOption]) {
        let mut models = Vec::new();
        let mut model_config_id = None;
        let mut modes = Vec::new();
        for option in options {
            let SessionConfigKind::Select(select) = &option.kind else {
                continue;
            };
            let current = select.current_value.to_string();
            let entries = || {
                select_entries(select)
                    .into_iter()
                    .map(|(id, name)| (id == current, id, name))
            };
            match option.category {
                Some(SessionConfigOptionCategory::Model) => {
                    model_config_id = Some(option.id.to_string());
                    models = entries()
                        .map(|(current, id, name)| AcpModel { id, name, current })
                        .collect();
                }
                Some(SessionConfigOptionCategory::Mode) => {
                    modes = entries()
                        .map(|(current, id, name)| AcpMode { id, name, current })
                        .collect();
                }
                // ModelConfig/ThoughtLevel/unknown categories (the enum is
                // `#[non_exhaustive]`) are not model discovery.
                _ => {}
            }
        }
        if model_config_id.is_some() || !self.models_from_legacy {
            self.models = models;
            self.models_from_legacy = false;
        }
        self.model_config_id = model_config_id;
        // `session/new`'s dedicated `modes` state is the richer source; a
        // Mode-category config option never overwrites it.
        if !self.modes_from_state {
            self.modes = modes;
        }
    }

    fn apply_current_mode(&mut self, mode_id: &str) {
        for mode in &mut self.modes {
            mode.current = mode.id == mode_id;
        }
    }
}

/// A parenthesized suffix for `session/new` failures naming the agent's
/// advertised authentication methods, in the agent's own words — turning an
/// opaque "session/new failed" into a message that says what to go do. Empty
/// when the agent advertised none: never fabricate a remedy it did not offer.
fn auth_methods_hint(methods: &[AcpAuthMethod]) -> String {
    if methods.is_empty() {
        return String::new();
    }
    let methods = methods
        .iter()
        .map(|method| match &method.description {
            Some(description) => format!("{} — {description}", method.name),
            None => method.name.clone(),
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(" (the agent advertises authentication: {methods})")
}

fn authentication_required(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "auth required",
        "authentication required",
        "not authenticated",
        "sign-in required",
        "sign in required",
        "login required",
        "not logged in",
        "unauthorized",
        "authentication failed",
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
}

/// Receives the events an ACP turn produces and answers the agent's permission
/// requests. The daemon implements this to fan mapped events into a run's ledger
/// and route permissions through the existing durable approval broker; tests
/// implement it to record events and auto-answer.
#[async_trait]
pub trait AcpEventSink: Send {
    /// A Codypendent event mapped from a streamed `session/update`.
    async fn on_event(&mut self, event: EventBody);

    /// Answer an ACP `session/request_permission`: return the chosen `optionId`,
    /// or `None` to cancel. `tool_call` is the agent's pending call as opaque
    /// JSON; `options` are the choices in the server role's [`PermissionOption`]
    /// shape (reused so both ACP directions speak one permission vocabulary).
    async fn on_permission(
        &mut self,
        tool_call: Value,
        options: Vec<PermissionOption>,
    ) -> Option<String>;
}

/// A connected ACP agent session. Dropping it closes the command channel, which
/// ends the driver's `main_fn`; the connection then shuts down and — for the
/// spawn path — [`AcpAgent`] tears down the child process group (SIGKILL on
/// Unix, covering `npx`/`uvx` wrapper descendants).
pub struct AcpClient {
    commands: mpsc::Sender<PromptCommand>,
    /// The driver task owns the live `agent_client_protocol` connection. Held so
    /// it is not detached mid-handshake; it exits on its own once `commands`
    /// (its only sender) is dropped.
    driver: JoinHandle<Result<(), AcpClientError>>,
    /// Handshake discovery (auth methods, models, modes), kept live by the
    /// driver as the agent announces config changes.
    discovery: Arc<Mutex<AcpDiscovery>>,
    google_home: Option<crate::acp_google_home::GoogleHome>,
}

/// A clonable handle that asks the agent to wind down the in-flight prompt
/// turn (`session/cancel`) without borrowing the [`AcpClient`] — so a caller
/// can hold it across the same `select!` that polls
/// [`prompt`](AcpClient::prompt). Best-effort: a closed connection means there
/// is nothing left to cancel.
#[derive(Clone)]
pub struct AcpCancelHandle {
    commands: mpsc::Sender<PromptCommand>,
}

impl AcpCancelHandle {
    /// Send the ACP `session/cancel` notification for the current turn. The
    /// agent then finishes the in-flight `session/prompt` with the
    /// wire-correct `cancelled` stop reason (or the caller times out and tears
    /// the process down).
    pub async fn cancel(&self) {
        let _ = self.commands.send(PromptCommand::Cancel).await;
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        // `JoinHandle` normally detaches on drop. An ACP prompt may still be
        // awaiting the external agent, so detaching would also retain its child
        // process. Aborting drops the SDK transport's process-group guard.
        self.driver.abort();
    }
}

struct PendingConnection(Option<tokio::task::AbortHandle>);

impl Drop for PendingConnection {
    fn drop(&mut self) {
        if let Some(driver) = &self.0 {
            driver.abort();
        }
    }
}

impl AcpClient {
    /// Connect over an existing byte transport (`reader` = the agent's output,
    /// `writer` = the agent's input) and complete the ACP handshake. Generic over
    /// the stream halves so an in-memory `tokio::io::duplex` can drive the whole
    /// client in tests; [`spawn`](Self::spawn) is the production entry point.
    pub async fn connect<R, W>(reader: R, writer: W, cwd: &str) -> Result<AcpClient, AcpClientError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::connect_with(reader, writer, cwd, AcpSessionOptions::default()).await
    }

    /// [`connect`](Self::connect) with explicit [`AcpSessionOptions`] (e.g. MCP
    /// servers to forward into `session/new`).
    pub async fn connect_with<R, W>(
        reader: R,
        writer: W,
        cwd: &str,
        options: AcpSessionOptions,
    ) -> Result<AcpClient, AcpClientError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::connect_transport(tokio_transport(reader, writer), cwd, options).await
    }

    /// Spawn `command args` (with `env`) as a child ACP agent and connect over
    /// its stdio. `env` carries the provider config's environment (secrets are
    /// referenced by NAME upstream and resolved into `env` by the caller — never
    /// stored here).
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &str,
    ) -> Result<AcpClient, AcpClientError> {
        Self::spawn_with(command, args, env, cwd, AcpSessionOptions::default()).await
    }

    /// [`spawn`](Self::spawn) with explicit [`AcpSessionOptions`] (e.g. MCP
    /// servers to forward into `session/new`).
    pub async fn spawn_with(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &str,
        options: AcpSessionOptions,
    ) -> Result<AcpClient, AcpClientError> {
        let (args, options) = agent_mcp_launch(command, args, options);
        let google_home = options
            .google_home
            .as_deref()
            .map(crate::acp_google_home::GoogleHome::prepare)
            .transpose()
            .map_err(AcpClientError::Io)?;
        let mut env = env.clone();
        if let Some(home) = &google_home {
            env.insert(
                "GEMINI_HOME".into(),
                home.path().to_string_lossy().into_owned(),
            );
        }
        let config = AcpAgentConfig::new(command).args(args).envs(env);
        let mut client = Self::connect_transport(AcpAgent::new(config), cwd, options).await?;
        client.google_home = google_home;
        Ok(client)
    }

    /// Recover only Google's explicitly retired individual-client route before any prompt.
    pub async fn spawn_with_google_recovery(
        spec: &crate::acp_registry::AcpLaunchSpec,
        coordinate: &str,
        data_dir: &std::path::Path,
        cwd: &str,
        options: AcpSessionOptions,
    ) -> Result<AcpResolvedConnection, AcpClientError> {
        let original = match Self::spawn_with(
            &spec.command.to_string_lossy(),
            &spec.args,
            &spec.env,
            cwd,
            options.clone(),
        )
        .await
        {
            Ok(client) => {
                return Ok(AcpResolvedConnection {
                    client,
                    spec: spec.clone(),
                    selected_model: None,
                })
            }
            Err(error) => error,
        };
        if crate::acp_registry::agent_id_from_coordinate(coordinate) != "gemini"
            || !retired_google_individual_client(&original)
        {
            return Err(original);
        }
        let Ok(replacement) =
            crate::acp_registry::AcpRegistryStore::new(data_dir).launch_spec("antigravity-acp")
        else {
            return Err(original);
        };
        let mut replacement_options = AcpSessionOptions::for_agent(&replacement);
        if !replacement_options
            .cached_auth
            .as_ref()
            .is_some_and(|auth| same_google_account(auth, spec))
        {
            return Err(original);
        }
        replacement_options.mcp_servers = options.mcp_servers;
        let client = Self::spawn_with(
            &replacement.command.to_string_lossy(),
            &replacement.args,
            &replacement.env,
            cwd,
            replacement_options,
        )
        .await
        .map_err(|error| {
            AcpClientError::Handshake(format!(
                "{original}; the existing Antigravity session could not connect: {error}"
            ))
        })?;
        let Some(selected_model) = recovery_gemini_model(
            &client.discovered_models(),
            crate::acp_registry::agent_model_from_coordinate(coordinate),
        ) else {
            return Err(original);
        };
        Ok(AcpResolvedConnection {
            client,
            spec: replacement,
            selected_model: Some(selected_model),
        })
    }

    async fn connect_transport<T>(
        transport: T,
        cwd: &str,
        options: AcpSessionOptions,
    ) -> Result<AcpClient, AcpClientError>
    where
        T: ConnectTo<Client> + Send + 'static,
    {
        let cwd = std::fs::canonicalize(cwd).map_err(AcpClientError::Io)?;
        if !cwd.is_dir() {
            return Err(AcpClientError::Handshake(format!(
                "session working directory is not a directory: {}",
                cwd.display()
            )));
        }
        let (ready_tx, ready_rx) = oneshot::channel();
        let (commands, command_rx) = mpsc::channel(PROMPT_QUEUE_DEPTH);
        let discovery = Arc::new(Mutex::new(AcpDiscovery {
            handshake_phase: Some("initialization"),
            ..AcpDiscovery::default()
        }));
        let driver = tokio::spawn(run_connection(
            transport,
            cwd,
            options,
            Arc::clone(&discovery),
            ready_tx,
            command_rx,
        ));
        let mut pending = PendingConnection(Some(driver.abort_handle()));
        let result = match tokio::time::timeout(HANDSHAKE_TIMEOUT, ready_rx).await {
            Err(_) => {
                driver.abort();
                let phase = lock_recovering(&discovery)
                    .handshake_phase
                    .unwrap_or("connection");
                Err(AcpClientError::Handshake(format!(
                    "agent did not complete {phase} within {} seconds",
                    HANDSHAKE_TIMEOUT.as_secs()
                )))
            }
            Ok(Ok(Ok(()))) => Ok(AcpClient {
                commands,
                driver,
                discovery,
                google_home: None,
            }),
            // The driver reported a specific handshake failure before returning.
            Ok(Ok(Err(error))) => {
                driver.abort();
                Err(error)
            }
            // The driver dropped `ready_tx` without signalling (e.g. the
            // transport itself failed before `main_fn` ran): recover its error.
            Ok(Err(_)) => match driver.await {
                Ok(Err(error)) => Err(error),
                Ok(Ok(())) => Err(AcpClientError::Handshake(
                    "acp connection closed before completing the handshake".to_string(),
                )),
                Err(join_error) => Err(AcpClientError::Handshake(format!(
                    "acp connection task failed: {join_error}"
                ))),
            },
        };
        if result.is_ok() {
            pending.0 = None;
        }
        result
    }

    /// Delegate `objective` to the agent as one ACP `session/prompt` turn,
    /// feeding every mapped `session/update` event and permission request to
    /// `sink`, and returning why the turn ended. The turn runs on whatever
    /// model the session currently has — the agent's default, or the one
    /// pinned earlier via [`set_model`](Self::set_model).
    pub async fn prompt(
        &mut self,
        objective: &str,
        run_id: RunId,
        sink: &mut dyn AcpEventSink,
    ) -> Result<AcpStopReason, AcpClientError> {
        let (events, mut incoming) = mpsc::channel(PROMPT_EVENT_QUEUE_DEPTH);
        self.commands
            .send(PromptCommand::Prompt {
                objective: objective.to_string(),
                run_id,
                events,
            })
            .await
            .map_err(|_| {
                AcpClientError::Prompt("acp connection is no longer running".to_string())
            })?;

        let mut assistant_text = String::new();
        while let Some(message) = incoming.recv().await {
            match message {
                PromptOut::Event(event) => {
                    if let EventBody::ModelStreamDelta {
                        text,
                        thought: false,
                        ..
                    } = &event
                    {
                        let remaining = (256 * 1024_usize).saturating_sub(assistant_text.len());
                        let end = (0..=text.len().min(remaining))
                            .rev()
                            .find(|end| text.is_char_boundary(*end))
                            .unwrap_or(0);
                        assistant_text.push_str(&text[..end]);
                    }
                    sink.on_event(event).await;
                }
                PromptOut::Permission {
                    tool_call,
                    options,
                    reply,
                } => {
                    let choice = sink.on_permission(tool_call, options).await;
                    // The driver's permission callback awaits this; if it is gone
                    // the turn is already ending, so a failed send is harmless.
                    let _ = reply.send(choice);
                }
                PromptOut::Done(stop) => {
                    if stop == AcpStopReason::EndTurn {
                        if let Some(message) = streamed_provider_failure(&assistant_text) {
                            return Err(AcpClientError::Prompt(message));
                        }
                    }
                    return Ok(stop);
                }
                PromptOut::Failed(reason) => return Err(AcpClientError::Prompt(reason)),
            }
        }
        Err(AcpClientError::Prompt(
            "acp connection closed before the prompt completed".to_string(),
        ))
    }

    /// Models advertised through current configuration options or legacy model
    /// state, in the agent's own order. Empty when it exposes no selector.
    #[must_use]
    pub fn discovered_models(&self) -> Vec<AcpModel> {
        lock_recovering(&self.discovery).models.clone()
    }

    /// The session modes the agent advertised, in the agent's own order.
    #[must_use]
    pub fn discovered_modes(&self) -> Vec<AcpMode> {
        lock_recovering(&self.discovery).modes.clone()
    }

    /// The authentication methods the agent advertised in `initialize`.
    #[must_use]
    pub fn auth_methods(&self) -> Vec<AcpAuthMethod> {
        lock_recovering(&self.discovery).auth_methods.clone()
    }

    /// Select a model through the advertised current or legacy protocol.
    /// Legacy requests require an exact advertised identifier.
    pub async fn set_model(&mut self, model_id: &str) -> Result<(), AcpClientError> {
        let config_id = {
            let discovery = lock_recovering(&self.discovery);
            if discovery.model_config_id.is_none()
                && (!discovery.models_from_legacy
                    || !discovery.models.iter().any(|model| model.id == model_id))
            {
                return Err(AcpClientError::Prompt(format!(
                    "the agent advertises no model selector matching `{model_id}`"
                )));
            }
            discovery.model_config_id.clone()
        };
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(PromptCommand::SetConfigOption {
                config_id,
                value: model_id.to_string(),
                reply,
            })
            .await
            .map_err(|_| {
                AcpClientError::Prompt("acp connection is no longer running".to_string())
            })?;
        match answer.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(reason)) => Err(AcpClientError::Prompt(reason)),
            Err(_) => Err(AcpClientError::Prompt(
                "acp connection closed before set_config_option resolved".to_string(),
            )),
        }
    }

    /// A clonable [`AcpCancelHandle`] for graceful turn cancellation. Taken
    /// BEFORE calling [`prompt`](Self::prompt) so the handle can be used while
    /// the prompt future holds the `&mut self` borrow.
    #[must_use]
    pub fn cancel_handle(&self) -> AcpCancelHandle {
        AcpCancelHandle {
            commands: self.commands.clone(),
        }
    }
}

/// A prompt turn's live routing: which run the streamed updates belong to and
/// where to push the mapped events / permission asks. The driver installs it for
/// the duration of one `session/prompt`; the notification and permission
/// callbacks read it to reach the in-flight [`AcpClient::prompt`] call.
#[derive(Clone)]
struct ActivePrompt {
    run_id: RunId,
    events: mpsc::Sender<PromptOut>,
    tools: Arc<Mutex<crate::acp_tool_history::AcpToolHistory>>,
}

/// A command from an [`AcpClient`] handle (or [`AcpCancelHandle`]) to its
/// connection driver.
enum PromptCommand {
    Prompt {
        objective: String,
        run_id: RunId,
        events: mpsc::Sender<PromptOut>,
    },
    /// `session/set_config_option` — switch a select option (the model) and
    /// report back whether the agent accepted it.
    SetConfigOption {
        config_id: Option<String>,
        value: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// `session/cancel` — ask the agent to wind down the in-flight turn. The
    /// driver services this DURING a prompt (the whole point); with no turn in
    /// flight it is still forwarded, which the agent treats as a no-op.
    Cancel,
}

/// One item streamed from the connection driver back to an in-flight `prompt`.
enum PromptOut {
    /// A Codypendent event mapped from a `session/update`.
    Event(EventBody),
    /// A permission request awaiting the sink's choice.
    Permission {
        tool_call: Value,
        options: Vec<PermissionOption>,
        reply: oneshot::Sender<Option<String>>,
    },
    /// The turn resolved with this stop reason.
    Done(AcpStopReason),
    /// The turn failed; carries a human-readable reason.
    Failed(String),
}

/// Drive one ACP connection: run the builder's `connect_with`, complete the
/// handshake (signalling `ready` and populating `discovery` from the
/// `initialize` / `session/new` responses), then service commands until every
/// [`AcpClient`] handle drops and `commands` closes.
async fn run_connection<T>(
    transport: T,
    cwd: PathBuf,
    options: AcpSessionOptions,
    discovery: Arc<Mutex<AcpDiscovery>>,
    ready: oneshot::Sender<Result<(), AcpClientError>>,
    mut commands: mpsc::Receiver<PromptCommand>,
) -> Result<(), AcpClientError>
where
    T: ConnectTo<Client> + Send + 'static,
{
    let active: Arc<Mutex<Option<ActivePrompt>>> = Arc::new(Mutex::new(None));

    let outcome = Client
        .builder()
        .name("codypendent-acp-client")
        // Every streamed `session/update` maps through the single Task 6 point.
        // Config/mode announcements ALSO refresh the shared discovery state, so
        // `discovered_models()` stays truthful after an agent-initiated switch.
        .on_receive_notification(
            {
                let active = Arc::clone(&active);
                let discovery = Arc::clone(&discovery);
                async move |notification: SessionNotification, _cx| {
                    match &notification.update {
                        SessionUpdate::ConfigOptionUpdate(update) => {
                            lock_recovering(&discovery)
                                .apply_config_options(&update.config_options);
                        }
                        SessionUpdate::CurrentModeUpdate(update) => {
                            lock_recovering(&discovery)
                                .apply_current_mode(&update.current_mode_id.to_string());
                        }
                        _ => {}
                    }
                    let current = lock_recovering(&active).clone();
                    if let Some(prompt) = current {
                        let events = lock_recovering(&prompt.tools)
                            .events(&notification.update, prompt.run_id);
                        for event in events {
                            let _ = prompt.events.send(PromptOut::Event(event)).await;
                        }
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        // A permission request is routed to the sink and answered with its choice.
        .on_receive_request(
            {
                let active = Arc::clone(&active);
                async move |request: RequestPermissionRequest, responder, _cx| {
                    let outcome = resolve_permission(&active, request).await;
                    responder.respond(RequestPermissionResponse::new(outcome))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, move |cx: ConnectionTo<Agent>| async move {
            // Handshake. The responses are KEPT: `initialize` advertises auth
            // methods (actionable errors), `session/new` advertises the
            // agent's models/modes (the rubric's automatic model discovery).
            match cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await
            {
                Ok(response) => lock_recovering(&discovery).apply_initialize(&response),
                Err(error) => {
                    let _ = ready.send(Err(AcpClientError::Handshake(format!(
                        "initialize failed: {error}"
                    ))));
                    return Err(error);
                }
            }
            if let Some(auth) = &options.cached_auth {
                let advertised = lock_recovering(&discovery)
                    .auth_methods
                    .iter()
                    .any(|method| method.id == auth.method_id);
                if !advertised || !cached_google_personal_profile(&auth.profile) {
                    let _ = ready.send(Err(AcpClientError::Handshake(
                        "Existing Google sign-in is unavailable; authentication is required."
                            .into(),
                    )));
                    return Ok(());
                }
                lock_recovering(&discovery).handshake_phase = Some("existing sign-in");
                if let Err(error) = cx
                    .send_request(AuthenticateRequest::new(auth.method_id.clone()))
                    .block_task()
                    .await
                {
                    let _ = ready.send(Err(AcpClientError::Handshake(format!(
                        "existing sign-in failed: {error}"
                    ))));
                    return Err(error);
                }
            }
            lock_recovering(&discovery).handshake_phase = Some("session creation");
            let mcp_servers = options
                .mcp_servers
                .iter()
                .map(|server| {
                    WireMcpServer::Stdio(
                        McpServerStdio::new(server.name.clone(), server.command.clone())
                            .args(server.args.clone()),
                    )
                })
                .collect::<Vec<_>>();
            let request = agent_client_protocol::UntypedMessage::new(
                "session/new",
                NewSessionRequest::new(cwd).mcp_servers(mcp_servers),
            )?;
            let session_id = match cx.send_request(request).block_task().await {
                Ok(response) => {
                    let parsed: NewSessionResponse = match serde_json::from_value(response.clone())
                    {
                        Ok(parsed) => parsed,
                        Err(error) => {
                            let _ = ready.send(Err(AcpClientError::Handshake(format!(
                                "invalid session/new response: {error}"
                            ))));
                            return Err(error.into());
                        }
                    };
                    let mut slot = lock_recovering(&discovery);
                    slot.apply_session(&parsed);
                    slot.apply_legacy_models(&response);
                    parsed.session_id
                }
                Err(error) => {
                    // An auth-gated agent fails exactly here; name its
                    // advertised remedies instead of an opaque failure.
                    let hint = if authentication_required(&error.to_string()) {
                        let slot = lock_recovering(&discovery);
                        auth_methods_hint(&slot.auth_methods)
                    } else {
                        String::new()
                    };
                    let _ = ready.send(Err(AcpClientError::Handshake(format!(
                        "session/new failed: {error}{hint}"
                    ))));
                    return Err(error);
                }
            };
            lock_recovering(&discovery).handshake_phase = None;
            let _ = ready.send(Ok(()));

            // One delegated prompt turn per command. The `active` slot lets the
            // concurrently-dispatched update/permission callbacks reach this
            // turn's event channel and run id while the request is in flight.
            while let Some(command) = commands.recv().await {
                match command {
                    PromptCommand::Prompt {
                        objective,
                        run_id,
                        events,
                    } => {
                        *lock_recovering(&active) = Some(ActivePrompt {
                            run_id,
                            events: events.clone(),
                            tools: Arc::new(Mutex::new(Default::default())),
                        });
                        let request = cx
                            .send_request(PromptRequest::new(
                                session_id.clone(),
                                vec![ContentBlock::Text(TextContent::new(objective))],
                            ))
                            .block_task();
                        let mut request = std::pin::pin!(request);
                        // Keep servicing commands while the turn is in flight —
                        // a `Cancel` must reach the agent as `session/cancel`
                        // DURING the prompt or graceful cancellation could
                        // never work.
                        let mut commands_open = true;
                        let result = loop {
                            if !commands_open {
                                break request.as_mut().await;
                            }
                            tokio::select! {
                                result = request.as_mut() => break result,
                                command = commands.recv() => match command {
                                    Some(PromptCommand::Cancel) => {
                                        let _ = cx.send_notification(CancelNotification::new(
                                            session_id.clone(),
                                        ));
                                    }
                                    Some(PromptCommand::SetConfigOption { reply, .. }) => {
                                        let _ = reply.send(Err(
                                            "a prompt turn is in flight".to_string(),
                                        ));
                                    }
                                    Some(PromptCommand::Prompt { events, .. }) => {
                                        // `prompt` takes `&mut self`, so a second
                                        // in-flight turn cannot happen through the
                                        // public API; refuse defensively anyway.
                                        let _ = events
                                            .send(PromptOut::Failed(
                                                "a prompt turn is already in flight".to_string(),
                                            ))
                                            .await;
                                    }
                                    None => commands_open = false,
                                },
                            }
                        };
                        *lock_recovering(&active) = None;
                        let resolved = match result {
                            Ok(response) => PromptOut::Done(map_stop_reason(response.stop_reason)),
                            Err(error) => {
                                PromptOut::Failed(format!("session/prompt failed: {error}"))
                            }
                        };
                        let _ = events.send(resolved).await;
                    }
                    PromptCommand::SetConfigOption {
                        config_id,
                        value,
                        reply,
                    } => {
                        let Some(config_id) = config_id else {
                            let request = agent_client_protocol::UntypedMessage::new(
                                "session/set_model",
                                serde_json::json!({"sessionId":session_id,"modelId":value}),
                            )?;
                            let outcome = match cx.send_request(request).block_task().await {
                                Ok(_) => {
                                    for model in &mut lock_recovering(&discovery).models {
                                        model.current = model.id == value;
                                    }
                                    Ok(())
                                }
                                Err(error) => Err(format!("session/set_model failed: {error}")),
                            };
                            let _ = reply.send(outcome);
                            continue;
                        };
                        let result = cx
                            .send_request(SetSessionConfigOptionRequest::new(
                                session_id.clone(),
                                SessionConfigId::new(config_id),
                                SessionConfigOptionValue::value_id(value),
                            ))
                            .block_task()
                            .await;
                        let outcome = match result {
                            Ok(response) => {
                                // The response carries the full refreshed set;
                                // fold it so `discovered_models()` reports the
                                // new current selection.
                                lock_recovering(&discovery)
                                    .apply_config_options(&response.config_options);
                                Ok(())
                            }
                            Err(error) => Err(format!("session/set_config_option failed: {error}")),
                        };
                        let _ = reply.send(outcome);
                    }
                    PromptCommand::Cancel => {
                        let _ = cx.send_notification(CancelNotification::new(session_id.clone()));
                    }
                }
            }
            Ok(())
        })
        .await;

    outcome.map_err(|error| AcpClientError::Prompt(format!("acp connection ended: {error}")))
}

/// Resolve one `session/request_permission` by asking the in-flight prompt's
/// sink (via the `active` slot) and translating its answer to the ACP outcome.
/// With no active prompt, or if the prompt is gone, the request is cancelled.
async fn resolve_permission(
    active: &Arc<Mutex<Option<ActivePrompt>>>,
    request: RequestPermissionRequest,
) -> RequestPermissionOutcome {
    let current = lock_recovering(active).clone();
    let Some(prompt) = current else {
        return RequestPermissionOutcome::Cancelled;
    };
    // The agent's `ToolCallUpdate` is passed to the sink as opaque JSON — the
    // approval flow decides on it; we never re-model it.
    let tool_call = serde_json::to_value(&request.tool_call).unwrap_or(Value::Null);
    let options = request
        .options
        .iter()
        .map(to_permission_option)
        .collect::<Vec<_>>();
    let (reply, answer) = oneshot::channel();
    if prompt
        .events
        .send(PromptOut::Permission {
            tool_call,
            options,
            reply,
        })
        .await
        .is_err()
    {
        return RequestPermissionOutcome::Cancelled;
    }
    match answer.await {
        Ok(Some(option_id)) => {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id))
        }
        _ => RequestPermissionOutcome::Cancelled,
    }
}

/// Project the ACP wire [`WirePermissionOption`] onto the server role's
/// [`PermissionOption`] so the whole codebase answers permissions in one shape.
fn to_permission_option(option: &WirePermissionOption) -> PermissionOption {
    PermissionOption {
        option_id: option.option_id.to_string(),
        name: option.name.clone(),
        kind: permission_kind_wire(option.kind).to_string(),
    }
}

/// The ACP wire string for a permission-option kind (`#[non_exhaustive]`, so an
/// unknown future kind degrades to `"unknown"` rather than a hard error).
fn permission_kind_wire(kind: PermissionOptionKind) -> &'static str {
    match kind {
        PermissionOptionKind::AllowOnce => "allow_once",
        PermissionOptionKind::AllowAlways => "allow_always",
        PermissionOptionKind::RejectOnce => "reject_once",
        PermissionOptionKind::RejectAlways => "reject_always",
        _ => "unknown",
    }
}

/// Map the ACP `stopReason` onto the client's [`AcpStopReason`]. `max_tokens` and
/// `max_turn_requests` — and any future variant (`StopReason` is
/// `#[non_exhaustive]`) — collapse into `EndTurn`: the turn ended.
fn map_stop_reason(reason: StopReason) -> AcpStopReason {
    match reason {
        StopReason::EndTurn | StopReason::MaxTokens | StopReason::MaxTurnRequests => {
            AcpStopReason::EndTurn
        }
        StopReason::Cancelled => AcpStopReason::Cancelled,
        StopReason::Refusal => AcpStopReason::Refusal,
        _ => AcpStopReason::EndTurn,
    }
}

/// Build the crate's line transport from a tokio reader/writer pair without any
/// `tokio-util` compat shim (the workspace has only `futures` + `tokio`, and ACP
/// is the sole new dependency): newline-framed JSON in, newline-framed JSON out.
fn tokio_transport<R, W>(
    reader: R,
    writer: W,
) -> Lines<
    impl futures::Sink<String, Error = std::io::Error> + Send + 'static,
    impl futures::Stream<Item = std::io::Result<String>> + Send + 'static,
>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let incoming = futures::stream::unfold(Some(BufReader::new(reader)), |state| async move {
        let mut reader = state?;
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => None,                           // clean EOF ends the stream
            Ok(_) => Some((Ok(line), Some(reader))), // one framed message
            Err(error) => Some((Err(error), None)),  // surface once, then stop
        }
    });
    let outgoing = futures::sink::unfold(writer, |mut writer, line: String| async move {
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok::<_, std::io::Error>(writer)
    });
    Lines::new(outgoing, incoming)
}

#[cfg(test)]
mod mapping_tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        Content, ImageContent, Plan, SessionConfigSelectOption, TextContent, ToolCallContent,
        ToolCallUpdateFields,
    };

    fn rid() -> RunId {
        RunId::new()
    }

    #[test]
    fn copilot_receives_stdio_servers_through_its_session_only_cli_configuration() {
        let server = AcpMcpServer {
            name: "codypendent-desktop".to_string(),
            command: "/test/codypendent".to_string(),
            args: vec!["__desktop_tools_mcp".to_string()],
        };
        let (args, options) = agent_mcp_launch(
            "/test/copilot",
            &["--acp".to_string()],
            AcpSessionOptions {
                mcp_servers: vec![server.clone()],
                ..AcpSessionOptions::default()
            },
        );
        assert!(options.mcp_servers.is_empty());
        assert_eq!(args[0], "--acp");
        assert_eq!(args[1], "--additional-mcp-config");
        let config: Value = serde_json::from_str(&args[2]).unwrap();
        assert_eq!(config["mcpServers"]["codypendent-desktop"]["type"], "local");
        assert_eq!(
            config["mcpServers"]["codypendent-desktop"]["command"],
            "/test/codypendent"
        );
        assert!(config["mcpServers"]["codypendent-desktop"]
            .get("env")
            .is_none());
        let (args, options) = agent_mcp_launch(
            "/test/codex-acp",
            &[],
            AcpSessionOptions {
                mcp_servers: vec![server.clone()],
                ..AcpSessionOptions::default()
            },
        );
        assert!(args.is_empty());
        assert_eq!(options.mcp_servers, vec![server]);
    }

    #[test]
    fn streamed_provider_error_is_not_a_successful_assistant_answer() {
        let error = r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"This model is not supported by this account."}}"#;
        assert_eq!(
            streamed_provider_failure(&format!(
                "Warning: Model metadata unavailable.\n\n{error}\n"
            )),
            Some("This model is not supported by this account.".to_string())
        );
        for text in [
            format!("Here is an example error:\n{error}"),
            format!("```json\n{error}\n```"),
            r#"{"status":400,"error":{"message":"Example data"}}"#.to_string(),
            r#"{"type":"error","status":200,"error":{"message":"Example data"}}"#.to_string(),
            "The operation succeeded.".to_string(),
        ] {
            assert_eq!(streamed_provider_failure(&text), None, "{text}");
        }
    }

    /// A `ContentChunk` wrapping a single text block, the common case for
    /// both `AgentMessageChunk` and `AgentThoughtChunk`.
    fn text_chunk(text: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
    }

    fn permission_request() -> RequestPermissionRequest {
        serde_json::from_value(serde_json::json!({
            "sessionId": "session-1",
            "toolCall": { "toolCallId": "call-1" },
            "options": [],
        }))
        .expect("a minimal v1 request deserializes")
    }

    /// Poison the active-prompt slot the only way it can be poisoned — a panic
    /// while holding it — and prove BOTH directions still hold: no in-flight
    /// turn still cancels (never a silent grant), and a live turn still reaches
    /// its sink. With `.expect(...)` back in place, both calls panic instead,
    /// and the panic propagates into the connection's request callback.
    #[tokio::test]
    async fn a_poisoned_active_prompt_slot_still_routes_permission_requests() {
        let active: Arc<Mutex<Option<ActivePrompt>>> = Arc::new(Mutex::new(None));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = active.lock().expect("fresh mutex");
            panic!("a holder panicked");
        }));
        assert!(active.is_poisoned());

        // No turn in flight: the request is cancelled, not granted.
        assert!(matches!(
            resolve_permission(&active, permission_request()).await,
            RequestPermissionOutcome::Cancelled
        ));

        // A turn in flight: the request still reaches the sink, which answers.
        let (events, mut inbox) = mpsc::channel(4);
        *active.lock().unwrap_or_else(|e| e.into_inner()) = Some(ActivePrompt {
            run_id: rid(),
            events,
            tools: Arc::new(Mutex::new(Default::default())),
        });
        let resolved = tokio::spawn({
            let active = Arc::clone(&active);
            async move { resolve_permission(&active, permission_request()).await }
        });
        match inbox.recv().await.expect("the sink was asked") {
            PromptOut::Permission { reply, .. } => {
                let _ = reply.send(None);
            }
            _ => panic!("expected a permission request on the sink channel"),
        }
        assert!(matches!(
            resolved.await.unwrap(),
            RequestPermissionOutcome::Cancelled
        ));
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
                text: "hello".to_string(),
                thought: false
            }]
        );
    }

    #[test]
    /// Reasoning streams as text, but MARKED — the distinction ACP draws must
    /// survive the bridge. Merging the two arms is what buried the answer under
    /// the model's own narration, so this asserts the flag and not merely that
    /// some delta came out.
    fn agent_thought_chunk_streams_as_marked_reasoning() {
        let run_id = rid();
        let update = SessionUpdate::AgentThoughtChunk(text_chunk("thinking"));
        let events = session_update_to_events(&update, run_id);
        assert_eq!(
            events,
            vec![EventBody::ModelStreamDelta {
                run_id,
                text: "thinking".to_string(),
                thought: true
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
                tool: "acp:t1".to_string(),
                args_digest: String::new(),
                label: Some("read_file".to_string()),
            }]
        );
    }

    #[test]
    fn acp_tool_calls_with_the_same_title_keep_distinct_identities() {
        let run_id = rid();
        for id in ["second", "first"] {
            let start = session_update_to_events(
                &SessionUpdate::ToolCall(ToolCall::new(id, "Terminal")),
                run_id,
            );
            let done = session_update_to_events(
                &SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    id,
                    ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
                )),
                run_id,
            );
            assert!(matches!((&start[0], &done[0]),
                (EventBody::ToolStarted { tool: started, label, .. },
                 EventBody::ToolCompleted { tool: completed, .. })
                if started == completed && started == &format!("acp:{id}")
                    && label.as_deref() == Some("Terminal")));
        }
    }

    #[test]
    fn acp_tool_refinement_keeps_command_and_file_targets_bounded_and_single_line() {
        let command = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "shell",
            ToolCallUpdateFields::new()
                .title("Terminal")
                .kind(ToolKind::Execute)
                .raw_input(serde_json::json!({"command": "git\nstatus\u{202e}"})),
        ));
        assert!(
            matches!(session_update_to_events(&command, rid()).as_slice(),
            [EventBody::ToolUpdated { tool, label, .. }]
            if tool == "acp:shell" && label.as_deref() == Some("Terminal · git status"))
        );
        let file = ToolCall::new("read", "Read File")
            .kind(ToolKind::Read)
            .raw_input(
                serde_json::json!({"file_path": "src/main.rs", "content": "do not display this"}),
            );
        assert_eq!(
            acp_tool_display_label(
                &file.title,
                Some(file.kind),
                file.raw_input.as_ref(),
                &file.locations
            ),
            "Read File · src/main.rs"
        );
        let huge = ToolCall::new("huge", "Terminal")
            .raw_input(serde_json::json!({"command": "x".repeat(10000)}));
        let label = acp_tool_display_label(
            &huge.title,
            Some(huge.kind),
            huge.raw_input.as_ref(),
            &huge.locations,
        );
        assert_eq!(label.chars().count(), 200);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn acp_already_completed_call_reports_its_terminal_outcome() {
        let events = session_update_to_events(
            &SessionUpdate::ToolCall(
                ToolCall::new("done", "Read File").status(ToolCallStatus::Completed),
            ),
            rid(),
        );
        assert!(matches!(
            events.as_slice(),
            [
                EventBody::ToolStarted { .. },
                EventBody::ToolCompleted {
                    outcome: ToolOutcome::Succeeded,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn acp_terminal_updates_keep_action_details_and_ignore_generic_title_only_refinements() {
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "shell",
            ToolCallUpdateFields::new()
                .title("Terminal")
                .kind(ToolKind::Execute)
                .status(ToolCallStatus::Completed)
                .raw_input(serde_json::json!({"command": "git status --short"})),
        ));
        assert!(
            matches!(session_update_to_events(&update, rid()).as_slice(),
            [EventBody::ToolUpdated { label, .. }, EventBody::ToolCompleted { outcome: ToolOutcome::Succeeded, .. }]
            if label.as_deref() == Some("Terminal · git status --short"))
        );
        let generic = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "shell",
            ToolCallUpdateFields::new()
                .title("Terminal")
                .status(ToolCallStatus::InProgress),
        ));
        assert!(session_update_to_events(&generic, rid()).is_empty());
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
                tool: "acp:t1".to_string(),
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
                tool: "acp:shell-1".to_string(),
                outcome: ToolOutcome::Failed {
                    message: "permission denied".to_string()
                },
                artifact: None,
            }]
        );
    }

    #[test]
    fn failed_tool_call_initial_snapshot_preserves_the_reported_exit() {
        let events = session_update_to_events(
            &SessionUpdate::ToolCall(
                ToolCall::new("search-137", "Search")
                    .status(ToolCallStatus::Failed)
                    .raw_output(serde_json::json!({"formatted_output":"", "exit_code":137})),
            ),
            rid(),
        );
        assert!(matches!(events.last(), Some(EventBody::ToolCompleted {
            outcome: ToolOutcome::Failed { message }, ..
        }) if message == "Process exited with code 137 (SIGKILL)."));
    }

    #[test]
    fn tool_call_update_without_a_title_preserves_the_tool_call_identity() {
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
                tool: "acp:t-42".to_string(),
                outcome: ToolOutcome::Succeeded,
                artifact: None,
            }]
        );
    }

    #[test]
    fn tool_call_updates_preserve_input_refinements_without_completion() {
        let run_id = rid();
        let in_progress = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "t1",
            ToolCallUpdateFields::new()
                .title("x")
                .status(ToolCallStatus::InProgress),
        ));
        assert!(
            matches!(session_update_to_events(&in_progress, run_id).as_slice(),
                [EventBody::ToolUpdated { tool, label, .. }] if tool == "acp:t1" && label.as_deref() == Some("x")
            )
        );

        let no_status_change = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "t1",
            ToolCallUpdateFields::new().title("renamed"),
        ));
        assert!(
            matches!(session_update_to_events(&no_status_change, run_id).as_slice(),
                [EventBody::ToolUpdated { tool, label, .. }] if tool == "acp:t1" && label.as_deref() == Some("renamed")
            )
        );
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

    /// A Model-category `select` option, the shape current ACP carries a model
    /// list in.
    fn model_selector(id: &str, current: &str, options: &[(&str, &str)]) -> SessionConfigOption {
        SessionConfigOption::new(
            id.to_string(),
            "Model",
            SessionConfigKind::Select(SessionConfigSelect::new(
                current.to_string(),
                options
                    .iter()
                    .map(|(value, name)| {
                        SessionConfigSelectOption::new((*value).to_string(), *name)
                    })
                    .collect::<Vec<_>>(),
            )),
        )
        .category(SessionConfigOptionCategory::Model)
    }

    #[test]
    fn current_mode_update_maps_to_a_run_scoped_note() {
        let run_id = rid();
        let update = SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new("review"));
        let events = session_update_to_events(&update, run_id);
        assert!(
            matches!(
                events.as_slice(),
                [EventBody::NoteAppended { text, run_id: Some(run) }]
                    if *run == run_id && text.contains("review")
            ),
            "expected one run-scoped mode note, got {events:?}"
        );
    }

    #[test]
    fn config_option_update_notes_only_the_model_and_mode_selectors() {
        let run_id = rid();
        let update = SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![
            model_selector(
                "model",
                "agent-model-2",
                &[("agent-model-1", "First"), ("agent-model-2", "Second")],
            ),
            // A non-model/mode selector stays silent.
            SessionConfigOption::new(
                "thought",
                "Thought level",
                SessionConfigKind::Select(SessionConfigSelect::new(
                    "high",
                    vec![SessionConfigSelectOption::new("high", "High")],
                )),
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
            // A boolean toggle is not a selector at all.
            SessionConfigOption::new(
                "verbose",
                "Verbose",
                SessionConfigKind::Boolean(
                    agent_client_protocol::schema::v1::SessionConfigBoolean::new(true),
                ),
            )
            .category(SessionConfigOptionCategory::Model),
        ]));
        let events = session_update_to_events(&update, run_id);
        assert!(
            matches!(
                events.as_slice(),
                [EventBody::NoteAppended { text, run_id: Some(run) }]
                    if *run == run_id
                        && text.contains("model")
                        && text.contains("Second")
                        && text.contains("agent-model-2")
            ),
            "expected exactly one model note naming the new selection, got {events:?}"
        );
    }

    #[test]
    fn grouped_select_options_flatten_for_notes() {
        use agent_client_protocol::schema::v1::SessionConfigSelectGroup;

        let run_id = rid();
        let grouped = SessionConfigOption::new(
            "model",
            "Model",
            SessionConfigKind::Select(SessionConfigSelect::new(
                "agent-model-2",
                vec![SessionConfigSelectGroup::new(
                    "group-1",
                    "Group one",
                    vec![
                        SessionConfigSelectOption::new("agent-model-1", "First"),
                        SessionConfigSelectOption::new("agent-model-2", "Second"),
                    ],
                )],
            )),
        )
        .category(SessionConfigOptionCategory::Model);
        let update = SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![grouped]));
        let events = session_update_to_events(&update, run_id);
        assert!(
            matches!(
                events.as_slice(),
                [EventBody::NoteAppended { text, .. }] if text.contains("Second")
            ),
            "expected the grouped option's display name in the note, got {events:?}"
        );
    }

    #[test]
    fn a_config_option_set_without_a_model_selector_clears_the_discovered_models() {
        // Every arrival is the COMPLETE set, so a dropped selector must not
        // leave a stale model list behind for a run to pin against.
        let mut discovery = AcpDiscovery::default();
        discovery.apply_config_options(&[model_selector(
            "model",
            "agent-model-1",
            &[("agent-model-1", "First")],
        )]);
        assert_eq!(discovery.models.len(), 1);
        assert_eq!(discovery.model_config_id.as_deref(), Some("model"));
        discovery.apply_config_options(&[]);
        assert!(discovery.models.is_empty());
        assert!(discovery.model_config_id.is_none());
    }

    #[test]
    fn a_dedicated_modes_state_outranks_a_mode_category_config_option() {
        use agent_client_protocol::schema::v1::{SessionMode, SessionModeState};

        let mut discovery = AcpDiscovery::default();
        let response = NewSessionResponse::new("s-1")
            .modes(SessionModeState::new(
                "build",
                vec![
                    SessionMode::new("build", "Build"),
                    SessionMode::new("review", "Review"),
                ],
            ))
            .config_options(vec![SessionConfigOption::new(
                "mode",
                "Mode",
                SessionConfigKind::Select(SessionConfigSelect::new(
                    "other",
                    vec![SessionConfigSelectOption::new("other", "Other")],
                )),
            )
            .category(SessionConfigOptionCategory::Mode)]);
        discovery.apply_session(&response);
        assert_eq!(
            discovery
                .modes
                .iter()
                .map(|mode| mode.id.as_str())
                .collect::<Vec<_>>(),
            vec!["build", "review"],
            "the richer `modes` state must win over a Mode-category option"
        );
        assert!(discovery.modes.iter().any(|mode| mode.current));
    }
}

#[cfg(test)]
mod client_tests {
    //! End-to-end tests over a scripted in-process ACP *agent* peer that speaks
    //! the real newline-delimited JSON-RPC 2.0 wire over `tokio::io::duplex`
    //! (mirroring the harness in `crate::acp`'s tests). They assert the handshake
    //! completes, a prompt is delegated, streamed `session/update`s reach
    //! `session_update_to_events`, a `session/request_permission` maps onto the
    //! sink's approval, and — for model discovery — that the handshake's
    //! `config_options` / `authMethods` are captured, `session/set_config_option`
    //! is spoken on the wire, and `session/cancel` reaches the agent mid-turn.

    use super::*;
    use serde_json::json;
    use tokio::io::AsyncBufRead;

    fn cached_google_fixture(
        home: &std::path::Path,
    ) -> (PathBuf, crate::acp_registry::AcpLaunchSpec) {
        let profile = home.join(".gemini/antigravity-cli/antigravity-oauth-token");
        std::fs::create_dir_all(profile.parent().unwrap()).unwrap();
        std::fs::write(&profile, json!({
            "auth_method":"consumer",
            "token":{"access_token":"sentinel-access-secret", "refresh_token":"sentinel-refresh-secret", "expiry":"2000-01-01T00:00:00Z"}
        }).to_string()).unwrap();
        let spec = crate::acp_registry::AcpLaunchSpec {
            registry_id: "antigravity-acp".into(),
            name: "Google Antigravity".into(),
            version: "test".into(),
            command: "/test/agy_acp_server.par".into(),
            args: Vec::new(),
            env: [("HOME".into(), home.to_string_lossy().into_owned())].into(),
        };
        (profile, spec)
    }

    #[test]
    fn cached_google_auth_requires_an_existing_consumer_profile_and_official_family() {
        let home = tempfile::tempdir().unwrap();
        let (profile, mut spec) = cached_google_fixture(home.path());
        let options = AcpSessionOptions::for_agent(&spec);
        assert_eq!(
            options.cached_auth.as_ref().unwrap().method_id,
            "oauth-personal"
        );
        assert!(!format!("{options:?}").contains("sentinel"));
        spec.registry_id = "antigravity-community".into();
        assert!(AcpSessionOptions::for_agent(&spec).cached_auth.is_none());
        spec.registry_id = "antigravity-acp".into();
        for content in [
            "",
            "{}",
            r#"{"auth_method":"consumer","token":{"access_token":"expired","expiry":"2000-01-01T00:00:00Z"}}"#,
            r#"{"auth_method":"business","token":{"refresh_token":"present","expiry":"2030-01-01T00:00:00Z"}}"#,
            r#"{"auth_method":"consumer","token":{"refresh_token":"present","expiry":"invalid"}}"#,
        ] {
            std::fs::write(&profile, content).unwrap();
            assert!(AcpSessionOptions::for_agent(&spec).cached_auth.is_none());
        }
        std::fs::write(&profile, vec![b' '; 65_537]).unwrap();
        assert!(AcpSessionOptions::for_agent(&spec).cached_auth.is_none());
        std::fs::remove_file(profile).unwrap();
        assert!(AcpSessionOptions::for_agent(&spec).cached_auth.is_none());
    }

    #[test]
    fn google_recovery_requires_the_same_cached_account_and_an_advertised_gemini() {
        use base64::Engine;
        let home = tempfile::tempdir().unwrap();
        let (profile, spec) = cached_google_fixture(home.path());
        let jwt = |subject: &str| {
            format!(
                "header.{}.signature",
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(json!({"iss":"https://accounts.google.com","sub":subject}).to_string())
            )
        };
        let mut current = bounded_auth_profile(&profile).unwrap();
        current["id_token"] = json!(jwt("same-account"));
        std::fs::write(&profile, current.to_string()).unwrap();
        let legacy = home.path().join(".gemini/oauth_creds.json");
        std::fs::write(&legacy, json!({"id_token":jwt("same-account")}).to_string()).unwrap();
        let auth = AcpSessionOptions::for_agent(&spec).cached_auth.unwrap();
        assert!(same_google_account(&auth, &spec));
        std::fs::write(
            &legacy,
            json!({"id_token":jwt("other-account")}).to_string(),
        )
        .unwrap();
        assert!(!same_google_account(&auth, &spec));
        std::fs::write(&legacy, r#"{"id_token":"invalid"}"#).unwrap();
        assert!(!same_google_account(&auth, &spec));
        let mut models = vec![
            AcpModel {
                id: "claude-default".into(),
                name: "Claude".into(),
                current: true,
            },
            AcpModel {
                id: "gemini-listed".into(),
                name: "Gemini".into(),
                current: false,
            },
        ];
        assert_eq!(
            recovery_gemini_model(&models, None).as_deref(),
            Some("gemini-listed")
        );
        assert_eq!(recovery_gemini_model(&models, Some("invented")), None);
        assert_eq!(recovery_gemini_model(&models, Some("claude-default")), None);
        models.push(AcpModel {
            id: "gemini-current".into(),
            name: "Current Gemini".into(),
            current: true,
        });
        assert_eq!(
            recovery_gemini_model(&models, None).as_deref(),
            Some("gemini-current")
        );
        assert_eq!(
            recovery_gemini_model(&models, Some("gemini-listed")).as_deref(),
            Some("gemini-listed")
        );
        assert!(retired_google_individual_client(&AcpClientError::Handshake(
            "This client is no longer supported for Gemini Code Assist for individuals. Migrate to https://antigravity.google".into()
        )));
        assert!(!retired_google_individual_client(
            &AcpClientError::Handshake("Authentication required".into())
        ));
    }

    #[tokio::test]
    async fn cached_google_auth_precedes_new_session_and_never_forwards_tokens() {
        let home = tempfile::tempdir().unwrap();
        let (_, spec) = cached_google_fixture(home.path());
        let (_client, sent) = connect_with_options(
            Script {
                auth_methods: json!([{"id":"oauth-personal", "name":"Google account"}]),
                ..Script::default()
            },
            AcpSessionOptions::for_agent(&spec),
        )
        .await
        .unwrap();
        let sent = lock_recovering(&sent);
        let methods: Vec<_> = sent
            .iter()
            .filter_map(|value| value.get("method").and_then(Value::as_str))
            .collect();
        assert_eq!(
            &methods[..3],
            &["initialize", "authenticate", "session/new"]
        );
        let auth = sent
            .iter()
            .find(|value| value["method"] == "authenticate")
            .unwrap();
        assert_eq!(auth["params"], json!({"methodId":"oauth-personal"}));
        assert!(!serde_json::to_string(&*sent).unwrap().contains("sentinel"));
    }

    #[tokio::test]
    async fn cached_google_auth_rechecks_removed_credentials_and_advertised_methods() {
        let home = tempfile::tempdir().unwrap();
        let (profile, spec) = cached_google_fixture(home.path());
        let options = AcpSessionOptions::for_agent(&spec);
        std::fs::remove_file(profile).unwrap();
        let error = connect_with_options(
            Script {
                auth_methods: json!([{"id":"oauth-personal", "name":"Google account"}]),
                ..Script::default()
            },
            options,
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("authentication is required"));
        let (_, spec) = cached_google_fixture(home.path());
        let error = connect_with_options(Script::default(), AcpSessionOptions::for_agent(&spec))
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("authentication is required"));
    }

    #[tokio::test]
    async fn a_retired_google_client_preserves_the_vendor_remedy_without_sign_in_advice() {
        let error = connect_with_options(
            Script {
                auth_methods: json!([{"id":"oauth-personal", "name":"Google login", "description":"Sign in again"}]),
                session_new_error: Some(json!({"code":-32000,"message":"This client is no longer supported. Please migrate to Antigravity: https://antigravity.google"})),
                ..Script::default()
            },
            AcpSessionOptions::default(),
        ).await.err().unwrap();
        let message = error.to_string();
        assert!(message.contains("https://antigravity.google"));
        assert!(!message.contains("Sign in again"));
        assert!(!message.contains("advertises authentication"));
    }

    #[tokio::test]
    async fn legacy_model_state_is_preserved_and_only_advertised_ids_can_be_selected() {
        let extra = json!({"models":{
            "currentModelId":"gemini-current",
            "availableModels":[
                {"modelId":"gemini-current", "name":"Current Gemini"},
                {"modelId":"gemini-other", "name":"Other Gemini"}
            ]
        }})
        .as_object()
        .unwrap()
        .clone();
        let (mut client, sent) = connect_with_options(
            Script {
                session_new_extra: extra,
                ..Script::default()
            },
            AcpSessionOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(client.discovered_models().len(), 2);
        assert!(client.discovered_models()[0].current);
        client.set_model("gemini-other").await.unwrap();
        assert!(client.discovered_models()[1].current);
        assert!(!client.discovered_models()[0].current);
        assert!(client.set_model("invented-model").await.is_err());
        let request = sent_method(&sent, "session/set_model").unwrap();
        assert_eq!(
            request["params"],
            json!({"sessionId":"s-1","modelId":"gemini-other"})
        );
        assert!(sent_method(&sent, "session/set_config_option").is_none());
    }

    #[tokio::test]
    async fn current_model_config_takes_precedence_over_legacy_state() {
        let extra = json!({
            "models":{"currentModelId":"legacy", "availableModels":[{"modelId":"legacy","name":"Legacy"}]},
            "configOptions":[wire_model_selector("modern", &[("modern", "Modern")])]
        }).as_object().unwrap().clone();
        let (client, _) = connect_with_options(
            Script {
                session_new_extra: extra,
                ..Script::default()
            },
            AcpSessionOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            client.discovered_models(),
            vec![AcpModel {
                id: "modern".into(),
                name: "Modern".into(),
                current: true
            }]
        );
    }

    /// Records mapped events and auto-approves any permission request by choosing
    /// the first offered option.
    struct RecordingSink {
        events: Arc<Mutex<Vec<EventBody>>>,
    }

    #[async_trait]
    impl AcpEventSink for RecordingSink {
        async fn on_event(&mut self, event: EventBody) {
            self.events.lock().unwrap().push(event);
        }
        async fn on_permission(
            &mut self,
            _tool_call: Value,
            options: Vec<PermissionOption>,
        ) -> Option<String> {
            options.first().map(|option| option.option_id.clone())
        }
    }

    /// How the scripted agent answers the handshake and one `session/prompt`.
    /// Defaults to the pre-discovery agent: no auth methods, no config options,
    /// no permission ask, stream a text chunk + a tool call, end the turn.
    #[derive(Clone, Default)]
    struct Script {
        /// `initialize` advertises these `authMethods`.
        auth_methods: Value,
        /// `session/new` answers with this JSON-RPC `error` instead of a result.
        session_new_error: Option<Value>,
        /// Extra keys merged into the `session/new` result (`configOptions`,
        /// `modes`).
        session_new_extra: serde_json::Map<String, Value>,
        /// Answer `session/set_config_option` with this full refreshed set.
        set_config_options: Option<Value>,
        /// Ask for tool permission before streaming (and record the answer).
        permission: bool,
        /// `session/update` payloads streamed before the turn resolves.
        updates: Vec<Value>,
        final_text: Option<String>,
        /// Park the turn until the client sends `session/cancel`, then resolve
        /// with `stopReason: cancelled` instead of `end_turn`.
        await_cancel: bool,
    }

    impl Script {
        fn session_new_result(&self) -> Value {
            let mut result = self.session_new_extra.clone();
            result.insert("sessionId".to_string(), json!("s-1"));
            Value::Object(result)
        }

        fn stop_reason(&self) -> &'static str {
            if self.await_cancel {
                "cancelled"
            } else {
                "end_turn"
            }
        }
    }

    /// A Model-category `select` config option in wire shape.
    fn wire_model_selector(current: &str, options: &[(&str, &str)]) -> Value {
        json!({
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": current,
            "options": options
                .iter()
                .map(|(value, name)| json!({ "value": value, "name": name }))
                .collect::<Vec<_>>(),
        })
    }

    async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &Value) {
        let mut line = serde_json::to_string(message).expect("serialize");
        line.push('\n');
        writer.write_all(line.as_bytes()).await.expect("write");
        writer.flush().await.expect("flush");
    }

    async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R, sent: &Sent) -> Option<Value> {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await.ok()?;
        if read == 0 {
            return None;
        }
        let message: Value = serde_json::from_str(line.trim()).ok()?;
        sent.lock().unwrap().push(message.clone());
        Some(message)
    }

    /// Everything the client sent the agent, in order — requests, responses and
    /// notifications alike. Tests assert against the real wire rather than an
    /// internal call count.
    type Sent = Arc<Mutex<Vec<Value>>>;

    /// The first recorded message whose `method` matches, if any.
    fn sent_method(sent: &Sent, method: &str) -> Option<Value> {
        sent.lock()
            .unwrap()
            .iter()
            .find(|message| message.get("method").and_then(Value::as_str) == Some(method))
            .cloned()
    }

    /// A scripted ACP *agent* peer. Answers `initialize` and `session/new` per
    /// [`Script`], then on `session/prompt` (optionally after a
    /// `session/request_permission` round trip) streams the scripted updates
    /// plus one text chunk + one tool call and resolves the turn. Reads/writes
    /// newline-delimited JSON-RPC on the duplex halves, recording every client
    /// message into `sent`.
    async fn scripted_agent<R, W>(reader: R, mut writer: W, script: Script, sent: Sent)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut reader = BufReader::new(reader);
        while let Some(message) = read_message(&mut reader, &sent).await {
            let method = message
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let id = message.get("id").cloned();
            match method {
                "initialize" => {
                    write_message(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {
                                "protocolVersion": 1,
                                "agentCapabilities": {},
                                "authMethods": script.auth_methods,
                            }
                        }),
                    )
                    .await;
                }
                "session/new" => {
                    let cwd = message
                        .pointer("/params/cwd")
                        .and_then(Value::as_str)
                        .expect("session/new cwd");
                    assert!(PathBuf::from(cwd).is_absolute(), "ACP cwd must be absolute");
                    let reply = match &script.session_new_error {
                        Some(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
                        None => {
                            json!({ "jsonrpc": "2.0", "id": id, "result": script.session_new_result() })
                        }
                    };
                    write_message(&mut writer, &reply).await;
                }
                "authenticate" => {
                    write_message(&mut writer, &json!({"jsonrpc":"2.0","id":id,"result":{}})).await;
                }
                "session/set_config_option" => {
                    let reply = match &script.set_config_options {
                        Some(options) => json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "configOptions": options }
                        }),
                        None => json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": { "code": -32602, "message": "unknown config option" }
                        }),
                    };
                    write_message(&mut writer, &reply).await;
                }
                "session/set_model" => {
                    write_message(&mut writer, &json!({"jsonrpc":"2.0","id":id,"result":{}})).await;
                }
                "session/prompt" => {
                    if script.permission {
                        write_message(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0", "id": 9001,
                                "method": "session/request_permission",
                                "params": {
                                    "sessionId": "s-1",
                                    "toolCall": { "toolCallId": "call-1", "title": "write_file" },
                                    "options": [
                                        { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                                        { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
                                    ]
                                }
                            }),
                        )
                        .await;
                        // The main loop is parked here, so the permission
                        // response must be read (and recorded) inline.
                        read_message(&mut reader, &sent).await;
                    }
                    for update in &script.updates {
                        write_message(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0", "method": "session/update",
                                "params": { "sessionId": "s-1", "update": update }
                            }),
                        )
                        .await;
                    }
                    write_message(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "s-1", "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": { "type": "text", "text": script.final_text.as_deref().unwrap_or("hi from agent") }
                            } }
                        }),
                    )
                    .await;
                    write_message(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "s-1", "update": {
                                "sessionUpdate": "tool_call",
                                "toolCallId": "t1", "title": "read_file", "status": "pending"
                            } }
                        }),
                    )
                    .await;
                    // A real agent only reports `cancelled` after the client's
                    // `session/cancel` reaches it — park until it does.
                    if script.await_cancel {
                        loop {
                            match read_message(&mut reader, &sent).await {
                                Some(message)
                                    if message.get("method").and_then(Value::as_str)
                                        == Some("session/cancel") =>
                                {
                                    break
                                }
                                Some(_) => continue,
                                None => return,
                            }
                        }
                    }
                    write_message(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "stopReason": script.stop_reason() }
                        }),
                    )
                    .await;
                }
                _ => {
                    if id.is_some() {
                        write_message(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0", "id": id,
                                "error": { "code": -32601, "message": "method not found" }
                            }),
                        )
                        .await;
                    }
                }
            }
        }
    }

    /// Wire a client to a freshly-spawned scripted agent over two duplex pipes,
    /// completing the handshake. Returns the connected client and the recorded
    /// client→agent wire.
    async fn connect_to_scripted_agent(script: Script) -> (AcpClient, Sent) {
        connect_with_options(script, AcpSessionOptions::default())
            .await
            .expect("handshake completes")
    }

    #[tokio::test]
    async fn cancelled_handshakes_close_their_agent_transport() {
        let (client, agent) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(client);
        let pending = tokio::spawn(AcpClient::connect(reader, writer, "."));
        let mut agent = BufReader::new(agent);
        let mut request = String::new();
        tokio::time::timeout(Duration::from_secs(2), agent.read_line(&mut request))
            .await
            .expect("initialize arrives")
            .expect("initialize request");
        assert!(request.contains("initialize"));
        pending.abort();
        let _ = pending.await;
        let mut remaining = String::new();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), agent.read_line(&mut remaining))
                .await
                .expect("transport closes")
                .expect("read EOF"),
            0
        );
    }

    /// [`connect_to_scripted_agent`] without the success expectation, so a
    /// handshake failure is itself assertable.
    async fn connect_with_options(
        script: Script,
        options: AcpSessionOptions,
    ) -> Result<(AcpClient, Sent), AcpClientError> {
        // agent -> client, and client -> agent.
        let (client_reads, agent_writes) = tokio::io::duplex(8192);
        let (agent_reads, client_writes) = tokio::io::duplex(8192);
        let sent: Sent = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(scripted_agent(
            agent_reads,
            agent_writes,
            script,
            Arc::clone(&sent),
        ));
        // A caller-friendly relative cwd is canonicalized before session/new;
        // a real agent rejects relative ACP working-directory URIs.
        let client = AcpClient::connect_with(client_reads, client_writes, ".", options).await?;
        Ok((client, sent))
    }

    #[tokio::test]
    async fn dropping_acp_handshake_closes_its_transport() {
        use tokio::io::AsyncReadExt;
        let (client_reads, _agent_writes) = tokio::io::duplex(8192);
        let (mut agent_reads, client_writes) = tokio::io::duplex(8192);
        {
            let connection = AcpClient::connect(client_reads, client_writes, ".");
            tokio::pin!(connection);
            let mut byte = [0_u8; 1];
            tokio::select! {
                _ = &mut connection => panic!("the silent agent must not finish handshaking"),
                read = agent_reads.read(&mut byte) => assert_eq!(read.unwrap(), 1),
            }
        }
        let mut remaining = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            agent_reads.read_to_end(&mut remaining),
        )
        .await
        .expect("cancelling initialization closes its transport")
        .unwrap();
    }

    #[tokio::test]
    async fn client_rejects_a_provider_error_streamed_as_an_end_turn_answer() {
        let script = Script {
            final_text: Some(r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"The selected model is unavailable for this account."}}"#.to_string()),
            ..Script::default()
        };
        let (mut client, _sent) = connect_to_scripted_agent(script).await;
        let mut sink = RecordingSink {
            events: Arc::new(Mutex::new(Vec::new())),
        };
        let error = client
            .prompt("check the selected model", RunId::new(), &mut sink)
            .await
            .expect_err("a provider error must not complete the run successfully");
        assert!(error
            .to_string()
            .contains("The selected model is unavailable for this account."));
    }

    #[tokio::test]
    async fn client_delegates_a_prompt_and_maps_streamed_updates() {
        let (mut client, _sent) = connect_to_scripted_agent(Script::default()).await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut sink = RecordingSink {
            events: Arc::clone(&events),
        };
        let run_id = RunId::new();

        let stop = client
            .prompt("do the thing", run_id, &mut sink)
            .await
            .expect("prompt resolves");
        assert_eq!(stop, AcpStopReason::EndTurn);

        let events = events.lock().unwrap().clone();
        assert!(
            events.contains(&EventBody::ModelStreamDelta {
                run_id,
                text: "hi from agent".to_string(),
                thought: false,
            }),
            "expected a ModelStreamDelta from the streamed chunk, got {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                EventBody::ToolStarted { tool, label, .. } if tool == "acp:t1" && label.as_deref() == Some("read_file")
            )),
            "expected a ToolStarted(read_file) from the streamed tool_call, got {events:?}"
        );
    }

    #[tokio::test]
    async fn client_answers_a_permission_request_with_the_sinks_choice() {
        let (mut client, sent) = connect_to_scripted_agent(Script {
            permission: true,
            ..Script::default()
        })
        .await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut sink = RecordingSink {
            events: Arc::clone(&events),
        };
        let run_id = RunId::new();

        let stop = client
            .prompt("do the thing", run_id, &mut sink)
            .await
            .expect("prompt resolves");
        assert_eq!(stop, AcpStopReason::EndTurn);

        // The agent received the sink's choice as an ACP `selected`/`allow` outcome.
        let response = sent
            .lock()
            .unwrap()
            .iter()
            .find(|message| message.pointer("/result/outcome").is_some())
            .cloned()
            .expect("agent captured a permission response");
        assert_eq!(response["result"]["outcome"]["outcome"], json!("selected"));
        assert_eq!(response["result"]["outcome"]["optionId"], json!("allow"));

        // Streamed updates still flow after the permission round-trip.
        let events = events.lock().unwrap().clone();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EventBody::ModelStreamDelta { .. })),
            "expected streamed updates after the permission, got {events:?}"
        );
    }

    #[tokio::test]
    async fn client_streams_tool_lifecycle_fixture_with_stable_identity_and_action_labels() {
        let updates =
            serde_json::from_str(include_str!("../../../testdata/acp-tool-lifecycle.json"))
                .unwrap();
        let (mut client, _sent) = connect_to_scripted_agent(Script {
            updates,
            ..Script::default()
        })
        .await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut sink = RecordingSink {
            events: Arc::clone(&events),
        };
        let run_id = RunId::new();
        client
            .prompt("review fixture", run_id, &mut sink)
            .await
            .unwrap();
        let events = events.lock().unwrap();
        let fixture: Vec<_> = events
            .iter()
            .filter(|event| match event {
                EventBody::ToolStarted { tool, .. }
                | EventBody::ToolUpdated { tool, .. }
                | EventBody::ToolCompleted { tool, .. } => tool.starts_with("acp:fixture-"),
                _ => false,
            })
            .collect();
        assert_eq!(
            fixture
                .iter()
                .filter(|event| matches!(event, EventBody::ToolStarted { .. }))
                .count(),
            2
        );
        assert_eq!(
            fixture
                .iter()
                .filter(|event| matches!(event, EventBody::ToolUpdated { .. }))
                .count(),
            3
        );
        assert_eq!(
            fixture
                .iter()
                .filter(|event| matches!(event, EventBody::ToolCompleted { .. }))
                .count(),
            2
        );
        assert!(fixture.iter().any(|event| matches!(event,
            EventBody::ToolUpdated { tool, label, .. } if tool == "acp:fixture-read"
                && label.as_deref() == Some("Read File · src/main.rs"))));
        assert!(fixture.iter().any(|event| matches!(event,
            EventBody::ToolUpdated { tool, label, .. } if tool == "acp:fixture-terminal"
                && label.as_deref() == Some("Terminal · git status --short"))));
        assert!(fixture.iter().any(|event| matches!(event,
            EventBody::ToolCompleted { tool, outcome: ToolOutcome::Failed { message }, .. }
                if tool == "acp:fixture-terminal" && message == "fixture command failed")));
        assert!(fixture.iter().any(|event| matches!(event,
            EventBody::ToolCompleted { tool, outcome: ToolOutcome::Succeeded, .. }
                if tool == "acp:fixture-read")));
        // An explicit QA output path lets the native acceptance harness seed
        // these real wire-mapped events into its disposable session ledger.
        if let Some(path) = std::env::var_os("CODY_ACP_FIXTURE_OUTPUT") {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .unwrap();
            file.write_all(
                &serde_json::to_vec_pretty(&serde_json::json!({
                    "run_id": run_id, "events": fixture,
                }))
                .unwrap(),
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn client_preserves_streamed_search_failure_details() {
        let (mut client, _) = connect_to_scripted_agent(Script {
            updates: vec![
                json!({"sessionUpdate":"tool_call","toolCallId":"killed-search","title":"Search","kind":"search","status":"in_progress"}),
                json!({"sessionUpdate":"tool_call_update","toolCallId":"killed-search","rawOutput":{"formatted_output":"","exit_code":137}}),
                json!({"sessionUpdate":"tool_call_update","toolCallId":"other","rawOutput":{"stderr":"unrelated tool"}}),
                json!({"sessionUpdate":"tool_call_update","toolCallId":"killed-search","status":"failed"}),
            ],
            ..Script::default()
        }).await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut sink = RecordingSink {
            events: Arc::clone(&events),
        };
        client
            .prompt("search the repository", RunId::new(), &mut sink)
            .await
            .unwrap();
        assert!(events.lock().unwrap().iter().any(|event| matches!(event,
            EventBody::ToolCompleted { tool, outcome: ToolOutcome::Failed { message }, .. }
            if tool == "acp:killed-search" && message.contains("137") && message.contains("SIGKILL") && !message.contains("unrelated")
        )));
    }

    // -- model discovery ----------------------------------------------------

    #[tokio::test]
    async fn handshake_captures_the_agents_models_modes_and_auth_methods() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "configOptions".to_string(),
            json!([wire_model_selector(
                "agent-model-2",
                &[("agent-model-1", "First"), ("agent-model-2", "Second")]
            )]),
        );
        extra.insert(
            "modes".to_string(),
            json!({
                "currentModeId": "build",
                "availableModes": [
                    { "id": "build", "name": "Build" },
                    { "id": "review", "name": "Review" },
                ]
            }),
        );
        let (client, _sent) = connect_to_scripted_agent(Script {
            auth_methods: json!([
                { "id": "agent-login", "name": "Agent login", "description": "Sign in first" }
            ]),
            session_new_extra: extra,
            ..Script::default()
        })
        .await;

        assert_eq!(
            client.discovered_models(),
            vec![
                AcpModel {
                    id: "agent-model-1".to_string(),
                    name: "First".to_string(),
                    current: false,
                },
                AcpModel {
                    id: "agent-model-2".to_string(),
                    name: "Second".to_string(),
                    current: true,
                },
            ]
        );
        assert_eq!(
            client
                .discovered_modes()
                .iter()
                .map(|mode| (mode.id.as_str(), mode.current))
                .collect::<Vec<_>>(),
            vec![("build", true), ("review", false)]
        );
        assert_eq!(
            client.auth_methods(),
            vec![AcpAuthMethod {
                id: "agent-login".to_string(),
                name: "Agent login".to_string(),
                description: Some("Sign in first".to_string()),
            }]
        );
    }

    #[tokio::test]
    async fn an_agent_without_config_options_discovers_no_models() {
        // The pre-discovery degradation path: the agent's own default model
        // applies and nothing is invented on its behalf.
        let (client, _sent) = connect_to_scripted_agent(Script::default()).await;
        assert!(client.discovered_models().is_empty());
        assert!(client.discovered_modes().is_empty());
        assert!(client.auth_methods().is_empty());
    }

    #[tokio::test]
    async fn set_model_speaks_set_config_option_and_folds_the_refreshed_set() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "configOptions".to_string(),
            json!([wire_model_selector(
                "agent-model-1",
                &[("agent-model-1", "First"), ("agent-model-2", "Second")]
            )]),
        );
        let (mut client, sent) = connect_to_scripted_agent(Script {
            session_new_extra: extra,
            set_config_options: Some(json!([wire_model_selector(
                "agent-model-2",
                &[("agent-model-1", "First"), ("agent-model-2", "Second")]
            )])),
            ..Script::default()
        })
        .await;

        client
            .set_model("agent-model-2")
            .await
            .expect("agent accepts its own model id");

        let request = sent_method(&sent, "session/set_config_option")
            .expect("client sent session/set_config_option");
        assert_eq!(request["params"]["sessionId"], json!("s-1"));
        assert_eq!(request["params"]["configId"], json!("model"));
        assert_eq!(request["params"]["value"], json!("agent-model-2"));
        // The response's full refreshed set became the new discovery state.
        assert_eq!(
            client
                .discovered_models()
                .into_iter()
                .find(|model| model.current)
                .map(|model| model.id),
            Some("agent-model-2".to_string())
        );
    }

    #[tokio::test]
    async fn set_model_fails_without_ever_guessing_a_selector() {
        // No Model-category option was advertised: refuse locally rather than
        // inventing a `configId` the agent never named.
        let (mut client, sent) = connect_to_scripted_agent(Script::default()).await;
        let error = client
            .set_model("agent-model-1")
            .await
            .expect_err("no model selector to target");
        assert!(
            error.to_string().contains("no model selector"),
            "unexpected error: {error}"
        );
        assert!(sent_method(&sent, "session/set_config_option").is_none());
    }

    #[tokio::test]
    async fn a_rejected_model_surfaces_the_agents_own_error() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "configOptions".to_string(),
            json!([wire_model_selector(
                "agent-model-1",
                &[("agent-model-1", "First")]
            )]),
        );
        let (mut client, _sent) = connect_to_scripted_agent(Script {
            session_new_extra: extra,
            // `set_config_options: None` answers with a JSON-RPC error.
            ..Script::default()
        })
        .await;
        let error = client
            .set_model("not-a-model")
            .await
            .expect_err("the agent rejects an unknown model id");
        assert!(
            error.to_string().contains("set_config_option"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn a_failed_session_new_names_the_agents_advertised_auth_methods() {
        let error = connect_with_options(
            Script {
                auth_methods: json!([
                    { "id": "agent-login", "name": "Agent login", "description": "Sign in first" }
                ]),
                session_new_error: Some(json!({ "code": -32000, "message": "auth required" })),
                ..Script::default()
            },
            AcpSessionOptions::default(),
        )
        .await
        .err()
        .expect("session/new failed");
        let message = error.to_string();
        assert!(message.contains("session/new failed"), "{message}");
        assert!(
            message.contains("Agent login") && message.contains("Sign in first"),
            "the advertised auth methods must be surfaced verbatim: {message}"
        );
    }

    #[tokio::test]
    async fn a_failed_session_new_invents_no_remedy_the_agent_did_not_offer() {
        let error = connect_with_options(
            Script {
                session_new_error: Some(json!({ "code": -32000, "message": "boom" })),
                ..Script::default()
            },
            AcpSessionOptions::default(),
        )
        .await
        .err()
        .expect("session/new failed");
        let message = error.to_string();
        assert!(message.contains("session/new failed"), "{message}");
        assert!(
            !message.contains("authentication"),
            "no auth methods were advertised, so none may be suggested: {message}"
        );
    }

    #[tokio::test]
    async fn an_agent_initiated_model_switch_notes_the_run_and_refreshes_discovery() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "configOptions".to_string(),
            json!([wire_model_selector(
                "agent-model-1",
                &[("agent-model-1", "First"), ("agent-model-2", "Second")]
            )]),
        );
        let (mut client, _sent) = connect_to_scripted_agent(Script {
            session_new_extra: extra,
            updates: vec![json!({
                "sessionUpdate": "config_option_update",
                "configOptions": [wire_model_selector(
                    "agent-model-2",
                    &[("agent-model-1", "First"), ("agent-model-2", "Second")]
                )]
            })],
            ..Script::default()
        })
        .await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut sink = RecordingSink {
            events: Arc::clone(&events),
        };

        client
            .prompt("do the thing", RunId::new(), &mut sink)
            .await
            .expect("prompt resolves");

        let events = events.lock().unwrap().clone();
        assert!(
            events.iter().any(|event| matches!(
                event,
                EventBody::NoteAppended { text, .. } if text.contains("agent-model-2")
            )),
            "an agent-initiated model switch must reach the transcript, got {events:?}"
        );
        assert_eq!(
            client
                .discovered_models()
                .into_iter()
                .find(|model| model.current)
                .map(|model| model.id),
            Some("agent-model-2".to_string()),
            "the live discovery state must follow the agent's announcement"
        );
    }

    #[tokio::test]
    async fn the_cancel_handle_sends_session_cancel_during_a_turn() {
        let (mut client, sent) = connect_to_scripted_agent(Script {
            await_cancel: true,
            ..Script::default()
        })
        .await;
        let cancel = client.cancel_handle();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut sink = RecordingSink {
            events: Arc::clone(&events),
        };

        let canceller = tokio::spawn(async move {
            // The scripted agent parks until `session/cancel` arrives, so this
            // races only against the turn never being cancellable at all.
            cancel.cancel().await;
        });
        let stop = client
            .prompt("do the thing", RunId::new(), &mut sink)
            .await
            .expect("prompt resolves");
        canceller.await.expect("canceller joins");

        assert_eq!(
            stop,
            AcpStopReason::Cancelled,
            "the agent's own `cancelled` stop reason must be honored"
        );
        let cancel_notification =
            sent_method(&sent, "session/cancel").expect("client sent session/cancel");
        assert_eq!(cancel_notification["params"]["sessionId"], json!("s-1"));
        assert!(
            cancel_notification.get("id").is_none_or(Value::is_null),
            "session/cancel is a notification, not a request"
        );
    }

    #[tokio::test]
    async fn configured_mcp_servers_are_forwarded_into_session_new() {
        let (_client, sent) = connect_with_options(
            Script::default(),
            AcpSessionOptions {
                mcp_servers: vec![AcpMcpServer {
                    name: "docs".to_string(),
                    command: "server-bin".to_string(),
                    args: vec!["--stdio".to_string()],
                }],
                ..AcpSessionOptions::default()
            },
        )
        .await
        .expect("handshake completes");

        let request = sent_method(&sent, "session/new").expect("client sent session/new");
        let servers = request["params"]["mcpServers"]
            .as_array()
            .expect("mcpServers array");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["name"], json!("docs"));
        assert_eq!(servers[0]["command"], json!("server-bin"));
        assert_eq!(servers[0]["args"], json!(["--stdio"]));
        assert_eq!(
            servers[0]["env"],
            json!([]),
            "a forwarded launch spec never carries environment pairs"
        );
    }

    #[test]
    fn forwardable_mcp_servers_withholds_secret_bearing_and_hermetic_servers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp.toml");
        std::fs::write(
            &path,
            r#"
[[server]]
name = "plain"
command = "server-bin"
args = ["--stdio"]

[[server]]
name = "with-secret"
command = "server-bin"
env = [["TOKEN", "s3cret"]]

[[server]]
name = "hermetic"
command = "server-bin"
inherit_environment = false
"#,
        )
        .expect("write mcp.toml");
        let servers = forwardable_mcp_servers(&path);
        assert_eq!(
            servers,
            vec![AcpMcpServer {
                name: "plain".to_string(),
                command: "server-bin".to_string(),
                args: vec!["--stdio".to_string()],
            }]
        );
    }

    #[test]
    fn forwarding_is_on_by_default_and_opt_out_is_honored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = "\n[[server]]\nname = \"plain\"\ncommand = \"server-bin\"\n";

        let default_path = dir.path().join("default.toml");
        std::fs::write(&default_path, server).expect("write");
        assert_eq!(forwardable_mcp_servers(&default_path).len(), 1);

        let opt_out = dir.path().join("opt-out.toml");
        std::fs::write(
            &opt_out,
            format!("[acp]\nforward_mcp_servers = false\n{server}"),
        )
        .expect("write");
        assert!(forwardable_mcp_servers(&opt_out).is_empty());

        // A missing file forwards nothing and never errors.
        assert!(forwardable_mcp_servers(&dir.path().join("absent.toml")).is_empty());
    }
}
