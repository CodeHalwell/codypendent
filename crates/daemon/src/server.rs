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
use crate::documents::{
    DocumentHub, DocumentLeaseReleaseRequest, DocumentLeaseRequest, DocumentLeaser,
    DocumentMutationRequest, DocumentMutator,
};
use crate::executor::{RunExecutor, RunLaunch};
use crate::instance::InstanceRecord;
use crate::ledger;
use crate::projections;
use crate::subscriptions::SubscriptionHub;
use crate::workflows::{StartWorkflowRequest, WorkflowStarter};

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
    /// Per-document CRDT-sync fan-out: a `MutateDocument` that applies publishes
    /// its sync here, and a client's `Subscription::Document` forwarder delivers
    /// from it (Phase 4 STEP 4.3).
    pub documents: DocumentHub,
    /// Applies an accepted `MutateDocument` onto the authoritative collaborative
    /// document. `None` in a lib-only / test embedding (the command is then
    /// rejected `document.transport-unavailable`); the assembly injects a
    /// knowledge-backed implementation (dependency inversion — see
    /// [`crate::documents`]).
    pub mutator: Option<Arc<dyn DocumentMutator>>,
    /// Acquires/releases the block-range edit leases gating `MutateDocument`.
    /// `None` in a lib-only / test embedding (lease commands are then rejected
    /// `document.transport-unavailable`); injected together with `mutator` by the
    /// assembly.
    pub leaser: Option<Arc<dyn DocumentLeaser>>,
    /// Creates a durable run from an accepted `StartWorkflow` (Phase 5 STEP 5.2).
    /// `None` in a lib-only / test embedding (the command is then rejected
    /// `workflow.transport-unavailable`); the assembly injects a
    /// `codypendent-workflow`-backed implementation over the pool.
    pub starter: Option<Arc<dyn WorkflowStarter>>,
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
    let listener = acquire_socket(&paths).await?;
    run_with_executor_on(listener, pool, paths, instance, executor).await
}

/// Bind the daemon socket (refusing if a live daemon owns it) and write the
/// pidfile. Split out so the assembly binary can claim single-instance
/// exclusivity **before** running startup recovery — a second daemon must never
/// get far enough to fail a live daemon's runs, relaunch its queued runs, or
/// wipe its code graph before discovering the socket is taken.
pub async fn acquire_socket(paths: &RuntimePaths) -> anyhow::Result<UnixListener> {
    prepare_socket(paths).await?;
    let listener = UnixListener::bind(&paths.socket_path)?;
    std::fs::write(&paths.pid_path, std::process::id().to_string())?;
    Ok(listener)
}

/// Like [`run_with_executor`], but on a pre-acquired [`acquire_socket`]
/// listener (the assembly binary acquires it before recovery).
pub async fn run_with_executor_on(
    listener: UnixListener,
    pool: SqlitePool,
    paths: RuntimePaths,
    instance: InstanceRecord,
    executor: Option<Arc<dyn RunExecutor>>,
) -> anyhow::Result<()> {
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

    // The document-transport seam, bundled with the executor by the assembly (as
    // its `collaborators` are). The per-document fan-out is created fresh here —
    // the server owns publishing (after a mutation applies) and subscribing (a
    // client's `Document` forwarder), and the mutator only computes the sync.
    let mutator = executor.as_ref().and_then(|e| e.document_mutator());
    let leaser = executor.as_ref().and_then(|e| e.document_leaser());
    let starter = executor.as_ref().and_then(|e| e.workflow_starter());
    let documents = DocumentHub::new();

    // Drive approval expiry: without a periodic caller, `expires_at` deadlines
    // are dead machinery — an approval with a deadline would simply never
    // expire at runtime. The same tick prunes session and document fan-out
    // channels whose last subscriber detached, so neither hub grows for the
    // daemon's lifetime. Aborted when the server stops.
    let expiry_task = {
        let broker = approvals.clone();
        let hub = subscriptions.clone();
        let doc_hub = documents.clone();
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match broker.expire_due(&pool, Utc::now()).await {
                    Ok(0) => {}
                    Ok(n) => info!(expired = n, "expired overdue approvals"),
                    Err(error) => warn!(%error, "approval expiry sweep failed"),
                }
                hub.prune_idle();
                doc_hub.prune_idle();
            }
        })
    };

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
        documents,
        mutator,
        leaser,
        starter,
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

    expiry_task.abort();
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
    /// Sessions this connection is attached to, with the role it attached under.
    /// On disconnect a `ClientPresenceChanged { present: false }` is published for
    /// each, so other clients see it leave (Phase 3 STEP 3.7).
    attached: Vec<(SessionId, ClientRole)>,
}

impl ConnState {
    fn new() -> Self {
        Self {
            client_id: None,
            role: ClientRole::Controller,
            handshaken: false,
            attached: Vec::new(),
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
    // Keyed by session: a re-attach to the same session on this connection
    // replaces (aborts) the prior forwarder instead of stacking a duplicate
    // that would double-deliver every live event.
    let mut forwarders: std::collections::HashMap<SessionId, JoinHandle<()>> =
        std::collections::HashMap::new();
    // Document forwarders grouped by the session attach that spawned them, so a
    // re-attach to a session replaces that session's whole document set: attaching
    // with a reduced `Document` list aborts the forwarders for the documents it no
    // longer names (mirrors the per-session replacement of `forwarders` above),
    // while another session's document forwarders are left untouched.
    let mut doc_forwarders: std::collections::HashMap<SessionId, Vec<JoinHandle<()>>> =
        std::collections::HashMap::new();

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
                match handle_request(
                    &state,
                    &writer,
                    &mut conn,
                    &mut forwarders,
                    &mut doc_forwarders,
                    request,
                )
                .await
                {
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
    // A slow or vanished client must never wedge a forwarder; drop them all —
    // both the session event forwarders and the document sync forwarders.
    for forwarder in forwarders.values() {
        forwarder.abort();
    }
    for handles in doc_forwarders.values() {
        for handle in handles {
            handle.abort();
        }
    }
    // Announce this client's departure from every session it was attached to, so
    // the remaining clients see it leave (STEP 3.7).
    if let Some(client_id) = conn.client_id {
        for (session_id, role) in &conn.attached {
            publish_presence(&state, *session_id, client_id, *role, false).await;
        }
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
    forwarders: &mut std::collections::HashMap<SessionId, JoinHandle<()>>,
    doc_forwarders: &mut std::collections::HashMap<SessionId, Vec<JoinHandle<()>>>,
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
                // Issue the token the verify path above consumes: the client
                // stores it opaquely and presents it on its next ClientHello,
                // resuming this identity across a client-process restart.
                resume_token: Some(codypendent_protocol::ResumeToken(
                    resume::mint_resume_token(&state.secret, client_id, 0),
                )),
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
                    // The requested role binds to the *connection* even when the
                    // attach itself is rejected (unknown session): role is a
                    // connection-level assertion under the Phase 1 local trust
                    // model, not a per-session grant.
                    conn.role = *requested_role;
                    let attached = handle_attach(
                        state,
                        writer,
                        conn,
                        forwarders,
                        doc_forwarders,
                        &request,
                        *session_id,
                        last_seen_sequence.unwrap_or(0),
                        subscriptions.clone(),
                    )
                    .await?;
                    // Remember the attachment so a detach presence event fires when
                    // this connection ends (STEP 3.7). De-duplicated by session: a
                    // re-attach on the same connection must not queue a second
                    // detach for the same client+session. A rejected attach must
                    // not be remembered — there is nothing to detach from.
                    if attached && !conn.attached.iter().any(|(s, _)| s == session_id) {
                        conn.attached.push((*session_id, *requested_role));
                    }
                }
                // IDE context is latest-wins, high-frequency projection state, not
                // a ledger command — upsert it directly and acknowledge, mirroring
                // the AttachSession interception above (Phase 3 STEP 3.4).
                CommandBody::UpdateIdeContext { session_id, update } => {
                    // Read-only clients must not overwrite the IDE-context
                    // projection the run read-path uses for provenance labeling.
                    if conn.role == ClientRole::Observer {
                        let reply = Envelope::reply_to(
                            &request,
                            Payload::CommandRejected(codypendent_protocol::CodypendentError::new(
                                "protocol.role-denied",
                                "an Observer may not update IDE context".to_string(),
                                false,
                            )),
                        );
                        send(writer, &reply).await?;
                        return Ok(false);
                    }
                    let reply = match crate::projections::upsert_ide_context(
                        &state.pool,
                        *session_id,
                        update,
                        chrono::Utc::now(),
                    )
                    .await
                    {
                        Ok(()) => Envelope::reply_to(
                            &request,
                            Payload::CommandAccepted {
                                command_id: command.command_id,
                                sequence: None,
                                created_run: None,
                            },
                        ),
                        Err(error) => Envelope::reply_to(
                            &request,
                            Payload::CommandRejected(codypendent_protocol::CodypendentError::new(
                                "ide.context-store-failed",
                                error.to_string(),
                                true,
                            )),
                        ),
                    };
                    send(writer, &reply).await?;
                }
                // A collaborative-document mutation is applied to the
                // authoritative Loro document (in `codypendent-knowledge`, reached
                // through the assembly's `DocumentMutator` seam), not the session
                // ledger — so, like `AttachSession`/`UpdateIdeContext`, it is
                // intercepted here rather than flowing through the event write
                // path (Phase 4 STEP 4.3).
                CommandBody::MutateDocument {
                    document_id,
                    mutation,
                } => {
                    // Role gate (the seam additionally enforces the document's
                    // collaboration mode and edit leases; this is the coarse role
                    // gate the daemon owns). An Observer may not mutate at all.
                    // Accepting/rejecting a suggestion *resolves* proposed content
                    // — it can apply an edit — so it mirrors `ResolveApproval`'s
                    // split in `commands.rs`: only an Approver or Controller may
                    // resolve. A Contributor may still propose (`Annotate`) and,
                    // where the mode allows, edit directly.
                    let resolves_suggestion = matches!(
                        mutation,
                        codypendent_protocol::DocumentMutation::AcceptSuggestion { .. }
                            | codypendent_protocol::DocumentMutation::RejectSuggestion { .. }
                    );
                    let permitted = if resolves_suggestion {
                        matches!(conn.role, ClientRole::Approver | ClientRole::Controller)
                    } else {
                        conn.role != ClientRole::Observer
                    };
                    if !permitted {
                        let message = if resolves_suggestion {
                            "only an Approver or Controller may resolve a document suggestion"
                        } else {
                            "an Observer may not mutate documents"
                        };
                        let reply = Envelope::reply_to(
                            &request,
                            Payload::CommandRejected(codypendent_protocol::CodypendentError::new(
                                "protocol.role-denied",
                                message.to_string(),
                                false,
                            )),
                        );
                        send(writer, &reply).await?;
                        return Ok(false);
                    }
                    // With no mutator injected (lib-only server / daemon tests)
                    // document transport is not enabled; reject structurally so the
                    // connection survives, mirroring the executor-less run path.
                    let Some(mutator) = state.mutator.as_ref() else {
                        let reply = Envelope::reply_to(
                            &request,
                            Payload::CommandRejected(codypendent_protocol::CodypendentError::new(
                                "document.transport-unavailable",
                                "document transport is not enabled on this daemon".to_string(),
                                false,
                            )),
                        );
                        send(writer, &reply).await?;
                        return Ok(false);
                    };
                    let mutate = DocumentMutationRequest {
                        document_id: *document_id,
                        mutation: mutation.clone(),
                        client_id: conn.client_id_or(request.client_id),
                    };
                    let reply = match mutator.apply_mutation(mutate).await {
                        Ok(sync) => {
                            // The mutation committed inside the seam; only now does
                            // its sync fan out to the document's subscribers
                            // (persist-before-publish, RULE 2). A subscriber's CRDT
                            // merge is idempotent, so a lost or duplicated sync
                            // self-heals — no watermark is needed here.
                            state.documents.publish(*document_id, sync);
                            Envelope::reply_to(
                                &request,
                                Payload::CommandAccepted {
                                    command_id: command.command_id,
                                    sequence: None,
                                    created_run: None,
                                },
                            )
                        }
                        Err(error) => Envelope::reply_to(&request, Payload::CommandRejected(error)),
                    };
                    send(writer, &reply).await?;
                }
                // Edit-lease acquire/release, intercepted at the connection level
                // like `MutateDocument` (leases live outside the session ledger).
                // A lease is a precursor to writing, so — as with a non-resolving
                // `MutateDocument` — an Observer may not take one.
                CommandBody::AcquireDocumentLease { lease, ttl_seconds } => {
                    if conn.role == ClientRole::Observer {
                        let reply = Envelope::reply_to(
                            &request,
                            Payload::CommandRejected(codypendent_protocol::CodypendentError::new(
                                "protocol.role-denied",
                                "an Observer may not acquire a document lease".to_string(),
                                false,
                            )),
                        );
                        send(writer, &reply).await?;
                        return Ok(false);
                    }
                    let Some(leaser) = state.leaser.as_ref() else {
                        let reply = Envelope::reply_to(
                            &request,
                            Payload::CommandRejected(codypendent_protocol::CodypendentError::new(
                                "document.transport-unavailable",
                                "document transport is not enabled on this daemon".to_string(),
                                false,
                            )),
                        );
                        send(writer, &reply).await?;
                        return Ok(false);
                    };
                    let acquire = DocumentLeaseRequest {
                        document_id: lease.document_id,
                        block_id: lease.block_id.clone(),
                        ttl: ttl_seconds.map(std::time::Duration::from_secs),
                        client_id: conn.client_id_or(request.client_id),
                    };
                    let reply = match leaser.acquire(acquire).await {
                        Ok(grant) => Envelope::reply_to(
                            &request,
                            Payload::DocumentLeaseGranted {
                                command_id: command.command_id,
                                grant,
                            },
                        ),
                        Err(error) => Envelope::reply_to(&request, Payload::CommandRejected(error)),
                    };
                    send(writer, &reply).await?;
                }
                CommandBody::ReleaseDocumentLease { lease_id } => {
                    if conn.role == ClientRole::Observer {
                        let reply = Envelope::reply_to(
                            &request,
                            Payload::CommandRejected(codypendent_protocol::CodypendentError::new(
                                "protocol.role-denied",
                                "an Observer may not release a document lease".to_string(),
                                false,
                            )),
                        );
                        send(writer, &reply).await?;
                        return Ok(false);
                    }
                    let Some(leaser) = state.leaser.as_ref() else {
                        let reply = Envelope::reply_to(
                            &request,
                            Payload::CommandRejected(codypendent_protocol::CodypendentError::new(
                                "document.transport-unavailable",
                                "document transport is not enabled on this daemon".to_string(),
                                false,
                            )),
                        );
                        send(writer, &reply).await?;
                        return Ok(false);
                    };
                    let release = DocumentLeaseReleaseRequest {
                        lease_id: lease_id.clone(),
                        client_id: conn.client_id_or(request.client_id),
                    };
                    let reply = match leaser.release(release).await {
                        Ok(()) => Envelope::reply_to(
                            &request,
                            Payload::CommandAccepted {
                                command_id: command.command_id,
                                sequence: None,
                                created_run: None,
                            },
                        ),
                        Err(error) => Envelope::reply_to(&request, Payload::CommandRejected(error)),
                    };
                    send(writer, &reply).await?;
                }
                // A `StartWorkflow` creates a durable run in the workflow store,
                // which lives outside the session ledger — so, like `MutateDocument`,
                // it is intercepted here and applied through the assembly's
                // `WorkflowStarter` seam rather than the event write path (Phase 5
                // STEP 5.2). Driving the created run is a later step.
                CommandBody::StartWorkflow { manifest, inputs } => {
                    // An Observer may not start a run.
                    if conn.role == ClientRole::Observer {
                        let reply = Envelope::reply_to(
                            &request,
                            Payload::CommandRejected(codypendent_protocol::CodypendentError::new(
                                "protocol.role-denied",
                                "an Observer may not start a workflow".to_string(),
                                false,
                            )),
                        );
                        send(writer, &reply).await?;
                        return Ok(false);
                    }
                    // With no starter injected (lib-only server / daemon tests)
                    // workflow transport is not enabled; reject structurally so the
                    // connection survives, mirroring the executor-less run path.
                    let Some(starter) = state.starter.as_ref() else {
                        let reply = Envelope::reply_to(
                            &request,
                            Payload::CommandRejected(codypendent_protocol::CodypendentError::new(
                                "workflow.transport-unavailable",
                                "workflow transport is not enabled on this daemon".to_string(),
                                false,
                            )),
                        );
                        send(writer, &reply).await?;
                        return Ok(false);
                    };
                    let start = StartWorkflowRequest {
                        manifest: manifest.clone(),
                        inputs: inputs.clone(),
                        client_id: conn.client_id_or(request.client_id),
                    };
                    let reply = match starter.start(start).await {
                        Ok(workflow_run_id) => Envelope::reply_to(
                            &request,
                            Payload::WorkflowRunStarted {
                                command_id: command.command_id,
                                workflow_run_id,
                            },
                        ),
                        Err(error) => Envelope::reply_to(&request, Payload::CommandRejected(error)),
                    };
                    send(writer, &reply).await?;
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
                            // Gate on `newly_applied`: a duplicate `StartRun`
                            // delivery replays the recorded outcome (with the same
                            // `created_run`), and launching again would run two
                            // agent loops for one run. A replayed outcome is never
                            // `newly_applied`, so the executor fires exactly once.
                            if let (true, Some(run_id), Some(executor)) = (
                                outcome.newly_applied,
                                outcome.created_run,
                                state.executor.as_ref(),
                            ) {
                                if let CommandBody::StartRun {
                                    session_id,
                                    objective,
                                    mode,
                                    repository,
                                } = &command.body
                                {
                                    executor.spawn_run(RunLaunch {
                                        session_id: *session_id,
                                        run_id,
                                        objective: objective.clone(),
                                        mode: *mode,
                                        // The run carries its own repository root
                                        // so a shared daemon attributes it to the
                                        // right checkout (issue #6 item 1); an
                                        // older client that sends none falls back
                                        // to the daemon's working directory.
                                        repository: repository
                                            .as_ref()
                                            .map(std::path::PathBuf::from)
                                            .unwrap_or_else(|| {
                                                std::env::current_dir().unwrap_or_else(|_| {
                                                    std::path::PathBuf::from(".")
                                                })
                                            }),
                                    });
                                }
                            }
                            // A `CancelRun` must also reach the LIVE runtime loop:
                            // recording `Cancelled` in the projection does not stop
                            // the agent, so signal the executor's per-run
                            // cancellation token. Idempotent and best-effort — a
                            // no-op with no executor injected or an already-finished
                            // run. (No `newly_applied` gate: cancellation is
                            // idempotent, and a re-delivered cancel should still be
                            // free to stop a run the first delivery raced.)
                            if let (Some(executor), CommandBody::CancelRun { run_id }) =
                                (state.executor.as_ref(), &command.body)
                            {
                                executor.cancel_run(*run_id);
                            }
                            let mut env = Envelope::reply_to(
                                &request,
                                Payload::CommandAccepted {
                                    command_id: outcome.command_id,
                                    sequence: outcome.last_sequence,
                                    // Bind the issuing client to exactly the run
                                    // its StartRun created (None otherwise).
                                    created_run: outcome.created_run,
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
/// spawn a task that forwards matching future events to this client. Returns
/// whether the attach was accepted (`false` = unknown session, error replied).
#[allow(clippy::too_many_arguments)]
async fn handle_attach(
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    conn: &ConnState,
    forwarders: &mut std::collections::HashMap<SessionId, JoinHandle<()>>,
    doc_forwarders: &mut std::collections::HashMap<SessionId, Vec<JoinHandle<()>>>,
    request: &Envelope,
    session_id: SessionId,
    last_seen: u64,
    subscriptions: Vec<Subscription>,
) -> anyhow::Result<bool> {
    // Reject an attach to a session this daemon has never seen. An empty
    // catch-up here used to make a typo'd id indistinguishable from a valid
    // empty session — the client then bound a blank UI to a dead id whose
    // every `StartRun` rejected `session-not-found`. Clients that probe a
    // remembered id (the TUI's resume flow) treat a non-`Catchup` reply as
    // "gone" and fall through to creating a fresh session.
    if !ledger::session_exists(&state.pool, session_id).await? {
        let reply = Envelope::reply_to(
            request,
            Payload::Error(ProtocolError {
                code: "protocol.session-not-found".to_string(),
                message: format!("no session {session_id}"),
                retryable: false,
            }),
        );
        send(writer, &reply).await?;
        return Ok(false);
    }

    // Subscribe *before* computing catch-up so an event published during the
    // read cannot slip through the gap. An event committed between subscribing
    // and the window read is then delivered twice — once in catch-up, once on
    // the live receiver — so the forwarder drops anything at or below the
    // catch-up watermark (`current_max`) to avoid a double-render on the
    // attach race.
    let receiver = state.subscriptions.subscribe(session_id);

    // Current max sequence (0 for an empty session).
    let current_max = ledger::next_sequence(&state.pool, session_id)
        .await?
        .saturating_sub(1);
    let gap = current_max.saturating_sub(last_seen);

    let catchup = if gap <= CATCHUP_EVENT_LIMIT {
        // Cap replay at `current_max` — the live forwarder's drop watermark. An
        // event committed between reading `current_max` and this window read
        // has sequence > current_max, so it is NOT dropped by the forwarder;
        // excluding it here keeps it delivered exactly once (live), instead of
        // both in catch-up and live. The window is filtered in SQL so the read
        // costs the gap, not the whole session history.
        let events: Vec<SessionEvent> =
            ledger::load_events_between(&state.pool, session_id, last_seen, current_max).await?;
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

    let client_id = conn.client_id_or(request.client_id);

    // Reconcile this session's document forwarders. Abort the ones its *previous*
    // attach spawned first, so a re-attach with a reduced (or empty) `Document`
    // set stops delivering syncs for the documents it no longer names — then spawn
    // the new set. Document syncs ride a separate, document-keyed fan-out (not the
    // session hub) and are delivered as `Payload::DocumentSync`; a subscriber's
    // baseline comes from the document read path, this stream carries the
    // post-subscribe updates it merges. Done before the session forwarder below
    // consumes `writer`/`subscriptions`.
    if let Some(previous) = doc_forwarders.remove(&session_id) {
        for handle in previous {
            handle.abort();
        }
    }
    let new_doc_forwarders: Vec<JoinHandle<()>> = subscriptions
        .iter()
        .filter_map(|subscription| match subscription {
            Subscription::Document { document_id } => {
                let receiver = state.documents.subscribe(*document_id);
                Some(tokio::spawn(forward_document_syncs(
                    Arc::clone(writer),
                    receiver,
                    client_id,
                )))
            }
            _ => None,
        })
        .collect();
    if !new_doc_forwarders.is_empty() {
        doc_forwarders.insert(session_id, new_doc_forwarders);
    }

    let writer = Arc::clone(writer);
    let handle = tokio::spawn(forward_events(
        writer,
        receiver,
        subscriptions,
        client_id,
        session_id,
        current_max,
    ));
    if let Some(previous) = forwarders.insert(session_id, handle) {
        previous.abort();
    }

    // Announce this client's arrival so other attached clients (e.g. the TUI
    // during a handoff to VS Code) see it join. Emitted after the forwarder is
    // live so the arriving client also receives its own presence event.
    publish_presence(state, session_id, client_id, conn.role, true).await;
    Ok(true)
}

/// Append a `ClientPresenceChanged` event and fan it out to the session's
/// attached clients (persist-before-publish). A failure is logged, never fatal —
/// presence is a convenience signal, not a correctness gate.
async fn publish_presence(
    state: &Arc<ServerState>,
    session_id: SessionId,
    client_id: ClientId,
    role: ClientRole,
    present: bool,
) {
    match ledger::append_next_event(
        &state.pool,
        session_id,
        &codypendent_protocol::Actor::Client { client_id },
        &codypendent_protocol::EventBody::ClientPresenceChanged {
            client_id,
            role,
            present,
        },
        chrono::Utc::now(),
    )
    .await
    {
        Ok(event) => state.subscriptions.publish(session_id, event),
        Err(error) => tracing::warn!(%error, "could not record client presence"),
    }
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

/// Forward a document's live CRDT syncs to one subscribed client, framing each
/// as a [`Payload::DocumentSync`]. Never blocks the publisher: a lagging receiver
/// skips the dropped span (its next merge reconverges — CRDT updates are
/// idempotent snapshots) and a vanished client ends the task. Document syncs are
/// not session-scoped, so the frame carries no `session_id`; the client routes by
/// the sync's own `document_id`.
async fn forward_document_syncs(
    writer: SharedWriter,
    mut receiver: broadcast::Receiver<codypendent_protocol::DocumentSync>,
    client_id: ClientId,
) {
    loop {
        match receiver.recv().await {
            Ok(sync) => {
                let envelope = Envelope::request(client_id, Payload::DocumentSync(sync));
                if send(&writer, &envelope).await.is_err() {
                    break; // client gone
                }
            }
            // Slow consumer: skip the dropped span; the next sync reconverges.
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
/// A token is `hex(payload_json) + "." + hex(signature)`, where the signature
/// is HMAC-SHA256 over the payload. HMAC (not an ad-hoc keyed hash: the
/// original `sha256(secret‖payload‖secret)` sandwich has no security proof and
/// invites length-extension-shaped mistakes) with the `Mac` API's
/// constant-time verification (a `==` string compare leaks a timing oracle on
/// the signature prefix). The payload carries the `client_id`, the last
/// observed sequence, and a 24h validity window; verification rejects a
/// tampered signature or an expired token.
mod resume {
    use chrono::{DateTime, Utc};
    use codypendent_protocol::ClientId;
    use hmac::{Hmac, Mac};
    use serde::{Deserialize, Serialize};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

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

    /// HMAC-SHA256 over `payload`, keyed by `secret`. HMAC accepts any key
    /// length, so construction cannot fail.
    fn mac(secret: &[u8], payload: &[u8]) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(secret).expect("hmac accepts any key length");
        mac.update(payload);
        mac
    }

    /// The hex-encoded signature for `payload` (mint-side; verification goes
    /// through [`Mac::verify_slice`], never a string compare).
    pub(super) fn sign(secret: &[u8], payload: &[u8]) -> String {
        hex::encode(mac(secret, payload).finalize().into_bytes())
    }

    /// Mint a token binding `client_id` + `last_sequence`, valid for 24h.
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

    /// Verify a token, returning its claims iff the signature matches (in
    /// constant time) and it has not expired. A malformed, tampered, or
    /// expired token yields `None`.
    pub(super) fn verify_resume_token(secret: &[u8], token: &str) -> Option<ResumeClaims> {
        let (payload_hex, signature_hex) = token.split_once('.')?;
        let payload = hex::decode(payload_hex).ok()?;
        let signature = hex::decode(signature_hex).ok()?;
        mac(secret, &payload).verify_slice(&signature).ok()?;
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
