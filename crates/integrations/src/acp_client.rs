//! ACP (Agent Client Protocol) *client* — the inverse of `acp.rs`.
//!
//! `acp.rs` is the SERVER role (Codypendent serves ACP to Zed). This module is
//! the CLIENT/host role: Codypendent spawns an external ACP agent
//! (`gemini --acp`, `npx @agentclientprotocol/claude-agent-acp`, ...), does the
//! initialize/session handshake, delegates a run's objective as an ACP prompt,
//! and maps the agent's streamed `session/update`s onto Codypendent's existing
//! `EventBody` model. The agent owns its model; we send no model id.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionUpdate, ToolCall, ToolCallContent, ToolCallStatus,
    ToolCallUpdate,
};
// The ACP *client* surface (see the client section below). `ContentBlock` above
// is shared with the Task 6 mapping; the rest are client-only.
use agent_client_protocol::schema::v1::{
    InitializeRequest, NewSessionRequest, PermissionOption as WirePermissionOption,
    PermissionOptionKind, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionNotification, StopReason,
    TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectTo, ConnectionTo, Lines,
};

use crate::acp::PermissionOption;
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

/// Receives the events an ACP turn produces and answers the agent's permission
/// requests. The daemon implements this to fan mapped events into a run's ledger
/// and to route a permission through the existing approval broker (that daemon
/// wiring is a follow-up — this task is the client + its mock-agent test); tests
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
    _driver: JoinHandle<Result<(), AcpClientError>>,
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
        Self::connect_transport(tokio_transport(reader, writer), cwd).await
    }

    /// Spawn `command args` (with `env`) as a child ACP agent and connect over
    /// its stdio. `env` carries the provider config's environment (secrets are
    /// referenced by NAME upstream and resolved into `env` by the caller — never
    /// stored here). The agent owns its model; no model id is ever sent.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &str,
    ) -> Result<AcpClient, AcpClientError> {
        let config = AcpAgentConfig::new(command)
            .args(args.iter().cloned())
            .envs(env.clone());
        Self::connect_transport(AcpAgent::new(config), cwd).await
    }

    async fn connect_transport<T>(transport: T, cwd: &str) -> Result<AcpClient, AcpClientError>
    where
        T: ConnectTo<Client> + Send + 'static,
    {
        let (ready_tx, ready_rx) = oneshot::channel();
        let (commands, command_rx) = mpsc::channel(PROMPT_QUEUE_DEPTH);
        let driver = tokio::spawn(run_connection(
            transport,
            PathBuf::from(cwd),
            ready_tx,
            command_rx,
        ));
        match ready_rx.await {
            Ok(Ok(())) => Ok(AcpClient {
                commands,
                _driver: driver,
            }),
            // The driver reported a specific handshake failure before returning.
            Ok(Err(error)) => Err(error),
            // The driver dropped `ready_tx` without signalling (e.g. the
            // transport itself failed before `main_fn` ran): recover its error.
            Err(_) => match driver.await {
                Ok(Err(error)) => Err(error),
                Ok(Ok(())) => Err(AcpClientError::Handshake(
                    "acp connection closed before completing the handshake".to_string(),
                )),
                Err(join_error) => Err(AcpClientError::Handshake(format!(
                    "acp connection task failed: {join_error}"
                ))),
            },
        }
    }

    /// Delegate `objective` to the agent as one ACP `session/prompt` turn,
    /// feeding every mapped `session/update` event and permission request to
    /// `sink`, and returning why the turn ended. No model id is sent.
    pub async fn prompt(
        &mut self,
        objective: &str,
        run_id: RunId,
        sink: &mut dyn AcpEventSink,
    ) -> Result<AcpStopReason, AcpClientError> {
        let (events, mut incoming) = mpsc::unbounded_channel();
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

        while let Some(message) = incoming.recv().await {
            match message {
                PromptOut::Event(event) => sink.on_event(event).await,
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
                PromptOut::Done(stop) => return Ok(stop),
                PromptOut::Failed(reason) => return Err(AcpClientError::Prompt(reason)),
            }
        }
        Err(AcpClientError::Prompt(
            "acp connection closed before the prompt completed".to_string(),
        ))
    }
}

/// A prompt turn's live routing: which run the streamed updates belong to and
/// where to push the mapped events / permission asks. The driver installs it for
/// the duration of one `session/prompt`; the notification and permission
/// callbacks read it to reach the in-flight [`AcpClient::prompt`] call.
#[derive(Clone)]
struct ActivePrompt {
    run_id: RunId,
    events: mpsc::UnboundedSender<PromptOut>,
}

/// A command from an [`AcpClient`] handle to its connection driver.
enum PromptCommand {
    Prompt {
        objective: String,
        run_id: RunId,
        events: mpsc::UnboundedSender<PromptOut>,
    },
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
/// handshake (signalling `ready`), then service prompt commands until every
/// [`AcpClient`] handle drops and `commands` closes.
async fn run_connection<T>(
    transport: T,
    cwd: PathBuf,
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
        .on_receive_notification(
            {
                let active = Arc::clone(&active);
                async move |notification: SessionNotification, _cx| {
                    let current = active
                        .lock()
                        .expect("acp active-prompt mutex poisoned")
                        .clone();
                    if let Some(prompt) = current {
                        for event in session_update_to_events(&notification.update, prompt.run_id) {
                            let _ = prompt.events.send(PromptOut::Event(event));
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
            // Handshake. The agent owns its model — we send no model id.
            if let Err(error) = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await
            {
                let _ = ready.send(Err(AcpClientError::Handshake(format!(
                    "initialize failed: {error}"
                ))));
                return Err(error);
            }
            let session_id = match cx
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await
            {
                Ok(response) => response.session_id,
                Err(error) => {
                    let _ = ready.send(Err(AcpClientError::Handshake(format!(
                        "session/new failed: {error}"
                    ))));
                    return Err(error);
                }
            };
            let _ = ready.send(Ok(()));

            // One delegated prompt turn per command. The `active` slot lets the
            // concurrently-dispatched update/permission callbacks reach this
            // turn's event channel and run id while the request is in flight.
            while let Some(command) = commands.recv().await {
                let PromptCommand::Prompt {
                    objective,
                    run_id,
                    events,
                } = command;
                *active.lock().expect("acp active-prompt mutex poisoned") = Some(ActivePrompt {
                    run_id,
                    events: events.clone(),
                });
                let result = cx
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new(objective))],
                    ))
                    .block_task()
                    .await;
                *active.lock().expect("acp active-prompt mutex poisoned") = None;
                let resolved = match result {
                    Ok(response) => PromptOut::Done(map_stop_reason(response.stop_reason)),
                    Err(error) => PromptOut::Failed(format!("session/prompt failed: {error}")),
                };
                let _ = events.send(resolved);
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
    let current = active
        .lock()
        .expect("acp active-prompt mutex poisoned")
        .clone();
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

#[cfg(test)]
mod client_tests {
    //! End-to-end tests over a scripted in-process ACP *agent* peer that speaks
    //! the real newline-delimited JSON-RPC 2.0 wire over `tokio::io::duplex`
    //! (mirroring the harness in `crate::acp`'s tests). They assert the handshake
    //! completes, a prompt is delegated, streamed `session/update`s reach
    //! `session_update_to_events`, and a `session/request_permission` maps onto
    //! the sink's approval.

    use super::*;
    use serde_json::json;
    use tokio::io::AsyncBufRead;

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

    /// What the scripted agent does on `session/prompt`.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Script {
        /// Stream a text chunk + a tool_call, then resolve `end_turn`.
        PlainTurn,
        /// First request permission (and capture the client's answer), then as above.
        PermissionThenTurn,
    }

    async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &Value) {
        let mut line = serde_json::to_string(message).expect("serialize");
        line.push('\n');
        writer.write_all(line.as_bytes()).await.expect("write");
        writer.flush().await.expect("flush");
    }

    async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> Option<Value> {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await.ok()?;
        if read == 0 {
            return None;
        }
        serde_json::from_str(line.trim()).ok()
    }

    /// A scripted ACP *agent* peer. Answers `initialize` and `session/new`, then
    /// on `session/prompt` (optionally after a `session/request_permission`
    /// round-trip whose response it stores in `permission`) streams one text
    /// chunk + one tool_call and returns `stopReason: end_turn`. Reads/writes
    /// newline-delimited JSON-RPC on the duplex halves.
    async fn scripted_agent<R, W>(
        reader: R,
        mut writer: W,
        script: Script,
        permission: Arc<Mutex<Option<Value>>>,
    ) where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut reader = BufReader::new(reader);
        while let Some(message) = read_message(&mut reader).await {
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
                            "result": { "protocolVersion": 1, "agentCapabilities": {} }
                        }),
                    )
                    .await;
                }
                "session/new" => {
                    write_message(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "sessionId": "s-1" }
                        }),
                    )
                    .await;
                }
                "session/prompt" => {
                    if script == Script::PermissionThenTurn {
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
                        // The only thing the client sends during the turn is the
                        // permission response; capture it for the assertion.
                        if let Some(response) = read_message(&mut reader).await {
                            *permission.lock().unwrap() = Some(response);
                        }
                    }
                    write_message(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "s-1", "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": { "type": "text", "text": "hi from agent" }
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
                    write_message(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "stopReason": "end_turn" }
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
    /// completing the handshake. Returns the connected client and the slot the
    /// agent records a captured permission response into.
    async fn connect_to_scripted_agent(script: Script) -> (AcpClient, Arc<Mutex<Option<Value>>>) {
        // agent -> client, and client -> agent.
        let (client_reads, agent_writes) = tokio::io::duplex(8192);
        let (agent_reads, client_writes) = tokio::io::duplex(8192);
        let permission = Arc::new(Mutex::new(None));
        tokio::spawn(scripted_agent(
            agent_reads,
            agent_writes,
            script,
            Arc::clone(&permission),
        ));
        let client = AcpClient::connect(client_reads, client_writes, "/tmp/repo")
            .await
            .expect("handshake completes");
        (client, permission)
    }

    #[tokio::test]
    async fn client_delegates_a_prompt_and_maps_streamed_updates() {
        let (mut client, _permission) = connect_to_scripted_agent(Script::PlainTurn).await;
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
            }),
            "expected a ModelStreamDelta from the streamed chunk, got {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                EventBody::ToolStarted { tool, .. } if tool == "read_file"
            )),
            "expected a ToolStarted(read_file) from the streamed tool_call, got {events:?}"
        );
    }

    #[tokio::test]
    async fn client_answers_a_permission_request_with_the_sinks_choice() {
        let (mut client, permission) = connect_to_scripted_agent(Script::PermissionThenTurn).await;
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
        let response = permission
            .lock()
            .unwrap()
            .clone()
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
}
