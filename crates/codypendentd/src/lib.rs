//! `codypendent-codypendentd` — the persistent Codypendent daemon, exposed both
//! as the standalone `codypendentd` binary (`src/main.rs`) and as a library so
//! the single `codypendent` binary can run the SAME daemon in-process via the
//! hidden `codypendent __daemon` subcommand. Installing/updating `codypendent`
//! therefore always updates the daemon — no separate binary on disk to go stale
//! (the client/daemon version-skew bug this crate's library form eliminates).
//!
//! This is the composition root. It depends on BOTH `codypendent-daemon` (the
//! server + persistence) and `codypendent-runtime` (the agent loop) — which the
//! daemon crate itself cannot, because the runtime depends on the daemon (a
//! cycle). [`run_daemon`] performs the daemon startup exactly as the old
//! `main.rs` did (paths already resolved, db, boot, recovery), then constructs a
//! [`RuntimeExecutor`] that drives the runtime agent loop and injects it into
//! the server.

// The agent document channel and the client mutation seam are `pub` so the
// crate's own integration tests can drive the SAME production seams the daemon
// wires (see `tests/docs_agent_it.rs`), rather than re-implementing them.
/// The concrete blackboard + task-board seams. Public so an integration test can
/// drive the SAME channel the `task.*` tools use, rather than reproducing the
/// board's write rules in test SQL (which would then drift from them).
/// The assembly side of automation: the [`AutomationEnvironment`] the daemon's
/// scheduler needs (workflow resolution, repository identity, owner
/// resolvability) and the `WebhookEventSink` that turns a verified delivery into
/// workflow runs. These are the production callers that make
/// `automation_bindings` fire at all.
mod automation;
pub mod blackboard;
/// Daemon-side handlers for `codypendent graph {build,status,show}`: the
/// on-demand fold, the report that explains an empty graph, and the
/// repository-scoped reads. Public for the same reason as [`blackboard`] — the
/// crate's own tests drive the REAL gateway rather than reproducing its
/// repository scoping in test SQL, which would then drift from it.
pub mod codegraph_ops;
pub mod control_plane_credentials;
pub mod docs_channel;
mod docs_job;
pub mod documents;
mod executor;
/// The `graph.assert_edge` write seam: the assembly binding that lets a run
/// assert code-graph edges no parser can see (a route handler to the service it
/// dispatches to, a config key to its reader). Public so the integration test
/// drives the SAME binding the executor wires, not a copy of it.
pub mod graph_assertions;
mod learning_capture;
/// Daemon-side handlers for memory inspect/edit/delete and opening a
/// provenance card's source (2026-08-13 review F3/F4). Public so a future
/// protocol command handler (elsewhere) can call these directly, and so this
/// crate's own tests exercise the SAME functions rather than reproducing
/// their scope-visibility check.
pub mod memory_ops;
pub mod model_probe;
mod promotion;
mod publish;
mod retrieval;
mod retrieval_catalog;
mod routing;
// Outcome 11: the writeback that fills a model profile's per-task-class
// success table — the map the router actually reads.
mod routing_outcomes;
/// The code-graph warm-up scan, the live filesystem watcher that keeps the graph
/// current during a session, and the `graph.*` query seam. Public for the same
/// reason as [`blackboard`]: the crate's own integration tests drive the REAL
/// watcher and the REAL scan (`tests/codegraph_live_it.rs`) rather than
/// reimplementing their debounce and ignore policy, which would then drift.
pub mod scan;
mod session_history;
// Voice v1 (rubric 8): the speech-to-text seam, implemented over the runtime's
// OpenAI-compatible `/audio/transcriptions` client.
mod transcription;
mod workflow_exec;
mod workflows;

use std::path::PathBuf;
use std::sync::Arc;

use codypendent_daemon::{db, instance, recovery, server};
use codypendent_protocol::discovery::RuntimePaths;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::executor::RuntimeExecutor;

/// Install the daemon's tracing subscriber: a formatted layer to stderr, its
/// filter taken from the environment (`RUST_LOG`/`EnvFilter`) and defaulting to
/// `info`. Both the standalone `codypendentd` binary and `codypendent __daemon`
/// call this FIRST — the daemon's stderr is what the CLI redirects to
/// `daemon.log`, so this preserves the daemon's logging behavior byte-for-byte.
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

/// Run the Codypendent daemon to shutdown. `paths` must already be resolved and
/// have had `ensure_directories()` called (the caller — the `codypendentd`
/// binary or `codypendent __daemon` — does that, "as today"). Returns `Ok(())`
/// on graceful shutdown.
pub async fn run_daemon(paths: RuntimePaths) -> anyhow::Result<()> {
    let database_path = paths.data_dir.join("codypendent.db");

    // Claim single-instance exclusivity FIRST — before touching any shared
    // state. Recovery fails live runs, the relaunch spawns workers, and the
    // scan wipes/rebuilds the code graph; if a second daemon ran those against
    // a live daemon's database before discovering the socket was taken, it
    // would corrupt in-flight runs (contradictory terminal events, double
    // execution). Binding the socket is the mutex; losers exit here.
    let listener = server::acquire_socket(&paths).await?;

    let pool = db::open_database(&database_path).await?;
    let boot = instance::record_boot(&pool).await?;
    info!(
        instance = %boot.instance_id,
        boot_count = boot.boot_count,
        database = %database_path.display(),
        "codypendentd starting"
    );

    // Reconcile state a previous process may have left mid-flight — after the
    // exclusivity claim, before serving, so no client observes a half-recovered
    // run (STEP 1.14).
    let report = recovery::recover_on_startup(&pool, &paths).await?;
    info!(
        swept_tmp = report.swept_tmp,
        orphaned_leases = report.orphaned_leases.len(),
        reconciled_effects = report.reconciled_effects,
        failed_runs = report.failed_runs.len(),
        preserved_paused = report.preserved_paused.len(),
        expired_approvals = report.expired_approvals.len(),
        "startup recovery complete"
    );

    // Register the built-in tools into the governed registry (STEP 2.2 — Phase-1
    // tools "now registered with metadata"). Idempotent: `register_builtins`
    // upserts by identity and reuses ids, so this is safe on every boot and is
    // what gives retrieval and the Skill Studio a populated registry from the
    // first start. A failure here is logged but never blocks the daemon.
    match codypendent_knowledge::register_builtins(&pool).await {
        Ok(()) => info!("built-in tools registered in the knowledge registry"),
        Err(error) => warn!(%error, "failed to register built-in tools"),
    }

    // Warm semantic vectors in the background while clients connect.
    let embedder = retrieval::build_embedder(&paths);
    retrieval::spawn_index_maintenance(pool.clone(), embedder.clone());

    // Derive the process's fallback repository identity without warming the code
    // graph synchronously. Session attach and run launch schedule valid Git
    // checkouts in the background; startup must never walk an arbitrary daemon
    // working directory before it can serve clients.
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repository = scan::repository_id_for(&workdir);
    let integration_health = server::IntegrationHealth::default();

    // Register the operator's installed skill packages, so retrieval has
    // something to disclose beyond the built-ins. `register_package` previously
    // had no production caller at all: a package on disk reached the registry
    // only from a test. Both well-known roots are probed on every boot —
    // `<data_dir>/skills/` (what `codypendent skill add` installs into) and the
    // startup checkout's `.codypendent/skills/` (packages committed alongside
    // the code they serve) — and an absent root is a clean no-op. Idempotent
    // like `register_builtins` above: identity is reused, so re-scanning every
    // boot re-verifies each package's content hash rather than duplicating it.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    scan_installed_skills(
        &pool,
        &paths.data_dir,
        &workdir,
        home.as_deref(),
        repository,
    )
    .await;

    // Scan operator-installed hooks (<data_dir>/hooks/ and <workdir>/.codypendent/hooks/)
    // on startup into the hooks table (adoption 08).
    codypendent_daemon::hooks::scan_installed_hooks(&pool, &paths.data_dir, Some(&workdir)).await;

    // The executor owns the shared event fan-out + approval broker the server
    // binds to (`RunExecutor::collaborators`), and drives each accepted run
    // through the runtime agent loop. `workdir` is the daemon's startup root,
    // used both as the per-run worktree-binding fallback / node repository (T5,
    // the 4th `new` arg) and as the document-publish root (Phase 4 STEP 4.4 —
    // a document has no per-command repository field the way `StartRun` does,
    // so publication uses this same startup root, as the code-graph scan does).
    let mut executor =
        RuntimeExecutor::new(pool.clone(), paths.clone(), repository, workdir.clone())
            .with_repository_root(workdir.clone())
            // Rubric 9: the SAME embedder the maintenance job persists vectors
            // with (one shared content-hash cache), plus the `[retrieval]`
            // tuning each run's agent runtime gates its MCP advertisement by.
            .with_retrieval(embedder, retrieval::retrieval_settings(&paths));

    // Personal-mode GitHub (Phase 3 STEP 3.2): discover a token from `gh auth
    // token` or `GITHUB_TOKEN` and enable the `github.*` tools. Absent (the
    // common case in CI/headless), the tools stay disabled and the daemon runs
    // exactly as before. The token is a secret — only whether one was found is
    // ever logged, never its value.
    match codypendent_integrations::github::GitHubToken::discover().await {
        Ok(token) => {
            match codypendent_integrations::github::RestGitHubClient::new(
                "https://api.github.com",
                token,
            ) {
                Ok(client) => {
                    executor = executor.with_github(Arc::new(client));
                    info!("github personal-mode client enabled");
                }
                Err(error) => {
                    warn!(%error, "could not build the github client; github tools disabled");
                    integration_health.report(
                        "GitHub tools disabled: the personal-mode client could not be initialized; see daemon.log",
                    );
                }
            }
        }
        Err(_) => {
            info!("no github token found; github tools disabled");
            // Missing credentials are actionable only for a GitHub checkout;
            // local/non-GitHub repositories should not get a permanent warning
            // for an optional integration they never selected.
            if executor::resolve_github_repo(&workdir).await.is_some() {
                integration_health.report(
                    "GitHub tools unavailable for this checkout: run `gh auth login` or set GITHUB_TOKEN",
                );
            }
        }
    }

    // Web search resolves its key per call. A TUI key update therefore applies
    // instantly without restarting this daemon; a missing key fails the tool
    // locally before any network request and remains secret-safe.
    executor = executor.with_search(Arc::new(
        codypendent_integrations::search::ReloadingTavilyClient::new(
            paths.data_dir.clone(),
            "https://api.tavily.com",
        ),
    ));
    info!("tavily web search adapter enabled (credentials resolve per call)");

    // Voice input (voice v1, rubric 8): enabled only when `models.toml` declares
    // a `[transcription]` endpoint — an affirmative operator act. Absent (the
    // default), audio submissions are rejected `voice.transport-unavailable`
    // and text input is entirely unaffected. Whether a transcription may leave
    // the device is decided by the daemon against `routing.toml`'s existing
    // off-device ceiling, NOT by the transcriber.
    match crate::transcription::HostedTranscriber::arc_from_paths(&paths) {
        Some(transcriber) => executor = executor.with_transcriber(transcriber),
        None => info!("no [transcription] entry in models.toml; voice input disabled"),
    }

    // MCP client (PR B): load the operator-declared server list from
    // `<config_dir>/mcp.toml` (sibling to `policy.toml`) and hand the registry
    // to the executor, so single-agent runs AND workflow agent nodes are
    // offered the `mcp.<server>.<tool>` tools their warm servers provide.
    //
    // A MALFORMED file is deliberately NOT fatal: MCP is an optional feature,
    // so a typo in its config must not kill the daemon. The load error is
    // legible (it names the path and the reason) and is logged loudly here,
    // and it stays visible via `codypendent mcp list`; the daemon simply
    // continues with NO MCP servers. An absent file — or one declaring zero
    // servers — builds no registry at all.
    match codypendent_integrations::mcp::load_mcp_config(&paths.global_mcp_path()) {
        Ok(config) if !config.servers.is_empty() => {
            let server_count = config.servers.len();
            let registry = Arc::new(codypendent_integrations::mcp::McpRegistry::new(config));
            executor = executor.with_mcp(registry.clone());
            // Fire-and-forget pre-warm (the code-graph scan precedent): a
            // server that won't start is logged inside `warm_all`, never
            // fatal — its tools stay unoffered until a lazy spawn succeeds.
            let health = integration_health.clone();
            tokio::spawn(async move {
                for (server, _reason) in registry.warm_all_with_failures().await {
                    health.report(format!(
                        "MCP server `{server}` failed to start; check mcp.toml and daemon.log"
                    ));
                }
            });
            info!(
                servers = server_count,
                "mcp registry enabled; warming servers in the background"
            );
        }
        Ok(_) => info!("no mcp servers configured; mcp tools disabled"),
        Err(error) => {
            error!(%error, "malformed mcp config; continuing with NO mcp servers");
            integration_health.report(format!(
                "MCP tools disabled: {} is invalid; run `codypendent mcp list` for details",
                paths.global_mcp_path().display()
            ));
        }
    }

    let executor = Arc::new(executor);

    // Re-launch any run left `Queued` by a crash between `StartRun`'s commit and
    // its fire-and-forget spawn — recovery's live-state sweep does not cover
    // `Queued`, so those runs would otherwise be stuck with no worker.
    match executor.relaunch_queued_runs().await {
        Ok(0) => {}
        Ok(n) => info!(
            relaunched = n,
            "re-launched queued runs orphaned by a prior crash"
        ),
        Err(error) => warn!(%error, "could not re-launch queued runs at startup"),
    }

    // Resume any durable workflow run left non-terminal by a crash (Phase 5 STEP
    // 5.2): recompile each from its stored manifest and drive it onward from where
    // it stopped. Paused runs are left for an explicit resume. Fire-and-forget, so
    // a slow workflow never stalls the socket server below; a run that cannot be
    // recompiled is a no-op logged by its drive task.
    match executor.recover_workflows().await {
        Ok(0) => {}
        Ok(n) => info!(recovered = n, "resumed incomplete workflow runs"),
        Err(error) => warn!(%error, "could not resume workflow runs at startup"),
    }

    // Re-arm approval-gated document publication from its durable plan. Generic
    // recovery deliberately leaves these synthetic runs live; this step runs
    // after GitHub/MCP adapter assembly so every publication target has the same
    // capabilities it had before the restart.
    match executor.recover_document_publications().await {
        Ok(0) => {}
        Ok(n) => info!(recovered = n, "resumed document publication jobs"),
        Err(error) => warn!(%error, "could not resume document publication jobs"),
    }

    // Start the offline-first control-plane synchronizer on every daemon boot.
    // With no active pairing its database lookup returns an empty set and makes
    // zero network calls. Each active pairing is independently backoff-gated by
    // `SyncEngine`, so one offline endpoint cannot force all pairings onto a
    // fixed retry cadence. The worker receives an explicit shutdown signal and
    // is joined below rather than becoming an orphan task after the socket exits.
    let (control_plane_shutdown, control_plane_shutdown_rx) = tokio::sync::watch::channel(false);
    let control_plane_engine =
        codypendent_daemon::control_plane_sync::SyncEngine::new(pool.clone());
    match control_plane_credentials::rehydrate_control_plane_credentials(
        &pool,
        &paths.data_dir,
        &control_plane_engine,
    )
    .await
    {
        Ok(report) => info!(
            loaded_credentials = report.loaded,
            unavailable_credentials = report.unavailable,
            "control-plane credentials rehydrated"
        ),
        Err(error) => warn!(
            error_code = "control-plane.credentials-rehydration-failed",
            error_kind = %error,
            "control-plane credentials could not be rehydrated; sync remains fail-closed"
        ),
    }
    match codypendent_daemon::control_plane_sync::outbox::reconcile_authoritative_writes(&pool)
        .await
    {
        Ok(repaired) if repaired > 0 => info!(
            repaired_deltas = repaired,
            "reconciled authoritative local writes into the control-plane outbox"
        ),
        Ok(_) => {}
        Err(error) => warn!(
            error_code = "control-plane.outbox-reconciliation-failed",
            error_kind = %error,
            "startup outbox repair was incomplete; existing transactional deltas will still sync"
        ),
    }
    let control_plane_sync = control_plane_engine.start_background_loop(control_plane_shutdown_rx);
    info!("control-plane sync worker started");

    // The automation scheduler: the writer `migrations/0044_automation.sql` was
    // built for. Spawned HERE — after the three recovery passes, so a tick never
    // races startup recovery, and before the blocking socket server — with the
    // same fire-and-forget shape as `spawn_index_maintenance`. A due binding is
    // claimed atomically (lease + compare-and-swap + receipt), started as its
    // OWN owner, and its outcome recorded in `automation_attempts`. `None` means
    // a precondition for firing anything is missing; it is logged there and
    // never fatal.
    let automation =
        automation::start_automation_scheduler(&pool, &paths, boot.instance_id, &executor);
    if automation.is_some() {
        info!("automation scheduler started");
    }

    // Optionally open the GitHub webhook listener (Phase 3 STEP 3.3). It is
    // disabled unless `<data_dir>/webhooks.toml` sets `enabled = true`, and even
    // then binds loopback by default. Deliveries are verified, deduplicated by
    // their `X-GitHub-Delivery` GUID, and normalized. They start workflows only
    // when the operator ALSO sets `automation_dispatch = true`, which attaches
    // the automation sink below — opening the listener alone still records and
    // goes no further. The listener runs concurrently with the blocking socket
    // server below.
    maybe_start_webhook_listener(&paths, &pool, &integration_health, automation).await;

    let server_result = server::run_with_executor_on_and_health(
        listener,
        pool,
        paths,
        boot,
        Some(executor),
        integration_health,
    )
    .await;
    let _ = control_plane_shutdown.send(true);
    if let Err(error) = control_plane_sync.await {
        warn!(%error, "control-plane sync worker did not shut down cleanly");
    }
    server_result
}

/// Register every skill package installed under the two well-known roots into
/// the governed registry, so the retrieval funnel can disclose them.
///
/// Both roots are probed on every boot: `<data_dir>/skills/` (the operator's
/// global installs, where `codypendent skill add` copies a validated package)
/// and `<workdir>/.codypendent/skills/` (packages a checkout commits alongside
/// the code they serve). `repository` anchors any package declaring
/// repository tier, matching the identity the executor attributes a run's
/// context to.
///
/// Best-effort throughout, like the code-graph warm-up: a broken package is
/// logged with its reason and skipped, never fatal and never blocking its
/// siblings. Registration is idempotent — a re-scan reuses the existing
/// identity's id and only flags a content change — so this is safe every boot.
/// `home` is passed rather than read from the environment. Reading `$HOME` in
/// here made this function's behaviour depend on the machine it ran on — and
/// immediately broke two tests by scanning the developer's own hundred
/// installed plugin skills. The workspace already draws this line the same way
/// (`runtime::instructions::discover_instructions` takes `home` as a
/// parameter), and a test passing `None` is then hermetic by construction
/// rather than by hoping the operator has nothing installed.
async fn scan_installed_skills(
    pool: &sqlx::SqlitePool,
    data_dir: &std::path::Path,
    workdir: &std::path::Path,
    home: Option<&std::path::Path>,
    repository: codypendent_protocol::RepositoryId,
) {
    use codypendent_knowledge::{
        conventional_skill_roots, conventional_user_skill_roots, scan_skill_root, user_skills_root,
    };

    // This tool's own roots, plus the conventions other agent tooling already
    // uses (`.claude/skills`, `.agents/skills`) in the checkout and under HOME.
    // A repository that has written skills has written them for the job, not
    // for the brand, and an absent directory scans as empty rather than failing.
    let mut roots: Vec<(&'static str, std::path::PathBuf)> =
        vec![("user", user_skills_root(data_dir))];
    roots.extend(conventional_skill_roots(workdir));
    if let Some(home) = home {
        roots.extend(conventional_user_skill_roots(home));
    }
    let mut registered = 0usize;
    for (label, root) in roots {
        let outcome = scan_skill_root(pool, &root, repository).await;
        registered += outcome.registered.len();
        for (dir, reason) in outcome.failures {
            warn!(root = label, package = %dir.display(), %reason, "skill package not registered");
        }
    }
    if registered > 0 {
        info!(skills = registered, "installed skill packages registered");
    }
}

/// Start the webhook listener if `<data_dir>/webhooks.toml` enables it. Any
/// failure is logged and never blocks daemon startup — the webhook endpoint is
/// an optional, opt-in surface.
async fn maybe_start_webhook_listener(
    paths: &RuntimePaths,
    pool: &sqlx::SqlitePool,
    health: &server::IntegrationHealth,
    automation: Option<codypendent_daemon::automation_scheduler::AutomationScheduler>,
) {
    use codypendent_integrations::webhook::{config, SqliteDeliveryStore, WebhookIngestor};

    let config_path = paths.data_dir.join("webhooks.toml");
    let webhooks = match config::load(&config_path) {
        Ok(Some(webhooks)) if webhooks.enabled => webhooks,
        Ok(_) => return, // absent or disabled — the default
        Err(error) => {
            warn!(%error, "failed to load webhooks configuration; listener not started");
            health.report(format!(
                "Webhook listener disabled: {} is invalid; see daemon.log",
                config_path.display()
            ));
            return;
        }
    };

    // The secret never reaches a log line: only its presence is reported.
    let secret = webhooks
        .secret
        .as_ref()
        .map(|value| value.as_bytes().to_vec());
    let store = Arc::new(SqliteDeliveryStore::new(pool.clone()));

    // The sink is attached ONLY when the operator opted in AND a scheduler
    // exists. Without both, `None` keeps the original default-deny: an accepted
    // delivery is recorded and goes no further.
    let sink: Option<Arc<dyn codypendent_integrations::webhook::WebhookEventSink>> =
        match (webhooks.automation_dispatch, automation) {
            (true, Some(scheduler)) => Some(Arc::new(
                crate::automation::AutomationWebhookSink::new(scheduler),
            )),
            (true, None) => {
                warn!(
                    "webhooks.toml requests automation dispatch but no automation scheduler is \
                     running; deliveries will be recorded and go no further"
                );
                None
            }
            (false, _) => None,
        };
    let dispatching = sink.is_some();

    // `automation_endpoints` governs the per-endpoint signing key, body ceiling
    // and replay window. Its resolver has existed unused since the table landed;
    // attaching it is what makes those columns actually bind an inbound request.
    let ingestor =
        Arc::new(WebhookIngestor::new(store.clone(), secret, sink).with_endpoint_resolver(store));

    match codypendent_integrations::webhook::server::bind(&webhooks.listen_addr).await {
        Ok(listener) => {
            info!(
                addr = %webhooks.listen_addr,
                signed = webhooks.secret.is_some(),
                automation_dispatch = dispatching,
                "webhook listener enabled"
            );
            let health = health.clone();
            tokio::spawn(async move {
                if let Err(error) =
                    codypendent_integrations::webhook::server::serve(listener, ingestor).await
                {
                    warn!(%error, "webhook listener stopped");
                    health.report("Webhook listener stopped unexpectedly; see daemon.log");
                }
            });
        }
        Err(error) => {
            warn!(
                %error,
                addr = %webhooks.listen_addr,
                "could not bind the webhook listener"
            );
            health.report(format!(
                "Webhook listener could not bind {}; check webhooks.toml and daemon.log",
                webhooks.listen_addr
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use codypendent_knowledge::{
        repository_skills_root, user_skills_root, Registry, RegistryItemKind, Scope,
    };
    use codypendent_protocol::RepositoryId;

    /// Write a minimal valid skill package at `dir`.
    fn write_package(dir: &std::path::Path, id: &str, scope: &str) {
        std::fs::create_dir_all(dir).expect("package dir");
        std::fs::write(
            dir.join("skill.toml"),
            format!(
                "schema_version = 1\n\
                 id = \"{id}\"\n\
                 name = \"Test Skill\"\n\
                 version = \"0.1.0\"\n\
                 scope = \"{scope}\"\n\
                 status = \"active\"\n\
                 description = \"A test skill.\"\n\
                 \n\
                 [entrypoints]\n\
                 instructions = \"SKILL.md\"\n\
                 \n\
                 [trust]\n\
                 publisher = \"local-user\"\n\
                 signature_required = false\n"
            ),
        )
        .expect("write skill.toml");
        std::fs::write(dir.join("SKILL.md"), "# Test\n").expect("write SKILL.md");
    }

    /// The startup scan is the production ingestion path the 2026-08-11 review
    /// found missing entirely (`register_package` was reachable only from
    /// tests): both well-known roots must register, a broken package must not
    /// block its siblings, and a second boot must not duplicate a row.
    #[tokio::test]
    async fn startup_registers_both_skill_roots_idempotently() {
        let data = tempfile::tempdir().expect("data dir");
        let workdir = tempfile::tempdir().expect("workdir");
        let pool = codypendent_daemon::db::open_database(&data.path().join("codypendent.db"))
            .await
            .expect("open db");
        let repository = RepositoryId::new();

        write_package(
            &user_skills_root(data.path()).join("global"),
            "test.global",
            "user",
        );
        write_package(
            &repository_skills_root(workdir.path()).join("local"),
            "test.local",
            "repository",
        );
        // A package whose declared entrypoint is missing: skipped with a
        // reason, never fatal to its siblings or the boot.
        let broken = user_skills_root(data.path()).join("broken");
        write_package(&broken, "test.broken", "user");
        std::fs::remove_file(broken.join("SKILL.md")).expect("break the package");

        scan_installed_skills(&pool, data.path(), workdir.path(), None, repository).await;

        let skills: Vec<_> = Registry::new()
            .list(&pool)
            .await
            .expect("list registry")
            .into_iter()
            .filter(|item| item.kind == RegistryItemKind::Skill)
            .collect();
        assert_eq!(skills.len(), 2, "both good packages register: {skills:?}");
        let global = skills
            .iter()
            .find(|item| item.name == "test.global")
            .expect("the data-dir package registered");
        assert_eq!(global.scope, codypendent_knowledge::local_user_scope());
        let local = skills
            .iter()
            .find(|item| item.name == "test.local")
            .expect("the repo-local package registered");
        assert_eq!(
            local.scope,
            Scope::Repository(repository),
            "a repository-tier package anchors to the daemon's repository"
        );

        // A second boot re-verifies rather than duplicating.
        let ids: Vec<_> = skills.iter().map(|item| item.id).collect();
        scan_installed_skills(&pool, data.path(), workdir.path(), None, repository).await;
        let after: Vec<_> = Registry::new()
            .list(&pool)
            .await
            .expect("list registry")
            .into_iter()
            .filter(|item| item.kind == RegistryItemKind::Skill)
            .collect();
        assert_eq!(after.len(), 2, "a re-scan must not duplicate rows");
        for item in &after {
            assert!(ids.contains(&item.id), "identity survives a re-scan");
        }
    }

    /// Absent roots — the common case on a fresh install — are a clean no-op,
    /// so the scan can run unconditionally on every boot.
    #[tokio::test]
    async fn startup_with_no_installed_skills_is_a_no_op() {
        let data = tempfile::tempdir().expect("data dir");
        let workdir = tempfile::tempdir().expect("workdir");
        let pool = codypendent_daemon::db::open_database(&data.path().join("codypendent.db"))
            .await
            .expect("open db");

        scan_installed_skills(
            &pool,
            data.path(),
            workdir.path(),
            None,
            RepositoryId::new(),
        )
        .await;

        let skills = Registry::new()
            .list(&pool)
            .await
            .expect("list registry")
            .into_iter()
            .filter(|item| item.kind == RegistryItemKind::Skill)
            .count();
        assert_eq!(skills, 0);
    }
}
