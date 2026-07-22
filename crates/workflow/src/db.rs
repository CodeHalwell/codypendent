//! Opening a migrated SQLite pool for workflow storage.
//!
//! In production the daemon owns the pool and this crate operates on it; this
//! helper exists for this crate's own tests (and any standalone workflow tool),
//! so `codypendent-workflow` never depends on `codypendent-daemon`. Mirrors the
//! knowledge crate's `db::open`: the migrations directory is shared at the
//! workspace root, so a pool opened here has the full schema.

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
        // Foreign keys on, so workflow_nodes/checkpoints → workflow_runs integrity
        // is enforced here exactly as in the daemon's pool.
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    sqlx::migrate!("../../migrations").run(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Migration 0013 (T8) applies on a FRESH db and on an EXISTING one (re-open):
    /// the append-only `workflow_nodes.error` column resolves, and re-running the
    /// migrator against a db already at head is an idempotent no-op.
    #[tokio::test]
    async fn migration_0013_adds_the_node_error_column_on_fresh_and_existing_dbs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wf.db");

        // Fresh db: every migration through 0013 applies. Naming the new `error`
        // column in a query proves 0013 ran (a missing column is a SQL error).
        let pool = open(&path).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(error) FROM workflow_nodes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        pool.close().await;

        // Existing db (the same file, now at head): the migrator sees 0013 already
        // recorded and applies nothing new — append-only, idempotent.
        let pool = open(&path).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(error) FROM workflow_nodes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}
