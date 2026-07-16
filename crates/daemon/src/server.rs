//! Unix-domain-socket protocol server.
//!
//! Phase 0 served the daemon-lifecycle messages (Ping, DaemonStatusRequest,
//! Shutdown). STEP 1.11 grows this into the full session server: a handshake
//! (`ClientHello`/`ServerHello`) with a 15s heartbeat, `AttachSession` with
//! catch-up (missed events vs. a snapshot per the ≤500 rule), per-session event
//! fan-out to subscribed clients, command routing through the crash-consistent
//! write path, and opaque daemon-signed resume tokens.
//!
//! The three lifecycle payloads keep working with **no** handshake — they are
//! connection-level daemon control, not session interaction — so the Phase 0
//! client (and `tests/socket.rs`) is unaffected. Only session interaction
//! (`Command`, including `AttachSession`) requires a prior `ClientHello`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    read_envelope, write_envelope, Catchup, ClientId, ClientRole, CommandBody, DaemonStatus,
    Envelope, FrameError, Payload, ProtocolError, ServerHello, SessionEvent, SessionId,
    Subscription, PROTOCOL_V1,
};
use sqlx::SqlitePool;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch, Mutex};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::approvals::ApprovalBroker;
use crate::artifacts::ArtifactStore;
use crate::commands::{ApplyContext, CommandProcessor};
use crate::executor::{RunExecutor, RunLaunch};
use crate::instance::InstanceRecord;
use crate::ledger;
use crate::projections;
use crate::subscriptions::SubscriptionHub;

/// Heartbeat cadence advertised in `ServerHello` and used to probe idle clients.
const HEARTBEAT_INTERVAL_MS: u64 = 15_000;
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(HEARTBEAT_INTERVAL_MS);
/// A client silent for this many heartbeat intervals (3 × 15s = 45s) is dropped.
const HEARTBEAT_MISS_LIMIT: u32 = 3;
/// The catch-up cutover: a client at most this many events behind is replayed
/// event-by-event; further behind, it receives a projection snapshot instead.
const CATCHUP_EVENT_LIMIT: u64 = 500;

/// A write half shared between a connection's request/reply path and its
/// per-session event forwarders, so both can frame envelopes onto one socket.
type SharedWriter = Arc<Mutex<OwnedWriteHalf>>;

pub struct ServerState {
    pub pool: SqlitePool,
    pub paths: RuntimePaths,
    pub instance: InstanceRecord,
    pub started_at: DateTime<Utc>,
    pub shutdown: watch::Sender<bool>,
    /// The crash-consistent command write path (persist-before-publish); shares
    /// its [`SubscriptionHub`] with `subscriptions` below.
    pub commands: CommandProcessor,
    /// Per-session event fan-out the server subscribes attached clients to.
    pub subscriptions: SubscriptionHub,
    /// Content-addressed artifact store (`<data_dir>/artifacts`); held here so
    /// the session server owns it for later steps (tool output, chronicles).
    pub artifacts: ArtifactStore,
    /// The per-user secret (32 bytes) that signs resume tokens.
    pub secret: Vec<u8>,
    /// Executes accepted runs. `None` in a lib-only / test embedding (the run
    /// stays `Queued`); the assembly binary injects an implementation that wraps
    /// the runtime agent loop (dependency inversion — see [`crate::executor`]).
    pub executor: Option<Arc<dyn RunExecutor>>,
}

/// Bind the socket, write the pidfile, and serve until Shutdown or SIGTERM /
/// SIGINT. Removes the socket and pidfile on exit.
///
/// This is the executor-less entry point: an accepted `StartRun` is persisted
/// and the run stays `Queued` (nothing executes it). It is what the daemon's own
/// integration tests (`tests/socket.rs`, `tests/server_it.rs`) drive. The
/// assembly binary calls [`run_with_executor`] with a real executor.
pub async fn run(
    pool: SqlitePool,
    paths: RuntimePaths,
    instance: InstanceRecord,
) -> anyhow::Result<()> {
    run_with_executor(pool, paths, instance, None).await
}

/// Like [`run`], but with an injected [`RunExecutor`] that actually executes an
/// accepted `StartRun` (the assembly binary wraps the runtime agent loop).
///
/// When an executor is present, the server binds its command fan-out and
/// approval broker to the executor's ([`RunExecutor::collaborators`]), so a
/// run's events reach attached clients and a client's `ResolveApproval` reaches
/// the runtime awaiting it. With `executor = None` the server creates its own
/// fresh instances and behaves exactly as the pre-executor server did.
pub async fn run_with_executor(
    pool: SqlitePool,
    paths: RuntimePaths,
    instance: InstanceRecord,
    executor: Option<Arc<dyn RunExecutor>>,
) -> anyhow::Result<()> {
    prepare_socket(&paths).await?;
    let listener = UnixListener::bind(&paths.socket_path)?;
    std::fs::write(&paths.pid_path, std::process::id().to_string())?;
    info!(socket = %paths.socket_path.display(), pid = std::process::id(), "daemon listening");

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    // One shared fan-out drives both the command processor (publisher) and the
    // server (subscriber); cloning a `SubscriptionHub` shares its channels. When
    // an executor is injected, reuse ITS hub + broker so run events published by
    // the agent loop reach this server's forwarders, and a client's
    // `ResolveApproval` (routed through the command processor) wakes the runtime.
    let (subscriptions, approvals) = executor
        .as_ref()
        .and_then(|e| e.collaborators())
        .unwrap_or_else(|| (SubscriptionHub::new(), ApprovalBroker::new()));
    let commands = CommandProcessor::new(subscriptions.clone(), approvals);
    let artifacts = ArtifactStore::new(paths.data_dir.join("artifacts"));
    let secret = load_or_create_secret(&paths.data_dir)?;

    let state = Arc::new(ServerState {
        pool,
        paths: paths.clone(),
        instance,
        started_at: Utc::now(),
        shutdown: shutdown_tx,
        commands,
        subscriptions,
        artifacts,
        secret,
        executor,
    });

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                info!("shutdown requested via protocol");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("SIGINT received");
                break;
            }
            _ = sigterm.recv() => {
                info!("SIGTERM received");
                break;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, state).await {
                                warn!(error = %e, "connection ended with error");
                            }
                        });
                    }
                    Err(e) => error!(error = %e, "accept failed"),
                }
            }
        }
    }

    let _ = std::fs::remove_file(&paths.socket_path);
    let _ = std::fs::remove_file(&paths.pid_path);
    info!("daemon stopped");
    Ok(())
}

/// Refuse to start if a live daemon already owns the socket; remove the
/// socket file if it is stale (bind would otherwise fail with AddrInUse).
async fn prepare_socket(paths: &RuntimePaths) -> anyhow::Result<()> {
    paths.validate_socket_path()?;
    if paths.socket_path.exists() {
        match UnixStream::connect(&paths.socket_path).await {
            Ok(_) => anyhow::bail!(
                "another daemon is already listening on {}",
                paths.socket_path.display()
            ),
            Err(_) => {
                warn!(socket = %paths.socket_path.display(), "removing stale socket");
                std::fs::remove_file(&paths.socket_path)?;
            }
        }
    }
    Ok(())
}

/// Per-connection mutable state established by the handshake and updated by
/// `AttachSession`.
struct ConnState {
    /// The client's identity — from `ClientHello` (its envelope, or a valid
    /// resume token). `None` until the connection handshakes.
    client_id: Option<ClientId>,
    /// The role applied to commands on this connection. A handshaken local
    /// client defaults to [`ClientRole::Controller`]: the Phase 1 socket is
    /// user-private (0700 dirs, OS peer identity), so the single connecting user
    /// is trusted to create sessions and control their own runs without a prior
    /// attach. An explicit `AttachSession` may narrow (or re-assert) the role —
    /// e.g. an observer-only view. Remote transports (later phases) will default
    /// to `Observer` and require authenticated elevation.
    role: ClientRole,
    /// Whether a `ClientHello` has been seen (session interaction requires it).
    handshaken: bool,
}

impl ConnState {
    fn new() -> Self {
        Self {
            client_id: None,
            role: ClientRole::Controller,
            handshaken: false,
        }
    }

    /// The identity to stamp on outgoing frames / commands, falling back to a
    /// per-message client id when the connection has not handshaked.
    fn client_id_or(&self, fallback: ClientId) -> ClientId {
        self.client_id.unwrap_or(fallback)
    }
}

/// Serve one connection: a frame-read loop plus a separate heartbeat task.
/// Lifecycle payloads are served without a handshake; session interaction is
/// gated on a prior `ClientHello`. Event forwarders spawned by `AttachSession`
/// write to the same (shared) socket and are aborted when the connection ends.
///
/// The heartbeat runs in its own task (not a `select!` arm of the read loop) so a
/// heartbeat tick can never cancel a `read_envelope` future mid-frame — which
/// would drop the consumed bytes and desynchronize the stream. The read loop only
/// races reads against an idle-shutdown signal the heartbeat task raises, and it
/// stamps a shared last-activity instant the heartbeat task consults to decide
/// when a silent client should be dropped.
async fn handle_connection(stream: UnixStream, state: Arc<ServerState>) -> anyhow::Result<()> {
    let (mut read_half, write_half) = stream.into_split();
    let writer: SharedWriter = Arc::new(Mutex::new(write_half));
    let mut conn = ConnState::new();
    let mut forwarders: Vec<JoinHandle<()>> = Vec::new();

    // The read loop stamps this on every frame; the heartbeat task reads it to
    // decide when the client has gone silent. Locked only for the instant swap,
    // never across an `.await`, so a std mutex is the right tool.
    let last_activity = Arc::new(std::sync::Mutex::new(tokio::time::Instant::now()));
    // The heartbeat task raises this to end an idle (or dead-peer) connection.
    let (idle_tx, mut idle_rx) = watch::channel(false);

    let heartbeat = tokio::spawn(heartbeat_loop(
        Arc::clone(&writer),
        Arc::clone(&last_activity),
        idle_tx,
    ));

    let result = loop {
        tokio::select! {
            // Frame reads are never raced against a timer, so a frame is never
            // cancelled mid-parse. The only competing arm is the idle signal,
            // which the heartbeat task raises only once the client has already
            // gone silent — so nothing in flight is lost.
            read = read_envelope(&mut read_half) => {
                let request = match read {
                    Ok(Some(request)) => request,
                    Ok(None) => break Ok(()), // clean end-of-stream
                    Err(e) => break Err(e.into()),
                };
                *last_activity
                    .lock()
                    .expect("last-activity mutex poisoned") = tokio::time::Instant::now();
                match handle_request(&state, &writer, &mut conn, &mut forwarders, request).await {
                    Ok(true) => break Ok(()), // shutdown handled
                    Ok(false) => {}
                    Err(e) => break Err(e),
                }
            }
            // The heartbeat task asked us to end (silent 3 intervals, or the peer
            // vanished mid-ping). A `changed()` error (sender dropped) ends it too.
            _ = idle_rx.changed() => break Ok(()),
        }
    };

    heartbeat.abort();
    // A slow or vanished client must never wedge a forwarder; drop them all.
    for forwarder in forwarders {
        forwarder.abort();
    }
    result
}

/// The per-connection heartbeat, run as its own task beside the read loop. It
/// pings the client every [`HEARTBEAT_INTERVAL`] via the shared writer and, when
/// the client has been silent for [`HEARTBEAT_MISS_LIMIT`] intervals (or a ping
/// write fails), signals `idle_tx` so the read loop ends the connection. Keeping
/// it off the read path is what guarantees a tick never cancels a frame read.
async fn heartbeat_loop(
    writer: SharedWriter,
    last_activity: Arc<std::sync::Mutex<tokio::time::Instant>>,
    idle_tx: watch::Sender<bool>,
) {
    // Delay the first tick a full interval so an idle-but-fresh connection is
    // not immediately probed.
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + HEARTBEAT_INTERVAL,
        HEARTBEAT_INTERVAL,
    );
    let idle_limit = HEARTBEAT_INTERVAL * HEARTBEAT_MISS_LIMIT;
    loop {
        ticker.tick().await;
        let idle = last_activity
            .lock()
            .expect("last-activity mutex poisoned")
            .elapsed();
        if idle >= idle_limit {
            let _ = idle_tx.send(true); // silent for 3 intervals — drop the client
            return;
        }
        let ping = Envelope::request(ClientId::new(), Payload::Ping);
        if send(&writer, &ping).await.is_err() {
            let _ = idle_tx.send(true); // peer gone
            return;
        }
    }
}

/// Handle one request. Returns `Ok(true)` when a Shutdown was served (the caller
/// should stop reading this connection). Replies are framed onto `writer`.
async fn handle_request(
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    conn: &mut ConnState,
    forwarders: &mut Vec<JoinHandle<()>>,
    request: Envelope,
) -> anyhow::Result<bool> {
    // Major-version incompatibility is refused structurally; the connection
    // survives (mirrors Phase 0).
    if !request.protocol_version.compatible_with(&PROTOCOL_V1) {
        let reply = Envelope::reply_to(
            &request,
            Payload::Error(ProtocolError {
                code: "protocol.incompatible-version".to_string(),
                message: format!(
                    "daemon speaks {PROTOCOL_V1}, client sent {}",
                    request.protocol_version
                ),
                retryable: false,
            }),
        );
        send(writer, &reply).await?;
        return Ok(false);
    }

    match &request.payload {
        // --- daemon lifecycle: served with NO handshake required ---
        Payload::Ping => {
            send(writer, &Envelope::reply_to(&request, Payload::Pong)).await?;
        }
        // A client's heartbeat reply; the read alone already reset the silence
        // counter, so nothing more is owed.
        Payload::Pong => {}
        Payload::DaemonStatusRequest => {
            let status = status(state).await?;
            send(
                writer,
                &Envelope::reply_to(&request, Payload::DaemonStatusResponse(status)),
            )
            .await?;
        }
        Payload::Shutdown => {
            send(writer, &Envelope::reply_to(&request, Payload::ShutdownAck)).await?;
            let _ = state.shutdown.send(true);
            return Ok(true);
        }

        // --- handshake ---
        Payload::ClientHello(hello) => {
            // A valid resume token restores the prior identity; an invalid or
            // expired one is ignored (proceed as a fresh client, do not drop).
            let client_id = hello
                .resume_token
                .as_ref()
                .and_then(|token| resume::verify_resume_token(&state.secret, &token.0))
                .map(|claims| claims.client_id)
                .unwrap_or(request.client_id);
            conn.client_id = Some(client_id);
            conn.handshaken = true;
            let server_hello = ServerHello {
                selected_protocol: PROTOCOL_V1,
                daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                daemon_instance: state.instance.instance_id,
                heartbeat_interval_ms: HEARTBEAT_INTERVAL_MS,
            };
            send(
                writer,
                &Envelope::reply_to(&request, Payload::ServerHello(server_hello)),
            )
            .await?;
        }

        // --- session interaction: requires a prior handshake ---
        Payload::Command(command) => {
            if !conn.handshaken {
                let reply = Envelope::reply_to(
                    &request,
                    Payload::CommandRejected(codypendent_protocol::CodypendentError::new(
                        "protocol.handshake-required",
                        "send a ClientHello before session commands",
                        false,
                    )),
                );
                send(writer, &reply).await?;
                return Ok(false);
            }

            match &command.body {
                // Attach is a connection-level concern the write path
                // deliberately rejects; intercept it here.
                CommandBody::AttachSession {
                    session_id,
                    last_seen_sequence,
                    subscriptions,
                    requested_role,
                } => {
                    conn.role = *requested_role;
                    handle_attach(
                        state,
                        writer,
                        conn,
                        forwarders,
                        &request,
                        *session_id,
                        last_seen_sequence.unwrap_or(0),
                        subscriptions.clone(),
                    )
                    .await?;
                }
                // Every other command flows through the crash-consistent write
                // path under the role recorded at attach (role enforcement is
                // inherited from the pipeline).
                _ => {
                    let ctx = ApplyContext {
                        client_id: conn.client_id_or(request.client_id),
                        role: conn.role,
                    };
                    let reply_envelope = match state
                        .commands
                        .apply(&state.pool, ctx, command.clone())
                        .await
                    {
                        Ok(outcome) => {
                            // A freshly accepted `StartRun` is handed to the
                            // executor so the run actually EXECUTES rather than
                            // sitting `Queued` forever. Fire-and-forget: the
                            // executor spawns its own task and we never await it.
                            // With no executor injected (lib-only / tests) this
                            // is a no-op — the run stays `Queued`, exactly as
                            // before.
                            if let (Some(run_id), Some(executor)) =
                                (outcome.created_run, state.executor.as_ref())
                            {
                                if let CommandBody::StartRun {
                                    session_id,
                                    objective,
                                    mode,
                                } = &command.body
                                {
                                    executor.spawn_run(RunLaunch {
                                        session_id: *session_id,
                                        run_id,
                                        objective: objective.clone(),
                                        mode: *mode,
                                        // Phase 1 carries no per-run repository
                                        // path on the wire, so fall back to the
                                        // daemon's working directory.
                                        repository: std::env::current_dir()
                                            .unwrap_or_else(|_| std::path::PathBuf::from(".")),
                                    });
                                }
                            }
                            let mut env = Envelope::reply_to(
                                &request,
                                Payload::CommandAccepted {
                                    command_id: outcome.command_id,
                                    sequence: outcome.last_sequence,
                                },
                            );
                            // Surface the created session id so a fresh client
                            // (`codypendent run`) can learn the session it just
                            // created. The `CommandAccepted` payload is
                            // intentionally minimal; the envelope's `session_id`
                            // field carries this connection-level metadata
                            // (Chapter 03).
                            if let Some(created) = outcome.created_session {
                                env.session_id = Some(created);
                            }
                            env
                        }
                        Err(error) => Envelope::reply_to(&request, Payload::CommandRejected(error)),
                    };
                    send(writer, &reply_envelope).await?;
                }
            }
        }

        // Anything else (including a future `Unknown` payload) is refused
        // structurally; the connection survives.
        other => {
            let reply = Envelope::reply_to(
                &request,
                Payload::Error(ProtocolError {
                    code: "protocol.unsupported-payload".to_string(),
                    message: format!("payload not handled in this phase: {other:?}"),
                    retryable: false,
                }),
            );
            send(writer, &reply).await?;
        }
    }
    Ok(false)
}

/// Register a connection's interest in a session: subscribe to its live stream,
/// reply with catch-up (missed events, or a snapshot when too far behind), and
/// spawn a task that forwards matching future events to this client.
#[allow(clippy::too_many_arguments)]
async fn handle_attach(
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    conn: &ConnState,
    forwarders: &mut Vec<JoinHandle<()>>,
    request: &Envelope,
    session_id: SessionId,
    last_seen: u64,
    subscriptions: Vec<Subscription>,
) -> anyhow::Result<()> {
    // Subscribe *before* computing catch-up so an event published during the
    // read cannot slip through the gap. An event committed between subscribing
    // and `load_events` is then delivered twice — once in catch-up, once on the
    // live receiver — so the forwarder drops anything at or below the catch-up
    // watermark (`current_max`) to avoid a double-render on the attach race.
    let receiver = state.subscriptions.subscribe(session_id);

    // Current max sequence (0 for an empty/absent session).
    let current_max = ledger::next_sequence(&state.pool, session_id)
        .await?
        .saturating_sub(1);
    let gap = current_max.saturating_sub(last_seen);

    let catchup = if gap <= CATCHUP_EVENT_LIMIT {
        // Cap replay at `current_max` — the live forwarder's drop watermark. An
        // event committed between reading `current_max` and this `load_events`
        // has sequence > current_max, so it is NOT dropped by the forwarder;
        // excluding it here keeps it delivered exactly once (live), instead of
        // both in catch-up and live.
        let events: Vec<SessionEvent> = ledger::load_events(&state.pool, session_id)
            .await?
            .into_iter()
            .filter(|event| event.sequence > last_seen && event.sequence <= current_max)
            .collect();
        Catchup::Events {
            from: last_seen + 1,
            through: current_max,
            events,
        }
    } else {
        let projection = projections::session_projection(&state.pool, session_id).await?;
        Catchup::Snapshot {
            through: current_max,
            projection,
        }
    };
    send(
        writer,
        &Envelope::reply_to(request, Payload::Catchup { catchup }),
    )
    .await?;

    let writer = Arc::clone(writer);
    let client_id = conn.client_id_or(request.client_id);
    let handle = tokio::spawn(forward_events(
        writer,
        receiver,
        subscriptions,
        client_id,
        session_id,
        current_max,
    ));
    forwarders.push(handle);
    Ok(())
}

/// Forward persisted session events to one attached client, filtered by its
/// subscription set. Never blocks the ledger: a lagging receiver skips the
/// missed span (the client re-attaches to catch up) and a vanished client ends
/// the task.
///
/// `catchup_through` is the last sequence the attach reply already delivered
/// (its `through`); events at or below it are dropped here, because subscribing
/// before catch-up can queue an event on the receiver that catch-up also
/// included — forwarding it again would double-render it on the client.
async fn forward_events(
    writer: SharedWriter,
    mut receiver: broadcast::Receiver<SessionEvent>,
    subscriptions: Vec<Subscription>,
    client_id: ClientId,
    session_id: SessionId,
    catchup_through: u64,
) {
    loop {
        match receiver.recv().await {
            Ok(event) => {
                // Already delivered in the catch-up reply — drop the overlap.
                if event.sequence <= catchup_through {
                    continue;
                }
                if !subscription_matches(&subscriptions, &event) {
                    continue;
                }
                let mut envelope = Envelope::request(client_id, Payload::Event(event));
                envelope.session_id = Some(session_id);
                if send(&writer, &envelope).await.is_err() {
                    break; // client gone
                }
            }
            // Slow consumer: skip the dropped span rather than stall the writer.
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Whether an event should be forwarded given a client's subscriptions. Phase 1
/// mapping: `SessionSummary`/`AgentActivity` receive every event; a
/// `RunTrace{run_id}` receives only that run's events; an empty set receives
/// everything. Views without a Phase 1 event mapping match nothing on their own.
fn subscription_matches(subscriptions: &[Subscription], event: &SessionEvent) -> bool {
    if subscriptions.is_empty() {
        return true;
    }
    subscriptions.iter().any(|subscription| match subscription {
        Subscription::SessionSummary | Subscription::AgentActivity => true,
        Subscription::RunTrace { run_id } => event_run_id(event) == Some(*run_id),
        _ => false,
    })
}

/// The run an event belongs to, if any (run-scoped events carry `run_id`).
fn event_run_id(event: &SessionEvent) -> Option<codypendent_protocol::RunId> {
    use codypendent_protocol::EventBody::*;
    match &event.body {
        RunStarted { run_id, .. }
        | RunStateChanged { run_id, .. }
        | ModelStreamDelta { run_id, .. }
        | ToolProposed { run_id, .. }
        | ToolStarted { run_id, .. }
        | ToolCompleted { run_id, .. }
        | PatchProposed { run_id, .. }
        | SteeringQueued { run_id }
        | SteeringApplied { run_id }
        | BudgetWarning { run_id, .. }
        | RunCompleted { run_id, .. } => Some(*run_id),
        _ => None,
    }
}

/// Frame one envelope onto the shared write half.
async fn send(writer: &SharedWriter, envelope: &Envelope) -> Result<(), FrameError> {
    let mut guard = writer.lock().await;
    write_envelope(&mut *guard, envelope).await
}

async fn status(state: &ServerState) -> anyhow::Result<DaemonStatus> {
    let uptime = Utc::now()
        .signed_duration_since(state.started_at)
        .num_seconds()
        .max(0) as u64;
    Ok(DaemonStatus {
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: PROTOCOL_V1,
        instance_id: state.instance.instance_id,
        pid: std::process::id(),
        started_at: state.started_at,
        uptime_seconds: uptime,
        boot_count: state.instance.boot_count,
        database_path: state
            .paths
            .data_dir
            .join("codypendent.db")
            .display()
            .to_string(),
        socket_path: state.paths.socket_path.display().to_string(),
        session_count: ledger::session_count(&state.pool).await?,
    })
}

/// Load the per-user resume-signing secret, creating it (32 random bytes, mode
/// 0600) on first boot. Unix-only in Phase 1.
fn load_or_create_secret(data_dir: &Path) -> anyhow::Result<Vec<u8>> {
    let path = data_dir.join("daemon.secret");
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() >= 32 {
            return Ok(bytes[..32].to_vec());
        }
        // A truncated secret is unusable; regenerate below.
    }
    let secret = random_secret();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Create the file with mode 0600 atomically, so the secret is never briefly
    // world-readable in the TOCTOU window a create-then-chmod would leave open.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(&secret)?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, &secret)?;
    Ok(secret)
}

/// 32 random bytes from `/dev/urandom`, or, if that is unavailable, derived from
/// two v4 UUIDs (16 bytes each).
fn random_secret() -> Vec<u8> {
    use std::io::Read;
    let mut buf = [0u8; 32];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        if file.read_exact(&mut buf).is_ok() {
            return buf.to_vec();
        }
    }
    let mut secret = Vec::with_capacity(32);
    secret.extend_from_slice(uuid::Uuid::now_v7().as_bytes());
    secret.extend_from_slice(uuid::Uuid::now_v7().as_bytes());
    secret
}

/// Opaque, daemon-signed resume tokens (STEP 1.11).
///
/// A token is `hex(payload_json) + "." + hex(signature)`, where the signature is
/// a keyed SHA-256 over the payload (`sha256(secret || payload || secret)`). The
/// payload carries the `client_id`, the last observed sequence, and a 24h
/// validity window; verification rejects a tampered signature or an expired
/// token.
mod resume {
    use chrono::{DateTime, Utc};
    use codypendent_protocol::ClientId;
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};

    /// A resume token is valid for 24 hours from issue.
    const TOKEN_TTL_HOURS: i64 = 24;

    /// The signed claims inside a resume token.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub(super) struct ResumeClaims {
        pub(super) client_id: ClientId,
        pub(super) last_sequence: u64,
        pub(super) issued_at: DateTime<Utc>,
        pub(super) expires_at: DateTime<Utc>,
    }

    /// Keyed SHA-256: `sha256(secret || payload || secret)`, hex-encoded.
    pub(super) fn sign(secret: &[u8], payload: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(secret);
        hasher.update(payload);
        hasher.update(secret);
        hex::encode(hasher.finalize())
    }

    /// Mint a token binding `client_id` + `last_sequence`, valid for 24h.
    #[allow(dead_code)] // Minting half of the pair: exercised by unit tests, issued to clients in later steps.
    pub(super) fn mint_resume_token(
        secret: &[u8],
        client_id: ClientId,
        last_sequence: u64,
    ) -> String {
        let issued_at = Utc::now();
        let claims = ResumeClaims {
            client_id,
            last_sequence,
            issued_at,
            expires_at: issued_at + chrono::Duration::hours(TOKEN_TTL_HOURS),
        };
        let payload = serde_json::to_vec(&claims).expect("resume claims serialize");
        let signature = sign(secret, &payload);
        format!("{}.{}", hex::encode(&payload), signature)
    }

    /// Verify a token, returning its claims iff the signature matches and it has
    /// not expired. A malformed, tampered, or expired token yields `None`.
    pub(super) fn verify_resume_token(secret: &[u8], token: &str) -> Option<ResumeClaims> {
        let (payload_hex, signature_hex) = token.split_once('.')?;
        let payload = hex::decode(payload_hex).ok()?;
        if sign(secret, &payload) != signature_hex {
            return None;
        }
        let claims: ResumeClaims = serde_json::from_slice(&payload).ok()?;
        if claims.expires_at <= Utc::now() {
            return None;
        }
        Some(claims)
    }
}

#[cfg(test)]
mod tests {
    use super::resume;
    use chrono::Utc;
    use codypendent_protocol::ClientId;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    #[test]
    fn resume_token_round_trips() {
        let client_id = ClientId::new();
        let token = resume::mint_resume_token(SECRET, client_id, 42);
        let claims = resume::verify_resume_token(SECRET, &token).expect("valid token verifies");
        assert_eq!(claims.client_id, client_id);
        assert_eq!(claims.last_sequence, 42);
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let token = resume::mint_resume_token(SECRET, ClientId::new(), 1);
        let mut chars: Vec<char> = token.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'a' { 'b' } else { 'a' };
        let tampered: String = chars.into_iter().collect();
        assert!(resume::verify_resume_token(SECRET, &tampered).is_none());
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let token = resume::mint_resume_token(SECRET, ClientId::new(), 1);
        assert!(resume::verify_resume_token(b"a-different-secret-of-32-bytes!!", &token).is_none());
    }

    #[test]
    fn expired_token_is_rejected() {
        let claims = resume::ResumeClaims {
            client_id: ClientId::new(),
            last_sequence: 5,
            issued_at: Utc::now() - chrono::Duration::hours(48),
            expires_at: Utc::now() - chrono::Duration::hours(24),
        };
        let payload = serde_json::to_vec(&claims).unwrap();
        let token = format!(
            "{}.{}",
            hex::encode(&payload),
            resume::sign(SECRET, &payload)
        );
        assert!(resume::verify_resume_token(SECRET, &token).is_none());
    }
}
