//! Opening a migrated SQLite pool for promotion-store tests.
//!
//! In production the daemon owns the pool and this crate's [`crate::store`]
//! operates on it; this helper exists for this crate's own tests (mirroring
//! `codypendent-workflow`'s `db::open` and the knowledge crate's own `db`
//! helper), so `codypendent-eval` never has to depend on `codypendent-daemon`.
//! The migrations directory is shared at the workspace root, so a pool opened
//! here has the full schema.

use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};

/// Open (creating if absent) the metadata database at `path`, in WAL mode, with
/// foreign keys on, and run every migration through the head.
pub async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    sqlx::migrate!("../../migrations").run(&pool).await?;
    Ok(pool)
}
