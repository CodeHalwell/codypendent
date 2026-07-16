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

    // The executor owns the shared event fan-out + approval broker the server
    // binds to (`RunExecutor::collaborators`), and drives each accepted run
    // through the runtime agent loop.
    let executor = Arc::new(RuntimeExecutor::new(pool.clone(), paths.clone()));

    server::run_with_executor(pool, paths, boot, Some(executor)).await
}
