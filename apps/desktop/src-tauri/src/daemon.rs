//! The desktop client's connection to `codypendentd`.
//!
//! This is the whole reason the shell has a Rust side (adoption 14 §4.1): a
//! webview cannot open a Unix domain socket, so the connection lives here and
//! the webview only ever sees JSON the shared protocol crate produced. The
//! framing, handshake, and command envelopes are NOT reimplemented — they come
//! from `codypendent_council::connection::Connection`, the same reference
//! client `codypendent run` and the TUI use.
//!
//! Everything in this module is deliberately free of `tauri` types: the sink
//! events are pushed into is a trait, so the transport is exercised in tests
//! against a real Unix socket without a webview (see the tests at the bottom).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
#[cfg(test)]
use base64::Engine as _;
use codypendent_council::connection::Connection;
use codypendent_protocol::{
    read_envelope, write_envelope, AgentMode, AnalyticsExportRequest, AnalyticsExportResult,
    AnalyticsPage, AnalyticsQuery, ApprovalDecision, ApprovalId, ApprovalScope, ArtifactRef,
    Catchup, ClientId, ClientRole, CommandBody, Envelope, InboxEntry, InboxListQuery,
    InboxMutation, InboxPage, MessageId, ModelId, Payload, RunId, SessionEvent, SessionId,
    Subscription, WorkspaceId,
};
// Session-library, workflow and blackboard contracts. Deliberately a second
// `use` block rather than an edit to the one above: this module is worked on by
// several people at once and an additive block cannot conflict with theirs.
use codypendent_protocol::{
    board_scope_id, BlackboardItemDraft, BlackboardItemView, BlackboardScope, PageCursor,
    SessionLifecycleAction, SessionSearchFilters, SessionSearchPage, SessionSearchQuery,
    SessionSummary, WorkflowCatalog, WorkflowRunSnapshot,
};
// Run lifecycle (`PauseRun`/`ResumeRun`) and the pending-prompt queue
// (`QueuePrompt` and friends). A fourth additive `use` block, for the same
// reason as the one above.
use codypendent_protocol::{PromptDelivery, PromptId};
// Parked questions (`ResolveQuestion`, adoption 03). A fifth additive block.
use codypendent_protocol::{QuestionId, QuestionOutcome};
// The knowledge surfaces' daemon-owned half (memories, documents, Remote UI
// plugins). Additive block, for the reason given above.
use codypendent_protocol::{
    DocumentEditLease, DocumentId, DocumentLeaseGrant, DocumentMutation, MemoryId, MemoryView,
    PublishTarget, UiPluginLifecycleStatus,
};

use serde::Serialize;
#[cfg(test)]
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{oneshot, Mutex};

#[path = "daemon_knowledge.rs"]
mod knowledge_context;

#[async_trait::async_trait]
impl codypendent_council::artifacts::ArtifactSource for &DaemonClient {
    async fn read_chunk(&mut self, command: CommandBody) -> anyhow::Result<Payload> {
        Ok(self.send_command(command).await?.payload)
    }
}

/// How long a command waits for its correlated reply before the client gives
/// up. A daemon that has stopped answering is a disconnect, and the UI must be
/// told that rather than spinning forever on a promise that never settles.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// What `PublishDocument` parked for approval: the daemon's deterministic
/// plan, shown before any write. Exactly the fields of
/// `Payload::DocumentPublishRequested`, with the approval id as a string.
#[derive(Debug, Clone, Serialize)]
pub struct DocumentPublishPlan {
    pub approval_id: String,
    pub target: String,
    pub changed_files: Vec<String>,
    pub git_action: String,
}

/// How long connecting and completing the handshake may take.
///
/// Neither had any bound. A peer that ACCEPTS the socket and then never answers
/// — a wedged daemon, a half-open socket, a stale socket file taken over by
/// something else — left this future pending forever. The UI sat at
/// "Connecting…", the reconnect effect only fires on "disconnected" so it never
/// ran, and the teardown waits for the attempt to settle so it could not close
/// it either. One hung connect disabled the app permanently, with no error
/// anywhere. Shorter than `COMMAND_TIMEOUT`: a local daemon answers a handshake
/// in milliseconds, and failing fast here is what lets the reconnect backoff do
/// its job.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The client name/version this shell announces in its `ClientHello`.
const CLIENT_NAME: &str = "codypendent-desktop";

/// Hard ceiling on the bytes one `read_artifact` call will assemble, applied
/// REGARDLESS of the daemon-declared `byte_length`. The declared length is the
/// daemon's word, and the loop already bails when the bytes run past it — but
/// a reference lying about its length (declaring a huge one) would otherwise
/// have the shell buffer it all before the digest check could refuse it.
/// 64 MiB matches the daemon's own content caps (`MAX_ACP_PATCH_BYTES` and
/// friends); each crate carries this constant locally.
const ARTIFACT_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// What the webview learns about a connection that actually completed its
/// handshake. Every field here is something the daemon said — there is no
/// "connected" state the shell can invent on its own.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionInfo {
    pub socket_path: String,
    pub protocol_version: String,
    pub daemon_version: String,
    /// This shell's own version, so the UI can show the two side by side and
    /// say when they differ — a daemon left running across an upgrade is the
    /// usual way for them to.
    pub client_version: String,
    pub daemon_instance: String,
    pub build_id: String,
}

/// A frame pushed to the webview. Session events are forwarded verbatim (the
/// protocol crate serializes them, tagged with its own event names), so the
/// TypeScript side reads daemon state rather than a shell-invented projection.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonFrame {
    RemoteUi {
        session_id: Option<SessionId>,
        message: Box<codypendent_protocol::UiWireMessage>,
    },
    /// One durable session event, live or replayed from attach catch-up.
    Event {
        session_id: Option<SessionId>,
        event: Box<SessionEvent>,
    },
    /// The daemon answered an attach with a snapshot instead of an event
    /// replay (the client was too far behind). Forwarded as-is so the webview
    /// can render pending approvals rather than silently dropping them.
    Catchup {
        session_id: SessionId,
        snapshot: Box<Catchup>,
    },
    /// Complete durable history through a compact snapshot's stable watermark.
    /// Live events may arrive before this frame; webview projections merge by
    /// sequence so the eventual transcript is ordered and duplicate-free.
    History {
        session_id: SessionId,
        through: u64,
        events: Vec<SessionEvent>,
    },
    /// One live node transition or run-phase change on a workflow run this
    /// client subscribed to (`Subscription::Workflow`).
    ///
    /// Not session-scoped: the event carries its own `workflow_run_id`, so the
    /// webview routes it to the right run without consulting the frame. Each
    /// `NodeTransitioned` is full-state, so a merge by `node_id` is an
    /// idempotent overwrite and no watermark is needed — but the live event
    /// omits `depends_on`, so a merge must PRESERVE the edges the snapshot
    /// taught it rather than blanking the graph.
    ///
    /// Before this variant existed `dispatch` dropped the payload silently and
    /// the workflow panel could only poll.
    WorkflowEvent {
        event: Box<codypendent_protocol::WorkflowEvent>,
    },
    /// One blackboard artifact that just landed on a board this client
    /// subscribed to (`Subscription::Blackboard`) — a workflow run's board or a
    /// repository task board. Also not session-scoped; the item carries the
    /// `workflow_run_id` (the synthetic `board:<repo>` id for a task board).
    /// A superseding revision arrives as its own delivery, so the webview
    /// merges by id and drops the row the new item supersedes.
    BlackboardPosted { item: Box<BlackboardItemView> },
    /// The socket closed or failed. The UI must fall back to a disconnected
    /// state on this frame; it is the only honest thing to show afterwards.
    Disconnected { reason: String },
}

/// One page of session-library search results, carrying **the query it
/// answers**.
///
/// `Payload::SessionSearchResults` echoes back only the page. Two searches can
/// be in flight at once (an operator types faster than the daemon ranks), and
/// without the query travelling back with its page the slower answer to an
/// abandoned query lands under the heading of the query since typed. The
/// webview compares `query` to what is in the box and discards a mismatch —
/// the same correlation `crates/cli/src/tui.rs::pending_session_searches` does
/// for the TUI.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSearchAnswer {
    /// The exact query string this page answers.
    pub query: String,
    /// The cursor this page continues from — `None` for a first page. A caller
    /// that asked for page 2 and gets `cursor: null` back is looking at a
    /// restart, not a continuation.
    pub cursor: Option<PageCursor>,
    /// The daemon's ranked page. `next_cursor` present means the result set was
    /// **cut**: there is more beyond this page and it must not read as the
    /// whole set.
    pub page: SessionSearchPage,
}

/// What a `MutateSessionLifecycle` actually did, as the daemon reported it.
///
/// One command, three possible replies, and they are not interchangeable: a
/// rename returns the re-projected session, a delete returns a retention
/// receipt, an export returns an artifact. Collapsing them into "ok" would lose
/// the one fact a delete must show — whether the daemon tombstoned or purged,
/// which is *its* retention policy to decide and not the client's to guess.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SessionLifecycleOutcome {
    /// Rename / pin / unpin / archive / restore: the daemon's authoritative
    /// projection of the session after the mutation. The UI re-renders from
    /// this rather than toggling a local flag.
    Applied { session: Box<SessionSummary> },
    /// The session was deleted. `tombstoned` is the daemon's retention
    /// decision, reported verbatim — the client neither chooses nor predicts
    /// it.
    Deleted {
        session_id: SessionId,
        tombstoned: bool,
    },
    /// The export's bytes live in an artifact; `read_artifact` fetches them.
    Exported { artifact: Box<ArtifactRef> },
}

/// The authoritative baselines a workflow watch establishes: the run snapshot a
/// live `WorkflowEvent` stream folds onto, and the run's blackboard.
///
/// Both are read AFTER the subscription is in place (persist-before-publish on
/// the daemon side means a snapshot read after subscribing already reflects, or
/// is superseded by, every buffered live event), so nothing falls between the
/// two.
#[derive(Debug, Clone, Serialize)]
pub struct WorkflowWatch {
    pub snapshot: WorkflowRunSnapshot,
    /// The run's board, superseded revisions included — the Blackboard panel
    /// shows history, unlike the task board.
    pub blackboard: Vec<BlackboardItemView>,
}

/// The repository task board as the daemon holds it, plus the anchoring the
/// client resolved to ask for it.
///
/// `repository` is echoed back deliberately: an empty board is a legitimate
/// answer, and the operator's first question about one is "which checkout did
/// you even look at?". Answering it in the panel is what makes a
/// wrongly-anchored board visible instead of silent.
#[derive(Debug, Clone, Serialize)]
pub struct BoardView {
    /// The git-toplevel path the board is keyed by.
    pub repository: String,
    /// `board:<repository>` — the synthetic run id the subscription uses.
    pub board_scope_id: String,
    /// The live (non-superseded) `task` cards.
    pub cards: Vec<BlackboardItemView>,
}

/// Where forwarded frames go. Implemented by the Tauri channel in `bridge`,
/// and by a plain `Vec` collector in tests.
pub trait FrameSink: Send + Sync + 'static {
    fn emit(&self, frame: DaemonFrame);
}

struct BoardFrameSink<S: FrameSink> {
    sink: Arc<S>,
    scope_id: String,
}

impl<S: FrameSink> FrameSink for BoardFrameSink<S> {
    fn emit(&self, frame: DaemonFrame) {
        if matches!(&frame, DaemonFrame::BlackboardPosted { item } if item.workflow_run_id == self.scope_id)
        {
            self.sink.emit(frame);
        }
    }
}

/// The run a submitted objective actually created, as reported by the daemon.
#[derive(Debug, Clone, Serialize)]
pub struct RunHandle {
    pub session_id: SessionId,
    /// `None` when the daemon accepted `StartRun` without naming the run in
    /// its reply (older daemons); the `RunStarted` event still carries it.
    pub run_id: Option<RunId>,
}

/// One session row, as the daemon lists it.
#[derive(Debug, Clone, Serialize)]
pub struct SessionRow {
    pub session_id: SessionId,
    pub title: String,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
}

/// The socket the daemon is expected on, resolved exactly the way every other
/// client resolves it (`CODYPENDENT_SOCKET`, then the platform data dir).
pub fn socket_path() -> anyhow::Result<PathBuf> {
    Ok(crate::runtime_paths::resolve()
        .context("resolving the codypendentd socket path")?
        .socket_path)
}

/// A live, handshaken connection to the daemon.
///
/// The socket is split: a reader task owns the read half and either completes
/// an in-flight command or forwards the envelope to the webview; commands take
/// the writer under a mutex. That split is what lets a streaming run push
/// events at the UI while the UI is still awaiting a command reply.
///
/// Dropping the client is NOT a close: the reader task holds its own `Arc` of
/// the writer for heartbeat pongs and would outlive the drop, leaking the
/// connection. [`DaemonClient::shutdown`] is the only real teardown.
pub struct DaemonClient {
    socket_path: PathBuf,
    board_client: Mutex<Option<Arc<DaemonClient>>>,
    writer: Arc<Mutex<OwnedWriteHalf>>,
    client_id: ClientId,
    selected_protocol: codypendent_protocol::ProtocolVersion,
    inflight: Arc<Mutex<HashMap<MessageId, oneshot::Sender<Envelope>>>>,
    workspace: WorkspaceId,
    repository: Option<String>,
    /// The checkout `repository` belongs to, resolved once at connect through
    /// [`crate::repo_anchor`]. The task board is keyed by this path, never by
    /// the launch directory — see that module for what happens otherwise.
    /// `None` when the shell was started with no repository at all.
    board_repository: Option<String>,
    /// The session this connection is currently attached to, if any.
    ///
    /// A subscription is a property of an ATTACHMENT, not of a connection: the
    /// only way to add one is to re-send `AttachSession` with the grown set
    /// (`crates/cli/src/tui.rs` does exactly this). So the client has to
    /// remember which session it is attached to, or a watch has nothing to
    /// re-attach to and would fail closed on every call.
    attached: Mutex<Option<SessionId>>,
    knowledge_session: Mutex<Option<SessionId>>,
    /// The subscription set this connection last attached with. Grown, never
    /// replaced: re-attaching with a smaller set silently cancels the streams
    /// another open panel is relying on.
    subscriptions: Mutex<Vec<Subscription>>,
    /// The highest session-event sequence this client has observed, so a
    /// re-attach asks for the events it MISSED rather than replaying the whole
    /// session into a transcript that already has it.
    ///
    /// Per-SESSION state held per CONNECTION: `attach` resets it when the
    /// attached session changes (sequences of two sessions are unrelated, and
    /// a stale watermark would make the next `grow_subscriptions` re-attach
    /// silently skip catch-up events), and keeps it on a same-session
    /// re-attach.
    last_seen: Arc<AtomicU64>,
    /// Serializes attach operations: `attach` and the `grow_subscriptions`
    /// re-attach both read-modify-send `attached`/`subscriptions`, and without
    /// a single guard a re-attach for the OLD session can land after an attach
    /// to the new one, or observe the subscription set mid-reset.
    attach_lock: Mutex<()>,
    /// The reader task's handle, so [`DaemonClient::shutdown`] can stop it
    /// deterministically instead of waiting for the daemon to close the
    /// socket — which it never does while its pings are answered.
    reader_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// The subscriptions every attachment starts with. A watch grows this set; it
/// never shrinks below it.
fn default_subscriptions() -> Vec<Subscription> {
    vec![Subscription::SessionSummary, Subscription::AgentActivity]
}

impl DaemonClient {
    /// Connect to `socket` and complete the protocol handshake. An error here
    /// is the normal case on a machine with no daemon running, and the caller
    /// is expected to surface it to the operator verbatim.
    pub async fn connect<S: FrameSink>(
        socket: &Path,
        repository: Option<String>,
        sink: Arc<S>,
    ) -> anyhow::Result<(Arc<Self>, ConnectionInfo)> {
        Self::connect_as(socket, repository, None, sink).await
    }

    /// [`Self::connect`] with the workspace named.
    ///
    /// A workspace is an IDENTITY, not a property of a socket: minting one per
    /// connection meant every automatic reconnect adopted a new one while the
    /// app re-attached the same session, and every workspace-scoped memory and
    /// document disappeared from view until a matching scope came back. The
    /// shell passes its persisted id; `None` mints a fresh one, which is what
    /// a test or a one-shot connection wants.
    pub async fn connect_as<S: FrameSink>(
        socket: &Path,
        repository: Option<String>,
        workspace: Option<WorkspaceId>,
        sink: Arc<S>,
    ) -> anyhow::Result<(Arc<Self>, ConnectionInfo)> {
        // Bounded as ONE step: a socket that accepts and then goes silent must
        // fail the whole attempt, not just the half that had a bound.
        let (connection, hello) = tokio::time::timeout(CONNECT_TIMEOUT, async {
            let mut connection = Connection::connect(socket).await?;
            let hello = connection
                .handshake(CLIENT_NAME, env!("CARGO_PKG_VERSION"), None)
                .await?;
            Ok::<_, anyhow::Error>((connection, hello))
        })
        .await
        .map_err(|_| {
            anyhow!(
                "connecting to the daemon timed out after {}s — the socket accepted but the \
                 handshake never completed",
                CONNECT_TIMEOUT.as_secs()
            )
        })??;

        let info = ConnectionInfo {
            socket_path: socket.display().to_string(),
            protocol_version: hello.selected_protocol.to_string(),
            daemon_version: hello.daemon_version.clone(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            daemon_instance: hello.daemon_instance.to_string(),
            build_id: hello.build_id.clone(),
        };

        let (reader, writer, buffered, client_id) = connection.into_split();
        let writer = Arc::new(Mutex::new(writer));
        let inflight = Arc::new(Mutex::new(HashMap::new()));
        let last_seen = Arc::new(AtomicU64::new(0));

        // Resolved once, here, because it shells out to `git` and the answer
        // cannot change for the life of a connection (the repository is fixed
        // at connect). Off the reactor thread so a slow filesystem cannot stall
        // the runtime during the handshake.
        let board_repository = match repository.clone() {
            Some(directory) => tokio::task::spawn_blocking(move || {
                crate::repo_anchor::anchor_repository_path(Path::new(&directory))
                    .to_string_lossy()
                    .into_owned()
            })
            .await
            .ok(),
            None => None,
        };

        let client = Arc::new(Self {
            socket_path: socket.to_path_buf(),
            board_client: Mutex::new(None),
            writer: Arc::clone(&writer),
            client_id,
            selected_protocol: hello.selected_protocol,
            inflight: Arc::clone(&inflight),
            workspace: workspace.unwrap_or_default(),
            repository,
            board_repository,
            attached: Mutex::new(None),
            knowledge_session: Mutex::new(None),
            subscriptions: Mutex::new(default_subscriptions()),
            last_seen: Arc::clone(&last_seen),
            attach_lock: Mutex::new(()),
            reader_task: Mutex::new(None),
        });

        // The handle is stored so `shutdown` can abort the task: the reader
        // holds its own writer Arc for heartbeat pongs, so it would otherwise
        // block in `read_envelope` forever after the client is dropped.
        let reader_task = tokio::spawn(read_loop(
            reader,
            ReadLoop {
                buffered,
                writer,
                client_id,
                inflight,
                sink,
                last_seen,
                heartbeat_interval_ms: hello.heartbeat_interval_ms,
            },
        ));
        *client.reader_task.lock().await = Some(reader_task);

        Ok((client, info))
    }

    /// Hold the attachment identity through the write: a delayed old view must
    /// never send an interaction to the newly selected session.
    pub async fn send_remote_ui(
        &self,
        session_id: SessionId,
        message: codypendent_protocol::UiWireMessage,
    ) -> anyhow::Result<()> {
        let _hold = self.attach_lock.lock().await;
        if *self.attached.lock().await != Some(session_id) {
            bail!("The extension's session is no longer attached");
        }
        if !matches!(
            message.kind.as_str(),
            "capabilities"
                | "event"
                | "viewport"
                | "resync"
                | "subscription"
                | "unsubscribe"
                | "action"
                | "cancelAction"
        ) {
            bail!("This Remote UI message is not a renderer request");
        }
        message.validate(&codypendent_protocol::UiHardLimits::default())?;
        let mut envelope = Envelope::request(
            self.client_id,
            Payload::RemoteUi {
                message: Box::new(message),
            },
        );
        envelope.session_id = Some(session_id);
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            let mut writer = self.writer.lock().await;
            write_envelope(&mut *writer, &envelope).await
        })
        .await
        .context("Timed out sending extension interaction")??;
        Ok(())
    }

    pub async fn probe_models(
        &self,
        model: Option<ModelId>,
        network: bool,
    ) -> anyhow::Result<Vec<codypendent_protocol::ModelProbe>> {
        codypendent_council::connection::require_model_probe(Some(self.selected_protocol))?;
        codypendent_council::connection::model_probe_reply(
            self.send_command(CommandBody::ProbeModel { model, network })
                .await?,
        )
    }

    /// Close this connection for real: abort the reader task, half-close the
    /// socket so the daemon sees EOF, and fail every in-flight command.
    ///
    /// Dropping the client's `Arc` alone is NOT a disconnect — the reader task
    /// holds its own `Arc` of the writer for heartbeat pongs and blocks in
    /// `read_envelope` until the daemon closes the socket, which it never does
    /// while its pings are answered. Without this, every disconnect/reconnect
    /// leaks a live connection and task, and the stale reader keeps forwarding
    /// the old session's frames into the channel it was created with.
    pub async fn shutdown(&self) {
        if let Some(board) = self.board_client.lock().await.take() {
            if let Some(session_id) = *board.attached.lock().await {
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    board.accepted(CommandBody::CloseSession { session_id }, "CloseSession"),
                )
                .await;
            }
            board.shutdown_connection().await;
        }
        self.shutdown_connection().await;
    }

    async fn shutdown_connection(&self) {
        // Abort first, so nothing further is forwarded while the socket is
        // being torn down. An aborted reader emits no `Disconnected` frame: a
        // deliberate shutdown is not a failure the UI should report.
        if let Some(task) = self.reader_task.lock().await.take() {
            task.abort();
        }
        // Half-close the write side: the daemon's read returns EOF and it
        // drops the connection. The read half is owned by the aborted task
        // and dies with it.
        let _ = self.writer.lock().await.shutdown().await;
        // Fail any command still awaiting its reply the same way the reader's
        // exit path does: dropping the senders resolves each waiter to "the
        // daemon connection closed before replying".
        self.inflight.lock().await.clear();
    }

    /// Sessions the daemon knows about.
    pub async fn list_sessions(&self) -> anyhow::Result<Vec<SessionRow>> {
        let reply = self
            .send_command(CommandBody::ListSessions {
                workspace: None,
                limit: Some(50),
            })
            .await?;
        match reply.payload {
            Payload::SessionList { sessions, .. } => Ok(sessions
                .into_iter()
                .map(|session| SessionRow {
                    session_id: session.session_id,
                    title: session.title,
                    state: session.state,
                    created_at: session.created_at.to_rfc3339(),
                    updated_at: session.updated_at.to_rfc3339(),
                })
                .collect()),
            Payload::CommandRejected(error) => {
                bail!("ListSessions rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to ListSessions: {other:?}"),
        }
    }

    /// Continue the attached session, or create and attach a session when no
    /// `session_id` is supplied, then start a run for `objective`.
    /// New conversations use the same three commands `codypendent run` sends
    /// (`crates/cli/src/commands.rs::run_over_connection`). Attach catch-up is
    /// replayed into the sink so the transcript starts from daemon state.
    ///
    /// `model` pins the serving model for this run: it is the `StartRun.model`
    /// field, the same one `Intent::StartRun` carries from the TUI's
    /// `pending_model` (`crates/cli/src/tui.rs::intent_to_command`). `None`
    /// leaves the choice to the daemon rather than naming a default this client
    /// has no basis for.
    pub async fn start_objective<S: FrameSink>(
        &self,
        objective: String,
        mode: AgentMode,
        model: Option<ModelId>,
        session_id: Option<SessionId>,
        sink: &Arc<S>,
    ) -> anyhow::Result<RunHandle> {
        self.start_objective_prepared(objective, mode, model, session_id, sink, |_| Ok(()))
            .await
    }

    pub async fn start_objective_prepared<S: FrameSink>(
        &self,
        objective: String,
        mode: AgentMode,
        model: Option<ModelId>,
        session_id: Option<SessionId>,
        sink: &Arc<S>,
        prepare: impl FnOnce(SessionId) -> anyhow::Result<()>,
    ) -> anyhow::Result<RunHandle> {
        // A concurrent session selection must not move the connection between
        // validating the requested conversation and sending its continuation.
        let _continuation = if session_id.is_some() {
            Some(self.attach_lock.lock().await)
        } else {
            None
        };
        let session_id = if let Some(session_id) = session_id {
            if *self.attached.lock().await != Some(session_id) {
                bail!("cannot continue a session that is not attached; select it before sending");
            }
            session_id
        } else {
            let create_reply = self
                .send_command(CommandBody::CreateSession {
                    workspace: self.workspace,
                    title: objective.clone(),
                    repository: self.repository.clone(),
                    internal: false,
                    parent_session_id: None,
                    parent_run_id: None,
                })
                .await?;
            let session_id = match &create_reply.payload {
                Payload::CommandAccepted { .. } => create_reply.session_id.ok_or_else(|| {
                    anyhow!(
                        "daemon accepted CreateSession but its reply carried no session_id; \
                     the desktop client cannot bind to the session it just created"
                    )
                })?,
                Payload::CommandRejected(error) => {
                    bail!("CreateSession rejected: {} ({})", error.message, error.code)
                }
                other => bail!("unexpected reply to CreateSession: {other:?}"),
            };

            self.attach(session_id, sink).await?;
            session_id
        };

        prepare(session_id)?;
        let start_reply = self
            .send_command(CommandBody::StartRun {
                session_id,
                objective,
                mode,
                repository: self.repository.clone(),
                model,
            })
            .await?;
        let run_id = match start_reply.payload {
            Payload::CommandAccepted { created_run, .. } => created_run,
            Payload::CommandRejected(error) => {
                bail!("StartRun rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to StartRun: {other:?}"),
        };

        Ok(RunHandle { session_id, run_id })
    }

    /// Attach to an existing session and replay its catch-up into the sink.
    pub async fn attach<S: FrameSink>(
        &self,
        session_id: SessionId,
        sink: &Arc<S>,
    ) -> anyhow::Result<()> {
        // Serialized against `grow_subscriptions`' re-attach: an in-flight
        // re-attach for the OLD session must not land after this attach has
        // moved the connection to the new one, and the subscription reset
        // below must not be observed mid-flight.
        let _attach = self.attach_lock.lock().await;

        // `last_seen` is a per-session watermark held per connection (see the
        // field). Attaching a DIFFERENT session with the old session's
        // watermark would make a later `grow_subscriptions` re-attach report a
        // `last_seen_sequence` from the wrong session and silently skip
        // catch-up events — so the watermark resets when the session changes.
        // Re-attaching the SAME session keeps it.
        if *self.attached.lock().await != Some(session_id) {
            self.last_seen.store(0, Ordering::Relaxed);
        }
        // A fresh attachment starts from the default subscription set: the
        // watches a previous session's panels grew belong to that session, and
        // carrying them over would keep this connection subscribed to streams
        // nothing is showing.
        let subscriptions = {
            let mut held = self.subscriptions.lock().await;
            *held = default_subscriptions();
            held.clone()
        };
        let reply = self
            .send_command(CommandBody::AttachSession {
                session_id,
                last_seen_sequence: None,
                subscriptions,
                requested_role: ClientRole::Controller,
                repository: self.repository.clone(),
            })
            .await?;
        match reply.payload {
            Payload::Catchup { catchup } => {
                let snapshot_through = match &catchup {
                    Catchup::Snapshot { through, .. } => Some(*through),
                    _ => None,
                };
                // Recorded only on success: a refused attach must not leave the
                // client believing it may issue session-scoped commands.
                *self.attached.lock().await = Some(session_id);
                replay_catchup(session_id, catchup, sink, &self.last_seen);
                // The attachment is settled here, so the lock's work is done.
                // What follows is a paged read of the durable log — 500 events
                // per round trip, seconds of them for a long session — and
                // holding the lock across it blocked `grow_subscriptions`,
                // which every panel calls to open a watch. Attaching a long
                // session and then opening the workflow or blackboard panel
                // therefore hung the panel until the entire history had
                // downloaded, for no reason: this read touches neither
                // `attached` nor `subscriptions`.
                drop(_attach);
                if let Some(through) = snapshot_through {
                    // `replay_catchup` already advanced `last_seen` to the
                    // snapshot's watermark; this fills in the durable history
                    // behind it.
                    let events = self.read_session_events(session_id, 0, through).await?;
                    // A concurrent attach may have moved the connection to
                    // another session while this was reading. Emitting then
                    // would push one session's history into another's
                    // transcript, so the frames are dropped instead — the
                    // session that IS attached ran its own attach and has its
                    // own history.
                    if *self.attached.lock().await == Some(session_id) {
                        sink.emit(DaemonFrame::History {
                            session_id,
                            through,
                            events,
                        });
                    }
                }
                Ok(())
            }
            Payload::CommandRejected(error) => {
                bail!("AttachSession rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to AttachSession: {other:?}"),
        }
    }

    pub async fn cancel_run(&self, run_id: RunId) -> anyhow::Result<()> {
        let reply = self.send_command(CommandBody::CancelRun { run_id }).await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => {
                bail!("CancelRun rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to CancelRun: {other:?}"),
        }
    }

    /// Queue steering text against a live run — a real `QueueSteering`
    /// command, the same one the TUI's steering prompt sends
    /// (`crates/tui/src/reduce.rs`, `Overlay::Steering`).
    ///
    /// Steering redirects a run in flight; it does not start a new one and it
    /// does not stop the current one.
    ///
    /// Three facts are kept apart deliberately, because the daemon keeps them
    /// apart: this call resolving means the daemon ACCEPTED the command;
    /// `SteeringQueued` on the session stream means it was QUEUED; and
    /// `SteeringApplied` means the run actually took it. The desktop never
    /// infers the second or third from the first.
    ///
    /// Blank text is refused here rather than sent. `apply_queue_steering` in
    /// the daemon enqueues nothing for text that trims empty while still
    /// replying `CommandAccepted`, so sending it would buy an acceptance that
    /// can never become a `SteeringQueued`.
    pub async fn queue_steering(&self, run_id: RunId, text: String) -> anyhow::Result<()> {
        if text.trim().is_empty() {
            bail!("steering text cannot be empty");
        }
        let reply = self
            .send_command(CommandBody::QueueSteering { run_id, text })
            .await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => {
                bail!("QueueSteering rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to QueueSteering: {other:?}"),
        }
    }

    pub async fn resolve_approval(
        &self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        scope: ApprovalScope,
    ) -> anyhow::Result<()> {
        if matches!(scope, ApprovalScope::Unknown)
            || (decision != ApprovalDecision::Approve && scope != ApprovalScope::Once)
        {
            bail!("unsupported approval scope");
        }
        let reply = self
            .send_command(CommandBody::ResolveApproval {
                approval_id,
                decision,
                scope,
            })
            .await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => {
                bail!(
                    "ResolveApproval rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to ResolveApproval: {other:?}"),
        }
    }

    /// Resolve a parked question (adoption 03): the operator's answers, one
    /// list of chosen labels per question, or a rejection with optional
    /// feedback. Idempotent and revision-guarded on the daemon side, exactly
    /// like `ResolveApproval` — the desktop could raise an OS notification for
    /// a question but had no way to answer it, so the run stayed blocked until
    /// someone opened the TUI.
    pub async fn resolve_question(
        &self,
        question_id: QuestionId,
        outcome: QuestionOutcome,
    ) -> anyhow::Result<()> {
        let reply = self
            .send_command(CommandBody::ResolveQuestion {
                question_id,
                outcome,
            })
            .await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => {
                bail!(
                    "ResolveQuestion rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to ResolveQuestion: {other:?}"),
        }
    }

    /// One owner-scoped page of the durable inbox. The daemon scopes the query
    /// to the connection's principal; the shell neither filters nor invents
    /// entries, so an empty page here means the daemon returned no entries and
    /// an error means the inbox was never read.
    pub async fn list_inbox(&self, query: InboxListQuery) -> anyhow::Result<InboxPage> {
        let reply = self.send_command(CommandBody::ListInbox { query }).await?;
        match reply.payload {
            Payload::InboxPage { page, .. } => Ok(page),
            Payload::CommandRejected(error) => {
                bail!("ListInbox rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to ListInbox: {other:?}"),
        }
    }

    /// Acknowledge or dismiss one inbox entry. The reply is the daemon's
    /// authoritative projection of the entry after the mutation — the UI
    /// re-renders from that rather than toggling a local flag.
    pub async fn mutate_inbox(&self, mutation: InboxMutation) -> anyhow::Result<InboxEntry> {
        let reply = self
            .send_command(CommandBody::MutateInbox { mutation })
            .await?;
        match reply.payload {
            Payload::InboxEntryApplied { entry, .. } => Ok(entry),
            Payload::CommandRejected(error) => {
                bail!("MutateInbox rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to MutateInbox: {other:?}"),
        }
    }

    /// Measured execution observations and aggregates. Every metric in the page
    /// is the daemon's own measurement, including the ones it reports as absent
    /// — nothing here fills a missing token count, cost or latency with a zero.
    pub async fn query_analytics(&self, query: AnalyticsQuery) -> anyhow::Result<AnalyticsPage> {
        let reply = self
            .send_command(CommandBody::QueryAnalytics { query })
            .await?;
        match reply.payload {
            Payload::AnalyticsResults { page, .. } => Ok(page),
            Payload::CommandRejected(error) => {
                bail!(
                    "QueryAnalytics rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to QueryAnalytics: {other:?}"),
        }
    }

    /// Export a bounded analytics query. The reply is metadata plus the
    /// `ArtifactRef` for the JSON/CSV bytes; the bytes themselves are fetched
    /// with [`DaemonClient::read_artifact`], never inlined into the reply.
    pub async fn export_analytics(
        &self,
        request: AnalyticsExportRequest,
    ) -> anyhow::Result<AnalyticsExportResult> {
        let reply = self
            .send_command(CommandBody::ExportAnalytics { request })
            .await?;
        match reply.payload {
            Payload::AnalyticsExported { result, .. } => Ok(result),
            Payload::CommandRejected(error) => {
                bail!(
                    "ExportAnalytics rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to ExportAnalytics: {other:?}"),
        }
    }

    /// One artifact's whole content, paged through bounded `ReadArtifact`
    /// chunks and verified end to end.
    ///
    /// This mirrors the reference client (`sdk/protocol/src/client.ts`) rather
    /// than inventing a second retrieval style: every chunk request repeats the
    /// digest from the reference in hand, each reply is checked to be a chunk of
    /// *that* artifact at the offset asked for, and the assembled bytes must
    /// match the reference's length and SHA-256 before the shell hands them to
    /// the webview. A mismatch is an error, never a truncated read passed off as
    /// the artifact.
    pub async fn read_artifact(&self, artifact: &ArtifactRef) -> anyhow::Result<Vec<u8>> {
        let mut source = self;
        codypendent_council::artifacts::read_all(&mut source, artifact, ARTIFACT_MAX_TOTAL_BYTES)
            .await
    }

    /// Save without the preview's memory ceiling; only a bounded chunk is held
    /// in memory and an existing destination survives verification/write errors.
    pub async fn save_artifact(&self, artifact: &ArtifactRef, path: &Path) -> anyhow::Result<u64> {
        let mut source = self;
        codypendent_council::artifacts::save_to(&mut source, artifact, path).await
    }

    // ---------------------------------------------------------------- Session
    // Library. `ListSessions` above is the flat picker; these are the ranked,
    // paged, mutable surface.

    /// One page of ranked session search, tagged with the query it answers.
    ///
    /// `limit` is deliberately `0`: that asks the daemon for **its own** page
    /// size. A client does not get to widen the server's cap, and hard-coding a
    /// number here would silently diverge from the TUI the day the daemon
    /// changes it (`crates/cli/src/tui.rs` carries the same comment).
    ///
    /// A refusal comes back as an `Err`, which the Session Library renders as a
    /// failed search — never as "no results". They are different facts and the
    /// only one of them that is safe to act on is the empty page.
    pub async fn search_sessions(
        &self,
        query: String,
        cursor: Option<PageCursor>,
    ) -> anyhow::Result<SessionSearchAnswer> {
        let reply = self
            .send_command(CommandBody::SearchSessions {
                query: SessionSearchQuery {
                    query: query.clone(),
                    filters: SessionSearchFilters::default(),
                    limit: 0,
                    cursor: cursor.clone(),
                },
            })
            .await?;
        match reply.payload {
            Payload::SessionSearchResults { page, .. } => Ok(SessionSearchAnswer {
                query,
                cursor,
                page,
            }),
            Payload::CommandRejected(error) => {
                bail!(
                    "SearchSessions rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to SearchSessions: {other:?}"),
        }
    }

    /// Rename, pin, unpin, archive, restore, delete or export one session.
    ///
    /// The daemon answers a *different* payload per action and each is
    /// forwarded as itself — in particular a delete's `tombstoned` flag, which
    /// is the daemon's retention decision. The client neither predicts it nor
    /// reports "deleted" over the top of a tombstone.
    ///
    /// An unauthorized session and an absent one are the daemon's to
    /// distinguish, and it deliberately does not: it answers a generic
    /// not-found for both so the command cannot be used to enumerate other
    /// people's sessions. That refusal is forwarded **verbatim**; nothing here
    /// re-words it into something that would leak which case it was.
    pub async fn mutate_session(
        &self,
        session_id: SessionId,
        action: SessionLifecycleAction,
    ) -> anyhow::Result<SessionLifecycleOutcome> {
        let reply = self
            .send_command(CommandBody::MutateSessionLifecycle { session_id, action })
            .await?;
        match reply.payload {
            Payload::SessionLifecycleApplied { session, .. } => {
                Ok(SessionLifecycleOutcome::Applied {
                    session: Box::new(session),
                })
            }
            Payload::SessionDeleted {
                session_id,
                tombstoned,
                ..
            } => Ok(SessionLifecycleOutcome::Deleted {
                session_id,
                tombstoned,
            }),
            Payload::SessionExported { artifact, .. } => Ok(SessionLifecycleOutcome::Exported {
                artifact: Box::new(artifact),
            }),
            Payload::CommandRejected(error) => bail!("{} ({})", error.message, error.code),
            other => bail!("unexpected reply to MutateSessionLifecycle: {other:?}"),
        }
    }

    // --------------------------------------------------------------- Workflow

    /// Discover exactly the workflows resolved by this connection's repository.
    pub async fn list_workflows(
        &self,
        expected_repository: Option<String>,
    ) -> anyhow::Result<WorkflowCatalog> {
        if self.selected_protocol.minor < 12 {
            bail!("Workflow discovery requires agent runtime protocol 1.12 or newer. Update the runtime, or use a known workflow id.");
        }
        self.check_workflow_repository(expected_repository.as_deref())?;
        let repository = self
            .repository
            .clone()
            .ok_or_else(|| anyhow!("Choose a repository before browsing workflows."))?;
        let reply = self
            .send_command(CommandBody::ListWorkflows {
                repository: Some(repository.clone()),
            })
            .await?;
        match reply.payload {
            Payload::WorkflowCatalog { catalog, .. } if catalog.repository.as_deref() == Some(repository.as_str()) => Ok(catalog),
            Payload::WorkflowCatalog { .. } => bail!("The workflow catalogue belongs to another repository. Reconnect before starting work."),
            Payload::CommandRejected(error) => bail!("{} ({})", error.message, error.code),
            other => bail!("unexpected reply to ListWorkflows: {other:?}"),
        }
    }

    fn check_workflow_repository(&self, expected: Option<&str>) -> anyhow::Result<()> {
        if expected.is_some_and(|path| self.repository.as_deref() != Some(path)) {
            bail!("The selected repository changed. Reload workflows before starting work.");
        }
        Ok(())
    }

    /// Start a durable workflow run from a workflow the daemon resolves by id.
    ///
    /// `manifest` stays empty: the desktop does not ship YAML over the wire,
    /// it names a workflow the daemon already knows (the daemon then enforces
    /// its registry's version-stability and shadowing rules, which an inline
    /// manifest would bypass).
    ///
    /// `inputs` must be a JSON **object**. A valid JSON scalar or array is
    /// refused here rather than sent — the same refusal `crates/tui/src/reduce.rs`
    /// applies — because a manifest's typed inputs are named fields and a bare
    /// `3` is not a mistake the daemon should have to describe.
    pub async fn start_workflow(
        &self,
        workflow_id: String,
        inputs: serde_json::Value,
    ) -> anyhow::Result<String> {
        self.start_workflow_with_options(workflow_id, inputs, None, None, None)
            .await
    }

    /// Pin the UI's repository and retain its launch identity across an explicit retry.
    pub async fn start_workflow_with_options(
        &self,
        workflow_id: String,
        inputs: serde_json::Value,
        expected_repository: Option<String>,
        idempotency_key: Option<String>,
        expected_definition_digest: Option<String>,
    ) -> anyhow::Result<String> {
        self.check_workflow_repository(expected_repository.as_deref())?;
        if expected_definition_digest.is_some() && self.selected_protocol.minor < 12 {
            bail!("Starting the selected definition requires agent runtime protocol 1.12 or newer. Update the runtime first.");
        }
        if expected_definition_digest.as_ref().is_some_and(|digest| {
            digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            bail!("Invalid workflow definition fingerprint.");
        }
        if !inputs.is_object() {
            bail!("workflow inputs must be a JSON object");
        }
        if serde_json::to_vec(&inputs)?.len() > 65_536 {
            bail!("Workflow inputs must fit within 64 KiB.");
        }
        if idempotency_key.as_ref().is_some_and(|key| {
            key.trim().is_empty() || key.len() > 256 || key.chars().any(char::is_control)
        }) {
            bail!("Invalid workflow launch identity.");
        }
        let reply = self
            .send_command_with_options(
                CommandBody::StartWorkflow {
                    manifest: String::new(),
                    workflow_id: Some(workflow_id),
                    inputs,
                    repository: self.repository.clone(),
                    expected_definition_digest,
                },
                COMMAND_TIMEOUT,
                idempotency_key,
            )
            .await?;
        match reply.payload {
            Payload::WorkflowRunStarted {
                workflow_run_id, ..
            } => Ok(workflow_run_id),
            Payload::CommandRejected(error) => {
                bail!("StartWorkflow rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to StartWorkflow: {other:?}"),
        }
    }

    /// A run's observability snapshot: its phase plus every node's full current
    /// view, in topological order, with the graph edges the live stream omits.
    pub async fn read_workflow_run(
        &self,
        workflow_run_id: String,
    ) -> anyhow::Result<WorkflowRunSnapshot> {
        let reply = self
            .send_command(CommandBody::ReadWorkflowRun { workflow_run_id })
            .await?;
        match reply.payload {
            Payload::WorkflowRunSnapshot { snapshot, .. } => Ok(snapshot),
            Payload::CommandRejected(error) => bail!(
                "ReadWorkflowRun rejected: {} ({})",
                error.message,
                error.code
            ),
            other => bail!("unexpected reply to ReadWorkflowRun: {other:?}"),
        }
    }

    /// Open (or re-open) a workflow run: grow this attachment's subscriptions to
    /// the run's node stream and its board, then read both authoritative
    /// baselines.
    ///
    /// The order matters and is the daemon's contract: subscribe first, read
    /// second. The daemon publishes each transition **after** persisting it, so
    /// a snapshot taken after the subscription is in place already reflects — or
    /// is superseded by — every event the stream has buffered. Reading first
    /// would leave a hole exactly the width of the round trip.
    ///
    /// A repeated watch skips the re-attach (the subscriptions are already
    /// there) but deliberately re-reads both baselines, so reopening the panel
    /// is fresh rather than showing whatever the last stream left behind.
    pub async fn watch_workflow<S: FrameSink>(
        &self,
        workflow_run_id: String,
        sink: &Arc<S>,
    ) -> anyhow::Result<WorkflowWatch> {
        self.grow_subscriptions(
            vec![
                Subscription::Workflow {
                    workflow_run_id: workflow_run_id.clone(),
                },
                Subscription::Blackboard {
                    workflow_run_id: workflow_run_id.clone(),
                },
            ],
            sink,
        )
        .await?;

        let snapshot = self.read_workflow_run(workflow_run_id.clone()).await?;
        // The run panel shows supersession history, so unlike the task board it
        // asks for superseded revisions too.
        let blackboard = self
            .read_blackboard(workflow_run_id, None, true, None)
            .await?;
        Ok(WorkflowWatch {
            snapshot,
            blackboard,
        })
    }

    /// Pause a running workflow. Cooperative: the driver stops launching further
    /// nodes and the in-flight wave finishes.
    pub async fn pause_workflow(&self, workflow_run_id: String) -> anyhow::Result<()> {
        self.accepted(
            CommandBody::PauseWorkflow { workflow_run_id },
            "PauseWorkflow",
        )
        .await
    }

    /// Resume a paused workflow from its ready frontier.
    pub async fn resume_workflow(&self, workflow_run_id: String) -> anyhow::Result<()> {
        self.accepted(
            CommandBody::ResumeWorkflow { workflow_run_id },
            "ResumeWorkflow",
        )
        .await
    }

    /// Cancel a workflow run. Terminal — there is no resume from `Cancelled`,
    /// which is why the UI confirms before calling this.
    pub async fn cancel_workflow(&self, workflow_run_id: String) -> anyhow::Result<()> {
        self.accepted(
            CommandBody::CancelWorkflow { workflow_run_id },
            "CancelWorkflow",
        )
        .await
    }

    /// Re-drive a run from one node. That node and everything transitively
    /// downstream of it reset to pending — the daemon decides the closure, the
    /// client does not compute it.
    pub async fn retry_workflow_node(
        &self,
        workflow_run_id: String,
        node_id: String,
    ) -> anyhow::Result<()> {
        self.accepted(
            CommandBody::RetryWorkflowNode {
                workflow_run_id,
                node_id,
            },
            "RetryWorkflowNode",
        )
        .await
    }

    // ------------------------------------------------------------- Blackboard

    /// A board's stored artifacts. Serves both boards: a workflow run's (by
    /// `workflow_run_id`) and a repository task board (by `board_repository`,
    /// which makes the daemon ignore `workflow_run_id`).
    pub async fn read_blackboard(
        &self,
        workflow_run_id: String,
        kind: Option<String>,
        include_superseded: bool,
        board_repository: Option<String>,
    ) -> anyhow::Result<Vec<BlackboardItemView>> {
        let reply = self
            .send_command(CommandBody::ReadBlackboard {
                workflow_run_id,
                kind,
                include_superseded,
                board_repository,
            })
            .await?;
        match reply.payload {
            Payload::BlackboardItems { items, .. } => Ok(items),
            Payload::CommandRejected(error) => {
                bail!(
                    "ReadBlackboard rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to ReadBlackboard: {other:?}"),
        }
    }

    /// Post an **open question** to a run's blackboard.
    ///
    /// Deliberately the only thing an operator may post, and deliberately not a
    /// general `post_blackboard_item`: a question carries no unverified factual
    /// claim, whereas a human-authored `finding` or `decision` would enter the
    /// agents' only communication channel as evidence they cannot distinguish
    /// from their own. There is no desktop affordance for those kinds, and this
    /// signature is what keeps it that way.
    pub async fn post_blackboard_question(
        &self,
        workflow_run_id: String,
        text: String,
    ) -> anyhow::Result<BlackboardItemView> {
        let text = text.trim();
        if text.is_empty() {
            bail!("question must not be empty");
        }
        self.applied_item(CommandBody::PostBlackboardItem {
            scope: BlackboardScope::WorkflowRun { workflow_run_id },
            item: BlackboardItemDraft {
                kind: "open_question".to_owned(),
                payload: serde_json::json!({ "question": text }),
                confidence: None,
                evidence: Vec::new(),
                status: None,
                assignee: None,
                ordinal: None,
            },
        })
        .await
    }

    // ------------------------------------------------------- Repository board

    /// Open (or re-open) the repository task board: subscribe to its channel and
    /// read its live cards.
    ///
    /// The board rides the ordinary per-run blackboard machinery — its channel
    /// key is the synthetic `board:<repository>` run id — so nothing new is on
    /// the wire beyond a board-scoped read. Subscribe before reading, for the
    /// same reason [`watch_workflow`](Self::watch_workflow) does.
    pub async fn watch_board<S: FrameSink>(&self, sink: &Arc<S>) -> anyhow::Result<BoardView> {
        let repository = self.board()?;
        let scope_id = board_scope_id(&repository);
        let board_sink = Arc::new(BoardFrameSink {
            sink: Arc::clone(sink),
            scope_id: scope_id.clone(),
        });
        let client = self.repository_board_client(&board_sink).await?;
        client
            .grow_subscriptions(
                vec![Subscription::Blackboard {
                    workflow_run_id: scope_id.clone(),
                }],
                &board_sink,
            )
            .await?;
        let cards = client
            .read_blackboard(
                String::new(),
                Some("task".to_owned()),
                false,
                Some(repository.clone()),
            )
            .await?;
        Ok(BoardView {
            repository,
            board_scope_id: scope_id,
            cards,
        })
    }

    /// Create one task card in the board's first column.
    pub async fn create_board_card(&self, title: String) -> anyhow::Result<BlackboardItemView> {
        let title = title.trim();
        if title.is_empty() {
            bail!("task title must not be empty");
        }
        let client = self
            .board_client
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("Open the task board before changing tasks."))?;
        client
            .applied_item(CommandBody::PostBlackboardItem {
                scope: BlackboardScope::RepositoryBoard {
                    repository: self.board()?,
                },
                item: BlackboardItemDraft {
                    kind: "task".to_owned(),
                    payload: serde_json::json!({ "title": title, "description": "" }),
                    confidence: None,
                    evidence: Vec::new(),
                    status: Some("todo".to_owned()),
                    assignee: None,
                    ordinal: None,
                },
            })
            .await
    }

    /// Move one card to another column.
    ///
    /// A supersession server-side: the daemon carries the card's body forward,
    /// re-ordinals it to the end of the target column and republishes it. Every
    /// other field is `None` so nothing the client did not touch is overwritten,
    /// and the pane never edits its own copy of the card — it renders the
    /// replacement the daemon returns.
    pub async fn move_board_card(
        &self,
        item_id: String,
        status: String,
    ) -> anyhow::Result<BlackboardItemView> {
        let client = self
            .board_client
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("Open the task board before changing tasks."))?;
        client
            .applied_item(CommandBody::UpdateBlackboardItem {
                scope: BlackboardScope::RepositoryBoard {
                    repository: self.board()?,
                },
                item_id,
                status: Some(status),
                assignee: None,
                ordinal: None,
                payload: None,
            })
            .await
    }

    /// The checkout the task board is keyed by, or an explanation of why there
    /// is no board — never a fallback to the launch directory, which would open
    /// a second, permanently empty board with no error (see
    /// [`crate::repo_anchor`]).
    fn board(&self) -> anyhow::Result<String> {
        self.board_repository
            .clone()
            .ok_or_else(|| anyhow!("Choose a repository to use its task board."))
    }

    async fn repository_board_client<S: FrameSink>(
        &self,
        sink: &Arc<S>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut held = self.board_client.lock().await;
        if self
            .reader_task
            .lock()
            .await
            .as_ref()
            .is_none_or(|task| task.is_finished())
        {
            bail!("Reconnect the workspace to load its task board.");
        }
        if let Some(client) = held.as_ref() {
            if client
                .reader_task
                .lock()
                .await
                .as_ref()
                .is_some_and(|task| !task.is_finished())
            {
                return Ok(Arc::clone(client));
            }
            client.shutdown_connection().await;
            *held = None;
        }
        let (client, _) = Self::connect_as(
            &self.socket_path,
            self.repository.clone(),
            Some(self.workspace),
            Arc::clone(sink),
        )
        .await?;
        let setup = async {
            let reply = client
                .send_command(CommandBody::CreateSession {
                    workspace: self.workspace,
                    title: "Repository task board".to_owned(),
                    repository: self.board_repository.clone(),
                    internal: true,
                    parent_session_id: None,
                    parent_run_id: None,
                })
                .await?;
            let session_id = match reply.payload {
                Payload::CommandAccepted { .. } => reply.session_id.ok_or_else(|| {
                    anyhow!("The task board connection has no workspace session.")
                })?,
                Payload::CommandRejected(error) => bail!(
                    "Could not open the task board: {} ({})",
                    error.message,
                    error.code
                ),
                other => bail!("Unexpected task board session reply: {other:?}"),
            };
            client.attach(session_id, sink).await
        }
        .await;
        if let Err(error) = setup {
            client.shutdown_connection().await;
            return Err(error);
        }
        *held = Some(Arc::clone(&client));
        Ok(client)
    }

    // ------------------------------------------------------------- Plumbing

    /// Add `wanted` to this attachment's subscription set, re-attaching when the
    /// set actually grew.
    ///
    /// A subscription belongs to an attachment, so the only way to add one is to
    /// re-send `AttachSession` with the whole (grown) set. Two consequences the
    /// implementation depends on:
    ///
    /// * The set is only ever grown. Re-attaching with a smaller set would
    ///   cancel the streams another open panel is relying on.
    /// * The re-attach carries `last_seen_sequence`, so the catch-up it returns
    ///   is the events this client MISSED. Those are replayed into the sink —
    ///   dropping them would lose real transcript, and asking from zero would
    ///   duplicate all of it.
    async fn grow_subscriptions<S: FrameSink>(
        &self,
        wanted: Vec<Subscription>,
        sink: &Arc<S>,
    ) -> anyhow::Result<()> {
        // Serialized against `attach`: the read of `attached`, the growth of
        // `subscriptions` and the re-attach below are one operation. Without
        // the guard, a concurrent attach to a NEW session could reset the
        // subscription set mid-flight, or this re-attach for the old session
        // could land after it.
        let _attach = self.attach_lock.lock().await;

        let Some(session_id) = *self.attached.lock().await else {
            bail!(
                "no session is attached, so this client cannot subscribe to a live stream — \
                 open or start a session first"
            );
        };

        let subscriptions = {
            let mut held = self.subscriptions.lock().await;
            let mut grew = false;
            for subscription in wanted {
                if !held.contains(&subscription) {
                    held.push(subscription);
                    grew = true;
                }
            }
            if !grew {
                return Ok(());
            }
            held.clone()
        };

        let reply = self
            .send_command(CommandBody::AttachSession {
                session_id,
                last_seen_sequence: Some(self.last_seen.load(Ordering::Relaxed)),
                subscriptions,
                requested_role: ClientRole::Controller,
                repository: self.repository.clone(),
            })
            .await?;
        match reply.payload {
            Payload::Catchup { catchup } => {
                replay_catchup(session_id, catchup, sink, &self.last_seen);
                Ok(())
            }
            Payload::CommandRejected(error) => bail!(
                "AttachSession rejected while subscribing: {} ({})",
                error.message,
                error.code
            ),
            other => bail!("unexpected reply to AttachSession: {other:?}"),
        }
    }

    /// Send a command whose only successful reply is an acknowledgement.
    async fn accepted(&self, body: CommandBody, name: &str) -> anyhow::Result<()> {
        let reply = self.send_command(body).await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => {
                bail!("{name} rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to {name}: {other:?}"),
        }
    }

    /// Send a blackboard write and return the stored (or superseding) item the
    /// daemon minted — the row the UI then renders, rather than the draft it
    /// sent.
    async fn applied_item(&self, body: CommandBody) -> anyhow::Result<BlackboardItemView> {
        let reply = self.send_command(body).await?;
        match reply.payload {
            Payload::BlackboardItemApplied { item, .. } => Ok(item),
            Payload::CommandRejected(error) => bail!("{} ({})", error.message, error.code),
            other => bail!("unexpected reply to a blackboard write: {other:?}"),
        }
    }

    /// Read the stable range `(after, target]` from the durable event log.
    ///
    /// Serves two callers: a compact catch-up snapshot, which asks from zero,
    /// and a live-stream gap, which asks from the last sequence the client
    /// actually saw. Commands share the live connection safely because the
    /// reader routes correlated replies while forwarding unrelated live events
    /// to the sink.
    pub async fn read_session_events(
        &self,
        session_id: SessionId,
        after: u64,
        target: u64,
    ) -> anyhow::Result<Vec<SessionEvent>> {
        let mut after = after;
        let mut history = Vec::new();
        while after < target {
            let limit = u32::try_from(target.saturating_sub(after).min(500)).unwrap_or(500);
            let reply = self
                .send_command(CommandBody::ReadSessionEvents {
                    session_id,
                    after_sequence: after,
                    limit,
                })
                .await?;
            let (events, through) = match reply.payload {
                Payload::SessionEventsPage {
                    session_id: returned,
                    events,
                    through,
                    ..
                } if returned == session_id => (events, through.min(target)),
                Payload::CommandRejected(error) => {
                    bail!(
                        "ReadSessionEvents rejected: {} ({})",
                        error.message,
                        error.code
                    )
                }
                other => bail!("unexpected reply to ReadSessionEvents: {other:?}"),
            };
            history.extend(events.into_iter().filter(|event| event.sequence <= target));
            if through <= after {
                bail!(
                    "session history stopped at event {after} before snapshot watermark {target}"
                );
            }
            after = through;
        }
        Ok(history)
    }

    // -----------------------------------------------------------------------
    // Knowledge: the daemon-owned half (memories, documents, Remote UI plugins).
    // -----------------------------------------------------------------------

    /// The workspace this connection carries: one of the scopes a local
    /// knowledge read may see, exactly as the TUI includes its own.
    pub fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub fn repository(&self) -> Option<&str> {
        self.repository.as_deref()
    }

    /// The confirmed attachment used when handing this session to another client.
    pub async fn attached_session_id(&self) -> Option<SessionId> {
        *self.attached.lock().await
    }

    pub async fn inspect_memory(&self, id: MemoryId) -> anyhow::Result<MemoryView> {
        let _selection = self.attach_lock.lock().await;
        let repository = self.knowledge_repository("memories").await?;
        self.inspect_memory_in(id, repository).await
    }

    async fn inspect_memory_in(
        &self,
        id: MemoryId,
        repository: String,
    ) -> anyhow::Result<MemoryView> {
        let reply = self
            .send_command(CommandBody::InspectMemory { id, repository })
            .await?;
        match reply.payload {
            Payload::Memory { memory, .. } => Ok(memory),
            Payload::CommandRejected(error) => {
                bail!("InspectMemory rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to InspectMemory: {other:?}"),
        }
    }

    /// Correct a memory's statement (Chapter 06's right to *edit*).
    ///
    /// The wire shape requires a confidence and carries the structured value,
    /// so the memory is read first and both are passed through unchanged: the
    /// operator corrected the words, not how sure the record is. The store
    /// supersedes rather than overwrites, so the history survives.
    pub async fn correct_memory(&self, id: MemoryId, statement: String) -> anyhow::Result<String> {
        let _selection = self.attach_lock.lock().await;
        let repository = self.knowledge_repository("memories").await?;
        let current = self.inspect_memory_in(id, repository.clone()).await?;
        let reply = self
            .send_command(CommandBody::CorrectMemory {
                id,
                repository,
                statement,
                structured_value: current.structured_value,
                confidence: current.confidence,
            })
            .await?;
        match reply.payload {
            Payload::Memory { .. } => {
                Ok("memory corrected; the earlier statement is kept as history".to_owned())
            }
            Payload::CommandRejected(error) => {
                bail!("CorrectMemory rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to CorrectMemory: {other:?}"),
        }
    }

    /// Forget one memory (Chapter 06's right to *delete*). The reply is a
    /// content-free audit; the notice repeats only how many rows went.
    pub async fn forget_memory(&self, id: MemoryId) -> anyhow::Result<String> {
        let _selection = self.attach_lock.lock().await;
        let repository = self.knowledge_repository("memories").await?;
        let reply = self
            .send_command(CommandBody::ForgetMemory { id, repository })
            .await?;
        match reply.payload {
            Payload::MemoryForgotten { forgotten, .. } => Ok(match forgotten.len() {
                1 => "memory forgotten".to_owned(),
                count => format!("memory forgotten ({count} records removed)"),
            }),
            Payload::CommandRejected(error) => {
                bail!("ForgetMemory rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to ForgetMemory: {other:?}"),
        }
    }

    pub async fn create_document(&self, title: String) -> anyhow::Result<DocumentId> {
        let _selection = self.attach_lock.lock().await;
        let repository = self.knowledge_repository("documents").await?;
        let reply = self
            .send_command(CommandBody::CreateDocument {
                title,
                scope: None,
                repository: Some(repository),
                initial_markdown: None,
            })
            .await?;
        match reply.payload {
            Payload::DocumentCreated { document_id, .. } => Ok(document_id),
            Payload::CommandRejected(error) => {
                bail!(
                    "CreateDocument rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to CreateDocument: {other:?}"),
        }
    }

    /// Lease one block (or, with `block_id` absent, the whole document
    /// structure) before editing it.
    pub async fn acquire_document_lease(
        &self,
        document_id: DocumentId,
        block_id: Option<String>,
    ) -> anyhow::Result<DocumentLeaseGrant> {
        let _selection = self.attach_lock.lock().await;
        self.ensure_selected_document(document_id).await?;
        let reply = self
            .send_command(CommandBody::AcquireDocumentLease {
                lease: DocumentEditLease {
                    document_id,
                    block_id,
                },
                ttl_seconds: None,
            })
            .await?;
        match reply.payload {
            Payload::DocumentLeaseGranted { grant, .. } => Ok(grant),
            Payload::CommandRejected(error) => {
                bail!(
                    "AcquireDocumentLease rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to AcquireDocumentLease: {other:?}"),
        }
    }

    /// Release a lease. Idempotent on the daemon, so a retry is safe.
    pub async fn release_document_lease(&self, lease_id: String) -> anyhow::Result<()> {
        let reply = self
            .send_command(CommandBody::ReleaseDocumentLease { lease_id })
            .await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => {
                bail!(
                    "ReleaseDocumentLease rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to ReleaseDocumentLease: {other:?}"),
        }
    }

    /// Apply one mutation under a lease the caller holds.
    pub async fn mutate_document(
        &self,
        document_id: DocumentId,
        mutation: DocumentMutation,
    ) -> anyhow::Result<()> {
        let _selection = self.attach_lock.lock().await;
        self.ensure_selected_document(document_id).await?;
        let reply = self
            .send_command(CommandBody::MutateDocument {
                document_id,
                mutation,
            })
            .await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => {
                bail!(
                    "MutateDocument rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to MutateDocument: {other:?}"),
        }
    }

    /// Ask the daemon to publish a document. Nothing is written yet: the
    /// reply is the parked plan, and a human approves it through the
    /// ordinary approval card.
    pub async fn publish_document(
        &self,
        document_id: DocumentId,
        target: PublishTarget,
    ) -> anyhow::Result<DocumentPublishPlan> {
        let _selection = self.attach_lock.lock().await;
        self.ensure_selected_document(document_id).await?;
        let reply = self
            .send_command(CommandBody::PublishDocument {
                document_id,
                target,
            })
            .await?;
        match reply.payload {
            Payload::DocumentPublishRequested {
                approval_id,
                target,
                changed_files,
                git_action,
                ..
            } => Ok(DocumentPublishPlan {
                approval_id: approval_id.to_string(),
                target,
                changed_files,
                git_action,
            }),
            Payload::CommandRejected(error) => {
                bail!(
                    "PublishDocument rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to PublishDocument: {other:?}"),
        }
    }

    /// The Remote UI plugin lifecycle commands all reply with the full
    /// lifecycle table, so the view re-renders from the daemon's answer
    /// rather than toggling a local flag.
    async fn ui_plugin_lifecycle(
        &self,
        name: &'static str,
        body: CommandBody,
    ) -> anyhow::Result<Vec<UiPluginLifecycleStatus>> {
        let reply = self.send_command(body).await?;
        match reply.payload {
            Payload::UiPluginLifecycle { plugins, .. } => Ok(plugins),
            Payload::CommandRejected(error) => {
                bail!("{name} rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to {name}: {other:?}"),
        }
    }

    pub async fn list_ui_plugins(&self) -> anyhow::Result<Vec<UiPluginLifecycleStatus>> {
        self.ui_plugin_lifecycle("ListUiPlugins", CommandBody::ListUiPlugins)
            .await
    }

    pub async fn smoke_test_ui_plugin(
        &self,
        plugin_id: String,
    ) -> anyhow::Result<Vec<UiPluginLifecycleStatus>> {
        self.ui_plugin_lifecycle(
            "SmokeTestUiPlugin",
            CommandBody::SmokeTestUiPlugin { plugin_id },
        )
        .await
    }

    /// Enable a plugin for the user, or for the attached session when the
    /// scope names one — the same binding the TUI makes.
    pub async fn enable_ui_plugin(
        &self,
        plugin_id: String,
        scope: String,
    ) -> anyhow::Result<Vec<UiPluginLifecycleStatus>> {
        let session_id = if scope == "user" {
            None
        } else {
            // A scoped enable BINDS the plugin to a session, and the daemon
            // refuses `SessionBindingRequired` when the scope names one and no
            // session is given. Opening Plugins before choosing a session is a
            // normal state and the UI defaults to `session`, so this refusal
            // was the default path: say what is missing instead of sending a
            // command that cannot succeed.
            let attached = *self.attached.lock().await;
            Some(attached.ok_or_else(|| {
                anyhow!(
                    "attach a session first: the `{scope}` scope binds this plugin to a \
                     session, and this connection carries none"
                )
            })?)
        };
        self.ui_plugin_lifecycle(
            "EnableUiPlugin",
            CommandBody::EnableUiPlugin {
                plugin_id,
                scope,
                session_id,
            },
        )
        .await
    }

    pub async fn approve_ui_plugin_update(
        &self,
        plugin_id: String,
        approval_receipt: String,
    ) -> anyhow::Result<Vec<UiPluginLifecycleStatus>> {
        self.ui_plugin_lifecycle(
            "ApproveUiPluginUpdate",
            CommandBody::ApproveUiPluginUpdate {
                plugin_id,
                approval_receipt,
            },
        )
        .await
    }

    pub async fn reject_ui_plugin_update(
        &self,
        plugin_id: String,
        approval_receipt: String,
    ) -> anyhow::Result<Vec<UiPluginLifecycleStatus>> {
        self.ui_plugin_lifecycle(
            "RejectUiPluginUpdate",
            CommandBody::RejectUiPluginUpdate {
                plugin_id,
                approval_receipt,
            },
        )
        .await
    }

    pub async fn revoke_ui_plugin(
        &self,
        plugin_id: String,
    ) -> anyhow::Result<Vec<UiPluginLifecycleStatus>> {
        self.ui_plugin_lifecycle("RevokeUiPlugin", CommandBody::RevokeUiPlugin { plugin_id })
            .await
    }

    /// Send one command and await its correlated reply.
    async fn send_command(&self, body: CommandBody) -> anyhow::Result<Envelope> {
        self.send_command_with_timeout(body, COMMAND_TIMEOUT).await
    }

    async fn send_command_with_timeout(
        &self,
        body: CommandBody,
        timeout: Duration,
    ) -> anyhow::Result<Envelope> {
        self.send_command_with_options(body, timeout, None).await
    }

    async fn send_command_with_options(
        &self,
        body: CommandBody,
        timeout: Duration,
        idempotency_key: Option<String>,
    ) -> anyhow::Result<Envelope> {
        let command_id = codypendent_protocol::CommandId::new();
        let command = codypendent_protocol::Command {
            command_id,
            idempotency_key: idempotency_key.unwrap_or_else(|| command_id.to_string()),
            expected_revision: None,
            body,
        };
        let envelope = Envelope::request(self.client_id, Payload::Command(command));
        let message_id = envelope.message_id;

        let (tx, rx) = oneshot::channel();
        self.inflight.lock().await.insert(message_id, tx);

        let write = {
            let mut writer = self.writer.lock().await;
            write_envelope(&mut *writer, &envelope).await
        };
        if let Err(error) = write {
            self.inflight.lock().await.remove(&message_id);
            return Err(anyhow::Error::from(error).context("writing a command to the daemon"));
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => {
                self.inflight.lock().await.remove(&message_id);
                bail!("the daemon connection closed before replying")
            }
            Err(_) => {
                self.inflight.lock().await.remove(&message_id);
                bail!(
                    "the daemon did not reply within {} seconds",
                    timeout.as_secs()
                )
            }
        }
    }
}

/// Replay an attach-time catch-up into the sink: event-by-event when the
/// daemon replayed, or the snapshot frame when it sent a projection instead.
///
/// Both arms advance `last_seen` to the watermark the catch-up establishes —
/// the replay per event, the snapshot to its `through` — so a later re-attach
/// asks only for what this client has not seen, whichever form the catch-up
/// took.
fn replay_catchup<S: FrameSink>(
    session_id: SessionId,
    catchup: Catchup,
    sink: &Arc<S>,
    last_seen: &AtomicU64,
) {
    match catchup {
        Catchup::Events { events, .. } => {
            for event in events {
                last_seen.fetch_max(event.sequence, Ordering::Relaxed);
                sink.emit(DaemonFrame::Event {
                    session_id: Some(session_id),
                    event: Box::new(event),
                });
            }
        }
        snapshot @ Catchup::Snapshot { through, .. } => {
            // A snapshot is the session compacted through `through`: sequences
            // at or below it are covered by the projection, so the watermark
            // advances even though no individual events were replayed.
            last_seen.fetch_max(through, Ordering::Relaxed);
            sink.emit(DaemonFrame::Catchup {
                session_id,
                snapshot: Box::new(snapshot),
            });
        }
        // A catch-up kind a newer daemon invented (RULE 1): nothing to replay,
        // and inventing transcript content for it would be exactly the lie
        // this module exists to avoid.
        _ => {}
    }
}

/// The single reader for the connection: complete in-flight commands, answer
/// heartbeats, forward everything else to the webview, and — when the socket
/// ends or goes silent — say so.
///
/// "Goes silent" is the heartbeat watchdog: the daemon negotiates
/// `heartbeat_interval_ms` in its `ServerHello` and pings on it, so a
/// connection on which NO frame (ping included) arrives for three intervals
/// is dead even if the OS has not noticed — a half-open socket otherwise
/// waits here forever. The watchdog surfaces through the same `Disconnected`
/// frame as EOF, so the webview falls back exactly as it does for a closed
/// socket.
struct ReadLoop<S: FrameSink> {
    buffered: VecDeque<Envelope>,
    writer: Arc<Mutex<OwnedWriteHalf>>,
    client_id: ClientId,
    inflight: Arc<Mutex<HashMap<MessageId, oneshot::Sender<Envelope>>>>,
    sink: Arc<S>,
    last_seen: Arc<AtomicU64>,
    heartbeat_interval_ms: u64,
}

async fn read_loop<S: FrameSink>(mut reader: OwnedReadHalf, context: ReadLoop<S>) {
    let ReadLoop {
        buffered,
        writer,
        client_id,
        inflight,
        sink,
        last_seen,
        heartbeat_interval_ms,
    } = context;
    // Envelopes the handshake buffered (live events that outraced a reply)
    // must be folded before the wire is read, or they are lost.
    for envelope in buffered {
        dispatch(envelope, &inflight, &sink, &last_seen).await;
    }

    // Three missed intervals, per the doc comment above. A daemon that
    // negotiated no heartbeat (0) gets no watchdog rather than an instant
    // timeout.
    let watchdog = heartbeat_interval_ms
        .checked_mul(3)
        .filter(|interval| *interval > 0)
        .map(Duration::from_millis);

    let reason = loop {
        let read = match watchdog {
            Some(limit) => match tokio::time::timeout(limit, read_envelope(&mut reader)).await {
                Ok(read) => read,
                Err(_) => {
                    break format!(
                        "the daemon sent nothing for {} seconds (three heartbeat intervals) \
                         — treating the connection as dead",
                        limit.as_secs()
                    );
                }
            },
            None => read_envelope(&mut reader).await,
        };
        match read {
            Ok(Some(envelope)) => {
                if matches!(envelope.payload, Payload::Ping) {
                    let pong = Envelope::request(client_id, Payload::Pong);
                    let mut writer = writer.lock().await;
                    if write_envelope(&mut *writer, &pong).await.is_err() {
                        break "the daemon connection dropped while answering a heartbeat"
                            .to_string();
                    }
                    continue;
                }
                dispatch(envelope, &inflight, &sink, &last_seen).await;
            }
            Ok(None) => break "the daemon closed the connection".to_string(),
            Err(error) => break format!("the daemon connection failed: {error}"),
        }
    };

    inflight.lock().await.clear();
    sink.emit(DaemonFrame::Disconnected { reason });
}

/// Route one envelope: correlated reply, forwarded event, or ignored.
async fn dispatch<S: FrameSink>(
    envelope: Envelope,
    inflight: &Arc<Mutex<HashMap<MessageId, oneshot::Sender<Envelope>>>>,
    sink: &Arc<S>,
    last_seen: &AtomicU64,
) {
    if let Some(correlation) = envelope.correlation_id {
        let waiter = inflight.lock().await.remove(&correlation);
        if let Some(waiter) = waiter {
            let _ = waiter.send(envelope);
            return;
        }
    }
    let session_id = envelope.session_id;
    match envelope.payload {
        Payload::RemoteUi { message } => sink.emit(DaemonFrame::RemoteUi {
            session_id,
            message,
        }),
        Payload::Event(event) => {
            last_seen.fetch_max(event.sequence, Ordering::Relaxed);
            sink.emit(DaemonFrame::Event {
                session_id,
                event: Box::new(event),
            });
        }
        // Uncorrelated, NOT session-scoped, and previously dropped on the
        // floor: without these two arms the workflow graph and both boards can
        // only poll, and a client that polls shows a run that finished a minute
        // ago as still running.
        Payload::WorkflowEvent { event } => sink.emit(DaemonFrame::WorkflowEvent {
            event: Box::new(event),
        }),
        Payload::BlackboardPosted(item) => sink.emit(DaemonFrame::BlackboardPosted {
            item: Box::new(item),
        }),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Code graph + checkpoint history (surfaces: EdgesView, BacktrackView)
//
// A fourth additive `impl` block rather than an edit to the one above, for the
// same reason the `use` blocks are additive: several people are adding handlers
// to this file at once.
//
// Both halves are real protocol commands read from `crates/protocol/src`:
// `ReadCodeGraphStatus` / `ReadCodeGraph` (replies `CodeGraphStatus` /
// `CodeGraphPage`) and `ForkSession` / `RestoreCheckpoint` (replies
// `SessionForked` / `CommandAccepted`). Nothing here synthesises graph rows or
// checkpoints — an absent graph and a session with no checkpoints are answers
// the daemon gives, not states this client invents.
// ---------------------------------------------------------------------------

use codypendent_protocol::{
    CheckpointId, CodeGraphPage, CodeGraphQuery, CodeGraphScanReport, CodeGraphStatusView,
};

impl DaemonClient {
    pub async fn build_code_graph(&self) -> anyhow::Result<CodeGraphScanReport> {
        let repository = self.graph_repository()?;
        let reply = self
            .send_command_with_timeout(
                CommandBody::BuildCodeGraph { repository },
                Duration::from_secs(300),
            )
            .await?;
        match reply.payload {
            Payload::CodeGraphBuilt { report, .. } => Ok(*report),
            Payload::CommandRejected(error) => bail!(
                "BuildCodeGraph rejected: {} ({})",
                error.message,
                error.code
            ),
            other => bail!("unexpected reply to BuildCodeGraph: {other:?}"),
        }
    }

    /// The checkout every code-graph command is scoped to.
    ///
    /// The daemon resolves a *path* to its enclosing checkout itself and derives
    /// the repository identity from that — a client cannot name a repository
    /// identity — so this hands it the anchored checkout resolved once at
    /// connect. Never the launch directory: that is how a code graph once
    /// reached 510,904 nodes indexing a home directory (see [`crate::repo_anchor`]).
    fn graph_repository(&self) -> anyhow::Result<String> {
        self.board_repository.clone().ok_or_else(|| {
            anyhow!(
                "the desktop shell was started without a repository, so there is no code graph \
                 to read — the graph is keyed by a checkout"
            )
        })
    }

    /// `ReadCodeGraphStatus` — what the STORED graph holds right now, with no
    /// re-scan: counts, per-language and per-kind breakdowns, the revisions it
    /// is stamped at, and whether it is stale against the working tree.
    pub async fn code_graph_status(&self) -> anyhow::Result<CodeGraphStatusView> {
        let repository = self.graph_repository()?;
        let reply = self
            .send_command(CommandBody::ReadCodeGraphStatus { repository })
            .await?;
        match reply.payload {
            Payload::CodeGraphStatus { status, .. } => Ok(*status),
            Payload::CommandRejected(error) => bail!(
                "ReadCodeGraphStatus rejected: {} ({})",
                error.message,
                error.code
            ),
            other => bail!("unexpected reply to ReadCodeGraphStatus: {other:?}"),
        }
    }

    /// `ReadCodeGraph` — one FILTERED, LIMITED page of nodes and edges.
    ///
    /// The limit is never dropped on the way through. A real graph runs to
    /// hundreds of thousands of nodes and over a million edges, and the daemon
    /// clamps any request to its own ceiling (`MAX_GRAPH_PAGE`, 500) precisely
    /// because the 16 MiB frame is a wall; `query.limit == 0` asks for that
    /// ceiling rather than for "everything". The reply carries `total_nodes` /
    /// `total_edges` **before** the limit and the `limit` actually applied, so
    /// the caller can say "showing N of M" instead of implying it showed the
    /// whole graph.
    pub async fn read_code_graph(&self, query: CodeGraphQuery) -> anyhow::Result<CodeGraphPage> {
        let repository = self.graph_repository()?;
        let reply = self
            .send_command(CommandBody::ReadCodeGraph { repository, query })
            .await?;
        match reply.payload {
            Payload::CodeGraphPage { page, .. } => Ok(*page),
            Payload::CommandRejected(error) => {
                bail!("ReadCodeGraph rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to ReadCodeGraph: {other:?}"),
        }
    }

    /// `ForkSession` — copy this session's ledger up to (excluding) the
    /// checkpointed run into a NEW session, and return the fork's id.
    ///
    /// The source session is never modified; the fork's runs carve their
    /// worktrees from the checkpointed filesystem state. The daemon enforces the
    /// cut rule itself: only an ordinal-1 (run-launch) checkpoint is forkable
    /// (`fork.mid-run-checkpoint`), and an absent or foreign checkpoint is
    /// refused `checkpoint.not-found` identically, so naming an id can never
    /// confirm it exists elsewhere. Those refusals travel back verbatim — the
    /// client does not restate them as a policy of its own.
    ///
    /// The session forked is the one this connection is ATTACHED to. There is no
    /// parameter for it, because a fork of some other session is not something
    /// the operator can see on screen to have consented to.
    pub async fn fork_session(
        &self,
        checkpoint: CheckpointId,
        name: Option<String>,
    ) -> anyhow::Result<SessionId> {
        let Some(session_id) = *self.attached.lock().await else {
            bail!("no session is attached, so there is nothing to fork — open a session first");
        };
        let name = name.and_then(|name| {
            let trimmed = name.trim().to_owned();
            (!trimmed.is_empty()).then_some(trimmed)
        });
        let reply = self
            .send_command(CommandBody::ForkSession {
                session_id,
                checkpoint,
                name,
            })
            .await?;
        match reply.payload {
            Payload::SessionForked { session_id, .. } => Ok(session_id),
            Payload::CommandRejected(error) => {
                bail!("{} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to ForkSession: {other:?}"),
        }
    }

    /// `RestoreCheckpoint` — rewind a settled run's worktree to a recorded
    /// checkpoint.
    ///
    /// `Ok(())` means only that the daemon ACCEPTED the request. It does not
    /// mean anything was restored: the daemon parks a
    /// `ProposedAction::RestoreCheckpoint` approval carrying its own
    /// `RiskLevel::High` reason and touches nothing until a human approves it,
    /// then journals `CheckpointRestored { restored }` either way. The caller
    /// must say "approval requested", never "restored", and the operator decides
    /// on the approval card where the daemon's own wording appears.
    ///
    /// Refusals are the daemon's: `checkpoint.run-active` while the run is not
    /// settled, `checkpoint.worktree-missing` when the recorded worktree is
    /// gone, `checkpoint.not-found`, `checkpoint.run-mismatch`.
    pub async fn restore_checkpoint(
        &self,
        run_id: RunId,
        checkpoint: CheckpointId,
    ) -> anyhow::Result<()> {
        let reply = self
            .send_command(CommandBody::RestoreCheckpoint { run_id, checkpoint })
            .await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => bail!("{} ({})", error.message, error.code),
            other => bail!("unexpected reply to RestoreCheckpoint: {other:?}"),
        }
    }

    // ----------------------------------------------------------- Run lifecycle
    //
    // `PauseRun` / `ResumeRun` — the NON-destructive siblings of `CancelRun`.
    // The TUI has had both since `Chip::new("p", "pause")`; this shell had
    // neither, so an operator who wanted to stop and think had to kill the run.
    //
    // Which transitions are legal is the daemon's decision, not this client's:
    // `validate_run_transition` in `crates/daemon/src/commands.rs` admits
    // `PauseRun` from any live, not-already-`Paused` state and `ResumeRun` ONLY
    // from `Paused`. A refusal comes back as `run.invalid-transition` and is
    // reported verbatim rather than restated as a rule of our own.

    /// Pause a live run. Real `PauseRun`; the daemon appends
    /// `RunStateChanged { Paused }` and the webview learns the new state from
    /// that event, never from this call resolving.
    pub async fn pause_run(&self, run_id: RunId) -> anyhow::Result<()> {
        let reply = self.send_command(CommandBody::PauseRun { run_id }).await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => {
                bail!("PauseRun rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to PauseRun: {other:?}"),
        }
    }

    /// Resume a paused run. Real `ResumeRun`. The daemon admits this only from
    /// `Paused`; sending it from any other state earns `run.invalid-transition`.
    pub async fn resume_run(&self, run_id: RunId) -> anyhow::Result<()> {
        let reply = self.send_command(CommandBody::ResumeRun { run_id }).await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => {
                bail!("ResumeRun rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to ResumeRun: {other:?}"),
        }
    }

    // --------------------------------------------------- Pending-prompt queue
    //
    // `QueuePrompt` / `UpdateQueuedPrompt` / `PromoteQueuedPrompt` /
    // `DeleteQueuedPrompt` (adoption 06). All four are SESSION-scoped and all
    // four target the session this connection is ATTACHED to — there is no
    // parameter for the session id, for the same reason `fork_session` has
    // none: a queue mutation on a session the operator cannot see on screen is
    // not something they consented to.
    //
    // None of these return the queue. The daemon appends a full
    // `PendingPromptsChanged` snapshot to the session's durable stream in the
    // same transaction, and that event — not this call — is what the webview
    // folds. Returning a queue from here would give the UI a second, racing
    // source of truth.

    /// The attached session, or the refusal a queue mutation gets without one.
    async fn attached_session(&self, what: &str) -> anyhow::Result<SessionId> {
        match *self.attached.lock().await {
            Some(session_id) => Ok(session_id),
            None => bail!(
                "no session is attached, so there is nothing to {what} — open a session first"
            ),
        }
    }

    /// Queue a prompt on the attached session's server-side pending queue.
    ///
    /// Blank text is refused here rather than sent: the daemon rejects it
    /// `prompt-queue.empty`, and a round trip to be told so is wasted.
    pub async fn queue_prompt(
        &self,
        text: String,
        mode: AgentMode,
        delivery: PromptDelivery,
    ) -> anyhow::Result<()> {
        if text.trim().is_empty() {
            bail!("a queued prompt cannot be empty");
        }
        let session_id = self.attached_session("queue a prompt on").await?;
        let reply = self
            .send_command(CommandBody::QueuePrompt {
                session_id,
                text,
                mode,
                delivery,
            })
            .await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => {
                bail!("QueuePrompt rejected: {} ({})", error.message, error.code)
            }
            other => bail!("unexpected reply to QueuePrompt: {other:?}"),
        }
    }

    /// Edit a queued prompt in place. Absent fields keep their current values;
    /// an emptied `text` is refused rather than sent (`prompt-queue.empty`).
    pub async fn update_queued_prompt(
        &self,
        prompt_id: PromptId,
        text: Option<String>,
        delivery: Option<PromptDelivery>,
    ) -> anyhow::Result<()> {
        if text.as_ref().is_some_and(|text| text.trim().is_empty()) {
            bail!("a queued prompt cannot be emptied — delete it instead");
        }
        let session_id = self.attached_session("edit a queued prompt on").await?;
        let reply = self
            .send_command(CommandBody::UpdateQueuedPrompt {
                session_id,
                prompt_id,
                text,
                delivery,
            })
            .await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => {
                bail!(
                    "UpdateQueuedPrompt rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to UpdateQueuedPrompt: {other:?}"),
        }
    }

    /// Promote a queued prompt to steer: its delivery becomes `Steer` and it
    /// moves to the front of the queue.
    pub async fn promote_queued_prompt(&self, prompt_id: PromptId) -> anyhow::Result<()> {
        let session_id = self.attached_session("promote a queued prompt on").await?;
        let reply = self
            .send_command(CommandBody::PromoteQueuedPrompt {
                session_id,
                prompt_id,
            })
            .await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => {
                bail!(
                    "PromoteQueuedPrompt rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to PromoteQueuedPrompt: {other:?}"),
        }
    }

    /// Remove a queued prompt without ever running it.
    pub async fn delete_queued_prompt(&self, prompt_id: PromptId) -> anyhow::Result<()> {
        let session_id = self.attached_session("remove a queued prompt from").await?;
        let reply = self
            .send_command(CommandBody::DeleteQueuedPrompt {
                session_id,
                prompt_id,
            })
            .await?;
        match reply.payload {
            Payload::CommandAccepted { .. } => Ok(()),
            Payload::CommandRejected(error) => {
                bail!(
                    "DeleteQueuedPrompt rejected: {} ({})",
                    error.message,
                    error.code
                )
            }
            other => bail!("unexpected reply to DeleteQueuedPrompt: {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex as StdMutex;

    use codypendent_protocol::{
        Actor, AnalyticsBucket, AnalyticsExportFormat, AnalyticsMetrics, ApprovalDecision,
        ApprovalId, ApprovalScope, ArtifactId, ClientCapabilities, CodypendentError,
        DaemonInstanceId, DataClassification, EventBody, InboxDeepLink, InboxEntryId,
        InboxEntryKind, InboxEntryState, InboxSource, InboxSourceIdentity, RepositoryId,
        ServerHello, SessionProjection, PROTOCOL_V1,
    };
    use tokio::net::UnixListener;

    #[derive(Default)]
    struct Collector {
        frames: StdMutex<Vec<DaemonFrame>>,
    }

    impl Collector {
        fn frames(&self) -> Vec<DaemonFrame> {
            self.frames.lock().expect("collector lock").clone()
        }
    }

    impl FrameSink for Collector {
        fn emit(&self, frame: DaemonFrame) {
            self.frames.lock().expect("collector lock").push(frame);
        }
    }

    /// Commands a fake daemon observed, so a test can assert that submitting an
    /// objective really put `StartRun` on the wire.
    #[derive(Default)]
    struct Observed {
        commands: StdMutex<Vec<CommandBody>>,
        repositories: StdMutex<HashMap<SessionId, Option<String>>>,
    }

    fn server_hello() -> ServerHello {
        ServerHello {
            selected_protocol: PROTOCOL_V1,
            daemon_version: "0.9.0-test".to_string(),
            daemon_instance: DaemonInstanceId::new(),
            heartbeat_interval_ms: 15_000,
            resume_token: None,
            build_id: "test-build".to_string(),
        }
    }

    fn event(sequence: u64, body: EventBody) -> SessionEvent {
        SessionEvent {
            sequence,
            occurred_at: chrono::Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body,
        }
    }

    #[tokio::test]
    async fn workflow_catalog_pins_repository_and_guarded_retry_identity() {
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_in(&dir);
        let listener = UnixListener::bind(&socket).unwrap();
        let digest = "a".repeat(64);
        let expected_digest = digest.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let hello = read_envelope(&mut stream).await.unwrap().unwrap();
            write_envelope(
                &mut stream,
                &Envelope::reply_to(&hello, Payload::ServerHello(server_hello())),
            )
            .await
            .unwrap();
            let mut launch_ids = Vec::new();
            for index in 0..4 {
                let request = read_envelope(&mut stream).await.unwrap().unwrap();
                let Payload::Command(command) = &request.payload else {
                    panic!("expected command");
                };
                let payload = match &command.body {
                    CommandBody::ListWorkflows { repository } => {
                        assert!(index < 2);
                        assert_eq!(repository.as_deref(), Some("/work/repo"));
                        Payload::WorkflowCatalog {
                            command_id: command.command_id,
                            catalog: WorkflowCatalog {
                                repository: Some(
                                    if index == 0 {
                                        "/work/repo"
                                    } else {
                                        "/work/foreign"
                                    }
                                    .into(),
                                ),
                                items: vec![],
                                diagnostics: vec![],
                                truncated: false,
                            },
                        }
                    }
                    CommandBody::StartWorkflow {
                        workflow_id,
                        inputs,
                        repository,
                        expected_definition_digest,
                        ..
                    } => {
                        assert_eq!(workflow_id.as_deref(), Some("review"));
                        assert_eq!(inputs, &serde_json::json!({"objective":"review the patch"}));
                        assert_eq!(repository.as_deref(), Some("/work/repo"));
                        assert_eq!(
                            expected_definition_digest.as_deref(),
                            Some(expected_digest.as_str())
                        );
                        assert_eq!(command.idempotency_key, "review-intent");
                        launch_ids.push(command.command_id);
                        Payload::WorkflowRunStarted {
                            command_id: command.command_id,
                            workflow_run_id: "owned-receipt".into(),
                        }
                    }
                    other => panic!("unexpected workflow command: {other:?}"),
                };
                write_envelope(&mut stream, &Envelope::reply_to(&request, payload))
                    .await
                    .unwrap();
            }
            assert_eq!(launch_ids.len(), 2);
            assert_ne!(
                launch_ids[0], launch_ids[1],
                "correlation ids change while launch identity stays fixed"
            );
        });
        let (client, _) = DaemonClient::connect(
            &socket,
            Some("/work/repo".into()),
            Arc::new(Collector::default()),
        )
        .await
        .unwrap();
        assert!(client
            .list_workflows(Some("/work/foreign".into()))
            .await
            .is_err());
        assert!(client
            .start_workflow_with_options(
                "review".into(),
                serde_json::json!({}),
                Some("/work/foreign".into()),
                None,
                None
            )
            .await
            .is_err());
        assert!(client
            .start_workflow_with_options(
                "review".into(),
                serde_json::json!({"text":"é".repeat(40_000)}),
                Some("/work/repo".into()),
                None,
                None
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("64 KiB"));
        assert!(client
            .start_workflow_with_options(
                "review".into(),
                serde_json::json!({}),
                Some("/work/repo".into()),
                Some(" ".into()),
                None
            )
            .await
            .is_err());
        assert_eq!(
            client
                .list_workflows(Some("/work/repo".into()))
                .await
                .unwrap()
                .repository
                .as_deref(),
            Some("/work/repo")
        );
        assert!(client
            .list_workflows(Some("/work/repo".into()))
            .await
            .unwrap_err()
            .to_string()
            .contains("another repository"));
        for _ in 0..2 {
            let receipt = client
                .start_workflow_with_options(
                    "review".into(),
                    serde_json::json!({"objective":"review the patch"}),
                    Some("/work/repo".into()),
                    Some("review-intent".into()),
                    Some(digest.clone()),
                )
                .await
                .unwrap();
            assert_eq!(receipt, "owned-receipt");
        }
        client.shutdown().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn workflow_catalog_and_digest_guard_do_not_reach_an_older_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_in(&dir);
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let hello = read_envelope(&mut stream).await.unwrap().unwrap();
            let mut selected = server_hello();
            selected.selected_protocol.minor = 11;
            write_envelope(
                &mut stream,
                &Envelope::reply_to(&hello, Payload::ServerHello(selected)),
            )
            .await
            .unwrap();
            let request = read_envelope(&mut stream).await.unwrap().unwrap();
            let Payload::Command(command) = &request.payload else {
                panic!("expected legacy start");
            };
            assert!(matches!(
                &command.body,
                CommandBody::StartWorkflow {
                    expected_definition_digest: None,
                    ..
                }
            ));
            write_envelope(
                &mut stream,
                &Envelope::reply_to(
                    &request,
                    Payload::WorkflowRunStarted {
                        command_id: command.command_id,
                        workflow_run_id: "legacy-receipt".into(),
                    },
                ),
            )
            .await
            .unwrap();
        });
        let (client, _) = DaemonClient::connect(
            &socket,
            Some("/work/repo".into()),
            Arc::new(Collector::default()),
        )
        .await
        .unwrap();
        assert!(client
            .list_workflows(Some("/work/repo".into()))
            .await
            .unwrap_err()
            .to_string()
            .contains("1.12"));
        assert!(client
            .start_workflow_with_options(
                "review".into(),
                serde_json::json!({}),
                Some("/work/repo".into()),
                Some("review-intent".into()),
                Some("a".repeat(64))
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("1.12"));
        assert_eq!(
            client
                .start_workflow("review".into(), serde_json::json!({}))
                .await
                .unwrap(),
            "legacy-receipt"
        );
        client.shutdown().await;
        server.await.unwrap();
    }

    /// A minimal daemon: handshake, accept every command, and emit one model
    /// delta after a `StartRun` so the client has a real event to forward.
    async fn serve(listener: UnixListener, observed: Arc<Observed>) {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut session: Option<SessionId> = None;
        while let Ok(Some(request)) = read_envelope(&mut stream).await {
            match request.payload.clone() {
                Payload::ClientHello(_) => {
                    let reply = Envelope::reply_to(&request, Payload::ServerHello(server_hello()));
                    write_envelope(&mut stream, &reply).await.expect("hello");
                }
                Payload::Command(command) => {
                    observed
                        .commands
                        .lock()
                        .expect("observed lock")
                        .push(command.body.clone());
                    match command.body {
                        CommandBody::CreateSession { repository, .. } => {
                            let id = SessionId::new();
                            session = Some(id);
                            observed.repositories.lock().unwrap().insert(id, repository);
                            let mut reply = Envelope::reply_to(
                                &request,
                                Payload::CommandAccepted {
                                    command_id: command.command_id,
                                    sequence: Some(1),
                                    created_run: None,
                                },
                            );
                            reply.session_id = Some(id);
                            write_envelope(&mut stream, &reply).await.expect("created");
                        }
                        CommandBody::ReadKnowledge {
                            session_id,
                            section,
                            ..
                        } => {
                            let page = codypendent_protocol::knowledge::KnowledgePage {
                                section,
                                workspace_id: None,
                                repository: observed
                                    .repositories
                                    .lock()
                                    .unwrap()
                                    .get(&session_id)
                                    .cloned()
                                    .flatten(),
                                skills: vec![],
                                memories: vec![],
                                learnings: vec![],
                                documents: vec![],
                                next_offset: None,
                                warnings: vec![],
                            };
                            write_envelope(
                                &mut stream,
                                &Envelope::reply_to(
                                    &request,
                                    Payload::KnowledgePage {
                                        command_id: command.command_id,
                                        page: Box::new(page),
                                    },
                                ),
                            )
                            .await
                            .unwrap();
                        }
                        CommandBody::AttachSession { .. } => {
                            let reply = Envelope::reply_to(
                                &request,
                                Payload::Catchup {
                                    catchup: Catchup::Events {
                                        from: 1,
                                        through: 1,
                                        events: vec![event(
                                            1,
                                            EventBody::SessionCreated {
                                                title: "replayed".to_string(),
                                            },
                                        )],
                                    },
                                },
                            );
                            write_envelope(&mut stream, &reply).await.expect("catchup");
                        }
                        CommandBody::StartRun { .. } => {
                            let run_id = RunId::new();
                            let reply = Envelope::reply_to(
                                &request,
                                Payload::CommandAccepted {
                                    command_id: command.command_id,
                                    sequence: Some(2),
                                    created_run: Some(run_id),
                                },
                            );
                            write_envelope(&mut stream, &reply).await.expect("started");

                            let mut live = Envelope::request(
                                request.client_id,
                                Payload::Event(event(
                                    3,
                                    EventBody::ModelStreamDelta {
                                        run_id,
                                        text: "real daemon output".to_string(),
                                        thought: false,
                                    },
                                )),
                            );
                            live.session_id = session;
                            write_envelope(&mut stream, &live).await.expect("event");
                        }
                        CommandBody::CancelRun { .. } => {
                            let reply = Envelope::reply_to(
                                &request,
                                Payload::CommandAccepted {
                                    command_id: command.command_id,
                                    sequence: Some(4),
                                    created_run: None,
                                },
                            );
                            write_envelope(&mut stream, &reply)
                                .await
                                .expect("cancelled");
                        }
                        CommandBody::ResolveApproval { .. } => {
                            let reply = Envelope::reply_to(
                                &request,
                                Payload::CommandAccepted {
                                    command_id: command.command_id,
                                    sequence: Some(5),
                                    created_run: None,
                                },
                            );
                            write_envelope(&mut stream, &reply)
                                .await
                                .expect("approval resolved");
                        }
                        // The real daemon answers `QueueSteering` with a plain
                        // acceptance and emits `SteeringQueued` separately, so the
                        // acceptance is deliberately NOT a claim that the text was
                        // queued — see `Steering.tsx`, which keeps accepted, queued
                        // and applied apart because the daemon does.
                        CommandBody::QueueSteering { .. } => {
                            let reply = Envelope::reply_to(
                                &request,
                                Payload::CommandAccepted {
                                    command_id: command.command_id,
                                    sequence: Some(6),
                                    created_run: None,
                                },
                            );
                            write_envelope(&mut stream, &reply)
                                .await
                                .expect("steering accepted");
                        }
                        _ => {
                            let reply = Envelope::reply_to(
                                &request,
                                Payload::CommandRejected(CodypendentError::new(
                                    "test.unsupported",
                                    "the test daemon does not implement this command",
                                    false,
                                )),
                            );
                            write_envelope(&mut stream, &reply).await.expect("rejected");
                        }
                    }
                }
                Payload::RemoteUi { message } => {
                    let reply = Envelope::reply_to(&request, Payload::RemoteUi { message });
                    write_envelope(&mut stream, &reply)
                        .await
                        .expect("remote UI echo");
                }
                _ => {}
            }
        }
    }

    fn socket_in(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("daemon.sock")
    }

    #[tokio::test]
    async fn knowledge_reads_the_selected_session_and_collects_every_page() {
        use codypendent_protocol::knowledge::{KnowledgePage, KnowledgeSection, SkillCard};
        let dir = tempfile::tempdir().unwrap();
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).unwrap();
        let session_id = SessionId::new();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let hello = read_envelope(&mut stream).await.unwrap().unwrap();
            write_envelope(
                &mut stream,
                &Envelope::reply_to(&hello, Payload::ServerHello(server_hello())),
            )
            .await
            .unwrap();
            for offset in [0, 1] {
                let request = read_envelope(&mut stream).await.unwrap().unwrap();
                let Payload::Command(command) = request.payload.clone() else {
                    panic!("expected knowledge request");
                };
                assert!(
                    matches!(command.body, CommandBody::ReadKnowledge { session_id: selected, section: KnowledgeSection::Skills, offset: requested, limit: 100 } if selected == session_id && requested == offset)
                );
                let page = KnowledgePage {
                    section: KnowledgeSection::Skills,
                    workspace_id: None,
                    repository: None,
                    next_offset: (offset == 0).then_some(1),
                    warnings: vec![],
                    memories: vec![],
                    learnings: vec![],
                    documents: vec![],
                    skills: vec![SkillCard {
                        name: format!("skill-{offset}"),
                        kind: "skill".into(),
                        scope: "system".into(),
                        trust: "first-party".into(),
                        status: "active".into(),
                        risk: "safe".into(),
                        description: "shared projection".into(),
                        permissions: vec![],
                    }],
                };
                write_envelope(
                    &mut stream,
                    &Envelope::reply_to(
                        &request,
                        Payload::KnowledgePage {
                            command_id: command.command_id,
                            page: Box::new(page),
                        },
                    ),
                )
                .await
                .unwrap();
            }
        });
        let (client, _) = DaemonClient::connect(&path, None, Arc::new(Collector::default()))
            .await
            .unwrap();
        *client.attached.lock().await = Some(session_id);
        let page = client
            .read_knowledge(KnowledgeSection::Skills)
            .await
            .unwrap();
        assert_eq!(
            page.skills
                .iter()
                .map(|card| card.name.as_str())
                .collect::<Vec<_>>(),
            vec!["skill-0", "skill-1"]
        );
        server.await.unwrap();
        client.shutdown().await;
    }

    #[tokio::test]
    async fn knowledge_cold_start_reuses_an_internal_session_without_selecting_a_chat() {
        use codypendent_protocol::knowledge::KnowledgeSection;
        let dir = tempfile::tempdir().unwrap();
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).unwrap();
        let observed = Arc::new(Observed::default());
        tokio::spawn(serve(listener, Arc::clone(&observed)));
        let sink = Arc::new(Collector::default());
        let (client, _) =
            DaemonClient::connect(&path, Some("/work/repo".into()), Arc::clone(&sink))
                .await
                .unwrap();
        let (skills, memories) = tokio::join!(
            client.read_knowledge(KnowledgeSection::Skills),
            client.read_knowledge(KnowledgeSection::Memories),
        );
        assert_eq!(skills.unwrap().repository.as_deref(), Some("/work/repo"));
        assert_eq!(memories.unwrap().repository.as_deref(), Some("/work/repo"));
        let session = client.knowledge_session.lock().await.unwrap();
        assert_eq!(*client.attached.lock().await, None);
        assert!(sink.frames().is_empty());
        let commands = observed.commands.lock().unwrap().clone();
        assert_eq!(
            commands
                .iter()
                .filter(|body| matches!(body, CommandBody::CreateSession { internal: true, .. }))
                .count(),
            1
        );
        assert_eq!(
            commands
                .iter()
                .filter(|body| matches!(body,
            CommandBody::ReadKnowledge { session_id, .. } if *session_id == session))
                .count(),
            2
        );
        assert!(!commands.iter().any(|body| matches!(
            body,
            CommandBody::AttachSession { .. } | CommandBody::StartRun { .. }
        )));
        client.shutdown().await;
    }

    #[tokio::test]
    async fn knowledge_writes_follow_the_selected_sessions_repository() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).unwrap();
        let observed = Arc::new(Observed::default());
        let session = SessionId::new();
        observed
            .repositories
            .lock()
            .unwrap()
            .insert(session, Some("/work/selected".into()));
        tokio::spawn(serve(listener, Arc::clone(&observed)));
        let sink = Arc::new(Collector::default());
        let (client, _) =
            DaemonClient::connect(&path, Some("/work/initial".into()), Arc::clone(&sink))
                .await
                .unwrap();
        client.attach(session, &sink).await.unwrap();
        let _ = client.create_document("Runbook".into()).await;
        let _ = client.forget_memory(MemoryId::new()).await;
        let _ = client.inspect_memory(MemoryId::new()).await;
        let commands = observed.commands.lock().unwrap().clone();
        assert!(commands.iter().any(|body| matches!(body,
            CommandBody::CreateDocument { repository: Some(repository), .. }
                if repository == "/work/selected")));
        let memory_repositories: Vec<_> = commands
            .iter()
            .filter_map(|body| match body {
                CommandBody::ForgetMemory { repository, .. }
                | CommandBody::InspectMemory { repository, .. } => Some(repository.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            memory_repositories,
            vec!["/work/selected", "/work/selected"]
        );
        assert!(!commands
            .iter()
            .any(|body| matches!(body, CommandBody::CreateSession { .. })));
        client.shutdown().await;
    }

    #[tokio::test]
    async fn knowledge_writes_never_fall_back_to_the_connection_repository() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).unwrap();
        let observed = Arc::new(Observed::default());
        tokio::spawn(serve(listener, Arc::clone(&observed)));
        let sink = Arc::new(Collector::default());
        let (client, _) =
            DaemonClient::connect(&path, Some("/work/initial".into()), Arc::clone(&sink))
                .await
                .unwrap();
        client.attach(SessionId::new(), &sink).await.unwrap();
        assert!(client.create_document("Runbook".into()).await.is_err());
        assert!(client.forget_memory(MemoryId::new()).await.is_err());
        assert!(client
            .mutate_learning(
                "foreign-learning",
                1,
                &crate::knowledge::LearningMutation::Activate
            )
            .await
            .is_err());
        assert!(!observed
            .commands
            .lock()
            .unwrap()
            .iter()
            .any(|body| matches!(
                body,
                CommandBody::CreateDocument { .. } | CommandBody::ForgetMemory { .. }
            )));
        client.shutdown().await;
    }

    #[tokio::test]
    async fn remote_ui_is_session_bound_forwarded_and_not_a_durable_event() {
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_in(&dir);
        let server = tokio::spawn(serve(
            UnixListener::bind(&socket).unwrap(),
            Arc::new(Observed::default()),
        ));
        let sink = Arc::new(Collector::default());
        let (client, _) = DaemonClient::connect(&socket, None, sink.clone())
            .await
            .unwrap();
        let session = SessionId::new();
        client.attach(session, &sink).await.unwrap();
        let sequence = client.last_seen.load(Ordering::SeqCst);
        let message: codypendent_protocol::UiWireMessage =
            serde_json::from_value(serde_json::json!({
                "type": "resync", "messageId": "recovery", "resync": { "documentId": "document" }
            }))
            .unwrap();
        assert!(client
            .send_remote_ui(SessionId::new(), message.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("no longer attached"));
        let forbidden: codypendent_protocol::UiWireMessage = serde_json::from_value(serde_json::json!({
            "type": "dispose", "messageId": "producer", "dispose": { "documentId": "document", "revision": 1 }
        })).unwrap();
        assert!(client
            .send_remote_ui(session, forbidden)
            .await
            .unwrap_err()
            .to_string()
            .contains("not a renderer request"));
        client.send_remote_ui(session, message).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if sink.frames().iter().any(|frame| matches!(frame, DaemonFrame::RemoteUi { session_id: Some(id), message } if *id == session && message.kind == "resync")) { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("forwarded Remote UI frame");
        assert_eq!(client.last_seen.load(Ordering::SeqCst), sequence);
        client.shutdown().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn legacy_daemon_probe_is_reported_as_protocol_support_not_bad_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_in(&dir);
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_envelope(&mut stream).await.unwrap().unwrap();
            let mut hello = server_hello();
            hello.selected_protocol.minor = 8;
            write_envelope(
                &mut stream,
                &Envelope::reply_to(&request, Payload::ServerHello(hello)),
            )
            .await
            .unwrap();
            assert!(
                read_envelope(&mut stream).await.unwrap().is_none(),
                "no unsupported probe was sent"
            );
        });
        let (client, _) = DaemonClient::connect(&socket, None, Arc::new(Collector::default()))
            .await
            .unwrap();
        let error = client
            .probe_models(None, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("daemon"), "{error}");
        assert!(
            error.contains("update") || error.contains("Update"),
            "{error}"
        );
        client.shutdown().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn connect_fails_when_no_daemon_listens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = Arc::new(Collector::default());
        let result = DaemonClient::connect(&socket_in(&dir), None, sink).await;
        assert!(
            result.is_err(),
            "connecting to an absent daemon must fail, never report a connection"
        );
    }

    /// Opening Plugins before choosing a session is a normal state, and the
    /// UI defaults the enable scope to `session`. Sending that with no
    /// attachment is refused by the daemon as `SessionBindingRequired`, so the
    /// DEFAULT enable path could never succeed. The prerequisite is now named
    /// before the wire; a `user`-scoped enable still needs no session.
    #[tokio::test]
    async fn a_scoped_plugin_enable_names_its_missing_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).expect("bind");
        let observed = Arc::new(Observed::default());
        tokio::spawn(serve(listener, Arc::clone(&observed)));

        let sink = Arc::new(Collector::default());
        let (client, _) = DaemonClient::connect(&path, None, Arc::clone(&sink))
            .await
            .expect("connect");

        let refused = client
            .enable_ui_plugin("charts".to_string(), "session".to_string())
            .await
            .expect_err("no session attached");
        let sentence = format!("{refused:#}");
        assert!(
            sentence.contains("attach a session first") && sentence.contains("session"),
            "the refusal names the prerequisite: {sentence}"
        );
        assert!(
            !observed
                .commands
                .lock()
                .expect("observed lock")
                .iter()
                .any(|body| matches!(body, CommandBody::EnableUiPlugin { .. })),
            "nothing reached the wire"
        );

        // A user-scoped enable is unaffected: it binds to no session, so it
        // reaches the daemon (which this test server does not implement).
        let _ = client
            .enable_ui_plugin("charts".to_string(), "user".to_string())
            .await;
        let sent = observed
            .commands
            .lock()
            .expect("observed lock")
            .iter()
            .any(|body| {
                matches!(
                    body,
                    CommandBody::EnableUiPlugin {
                        session_id: None,
                        ..
                    }
                )
            });
        assert!(sent, "a user-scoped enable still goes out");
    }

    /// A local knowledge read must scope itself by the repository the LIVE
    /// connection carries. Changing the repository stages a new selection at
    /// once while the client keeps the one it connected with until a
    /// reconnect, so reading by the selection and writing by the connection
    /// lists checkout B while a create writes into A — and the refresh loses
    /// the new document.
    #[tokio::test]
    async fn a_connection_reports_the_repository_its_commands_will_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).expect("bind");
        tokio::spawn(serve(listener, Arc::new(Observed::default())));

        let sink = Arc::new(Collector::default());
        let (client, _) =
            DaemonClient::connect(&path, Some("/work/repo".to_string()), Arc::clone(&sink))
                .await
                .expect("connect");
        assert_eq!(client.repository(), Some("/work/repo"));

        let path = dir.path().join("unscoped.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        tokio::spawn(serve(listener, Arc::new(Observed::default())));
        let (unscoped, _) = DaemonClient::connect(&path, None, Arc::clone(&sink))
            .await
            .expect("connect");
        assert_eq!(unscoped.repository(), None);
    }

    /// A workspace is an identity, not a connection attribute. Minting one per
    /// connect meant every automatic reconnect adopted a new workspace while
    /// the app re-attached the SAME session, so workspace-scoped memories and
    /// documents dropped out of view until a matching scope came back.
    #[tokio::test]
    async fn a_named_workspace_survives_a_reconnect() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = codypendent_protocol::WorkspaceId::new();
        let sink = Arc::new(Collector::default());

        let mut seen = Vec::new();
        for name in ["first.sock", "second.sock"] {
            let path = dir.path().join(name);
            let listener = UnixListener::bind(&path).expect("bind");
            tokio::spawn(serve(listener, Arc::new(Observed::default())));
            let (client, _) =
                DaemonClient::connect_as(&path, None, Some(workspace), Arc::clone(&sink))
                    .await
                    .expect("connect");
            seen.push(client.workspace());
        }
        assert_eq!(
            seen,
            vec![workspace, workspace],
            "the knowledge scope must not depend on how many times the socket dropped"
        );

        // Unnamed still mints its own, which is what a one-shot wants.
        let path = dir.path().join("third.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        tokio::spawn(serve(listener, Arc::new(Observed::default())));
        let (fresh, _) = DaemonClient::connect(&path, None, Arc::clone(&sink))
            .await
            .expect("connect");
        assert_ne!(fresh.workspace(), workspace);
    }

    /// Creating a document without a selected repository would land it in the
    /// DAEMON's startup checkout, while this client's own reads carry no
    /// repository scope at all — the create would report success and the
    /// refreshed list would not contain it. The guard refuses before the wire,
    /// and names the fix.
    #[tokio::test]
    async fn a_document_is_never_created_into_a_scope_this_client_cannot_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).expect("bind");
        let observed = Arc::new(Observed::default());
        tokio::spawn(serve(listener, Arc::clone(&observed)));

        let sink = Arc::new(Collector::default());
        let (client, _) = DaemonClient::connect(&path, None, Arc::clone(&sink))
            .await
            .expect("connect");

        let refused = client
            .create_document("Runbook".to_string())
            .await
            .expect_err("no repository, no document");
        let sentence = format!("{refused:#}");
        assert!(
            sentence.contains("select a repository first") && sentence.contains("documents"),
            "the refusal names the fix and its subject: {sentence}"
        );
        assert!(
            !observed
                .commands
                .lock()
                .expect("observed lock")
                .iter()
                .any(|body| matches!(body, CommandBody::CreateDocument { .. })),
            "nothing reached the wire"
        );
    }

    /// With a repository selected the command carries it, so the daemon writes
    /// where `knowledge_identity` will later look.
    #[tokio::test]
    async fn a_scoped_connection_names_its_repository_when_creating_a_document() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).expect("bind");
        let observed = Arc::new(Observed::default());
        tokio::spawn(serve(listener, Arc::clone(&observed)));

        let sink = Arc::new(Collector::default());
        let (client, _) =
            DaemonClient::connect(&path, Some("/work/repo".to_string()), Arc::clone(&sink))
                .await
                .expect("connect");

        // The test daemon rejects the command; what matters is what it saw.
        let _ = client.create_document("Runbook".to_string()).await;
        let commands = observed.commands.lock().expect("observed lock").clone();
        let created = commands
            .iter()
            .find_map(|body| match body {
                CommandBody::CreateDocument { repository, .. } => Some(repository.clone()),
                _ => None,
            })
            .expect("CreateDocument reached the wire");
        assert_eq!(created.as_deref(), Some("/work/repo"));
    }

    #[tokio::test]
    async fn prepared_objective_publishes_the_minted_attached_session_before_starting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let observed = Arc::new(Observed::default());
        let server = tokio::spawn(serve(
            UnixListener::bind(&path).expect("bind"),
            Arc::clone(&observed),
        ));
        let sink = Arc::new(Collector::default());
        let (client, _) = DaemonClient::connect(&path, None, Arc::clone(&sink))
            .await
            .expect("connect");
        let mut prepared = None;
        let handle = client.start_objective_prepared("First chat".into(), AgentMode::Build, None, None, &sink, |session| {
            prepared = Some(session);
            assert_eq!(*client.attached.try_lock().expect("attachment complete"), Some(session));
            let commands = observed.commands.lock().expect("observed");
            assert!(matches!(commands.as_slice(), [CommandBody::CreateSession { .. }, CommandBody::AttachSession { session_id, .. }] if *session_id == session));
            assert!(sink.frames().iter().any(|frame| matches!(frame, DaemonFrame::Event { event, .. } if matches!(event.body, EventBody::SessionCreated { .. }))));
            Ok(())
        }).await.expect("prepared start");
        assert_eq!(prepared, Some(handle.session_id));
        assert!(
            matches!(observed.commands.lock().expect("observed").last(), Some(CommandBody::StartRun { session_id, .. }) if *session_id == handle.session_id)
        );
        client.shutdown().await;
        server.await.expect("server finished");
    }

    #[tokio::test]
    async fn prepared_objective_failure_prevents_start_run_and_wrapper_can_retry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let observed = Arc::new(Observed::default());
        let server = tokio::spawn(serve(
            UnixListener::bind(&path).expect("bind"),
            Arc::clone(&observed),
        ));
        let sink = Arc::new(Collector::default());
        let (client, _) = DaemonClient::connect(&path, None, Arc::clone(&sink))
            .await
            .expect("connect");
        let mut prepared = None;
        let error = client
            .start_objective_prepared(
                "First chat".into(),
                AgentMode::Build,
                None,
                None,
                &sink,
                |session| {
                    prepared = Some(session);
                    anyhow::bail!("fixture desktop publication failed")
                },
            )
            .await
            .expect_err("publication failed");
        assert!(error
            .to_string()
            .contains("fixture desktop publication failed"));
        assert!(!observed
            .commands
            .lock()
            .expect("observed")
            .iter()
            .any(|command| matches!(command, CommandBody::StartRun { .. })));
        let handle = client
            .start_objective("Try again".into(), AgentMode::Build, None, prepared, &sink)
            .await
            .expect("wrapper retries attached session");
        assert_eq!(Some(handle.session_id), prepared);
        assert_eq!(
            observed
                .commands
                .lock()
                .expect("observed")
                .iter()
                .filter(|command| matches!(command, CommandBody::CreateSession { .. }))
                .count(),
            1
        );
        assert_eq!(
            observed
                .commands
                .lock()
                .expect("observed")
                .iter()
                .filter(|command| matches!(command, CommandBody::StartRun { .. }))
                .count(),
            1
        );
        client.shutdown().await;
        server.await.expect("server finished");
    }

    #[tokio::test]
    async fn follow_up_objective_keeps_the_attached_conversation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let observed = Arc::new(Observed::default());
        let server = tokio::spawn(serve(
            UnixListener::bind(&path).expect("bind"),
            Arc::clone(&observed),
        ));
        let sink = Arc::new(Collector::default());
        let (client, _) = DaemonClient::connect(&path, None, Arc::clone(&sink))
            .await
            .expect("connect");
        let first = client
            .start_objective("Hello".into(), AgentMode::Build, None, None, &sink)
            .await
            .expect("first message");
        let second = client
            .start_objective(
                "Review the project".into(),
                AgentMode::Build,
                None,
                Some(first.session_id),
                &sink,
            )
            .await
            .expect("follow-up");
        assert_eq!(first.session_id, second.session_id);
        let commands = observed.commands.lock().expect("observed lock").clone();
        assert_eq!(
            commands
                .iter()
                .filter(|body| matches!(body, CommandBody::CreateSession { .. }))
                .count(),
            1
        );
        assert_eq!(
            commands
                .iter()
                .filter(|body| matches!(body, CommandBody::AttachSession { .. }))
                .count(),
            1
        );
        let starts: Vec<_> = commands
            .iter()
            .filter_map(|body| match body {
                CommandBody::StartRun {
                    session_id,
                    objective,
                    ..
                } => Some((*session_id, objective.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            vec![
                (first.session_id, "Hello"),
                (first.session_id, "Review the project")
            ]
        );
        let before = commands.len();
        assert!(client
            .start_objective(
                "Wrong chat".into(),
                AgentMode::Build,
                None,
                Some(SessionId::new()),
                &sink
            )
            .await
            .is_err());
        assert_eq!(
            observed.commands.lock().expect("observed lock").len(),
            before
        );
        client.shutdown().await;
        server.await.expect("server finished");
    }

    #[tokio::test]
    async fn submitting_an_objective_sends_real_commands_and_forwards_real_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).expect("bind");
        let observed = Arc::new(Observed::default());
        tokio::spawn(serve(listener, Arc::clone(&observed)));

        let sink = Arc::new(Collector::default());
        let (client, info) = DaemonClient::connect(&path, None, Arc::clone(&sink))
            .await
            .expect("connect");
        assert_eq!(info.daemon_version, "0.9.0-test");

        let handle = client
            .start_objective(
                "ship the thing".to_string(),
                AgentMode::Build,
                None,
                None,
                &sink,
            )
            .await
            .expect("start objective");
        assert!(
            handle.run_id.is_some(),
            "the daemon named the run it created"
        );

        let commands = observed.commands.lock().expect("observed lock").clone();
        let started = commands
            .iter()
            .find_map(|command| match command {
                CommandBody::StartRun { objective, .. } => Some(objective.clone()),
                _ => None,
            })
            .expect("a StartRun command reached the daemon");
        assert_eq!(started, "ship the thing");

        // The live event the daemon emitted must arrive; nothing else may.
        for _ in 0..50 {
            if sink
                .frames()
                .iter()
                .any(|frame| matches!(frame, DaemonFrame::Event { event, .. } if matches!(event.body, EventBody::ModelStreamDelta { .. })))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let frames = sink.frames();
        let deltas: Vec<String> = frames
            .iter()
            .filter_map(|frame| match frame {
                DaemonFrame::Event { event, .. } => match &event.body {
                    EventBody::ModelStreamDelta { text, .. } => Some(text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["real daemon output".to_string()]);

        let replayed = frames.iter().any(|frame| {
            matches!(frame, DaemonFrame::Event { event, .. }
                if matches!(event.body, EventBody::SessionCreated { .. }))
        });
        assert!(replayed, "attach catch-up is replayed into the transcript");

        client
            .queue_steering(
                handle.run_id.expect("run id"),
                "prefer the parser".to_string(),
            )
            .await
            .expect("steer");
        let steered = observed
            .commands
            .lock()
            .expect("observed lock")
            .iter()
            .any(|command| {
                matches!(command, CommandBody::QueueSteering { text, .. } if text == "prefer the parser")
            });
        assert!(steered, "steering sends a real QueueSteering command");
        assert!(
            client
                .queue_steering(handle.run_id.expect("run id"), "   ".to_string())
                .await
                .is_err(),
            "blank steering is refused before it reaches the daemon"
        );

        client
            .cancel_run(handle.run_id.expect("run id"))
            .await
            .expect("cancel");
        let cancelled = observed
            .commands
            .lock()
            .expect("observed lock")
            .iter()
            .any(|command| matches!(command, CommandBody::CancelRun { .. }));
        assert!(cancelled, "cancel sends a real CancelRun command");

        let approval_id = ApprovalId::new();
        client
            .resolve_approval(approval_id, ApprovalDecision::Approve, ApprovalScope::Once)
            .await
            .expect("approve");
        let resolved = observed
            .commands
            .lock()
            .expect("observed lock")
            .iter()
            .any(|command| {
                matches!(
                    command,
                    CommandBody::ResolveApproval {
                        approval_id: seen,
                        decision: ApprovalDecision::Approve,
                        scope: ApprovalScope::Once,
                    } if *seen == approval_id
                )
            });
        assert!(resolved, "approval sends a real ResolveApproval command");
        for scope in [
            ApprovalScope::Run,
            ApprovalScope::Pattern,
            ApprovalScope::Repository,
        ] {
            let id = ApprovalId::new();
            client
                .resolve_approval(id, ApprovalDecision::Approve, scope)
                .await
                .expect("scoped approval");
            assert!(observed.commands.lock().expect("observed lock").iter().any(|command| {
                matches!(command, CommandBody::ResolveApproval { approval_id, decision: ApprovalDecision::Approve, scope: seen } if *approval_id == id && *seen == scope)
            }));
        }
        assert!(client
            .resolve_approval(
                approval_id,
                ApprovalDecision::Approve,
                ApprovalScope::Unknown
            )
            .await
            .is_err());
        assert!(client
            .resolve_approval(
                approval_id,
                ApprovalDecision::Reject,
                ApprovalScope::Repository
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn snapshot_attach_restores_the_authoritative_history_range() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).expect("bind");
        let session_id = SessionId::new();
        let history_target = 501_u64;
        let requested_after = Arc::new(StdMutex::new(Vec::new()));
        let server_requested_after = Arc::clone(&requested_after);

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            while let Ok(Some(request)) = read_envelope(&mut stream).await {
                match request.payload.clone() {
                    Payload::ClientHello(_) => {
                        let reply =
                            Envelope::reply_to(&request, Payload::ServerHello(server_hello()));
                        write_envelope(&mut stream, &reply).await.expect("hello");
                    }
                    Payload::Command(command) => match command.body {
                        CommandBody::AttachSession { .. } => {
                            let reply = Envelope::reply_to(
                                &request,
                                Payload::Catchup {
                                    catchup: Catchup::Snapshot {
                                        through: history_target,
                                        projection: SessionProjection {
                                            session_id,
                                            title: "long session".to_string(),
                                            last_sequence: history_target,
                                            active_runs: Vec::new(),
                                            pending_approvals: Vec::new(),
                                            pending_prompts: Vec::new(),
                                            closed: false,
                                        },
                                    },
                                },
                            );
                            write_envelope(&mut stream, &reply).await.expect("snapshot");
                        }
                        CommandBody::ReadSessionEvents {
                            after_sequence,
                            limit,
                            ..
                        } => {
                            server_requested_after
                                .lock()
                                .expect("requested-after lock")
                                .push(after_sequence);
                            let events = (1..=history_target)
                                .filter(|sequence| *sequence > after_sequence)
                                .take(limit as usize)
                                .map(|sequence| {
                                    event(
                                        sequence,
                                        EventBody::SessionCreated {
                                            title: format!("event-{sequence}"),
                                        },
                                    )
                                })
                                .collect::<Vec<_>>();
                            let through =
                                events.last().map_or(after_sequence, |event| event.sequence);
                            let reply = Envelope::reply_to(
                                &request,
                                Payload::SessionEventsPage {
                                    command_id: command.command_id,
                                    session_id,
                                    events,
                                    through,
                                    has_more: false,
                                },
                            );
                            write_envelope(&mut stream, &reply).await.expect("history");
                        }
                        other => panic!("unexpected command: {other:?}"),
                    },
                    _ => {}
                }
            }
        });

        let sink = Arc::new(Collector::default());
        let (client, _) = DaemonClient::connect(&path, None, Arc::clone(&sink))
            .await
            .expect("connect");
        client.attach(session_id, &sink).await.expect("attach");

        let frames = sink.frames();
        assert!(frames
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::Catchup { .. })));
        let history = frames.iter().find_map(|frame| match frame {
            DaemonFrame::History {
                session_id: returned,
                through,
                events,
            } if *returned == session_id => Some((*through, events.len())),
            _ => None,
        });
        assert_eq!(history, Some((history_target, history_target as usize)));
        assert_eq!(
            *requested_after.lock().expect("requested-after lock"),
            vec![0, 500],
            "history reads advance through bounded pages"
        );
    }

    /// Opening a panel must not wait for a long session's history download.
    ///
    /// The attach lock exists to serialize the attachment decision — which
    /// session is attached, and the subscription reset that goes with it. It
    /// used to be held across the paged history read that follows a snapshot
    /// catch-up too: 500 events per round trip, seconds of them for a long
    /// session. Every panel opens its live stream through `grow_subscriptions`,
    /// which takes the same lock, so attaching a long session and then opening
    /// the workflow or blackboard panel hung the panel for the whole download.
    ///
    /// Here the server stalls the history read until the test releases it, and
    /// a subscription growth must still complete while it is stalled.
    #[tokio::test]
    async fn growing_a_subscription_does_not_wait_for_a_history_download() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).expect("bind");
        let session_id = SessionId::new();
        let history_target = 400_u64;
        // Closed once the test is satisfied, which releases the stalled read.
        let (release, released) = tokio::sync::mpsc::channel::<()>(1);
        // Signals that the server has received the history read and is holding
        // it, so the test knows `attach` is inside the stalled section.
        let (reading, mut is_reading) = tokio::sync::mpsc::channel::<()>(1);

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            // Read and write halves are separate so a held reply cannot stop
            // the server reading the next request — otherwise the stall below
            // would block the client on the SERVER rather than on the lock,
            // and the test would pass or fail for the wrong reason.
            let (mut reader, writer) = tokio::io::split(stream);
            let writer = Arc::new(tokio::sync::Mutex::new(writer));
            let mut released = Some(released);
            while let Ok(Some(request)) = read_envelope(&mut reader).await {
                match request.payload.clone() {
                    Payload::ClientHello(_) => {
                        let reply =
                            Envelope::reply_to(&request, Payload::ServerHello(server_hello()));
                        write_envelope(&mut *writer.lock().await, &reply)
                            .await
                            .expect("hello");
                    }
                    Payload::Command(command) => match command.body {
                        CommandBody::AttachSession { subscriptions, .. } => {
                            // The first attach answers with a snapshot, which
                            // is what triggers the history read. The re-attach
                            // that grows the subscription set is the one under
                            // test and answers immediately.
                            let growing = subscriptions
                                .iter()
                                .any(|s| matches!(s, Subscription::Workflow { .. }));
                            let catchup = if growing {
                                Catchup::Events {
                                    from: history_target,
                                    through: history_target,
                                    events: Vec::new(),
                                }
                            } else {
                                Catchup::Snapshot {
                                    through: history_target,
                                    projection: SessionProjection {
                                        session_id,
                                        title: "long session".to_string(),
                                        last_sequence: history_target,
                                        active_runs: Vec::new(),
                                        pending_approvals: Vec::new(),
                                        pending_prompts: Vec::new(),
                                        closed: false,
                                    },
                                }
                            };
                            let reply = Envelope::reply_to(&request, Payload::Catchup { catchup });
                            write_envelope(&mut *writer.lock().await, &reply)
                                .await
                                .expect("catchup");
                        }
                        CommandBody::ReadSessionEvents { .. } => {
                            // Hold this reply open until the test says so,
                            // WITHOUT stopping the read loop.
                            let reply = Envelope::reply_to(
                                &request,
                                Payload::SessionEventsPage {
                                    command_id: command.command_id,
                                    session_id,
                                    events: Vec::new(),
                                    through: history_target,
                                    has_more: false,
                                },
                            );
                            let writer = Arc::clone(&writer);
                            let reading = reading.clone();
                            let mut gate = released.take().expect("one history read");
                            tokio::spawn(async move {
                                let _ = reading.send(()).await;
                                let _ = gate.recv().await;
                                write_envelope(&mut *writer.lock().await, &reply)
                                    .await
                                    .expect("history");
                            });
                        }
                        other => panic!("unexpected command: {other:?}"),
                    },
                    _ => {}
                }
            }
        });

        let sink = Arc::new(Collector::default());
        let (client, _) = DaemonClient::connect(&path, None, Arc::clone(&sink))
            .await
            .expect("connect");
        let client = Arc::new(client);

        let attaching = {
            let client = Arc::clone(&client);
            let sink = Arc::clone(&sink);
            tokio::spawn(async move { client.attach(session_id, &sink).await })
        };

        // Wait until the history read is genuinely in flight and stalled.
        tokio::time::timeout(Duration::from_secs(5), is_reading.recv())
            .await
            .expect("the history read should have started")
            .expect("reading signal");

        // The point of the test: this must not queue behind the download.
        tokio::time::timeout(
            Duration::from_secs(5),
            client.grow_subscriptions(
                vec![Subscription::Workflow {
                    workflow_run_id: "wf-1".to_string(),
                }],
                &sink,
            ),
        )
        .await
        .expect("a panel opening its stream must not wait for the history download")
        .expect("grow");

        drop(release);
        tokio::time::timeout(Duration::from_secs(5), attaching)
            .await
            .expect("attach should finish")
            .expect("join")
            .expect("attach");
    }

    #[tokio::test]
    async fn a_closed_socket_reports_a_disconnect() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).expect("bind");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = read_envelope(&mut stream)
                .await
                .expect("read")
                .expect("hello");
            let reply = Envelope::reply_to(&request, Payload::ServerHello(server_hello()));
            write_envelope(&mut stream, &reply).await.expect("hello");
            drop(stream);
        });

        let sink = Arc::new(Collector::default());
        let (_client, _info) = DaemonClient::connect(&path, None, Arc::clone(&sink))
            .await
            .expect("connect");

        for _ in 0..50 {
            if sink
                .frames()
                .iter()
                .any(|frame| matches!(frame, DaemonFrame::Disconnected { .. }))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("a dropped socket must surface as a Disconnected frame");
    }

    #[tokio::test]
    async fn the_handshake_advertises_this_client() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).expect("bind");
        let seen: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
        let recorder = Arc::clone(&seen);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = read_envelope(&mut stream)
                .await
                .expect("read")
                .expect("hello");
            if let Payload::ClientHello(hello) = &request.payload {
                *recorder.lock().expect("recorder") = Some(hello.client_name.clone());
                assert_eq!(hello.capabilities, ClientCapabilities::default());
            }
            let reply = Envelope::reply_to(&request, Payload::ServerHello(server_hello()));
            write_envelope(&mut stream, &reply).await.expect("hello");
            // Hold the socket open so the client is still connected when the
            // assertion below runs.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let sink = Arc::new(Collector::default());
        let (_client, _info) = DaemonClient::connect(&path, None, sink)
            .await
            .expect("connect");
        assert_eq!(
            seen.lock().expect("recorder").clone(),
            Some(CLIENT_NAME.to_string())
        );
    }

    fn inbox_entry(id: InboxEntryId, state: InboxEntryState) -> InboxEntry {
        InboxEntry {
            id,
            repository_id: RepositoryId::new(),
            kind: InboxEntryKind::ApprovalRequest,
            state,
            title: "approve the patch".to_string(),
            summary: String::new(),
            source: InboxSource {
                identity: InboxSourceIdentity::Run {
                    run_id: RunId::new(),
                },
                dedup_key: "run-1".to_string(),
                session_id: None,
                run_id: None,
                workflow_id: None,
            },
            deep_link: InboxDeepLink::Repository {
                repository_id: RepositoryId::new(),
            },
            created_at: chrono::Utc::now(),
            acknowledged_at: None,
            dismissed_at: None,
            resolved_at: None,
        }
    }

    fn analytics_export_result(artifact: ArtifactRef) -> AnalyticsExportResult {
        AnalyticsExportResult {
            format: AnalyticsExportFormat::Csv,
            artifact,
            row_count: 1,
            truncated: false,
            generated_at: chrono::Utc::now(),
        }
    }

    /// A daemon that answers the inbox/analytics/artifact surface. Artifact
    /// bytes are handed back in deliberately small chunks so the retrieval loop
    /// has to page, exactly as it must against the real daemon's byte ceiling.
    async fn serve_reads(
        listener: UnixListener,
        observed: Arc<Observed>,
        entry: InboxEntry,
        artifact: ArtifactRef,
        contents: Vec<u8>,
    ) {
        const CHUNK: usize = 4;
        let (mut stream, _) = listener.accept().await.expect("accept");
        while let Ok(Some(request)) = read_envelope(&mut stream).await {
            match request.payload.clone() {
                Payload::ClientHello(_) => {
                    let reply = Envelope::reply_to(&request, Payload::ServerHello(server_hello()));
                    write_envelope(&mut stream, &reply).await.expect("hello");
                }
                Payload::Command(command) => {
                    observed
                        .commands
                        .lock()
                        .expect("observed lock")
                        .push(command.body.clone());
                    let payload = match &command.body {
                        CommandBody::ListInbox { .. } => Payload::InboxPage {
                            command_id: command.command_id,
                            page: InboxPage {
                                items: vec![entry.clone()],
                                next_cursor: None,
                            },
                        },
                        CommandBody::MutateInbox { .. } => Payload::InboxEntryApplied {
                            command_id: command.command_id,
                            entry: inbox_entry(entry.id, InboxEntryState::Acknowledged),
                        },
                        CommandBody::QueryAnalytics { .. } => Payload::AnalyticsResults {
                            command_id: command.command_id,
                            page: AnalyticsPage {
                                items: vec![AnalyticsBucket {
                                    dimensions: vec!["gpt-5".to_string()],
                                    // Every metric absent: the daemon measured
                                    // none of them for this bucket, and nothing
                                    // downstream may turn that into a zero.
                                    metrics: serde_json::from_str::<AnalyticsMetrics>("{}")
                                        .expect("absent metrics"),
                                }],
                                next_cursor: None,
                            },
                        },
                        CommandBody::ExportAnalytics { .. } => Payload::AnalyticsExported {
                            command_id: command.command_id,
                            result: analytics_export_result(artifact.clone()),
                        },
                        CommandBody::ReadArtifact { offset, .. } => {
                            let start = usize::try_from(*offset).expect("offset");
                            let end = (start + CHUNK).min(contents.len());
                            Payload::ArtifactChunk {
                                artifact_id: artifact.id,
                                offset: *offset,
                                bytes_base64: base64::engine::general_purpose::STANDARD
                                    .encode(&contents[start..end]),
                                eof: end >= contents.len(),
                                sha256: artifact.sha256.clone(),
                            }
                        }
                        _ => Payload::CommandRejected(CodypendentError::new(
                            "test.unsupported",
                            "the test daemon does not implement this command",
                            false,
                        )),
                    };
                    write_envelope(&mut stream, &Envelope::reply_to(&request, payload))
                        .await
                        .expect("reply");
                }
                _ => {}
            }
        }
    }

    fn artifact_of(contents: &[u8]) -> ArtifactRef {
        ArtifactRef {
            id: ArtifactId::new(),
            media_type: "text/csv".to_string(),
            byte_length: contents.len() as u64,
            sha256: hex::encode(Sha256::digest(contents)),
            sensitivity: DataClassification::Internal,
        }
    }

    #[tokio::test]
    async fn inbox_and_analytics_commands_reach_the_daemon_and_return_its_replies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).expect("bind");
        let observed = Arc::new(Observed::default());
        let entry = inbox_entry(InboxEntryId::new(), InboxEntryState::Unread);
        let contents = b"run,cost\r\nrun-1,\r\n".to_vec();
        let artifact = artifact_of(&contents);
        tokio::spawn(serve_reads(
            listener,
            Arc::clone(&observed),
            entry.clone(),
            artifact.clone(),
            contents.clone(),
        ));

        let sink = Arc::new(Collector::default());
        let (client, _) = DaemonClient::connect(&path, None, sink)
            .await
            .expect("connect");

        let page = client
            .list_inbox(InboxListQuery::default())
            .await
            .expect("list inbox");
        assert_eq!(page.items, vec![entry.clone()]);

        let applied = client
            .mutate_inbox(InboxMutation::Acknowledge { entry_id: entry.id })
            .await
            .expect("mutate inbox");
        assert_eq!(applied.id, entry.id);
        assert_eq!(applied.state, InboxEntryState::Acknowledged);

        let analytics = client
            .query_analytics(AnalyticsQuery::default())
            .await
            .expect("query analytics");
        let metrics = &analytics.items.first().expect("one bucket").metrics;
        assert_eq!(
            (
                metrics.input_tokens,
                metrics.cost_micros,
                metrics.latency_ms
            ),
            (None, None, None),
            "a measurement the daemon reported as absent must not become a zero"
        );

        let export = client
            .export_analytics(AnalyticsExportRequest {
                query: AnalyticsQuery::default(),
                format: AnalyticsExportFormat::Csv,
                max_rows: 0,
            })
            .await
            .expect("export analytics");
        assert_eq!(export.artifact, artifact);

        // The export names an artifact; reading it pages the daemon's chunks
        // back into exactly the bytes the reference declares.
        let bytes = client
            .read_artifact(&export.artifact)
            .await
            .expect("read artifact");
        assert_eq!(bytes, contents);
        let saved = dir.path().join("report.csv");
        std::fs::write(&saved, "old export").unwrap();
        assert_eq!(
            client
                .save_artifact(&export.artifact, &saved)
                .await
                .unwrap(),
            contents.len() as u64
        );
        assert_eq!(std::fs::read(&saved).unwrap(), contents);

        let commands = observed.commands.lock().expect("observed lock").clone();
        for expected in [
            "ListInbox",
            "MutateInbox",
            "QueryAnalytics",
            "ExportAnalytics",
            "ReadArtifact",
        ] {
            assert!(
                commands
                    .iter()
                    .any(|command| format!("{command:?}").starts_with(expected)),
                "a real {expected} command reached the daemon"
            );
        }
        assert!(
            commands
                .iter()
                .filter(|command| matches!(command, CommandBody::ReadArtifact { .. }))
                .count()
                > 1,
            "an artifact larger than one chunk is paged, not truncated"
        );
    }

    #[tokio::test]
    async fn a_rejected_inbox_read_is_an_error_not_an_empty_page() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).expect("bind");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            while let Ok(Some(request)) = read_envelope(&mut stream).await {
                let payload = match &request.payload {
                    Payload::ClientHello(_) => Payload::ServerHello(server_hello()),
                    Payload::Command(_) => Payload::CommandRejected(CodypendentError::new(
                        "protocol.role-denied",
                        "this client may not read the inbox",
                        false,
                    )),
                    _ => continue,
                };
                write_envelope(&mut stream, &Envelope::reply_to(&request, payload))
                    .await
                    .expect("reply");
            }
        });

        let sink = Arc::new(Collector::default());
        let (client, _) = DaemonClient::connect(&path, None, sink)
            .await
            .expect("connect");

        let error = client
            .list_inbox(InboxListQuery::default())
            .await
            .expect_err("a refused inbox read must fail, never return an empty page");
        assert!(error.to_string().contains("protocol.role-denied"));

        let error = client
            .query_analytics(AnalyticsQuery::default())
            .await
            .expect_err("a refused analytics query must fail, never return an empty page");
        assert!(error.to_string().contains("protocol.role-denied"));
    }

    #[tokio::test]
    async fn artifact_bytes_that_do_not_match_the_reference_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_in(&dir);
        let listener = UnixListener::bind(&path).expect("bind");
        let observed = Arc::new(Observed::default());
        // Same length, different content: the daemon answers under the digest
        // the reference carries, so only hashing the assembled bytes catches it.
        let contents = b"the bytes actually stored here!!".to_vec();
        let claimed = artifact_of(b"the bytes the reference promises");
        assert_eq!(contents.len() as u64, claimed.byte_length);
        tokio::spawn(serve_reads(
            listener,
            observed,
            inbox_entry(InboxEntryId::new(), InboxEntryState::Unread),
            claimed.clone(),
            contents,
        ));

        let sink = Arc::new(Collector::default());
        let (client, _) = DaemonClient::connect(&path, None, sink)
            .await
            .expect("connect");

        let error = client
            .read_artifact(&claimed)
            .await
            .expect_err("bytes that do not match the reference must not be handed to the webview");
        let message = error.to_string();
        assert!(message.contains("digest"), "unexpected failure: {message}");
        let saved = dir.path().join("keep.csv");
        std::fs::write(&saved, "preserve me").unwrap();
        assert!(client.save_artifact(&claimed, &saved).await.is_err());
        assert_eq!(std::fs::read_to_string(saved).unwrap(), "preserve me");
    }
}

#[cfg(test)]
#[path = "daemon_board_tests.rs"]
mod board_tests;
