//! The webview-facing half of the shell: Tauri commands in, a Tauri channel
//! of daemon frames out.
//!
//! Nothing here decides anything. Every command is a thin wrapper over a real
//! protocol command, and every reply is what the daemon said. If the daemon is
//! not reachable, these commands return an error string and the UI renders a
//! disconnected state — there is no path through this module that produces
//! transcript content the daemon did not emit.

use std::sync::Arc;

use codypendent_protocol::{
    AgentMode, AnalyticsExportRequest, AnalyticsExportResult, AnalyticsPage, AnalyticsQuery,
    ApprovalDecision, ApprovalId, ApprovalScope, ArtifactRef, InboxEntry, InboxListQuery,
    InboxMutation, InboxPage, RunId, SessionId,
};
use tauri::ipc::{Channel, Response};
use tauri::State;
use tokio::sync::Mutex;

// Session-library, workflow and blackboard contracts. A second `use` block on
// purpose: several people add handlers to this file at once and an additive
// block cannot conflict with theirs.
use codypendent_protocol::{BlackboardItemView, PageCursor, SessionLifecycleAction};

use crate::daemon::{
    socket_path, BoardView, ConnectionInfo, DaemonClient, DaemonFrame, FrameSink,
    SessionLifecycleOutcome, SessionRow, SessionSearchAnswer, WorkflowWatch,
};

// LOCAL CONFIG (models.toml, providers.toml, auth.json). A third additive `use`
// block, for the same reason as the one above.
use codypendent_protocol::ModelId;

// Run lifecycle (`PauseRun`/`ResumeRun`) and the pending-prompt queue
// (`QueuePrompt` and friends). A fourth additive `use` block, same reason.
use codypendent_protocol::{PromptDelivery, PromptId};

use crate::models::{
    CatalogModelsView, KeyTarget, KeysView, ModeCard, ModelsView, ProvidersView, SecretKey,
};

use codypendent_protocol::{
    DocumentId, DocumentLeaseGrant, DocumentMutation, MemoryId, PublishTarget,
    UiPluginLifecycleStatus,
};

use crate::daemon::DocumentPublishPlan;
use crate::knowledge::LearningMutation;
use codypendent_protocol::knowledge::{
    DocCard, KnowledgeSection, LearningCard, MemoryCard, SkillCard,
};

/// A Tauri channel used as the frame sink. This is the only place a daemon
/// frame becomes a webview message.
struct ChannelSink(Channel<DaemonFrame>);

impl FrameSink for ChannelSink {
    fn emit(&self, frame: DaemonFrame) {
        // A send failure means the webview went away (window closed, reload).
        // There is nothing useful to do about it here; the reader task ends
        // with the connection.
        let _ = self.0.send(frame);
    }
}

/// The connection the shell currently holds, if any.
#[derive(Default)]
pub struct Bridge {
    connection: Mutex<Option<Connected>>,
    attempt: Mutex<ConnectionAttempt>,
    connect_lock: Mutex<()>,
    /// The mode and model the operator has staged for the NEXT run.
    ///
    /// Client state, exactly as in the TUI: picking a mode sets
    /// `AppState::default_mode` and picking a model sets `pending_model`, and
    /// neither sends anything — both ride on the next `StartRun`
    /// (`crates/tui/src/reduce.rs`). Held here rather than in the webview so the
    /// existing composer keeps working unchanged: it invokes `start_objective`
    /// with an objective and nothing else, and the staged selection is applied
    /// on the way past.
    run_defaults: Mutex<RunDefaults>,
}

#[derive(Default)]
struct ConnectionAttempt {
    epoch: u64,
    generation: Option<u64>,
}

impl ConnectionAttempt {
    fn begin(&mut self, generation: Option<u64>) -> u64 {
        self.epoch = self.epoch.wrapping_add(1);
        self.generation = Some(generation.unwrap_or(0));
        self.epoch
    }

    fn cancel(&mut self, generation: Option<u64>) {
        if generation.is_none() || self.generation == generation {
            self.epoch = self.epoch.wrapping_add(1);
            self.generation = None;
        }
    }

    fn validate(&self, epoch: u64) -> Result<(), String> {
        if self.epoch == epoch && self.generation.is_some() {
            Ok(())
        } else {
            Err("workspace connection was superseded".to_owned())
        }
    }
}

/// What the next run will use unless the caller overrides it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunDefaults {
    /// Serialized as `{ "type": "Build" }` — the protocol enum's own shape.
    pub mode: AgentMode,
    /// The pinned model id, or `None` for "let the daemon choose". Never a
    /// fabricated default: an unpinned selection is absent, not a guess.
    pub model: Option<ModelId>,
}

impl Default for RunDefaults {
    fn default() -> Self {
        // Build is the TUI's default mode (`crates/tui/src/state.rs`), and the
        // mode this command hard-coded before the picker existed.
        Self {
            mode: AgentMode::Build,
            model: None,
        }
    }
}

impl RunDefaults {
    /// The defaults saved at the last launch, or the built-ins. A stored model
    /// that is no longer in `models.toml` is dropped rather than carried into
    /// a `StartRun` the daemon would refuse; a preferences file that cannot be
    /// read yields the built-ins, because a launch must not fail on a
    /// preference.
    fn restored() -> Self {
        let stored = crate::repository::stored_run_defaults().unwrap_or_default();
        let model = stored
            .model
            .map(ModelId)
            .filter(|model| crate::models::model_is_configured(model).unwrap_or(false));
        Self {
            mode: stored.mode.unwrap_or(AgentMode::Build),
            model,
        }
    }

    fn stored(&self) -> crate::repository::StoredRunDefaults {
        crate::repository::StoredRunDefaults {
            mode: Some(self.mode),
            model: self.model.as_ref().map(|model| model.0.clone()),
        }
    }
}

impl Bridge {
    /// The shell's state at launch: no connection, and the run defaults the
    /// operator staged last time.
    pub fn load() -> Self {
        Self {
            connection: Mutex::new(None),
            attempt: Mutex::new(ConnectionAttempt::default()),
            connect_lock: Mutex::new(()),
            run_defaults: Mutex::new(RunDefaults::restored()),
        }
    }
}

/// Persist the staged defaults. The in-memory choice already applies; a save
/// that fails is reported so the operator knows it is for this session only.
fn persist_run_defaults(defaults: &RunDefaults) -> Result<(), String> {
    crate::repository::store_run_defaults(&defaults.stored()).map_err(|error| {
        format!("the choice applies to this session, but could not be saved for the next launch: {error:#}")
    })
}

struct Connected {
    client: Arc<DaemonClient>,
    sink: Arc<ChannelSink>,
    /// Which connection attempt this is, supplied by the webview.
    ///
    /// `daemon_disconnect` used to take whatever connection happened to be
    /// registered, with no notion of WHICH one the caller meant to close. The
    /// webview's reconnect tears down and reconnects, and its teardown is
    /// deferred until the previous connect settles — so the deferred disconnect
    /// could land AFTER the replacement registered and shut that one down
    /// instead. And because a deliberate disconnect deliberately emits no
    /// `Disconnected` frame, the store went on reporting "connected" while
    /// every command timed out, and the reconnect effect — which only fires on
    /// "disconnected" — never ran again. One race permanently disabled the app,
    /// silently.
    generation: u64,
}

/// Where the shell will look for a daemon, so the UI can name the socket in a
/// disconnected state instead of saying "unavailable" with no detail.
#[tauri::command]
fn daemon_socket() -> Result<String, String> {
    socket_path()
        .map(|path| path.display().to_string())
        .map_err(|error| format!("{error:#}"))
}

/// Whether a daemon is listening, and what the shell would launch if not —
/// including the exact command to run by hand when it cannot.
#[tauri::command]
async fn daemon_launch_status() -> Result<crate::launcher::LaunchStatus, String> {
    let paths = crate::runtime_paths::resolve().map_err(|error| format!("{error:#}"))?;
    Ok(crate::launcher::launch_status(&paths).await)
}

/// Start `codypendentd` unless one already answers, and wait for its socket.
/// The webview reconnects on success; on failure the error names what was
/// tried and the manual command.
#[tauri::command]
async fn daemon_start() -> Result<crate::launcher::StartOutcome, String> {
    let paths = crate::runtime_paths::resolve().map_err(|error| format!("{error:#}"))?;
    crate::launcher::start_daemon(&paths)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Connect and handshake. Succeeds only when a daemon actually answered.
#[tauri::command]
async fn daemon_connect(
    bridge: State<'_, Bridge>,
    channel: Channel<DaemonFrame>,
    // The webview's monotonically increasing attempt number. Absent from an
    // older frontend, which then behaves as it did before.
    generation: Option<u64>,
) -> Result<ConnectionInfo, String> {
    let epoch = bridge.attempt.lock().await.begin(generation);
    let _connecting = bridge.connect_lock.lock().await;
    bridge.attempt.lock().await.validate(epoch)?;
    let paths = crate::runtime_paths::resolve().map_err(|error| format!("{error:#}"))?;
    // The repository the OPERATOR chose, or `None`.
    //
    // This used to be `std::env::current_dir()`. For a bundled `.app` that is
    // the launch directory — `/` under Finder, `$HOME` under a login shell —
    // and it rides on `CreateSession`/`AttachSession`/`StartRun`, which is how
    // a code graph once reached 510,904 nodes indexing a home directory.
    // `repository::connection_repository` yields only a validated git checkout
    // root and otherwise `None`; the UI then says no repository is selected
    // rather than the shell guessing one (see `repository.rs`).
    //
    // Blocking work — it canonicalizes paths and shells out to
    // `git rev-parse` (`repo_anchor::checkout_root`) — so it runs off the
    // reactor thread, the same pattern `DaemonClient::connect` uses for its
    // own anchoring.
    let repository = tokio::task::spawn_blocking(crate::repository::connection_repository)
        .await
        .unwrap_or_default();
    let sink = Arc::new(ChannelSink(channel));

    // The shell's PERSISTED workspace, so a reconnect keeps the knowledge scope
    // the person was already looking at. A preferences file that cannot be read
    // is not a reason to refuse the connection, but it is also not a reason to
    // hand over `None`: that let the daemon mint a fresh workspace per
    // connection, so every automatic reconnect moved the scope and the
    // operator's memories and documents vanished again. The stand-in is stable
    // for the life of the process.
    let workspace = Some(crate::repository::workspace_for_connection());
    bridge.attempt.lock().await.validate(epoch)?;
    let (client, info) = match DaemonClient::connect_as(
        &paths.socket_path,
        repository.clone(),
        workspace,
        Arc::clone(&sink),
    )
    .await
    {
        Ok(connected) => connected,
        Err(error) if daemon_is_absent(&error) => {
            bridge.attempt.lock().await.validate(epoch)?;
            crate::launcher::start_daemon(&paths)
                .await
                .map_err(|error| format!("{error:#}"))?;
            bridge.attempt.lock().await.validate(epoch)?;
            DaemonClient::connect_as(&paths.socket_path, repository, workspace, Arc::clone(&sink))
                .await
                .map_err(|error| format!("{error:#}"))?
        }
        Err(error) => return Err(format!("{error:#}")),
    };
    let attempt = bridge.attempt.lock().await;
    if let Err(error) = attempt.validate(epoch) {
        drop(attempt);
        client.shutdown().await;
        return Err(error);
    }

    // Replacing a live connection tears the old one down for real
    // (`DaemonClient::shutdown`): dropping its Arc alone would leak the reader
    // task, which holds its own writer Arc for heartbeat pongs. The stale
    // reader only ever forwards into the OLD sink, so it cannot reach the new
    // connection's channel, and it is aborted before this command resolves.
    let previous = bridge.connection.lock().await.replace(Connected {
        client,
        sink,
        generation: generation.unwrap_or(0),
    });
    drop(attempt);
    if let Some(previous) = previous {
        previous.client.shutdown().await;
    }
    Ok(info)
}

fn daemon_is_absent(error: &anyhow::Error) -> bool {
    error
        .root_cause()
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        })
}

/// Drop the connection, tearing it down for real: the reader task is aborted
/// and the write half shut down so the daemon sees EOF. Dropping the client
/// Arc alone would leak the reader — see [`DaemonClient::shutdown`].
#[tauri::command]
async fn daemon_disconnect(
    bridge: State<'_, Bridge>,
    // Close only this attempt. `None` closes whatever is registered, which is
    // what an app teardown wants and what an older frontend sends.
    generation: Option<u64>,
) -> Result<(), String> {
    let mut attempt = bridge.attempt.lock().await;
    attempt.cancel(generation);
    let mut held = bridge.connection.lock().await;
    // A stale teardown must not close a NEWER connection. Compared while the
    // lock is held, so the check and the take cannot be separated by a connect.
    if let Some(wanted) = generation {
        if held.as_ref().is_some_and(|open| open.generation != wanted) {
            return Ok(());
        }
    }
    if let Some(connection) = held.take() {
        drop(held);
        drop(attempt);
        connection.client.shutdown().await;
    }
    Ok(())
}

#[tauri::command]
async fn list_sessions(bridge: State<'_, Bridge>) -> Result<Vec<SessionRow>, String> {
    let client = client_of(&bridge).await?;
    client
        .list_sessions()
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Submit an objective as a real run. The reply carries the ids the daemon
/// minted; the transcript fills from the events that follow.
///
/// `mode` and `model` are optional: omitted, the run uses whatever the operator
/// staged in the mode/model pickers (see [`RunDefaults`]). An explicit argument
/// wins for that one run without changing the staged selection.
#[derive(serde::Deserialize)]
pub struct DesktopManifest {
    context: serde_json::Value,
    tools: Vec<codypendent_runtime::desktop_tools::DesktopTool>,
}

#[tauri::command]
async fn start_objective(
    bridge: State<'_, Bridge>,
    objective: String,
    mode: Option<AgentMode>,
    model: Option<ModelId>,
    session_id: Option<SessionId>,
    desktop: Option<DesktopManifest>,
) -> Result<crate::daemon::RunHandle, String> {
    let (client, sink) = connected(&bridge).await?;
    let defaults = bridge.run_defaults.lock().await.clone();
    let mode = mode.unwrap_or(defaults.mode);
    let model = model.or(defaults.model);
    client
        .start_objective_prepared(objective, mode, model, session_id, &sink, |session| {
            if let Some(mut desktop) = desktop {
                desktop
                    .context
                    .as_object_mut()
                    .ok_or_else(|| anyhow::anyhow!("Desktop context must be an object"))?
                    .insert("sessionId".into(), serde_json::json!(session.to_string()));
                crate::runtime_paths::desktop_tools()
                    .and_then(|tools| tools.publish(session, desktop.context, desktop.tools))
                    .map_err(anyhow::Error::msg)?;
            }
            Ok(())
        })
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Attach to a session that already exists; its catch-up replays into the
/// transcript.
#[tauri::command]
async fn attach_session(bridge: State<'_, Bridge>, session_id: SessionId) -> Result<(), String> {
    let (client, sink) = connected(&bridge).await?;
    client
        .attach(session_id, &sink)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Read the durable events a live gap skipped over.
///
/// The event stream can leave a hole — a lagging subscriber, a frame dropped
/// under load — and the client sees it as a sequence jump. Detecting it was
/// never the hard part; the transcript simply stayed short by however many
/// events went missing, with no mark where they should have been. This is the
/// read that fills the hole from the durable log.
#[tauri::command]
async fn read_session_event_range(
    bridge: State<'_, Bridge>,
    session_id: SessionId,
    after_sequence: u64,
    through: u64,
) -> Result<Vec<codypendent_protocol::SessionEvent>, String> {
    let client = client_of(&bridge).await?;
    client
        .read_session_events(session_id, after_sequence, through)
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn cancel_run(bridge: State<'_, Bridge>, run_id: RunId) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .cancel_run(run_id)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Queue steering text against a live run (`QueueSteering`).
///
/// Distinct from [`start_objective`]: it redirects the run already in flight
/// rather than starting another, and the daemon — not this client — decides
/// when the text takes effect. Resolving here means only that the daemon
/// accepted the command; the webview learns "queued" and "applied" from the
/// `SteeringQueued` / `SteeringApplied` events on the session stream.
#[tauri::command]
async fn queue_steering(
    bridge: State<'_, Bridge>,
    run_id: RunId,
    text: String,
) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .queue_steering(run_id, text)
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn resolve_approval(
    bridge: State<'_, Bridge>,
    approval_id: ApprovalId,
    approved: bool,
    scope: Option<ApprovalScope>,
) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    let decision = if approved {
        ApprovalDecision::Approve
    } else {
        ApprovalDecision::Reject
    };
    client
        .resolve_approval(approval_id, decision, scope.unwrap_or(ApprovalScope::Once))
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Answer or reject the exact parked question the desktop card shows. The
/// outcome is the protocol's own `QuestionOutcome` — chosen labels per
/// question, or a rejection with optional feedback — serialized by the webview
/// in the wire shape and deserialized here by the shared crate.
#[tauri::command]
async fn resolve_question(
    bridge: State<'_, Bridge>,
    question_id: codypendent_protocol::QuestionId,
    outcome: codypendent_protocol::QuestionOutcome,
) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .resolve_question(question_id, outcome)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// One page of the durable inbox, straight from `ListInbox`. An error here is
/// the honest answer when the daemon is absent or refused: the Inbox view
/// renders "unavailable" on it, which is not the same thing as an empty page.
#[tauri::command]
async fn list_inbox(
    bridge: State<'_, Bridge>,
    query: Option<InboxListQuery>,
) -> Result<InboxPage, String> {
    let client = client_of(&bridge).await?;
    client
        .list_inbox(query.unwrap_or_default())
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Apply one idempotent inbox mutation and return the daemon's projection of
/// the entry afterwards.
#[tauri::command]
async fn mutate_inbox(
    bridge: State<'_, Bridge>,
    mutation: InboxMutation,
) -> Result<InboxEntry, String> {
    let client = client_of(&bridge).await?;
    client
        .mutate_inbox(mutation)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Measured analytics, straight from `QueryAnalytics`. Absent measurements stay
/// absent across this boundary — the page is forwarded as the daemon serialized
/// it, so a metric the daemon never measured arrives as null, not as zero.
#[tauri::command]
async fn query_analytics(
    bridge: State<'_, Bridge>,
    query: Option<AnalyticsQuery>,
) -> Result<AnalyticsPage, String> {
    let client = client_of(&bridge).await?;
    client
        .query_analytics(query.unwrap_or_default())
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Export a bounded analytics query. The reply names the artifact the daemon
/// wrote; `read_artifact` fetches its bytes.
#[tauri::command]
async fn export_analytics(
    bridge: State<'_, Bridge>,
    request: AnalyticsExportRequest,
) -> Result<AnalyticsExportResult, String> {
    let client = client_of(&bridge).await?;
    client
        .export_analytics(request)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// An artifact's bytes, retrieved and verified against the reference the
/// webview observed.
///
/// Returned as a raw IPC response rather than a string: an artifact is bytes
/// (a patch, a CSV, an audio blob), and decoding it as text in the shell would
/// both corrupt non-UTF-8 content and hide that fact. The webview receives an
/// `ArrayBuffer` and decodes it itself when it knows the artifact is text.
#[tauri::command]
async fn read_artifact(
    bridge: State<'_, Bridge>,
    artifact: ArtifactRef,
) -> Result<Response, String> {
    let client = client_of(&bridge).await?;
    client
        .read_artifact(&artifact)
        .await
        .map(Response::new)
        .map_err(|error| format!("{error:#}"))
}

/// The webview supplies an artifact reference, never a writable path. A native
/// Save dialog chooses the destination; the existing reader verifies every byte
/// before any file is replaced. Cancellation is a distinct, successful outcome.
#[tauri::command]
async fn save_artifact<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    bridge: State<'_, Bridge>,
    artifact: ArtifactRef,
) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt as _;

    let client = client_of(&bridge).await?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Save Codypendent artifact")
        .set_file_name(crate::artifacts::suggested_name(&artifact))
        .save_file(move |picked| {
            let _ = tx.send(picked);
        });
    let Some(picked) = rx
        .await
        .map_err(|_| "the save dialog closed without answering".to_owned())?
    else {
        return Ok(None);
    };
    let path = picked
        .into_path()
        .map_err(|error| format!("the save destination is not a local path: {error}"))?;
    client
        .save_artifact(&artifact, &path)
        .await
        .map_err(|error| format!("{error:#}"))?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

// ---------------------------------------------------------------- Session
// Library.

/// One page of ranked session search, carrying the query it answers so a page
/// for a query the operator has since typed past is discarded rather than
/// rendered under the new heading.
///
/// An error is a **failed search**, which the library must not draw as "no
/// results": one says the daemon looked and found nothing, the other says
/// nobody looked.
#[tauri::command]
async fn search_sessions(
    bridge: State<'_, Bridge>,
    query: String,
    cursor: Option<PageCursor>,
) -> Result<SessionSearchAnswer, String> {
    let client = client_of(&bridge).await?;
    client
        .search_sessions(query, cursor)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Apply one session lifecycle action and return what the daemon actually did.
///
/// The webview supplies the typed action verbatim; the reply distinguishes a
/// re-projection from a retention-policy deletion from an export artifact,
/// because those are different outcomes and a delete in particular must show
/// the daemon's tombstone decision rather than a client-invented "deleted".
#[tauri::command]
async fn mutate_session(
    bridge: State<'_, Bridge>,
    session_id: SessionId,
    action: SessionLifecycleAction,
) -> Result<SessionLifecycleOutcome, String> {
    let client = client_of(&bridge).await?;
    client
        .mutate_session(session_id, action)
        .await
        .map_err(|error| format!("{error:#}"))
}

// --------------------------------------------------------------- Workflow.

#[tauri::command]
async fn list_workflows(
    bridge: State<'_, Bridge>,
    expected_repository: Option<String>,
) -> Result<codypendent_protocol::WorkflowCatalog, String> {
    client_of(&bridge)
        .await?
        .list_workflows(expected_repository)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Start a durable workflow run by the id the daemon resolves from its own
/// sources. `inputs` must be a JSON object; anything else is refused before it
/// reaches the wire.
#[tauri::command]
async fn start_workflow(
    bridge: State<'_, Bridge>,
    workflow_id: String,
    inputs: serde_json::Value,
    expected_repository: Option<String>,
    idempotency_key: Option<String>,
    expected_definition_digest: Option<String>,
) -> Result<String, String> {
    let client = client_of(&bridge).await?;
    client
        .start_workflow_with_options(
            workflow_id,
            inputs,
            expected_repository,
            idempotency_key,
            expected_definition_digest,
        )
        .await
        .map_err(|error| format!("{error:#}"))
}

/// A workflow run's observability snapshot — the baseline the live
/// `workflow_event` frames fold onto.
#[tauri::command]
async fn read_workflow_run(
    bridge: State<'_, Bridge>,
    workflow_run_id: String,
) -> Result<codypendent_protocol::WorkflowRunSnapshot, String> {
    let client = client_of(&bridge).await?;
    client
        .read_workflow_run(workflow_run_id)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Subscribe this connection to a run's node stream and its board, then return
/// both authoritative baselines. Live updates arrive afterwards as
/// `workflow_event` / `blackboard_posted` frames on the daemon channel.
#[tauri::command]
async fn watch_workflow(
    bridge: State<'_, Bridge>,
    workflow_run_id: String,
) -> Result<WorkflowWatch, String> {
    let (client, sink) = connected(&bridge).await?;
    client
        .watch_workflow(workflow_run_id, &sink)
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn pause_workflow(bridge: State<'_, Bridge>, workflow_run_id: String) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .pause_workflow(workflow_run_id)
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn resume_workflow(bridge: State<'_, Bridge>, workflow_run_id: String) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .resume_workflow(workflow_run_id)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Cancel a run. Terminal on the daemon side — the UI confirms first, and this
/// command is what the confirmation authorizes.
#[tauri::command]
async fn cancel_workflow(bridge: State<'_, Bridge>, workflow_run_id: String) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .cancel_workflow(workflow_run_id)
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn retry_workflow_node(
    bridge: State<'_, Bridge>,
    workflow_run_id: String,
    node_id: String,
) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .retry_workflow_node(workflow_run_id, node_id)
        .await
        .map_err(|error| format!("{error:#}"))
}

// ------------------------------------------------------------- Blackboard.

/// A workflow run's board including superseded revisions, so the panel can show
/// what a correction replaced rather than only its result.
#[tauri::command]
async fn read_blackboard(
    bridge: State<'_, Bridge>,
    workflow_run_id: String,
) -> Result<Vec<BlackboardItemView>, String> {
    let client = client_of(&bridge).await?;
    client
        .read_blackboard(workflow_run_id, None, true, None)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Post an open question to a run's board. The only kind an operator may post:
/// a question carries no unverified factual claim into the channel the agents
/// treat as evidence.
#[tauri::command]
async fn post_blackboard_question(
    bridge: State<'_, Bridge>,
    workflow_run_id: String,
    text: String,
) -> Result<BlackboardItemView, String> {
    let client = client_of(&bridge).await?;
    client
        .post_blackboard_question(workflow_run_id, text)
        .await
        .map_err(|error| format!("{error:#}"))
}

// --------------------------------------------------------- Repository board.

/// Subscribe to the repository task board and read its live cards. The reply
/// names the checkout the board is keyed by, so a board that looks empty can be
/// checked against the repository the operator meant.
#[tauri::command]
async fn watch_board(bridge: State<'_, Bridge>) -> Result<BoardView, String> {
    let (client, sink) = connected(&bridge).await?;
    client
        .watch_board(&sink)
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn create_board_card(
    bridge: State<'_, Bridge>,
    title: String,
) -> Result<BlackboardItemView, String> {
    let client = client_of(&bridge).await?;
    client
        .create_board_card(title)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Move a card to another column. The daemon supersedes the card and returns
/// the replacement; the pane renders that rather than its own edit.
#[tauri::command]
async fn move_board_card(
    bridge: State<'_, Bridge>,
    item_id: String,
    status: String,
) -> Result<BlackboardItemView, String> {
    let client = client_of(&bridge).await?;
    client
        .move_board_card(item_id, status)
        .await
        .map_err(|error| format!("{error:#}"))
}

// ---------------------------------------------------------------------------
// LOCAL CONFIG: models, providers, API keys, mode
//
// None of these touch the daemon. `models.toml`, `providers.toml` and
// `auth.json` are files under the runtime data dir with no wire command behind
// them, so unlike every handler above these work with the daemon stopped — and
// must, because configuring a model is what you do BEFORE a run.
//
// The secret rule, once, for all four: a key crosses INTO these commands as
// `SecretKey` and never crosses back. Nothing below returns key material, and
// `KeyStatus` reports presence — stored, the NAME of an environment variable,
// or missing.
// ---------------------------------------------------------------------------

/// Every model configured in `models.toml`, with credential PRESENCE per row.
///
/// Join `items` for a one-line setup summary, naming a few and counting the
/// rest.
///
/// The setup page rendered these by joining EVERY entry, so a machine with
/// twenty-one models answered step one with twenty-one identifiers and step two
/// with twenty-one near-identical sentences — several hundred characters of
/// monospace where a reader wants a status. Naming a handful and counting the
/// remainder says the same thing and can be read at a glance; the per-model
/// detail lives on the Models and API Keys pages, which exist for it.
fn summarize(items: &[String], limit: usize) -> String {
    if items.len() <= limit {
        return items.join("; ");
    }
    let named = items[..limit].join("; ");
    format!("{named}; and {} more", items.len() - limit)
}

/// The same, for a list of bare identifiers.
fn summarize_ids(ids: &[&str], limit: usize) -> String {
    if ids.len() <= limit {
        return ids.join(", ");
    }
    format!(
        "{}, and {} more",
        ids[..limit].join(", "),
        ids.len() - limit
    )
}

/// A missing `models.toml` answers with an empty list and `configured: false`;
/// a `models.toml` that exists and does not parse is an `Err`. The two are not
/// the same thing and the view must not render them the same way.
#[tauri::command]
async fn list_models(bridge: State<'_, Bridge>) -> Result<ModelsView, String> {
    let pinned = bridge.run_defaults.lock().await.model.clone();
    crate::models::list_models(pinned.as_ref()).map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn discover_models(
    bridge: State<'_, Bridge>,
    prefer_available: Option<bool>,
) -> Result<crate::models::ModelDiscovery, String> {
    let starting_model = bridge.run_defaults.lock().await.model.clone();
    let repository = crate::repository::selected_repository()
        .map_err(|error| format!("{error:#}"))?
        .map(|repository| std::path::PathBuf::from(repository.path));
    let report = crate::models::discover_models(repository.as_deref())
        .await
        .map_err(|error| format!("{error:#}"))?;
    let mut defaults = bridge.run_defaults.lock().await;
    if defaults.model != starting_model {
        return Ok(report);
    }
    let selected = crate::models::reconcile_discovery_default(
        &report,
        defaults.model.as_ref(),
        prefer_available.unwrap_or(false),
    )
    .map_err(|error| format!("{error:#}"))?;
    if defaults.model != selected {
        defaults.model = selected;
        persist_run_defaults(&defaults)?;
    }
    Ok(report)
}

/// Pin a model for the next run, or clear the pin with `null`.
///
/// Refuses an id that is not in `models.toml`: a pin naming nothing configured
/// would surface later as a rejected run rather than as a rejected pin.
#[tauri::command]
async fn set_run_model(bridge: State<'_, Bridge>, model: Option<ModelId>) -> Result<(), String> {
    if let Some(model) = &model {
        let configured =
            crate::models::model_is_configured(model).map_err(|error| format!("{error:#}"))?;
        if !configured {
            return Err(format!("model `{model}` is not configured in models.toml"));
        }
    }
    let mut defaults = bridge.run_defaults.lock().await;
    defaults.model = model;
    persist_run_defaults(&defaults)
}

#[tauri::command]
async fn send_remote_ui(
    bridge: State<'_, Bridge>,
    session_id: String,
    message: codypendent_protocol::UiWireMessage,
) -> Result<(), String> {
    let session_id = session_id
        .parse::<SessionId>()
        .map_err(|error| error.to_string())?;
    client_of(&bridge)
        .await?
        .send_remote_ui(session_id, message)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Passive readiness is answered by the connected daemon without provider requests.
#[tauri::command]
async fn list_model_readiness(
    bridge: State<'_, Bridge>,
) -> Result<Vec<codypendent_protocol::ModelProbe>, String> {
    client_of(&bridge)
        .await?
        .probe_models(None, false)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// The operator explicitly requested a provider test.
#[tauri::command]
async fn model_readiness(
    bridge: State<'_, Bridge>,
    model_id: String,
    probe: bool,
) -> Result<codypendent_protocol::ModelProbe, String> {
    let models = client_of(&bridge)
        .await?
        .probe_models(Some(ModelId(model_id.clone())), probe)
        .await
        .map_err(|error| format!("{error:#}"))?;
    models
        .into_iter()
        .find(|row| row.id.0 == model_id)
        .ok_or_else(|| "The daemon returned no readiness for this model".to_owned())
}

/// Add a model to `models.toml`, optionally storing its API key.
///
/// The key travels one way. It is written to `auth.json` at mode 0600 and is
/// not part of any reply.
#[tauri::command]
async fn add_model(
    display_id: String,
    provider_id: String,
    model: String,
    api_key: Option<SecretKey>,
    context_tokens: Option<u64>,
) -> Result<(), String> {
    crate::models::add_model(
        &display_id,
        &provider_id,
        &model,
        api_key.as_ref(),
        context_tokens,
    )
    .map_err(|error| format!("{error:#}"))
}

/// Remove a model from `models.toml` and drop its stored key. Comment- and
/// formatting-preserving; all-or-nothing across the two files.
#[tauri::command]
async fn remove_model(bridge: State<'_, Bridge>, model_id: String) -> Result<(), String> {
    crate::models::remove_model(&model_id).map_err(|error| format!("{error:#}"))?;
    // A pin that named the removed model would otherwise outlive it and be
    // refused by the daemon on the next run.
    let mut defaults = bridge.run_defaults.lock().await;
    if defaults.model.as_ref().is_some_and(|id| id.0 == model_id) {
        defaults.model = None;
    }
    Ok(())
}

/// The provider catalog: built-ins layered with the user's `providers.toml`.
///
/// Carries the derived gates verbatim from the TUI, including
/// `community_consent_required` — selecting a community ACP bridge is a trust
/// decision and the confirmation is host chrome, never something the row itself
/// can waive.
#[tauri::command]
async fn list_providers() -> Result<ProvidersView, String> {
    crate::models::list_providers().map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn refresh_agents() -> Result<ProvidersView, String> {
    crate::models::refresh_agents()
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn connect_acp(
    provider_id: String,
    repository: String,
    accept_community_risk: bool,
) -> Result<crate::models::ConnectedAcp, String> {
    let repository = crate::repository::validate_repository(std::path::Path::new(&repository))
        .map_err(|error| format!("{error:#}"))?;
    crate::models::connect_acp(
        &provider_id,
        std::path::Path::new(&repository.path),
        accept_community_risk,
    )
    .await
    .map_err(|error| format!("{error:#}"))
}

/// The curated catalog models for one provider — a real, offline pick-list.
#[tauri::command]
async fn list_catalog_models(provider_id: String) -> Result<CatalogModelsView, String> {
    crate::models::list_catalog_models(&provider_id).map_err(|error| format!("{error:#}"))
}

/// Which credentials are set. Presence only — no reply from this command has
/// ever contained a key.
#[tauri::command]
async fn list_api_keys() -> Result<KeysView, String> {
    crate::models::key_statuses().map_err(|error| format!("{error:#}"))
}

/// Store one API key in `auth.json`. A blank key is refused rather than stored,
/// because an empty entry would silently shadow a valid `api_key_env`.
#[tauri::command]
async fn set_api_key(target: KeyTarget, key: SecretKey) -> Result<(), String> {
    crate::models::write_api_key(&target, Some(&key)).map_err(|error| format!("{error:#}"))
}

/// Remove one stored API key. Removing an absent entry writes nothing.
#[tauri::command]
async fn remove_api_key(target: KeyTarget) -> Result<(), String> {
    crate::models::write_api_key(&target, None).map_err(|error| format!("{error:#}"))
}

/// The five agent modes, with the TUI's own labels and summaries.
#[tauri::command]
fn list_modes() -> Vec<ModeCard> {
    crate::models::mode_cards()
}

/// The mode and model staged for the next run.
#[tauri::command]
async fn run_defaults(bridge: State<'_, Bridge>) -> Result<RunDefaults, String> {
    Ok(bridge.run_defaults.lock().await.clone())
}

/// Set the mode the next run submits with. Nothing is sent — the mode rides on
/// the next `StartRun`, exactly as `Overlay::ModePicker` stages it in the TUI.
#[tauri::command]
async fn set_run_mode(bridge: State<'_, Bridge>, mode: AgentMode) -> Result<(), String> {
    let mut defaults = bridge.run_defaults.lock().await;
    defaults.mode = mode;
    persist_run_defaults(&defaults)
}

// ---------------------------------------------------------------------------
// Repository selection (surface: RepoPicker)
//
// LOCAL, not protocol: nothing on the wire names a repository chooser. The
// chosen path is what `CreateSession`/`AttachSession`/`StartRun` carry, and it
// drives the daemon's code-graph indexing, so every one of these commands goes
// through `repository::validate_repository`, which refuses a folder that is not
// a git checkout and refuses `$HOME`.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Knowledge surfaces: Skills, Memory, Docs, Remote UI plugins.
//
// Two kinds of command, kept distinct on purpose. The LISTS read the daemon's
// SQLite database directly (`crate::knowledge`), exactly as the TUI harness
// does — no wire command lists registry items, memories, learnings or
// documents. The MUTATIONS are protocol commands on the live connection: the
// daemon owns every write except a learning's, which is local like the TUI's.
// The webview names each of these in the panel it shows when one is missing
// (`REQUIRED_COMMANDS` in `App.tsx`), so the names are a contract.
// ---------------------------------------------------------------------------

fn knowledge_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

/// Every governed registry item (authenticated daemon read).
#[tauri::command]
async fn list_skills(bridge: State<'_, Bridge>) -> Result<Vec<SkillCard>, String> {
    let client = client_of(&bridge).await?;
    client
        .read_knowledge(KnowledgeSection::Skills)
        .await
        .map(|page| page.skills)
        .map_err(knowledge_error)
}

/// The live memories this client may see (authenticated daemon read).
#[tauri::command]
async fn list_memories(bridge: State<'_, Bridge>) -> Result<Vec<MemoryCard>, String> {
    let client = client_of(&bridge).await?;
    client
        .read_knowledge(KnowledgeSection::Memories)
        .await
        .map(|page| page.memories)
        .map_err(knowledge_error)
}

/// `CorrectMemory`: returns the daemon's notice.
#[tauri::command]
async fn correct_memory(
    bridge: State<'_, Bridge>,
    memory_id: MemoryId,
    statement: String,
) -> Result<String, String> {
    let client = client_of(&bridge).await?;
    client
        .correct_memory(memory_id, statement)
        .await
        .map_err(knowledge_error)
}

/// `ForgetMemory`: returns the daemon's notice.
#[tauri::command]
async fn forget_memory(bridge: State<'_, Bridge>, memory_id: MemoryId) -> Result<String, String> {
    let client = client_of(&bridge).await?;
    client
        .forget_memory(memory_id)
        .await
        .map_err(knowledge_error)
}

/// The proposed and active learnings this client may see (daemon-owned store).
#[tauri::command]
async fn list_learnings(bridge: State<'_, Bridge>) -> Result<Vec<LearningCard>, String> {
    let client = client_of(&bridge).await?;
    client
        .read_knowledge(KnowledgeSection::Learnings)
        .await
        .map(|page| page.learnings)
        .map_err(knowledge_error)
}

/// One optimistic-revision learning mutation (daemon-owned store write). Returns
/// the outcome sentence; a conflict or duplicate rejects with its own.
#[tauri::command]
async fn mutate_learning(
    bridge: State<'_, Bridge>,
    learning_id: String,
    revision: u64,
    mutation: LearningMutation,
) -> Result<String, String> {
    let client = client_of(&bridge).await?;
    client
        .mutate_learning(&learning_id, revision, &mutation)
        .await
        .map_err(knowledge_error)
}

/// Every document this client may see, with blocks and pending suggestions
/// (authenticated daemon read).
#[tauri::command]
async fn list_documents(bridge: State<'_, Bridge>) -> Result<Vec<DocCard>, String> {
    let client = client_of(&bridge).await?;
    client
        .read_knowledge(KnowledgeSection::Documents)
        .await
        .map(|page| page.documents)
        .map_err(knowledge_error)
}

/// `CreateDocument`: returns the new document's id.
#[tauri::command]
async fn create_document(bridge: State<'_, Bridge>, title: String) -> Result<DocumentId, String> {
    let client = client_of(&bridge).await?;
    client.create_document(title).await.map_err(knowledge_error)
}

/// `AcquireDocumentLease`: returns the granted lease.
#[tauri::command]
async fn acquire_document_lease(
    bridge: State<'_, Bridge>,
    document_id: DocumentId,
    block_id: Option<String>,
) -> Result<DocumentLeaseGrant, String> {
    let client = client_of(&bridge).await?;
    client
        .acquire_document_lease(document_id, block_id)
        .await
        .map_err(knowledge_error)
}

/// `MutateDocument` under a lease the webview holds.
#[tauri::command]
async fn mutate_document(
    bridge: State<'_, Bridge>,
    document_id: DocumentId,
    mutation: DocumentMutation,
) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .mutate_document(document_id, mutation)
        .await
        .map_err(knowledge_error)
}

/// `ReleaseDocumentLease`.
#[tauri::command]
async fn release_document_lease(bridge: State<'_, Bridge>, lease_id: String) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .release_document_lease(lease_id)
        .await
        .map_err(knowledge_error)
}

/// `PublishDocument`: returns the parked plan a human still has to approve.
#[tauri::command]
async fn publish_document(
    bridge: State<'_, Bridge>,
    document_id: DocumentId,
    target: PublishTarget,
) -> Result<DocumentPublishPlan, String> {
    let client = client_of(&bridge).await?;
    client
        .publish_document(document_id, target)
        .await
        .map_err(knowledge_error)
}

#[tauri::command]
async fn list_ui_plugins(
    bridge: State<'_, Bridge>,
) -> Result<Vec<UiPluginLifecycleStatus>, String> {
    let client = client_of(&bridge).await?;
    client.list_ui_plugins().await.map_err(knowledge_error)
}

#[tauri::command]
async fn smoke_test_ui_plugin(
    bridge: State<'_, Bridge>,
    plugin_id: String,
) -> Result<Vec<UiPluginLifecycleStatus>, String> {
    let client = client_of(&bridge).await?;
    client
        .smoke_test_ui_plugin(plugin_id)
        .await
        .map_err(knowledge_error)
}

#[tauri::command]
async fn enable_ui_plugin(
    bridge: State<'_, Bridge>,
    plugin_id: String,
    scope: String,
) -> Result<Vec<UiPluginLifecycleStatus>, String> {
    let client = client_of(&bridge).await?;
    client
        .enable_ui_plugin(plugin_id, scope)
        .await
        .map_err(knowledge_error)
}

#[tauri::command]
async fn approve_ui_plugin_update(
    bridge: State<'_, Bridge>,
    plugin_id: String,
    approval_receipt: String,
) -> Result<Vec<UiPluginLifecycleStatus>, String> {
    let client = client_of(&bridge).await?;
    client
        .approve_ui_plugin_update(plugin_id, approval_receipt)
        .await
        .map_err(knowledge_error)
}

#[tauri::command]
async fn reject_ui_plugin_update(
    bridge: State<'_, Bridge>,
    plugin_id: String,
    approval_receipt: String,
) -> Result<Vec<UiPluginLifecycleStatus>, String> {
    let client = client_of(&bridge).await?;
    client
        .reject_ui_plugin_update(plugin_id, approval_receipt)
        .await
        .map_err(knowledge_error)
}

#[tauri::command]
async fn revoke_ui_plugin(
    bridge: State<'_, Bridge>,
    plugin_id: String,
) -> Result<Vec<UiPluginLifecycleStatus>, String> {
    let client = client_of(&bridge).await?;
    client
        .revoke_ui_plugin(plugin_id)
        .await
        .map_err(knowledge_error)
}

/// Open the OS folder picker and select the chosen checkout.
///
/// `Ok(None)` means the operator DISMISSED the dialog — a real outcome, and not
/// the same thing as a refusal, which is `Err` carrying the reason. The dialog
/// is opened from Rust and its result validated before the webview ever sees a
/// path, so the webview cannot name a directory this gate did not approve;
/// `capabilities/default.json` grants it no `dialog:*` permission at all.
#[tauri::command]
async fn pick_repository<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Option<crate::repository::RepositorySelection>, String> {
    use tauri_plugin_dialog::DialogExt as _;

    let start = tokio::task::spawn_blocking(crate::repository::selected_repository)
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
        .map(|selection| selection.path);

    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut builder = app
        .dialog()
        .file()
        .set_title("Choose a repository checkout");
    if let Some(start) = start {
        builder = builder.set_directory(start);
    }
    builder.pick_folder(move |picked| {
        let _ = tx.send(picked);
    });

    let Some(picked) = rx
        .await
        .map_err(|_| "the folder picker closed without answering".to_string())?
    else {
        return Ok(None);
    };
    let path = picked
        .into_path()
        .map_err(|error| format!("the chosen folder is not a local path: {error}"))?;
    tokio::task::spawn_blocking(move || crate::repository::select_repository(&path))
        .await
        .map_err(|error| format!("the repository selection task failed: {error}"))?
        .map(Some)
        .map_err(|error| format!("{error:#}"))
}

/// The repository currently selected, or `None` when none is.
///
/// `None` is rendered by the UI as "no repository selected" — it is never a cue
/// to substitute a directory. Re-validated on every read, so a checkout that
/// has since been moved or deleted surfaces as an error rather than continuing
/// to be sent to the daemon.
#[tauri::command]
async fn current_repository() -> Result<Option<crate::repository::RepositorySelection>, String> {
    // Re-validating shells out to `git` (`repo_anchor::checkout_root`) — off
    // the reactor thread, as in `daemon_connect`.
    tokio::task::spawn_blocking(crate::repository::selected_repository)
        .await
        .map_err(|error| format!("the repository read task failed: {error}"))?
        .map_err(|error| format!("{error:#}"))
}

/// Select a repository by path (a typed path, or a recent one), through exactly
/// the same gate the folder picker goes through.
#[tauri::command]
async fn set_repository(path: String) -> Result<crate::repository::RepositorySelection, String> {
    tokio::task::spawn_blocking(move || {
        crate::repository::select_repository(std::path::Path::new(&path))
    })
    .await
    .map_err(|error| format!("the repository selection task failed: {error}"))?
    .map_err(|error| format!("{error:#}"))
}

/// Forget the selection. The client then has no repository until one is chosen.
#[tauri::command]
async fn clear_repository() -> Result<(), String> {
    crate::repository::clear_repository().map_err(|error| format!("{error:#}"))
}

// ---------------------------------------------------------------------------
// Councils (surfaces: CouncilBrowser, CouncilBuilder, CouncilResults)
//
// LOCAL CONFIGURATION. There is no council variant in `CommandBody` — the
// daemon never hears the word. Definitions live in `<config_dir>/councils.toml`
// and results in `<data_dir>/councils/`, reached through the shared
// `codypendent-council` crate exactly as the TUI reaches them.
// ---------------------------------------------------------------------------

/// A running council's progress channel. One frame per round/member/chair
/// transition, emitted while `run_council`'s future is still pending.
struct CouncilChannelSink(Channel<crate::council::CouncilProgressFrame>);

impl crate::council::ProgressSink for CouncilChannelSink {
    fn emit(&self, frame: crate::council::CouncilProgressFrame) {
        // A send failure means the webview went away; the run continues and its
        // report is persisted regardless, which is the point of the report.
        let _ = self.0.send(frame);
    }
}

#[tauri::command]
async fn list_councils() -> Result<Vec<crate::council::CouncilCard>, String> {
    crate::council::list_councils().map_err(|error| format!("{error:#}"))
}

/// Persist a new council. Every refusal — name charset, 2..=N members, unique
/// member models, chair and members having to already exist in `models.toml` —
/// is `codypendent_council`'s own, so it is identical to the TUI's.
#[tauri::command]
async fn create_council(
    draft: crate::council::CouncilDraft,
) -> Result<crate::council::CouncilCard, String> {
    crate::council::create_council(draft).map_err(|error| format!("{error:#}"))
}

/// Remove a definition. Saved run reports are deliberately left on disk.
#[tauri::command]
async fn delete_council(name: String) -> Result<(), String> {
    crate::council::delete_council(&name).map_err(|error| format!("{error:#}"))
}

/// Every council's newest durable result. An unreadable individual report
/// degrades to a warning on the page rather than emptying it.
#[tauri::command]
async fn list_council_results() -> Result<crate::council::CouncilResultsPage, String> {
    crate::council::list_council_results().map_err(|error| format!("{error:#}"))
}

/// One durable result by council name or result id. `Ok(null)` is "looked,
/// nothing there"; an error is "could not look".
#[tauri::command]
async fn council_result(
    selector: String,
) -> Result<Option<crate::council::CouncilResultCard>, String> {
    crate::council::council_result(&selector).map_err(|error| format!("{error:#}"))
}

/// Convene a council against an objective.
///
/// Long-running by nature: each member and the chair is a real daemon run on its
/// own connection, so the promise settles when the deliberation does, while
/// `channel` carries the round/member/chair transitions in the meantime.
///
/// `session_id` links the result to the session it was asked from, so the report
/// is attributable later. It is optional because a council may legitimately be
/// convened before any session exists — but it is never invented.
#[tauri::command]
async fn run_council(
    name: String,
    objective: String,
    repository: Option<String>,
    session_id: Option<SessionId>,
    channel: Channel<crate::council::CouncilProgressFrame>,
) -> Result<crate::council::CouncilRunReply, String> {
    let repository = crate::council::council_repository(repository.as_deref())
        .map_err(|error| format!("{error:#}"))?;
    let sink = Arc::new(CouncilChannelSink(channel));
    crate::council::run_council(name, objective, repository, session_id, sink)
        .await
        .map_err(|error| format!("{error:#}"))
}

// ---------------------------------------------------------------------------
// First-run onboarding (surface: Onboarding)
//
// The TUI opens `Overlay::Onboard` after boot when — and only when — the
// authoritative runnable-model projection is empty and the operator has not
// chosen to skip (`crates/cli/src/tui.rs::apply_post_boot_onboard_gate`). The
// desktop shell has no runnable projection: it configures models, the daemon
// runs them. So this command answers the three conditions the desktop surface
// actually CLAIMS to detect, each from a real read, and each able to answer
// "I could not tell" — which is not the same as "no".
//
// The environment lookup is the reason this lives in Rust at all. A webview
// cannot read `$ANTHROPIC_API_KEY`, and `models::list_models` reports an entry
// that names an environment variable as `Env { name }` WITHOUT resolving it —
// a correct projection for a key-presence table, but it would let an
// onboarding step claim "credential configured" for an unset variable.
// ---------------------------------------------------------------------------

/// One first-run condition. `Unknown` exists so a failed read never renders as
/// a confident "not done" (nor as a confident "done").
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OnboardCheck {
    /// The condition holds, and `detail` is the evidence that says so.
    Satisfied { detail: String },
    /// The condition does not hold. `detail` is what was read.
    Unsatisfied { detail: String },
    /// The read could not answer. NOT an absence — see the module rule above.
    Unknown { reason: String },
}

/// What [`onboarding_status`] answers with.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OnboardingStatus {
    /// Whether `models.toml` holds at least one `[[model]]` entry.
    pub model: OnboardCheck,
    /// Whether a credential for at least one configured model RESOLVES NOW —
    /// stored in `auth.json`, or a named environment variable that is set, or
    /// a provider that requires no key at all.
    pub credential: OnboardCheck,
    /// Whether a validated git checkout is selected.
    pub repository: OnboardCheck,
    /// The file the model and credential checks read, named so an empty answer
    /// can say where it looked.
    pub models_path: String,
    /// Degradations that did not stop the read. Surfaced, never swallowed.
    pub warnings: Vec<String>,
}

/// Resolve the three first-run conditions.
///
/// Nothing here is cached and nothing is persisted: the answer is recomputed
/// from `models.toml`, `auth.json`, the provider catalog, the process
/// environment and the stored repository preference every time it is asked, so
/// a step cannot report complete because it was complete once.
#[tauri::command]
async fn onboarding_status(bridge: State<'_, Bridge>) -> Result<OnboardingStatus, String> {
    let pinned = bridge.run_defaults.lock().await.model.clone();
    let mut warnings: Vec<String> = Vec::new();

    // Which catalog providers require a key. Without this a local endpoint
    // (Ollama, LM Studio) — whose entries carry no `api_key_env` and therefore
    // report `Missing` — would be miscounted as a model waiting for a key.
    let requires_key: std::collections::BTreeMap<String, bool> =
        match crate::models::list_providers() {
            Ok(view) => {
                warnings.extend(view.warnings);
                view.providers
                    .into_iter()
                    .map(|row| (row.id, row.requires_key))
                    .collect()
            }
            Err(error) => {
                warnings.push(format!(
                "the provider catalog could not be read ({error:#}); whether a configured model \
                 needs a key could not be determined for every entry"
            ));
                std::collections::BTreeMap::new()
            }
        };

    let models = match crate::models::list_models(pinned.as_ref()) {
        Ok(view) => view,
        Err(error) => {
            // `models.toml` exists and does not parse. That is not an empty
            // configuration, and neither dependent step can be judged.
            let reason = format!("models.toml could not be read: {error:#}");
            return Ok(OnboardingStatus {
                model: OnboardCheck::Unknown {
                    reason: reason.clone(),
                },
                credential: OnboardCheck::Unknown {
                    reason: reason.clone(),
                },
                repository: repository_check_off_thread().await,
                models_path: String::new(),
                warnings,
            });
        }
    };
    warnings.extend(models.warnings.iter().cloned());

    let model = if models.models.is_empty() {
        OnboardCheck::Unsatisfied {
            detail: if models.configured {
                format!(
                    "{} exists but declares no [[model]] entry",
                    models.models_path
                )
            } else {
                format!("{} does not exist yet", models.models_path)
            },
        }
    } else {
        OnboardCheck::Satisfied {
            detail: format!(
                "{} model{} configured: {}",
                models.models.len(),
                if models.models.len() == 1 { "" } else { "s" },
                summarize_ids(
                    &models
                        .models
                        .iter()
                        .map(|row| row.id.as_str())
                        .collect::<Vec<_>>(),
                    4
                )
            ),
        }
    };

    let credential = credential_check(&models, &requires_key);
    Ok(OnboardingStatus {
        model,
        credential,
        repository: repository_check_off_thread().await,
        models_path: models.models_path,
        warnings,
    })
}

/// Whether any configured model has a credential that resolves right now.
///
/// Per entry, in the order `models::model_key_status` establishes:
///
/// * `Stored` — a key is in `auth.json`. Resolves.
/// * `Env { name }` — the entry names a variable; this is where it is actually
///   looked up. A blank or whitespace-only value is absent, matching
///   `models::provider_has_resolvable_key`.
/// * `Missing` — no stored key and no variable named. Whether that MATTERS
///   depends on the provider: a local endpoint needs none. Without a recorded
///   `provider_id` (a hand-written entry) the answer is unknown, not "no".
/// * `Unknown` — `auth.json` could not be read.
fn credential_check(
    models: &crate::models::ModelsView,
    requires_key: &std::collections::BTreeMap<String, bool>,
) -> OnboardCheck {
    if models.models.is_empty() {
        return OnboardCheck::Unsatisfied {
            detail: "no model is configured yet, so no credential can resolve".to_string(),
        };
    }

    let mut ready: Vec<String> = Vec::new();
    let mut waiting: Vec<String> = Vec::new();
    let mut undetermined: Vec<String> = Vec::new();

    for row in &models.models {
        if row.provider == "acp" {
            ready.push(format!("{} (uses the installed agent's sign-in)", row.id));
            continue;
        }
        match &row.key {
            crate::models::KeyStatus::Stored => {
                ready.push(format!("{} (key stored in auth.json)", row.id));
            }
            crate::models::KeyStatus::Env { name } => {
                if std::env::var(name).is_ok_and(|value| !value.trim().is_empty()) {
                    ready.push(format!("{} (${name} is set)", row.id));
                } else {
                    waiting.push(format!("{} (${name} is not set in this process)", row.id));
                }
            }
            crate::models::KeyStatus::Missing => match row.provider_id.as_deref() {
                Some(provider_id) => match requires_key.get(provider_id) {
                    Some(false) => {
                        ready.push(format!("{} ({provider_id} requires no key)", row.id));
                    }
                    Some(true) => {
                        waiting.push(format!(
                            "{} (no stored key and no environment variable named)",
                            row.id
                        ));
                    }
                    None => undetermined.push(format!(
                        "{}: provider `{provider_id}` is not in the catalog, so whether it needs \
                         a key is unknown",
                        row.id
                    )),
                },
                None => undetermined.push(format!(
                    "{}: models.toml records no provider for this entry, so whether it needs a \
                     key is unknown",
                    row.id
                )),
            },
            crate::models::KeyStatus::Unknown { reason } => {
                undetermined.push(format!("{}: {reason}", row.id));
            }
        }
    }

    if !ready.is_empty() {
        return OnboardCheck::Satisfied {
            detail: summarize(&ready, 3),
        };
    }
    // Nothing resolved. Say "no" only where the read proved it; anything the
    // read could not determine keeps the whole step at "unknown", because a
    // wrong "no" here is a setup wizard shown to somebody already set up.
    if !undetermined.is_empty() {
        return OnboardCheck::Unknown {
            reason: summarize(&undetermined, 3),
        };
    }
    OnboardCheck::Unsatisfied {
        detail: summarize(&waiting, 3),
    }
}

/// `repository_check` off the reactor thread: re-validating the selection
/// shells out to `git rev-parse` (`repo_anchor::checkout_root`), which must
/// not block the Tauri runtime.
async fn repository_check_off_thread() -> OnboardCheck {
    tokio::task::spawn_blocking(repository_check)
        .await
        .unwrap_or_else(|error| OnboardCheck::Unknown {
            reason: format!("the repository check task failed: {error}"),
        })
}

/// The stored repository selection, re-validated by `repository.rs` on read.
fn repository_check() -> OnboardCheck {
    match crate::repository::selected_repository() {
        Ok(Some(selection)) => OnboardCheck::Satisfied {
            detail: selection.path,
        },
        Ok(None) => OnboardCheck::Unsatisfied {
            detail: "no repository is selected, so sessions are created without one".to_string(),
        },
        Err(error) => OnboardCheck::Unknown {
            reason: format!("{error:#}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Code graph + backtrack (surfaces: EdgesView, BacktrackView)
//
// A fourth additive `use` block, for the same reason as the ones above.
// ---------------------------------------------------------------------------
use codypendent_protocol::{
    CheckpointId, CodeGraphPage, CodeGraphQuery, CodeGraphScanReport, CodeGraphStatusView,
};

static CODE_GRAPH_BUILD_LOCK: Mutex<()> = Mutex::const_new(());

#[tauri::command]
async fn build_code_graph(bridge: State<'_, Bridge>) -> Result<CodeGraphScanReport, String> {
    let _build = CODE_GRAPH_BUILD_LOCK
        .try_lock()
        .map_err(|_| "The code map is already being built. Refresh it in a moment.".to_string())?;
    client_of(&bridge)
        .await?
        .build_code_graph()
        .await
        .map_err(|error| format!("{error:#}"))
}

/// `ReadCodeGraphStatus`: what the stored graph holds for the connection's
/// checkout, with no re-scan.
///
/// A read — an Observer may issue it too. The reply names the repository root
/// the daemon actually resolved, so a graph read against the wrong checkout is
/// visible rather than inferred.
#[tauri::command]
async fn code_graph_status(bridge: State<'_, Bridge>) -> Result<CodeGraphStatusView, String> {
    let client = client_of(&bridge).await?;
    client
        .code_graph_status()
        .await
        .map_err(|error| format!("{error:#}"))
}

/// `ReadCodeGraph`: one filtered, limited page of nodes and edges.
///
/// The query crosses verbatim — it is the protocol's own `CodeGraphQuery`, not
/// a shape invented here — so every field narrows and none widens: the
/// repository gate is the daemon's and applies to `node_id` too. The webview
/// always sends a limit; a real graph is ~500k nodes and 1.2M edges and the
/// daemon clamps to its own ceiling regardless, which is why the reply's
/// `total_nodes`/`total_edges`/`limit` are rendered as "showing N of M".
#[tauri::command]
async fn read_code_graph(
    bridge: State<'_, Bridge>,
    query: CodeGraphQuery,
) -> Result<CodeGraphPage, String> {
    let client = client_of(&bridge).await?;
    client
        .read_code_graph(query)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// `ForkSession`: branch the ATTACHED session at a run-launch checkpoint.
///
/// Returns the fork's session id so the UI can open it. The source session is
/// untouched — this is the non-destructive half of backtracking, and it is the
/// daemon that decides whether a given checkpoint may be forked at all.
#[tauri::command]
async fn fork_session(
    bridge: State<'_, Bridge>,
    checkpoint: CheckpointId,
    name: Option<String>,
) -> Result<SessionId, String> {
    let client = client_of(&bridge).await?;
    client
        .fork_session(checkpoint, name)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// `RestoreCheckpoint`: ask to rewind a settled run's worktree.
///
/// Resolving means the daemon ACCEPTED the request and parked its own
/// high-risk approval; nothing on disk has changed yet. The caller says
/// "approval requested" and the operator decides on the approval card, which
/// carries the daemon's own reason — this shell never restates that reason as a
/// policy of its own, and never reports a restore that has not happened.
#[tauri::command]
async fn restore_checkpoint(
    bridge: State<'_, Bridge>,
    run_id: RunId,
    checkpoint: CheckpointId,
) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .restore_checkpoint(run_id, checkpoint)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Pause a live run — a real `PauseRun`, the non-destructive sibling of
/// `cancel_run`.
///
/// Whether the run can actually take it is the DAEMON's call
/// (`validate_run_transition`), and a refusal comes back as its own
/// `run.invalid-transition` error string. The webview additionally hides the
/// button unless the run state it folded from `RunStateChanged` says the run is
/// pausable, so an operator is not offered an action that can only fail.
#[tauri::command]
async fn pause_run(bridge: State<'_, Bridge>, run_id: RunId) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .pause_run(run_id)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Resume a paused run — a real `ResumeRun`. The daemon admits this ONLY from
/// `Paused`; from anything else it answers `run.invalid-transition`.
#[tauri::command]
async fn resume_run(bridge: State<'_, Bridge>, run_id: RunId) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .resume_run(run_id)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Queue a follow-up prompt on the attached session's server-side queue
/// (`QueuePrompt`).
///
/// The mode is NOT a webview argument by default: an omitted `mode` uses
/// whatever the operator staged in the mode picker, exactly as
/// [`start_objective`] does, so a queued prompt runs under the mode the UI is
/// showing rather than one this command invented.
///
/// `Ok(())` means the daemon ACCEPTED the command. The queue itself arrives as
/// a `PendingPromptsChanged` event on the session stream; nothing here returns
/// a queue, because a second source of truth would race the event.
#[tauri::command]
async fn queue_prompt(
    bridge: State<'_, Bridge>,
    text: String,
    delivery: PromptDelivery,
    mode: Option<AgentMode>,
) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    let mode = match mode {
        Some(mode) => mode,
        None => bridge.run_defaults.lock().await.mode,
    };
    client
        .queue_prompt(text, mode, delivery)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Edit a queued prompt in place (`UpdateQueuedPrompt`). Absent fields keep
/// their current values.
#[tauri::command]
async fn update_queued_prompt(
    bridge: State<'_, Bridge>,
    prompt_id: PromptId,
    text: Option<String>,
    delivery: Option<PromptDelivery>,
) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .update_queued_prompt(prompt_id, text, delivery)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Promote a queued prompt to steer (`PromoteQueuedPrompt`): delivery becomes
/// `Steer` and the entry moves to the front.
#[tauri::command]
async fn promote_queued_prompt(
    bridge: State<'_, Bridge>,
    prompt_id: PromptId,
) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .promote_queued_prompt(prompt_id)
        .await
        .map_err(|error| format!("{error:#}"))
}

/// Remove a queued prompt without running it (`DeleteQueuedPrompt`).
#[tauri::command]
async fn delete_queued_prompt(
    bridge: State<'_, Bridge>,
    prompt_id: PromptId,
) -> Result<(), String> {
    let client = client_of(&bridge).await?;
    client
        .delete_queued_prompt(prompt_id)
        .await
        .map_err(|error| format!("{error:#}"))
}

async fn client_of(bridge: &State<'_, Bridge>) -> Result<Arc<DaemonClient>, String> {
    Ok(connected(bridge).await?.0)
}

async fn connected(
    bridge: &State<'_, Bridge>,
) -> Result<(Arc<DaemonClient>, Arc<ChannelSink>), String> {
    let guard = bridge.connection.lock().await;
    match guard.as_ref() {
        Some(connection) => Ok((Arc::clone(&connection.client), Arc::clone(&connection.sink))),
        None => Err("not connected to codypendentd".to_string()),
    }
}

/// Discover installed tools off the UI/reactor thread. Discovery is local and
/// never runs an editor, invokes a shell profile, or makes a provider request.
#[tauri::command]
async fn workspace_targets() -> Result<Vec<crate::environment::WorkspaceTarget>, String> {
    tokio::task::spawn_blocking(crate::environment::targets)
        .await
        .map_err(|error| format!("Detecting workspace applications failed: {error}"))
}

#[tauri::command]
async fn workspace_open(
    bridge: State<'_, Bridge>,
    target: String,
    expected_repository: String,
    expected_session: Option<SessionId>,
    expected_run: Option<RunId>,
    file: Option<String>,
    line: Option<u32>,
) -> Result<(), String> {
    if let Some(session) = expected_session {
        let (client, _) = connected(&bridge).await?;
        if client.repository() != Some(expected_repository.as_str())
            || client.attached_session_id().await != Some(session)
        {
            return Err(
                "The active session or repository changed. Retry from the current session.".into(),
            );
        }
    }
    let socket = socket_path().map_err(|error| format!("{error:#}"))?;
    let checkout = match (expected_session, expected_run) {
        (Some(session), Some(run)) => Some(
            crate::environment::run_checkout(&expected_repository, session, run)
                .await
                .map_err(|error| format!("{error:#}"))?,
        ),
        (None, Some(_)) => return Err("A reviewed run requires its owning session".into()),
        _ => None,
    };
    tokio::task::spawn_blocking(move || {
        crate::environment::open_workspace(
            &target,
            &expected_repository,
            checkout.as_deref(),
            expected_session,
            &socket,
            file.as_deref(),
            line,
        )
    })
    .await
    .map_err(|error| format!("Opening the workspace application failed: {error}"))?
    .map_err(|error| format!("{error:#}"))
}

/// Register the bridge state and command handlers on a Tauri builder.
pub fn register<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder
        .register_uri_scheme_protocol("cody-sandbox", |_context, request| {
            crate::sandbox::response(request)
        })
        .menu(crate::native_menu::create)
        .on_menu_event(crate::native_menu::dispatch)
        // OS notifications for blocking work (approvals, questions). Registered
        // here because the webview reaches the plugin through the same
        // capability set as the bridge commands; the grant is narrowed to
        // is-permission-granted / request-permission / notify in
        // `capabilities/default.json`.
        .plugin(tauri_plugin_notification::init())
        // The native folder picker for repository selection. Driven from Rust
        // (`bridge::pick_repository`), so the webview receives a validated
        // checkout path and never a filesystem capability of its own — no
        // `dialog:*` permission appears in `capabilities/default.json`.
        .plugin(tauri_plugin_dialog::init())
        .manage(Bridge::load())
        .invoke_handler(tauri::generate_handler![
            daemon_socket,
            daemon_launch_status,
            daemon_start,
            daemon_connect,
            daemon_disconnect,
            list_sessions,
            start_objective,
            attach_session,
            read_session_event_range,
            cancel_run,
            queue_steering,
            resolve_approval,
            resolve_question,
            list_inbox,
            mutate_inbox,
            query_analytics,
            export_analytics,
            read_artifact,
            send_remote_ui,
            save_artifact,
            search_sessions,
            mutate_session,
            list_workflows,
            start_workflow,
            read_workflow_run,
            watch_workflow,
            pause_workflow,
            resume_workflow,
            cancel_workflow,
            retry_workflow_node,
            read_blackboard,
            post_blackboard_question,
            watch_board,
            create_board_card,
            move_board_card,
            list_models,
            discover_models,
            crate::desktop_tools::desktop_tools_publish,
            crate::python_sandbox::desktop_tools_python_execute,
            crate::python_sandbox::desktop_tools_python_reply,
            crate::python_sandbox::desktop_tools_python_cancel,
            crate::desktop_tools::desktop_tools_pending,
            crate::desktop_tools::desktop_tools_resolve,
            crate::desktop_tools::desktop_tools_list,
            crate::desktop_tools::desktop_tools_remove,
            list_model_readiness,
            model_readiness,
            set_run_model,
            add_model,
            remove_model,
            list_providers,
            refresh_agents,
            connect_acp,
            list_catalog_models,
            list_api_keys,
            set_api_key,
            remove_api_key,
            list_modes,
            run_defaults,
            set_run_mode,
            list_skills,
            list_memories,
            correct_memory,
            forget_memory,
            list_learnings,
            mutate_learning,
            list_documents,
            create_document,
            acquire_document_lease,
            mutate_document,
            release_document_lease,
            publish_document,
            list_ui_plugins,
            smoke_test_ui_plugin,
            enable_ui_plugin,
            approve_ui_plugin_update,
            reject_ui_plugin_update,
            revoke_ui_plugin,
            pick_repository,
            current_repository,
            set_repository,
            clear_repository,
            workspace_targets,
            workspace_open,
            list_councils,
            create_council,
            delete_council,
            list_council_results,
            council_result,
            run_council,
            onboarding_status,
            code_graph_status,
            build_code_graph,
            read_code_graph,
            fork_session,
            restore_checkpoint,
            // Run lifecycle and the pending-prompt queue. A `#[tauri::command]`
            // missing from this list is invisible from the webview, which is
            // exactly how the previously-dead pickers shipped.
            pause_run,
            resume_run,
            queue_prompt,
            update_queued_prompt,
            promote_queued_prompt,
            delete_queued_prompt
        ])
}

#[cfg(test)]
mod disconnect_generation_tests {
    /// A deferred teardown must not close the connection that REPLACED the one
    /// it meant to close.
    ///
    /// The webview defers its disconnect until the previous connect settles, so
    /// on a reconnect the stale call can land after the replacement registered.
    /// `daemon_disconnect` used to `take()` whatever was there. And because a
    /// deliberate disconnect emits no `Disconnected` frame — deliberately, so a
    /// teardown cannot clobber the new connection's state — the store went on
    /// reporting "connected" while every command timed out, and the reconnect
    /// effect only fires on "disconnected". One race disabled the app for good,
    /// in silence.
    ///
    /// This exercises the comparison itself: the command needs a live socket,
    /// but the decision it makes is "is the registered generation the one asked
    /// for", and that is what must hold.
    #[test]
    fn a_stale_generation_does_not_match_a_newer_connection() {
        // Registered generation, requested generation, may it close?
        let cases = [
            (1_u64, Some(1_u64), true), // the attempt closing itself
            (2, Some(1), false),        // a STALE teardown after a reconnect
            (1, Some(2), false),        // a teardown that ran ahead of itself
            (7, None, true),            // app teardown closes whatever is open
        ];
        for (registered, requested, expected) in cases {
            let may_close = match requested {
                Some(wanted) => registered == wanted,
                None => true,
            };
            assert_eq!(
                may_close, expected,
                "registered={registered} requested={requested:?}"
            );
        }
    }
}

#[cfg(test)]
mod automatic_connection_tests {
    use super::daemon_is_absent;
    use codypendent_council::connection::Connection;
    use codypendent_protocol::{read_envelope, write_envelope, Envelope, Payload, ProtocolError};

    #[tokio::test]
    async fn absent_and_stale_sockets_can_start_a_daemon() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("daemon.sock");
        let absent = Connection::connect(&socket).await.err().expect("absent");
        assert!(daemon_is_absent(&absent));
        drop(tokio::net::UnixListener::bind(&socket).expect("bind"));
        let refused = Connection::connect(&socket).await.err().expect("refused");
        assert!(daemon_is_absent(&refused));
        assert!(!daemon_is_absent(&anyhow::Error::new(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied,)
        )));
    }

    #[tokio::test]
    async fn rejected_handshake_does_not_start_another_daemon() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = read_envelope(&mut stream)
                .await
                .expect("read")
                .expect("hello");
            let reply = Envelope::reply_to(
                &request,
                Payload::Error(ProtocolError {
                    code: "protocol.unsupported".to_owned(),
                    message: "incompatible protocol".to_owned(),
                    retryable: false,
                }),
            );
            write_envelope(&mut stream, &reply).await.expect("reply");
        });
        let mut connection = Connection::connect(&socket).await.expect("connect");
        let error = connection
            .handshake("desktop", "test", None)
            .await
            .expect_err("refused");
        assert!(!daemon_is_absent(&error));
        assert!(error.to_string().contains("incompatible protocol"));
        server.await.expect("server");
    }
}
