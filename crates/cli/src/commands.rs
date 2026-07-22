//! Daemon lifecycle commands (Phase 0), the headless JSONL client (STEP 1.13:
//! `run` and `attach`), the Phase-2 `index rebuild` maintenance command, and
//! `docs publish` (Phase 4 STEP 4.4).

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use codypendent_knowledge::{
    db as knowledge_db, plan_publication, publications, register_builtins, retrieve, DocumentStore,
    HashingEmbedder, Publication, PublishTarget as KnowledgePublishTarget, Registry,
    RetrievalConfig, RetrievalIndexes, RetrievalQuery, RiskClass, Scope,
};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::{
    AgentMode, ApprovalDecision, ApprovalScope, ClientRole, CommandBody, DocumentId, Payload,
    PromotionAction, SessionId, Subscription, WorkspaceId,
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
    conn.handshake("codypendent", env!("CARGO_PKG_VERSION"), None)
        .await?;

    // CreateSession: the daemon's `CommandAccepted` *payload* is intentionally
    // minimal (only `command_id` + `sequence`). The freshly created session's id
    // travels on the reply envelope's own `session_id` field
    // (`Envelope.session_id`, Chapter 03) — connection-level metadata the server
    // sets on a `CreateSession` reply from `CommandOutcome::created_session`
    // (`crates/daemon/src/server.rs`). This client reads it from there; if a
    // daemon ever omits it we fail loudly and specifically below rather than
    // hang waiting for an id that will never arrive.
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
    // Bind to exactly the run OUR StartRun created (the daemon reports it on
    // the accept). Falling back to first-observed `RunStarted` is only for an
    // older daemon that doesn't send it — under which a concurrent client's
    // run starting first could otherwise capture the exit code.
    let created_run = match &start_reply.payload {
        Payload::CommandAccepted { created_run, .. } => *created_run,
        _ => None,
    };

    stream::stream_until_terminal(conn, out, created_run).await
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
    conn.handshake("codypendent", env!("CARGO_PKG_VERSION"), None)
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

/// `codypendent open <session> --in <ide>` (STEP 3.7). Print how the IDE should
/// attach to the session, then best-effort launch the editor with the session in
/// its environment. The IDE joins as a *contributor* to the SAME session — the
/// run is never restarted; the daemon publishes a `ClientPresenceChanged` so the
/// TUI shows the editor arriving. A missing editor binary is not an error: the
/// printed instructions still let a user attach manually.
pub async fn open(
    paths: &RuntimePaths,
    session_id: SessionId,
    ide_binary: &str,
    ide_name: &str,
    repo: PathBuf,
) -> anyhow::Result<()> {
    println!("{}", handoff_message(session_id, paths, ide_name));

    // Best-effort launch. The extension reads `CODYPENDENT_SESSION` to attach to
    // this exact session (rather than opening a fresh one).
    let launched = std::process::Command::new(ide_binary)
        .arg(&repo)
        .env("CODYPENDENT_SESSION", session_id.to_string())
        .env("CODYPENDENT_SOCKET", &paths.socket_path)
        .spawn();
    match launched {
        Ok(_) => println!("Launched {ide_name}."),
        Err(_) => println!(
            "Could not launch `{ide_binary}` (is it on PATH?). \
             Open {ide_name} yourself and attach to the session above."
        ),
    }
    Ok(())
}

/// `codypendent docs publish --target <T>` (Phase 4 STEP 4.4). Which Git
/// target to publish to, decoupled from `clap`'s `ValueEnum` derive (mirrors
/// `codypendent_knowledge::PublishTarget`'s three variants with CLI-friendly
/// names: `repo-file` / `docs-branch` / `doc-pr`).
pub enum PublishTargetKind {
    RepoFile,
    DocsBranch,
    DocPr,
}

/// `codypendent docs publish <DOCUMENT> --target <T>` (Phase 4 STEP 4.4,
/// closing the deferred "executing a `PublishPlan`" roadmap item).
///
/// Opens the daemon's database directly (the CLI projection seam — the same
/// read-only pattern the TUI's Docs view and `index rebuild` use) to load the
/// document and compute the exact deterministic plan the daemon itself will
/// compute, so the target/changed-files/Git-action preview printed here is
/// never a guess (STEP 4.4.2). After confirming (or `--yes`), ensures a
/// daemon, sends `PublishDocument` — which durably parks an approval and
/// replies with the parked plan the daemon computed independently — then
/// immediately resolves that approval with the confirmed decision over the
/// SAME connection (this CLI invocation is the human approver). A rejection
/// performs no write. On approval the daemon executes in the background, so
/// this polls the publication history briefly for the resulting commit before
/// reporting the outcome.
pub async fn docs_publish(
    paths: &RuntimePaths,
    document_id: DocumentId,
    target: PublishTargetKind,
    path: Option<String>,
    branch: Option<String>,
    title: Option<String>,
    yes: bool,
) -> anyhow::Result<()> {
    paths.ensure_directories()?;
    let database_path = paths.data_dir.join("codypendent.db");
    let pool = knowledge_db::open(&database_path)
        .await
        .with_context(|| format!("opening {}", database_path.display()))?;

    let doc = DocumentStore::new()
        .snapshot_document(&pool, document_id)
        .await
        .with_context(|| format!("loading document {document_id}"))?
        .ok_or_else(|| anyhow::anyhow!("no document {document_id}"))?;

    let resolved_target = resolve_publish_target(target, &doc.title, path, branch, title);
    let plan = plan_publication(&doc, resolved_target.clone());

    println!("Publish plan for \"{}\" ({document_id}):", doc.title);
    println!("  target: {}", describe_publish_target(&resolved_target));
    println!("  changed files:");
    for file in &plan.changed_files {
        println!("    {file}");
    }
    println!("  git action: {}", plan.git_action);

    let approved = if yes {
        true
    } else {
        print!("Publish? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    };

    let decision = if approved {
        ApprovalDecision::Approve
    } else {
        ApprovalDecision::Reject
    };

    ensure_daemon(paths).await?;
    let mut conn = Connection::connect(&paths.socket_path)
        .await
        .with_context(|| "connecting to the daemon (is it running?)")?;
    let approval_id = docs_publish_over_connection(
        &mut conn,
        document_id,
        to_wire_target(&resolved_target),
        decision,
    )
    .await?;
    println!("Parked approval {approval_id}.");

    if !approved {
        println!("Publish rejected; nothing was written.");
        return Ok(());
    }

    let existing = publications(&pool, document_id).await?.len();
    match wait_for_new_publication(&pool, document_id, existing).await {
        Some(publication) => println!(
            "Published \"{}\" ({document_id}) -> commit {}",
            doc.title,
            publication.git_commit.as_deref().unwrap_or("(none)")
        ),
        None => println!(
            "Publish approved; the daemon is still executing it in the background. \
             Check the daemon log, or re-run `codypendent docs publish` shortly to see \
             the recorded commit."
        ),
    }
    Ok(())
}

/// The connected core of [`docs_publish`]: handshake, bind the `Controller`
/// role, send `PublishDocument`, then immediately resolve the parked approval
/// with `decision` over the SAME connection (this CLI invocation is the human
/// approver — the confirmation already happened before this is called).
/// Returns the parked [`ApprovalId`](codypendent_protocol::ApprovalId). Split
/// out so a test can drive it against a mock daemon, mirroring
/// [`workflow_run_over_connection`].
pub async fn docs_publish_over_connection(
    conn: &mut Connection,
    document_id: DocumentId,
    target: codypendent_protocol::document::PublishTarget,
    decision: ApprovalDecision,
) -> anyhow::Result<codypendent_protocol::ApprovalId> {
    conn.handshake("codypendent", env!("CARGO_PKG_VERSION"), None)
        .await?;
    bind_control_role(conn).await?;

    let reply = conn
        .send_command(CommandBody::PublishDocument {
            document_id,
            target,
        })
        .await?;
    let approval_id = match reply.payload {
        Payload::DocumentPublishRequested { approval_id, .. } => approval_id,
        Payload::CommandRejected(error) => {
            anyhow::bail!("publish rejected: {} ({})", error.message, error.code)
        }
        other => anyhow::bail!("unexpected reply to PublishDocument: {other:?}"),
    };

    let reply = conn
        .send_command(CommandBody::ResolveApproval {
            approval_id,
            decision,
            scope: ApprovalScope::Once,
        })
        .await?;
    match reply.payload {
        Payload::CommandAccepted { .. } => Ok(approval_id),
        Payload::CommandRejected(error) => {
            anyhow::bail!(
                "could not resolve the publish approval: {} ({})",
                error.message,
                error.code
            )
        }
        other => anyhow::bail!("unexpected reply to ResolveApproval: {other:?}"),
    }
}

/// Resolve `kind` (plus any explicit `--path`/`--branch`/`--title`) into the
/// knowledge engine's domain `PublishTarget`, filling in sensible defaults: a
/// slug of the document's title under `docs/` for `path`, `docs/publish` for
/// `branch`, and `Publish: <title>` for a PR's `title`.
fn resolve_publish_target(
    kind: PublishTargetKind,
    document_title: &str,
    path: Option<String>,
    branch: Option<String>,
    title: Option<String>,
) -> KnowledgePublishTarget {
    let path = path.unwrap_or_else(|| format!("docs/{}.md", slugify(document_title)));
    match kind {
        PublishTargetKind::RepoFile => KnowledgePublishTarget::RepositoryFile { path },
        PublishTargetKind::DocsBranch => {
            let branch = branch.unwrap_or_else(|| "docs/publish".to_string());
            KnowledgePublishTarget::DocsBranchCommit { branch, path }
        }
        PublishTargetKind::DocPr => {
            let branch = branch.unwrap_or_else(|| "docs/publish".to_string());
            let title = title.unwrap_or_else(|| format!("Publish: {document_title}"));
            KnowledgePublishTarget::DocumentationPr {
                branch,
                path,
                title,
            }
        }
    }
}

/// A short human description of a target, matching the daemon seam's own
/// `describe_target` (kept independent — the CLI's is a client-side preview
/// computed from the SAME plan function, not a value the daemon returns until
/// after `PublishDocument` is sent).
fn describe_publish_target(target: &KnowledgePublishTarget) -> String {
    match target {
        KnowledgePublishTarget::RepositoryFile { path } => format!("repository file {path}"),
        KnowledgePublishTarget::DocsBranchCommit { branch, path } => {
            format!("docs-branch commit {path} on {branch}")
        }
        KnowledgePublishTarget::DocumentationPr {
            branch,
            path,
            title,
        } => format!("documentation PR \"{title}\" ({path} on {branch})"),
    }
}

/// Convert the knowledge engine's domain `PublishTarget` into its wire mirror
/// for the `PublishDocument` command.
fn to_wire_target(
    target: &KnowledgePublishTarget,
) -> codypendent_protocol::document::PublishTarget {
    use codypendent_protocol::document::PublishTarget as Wire;
    match target {
        KnowledgePublishTarget::RepositoryFile { path } => {
            Wire::RepositoryFile { path: path.clone() }
        }
        KnowledgePublishTarget::DocsBranchCommit { branch, path } => Wire::DocsBranchCommit {
            branch: branch.clone(),
            path: path.clone(),
        },
        KnowledgePublishTarget::DocumentationPr {
            branch,
            path,
            title,
        } => Wire::DocumentationPr {
            branch: branch.clone(),
            path: path.clone(),
            title: title.clone(),
        },
    }
}

/// A filesystem/branch-safe slug: lowercased alphanumerics, runs of anything
/// else collapsed to a single `-`, with no leading/trailing dash. Never empty
/// (falls back to `document`).
fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "document".to_string()
    } else {
        slug
    }
}

/// Poll the publication history for a fresh row beyond `existing_count`
/// (the daemon's execution is fire-and-forget once the approval resolves),
/// or give up after a generous bound and let the caller report "still
/// running" rather than hang indefinitely.
async fn wait_for_new_publication(
    pool: &sqlx::SqlitePool,
    document_id: DocumentId,
    existing_count: usize,
) -> Option<Publication> {
    for _ in 0..100 {
        let published = publications(pool, document_id).await.ok()?;
        if published.len() > existing_count {
            return published.into_iter().next();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// `codypendent workflow validate <FILE> [--agents <DIR>]` (Phase 5 STEP 5.1):
/// parse and compile a declarative `workflow.yaml`, reporting either a one-line
/// summary of the validated graph or the precise error (naming the offending
/// step). Self-contained — it never touches the daemon; a manifest and its agent
/// profiles are just text on disk.
///
/// Without `--agents` this is **structural** validation: schema version,
/// unique/non-empty ids, exactly one action per step, resolvable + acyclic
/// dependencies, budget sanity, and the multi-agent `orchestration_reason` rule.
/// With `--agents <DIR>` it additionally **resolves agent roles**: every agent
/// step's short role must be fulfilled by a profile in that directory, so an
/// author catches a role with no profile before a run reaches it. (Whether a
/// named *tool* or *skill* exists still needs the live registry — a daemon-side
/// cross-check via `compile_with_registry`.)
pub fn workflow_validate(
    file: &std::path::Path,
    agents: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let yaml = std::fs::read_to_string(file)
        .with_context(|| format!("reading workflow manifest {}", file.display()))?;
    // A structural error is the user's to fix — surface it verbatim, tagged with
    // the file, and exit non-zero (via `?` in `main`).
    let compiled = codypendent_workflow::compile_yaml(&yaml)
        .map_err(|error| anyhow::anyhow!("{}: {error}", file.display()))?;
    println!("{}", workflow_summary(&compiled));

    if let Some(agents_dir) = agents {
        let profiles = codypendent_workflow::AgentProfileSet::load_dir(agents_dir)
            .with_context(|| format!("loading agent profiles from {}", agents_dir.display()))?;
        let unresolved = profiles.unresolved_roles(&compiled);
        if unresolved.is_empty() {
            println!(
                "\u{2713} agent roles: all resolved against {} ({} profile(s))",
                agents_dir.display(),
                profiles.len(),
            );
        } else {
            // Report every unresolved role so an author fixes them in one pass.
            let detail = unresolved
                .iter()
                .map(|r| format!("step `{}` \u{2192} role `{}`", r.step, r.role))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "{}: {} agent role(s) unresolved against {}: {detail}",
                file.display(),
                unresolved.len(),
                agents_dir.display(),
            );
        }
    }
    Ok(())
}

/// `codypendent workflow show <FILE> [--json]` (Phase 5 STEP 5.2): compile a
/// manifest and print its full graph — every node's action, dependencies,
/// workspace, approval, retry, and declared outputs — as a human tree or, with
/// `--json`, the serialized [`CompiledWorkflow`] projection a graph-view client
/// consumes. Structural compilation only, like [`workflow_validate`]; a compile
/// error is surfaced verbatim and exits non-zero.
pub fn workflow_show(file: &std::path::Path, json: bool) -> anyhow::Result<()> {
    let yaml = std::fs::read_to_string(file)
        .with_context(|| format!("reading workflow manifest {}", file.display()))?;
    let compiled = codypendent_workflow::compile_yaml(&yaml)
        .map_err(|error| anyhow::anyhow!("{}: {error}", file.display()))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&compiled)?);
    } else {
        print!("{}", workflow_tree(&compiled));
    }
    Ok(())
}

/// `codypendent workflow run <FILE> [--inputs <JSON>] [--repo <PATH>]` (Phase 5
/// STEP 5.2): start a durable workflow run. Ensures a daemon, sends `StartWorkflow`,
/// and prints the new run id the daemon drives to a terminal state in the
/// background. The manifest content (never a path) is what crosses the wire,
/// `--inputs` is parsed as a JSON value the manifest's typed inputs bind to, and
/// `repo`'s canonical path is carried so the daemon carves each writing node its
/// own isolated worktree from *this* checkout (Phase 5 T5) — mirroring how the
/// `run` command carries `StartRun`'s repository.
pub async fn workflow_run(
    paths: &RuntimePaths,
    file: &std::path::Path,
    inputs: Option<String>,
    repo: PathBuf,
) -> anyhow::Result<()> {
    let manifest = std::fs::read_to_string(file)
        .with_context(|| format!("reading workflow manifest {}", file.display()))?;
    let inputs = match inputs {
        Some(text) => {
            serde_json::from_str(&text).with_context(|| "parsing --inputs as a JSON value")?
        }
        None => serde_json::Value::Null,
    };
    let repo = repo.canonicalize().with_context(|| {
        format!(
            "--repo {}: not a valid, accessible directory",
            repo.display()
        )
    })?;
    if !repo.is_dir() {
        anyhow::bail!("--repo {}: not a directory", repo.display());
    }
    let repository = repo.to_string_lossy().into_owned();
    ensure_daemon(paths).await?;
    let mut conn = Connection::connect(&paths.socket_path).await?;
    let run_id =
        workflow_run_over_connection(&mut conn, manifest, inputs, Some(repository)).await?;
    println!("workflow run started: {run_id}");
    Ok(())
}

/// The connected core of [`workflow_run`]: handshake, bind the `Controller` role,
/// send `StartWorkflow`, and return the new run id. Split out so a test can drive it
/// against a mock server (like [`run_over_connection`]). `repository` is the
/// canonical repo root the run's agent nodes operate on (Phase 5 T5); `None` lets
/// the daemon fall back to its startup repository.
pub async fn workflow_run_over_connection(
    conn: &mut Connection,
    manifest: String,
    inputs: serde_json::Value,
    repository: Option<String>,
) -> anyhow::Result<String> {
    conn.handshake("codypendent", env!("CARGO_PKG_VERSION"), None)
        .await?;
    bind_control_role(conn).await?;
    let reply = conn
        .send_command(CommandBody::StartWorkflow {
            manifest,
            inputs,
            repository,
        })
        .await?;
    match reply.payload {
        Payload::WorkflowRunStarted {
            workflow_run_id, ..
        } => Ok(workflow_run_id),
        Payload::CommandRejected(error) => {
            anyhow::bail!("StartWorkflow rejected: {} ({})", error.message, error.code)
        }
        other => anyhow::bail!("unexpected reply to StartWorkflow: {other:?}"),
    }
}

/// `codypendent workflow pause <RUN_ID>` (Phase 5 STEP 5.2).
pub async fn workflow_pause(paths: &RuntimePaths, workflow_run_id: String) -> anyhow::Result<()> {
    lifecycle_command(
        paths,
        CommandBody::PauseWorkflow { workflow_run_id },
        "pause",
    )
    .await
}

/// `codypendent workflow resume <RUN_ID>` (Phase 5 STEP 5.2).
pub async fn workflow_resume(paths: &RuntimePaths, workflow_run_id: String) -> anyhow::Result<()> {
    lifecycle_command(
        paths,
        CommandBody::ResumeWorkflow { workflow_run_id },
        "resume",
    )
    .await
}

/// `codypendent workflow retry <RUN_ID> --node <NODE>` (Phase 5 STEP 5.2).
pub async fn workflow_retry(
    paths: &RuntimePaths,
    workflow_run_id: String,
    node: String,
) -> anyhow::Result<()> {
    lifecycle_command(
        paths,
        CommandBody::RetryWorkflowNode {
            workflow_run_id,
            node_id: node,
        },
        "retry",
    )
    .await
}

/// Send one workflow lifecycle command to a *running* daemon (it does not start
/// one — pausing/resuming/retrying only makes sense against live durable runs) and
/// report whether it was accepted. `verb` names the action in the output/errors.
async fn lifecycle_command(
    paths: &RuntimePaths,
    body: CommandBody,
    verb: &str,
) -> anyhow::Result<()> {
    let mut conn = Connection::connect(&paths.socket_path)
        .await
        .with_context(|| "connecting to the daemon (is it running?)")?;
    conn.handshake("codypendent", env!("CARGO_PKG_VERSION"), None)
        .await?;
    bind_control_role(&mut conn).await?;
    let reply = conn.send_command(body).await?;
    match reply.payload {
        Payload::CommandAccepted { .. } => {
            println!("workflow {verb} accepted");
            Ok(())
        }
        Payload::CommandRejected(error) => {
            anyhow::bail!(
                "workflow {verb} rejected: {} ({})",
                error.message,
                error.code
            )
        }
        other => anyhow::bail!("unexpected reply to workflow {verb}: {other:?}"),
    }
}

/// Bind this connection to the `Controller` role, which starting and controlling a
/// workflow requires. Roles bind at the connection level via an `AttachSession`
/// (Chapter 03); a workflow lives outside any session, so we attach to a throwaway
/// session id purely for the role — the daemon binds the role before it checks the
/// session, so the expected `session-not-found` rejection is irrelevant and ignored.
async fn bind_control_role(conn: &mut Connection) -> anyhow::Result<()> {
    conn.send_command(CommandBody::AttachSession {
        session_id: SessionId::new(),
        last_seen_sequence: None,
        subscriptions: vec![],
        requested_role: ClientRole::Controller,
    })
    .await?;
    Ok(())
}

// --- STEP 7.1: eval harness runner -------------------------------------------

/// `codypendent eval run --suite <NAME> [--policy P] --report out.json`
/// (Phase 7 STEP 7.1). Loads every case in the suite, runs each headlessly
/// against its pinned fixture revision, scores it, and writes the aggregate
/// [`codypendent_eval::SuiteReport`] to `--report`. `policy` is recorded on
/// stdout only — Phase 7 routing is not yet wired into `StartRun` (see the
/// roadmap's "routing⇄eval composition" note), so it does not yet select a
/// model.
pub async fn eval_run(
    paths: &RuntimePaths,
    suite: &str,
    policy: Option<String>,
    report: &std::path::Path,
) -> anyhow::Result<()> {
    let suite_dir = crate::eval::resolve_suite_dir(suite)?;
    let cases = crate::eval::load_suite(&suite_dir)?;
    println!(
        "eval: loaded {} case(s) from {}{}",
        cases.len(),
        suite_dir.display(),
        policy
            .as_deref()
            .map(|p| format!(" (policy: {p}, not yet enforced)"))
            .unwrap_or_default()
    );
    let fixture_root = crate::eval::fixture_root(&suite_dir, "tiny-crate")?;
    let suite_report = crate::eval::run_suite(paths, &cases, &fixture_root).await?;

    let json = serde_json::to_string_pretty(&suite_report)?;
    std::fs::write(report, json)
        .with_context(|| format!("writing suite report to {}", report.display()))?;

    let passed = suite_report.results.iter().filter(|r| r.passed()).count();
    println!(
        "eval: {passed}/{} case(s) passed ({:.0}%); report written to {}",
        suite_report.results.len(),
        suite_report.success_rate() * 100.0,
        report.display()
    );
    if !suite_report.all_passed() {
        anyhow::bail!(
            "eval suite did not pass: failed case(s): {}",
            suite_report.failed_case_ids().join(", ")
        );
    }
    Ok(())
}

// --- STEP 7.5: promotion pipeline commands ----------------------------------

/// `codypendent promote propose --kind K --name NAME --version N
/// [--requires-permission-review]` (Phase 7 STEP 7.5). Ensures a daemon (like
/// `workflow run`, this is the "creation" verb) and prints the new candidate id.
pub async fn promote_propose(
    paths: &RuntimePaths,
    kind: String,
    name: String,
    version: u32,
    requires_permission_review: bool,
) -> anyhow::Result<()> {
    ensure_daemon(paths).await?;
    let mut conn = Connection::connect(&paths.socket_path).await?;
    let candidate_id =
        promote_propose_over_connection(&mut conn, kind, name, version, requires_permission_review)
            .await?;
    println!("promotion candidate proposed: {candidate_id}");
    Ok(())
}

/// The connected core of [`promote_propose`]: handshake, bind the `Controller`
/// role, send `ProposePromotion`, and return the new candidate id. Split out
/// for the same testability reason as [`workflow_run_over_connection`].
pub async fn promote_propose_over_connection(
    conn: &mut Connection,
    kind: String,
    name: String,
    version: u32,
    requires_permission_review: bool,
) -> anyhow::Result<String> {
    conn.handshake("codypendent", env!("CARGO_PKG_VERSION"), None)
        .await?;
    bind_control_role(conn).await?;
    let reply = conn
        .send_command(CommandBody::ProposePromotion {
            kind,
            name,
            version,
            requires_permission_review,
        })
        .await?;
    match reply.payload {
        Payload::PromotionProposed { candidate_id, .. } => Ok(candidate_id),
        Payload::CommandRejected(error) => {
            anyhow::bail!("propose rejected: {} ({})", error.message, error.code)
        }
        other => anyhow::bail!("unexpected reply to ProposePromotion: {other:?}"),
    }
}

/// `codypendent promote advance <CANDIDATE_ID> --step <STEP> [--regressed]`
/// (Phase 7 STEP 7.5).
pub async fn promote_advance(
    paths: &RuntimePaths,
    candidate_id: String,
    action: PromotionAction,
) -> anyhow::Result<()> {
    promotion_command(
        paths,
        CommandBody::AdvancePromotion {
            candidate_id,
            action,
        },
        "advance",
    )
    .await
}

/// `codypendent promote approve <CANDIDATE_ID>` — the human-approval gate
/// (Phase 7 STEP 7.5, ADR-010). This command does not start a daemon: approving
/// only makes sense against an already-running one with a real candidate.
pub async fn promote_approve(paths: &RuntimePaths, candidate_id: String) -> anyhow::Result<()> {
    promotion_command(
        paths,
        CommandBody::ApprovePromotion { candidate_id },
        "approve",
    )
    .await
}

/// `codypendent promote rollback <CANDIDATE_ID>` (Phase 7 STEP 7.5).
pub async fn promote_rollback(paths: &RuntimePaths, candidate_id: String) -> anyhow::Result<()> {
    promotion_command(
        paths,
        CommandBody::RollbackPromotion { candidate_id },
        "rollback",
    )
    .await
}

/// Send one promotion command to a *running* daemon (mirrors
/// [`lifecycle_command`]: advancing/approving/rolling back only makes sense
/// against a daemon that already exists) and report whether it was accepted.
async fn promotion_command(
    paths: &RuntimePaths,
    body: CommandBody,
    verb: &str,
) -> anyhow::Result<()> {
    let mut conn = Connection::connect(&paths.socket_path)
        .await
        .with_context(|| "connecting to the daemon (is it running?)")?;
    promotion_command_over_connection(&mut conn, body, verb).await
}

/// The connected core of [`promotion_command`]: handshake, bind the
/// `Controller` role, send `body`, and report whether it was accepted. Split
/// out (and `pub`, like [`workflow_run_over_connection`]) so a test can drive
/// it against a hand-rolled mock daemon.
pub async fn promotion_command_over_connection(
    conn: &mut Connection,
    body: CommandBody,
    verb: &str,
) -> anyhow::Result<()> {
    conn.handshake("codypendent", env!("CARGO_PKG_VERSION"), None)
        .await?;
    bind_control_role(conn).await?;
    let reply = conn.send_command(body).await?;
    match reply.payload {
        Payload::CommandAccepted { .. } => {
            println!("promotion {verb} accepted");
            Ok(())
        }
        Payload::CommandRejected(error) => {
            anyhow::bail!(
                "promotion {verb} rejected: {} ({})",
                error.message,
                error.code
            )
        }
        other => anyhow::bail!("unexpected reply to promotion {verb}: {other:?}"),
    }
}

/// A human, indented rendering of a compiled workflow graph. Pure, so it is tested
/// directly. Nodes are listed in topological order; each shows its action and the
/// execution-affecting settings that are set.
fn workflow_tree(compiled: &codypendent_workflow::CompiledWorkflow) -> String {
    use codypendent_workflow::NodeAction;
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} v{} ({} step(s), {} agent step(s))",
        compiled.id,
        compiled.version,
        compiled.nodes.len(),
        compiled.agent_node_count()
    );
    for node in &compiled.nodes {
        let action = match &node.action {
            NodeAction::Agent { role, skill, .. } => match skill {
                Some(skill) => format!("agent {role} · skill {skill}"),
                None => format!("agent {role}"),
            },
            NodeAction::Tool { name } => format!("tool {name}"),
        };
        let _ = writeln!(out, "  - {} [{action}]", node.id);
        if !node.depends_on.is_empty() {
            let _ = writeln!(out, "      depends_on: {}", node.depends_on.join(", "));
        }
        if let Some(approval) = &node.approval {
            let _ = writeln!(out, "      approval: {approval:?}");
        }
        if !node.outputs.is_empty() {
            let _ = writeln!(out, "      outputs: {}", node.outputs.join(", "));
        }
    }
    out
}

/// A one-line human summary of a validated workflow graph. Pure, so it is tested
/// directly.
fn workflow_summary(compiled: &codypendent_workflow::CompiledWorkflow) -> String {
    let order: Vec<&str> = compiled.nodes.iter().map(|node| node.id.as_str()).collect();
    format!(
        "\u{2713} {} v{} valid — {} step(s), {} agent step(s); order: {}",
        compiled.id,
        compiled.version,
        compiled.nodes.len(),
        compiled.agent_node_count(),
        order.join(" \u{2192} "),
    )
}

/// `codypendent plugin inspect <FILE>` (Phase 6 STEP 6.1): parse a `plugin.toml`
/// and render its identity, requested capabilities, resource caps, and trust
/// posture — the "evaluate permissions (render the capability list to the user)"
/// step, before a plugin is ever enabled. Manifest parsing only; nothing runs.
pub fn plugin_inspect(file: &std::path::Path) -> anyhow::Result<()> {
    let toml = std::fs::read_to_string(file)
        .with_context(|| format!("reading plugin manifest {}", file.display()))?;
    let manifest = codypendent_sandbox::parse_manifest(&toml)
        .map_err(|error| anyhow::anyhow!("{}: {error}", file.display()))?;
    print!("{}", plugin_report(&manifest));
    Ok(())
}

/// `codypendent plugin diff <INSTALLED> <UPDATE>` (Phase 6 STEP 6.1): parse both
/// manifests, print the permission diff (capabilities **and** resource caps —
/// P6-A), and report whether the update expands permissions and so requires
/// re-approval (exit criterion 2). Exits non-zero when the update expands
/// permissions, so a caller (or CI) can gate on it.
pub fn plugin_diff(installed: &std::path::Path, update: &std::path::Path) -> anyhow::Result<()> {
    let installed_manifest = read_manifest(installed)?;
    let update_manifest = read_manifest(update)?;
    if installed_manifest.id != update_manifest.id {
        anyhow::bail!(
            "these are different plugins ({} vs {}); a diff compares versions of one plugin",
            installed_manifest.id,
            update_manifest.id
        );
    }
    // diff_manifests() folds resource-cap changes in alongside the capability
    // diff (P6-A) — a bare CapabilitySet::diff_to() here would miss a raised
    // memory/cpu/wall/output cap entirely, since it has no resource data to
    // compare, letting this CI re-approval gate print "safe" and exit 0 on
    // exactly the update it exists to catch.
    let diff = codypendent_sandbox::diff_manifests(&installed_manifest, &update_manifest);
    print!("{}", plugin_diff_report(&installed_manifest.id, &diff));
    if diff.expands_permissions() {
        // A widening update is not applied without re-approval — signal that with a
        // non-zero exit so automation blocks on it.
        anyhow::bail!("update expands permissions — re-approval required before it can be applied");
    }
    Ok(())
}

fn read_manifest(file: &std::path::Path) -> anyhow::Result<codypendent_sandbox::PluginManifest> {
    let toml = std::fs::read_to_string(file)
        .with_context(|| format!("reading plugin manifest {}", file.display()))?;
    codypendent_sandbox::parse_manifest(&toml)
        .map_err(|error| anyhow::anyhow!("{}: {error}", file.display()))
}

/// A human rendering of a plugin manifest's identity, capabilities, resources, and
/// trust posture. Pure, so it is tested directly.
fn plugin_report(manifest: &codypendent_sandbox::PluginManifest) -> String {
    use codypendent_sandbox::CapabilitySet;
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} v{} ({}) — publisher {}",
        manifest.id,
        manifest.version,
        manifest.kind.as_str(),
        manifest.publisher,
    );
    let trust = if manifest.security.is_signed() {
        "signed"
    } else {
        "unsigned"
    };
    let checksum = if manifest.security.checksum.is_empty() {
        "no checksum"
    } else {
        manifest.security.checksum.as_str()
    };
    let profile = if manifest.security.sandbox_profile.is_empty() {
        "(none)"
    } else {
        manifest.security.sandbox_profile.as_str()
    };
    let _ = writeln!(
        out,
        "  trust: {trust} ({checksum}), sandbox profile {profile}"
    );

    let caps = CapabilitySet::from_spec(&manifest.capabilities);
    if caps.is_empty() {
        let _ = writeln!(
            out,
            "  capabilities: none — this plugin requests no capabilities"
        );
    } else {
        let _ = writeln!(out, "  capabilities:");
        for cap in caps.iter() {
            let _ = writeln!(out, "    {cap}");
        }
    }

    let r = &manifest.resources;
    let _ = writeln!(
        out,
        "  resources: {} MB mem, {} CPU s, {} wall s, {} MB output",
        r.memory_mb, r.cpu_seconds, r.wall_seconds, r.maximum_output_mb,
    );
    if !manifest.scopes.is_empty() {
        let _ = writeln!(out, "  scopes: {}", manifest.scopes.join(", "));
    }
    out
}

/// A human rendering of a permission diff between two versions of a plugin, with
/// the re-approval verdict. Pure, so it is tested directly.
fn plugin_diff_report(id: &str, diff: &codypendent_sandbox::PermissionDiff) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    if diff.is_identical() {
        let _ = writeln!(out, "{id}: no permission changes — safe to update.");
        return out;
    }
    let _ = writeln!(out, "{id}: permission changes:");
    let _ = writeln!(out, "{}", diff.render());
    if diff.expands_permissions() {
        let _ = writeln!(
            out,
            "\u{2192} update EXPANDS permissions — re-approval required (exit criterion 2)."
        );
    } else {
        let _ = writeln!(
            out,
            "\u{2192} update only narrows permissions — safe to update."
        );
    }
    out
}

/// Resolve the trusted-publisher key store path
/// (`<config_dir>/codypendent/trusted_publishers.toml`, the `models.toml`
/// precedent). `CODYPENDENT_CONFIG_DIR` overrides the config root (test isolation),
/// mirroring how `CODYPENDENT_DATA_DIR` overrides the data root.
fn trusted_publishers_path() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CODYPENDENT_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join("trusted_publishers.toml"));
    }
    let dirs = directories::ProjectDirs::from("", "", "codypendent")
        .context("cannot determine a config directory for the current user")?;
    Ok(dirs.config_dir().join("trusted_publishers.toml"))
}

/// `codypendent plugin trust add <ID> <PUBLIC_KEY_B64>` (Phase 6 STEP 6.2): record
/// a trusted publisher's ed25519 public key so signed plugins from that publisher
/// verify against a real key. The key is validated before it is stored.
pub fn plugin_trust_add(id: &str, public_key_b64: &str) -> anyhow::Result<()> {
    let path = trusted_publishers_path()?;
    let mut store = codypendent_sandbox::TrustedPublishers::load(&path)
        .with_context(|| format!("loading trusted-publisher store {}", path.display()))?;
    store
        .add(id, public_key_b64)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    store
        .save(&path)
        .with_context(|| format!("writing trusted-publisher store {}", path.display()))?;
    println!("Trusted publisher `{id}` added ({}).", path.display());
    Ok(())
}

/// `codypendent plugin trust list` (Phase 6 STEP 6.2): print the trusted
/// publishers and their public keys.
pub fn plugin_trust_list() -> anyhow::Result<()> {
    let path = trusted_publishers_path()?;
    let store = codypendent_sandbox::TrustedPublishers::load(&path)
        .with_context(|| format!("loading trusted-publisher store {}", path.display()))?;
    if store.is_empty() {
        println!(
            "No trusted publishers ({}). Signed plugins from unknown publishers are refused.",
            path.display()
        );
        return Ok(());
    }
    println!("Trusted publishers ({}):", path.display());
    for (id, key) in store.list() {
        println!("  {id}  {key}");
    }
    Ok(())
}

/// `codypendent plugin trust remove <ID>` (Phase 6 STEP 6.2): stop trusting a
/// publisher. Exits non-zero if the publisher was not present.
pub fn plugin_trust_remove(id: &str) -> anyhow::Result<()> {
    let path = trusted_publishers_path()?;
    let mut store = codypendent_sandbox::TrustedPublishers::load(&path)
        .with_context(|| format!("loading trusted-publisher store {}", path.display()))?;
    if !store.remove(id) {
        anyhow::bail!("publisher `{id}` was not trusted");
    }
    store
        .save(&path)
        .with_context(|| format!("writing trusted-publisher store {}", path.display()))?;
    println!("Trusted publisher `{id}` removed.");
    Ok(())
}

/// `codypendent plugin verify <MANIFEST> <ARTIFACT>` (Phase 6 STEP 6.2): verify a
/// plugin artifact against its manifest using the **trusted-publisher key store** —
/// the real-keys install gate. Checksum + signature are checked, then the grant is
/// evaluated (`install_disabled`). A signed plugin from an unknown publisher, a bad
/// signature, or an unsigned plugin (unless `--allow-unsigned`) is **refused** with
/// a non-zero exit, so this is the fail-closed pre-install verification a stateful
/// `plugin install` builds on (persisting the installed record is daemon-wired
/// follow-up work).
pub fn plugin_verify(
    manifest_file: &std::path::Path,
    artifact_file: &std::path::Path,
    allow_unsigned: bool,
) -> anyhow::Result<()> {
    let manifest = read_manifest(manifest_file)?;
    let artifact = std::fs::read(artifact_file)
        .with_context(|| format!("reading plugin artifact {}", artifact_file.display()))?;

    let path = trusted_publishers_path()?;
    let store = codypendent_sandbox::TrustedPublishers::load(&path)
        .with_context(|| format!("loading trusted-publisher store {}", path.display()))?;

    let unsigned = if allow_unsigned {
        codypendent_sandbox::UnsignedPolicy::Allow
    } else {
        codypendent_sandbox::UnsignedPolicy::Deny
    };
    // The store is the resolver: an unknown publisher yields no key, so a signed
    // plugin fails closed (default-deny unsigned already covers the unsigned case).
    let publisher_key = store.key_for(&manifest.publisher);

    // Full grant at install: the profile is derived from what the manifest requests.
    let granted = codypendent_sandbox::CapabilitySet::from_spec(&manifest.capabilities);
    let installed = codypendent_sandbox::InstalledPlugin::install_disabled(
        manifest.clone(),
        &artifact,
        publisher_key.map(|k| k.as_slice()),
        unsigned,
        granted,
    )
    .map_err(|error| {
        anyhow::anyhow!("{} @ {}: refused — {error}", manifest.id, manifest.version)
    })?;

    let trust = if installed.signed {
        format!("signed by trusted publisher `{}`", manifest.publisher)
    } else {
        "unsigned (allowed by --allow-unsigned)".to_string()
    };
    println!(
        "\u{2713} {} v{} verified — {trust}; installed disabled (inert until enabled).",
        manifest.id, manifest.version,
    );
    Ok(())
}

/// The handoff instructions printed by [`open`]. Pure (no I/O) so it is testable.
fn handoff_message(session_id: SessionId, paths: &RuntimePaths, ide_name: &str) -> String {
    format!(
        "Handing session {session_id} off to {ide_name}.\n\
         The editor attaches as a contributor to this session — the run keeps \
         going, it does not restart.\n\
         Session: {session_id}\n\
         Socket:  {}",
        paths.socket_path.display()
    )
}

#[cfg(test)]
mod open_tests {
    use super::*;

    #[test]
    fn handoff_message_names_the_session_and_socket() {
        let paths = RuntimePaths::from_data_dir(std::path::PathBuf::from("/tmp/cp-test"));
        let session = SessionId::new();
        let message = handoff_message(session, &paths, "VS Code");
        assert!(message.contains(&session.to_string()));
        assert!(message.contains("VS Code"));
        assert!(message.contains("does not restart"));
        assert!(message.contains(&paths.socket_path.display().to_string()));
    }
}

#[cfg(test)]
mod workflow_tests {
    use super::*;

    const VALID: &str = "\
schema_version: 1
id: pipeline
version: 2
budget:
  maximum_cost_usd: 5.0
steps:
  - id: build
    tool: repository.test
  - id: check
    depends_on: [build]
    tool: repository.test
";

    #[test]
    fn summary_reports_id_version_counts_and_order() {
        let compiled = codypendent_workflow::compile_yaml(VALID).unwrap();
        let summary = workflow_summary(&compiled);
        assert!(summary.contains("pipeline v2 valid"));
        assert!(summary.contains("2 step(s)"));
        assert!(summary.contains("0 agent step(s)"));
        // Topological order is shown, dependency first.
        assert!(summary.contains("build \u{2192} check"), "got: {summary}");
    }

    #[test]
    fn validate_accepts_a_good_manifest_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wf.yaml");
        std::fs::write(&path, VALID).unwrap();
        workflow_validate(&path, None).expect("a valid manifest validates");
    }

    #[test]
    fn validate_reports_a_compile_error_tagged_with_the_file() {
        // A step depending on a missing step fails to compile; the error names the
        // file and the offending dependency.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("broken.yaml");
        std::fs::write(
            &path,
            "schema_version: 1\nid: wf\nversion: 1\nsteps:\n  - id: a\n    depends_on: [ghost]\n    tool: repository.test\n",
        )
        .unwrap();
        let err = workflow_validate(&path, None).unwrap_err().to_string();
        assert!(err.contains("broken.yaml"), "error names the file: {err}");
        assert!(err.contains("ghost"), "error names the bad dep: {err}");
    }

    #[test]
    fn validate_reports_a_missing_file() {
        let err = workflow_validate(std::path::Path::new("/no/such/manifest.yaml"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("reading workflow manifest"));
    }

    #[test]
    fn validate_with_agents_resolves_or_reports_roles() {
        // `AGENT_MANIFEST` has one agent step (`inspect`, role `investigator`).
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("wf.yaml");
        std::fs::write(&manifest, AGENT_MANIFEST).unwrap();
        let agents = tmp.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();

        // No profile fulfils `investigator` yet → the cross-check fails, naming
        // the manifest, the step, and the unresolved role.
        let err = workflow_validate(&manifest, Some(&agents))
            .unwrap_err()
            .to_string();
        assert!(err.contains("wf.yaml"), "names the manifest: {err}");
        assert!(err.contains("investigator"), "names the role: {err}");
        assert!(err.contains("inspect"), "names the step: {err}");

        // Add a profile fulfilling the role (via the id suffix) → it resolves.
        std::fs::write(
            agents.join("scout.toml"),
            "schema_version = 1\nid = \"agents.investigator\"\nname = \"Scout\"\n",
        )
        .unwrap();
        workflow_validate(&manifest, Some(&agents)).expect("every agent role now resolves");
    }

    const AGENT_MANIFEST: &str = "\
schema_version: 1
id: review-flow
version: 1
budget:
  maximum_cost_usd: 5.0
  maximum_agents: 1
steps:
  - id: inspect
    agent:
      role: investigator
    skill: github.inspect-failed-check
    outputs: [finding]
  - id: publish
    depends_on: [inspect]
    tool: github.update-pull-request
    approval: always
";

    #[test]
    fn tree_shows_each_node_action_edge_and_settings() {
        let compiled = codypendent_workflow::compile_yaml(AGENT_MANIFEST).unwrap();
        let tree = workflow_tree(&compiled);
        assert!(tree.contains("review-flow v1"));
        assert!(tree.contains("inspect [agent investigator · skill github.inspect-failed-check]"));
        assert!(tree.contains("publish [tool github.update-pull-request]"));
        assert!(tree.contains("depends_on: inspect"));
        assert!(tree.contains("approval: Always"));
        assert!(tree.contains("outputs: finding"));
    }

    #[test]
    fn show_json_emits_a_parseable_graph_projection() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wf.yaml");
        std::fs::write(&path, AGENT_MANIFEST).unwrap();
        // The command runs; and the same compiled graph serializes to the JSON
        // shape a graph-view client parses (tagged actions, edges).
        workflow_show(&path, true).expect("show --json succeeds");
        let compiled = codypendent_workflow::compile_yaml(AGENT_MANIFEST).unwrap();
        let value = serde_json::to_value(&compiled).unwrap();
        assert_eq!(value["id"], "review-flow");
        assert_eq!(value["nodes"][0]["action"]["kind"], "agent");
        assert_eq!(
            value["nodes"][1]["action"]["name"],
            "github.update-pull-request"
        );
    }
}

#[cfg(test)]
mod plugin_tests {
    use super::*;

    const GITHUB_MANIFEST: &str = r#"
schema_version = 1
id = "github"
name = "GitHub Integration"
version = "0.1.0"
kind = "native-process"
publisher = "codypendent-project"
scopes = ["user", "organization", "repository"]
[runtime]
command = "codypendent-plugin-github"
protocol = "mcp-stdio"
[capabilities]
network = ["api.github.com:443", "uploads.github.com:443"]
secrets = ["github-token"]
subprocess = false
[resources]
memory_mb = 256
cpu_seconds = 60
wall_seconds = 120
maximum_output_mb = 20
[security]
checksum = "sha256:set-during-packaging"
signature = "set-during-packaging"
sandbox_profile = "network-client"
"#;

    #[test]
    fn report_renders_identity_capabilities_and_trust() {
        let manifest = codypendent_sandbox::parse_manifest(GITHUB_MANIFEST).unwrap();
        let report = plugin_report(&manifest);
        assert!(report.contains("github v0.1.0 (native-process)"));
        assert!(report.contains("trust: unsigned"));
        assert!(report.contains("sandbox profile network-client"));
        // The capability list is rendered verbatim, one per line.
        assert!(report.contains("network: api.github.com:443"));
        assert!(report.contains("network: uploads.github.com:443"));
        assert!(report.contains("secret: github-token"));
        assert!(report.contains("256 MB mem"));
        assert!(report.contains("scopes: user, organization, repository"));
    }

    #[test]
    fn report_notes_a_capability_free_plugin() {
        let manifest = codypendent_sandbox::parse_manifest(
            "schema_version = 1\nid = \"theme\"\nname = \"T\"\nversion = \"1.0.0\"\nkind = \"wasm-component\"\npublisher = \"me\"\n[runtime]\ncommand = \"t.wasm\"\n",
        )
        .unwrap();
        let report = plugin_report(&manifest);
        assert!(report.contains("requests no capabilities"));
    }

    #[test]
    fn inspect_reads_a_manifest_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin.toml");
        std::fs::write(&path, GITHUB_MANIFEST).unwrap();
        plugin_inspect(&path).expect("inspect succeeds on a valid manifest");
    }

    #[test]
    fn inspect_surfaces_a_parse_error_with_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "schema_version = 99\nid = \"x\"\n").unwrap();
        let err = plugin_inspect(&path).unwrap_err().to_string();
        assert!(err.contains("bad.toml"), "error names the file: {err}");
    }

    fn diff_report_for(installed_net: &[&str], update_net: &[&str]) -> String {
        let spec = |net: &[&str]| codypendent_sandbox::CapabilitiesSpec {
            filesystem_read: vec![],
            filesystem_write: vec![],
            network: net.iter().map(|s| s.to_string()).collect(),
            secrets: vec![],
            subprocess: false,
        };
        let old = codypendent_sandbox::CapabilitySet::from_spec(&spec(installed_net));
        let new = codypendent_sandbox::CapabilitySet::from_spec(&spec(update_net));
        plugin_diff_report("github", &old.diff_to(&new))
    }

    #[test]
    fn diff_report_flags_an_expanding_update() {
        let report = diff_report_for(
            &["api.github.com:443"],
            &["api.github.com:443", "uploads.github.com:443"],
        );
        assert!(report.contains("+ network: uploads.github.com:443"));
        assert!(report.contains("EXPANDS permissions"));
    }

    #[test]
    fn diff_report_marks_an_identical_update_safe() {
        let report = diff_report_for(&["api.github.com:443"], &["api.github.com:443"]);
        assert!(report.contains("no permission changes"));
    }

    #[test]
    fn diff_report_marks_a_narrowing_update_safe() {
        let report = diff_report_for(&["a:1", "b:2"], &["a:1"]);
        assert!(report.contains("only narrows"));
        assert!(!report.contains("EXPANDS"));
    }

    #[test]
    fn diff_command_exits_nonzero_when_permissions_expand() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("installed.toml");
        let update = dir.path().join("update.toml");
        std::fs::write(&installed, GITHUB_MANIFEST).unwrap();
        // The update adds a filesystem_read capability.
        let expanded = GITHUB_MANIFEST.replace(
            "network = [\"api.github.com:443\", \"uploads.github.com:443\"]",
            "network = [\"api.github.com:443\", \"uploads.github.com:443\"]\nfilesystem_read = [\"/etc\"]",
        );
        std::fs::write(&update, expanded).unwrap();
        let err = plugin_diff(&installed, &update).unwrap_err().to_string();
        assert!(err.contains("re-approval required"), "got: {err}");
    }

    #[test]
    fn diff_command_exits_nonzero_when_a_resource_cap_is_raised() {
        // P6-A fix pass: identical capabilities, only the memory cap raised.
        // plugin_diff() now routes through codypendent_sandbox::diff_manifests(),
        // which folds resource-cap changes into the diff. Before that, this
        // computed via a bare CapabilitySet diff that never saw resources,
        // so it printed "no permission changes — safe to update" and exited
        // 0 on exactly the update this CI gate exists to catch.
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("installed.toml");
        let update = dir.path().join("update.toml");
        std::fs::write(&installed, GITHUB_MANIFEST).unwrap();
        let raised = GITHUB_MANIFEST.replace("memory_mb = 256", "memory_mb = 4096");
        std::fs::write(&update, raised).unwrap();
        let err = plugin_diff(&installed, &update).unwrap_err().to_string();
        assert!(err.contains("re-approval required"), "got: {err}");
    }

    #[test]
    fn diff_rejects_two_different_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.toml");
        let b = dir.path().join("b.toml");
        std::fs::write(&a, GITHUB_MANIFEST).unwrap();
        std::fs::write(
            &b,
            GITHUB_MANIFEST.replace("id = \"github\"", "id = \"gitlab\""),
        )
        .unwrap();
        let err = plugin_diff(&a, &b).unwrap_err().to_string();
        assert!(err.contains("different plugins"));
    }
}
