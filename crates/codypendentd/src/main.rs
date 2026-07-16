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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use codypendent_daemon::{db, instance, recovery, server};
use codypendent_knowledge::{codegraph, GitRevision};
use codypendent_protocol::discovery::RuntimePaths;
use codypendent_protocol::RepositoryId;
use sqlx::SqlitePool;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::executor::RuntimeExecutor;

/// The upper bound on files folded into the code graph at startup. The scan is a
/// best-effort warm-up so the repository map is non-empty from the first run; it
/// is capped so a large tree never delays the socket opening.
const SCAN_FILE_CAP: usize = 60;

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
    let repository =
        codypendent_knowledge::stable_repository_id(&workdir.canonicalize().unwrap_or(workdir));
    scan_repository(&pool, repository).await;

    // The executor owns the shared event fan-out + approval broker the server
    // binds to (`RunExecutor::collaborators`), and drives each accepted run
    // through the runtime agent loop.
    let executor = Arc::new(RuntimeExecutor::new(
        pool.clone(),
        paths.clone(),
        repository,
    ));

    server::run_with_executor(pool, paths, boot, Some(executor)).await
}

/// Fold up to [`SCAN_FILE_CAP`] of the working directory's `*.rs` files into the
/// code graph for `repository`, so the repository map is populated from the first
/// run. Best-effort: a per-file parse/read failure is logged and skipped, never
/// propagated — a warm-up must not block or fail startup.
async fn scan_repository(pool: &SqlitePool, repository: RepositoryId) {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let revision = head_revision(&root);

    // Retire the repository's prior graph before a full re-scan so symbols
    // removed since the last boot (with the now-stable repository id) do not
    // linger in the graph or the repository map. The code graph is derived and
    // regenerable, so wiping + rebuilding is safe.
    if let Err(error) = codegraph::clear_repository(pool, repository).await {
        warn!(%error, "could not clear the prior code graph before re-scan");
    }

    let files = collect_rust_sources(&root, SCAN_FILE_CAP);
    let mut scanned = 0usize;
    let mut nodes = 0usize;
    for (relative, source) in files {
        match codegraph::upsert_file_graph(pool, repository, &revision, &relative, &source).await {
            Ok(delta) => {
                scanned += 1;
                nodes += delta.nodes.len();
            }
            Err(error) => {
                warn!(path = %relative, %error, "skipped a file that would not fold into the code graph");
            }
        }
    }
    info!(
        repository = %repository,
        revision = %revision.0,
        files = scanned,
        nodes,
        "code-graph startup scan complete"
    );
}

/// The working tree's `HEAD` commit as a [`GitRevision`], or the `"workdir"`
/// placeholder when Git is unavailable or `root` is not a repository. Shelling out
/// keeps startup free of a Git library dependency.
fn head_revision(root: &Path) -> GitRevision {
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    GitRevision(head.unwrap_or_else(|| "workdir".to_string()))
}

/// Collect up to `cap` `(repo-relative-path, source)` pairs for the `*.rs` files
/// under `root`, skipping `target/` and hidden (dot-prefixed) directories. A plain
/// iterative walk (no `walkdir` dependency); unreadable entries are skipped.
fn collect_rust_sources(root: &Path, cap: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= cap {
            break;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Skip hidden dirs/files and the build output tree.
            if name.starts_with('.') || name == "target" {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                if out.len() >= cap {
                    break;
                }
                let Ok(source) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                out.push((relative, source));
            }
        }
    }
    out
}
