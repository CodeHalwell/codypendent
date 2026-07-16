//! Opening a migrated SQLite pool for the knowledge fabric.
//!
//! In production the daemon owns the pool and this crate operates on it; this
//! helper exists for the `index rebuild` CLI path and for this crate's own
//! tests, so knowledge never has to depend on `codypendent-daemon` (which
//! depends on knowledge — the same inversion the runtime uses).

use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};

/// Open (creating if absent) the metadata database at `path`, in WAL mode, and
/// run every migration through the head. Mirrors the daemon's `open_database`;
/// the migrations directory is shared at the workspace root.
pub async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    sqlx::migrate!("../../migrations").run(&pool).await?;
    Ok(pool)
}
