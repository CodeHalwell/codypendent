//! The interactive TUI harness (STEP 1.12).
//!
//! Running `codypendent` with no subcommand opens the Ratatui client attached to
//! the current repository's session (creating it if needed, auto-starting the
//! daemon if needed). The rendering, input mapping, and reducer all live in the
//! pure `codypendent-tui` crate, which performs no I/O; this module is the
//! *harness* that the crate's own docs describe — it owns the protocol
//! connection, the terminal, and the event loop, and it is the only place the
//! two worlds meet.
//!
//! # The loop
//!
//! ```text
//!   crossterm event ─┐                        ┌─▶ reduce(&mut AppState, action)
//!   daemon event ────┼─▶ tokio::select! ─▶ Action        │
//!   200ms tick ──────┘                                   ├─▶ drain outbox → Commands
//!                                                         └─▶ render(frame, &state)
//! ```
//!
//! Two background tasks decouple the socket from the loop so a keystroke never
//! cancels a half-read frame (RULE: no partial-frame loss): a **reader** task
//! owns the read half and forwards each live [`SessionEvent`] to the loop (and
//! answers heartbeat `Ping`s via the writer), and a **writer** task owns the
//! write half and serializes every outgoing envelope — commands from the loop
//! and pongs from the reader. A third OS thread bridges blocking `crossterm`
//! input into the async loop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use codypendent_knowledge::{
    db as knowledge_db, BlockContent, CapabilityRequest, CodeEdge, CodeRelation, CollaborationMode,
    DocumentAuthor, DocumentBlock, DocumentStore, EvidenceKind, EvidenceRef, KnowledgeDocument,
    MemoryClass, MemoryRecord, MemoryStore, Registry, RegistryItem, RegistryItemKind,
    RegistryStatus, RiskClass, Scope, Suggestion, SuggestionStatus, SuggestionStore, TrustTier,
};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    read_envelope, write_envelope, Catchup, ClientId, ClientRole, CodeNodeId, Command, CommandBody,
    CommandId, Envelope, Payload, RepositoryId, SessionEvent, SessionId, Subscription, WorkspaceId,
};
use codypendent_tui::{
    map_event, reduce, render, Action, AppState, DocBlockView, DocCard, DocSuggestionView,
    GraphEdgeCard, Intent, MemoryCard, SkillCard, TerminalGuard, Theme,
};
use crossterm::event::Event as CrosstermEvent;
use serde::{Deserialize, Serialize};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use crate::commands;
use crate::connection::Connection;

/// How often the loop wakes with a [`Action::Tick`] for spinner / elapsed-timer
/// animation when nothing else is happening (5 fps — cheap, and the loop redraws
/// immediately on any real event anyway).
const TICK: Duration = Duration::from_millis(200);

/// The client-facing subscription set for the TUI: it wants the whole session,
/// not one run's trace.
fn default_subscriptions() -> Vec<Subscription> {
    vec![Subscription::SessionSummary, Subscription::AgentActivity]
}

/// `codypendent` with no subcommand: open the interactive TUI for `repo`.
///
/// Auto-starts the daemon, resolves (or creates) `repo`'s session, attaches with
/// catch-up, and runs the event loop until the user detaches (`q`) or the daemon
/// closes the stream. Detaching never affects the run — the daemon keeps it
/// going; a later `codypendent` reopens the same session and catches up.
pub async fn run(paths: &RuntimePaths, repo: PathBuf) -> anyhow::Result<()> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("{}: not a valid, accessible directory", repo.display()))?;
    if !repo.is_dir() {
        bail!("{}: not a directory", repo.display());
    }

    commands::ensure_daemon(paths).await?;
    let mut conn = Connection::connect(&paths.socket_path).await?;
    let mut store = SessionStore::load(paths);
    let resume = store
        .resume_token
        .clone()
        .map(codypendent_protocol::ResumeToken);
    let hello = conn
        .handshake("codypendent-tui", env!("CARGO_PKG_VERSION"), resume)
        .await?;
    // Store the daemon-issued token so the NEXT launch resumes this client
    // identity (best-effort; an absent token just means a fresh identity).
    if let Some(token) = hello.resume_token {
        store.resume_token = Some(token.0);
        store.save(paths);
    }

    let (session_id, workspace_id, catchup) =
        resolve_or_create_session(&mut conn, &mut store, paths, &repo).await?;

    // Seed the state from catch-up, then from any live event that outraced the
    // attach reply and was buffered during setup — both before the loop reads a
    // single new frame, so no event is dropped or reordered.
    let mut state = AppState::new();
    let attach_watermark = fold_catchup(&mut state, catchup);
    let (read_half, write_half, pending, client_id) = conn.into_split();
    for envelope in pending {
        if let Payload::Event(event) = envelope.payload {
            reduce(&mut state, Action::DaemonEvent(Box::new(event)));
        }
    }

    // STEP 2.6 + Phase 4 client wiring: seed the Skill Studio, memory browser,
    // Docs Studio, and code-graph edge-inspector projections. This reads the
    // knowledge fabric's authoritative rows directly from SQLite (WAL allows
    // concurrent reads alongside the daemon) and maps them into the TUI's plain
    // projection structs — the one place the two worlds meet, done here (not in
    // the pure TUI crate, which never depends on `codypendent-knowledge`). A read
    // failure logs and continues with empty lists; it never fails the TUI. Done
    // before entering the terminal so any diagnostic reaches a cooked screen.
    let projections = load_knowledge(paths, workspace_id, &repo).await;
    state.skills = projections.skills;
    state.memories = projections.memories;
    state.docs = projections.docs;
    state.edges = projections.edges;

    // Wire the two socket tasks. Start them before the terminal so no live
    // event or heartbeat is missed during setup.
    let (out_tx, out_rx) = mpsc::channel::<Envelope>(256);
    let (event_tx, mut event_rx) = mpsc::channel::<ReaderSignal>(256);
    let reader = tokio::spawn(read_loop(read_half, event_tx, out_tx.clone(), client_id));
    let writer = tokio::spawn(write_loop(write_half, out_rx));

    // Enter raw mode + the alternate screen. RAII restores the terminal on any
    // exit path, including a panic mid-loop. Done before spawning the input
    // bridge so it never reads a keystroke in cooked mode, and so a non-TTY
    // error returns here without a stray thread to wind down.
    let mut guard = TerminalGuard::enter().context(
        "the interactive TUI needs a terminal (a TTY); for headless use run \
         `codypendent run --jsonl` instead",
    )?;

    let (input_tx, mut input_rx) = mpsc::channel::<CrosstermEvent>(256);
    let input_running = Arc::new(AtomicBool::new(true));
    spawn_input_thread(input_tx, Arc::clone(&input_running));

    let theme = Theme::dark();
    let (mut width, _) = crossterm::terminal::size().unwrap_or((80, 24));

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let repository = repo.to_string_lossy().into_owned();
    let result = event_loop(
        &mut guard,
        &theme,
        &mut state,
        &mut width,
        &mut event_rx,
        &mut input_rx,
        &mut ticker,
        &out_tx,
        client_id,
        session_id,
        &repository,
        attach_watermark,
    )
    .await;

    // Teardown: stop the input thread, restore the terminal *before* any trailing
    // error text reaches the (now cooked) screen, then wind down the socket tasks.
    input_running.store(false, Ordering::Relaxed);
    drop(guard);
    drop(out_tx);
    reader.abort();
    writer.abort();
    result
}

/// The render/reduce/dispatch loop. Broken out from [`run`] so the setup and
/// teardown read linearly and the borrow of every loop input is explicit.
#[allow(clippy::too_many_arguments)]
async fn event_loop(
    guard: &mut TerminalGuard,
    theme: &Theme,
    state: &mut AppState,
    width: &mut u16,
    event_rx: &mut mpsc::Receiver<ReaderSignal>,
    input_rx: &mut mpsc::Receiver<CrosstermEvent>,
    ticker: &mut tokio::time::Interval,
    out_tx: &mpsc::Sender<Envelope>,
    client_id: ClientId,
    session_id: SessionId,
    repository: &str,
    attach_watermark: u64,
) -> anyhow::Result<()> {
    guard
        .terminal_mut()
        .draw(|frame| render(frame, state, theme))?;

    // The highest ledger sequence folded so far. Live fan-out is lossy for a
    // slow client (the daemon skips `Lagged` spans), so a jump past
    // `last_seen + 1` means events were dropped from the live view; a
    // re-attach with `last_seen_sequence` replays exactly the gap. Also
    // dedups: catch-up + live can overlap during that window.
    let mut last_seen: u64 = attach_watermark;

    loop {
        let action = tokio::select! {
            signal = event_rx.recv() => match signal {
                Some(ReaderSignal::Event(event)) => {
                    if event.sequence != 0 && event.sequence <= last_seen {
                        Action::NoOp // duplicate of something already folded
                    } else {
                        if last_seen != 0 && event.sequence > last_seen + 1 {
                            // Gap: re-attach to replay the missed span. The
                            // daemon replaces this connection's forwarder and
                            // replies with a Catchup the reader forwards back.
                            let attach = command_envelope(
                                client_id,
                                CommandBody::AttachSession {
                                    session_id,
                                    last_seen_sequence: Some(last_seen),
                                    subscriptions: default_subscriptions(),
                                    requested_role: ClientRole::Controller,
                                },
                            );
                            let _ = out_tx.send(attach).await;
                        }
                        last_seen = last_seen.max(event.sequence);
                        Action::DaemonEvent(event)
                    }
                }
                Some(ReaderSignal::Rejected { code, message }) => {
                    Action::Notice(format!("command rejected: {message} ({code})"))
                }
                Some(ReaderSignal::Catchup(catchup)) => {
                    // Fold the gap replay; live events past it resume above.
                    if let Catchup::Events { events, through, .. } = *catchup {
                        for event in events {
                            if event.sequence > last_seen {
                                last_seen = last_seen.max(event.sequence);
                                reduce(state, Action::DaemonEvent(Box::new(event)));
                            }
                        }
                        last_seen = last_seen.max(through);
                    }
                    Action::NoOp
                }
                // The daemon closed the stream (shutdown / dropped client). The
                // run is unaffected; we simply leave the TUI.
                Some(ReaderSignal::Closed) | None => return Ok(()),
            },
            input = input_rx.recv() => match input {
                // Track width for mouse-column → pane mapping; the draw below
                // re-reads the real size, so a resize just needs a redraw.
                Some(CrosstermEvent::Resize(w, _)) => { *width = w; Action::NoOp }
                Some(event) => map_event(&event, state.input_mode(), *width),
                None => return Ok(()), // input bridge ended
            },
            _ = ticker.tick() => Action::Tick,
        };

        reduce(state, action);

        for intent in state.drain_outbox() {
            let envelope =
                command_envelope(client_id, intent_to_command(intent, session_id, repository));
            if out_tx.send(envelope).await.is_err() {
                return Ok(()); // writer gone → connection is down; leave cleanly
            }
        }

        if state.should_detach || state.session_closed {
            return Ok(());
        }

        guard
            .terminal_mut()
            .draw(|frame| render(frame, state, theme))?;
    }
}

/// What the reader task forwards to the loop.
enum ReaderSignal {
    /// A live session event to fold into state (boxed: it is a large payload and
    /// every other message here is tiny).
    Event(Box<SessionEvent>),
    /// The daemon rejected a command this TUI sent (code + message). Surfaced
    /// as a transient status notice — silence here meant a rejected StartRun
    /// showed the user nothing at all.
    Rejected { code: String, message: String },
    /// A catch-up reply (from the loop's own gap-triggered re-attach).
    Catchup(Box<Catchup>),
    /// The daemon closed the connection.
    Closed,
}

/// Own the read half: forward each live [`SessionEvent`], answer heartbeat
/// `Ping`s through the writer, and signal `Closed` on EOF or error. Runs to
/// completion of each `read_envelope` (never cancelled by the loop's `select!`),
/// so a frame is never torn in half.
async fn read_loop(
    mut read_half: OwnedReadHalf,
    event_tx: mpsc::Sender<ReaderSignal>,
    out_tx: mpsc::Sender<Envelope>,
    client_id: ClientId,
) {
    loop {
        match read_envelope(&mut read_half).await {
            Ok(Some(envelope)) => match envelope.payload {
                Payload::Ping => {
                    let pong = Envelope::request(client_id, Payload::Pong);
                    if out_tx.send(pong).await.is_err() {
                        break;
                    }
                }
                Payload::Event(event) => {
                    if event_tx
                        .send(ReaderSignal::Event(Box::new(event)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Payload::CommandRejected(error) => {
                    if event_tx
                        .send(ReaderSignal::Rejected {
                            code: error.code,
                            message: error.message,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Payload::Catchup { catchup } => {
                    if event_tx
                        .send(ReaderSignal::Catchup(Box::new(catchup)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                // Everything else (CommandAccepted, stray replies) is not an
                // input to the reducer's live loop — the TUI's state is driven
                // by durable events (Chapter 03). Drop it.
                _ => {}
            },
            Ok(None) | Err(_) => {
                let _ = event_tx.send(ReaderSignal::Closed).await;
                break;
            }
        }
    }
}

/// Own the write half: serialize every outgoing envelope (loop commands + reader
/// pongs) so the two producers never interleave a frame on the socket.
async fn write_loop(mut write_half: OwnedWriteHalf, mut out_rx: mpsc::Receiver<Envelope>) {
    while let Some(envelope) = out_rx.recv().await {
        if write_envelope(&mut write_half, &envelope).await.is_err() {
            break;
        }
    }
}

/// Bridge blocking `crossterm` input into the async loop on a dedicated OS
/// thread. Polls with a short timeout so it observes `running` going false
/// promptly; sends each event over `tx` until the loop drops the receiver.
fn spawn_input_thread(tx: mpsc::Sender<CrosstermEvent>, running: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(event) => {
                        if tx.blocking_send(event).is_err() {
                            break; // loop ended
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => continue, // timed out; re-check `running`
                Err(_) => break,
            }
        }
    });
}

/// Map a reducer [`Intent`] to the wire [`CommandBody`], binding the session id
/// the TUI is attached to. A pure 1:1 translation — the whole point of the
/// outbox is that `reduce` stays I/O-free and this is the only place intents
/// become protocol.
fn intent_to_command(intent: Intent, session_id: SessionId, repository: &str) -> CommandBody {
    match intent {
        Intent::StartRun { objective, mode } => CommandBody::StartRun {
            session_id,
            objective,
            mode,
            // Attribute the run to the repository this TUI is attached to, so a
            // shared daemon does not store its memories under its own directory
            // (issue #6 item 1).
            repository: Some(repository.to_owned()),
        },
        Intent::ResolveApproval {
            approval_id,
            decision,
            scope,
        } => CommandBody::ResolveApproval {
            approval_id,
            decision,
            scope,
        },
        Intent::PauseRun { run_id } => CommandBody::PauseRun { run_id },
        Intent::ResumeRun { run_id } => CommandBody::ResumeRun { run_id },
        Intent::CancelRun { run_id } => CommandBody::CancelRun { run_id },
        Intent::QueueSteering { run_id, text } => CommandBody::QueueSteering { run_id, text },
    }
}

/// Wrap a command in a fresh, self-idempotent request envelope (the command id's
/// own string is the idempotency key, so a client-side retry reuses it — same
/// contract as [`Connection::send_command`](crate::connection::Connection)).
fn command_envelope(client_id: ClientId, body: CommandBody) -> Envelope {
    let command_id = CommandId::new();
    Envelope::request(
        client_id,
        Payload::Command(Command {
            command_id,
            idempotency_key: command_id.to_string(),
            expected_revision: None,
            body,
        }),
    )
}

/// Fold an attach-time [`Catchup`] into fresh state. `Catchup::Events` replays
/// each missed event through the reducer; `Catchup::Snapshot` (the client was
/// too far behind — Chapter 03's cap) and a future `Unknown` variant carry no
/// individual events, so the transcript simply begins from the next live event
/// (Phase 1 keeps runs short enough that this is rare).
fn fold_catchup(state: &mut AppState, catchup: Catchup) -> u64 {
    match catchup {
        Catchup::Events {
            events, through, ..
        } => {
            for event in events {
                reduce(state, Action::DaemonEvent(Box::new(event)));
            }
            through
        }
        // Too far behind for an event replay — fold the projection so a reopened
        // long-running session shows its title + active runs instead of blank.
        Catchup::Snapshot {
            through,
            projection,
        } => {
            reduce(
                state,
                Action::CatchupSnapshot {
                    title: projection.title,
                    closed: projection.closed,
                    runs: projection.active_runs,
                },
            );
            through
        }
        _ => 0,
    }
}

/// Resolve the session for `repo`, reusing the one this repo last used when it
/// still exists, otherwise creating a fresh one. Returns the session id and its
/// attach-time catch-up. This is what makes "close the TUI, reopen, the run
/// continued" work: the mapping persists across launches.
async fn resolve_or_create_session(
    conn: &mut Connection,
    store: &mut SessionStore,
    paths: &RuntimePaths,
    repo: &Path,
) -> anyhow::Result<(SessionId, WorkspaceId, Catchup)> {
    let key = repo.to_string_lossy().into_owned();

    // Try to resume the repo's remembered session.
    if let Some(stored) = store.sessions.get(&key).copied() {
        let reply = conn
            .send_command(CommandBody::AttachSession {
                session_id: stored.session_id,
                last_seen_sequence: None,
                subscriptions: default_subscriptions(),
                requested_role: ClientRole::Controller,
            })
            .await?;
        // Only resume when the catch-up proves the session still exists. The
        // daemon can't tell an *absent* session from an empty one — it reports max
        // sequence 0 for both and replies with an empty `Catchup`, never a
        // rejection — but a real session always replays at least its
        // `SessionCreated` event (sequence 1). Accepting a zero-event catch-up
        // would open a blank TUI bound to a dead id whose every `StartRun` is then
        // rejected `session-not-found`; instead fall through and create a fresh
        // session, keeping the workspace. (issue #6 item 6)
        if let Payload::Catchup { catchup } = reply.payload {
            // A CLOSED session resumes technically (through > 0) but the event
            // loop exits the moment it folds `SessionClosed` — and with the
            // store never overwritten, every later launch would re-open the
            // closed session and instantly exit: a permanent lockout. Treat
            // closed like missing and fall through to create a fresh session.
            if catchup_proves_session_exists(&catchup) && !catchup_shows_closed(&catchup) {
                return Ok((stored.session_id, stored.workspace_id, catchup));
            }
        }
        // Rejected, or a zero-event catch-up to a session the daemon no longer has
        // (fresh data dir, GC'd, or closed): fall through and create a new one.
    }

    // Create a new session (reusing this repo's workspace id if we have one, so a
    // recreated session still belongs to the same logical workspace).
    let workspace = store
        .sessions
        .get(&key)
        .map(|s| s.workspace_id)
        .unwrap_or_default();
    let title = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| key.clone());

    let created = conn
        .send_command(CommandBody::CreateSession { workspace, title })
        .await?;
    let session_id = match &created.payload {
        Payload::CommandAccepted { .. } => created.session_id.ok_or_else(|| {
            anyhow!("daemon accepted CreateSession but its reply carried no session_id")
        })?,
        Payload::CommandRejected(error) => {
            bail!("CreateSession rejected: {} ({})", error.message, error.code)
        }
        other => bail!("unexpected reply to CreateSession: {other:?}"),
    };

    let attach = conn
        .send_command(CommandBody::AttachSession {
            session_id,
            last_seen_sequence: None,
            subscriptions: default_subscriptions(),
            requested_role: ClientRole::Controller,
        })
        .await?;
    let catchup = commands::expect_catchup(attach)?;

    store.sessions.insert(
        key,
        StoredSession {
            session_id,
            workspace_id: workspace,
        },
    );
    store.save(paths); // best-effort: a persistence miss only costs the next
                       // launch a fresh session, never correctness.
    Ok((session_id, workspace, catchup))
}

/// Whether an attach-time [`Catchup`] proves its session still exists in the
/// daemon. A live session always replays at least its `SessionCreated` event, so
/// its watermark is `>= 1`; the daemon reports `0` for an absent session (it
/// cannot distinguish "gone" from "empty"). An unrecognized future variant is
/// accepted rather than needlessly discarding a resumable session — the concrete
/// failure (issue #6 item 6) is specifically the provably-empty catch-up.
/// Whether an attach-time [`Catchup`] shows the session is already CLOSED — a
/// `SessionClosed` in the replayed events, or the snapshot's `closed` flag. A
/// closed session must not be resumed from the store: the event loop exits the
/// moment it folds the close, and the remembered mapping would re-open it on
/// every later launch (a permanent lockout).
fn catchup_shows_closed(catchup: &Catchup) -> bool {
    match catchup {
        Catchup::Events { events, .. } => events
            .iter()
            .any(|event| matches!(event.body, codypendent_protocol::EventBody::SessionClosed)),
        Catchup::Snapshot { projection, .. } => projection.closed,
        _ => false,
    }
}

fn catchup_proves_session_exists(catchup: &Catchup) -> bool {
    match catchup {
        Catchup::Events { through, .. } | Catchup::Snapshot { through, .. } => *through > 0,
        _ => true,
    }
}

/// The knowledge-fabric projections the TUI reads (STEP 2.6 + Phase 4 client
/// wiring), all loaded in the one place the two worlds meet.
struct KnowledgeProjections {
    skills: Vec<SkillCard>,
    memories: Vec<MemoryCard>,
    docs: Vec<DocCard>,
    edges: Vec<GraphEdgeCard>,
}

/// The cap on edges surfaced in the inspector. A large repository's graph can
/// carry thousands of edges; the read-only inspector shows the first
/// [`MAX_INSPECTOR_EDGES`] (oldest first) and logs when it truncates — never a
/// silent cut.
const MAX_INSPECTOR_EDGES: usize = 500;

/// Read the knowledge fabric's registry, memories, documents, and code-graph
/// edges directly from SQLite and map them into the TUI's plain projection
/// structs (STEP 2.6 + Phase 4 client wiring). This is the CLI's job precisely
/// because the TUI crate performs no I/O and never depends on
/// `codypendent-knowledge`; the mapping from the knowledge domain types to the
/// projection structs happens here and nowhere else.
///
/// The database is opened via the same helper the `index rebuild` path uses; WAL
/// mode lets this read concurrently with the running daemon. Every failure path
/// (open, list, query) is swallowed into an empty list with a stderr note, so a
/// missing or busy database only means empty browsers — never a TUI that refuses
/// to start.
async fn load_knowledge(
    paths: &RuntimePaths,
    workspace_id: WorkspaceId,
    repo: &Path,
) -> KnowledgeProjections {
    let empty = || KnowledgeProjections {
        skills: Vec::new(),
        memories: Vec::new(),
        docs: Vec::new(),
        edges: Vec::new(),
    };

    let database_path = paths.data_dir.join("codypendent.db");
    let pool = match knowledge_db::open(&database_path).await {
        Ok(pool) => pool,
        Err(error) => {
            eprintln!(
                "codypendent: knowledge views unavailable \
                 (opening {}: {error})",
                database_path.display()
            );
            return empty();
        }
    };

    let skills = match Registry::new().list(&pool).await {
        Ok(items) => items.iter().map(skill_card).collect(),
        Err(error) => {
            eprintln!("codypendent: could not list registry items: {error}");
            Vec::new()
        }
    };

    // Visible scopes: the System tier, this session's workspace, and THIS
    // repository — where a run's harvested memories and documents live, derived
    // from the same canonical path the daemon uses. The stores enforce
    // cross-scope isolation in SQL; an empty result is fine.
    let repository = codypendent_knowledge::stable_repository_id(repo);
    let scopes = vec![
        Scope::System,
        Scope::Workspace(workspace_id),
        Scope::Repository(repository),
    ];
    let memories = match MemoryStore::new().query(&pool, &scopes, None).await {
        Ok(records) => records.iter().map(memory_card).collect(),
        Err(error) => {
            eprintln!("codypendent: could not query memories: {error}");
            Vec::new()
        }
    };

    let docs = load_docs(&pool, &scopes).await;
    let edges = load_edges(&pool, repository).await;

    pool.close().await;
    KnowledgeProjections {
        skills,
        memories,
        docs,
        edges,
    }
}

/// Project each visible-scope document (snapshot + pending suggestions) into a
/// [`DocCard`]. A per-document read failure logs and skips that document; the
/// browser degrades to what it could load rather than failing.
async fn load_docs(pool: &sqlx::SqlitePool, scopes: &[Scope]) -> Vec<DocCard> {
    let doc_store = DocumentStore::new();
    let suggestion_store = SuggestionStore::new();
    let summaries = match doc_store.list(pool, scopes).await {
        Ok(summaries) => summaries,
        Err(error) => {
            eprintln!("codypendent: could not list documents: {error}");
            return Vec::new();
        }
    };

    let mut docs = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let document = match doc_store.snapshot_document(pool, summary.id).await {
            Ok(Some(document)) => document,
            Ok(None) => continue,
            Err(error) => {
                eprintln!(
                    "codypendent: could not load document {}: {error}",
                    summary.id
                );
                continue;
            }
        };
        let suggestions = suggestion_store
            .pending(pool, summary.id)
            .await
            .unwrap_or_else(|error| {
                eprintln!(
                    "codypendent: could not load suggestions for {}: {error}",
                    summary.id
                );
                Vec::new()
            });
        docs.push(doc_card(&document, &suggestions));
    }
    docs
}

/// Project this repository's code-graph edges into [`GraphEdgeCard`]s, resolving
/// each endpoint node id to its qualified name. Bounded by
/// [`MAX_INSPECTOR_EDGES`] with a note when it truncates.
async fn load_edges(pool: &sqlx::SqlitePool, repository: RepositoryId) -> Vec<GraphEdgeCard> {
    use codypendent_knowledge::codegraph;

    let names: HashMap<CodeNodeId, String> = match codegraph::nodes(pool, repository).await {
        Ok(nodes) => nodes
            .into_iter()
            .map(|node| (node.id, node.key.qualified_name))
            .collect(),
        Err(error) => {
            eprintln!("codypendent: could not load code-graph nodes: {error}");
            HashMap::new()
        }
    };

    let mut edges = match codegraph::edges(pool, repository).await {
        Ok(edges) => edges,
        Err(error) => {
            eprintln!("codypendent: could not load code-graph edges: {error}");
            return Vec::new();
        }
    };
    if edges.len() > MAX_INSPECTOR_EDGES {
        eprintln!(
            "codypendent: code graph has {} edges; the inspector shows the first {MAX_INSPECTOR_EDGES}",
            edges.len()
        );
        edges.truncate(MAX_INSPECTOR_EDGES);
    }
    edges.iter().map(|edge| edge_card(edge, &names)).collect()
}

/// Map a governed [`RegistryItem`] into the TUI's [`SkillCard`] projection,
/// rendering each requested capability **verbatim** (STEP 2.6 "skill permissions
/// are visible").
fn skill_card(item: &RegistryItem) -> SkillCard {
    SkillCard {
        name: item.name.clone(),
        kind: registry_kind_label(item.kind).to_owned(),
        scope: scope_label(&item.scope),
        trust: trust_label(item.trust.tier).to_owned(),
        status: status_label(item.status).to_owned(),
        risk: risk_label(item.risk).to_owned(),
        description: item.description.clone(),
        permissions: item.permissions.iter().map(capability_verbatim).collect(),
    }
}

/// Map a [`MemoryRecord`] into the TUI's [`MemoryCard`] projection. `source` is a
/// human rendering of the record's evidence refs (joined when there are several),
/// which the memory browser's "open source" affordance surfaces in full.
fn memory_card(record: &MemoryRecord) -> MemoryCard {
    let source = if record.provenance.is_empty() {
        "(no evidence)".to_owned()
    } else {
        record
            .provenance
            .iter()
            .map(evidence_source)
            .collect::<Vec<_>>()
            .join("; ")
    };
    MemoryCard {
        statement: record.statement.clone(),
        class: memory_class_label(record.class).to_owned(),
        scope: scope_label(&record.scope),
        revision: record.valid_from.0.clone(),
        observed: record.observed_at.date_naive().to_string(),
        confidence: record.confidence,
        source,
    }
}

/// Render one requested capability exactly as declared, e.g.
/// `"filesystem_read: $REPOSITORY"` or `"command: cargo"` — the verbatim form the
/// Skill Studio shows.
fn capability_verbatim(capability: &CapabilityRequest) -> String {
    match capability {
        CapabilityRequest::FilesystemRead(value) => format!("filesystem_read: {value}"),
        CapabilityRequest::FilesystemWrite(value) => format!("filesystem_write: {value}"),
        CapabilityRequest::Command(value) => format!("command: {value}"),
        CapabilityRequest::Network(value) => format!("network: {value}"),
        CapabilityRequest::Secret(value) => format!("secret: {value}"),
    }
}

/// A human rendering of a memory's evidence ref (what "open source" reveals).
fn evidence_source(evidence: &EvidenceRef) -> String {
    match evidence {
        EvidenceRef::EventRange {
            session_id,
            from_sequence,
            to_sequence,
        } => format!("events {from_sequence}..{to_sequence} of session {session_id}"),
        EvidenceRef::Artifact {
            artifact,
            source_path,
        } => match source_path {
            Some(path) => format!("artifact {} ({path})", artifact.id),
            None => format!("artifact {}", artifact.id),
        },
    }
}

/// A compact human label for a memory/registry [`Scope`]: the tier, plus a short
/// prefix of its key for the id-bearing tiers (the full UUID is noise in a card).
fn scope_label(scope: &Scope) -> String {
    match scope.key() {
        Some(key) => format!(
            "{} {}",
            scope.tier(),
            key.chars().take(8).collect::<String>()
        ),
        None => scope.tier().to_owned(),
    }
}

fn registry_kind_label(kind: RegistryItemKind) -> &'static str {
    match kind {
        RegistryItemKind::Tool => "tool",
        RegistryItemKind::Skill => "skill",
        RegistryItemKind::Plugin => "plugin",
        RegistryItemKind::Hook => "hook",
        RegistryItemKind::Command => "command",
    }
}

fn trust_label(tier: TrustTier) -> &'static str {
    match tier {
        TrustTier::Untrusted => "untrusted",
        TrustTier::Community => "community",
        TrustTier::Verified => "verified",
        TrustTier::FirstParty => "first-party",
    }
}

fn status_label(status: RegistryStatus) -> &'static str {
    match status {
        RegistryStatus::Draft => "draft",
        RegistryStatus::Active => "active",
        RegistryStatus::Modified => "modified",
        RegistryStatus::Deprecated => "deprecated",
    }
}

fn risk_label(risk: RiskClass) -> &'static str {
    match risk {
        RiskClass::Safe => "safe",
        RiskClass::Low => "low",
        RiskClass::Medium => "medium",
        RiskClass::High => "high",
    }
}

fn memory_class_label(class: MemoryClass) -> &'static str {
    match class {
        MemoryClass::Working => "working",
        MemoryClass::Episodic => "episodic",
        MemoryClass::Semantic => "semantic",
        MemoryClass::Procedural => "procedural",
        MemoryClass::Preference => "preference",
        MemoryClass::Failure => "failure",
        MemoryClass::Artifact => "artifact",
        MemoryClass::Code => "code",
    }
}

/// Map a [`KnowledgeDocument`] (plus its pending suggestions) into the TUI's
/// [`DocCard`] projection. `mode` is the collaboration mode the document's scope
/// defaults to — org-scope docs read `suggest`, the suggest-by-default the
/// engine enforces (STEP 4.3).
fn doc_card(document: &KnowledgeDocument, suggestions: &[Suggestion]) -> DocCard {
    DocCard {
        title: document.title.clone(),
        scope: scope_label(&document.scope),
        status: document.status.as_str().to_owned(),
        mode: collab_mode_label(CollaborationMode::default_for_scope(&document.scope)).to_owned(),
        revision: format!("r{}", document.revision),
        blocks: document.blocks.iter().map(block_view).collect(),
        suggestions: suggestions.iter().map(suggestion_view).collect(),
    }
}

/// Render one [`DocumentBlock`] into the editor rail's [`DocBlockView`]: a kind
/// label and a single-line human rendering of its content (never the raw
/// serialized block). Structured/embed blocks get a compact stand-in.
fn block_view(block: &DocumentBlock) -> DocBlockView {
    let (kind, text) = match &block.content {
        BlockContent::Heading { level, text } => (format!("heading h{level}"), text.clone()),
        BlockContent::Paragraph { text } => ("paragraph".to_owned(), text.clone()),
        BlockContent::Code { language, text } => (
            match language {
                Some(language) => format!("code {language}"),
                None => "code".to_owned(),
            },
            text.clone(),
        ),
        BlockContent::Diagram { format, .. } => {
            (format!("diagram {format}"), "(diagram)".to_owned())
        }
        BlockContent::Table { rows } => ("table".to_owned(), format!("({} rows)", rows.len())),
        BlockContent::Callout { kind, text } => (format!("callout {kind}"), text.clone()),
        BlockContent::Checklist { items } => {
            ("checklist".to_owned(), format!("({} items)", items.len()))
        }
        BlockContent::Query { query } => ("query".to_owned(), query.clone()),
        BlockContent::EmbeddedFile { path } => ("embed-file".to_owned(), path.clone()),
        BlockContent::EmbeddedSymbol { symbol } => ("embed-symbol".to_owned(), symbol.clone()),
        BlockContent::EmbeddedWorkflow { workflow } => {
            ("embed-workflow".to_owned(), workflow.clone())
        }
        BlockContent::EmbeddedSkill { skill } => ("embed-skill".to_owned(), skill.clone()),
    };
    // Collapse to one line — the editor rail renders a block per row.
    DocBlockView {
        kind,
        text: text.replace('\n', " "),
    }
}

/// Map a [`Suggestion`] into the review rail's [`DocSuggestionView`].
fn suggestion_view(suggestion: &Suggestion) -> DocSuggestionView {
    DocSuggestionView {
        status: suggestion_status_label(suggestion.status).to_owned(),
        author: document_author_label(&suggestion.author),
        range: format!("{}..{}", suggestion.range_start, suggestion.range_end),
        replacement: suggestion.replacement.clone(),
        rationale: suggestion.rationale.clone(),
    }
}

/// Map a [`CodeEdge`] into the inspector's [`GraphEdgeCard`], resolving each
/// endpoint node id to its qualified name via `names` (falling back to the id
/// when a node is not in the map). Carries the evidence + revision the Phase 4
/// exit criterion requires the inspector to expose.
fn edge_card(edge: &CodeEdge, names: &HashMap<CodeNodeId, String>) -> GraphEdgeCard {
    let name = |id: &CodeNodeId| {
        names
            .get(id)
            .cloned()
            .unwrap_or_else(|| format!("node {id}"))
    };
    GraphEdgeCard {
        from: name(&edge.from),
        to: name(&edge.to),
        relation: code_relation_label(edge.relation).to_owned(),
        confidence: edge.confidence,
        evidence_kind: evidence_kind_label(edge.evidence_kind).to_owned(),
        evidence: edge
            .evidence
            .as_ref()
            .map_or_else(|| "(none)".to_owned(), evidence_source),
        revision: edge.revision.0.clone(),
    }
}

fn collab_mode_label(mode: CollaborationMode) -> &'static str {
    match mode {
        CollaborationMode::Ask => "ask",
        CollaborationMode::Suggest => "suggest",
        CollaborationMode::Edit => "edit",
        CollaborationMode::CoAuthor => "co-author",
        CollaborationMode::Review => "review",
        CollaborationMode::Maintain => "maintain",
    }
}

fn suggestion_status_label(status: SuggestionStatus) -> &'static str {
    match status {
        SuggestionStatus::Pending => "pending",
        SuggestionStatus::Accepted => "accepted",
        SuggestionStatus::Rejected => "rejected",
    }
}

/// A compact label for who authored a document mutation — an agent sentence
/// names its serving model (the traceability triple's public face).
fn document_author_label(author: &DocumentAuthor) -> String {
    match author {
        DocumentAuthor::Human { .. } => "human".to_owned(),
        DocumentAuthor::Agent { model, .. } => format!("agent ({model})"),
        DocumentAuthor::Integration { integration } => format!("integration ({integration})"),
    }
}

fn code_relation_label(relation: CodeRelation) -> &'static str {
    match relation {
        CodeRelation::Contains => "contains",
        CodeRelation::Defines => "defines",
        CodeRelation::Imports => "imports",
        CodeRelation::References => "references",
        CodeRelation::Calls => "calls",
        CodeRelation::Implements => "implements",
        CodeRelation::Extends => "extends",
        CodeRelation::Reads => "reads",
        CodeRelation::Writes => "writes",
        CodeRelation::Mutates => "mutates",
        CodeRelation::Returns => "returns",
        CodeRelation::Accepts => "accepts",
        CodeRelation::Tests => "tests",
        CodeRelation::Configures => "configures",
        CodeRelation::Serializes => "serializes",
        CodeRelation::DependsOn => "depends-on",
        CodeRelation::GeneratedFrom => "generated-from",
    }
}

fn evidence_kind_label(kind: EvidenceKind) -> &'static str {
    match kind {
        EvidenceKind::SyntaxInferred => "syntax_inferred",
        EvidenceKind::LspResolved => "lsp_resolved",
        EvidenceKind::CompilerResolved => "compiler_resolved",
        EvidenceKind::RuntimeObserved => "runtime_observed",
    }
}

/// The persisted repo → session mapping, so reopening the TUI in a repository
/// resumes its session instead of starting over. Stored as JSON in the data dir;
/// a corrupt or absent file reads as empty (the store is a convenience, never a
/// source of truth — the daemon's ledger is).
#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionStore {
    /// Canonical repository path → the session last opened there.
    sessions: HashMap<String, StoredSession>,
    /// The opaque daemon-issued resume token from the last handshake, presented
    /// on the next launch so this client keeps one identity across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resume_token: Option<String>,
}

/// One remembered session: its id and the workspace it belongs to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct StoredSession {
    session_id: SessionId,
    workspace_id: WorkspaceId,
}

impl SessionStore {
    fn file(paths: &RuntimePaths) -> PathBuf {
        paths.data_dir.join("tui-sessions.json")
    }

    fn load(paths: &RuntimePaths) -> Self {
        std::fs::read(Self::file(paths))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, paths: &RuntimePaths) {
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = paths.ensure_directories();
            let _ = std::fs::write(Self::file(paths), bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::{AgentMode, ApprovalDecision, ApprovalId, ApprovalScope, RunId};

    #[test]
    fn intents_map_to_the_matching_command_bodies() {
        let session_id = SessionId::new();
        let run_id = RunId::new();
        let repository = "/repo/one";

        assert_eq!(
            intent_to_command(
                Intent::StartRun {
                    objective: "diagnose".into(),
                    mode: AgentMode::Build,
                },
                session_id,
                repository,
            ),
            CommandBody::StartRun {
                session_id,
                objective: "diagnose".into(),
                mode: AgentMode::Build,
                repository: Some(repository.to_owned()),
            }
        );

        let approval_id = ApprovalId::new();
        assert_eq!(
            intent_to_command(
                Intent::ResolveApproval {
                    approval_id,
                    decision: ApprovalDecision::Approve,
                    scope: ApprovalScope::Once,
                },
                session_id,
                repository,
            ),
            CommandBody::ResolveApproval {
                approval_id,
                decision: ApprovalDecision::Approve,
                scope: ApprovalScope::Once,
            }
        );

        assert_eq!(
            intent_to_command(Intent::PauseRun { run_id }, session_id, repository),
            CommandBody::PauseRun { run_id }
        );
        assert_eq!(
            intent_to_command(Intent::ResumeRun { run_id }, session_id, repository),
            CommandBody::ResumeRun { run_id }
        );
        assert_eq!(
            intent_to_command(Intent::CancelRun { run_id }, session_id, repository),
            CommandBody::CancelRun { run_id }
        );
        assert_eq!(
            intent_to_command(
                Intent::QueueSteering {
                    run_id,
                    text: "focus on the failing test".into(),
                },
                session_id,
                repository,
            ),
            CommandBody::QueueSteering {
                run_id,
                text: "focus on the failing test".into(),
            }
        );
    }

    #[test]
    fn command_envelope_is_self_idempotent() {
        let client_id = ClientId::new();
        let envelope = command_envelope(
            client_id,
            CommandBody::PauseRun {
                run_id: RunId::new(),
            },
        );
        match envelope.payload {
            Payload::Command(command) => {
                // A client-side retry reuses the command id, so the idempotency
                // key must be that same id — the duplicate-delivery contract.
                assert_eq!(command.idempotency_key, command.command_id.to_string());
                assert!(command.expected_revision.is_none());
            }
            other => panic!("expected a Command payload, got {other:?}"),
        }
    }

    #[test]
    fn session_store_round_trips_through_the_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::from_data_dir(tmp.path().to_path_buf());
        paths.ensure_directories().unwrap();

        let mut store = SessionStore::default();
        let stored = StoredSession {
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
        };
        store.sessions.insert("/repo/one".into(), stored);
        store.save(&paths);

        let loaded = SessionStore::load(&paths);
        let got = loaded.sessions.get("/repo/one").expect("entry persisted");
        assert_eq!(got.session_id, stored.session_id);
        assert_eq!(got.workspace_id, stored.workspace_id);
    }

    #[test]
    fn a_missing_store_reads_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::from_data_dir(tmp.path().to_path_buf());
        assert!(SessionStore::load(&paths).sessions.is_empty());
    }

    #[test]
    fn a_zero_event_catchup_is_treated_as_a_missing_session() {
        // The helper keys off the watermark, not the event vec. An absent session
        // watermarks at 0 and must not be resumed (issue #6 item 6); a live one
        // replays at least its SessionCreated event, so its watermark is >= 1.
        assert!(!catchup_proves_session_exists(&Catchup::Events {
            from: 1,
            through: 0,
            events: vec![],
        }));
        assert!(catchup_proves_session_exists(&Catchup::Events {
            from: 1,
            through: 3,
            events: vec![],
        }));
        // A forward-compat variant we can't inspect is accepted rather than
        // discarding a possibly-resumable session against a newer daemon.
        assert!(catchup_proves_session_exists(&Catchup::Unknown));
    }
}
