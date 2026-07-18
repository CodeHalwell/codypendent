//! Zed ACP (Agent Client Protocol) adapter (Phase 3 STEP 3.6).
//!
//! ACP is the protocol the [Zed](https://zed.dev) editor speaks to an external
//! coding agent: newline-delimited JSON-RPC 2.0 over a child process's stdio.
//! Per ADR-002 it is an *adapter*, never the internal protocol — so this module
//! is a thin, self-contained translation layer that turns ACP wire messages
//! into calls on an [`AcpBackend`] and streams the backend's progress back out
//! as `session/update` notifications.
//!
//! # Wire format
//!
//! Every message is a single JSON object on its own `\n`-terminated line, with
//! `"jsonrpc": "2.0"`. Requests carry `id` + `method` + `params`; responses
//! carry `id` + (`result` | `error`); notifications carry `method` + `params`
//! and no `id`. Parsing is deliberately tolerant: a malformed line is skipped,
//! an unknown request method returns JSON-RPC error `-32601`, and an unknown
//! notification is ignored.
//!
//! # Roles
//!
//! Here the *agent* is the server (we drive the model) and Zed is the *client*.
//! The methods we answer are [`initialize`], [`session/new`], [`session/prompt`]
//! (requests) and [`session/cancel`] (a notification). The messages we send are
//! `session/update` notifications and `session/request_permission` requests
//! whose result the client returns to us.
//!
//! [`initialize`]: AcpBackend
//! [`session/new`]: AcpBackend::new_session
//! [`session/prompt`]: AcpBackend::prompt
//! [`session/cancel`]: PromptSink::cancelled
//!
//! # Decoupling
//!
//! [`serve`] is generic over an [`AcpBackend`]; it never touches the daemon
//! directly. That keeps the whole protocol surface unit-testable over an
//! in-memory pipe with a fake backend, and lets the assembly layer wire the
//! real daemon behind the same trait.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch, Mutex};

/// The default ACP protocol version, used when the client omits it.
const DEFAULT_PROTOCOL_VERSION: u32 = 1;
/// JSON-RPC "method not found" error code.
const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC implementation-defined server error, used when a backend call fails.
const BACKEND_ERROR: i64 = -32000;
/// Ceiling on one incoming JSON-RPC line. `read_line` would otherwise grow the
/// buffer until a `\n` arrives — a single unterminated multi-GB line OOMs the
/// process. An over-long line is skipped (tolerantly, like a malformed one).
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
/// Outgoing-channel depth. Bounded so a backend streaming faster than a stalled
/// client drains applies backpressure instead of growing the queue unboundedly.
const OUT_CHANNEL_DEPTH: usize = 256;

/// A failure inside the ACP adapter.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// An I/O failure reading from or writing to the transport.
    #[error("acp I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A message could not be (de)serialized.
    #[error("acp serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// The backend refused or failed to service a request. The payload is a
    /// short human-readable reason surfaced to the client as a JSON-RPC error.
    #[error("acp backend error: {0}")]
    Backend(String),
    /// Any other failure, carried transparently from an implementation.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Why a prompt turn ended, mapped to the ACP `stopReason` wire value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The agent completed its turn normally.
    EndTurn,
    /// The turn was cancelled by the client (`session/cancel`).
    Cancelled,
    /// The agent declined to act on the prompt.
    Refusal,
}

impl StopReason {
    /// The ACP wire string for this stop reason.
    fn as_wire(self) -> &'static str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::Cancelled => "cancelled",
            StopReason::Refusal => "refusal",
        }
    }
}

/// One choice offered to the client in a `session/request_permission` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    /// Stable identifier the client echoes back when this option is selected.
    pub option_id: String,
    /// Human-readable label shown to the user.
    pub name: String,
    /// The option's kind (e.g. `allow_once`, `reject_once`); opaque to us.
    pub kind: String,
}

/// The client's answer to a `session/request_permission` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// The user selected the option with this `optionId`.
    Selected(String),
    /// The user dismissed the request without choosing.
    Cancelled,
}

/// The handle a backend uses, during a single prompt turn, to stream progress
/// back to the client and to ask the client for permission.
///
/// It also exposes cancellation: when the client sends `session/cancel` for the
/// in-flight prompt, [`cancelled`](PromptSink::cancelled) resolves and
/// [`is_cancelled`](PromptSink::is_cancelled) starts returning `true`, so a
/// long-running backend can bail out and return [`StopReason::Cancelled`].
#[async_trait]
pub trait PromptSink: Send {
    /// Push a `session/update` notification to the client. `update` is the
    /// opaque update payload (an ACP `SessionNotification` body).
    async fn update(&mut self, update: Value);

    /// Ask the client to authorize a tool call, blocking until it answers.
    /// `tool_call` describes the pending call; `options` are the choices the
    /// user picks from.
    async fn request_permission(
        &mut self,
        tool_call: Value,
        options: Vec<PermissionOption>,
    ) -> PermissionOutcome;

    /// Whether the client has cancelled this prompt turn.
    fn is_cancelled(&self) -> bool;

    /// Resolve once the client cancels this prompt turn (immediately, if it
    /// already has).
    async fn cancelled(&mut self);
}

/// The daemon-facing surface the ACP adapter drives. Implementations own the
/// real session and model loop; the adapter only translates the wire protocol.
#[async_trait]
pub trait AcpBackend: Send + Sync {
    /// Create a new session and return its identifier.
    async fn new_session(&self) -> Result<String, AcpError>;

    /// Run one prompt turn for `session_id` with the client's `text`, streaming
    /// progress and permission requests through `ctx`, and return why the turn
    /// ended.
    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        ctx: &mut dyn PromptSink,
    ) -> Result<StopReason, AcpError>;
}

/// Outgoing permission requests awaiting a client response, keyed by our
/// JSON-RPC request id.
type PendingPermissions = Arc<Mutex<HashMap<i64, oneshot::Sender<PermissionOutcome>>>>;
/// Per-session cancellation signals for in-flight prompts, keyed by session id.
/// The generation distinguishes prompts on the same session: a finished older
/// prompt's cleanup must not remove a newer prompt's cancel flag.
type CancelFlags = Arc<Mutex<HashMap<String, (u64, watch::Sender<bool>)>>>;

/// A tolerantly-parsed JSON-RPC message. Unknown fields (e.g. `jsonrpc`) are
/// ignored; the combination of present fields classifies it as a request,
/// notification, or response.
#[derive(Debug, Deserialize)]
struct Incoming {
    /// Present on requests and responses; absent on notifications.
    #[serde(default)]
    id: Option<Value>,
    /// Present on requests and notifications; absent on responses.
    #[serde(default)]
    method: Option<String>,
    /// Request/notification parameters.
    #[serde(default)]
    params: Option<Value>,
    /// A successful response payload.
    #[serde(default)]
    result: Option<Value>,
    /// An error response payload.
    #[serde(default)]
    #[allow(dead_code)]
    error: Option<Value>,
}

/// Build a JSON-RPC success response.
fn response_msg(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Build a JSON-RPC error response.
fn error_msg(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Concatenate the `text` of every `{ "type": "text", .. }` block in a
/// `session/prompt` params `prompt` array.
fn extract_prompt_text(params: Option<&Value>) -> String {
    let Some(blocks) = params
        .and_then(|p| p.get("prompt"))
        .and_then(|v| v.as_array())
    else {
        return String::new();
    };
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse a `session/request_permission` response body into an outcome.
fn parse_outcome(result: Option<&Value>) -> PermissionOutcome {
    let outcome = result.and_then(|r| r.get("outcome"));
    match outcome
        .and_then(|o| o.get("outcome"))
        .and_then(Value::as_str)
    {
        Some("selected") => {
            let option_id = outcome
                .and_then(|o| o.get("optionId"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            PermissionOutcome::Selected(option_id)
        }
        _ => PermissionOutcome::Cancelled,
    }
}

/// The [`PromptSink`] the adapter hands to the backend for one prompt turn. It
/// serializes outgoing traffic onto the shared writer channel and correlates
/// permission responses back through the [`PendingPermissions`] map.
struct ClientSink {
    session_id: String,
    out: mpsc::Sender<Value>,
    pending: PendingPermissions,
    id_counter: Arc<AtomicI64>,
    cancel: watch::Receiver<bool>,
}

#[async_trait]
impl PromptSink for ClientSink {
    async fn update(&mut self, update: Value) {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": { "sessionId": self.session_id, "update": update },
        });
        let _ = self.out.send(msg).await;
    }

    async fn request_permission(
        &mut self,
        tool_call: Value,
        options: Vec<PermissionOption>,
    ) -> PermissionOutcome {
        let id = self.id_counter.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let options_json =
            serde_json::to_value(&options).unwrap_or_else(|_| Value::Array(Vec::new()));
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/request_permission",
            "params": {
                "sessionId": self.session_id,
                "toolCall": tool_call,
                "options": options_json,
            },
        });
        if self.out.send(msg).await.is_err() {
            self.pending.lock().await.remove(&id);
            return PermissionOutcome::Cancelled;
        }
        // A `session/cancel` must interrupt an outstanding permission prompt —
        // otherwise the backend stays parked here until the client answers,
        // with the cancel flag set but never observed. (A manual borrow/changed
        // loop rather than `wait_for`, whose returned guard is not `Send`; if
        // the sender is gone the arm parks forever and `rx` decides.)
        let mut cancel = self.cancel.clone();
        tokio::select! {
            outcome = rx => match outcome {
                Ok(outcome) => outcome,
                Err(_) => PermissionOutcome::Cancelled,
            },
            _ = async {
                loop {
                    if *cancel.borrow() {
                        break;
                    }
                    if cancel.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
            } => {
                self.pending.lock().await.remove(&id);
                PermissionOutcome::Cancelled
            }
        }
    }

    fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    async fn cancelled(&mut self) {
        let _ = self.cancel.wait_for(|flag| *flag).await;
    }
}

/// Serve the ACP protocol over `reader`/`writer` (typically a child process's
/// stdin/stdout), dispatching to `backend`, until the client closes the input.
///
/// The read loop stays responsive during a prompt: each `session/prompt` runs
/// as its own task writing to an outgoing channel, so the loop keeps reading and
/// can route `session/cancel` notifications and `session/request_permission`
/// responses while the turn is in flight. A single writer task drains the
/// channel, so outgoing messages never interleave. One connection is served;
/// Zed drives one prompt at a time.
pub async fn serve<R, W, B>(mut reader: R, writer: W, backend: B) -> Result<(), AcpError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
    B: AcpBackend + 'static,
{
    let backend = Arc::new(backend);
    let pending: PendingPermissions = Arc::new(Mutex::new(HashMap::new()));
    let cancels: CancelFlags = Arc::new(Mutex::new(HashMap::new()));
    let id_counter = Arc::new(AtomicI64::new(1));
    let mut prompt_generation: u64 = 0;

    let (out_tx, mut out_rx) = mpsc::channel::<Value>(OUT_CHANNEL_DEPTH);

    // Single writer task: one JSON object per line, flushed after each so the
    // client sees progress promptly and messages from different senders (the
    // read loop and prompt tasks) never interleave mid-line.
    let writer_handle = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(msg) = out_rx.recv().await {
            let Ok(mut bytes) = serde_json::to_vec(&msg) else {
                continue;
            };
            bytes.push(b'\n');
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
    });

    let mut line: Vec<u8> = Vec::new();
    loop {
        match read_line_bounded(&mut reader, &mut line).await? {
            LineRead::Eof => break,          // client closed the input.
            LineRead::Oversized => continue, // skipped, tolerated like malformed.
            LineRead::Line => {}
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            continue; // Tolerate non-UTF-8 noise.
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(incoming) = serde_json::from_str::<Incoming>(trimmed) else {
            continue; // Tolerate a malformed line.
        };

        match (incoming.method, incoming.id) {
            // A request from the client.
            (Some(method), Some(id)) => match method.as_str() {
                "initialize" => {
                    let version = incoming
                        .params
                        .as_ref()
                        .and_then(|p| p.get("protocolVersion"))
                        .and_then(Value::as_u64)
                        .map_or(DEFAULT_PROTOCOL_VERSION, |v| v as u32);
                    let result = json!({
                        "protocolVersion": version,
                        "agentCapabilities": { "promptCapabilities": { "image": false } },
                    });
                    let _ = out_tx.send(response_msg(id, result)).await;
                }
                "session/new" => {
                    let msg = match backend.new_session().await {
                        Ok(session_id) => response_msg(id, json!({ "sessionId": session_id })),
                        Err(e) => error_msg(id, BACKEND_ERROR, &format!("new_session failed: {e}")),
                    };
                    let _ = out_tx.send(msg).await;
                }
                "session/prompt" => {
                    let session_id = incoming
                        .params
                        .as_ref()
                        .and_then(|p| p.get("sessionId"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let text = extract_prompt_text(incoming.params.as_ref());

                    // Register the cancel flag before spawning so a cancel that
                    // arrives on the very next line always finds it. Keyed with a
                    // generation: if a newer prompt for this session has replaced
                    // the entry, the older prompt's cleanup must leave it alone.
                    prompt_generation += 1;
                    let generation = prompt_generation;
                    let (cancel_tx, cancel_rx) = watch::channel(false);
                    cancels
                        .lock()
                        .await
                        .insert(session_id.clone(), (generation, cancel_tx));

                    let backend = Arc::clone(&backend);
                    let out = out_tx.clone();
                    let pending = Arc::clone(&pending);
                    let cancels = Arc::clone(&cancels);
                    let id_counter = Arc::clone(&id_counter);

                    tokio::spawn(async move {
                        let mut sink = ClientSink {
                            session_id: session_id.clone(),
                            out: out.clone(),
                            pending,
                            id_counter,
                            cancel: cancel_rx,
                        };
                        let response = match backend.prompt(&session_id, &text, &mut sink).await {
                            Ok(stop) => response_msg(id, json!({ "stopReason": stop.as_wire() })),
                            Err(e) => error_msg(id, BACKEND_ERROR, &format!("prompt failed: {e}")),
                        };
                        let _ = out.send(response).await;
                        let mut cancels = cancels.lock().await;
                        if cancels
                            .get(&session_id)
                            .is_some_and(|(current, _)| *current == generation)
                        {
                            cancels.remove(&session_id);
                        }
                    });
                }
                _ => {
                    let _ = out_tx
                        .send(error_msg(id, METHOD_NOT_FOUND, "method not found"))
                        .await;
                }
            },
            // A notification from the client.
            (Some(method), None) => {
                if method == "session/cancel" {
                    if let Some(session_id) = incoming
                        .params
                        .as_ref()
                        .and_then(|p| p.get("sessionId"))
                        .and_then(Value::as_str)
                    {
                        if let Some((_, tx)) = cancels.lock().await.get(session_id) {
                            let _ = tx.send(true);
                        }
                    }
                }
                // Unknown notifications are ignored.
            }
            // A response from the client to one of our outgoing requests.
            (None, Some(id)) => {
                if let Some(request_id) = id.as_i64() {
                    let sender = pending.lock().await.remove(&request_id);
                    if let Some(sender) = sender {
                        let _ = sender.send(parse_outcome(incoming.result.as_ref()));
                    }
                }
            }
            // Neither method nor id: not a valid JSON-RPC message; ignore.
            (None, None) => {}
        }
    }

    // Drop our sender so the writer task drains and exits once in-flight prompt
    // tasks have released their clones.
    drop(out_tx);
    let _ = writer_handle.await;
    Ok(())
}

/// The outcome of one bounded line read.
enum LineRead {
    /// End of input with nothing buffered.
    Eof,
    /// A complete line (without its terminator) is in the buffer.
    Line,
    /// The line exceeded [`MAX_LINE_BYTES`]; it was consumed and discarded.
    Oversized,
}

/// Read one `\n`-terminated line into `buf`, holding at most
/// [`MAX_LINE_BYTES`] in memory. An over-long line is drained from the input
/// (so framing stays intact) but not retained.
async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<LineRead> {
    buf.clear();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // EOF. A partial trailing line (no `\n`) is still delivered.
            return Ok(if oversized {
                LineRead::Oversized
            } else if buf.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Line
            });
        }
        if let Some(newline) = available.iter().position(|&b| b == b'\n') {
            if !oversized && buf.len() + newline <= MAX_LINE_BYTES {
                buf.extend_from_slice(&available[..newline]);
            } else {
                oversized = true;
            }
            reader.consume(newline + 1);
            return Ok(if oversized {
                LineRead::Oversized
            } else {
                LineRead::Line
            });
        }
        let chunk_len = available.len();
        if !oversized && buf.len() + chunk_len <= MAX_LINE_BYTES {
            buf.extend_from_slice(available);
        } else {
            oversized = true;
            buf.clear();
        }
        reader.consume(chunk_len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{split, AsyncWrite, BufReader, DuplexStream};

    /// A fake backend whose prompt behavior is selected per test.
    #[derive(Clone, Copy)]
    enum Mode {
        /// Return `EndTurn` immediately with no updates.
        Empty,
        /// Emit one `session/update`, then return `EndTurn`.
        UpdateThenEnd,
        /// Block until cancelled, then return `Cancelled`.
        WaitForCancel,
    }

    struct FakeBackend {
        mode: Mode,
    }

    #[async_trait]
    impl AcpBackend for FakeBackend {
        async fn new_session(&self) -> Result<String, AcpError> {
            Ok("sess-1".to_string())
        }

        async fn prompt(
            &self,
            _session_id: &str,
            _text: &str,
            ctx: &mut dyn PromptSink,
        ) -> Result<StopReason, AcpError> {
            match self.mode {
                Mode::Empty => Ok(StopReason::EndTurn),
                Mode::UpdateThenEnd => {
                    ctx.update(json!({ "kind": "agent_message_chunk", "text": "hi" }))
                        .await;
                    Ok(StopReason::EndTurn)
                }
                Mode::WaitForCancel => {
                    ctx.cancelled().await;
                    Ok(StopReason::Cancelled)
                }
            }
        }
    }

    /// Serialize `value` as a single JSON-RPC line and flush it.
    async fn write_msg<W: AsyncWrite + Unpin>(writer: &mut W, value: Value) {
        let mut line = serde_json::to_string(&value).expect("serialize");
        line.push('\n');
        writer.write_all(line.as_bytes()).await.expect("write line");
        writer.flush().await.expect("flush");
    }

    /// Read one JSON-RPC line and parse it.
    async fn read_msg<R: AsyncBufRead + Unpin>(reader: &mut R) -> Value {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.expect("read line");
        assert_ne!(n, 0, "unexpected EOF from server");
        serde_json::from_str(line.trim()).expect("parse line")
    }

    /// Spawn a server over an in-memory duplex and return the client's read and
    /// write halves.
    fn start(
        mode: Mode,
    ) -> (
        BufReader<tokio::io::ReadHalf<DuplexStream>>,
        tokio::io::WriteHalf<DuplexStream>,
    ) {
        let (client, server) = tokio::io::duplex(4096);
        let (server_read, server_write) = split(server);
        tokio::spawn(serve(
            BufReader::new(server_read),
            server_write,
            FakeBackend { mode },
        ));
        let (client_read, client_write) = split(client);
        (BufReader::new(client_read), client_write)
    }

    #[tokio::test]
    async fn initialize_echoes_protocol_version_and_capabilities() {
        let (mut reader, mut writer) = start(Mode::Empty);
        write_msg(
            &mut writer,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": 3 } }),
        )
        .await;

        let resp = read_msg(&mut reader).await;
        assert_eq!(resp["id"], json!(1));
        assert_eq!(resp["result"]["protocolVersion"], json!(3));
        assert_eq!(
            resp["result"]["agentCapabilities"]["promptCapabilities"]["image"],
            json!(false)
        );
    }

    #[tokio::test]
    async fn session_new_returns_non_empty_session_id() {
        let (mut reader, mut writer) = start(Mode::Empty);
        write_msg(
            &mut writer,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {} }),
        )
        .await;

        let resp = read_msg(&mut reader).await;
        assert_eq!(resp["id"], json!(2));
        let session_id = resp["result"]["sessionId"].as_str().expect("sessionId");
        assert!(!session_id.is_empty());
    }

    #[tokio::test]
    async fn prompt_streams_update_then_end_turn() {
        let (mut reader, mut writer) = start(Mode::UpdateThenEnd);
        write_msg(
            &mut writer,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                    "params": {
                        "sessionId": "sess-1",
                        "prompt": [ { "type": "text", "text": "hello" } ]
                    } }),
        )
        .await;

        // First: the streamed update notification.
        let update = read_msg(&mut reader).await;
        assert_eq!(update["method"], json!("session/update"));
        assert_eq!(update["params"]["sessionId"], json!("sess-1"));
        assert_eq!(update["params"]["update"]["text"], json!("hi"));
        assert!(update.get("id").is_none());

        // Then: the prompt result.
        let result = read_msg(&mut reader).await;
        assert_eq!(result["id"], json!(3));
        assert_eq!(result["result"]["stopReason"], json!("end_turn"));
    }

    #[tokio::test]
    async fn cancel_stops_in_flight_prompt() {
        let (mut reader, mut writer) = start(Mode::WaitForCancel);
        write_msg(
            &mut writer,
            json!({ "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
                    "params": {
                        "sessionId": "sess-1",
                        "prompt": [ { "type": "text", "text": "work" } ]
                    } }),
        )
        .await;
        write_msg(
            &mut writer,
            json!({ "jsonrpc": "2.0", "method": "session/cancel",
                    "params": { "sessionId": "sess-1" } }),
        )
        .await;

        let result = read_msg(&mut reader).await;
        assert_eq!(result["id"], json!(4));
        assert_eq!(result["result"]["stopReason"], json!("cancelled"));
    }
}
