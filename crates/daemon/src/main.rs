//! `codypendentd` — the persistent Codypendent daemon.

use codypendent_daemon::{db, instance, server};
use codypendent_protocol::discovery::RuntimePaths;
use tracing::info;
use tracing_subscriber::EnvFilter;

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

    server::run(pool, paths, boot).await
}
