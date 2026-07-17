//! Daemon lifecycle commands (Phase 0), the headless JSONL client (STEP 1.13:
//! `run` and `attach`), and the Phase-2 `index rebuild` maintenance command.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use codypendent_knowledge::{
    db as knowledge_db, register_builtins, retrieve, HashingEmbedder, Registry, RetrievalConfig,
    RetrievalIndexes, RetrievalQuery, RiskClass, Scope,
};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    AgentMode, ClientRole, CommandBody, Payload, SessionId, Subscription, WorkspaceId,
};

use crate::client;
use crate::connection::Connection;
use crate::stream::{self, RunExit};

/// Outcome of making sure a daemon is listening: either one already was, or
/// this call spawned and waited for one. Shared by the human-facing
/// `codypendent daemon start` and the silent variant `run --jsonl` uses (its
/// stdout must carry nothing but JSONL envelopes).
pub(crate) enum EnsureOutcome {
    AlreadyRunning,
    Started { pid: u32 },
}

/// Spawn `codypendentd` detached if nothing answers Ping yet, then wait for
/// the socket to come up (5 second budget). No I/O beyond the daemon's own
/// log file — callers decide how (or whether) to report the outcome.
pub(crate) async fn ensure_daemon(paths: &RuntimePaths) -> anyhow::Result<EnsureOutcome> {
    if client::ping(&paths.socket_path).await {
        return Ok(EnsureOutcome::AlreadyRunning);
    }
    paths.ensure_directories()?;

    let daemon_binary = resolve_daemon_binary();
    let log_path = paths.log_dir.join("daemon.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_for_stderr = log.try_clone()?;

    let mut command = std::process::Command::new(&daemon_binary);
    command
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_for_stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New process group: the daemon must not die with this CLI's terminal.
        command.process_group(0);
    }
    let child = command
        .spawn()
        .with_context(|| format!("failed to spawn {}", daemon_binary.display()))?;

    for _ in 0..50 {
        if client::ping(&paths.socket_path).await {
            return Ok(EnsureOutcome::Started { pid: child.id() });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "daemon did not become ready within 5 seconds; check {}",
        log_path.display()
    )
}

/// `codypendent daemon start`: spawn `codypendentd` detached, then wait for
/// the socket to answer Ping (5 second budget).
pub async fn start(paths: &RuntimePaths) -> anyhow::Result<()> {
    match ensure_daemon(paths).await? {
        EnsureOutcome::AlreadyRunning => println!("daemon already running"),
        EnsureOutcome::Started { pid } => println!("daemon started (pid {pid})"),
    }
    Ok(())
}

/// `codypendent daemon stop`: request graceful shutdown, then wait for the
/// socket to stop answering (5 second budget).
pub async fn stop(paths: &RuntimePaths) -> anyhow::Result<()> {
    if !client::ping(&paths.socket_path).await {
        println!("daemon is not running");
        return Ok(());
    }
    client::shutdown(&paths.socket_path).await?;
    for _ in 0..50 {
        if !client::ping(&paths.socket_path).await {
            println!("daemon stopped");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("daemon acknowledged shutdown but is still answering after 5 seconds")
}

/// `codypendent daemon status [--json]`.
///
/// Prints the status (human text or JSON) and RETURNS whether the daemon is
/// running (`true`) or not (`false`). The library never calls
/// `std::process::exit`; the `status` subcommand's exit-1-when-not-running
/// decision lives in `main.rs`.
pub async fn status(paths: &RuntimePaths, json: bool) -> anyhow::Result<bool> {
    match client::daemon_status(&paths.socket_path).await {
        Ok(status) => {
            if json {
                let value = serde_json::json!({ "running": true, "status": status });
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("Codypendent daemon");
                println!("  running      yes");
                println!("  version      {}", status.daemon_version);
                println!("  protocol     {}", status.protocol_version);
                println!("  pid          {}", status.pid);
                println!("  instance     {}", status.instance_id);
                println!("  boot count   {}", status.boot_count);
                println!("  started at   {}", status.started_at.to_rfc3339());
                println!("  uptime       {}s", status.uptime_seconds);
                println!("  database     {}", status.database_path);
                println!("  socket       {}", status.socket_path);
                println!("  sessions     {}", status.session_count);
            }
            Ok(true)
        }
        Err(_) => {
            if json {
                println!("{}", serde_json::json!({ "running": false }));
            } else {
                println!("daemon is not running");
            }
            Ok(false)
        }
    }
}

/// Prefer a `codypendentd` sitting next to this executable (the layout that
/// `cargo build` and installers both produce); fall back to PATH lookup.
fn resolve_daemon_binary() -> PathBuf {
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let candidate = dir.join("codypendentd");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from("codypendentd")
}

// --- STEP 1.13: headless JSONL client ---------------------------------------

/// `codypendent run --objective "..." [--mode build] [--repo PATH] --jsonl`.
///
/// Ensures a daemon is running, creates a session, starts one run in it, and
/// streams every session event to stdout as JSONL until the run reaches a
/// terminal state. Returns the STEP 1.13 exit code (`0` completed, `2`
/// failed, `130` cancelled); `main` is the only place that calls
/// `std::process::exit`.
///
/// `repo` is validated (must exist) and its canonical path is carried on
/// `StartRun`, so the daemon attributes the run's repository map and curated
/// memories to *this* checkout rather than to its own working directory — the
/// per-user daemon can serve several checkouts over one socket (issue #6
/// item 1). `CreateSession` still carries only an opaque `WorkspaceId`; binding
/// a dedicated worktree to a run is a later step (STEP 1.8).
pub async fn run(
    paths: &RuntimePaths,
    objective: String,
    mode: AgentMode,
    repo: PathBuf,
    jsonl: bool,
) -> anyhow::Result<i32> {
    if !jsonl {
        anyhow::bail!(
            "codypendent run currently requires --jsonl; interactive TUI attach \
             lands in a later build step"
        );
    }
    let repo = repo.canonicalize().with_context(|| {
        format!(
            "--repo {}: not a valid, accessible directory",
            repo.display()
        )
    })?;
    if !repo.is_dir() {
        anyhow::bail!("--repo {}: not a directory", repo.display());
    }

    // The daemon-start banner ("daemon already running" / "daemon started
    // (pid N)") is Phase 0 human output; --jsonl's contract is that stdout
    // carries nothing but JSONL envelopes, so this step is silent on success
    // and only ever writes to stderr on failure (via the `?` below).
    ensure_daemon(paths).await?;

    let mut conn = Connection::connect(&paths.socket_path).await?;
    let mut stdout = std::io::stdout();
    let repository = repo.to_string_lossy().into_owned();
    let exit = run_over_connection(&mut conn, objective, mode, &repository, &mut stdout).await?;
    Ok(exit.exit_code())
}

/// The connected core of [`run`]: handshake, create + attach + start, then
/// stream to `out` until terminal. Split out so tests can drive it against a
/// hand-rolled mock server over a `Connection` that already points at a test
/// socket, asserting the returned [`RunExit`] directly instead of a process
/// exit code.
pub async fn run_over_connection<W: Write>(
    conn: &mut Connection,
    objective: String,
    mode: AgentMode,
    repository: &str,
    out: &mut W,
) -> anyhow::Result<RunExit> {
    conn.handshake("codypendent", env!("CARGO_PKG_VERSION"))
        .await?;

    // CreateSession: the daemon's `CommandAccepted` reply carries no
    // session/run id in its *payload* — `crates/daemon/src/server.rs` builds
    // it from only `command_id` and `sequence`, dropping
    // `CommandOutcome::created_session`/`created_run`. (Confirmed by the
    // daemon's own integration test, `crates/daemon/tests/server_it.rs`'s
    // `only_session_id`, which resorts to querying the session table
    // directly — an option this crate does not have, since a client only
    // ever speaks the wire protocol.) The one wire-level place a freshly
    // created session's id *can* travel is the reply envelope's own
    // `session_id` field (`Envelope.session_id`, Chapter 03) — general
    // envelope metadata alongside any payload — so that is the contract this
    // client relies on. A daemon that (like the currently committed STEP
    // 1.11 server) never populates that field on a `CreateSession` reply
    // cannot support `run` end-to-end; closing that gap is a
    // `codypendent-daemon` change, out of this crate's scope. We fail
    // loudly and specifically here rather than hang waiting for an id that
    // will never arrive.
    let workspace = WorkspaceId::new();
    let create_reply = conn
        .send_command(CommandBody::CreateSession {
            workspace,
            title: objective.clone(),
        })
        .await?;
    let session_id = match &create_reply.payload {
        Payload::CommandAccepted { .. } => create_reply.session_id.ok_or_else(|| {
            anyhow::anyhow!(
                "daemon accepted CreateSession but its reply carried no session_id \
                 (neither in the payload nor Envelope.session_id); codypendent run \
                 cannot learn the newly created session's id"
            )
        })?,
        Payload::CommandRejected(error) => {
            anyhow::bail!("CreateSession rejected: {} ({})", error.message, error.code)
        }
        other => anyhow::bail!("unexpected reply to CreateSession: {other:?}"),
    };

    let attach_reply = conn
        .send_command(CommandBody::AttachSession {
            session_id,
            last_seen_sequence: None,
            subscriptions: vec![Subscription::SessionSummary, Subscription::AgentActivity],
            requested_role: ClientRole::Controller,
        })
        .await?;
    let catchup = expect_catchup(attach_reply)?;
    stream::replay_catchup(out, conn.client_id(), session_id, catchup)?;

    let start_reply = conn
        .send_command(CommandBody::StartRun {
            session_id,
            objective,
            mode,
            // Attribute the run to the `--repo` the client is operating on, so a
            // shared daemon does not store its memories under its own directory
            // (issue #6 item 1).
            repository: Some(repository.to_owned()),
        })
        .await?;
    if let Payload::CommandRejected(error) = &start_reply.payload {
        anyhow::bail!("StartRun rejected: {} ({})", error.message, error.code);
    }

    stream::stream_until_terminal(conn, out).await
}

/// `codypendent attach <SESSION_ID> [--from-sequence N] --events jsonl`.
///
/// Attaches as an `Observer` and streams the catch-up plus every subsequent
/// session event as JSONL until the connection ends or the user interrupts
/// with Ctrl-C — never stopping (let alone affecting) the run itself.
pub async fn attach(
    paths: &RuntimePaths,
    session_id: SessionId,
    from_sequence: Option<u64>,
) -> anyhow::Result<()> {
    let mut conn = Connection::connect(&paths.socket_path).await?;
    let mut stdout = std::io::stdout();
    tokio::select! {
        result = attach_over_connection(&mut conn, session_id, from_sequence, &mut stdout) => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

/// The connected core of [`attach`], split out for the same testability
/// reason as [`run_over_connection`].
pub async fn attach_over_connection<W: Write>(
    conn: &mut Connection,
    session_id: SessionId,
    from_sequence: Option<u64>,
    out: &mut W,
) -> anyhow::Result<()> {
    conn.handshake("codypendent", env!("CARGO_PKG_VERSION"))
        .await?;

    let attach_reply = conn
        .send_command(CommandBody::AttachSession {
            session_id,
            last_seen_sequence: from_sequence,
            subscriptions: vec![Subscription::SessionSummary, Subscription::AgentActivity],
            requested_role: ClientRole::Observer,
        })
        .await?;
    let catchup = expect_catchup(attach_reply)?;
    stream::replay_catchup(out, conn.client_id(), session_id, catchup)?;

    stream::stream_forever(conn, out).await
}

/// Common `AttachSession` reply handling shared by `run` and `attach`.
pub(crate) fn expect_catchup(
    reply: codypendent_protocol::Envelope,
) -> anyhow::Result<codypendent_protocol::Catchup> {
    match reply.payload {
        Payload::Catchup { catchup } => Ok(catchup),
        Payload::CommandRejected(error) => {
            anyhow::bail!("AttachSession rejected: {} ({})", error.message, error.code)
        }
        other => anyhow::bail!("unexpected reply to AttachSession: {other:?}"),
    }
}

/// `codypendent index rebuild`: delete the derived indexes and rebuild them from
/// the authoritative rows (STEP 2.1 rule 2 / the Phase-2 "stale indexes rebuild
/// from authority" exit criterion).
///
/// The derived indexes are a *pure function* of the authoritative
/// registry/memory/code rows, so they can be discarded at any time and replaying
/// authority restores identical results. In Phase 2 the retrieval indexes
/// (Tantivy BM25 + the vector index) are held in memory and rebuilt from the
/// registry on demand — persisting them under `<data_dir>/index/` is a later
/// step. This command is self-contained (it does not require the daemon): it
/// opens the database directly, ensures the built-in tools are registered,
/// removes `<data_dir>/index/` if present (forward-compatible with persisted
/// indexes, a no-op today), rebuilds the retrieval indexes from the registry,
/// and runs a canary query to prove the fresh index serves retrieval.
pub async fn index_rebuild(paths: &RuntimePaths) -> anyhow::Result<()> {
    paths.ensure_directories()?;
    let database_path = paths.data_dir.join("codypendent.db");
    let pool = knowledge_db::open(&database_path)
        .await
        .with_context(|| format!("opening {}", database_path.display()))?;

    // Idempotent baseline: a rebuild on a never-started daemon still has the
    // built-in tools to index.
    register_builtins(&pool).await?;

    // Derived indexes are deletable at any time.
    let index_dir = paths.data_dir.join("index");
    if index_dir.exists() {
        std::fs::remove_dir_all(&index_dir)
            .with_context(|| format!("removing derived index dir {}", index_dir.display()))?;
    }

    // Replay authority into fresh indexes.
    let items = Registry::new().list(&pool).await?;
    let indexes = RetrievalIndexes::build(&items, HashingEmbedder::new())?;

    // Canary: the freshly rebuilt index still serves retrieval (System-scoped
    // built-ins are visible; a Medium ceiling admits every first-party tool).
    let query = RetrievalQuery::new("run the tests", vec![Scope::System], RiskClass::Medium);
    let result = retrieve(&items, &indexes, &query, &RetrievalConfig::default())?;

    println!(
        "index rebuild complete: {} registry item(s) re-indexed from authority; \
         canary \"run the tests\" -> {} tool card(s), {} skill card(s)",
        items.len(),
        result.tools.len(),
        result.skills.len(),
    );
    Ok(())
}
