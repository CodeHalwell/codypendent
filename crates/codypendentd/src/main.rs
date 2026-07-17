//! `codypendentd` — the persistent Codypendent daemon (assembly binary).
//!
//! This is the composition root. It depends on BOTH `codypendent-daemon` (the
//! server + persistence) and `codypendent-runtime` (the agent loop) — which the
//! daemon crate itself cannot, because the runtime depends on the daemon (a
//! cycle). It performs the daemon startup exactly as the old lib-side `main.rs`
//! did (tracing, paths, db, boot, recovery), then constructs a [`RunExecutor`]
//! that drives the runtime agent loop and injects it into the server.
//!
//! [`RunExecutor`]: codypendent_daemon::executor::RunExecutor

mod executor;
mod scan;

use std::path::PathBuf;
use std::sync::Arc;

use codypendent_daemon::{db, instance, recovery, server};
use codypendent_protocol::discovery::RuntimePaths;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::executor::RuntimeExecutor;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let paths = RuntimePaths::resolve()?;
    paths.ensure_directories()?;
    let database_path = paths.data_dir.join("codypendent.db");

    let pool = db::open_database(&database_path).await?;
    let boot = instance::record_boot(&pool).await?;
    info!(
        instance = %boot.instance_id,
        boot_count = boot.boot_count,
        database = %database_path.display(),
        "codypendentd starting"
    );

    // Reconcile state a previous process may have left mid-flight — before the
    // socket opens, so no client observes a half-recovered run (STEP 1.14).
    let report = recovery::recover_on_startup(&pool, &paths).await?;
    info!(
        swept_tmp = report.swept_tmp,
        orphaned_leases = report.orphaned_leases.len(),
        reconciled_effects = report.reconciled_effects,
        failed_runs = report.failed_runs.len(),
        resurfaced_approvals = report.resurfaced_approvals.len(),
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

    // Derive the process's repository identity from the working directory's
    // canonical path, so the SAME checkout maps to the SAME id across restarts —
    // a random id per boot would orphan the previous run's code graph and
    // repository-scoped memories and bloat the database. Then warm the code graph
    // so the repository map a run's context opens with is real. The same id is
    // handed to the executor, so runs, their context maps, and their curated
    // memories all share one stable repository. The scan is bounded and
    // failure-tolerant — a parse error on one file must never abort startup.
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repository = scan::repository_id_for(&workdir);
    scan::scan_repository(&pool, repository, &workdir).await;

    // The executor owns the shared event fan-out + approval broker the server
    // binds to (`RunExecutor::collaborators`), and drives each accepted run
    // through the runtime agent loop.
    let executor = Arc::new(RuntimeExecutor::new(
        pool.clone(),
        paths.clone(),
        repository,
    ));

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

    // Optionally open the GitHub webhook listener (Phase 3 STEP 3.3). It is
    // disabled unless `<data_dir>/webhooks.toml` sets `enabled = true`, and even
    // then binds loopback by default. Deliveries are verified, deduplicated by
    // their `X-GitHub-Delivery` GUID, and normalized; they never trigger
    // workflows here (that requires explicit policy, wired in a later phase). The
    // listener runs concurrently with the blocking socket server below.
    maybe_start_webhook_listener(&paths, &pool).await;

    server::run_with_executor(pool, paths, boot, Some(executor)).await
}

/// Start the webhook listener if `<data_dir>/webhooks.toml` enables it. Any
/// failure is logged and never blocks daemon startup — the webhook endpoint is
/// an optional, opt-in surface.
async fn maybe_start_webhook_listener(paths: &RuntimePaths, pool: &sqlx::SqlitePool) {
    use codypendent_integrations::webhook::{config, SqliteDeliveryStore, WebhookIngestor};

    let config_path = paths.data_dir.join("webhooks.toml");
    let webhooks = match config::load(&config_path) {
        Ok(Some(webhooks)) if webhooks.enabled => webhooks,
        Ok(_) => return, // absent or disabled — the default
        Err(error) => {
            warn!(%error, "failed to load webhooks configuration; listener not started");
            return;
        }
    };

    // The secret never reaches a log line: only its presence is reported.
    let secret = webhooks
        .secret
        .as_ref()
        .map(|value| value.as_bytes().to_vec());
    let store = Arc::new(SqliteDeliveryStore::new(pool.clone()));
    // Deliveries never trigger workflows in this phase (default-deny policy).
    let ingestor = Arc::new(WebhookIngestor::new(store, secret, false));

    match codypendent_integrations::webhook::server::bind(&webhooks.listen_addr).await {
        Ok(listener) => {
            info!(
                addr = %webhooks.listen_addr,
                signed = webhooks.secret.is_some(),
                "webhook listener enabled"
            );
            tokio::spawn(async move {
                if let Err(error) =
                    codypendent_integrations::webhook::server::serve(listener, ingestor).await
                {
                    warn!(%error, "webhook listener stopped");
                }
            });
        }
        Err(error) => warn!(
            %error,
            addr = %webhooks.listen_addr,
            "could not bind the webhook listener"
        ),
    }
}
