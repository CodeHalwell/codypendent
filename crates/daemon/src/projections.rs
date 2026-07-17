//! Run/session projections backing the command pipeline (STEP 1.3).
//!
//! Projections are *derived* state: folding the same events must always produce
//! the same projection (invariant 5, property-tested in [`crate::commands`]).
//! The `runs` table **is** the run projection — the write path
//! ([`crate::commands`]) keeps it in step with the ledger inside the same
//! transaction that appends the run's events, so a committed run row and its
//! `RunStarted`/`RunStateChanged` events never disagree.
//!
//! Every write helper takes `impl sqlx::SqliteExecutor<'_>` (never a bare pool)
//! so it composes *inside* the command transaction; the read helpers take the
//! pool because they run standalone (attach-time catch-up, validation).

use std::str::FromStr;

use codypendent_protocol::ide::IdeContextUpdate;
use codypendent_protocol::{AgentMode, RunId, RunState, SessionId, SessionProjection};
use sqlx::SqlitePool;

/// The DB string for a [`RunState`] (the `runs.state` column). PascalCase to
/// match the variant names, consistent with the seed rows the sibling modules
/// write. Unknown / future variants collapse to `"Unknown"`.
pub fn run_state_to_db(state: RunState) -> &'static str {
    match state {
        RunState::Queued => "Queued",
        RunState::Preparing => "Preparing",
        RunState::Running => "Running",
        RunState::WaitingForApproval => "WaitingForApproval",
        RunState::WaitingForUserInput => "WaitingForUserInput",
        RunState::Paused => "Paused",
        RunState::Recovering => "Recovering",
        RunState::Completed => "Completed",
        RunState::Failed => "Failed",
        RunState::Cancelled => "Cancelled",
        // `Unknown` and any future non_exhaustive variant.
        _ => "Unknown",
    }
}

/// Parse a `runs.state` string back into a [`RunState`]; an unrecognized string
/// yields [`RunState::Unknown`] rather than erroring (forward-compatibility).
pub fn run_state_from_db(s: &str) -> RunState {
    match s {
        "Queued" => RunState::Queued,
        "Preparing" => RunState::Preparing,
        "Running" => RunState::Running,
        "WaitingForApproval" => RunState::WaitingForApproval,
        "WaitingForUserInput" => RunState::WaitingForUserInput,
        "Paused" => RunState::Paused,
        "Recovering" => RunState::Recovering,
        "Completed" => RunState::Completed,
        "Failed" => RunState::Failed,
        "Cancelled" => RunState::Cancelled,
        _ => RunState::Unknown,
    }
}

/// The DB string for an [`AgentMode`] (the `runs.mode` column).
pub fn agent_mode_to_db(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Ask => "Ask",
        AgentMode::Explore => "Explore",
        AgentMode::Plan => "Plan",
        AgentMode::Build => "Build",
        AgentMode::Review => "Review",
        _ => "Unknown",
    }
}

/// Parse a `runs.mode` string back into an [`AgentMode`]; an unrecognized string
/// falls back to [`AgentMode::Build`] (the default preset).
pub fn agent_mode_from_db(s: &str) -> AgentMode {
    match s {
        "Ask" => AgentMode::Ask,
        "Explore" => AgentMode::Explore,
        "Plan" => AgentMode::Plan,
        "Review" => AgentMode::Review,
        _ => AgentMode::Build,
    }
}

/// Whether a run is in a terminal state (no further work). A terminal run is
/// excluded from a session's `active_runs`.
pub fn is_terminal(state: RunState) -> bool {
    matches!(
        state,
        RunState::Completed | RunState::Failed | RunState::Cancelled
    )
}

/// Insert a fresh run row in state [`RunState::Queued`]. STEP 1.3 only *creates*
/// the run; the agent loop that executes it is STEP 1.10. `model_policy` and
/// `budget_json` are the run's resolved policy/budget (the command carries
/// neither in Phase 1, so the write path supplies defaults).
pub async fn insert_run(
    exec: impl sqlx::SqliteExecutor<'_>,
    run_id: RunId,
    session_id: SessionId,
    objective: &str,
    mode: AgentMode,
    model_policy: &str,
    budget_json: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO runs (id, session_id, objective, state, mode, model_policy, budget_json) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(run_id.to_string())
    .bind(session_id.to_string())
    .bind(objective)
    .bind(run_state_to_db(RunState::Queued))
    .bind(agent_mode_to_db(mode))
    .bind(model_policy)
    .bind(budget_json)
    .execute(exec)
    .await?;
    Ok(())
}

/// Move a run to a new [`RunState`]. A no-op update (run absent) is not an error
/// here; the write path validates run existence before calling this.
pub async fn set_run_state(
    exec: impl sqlx::SqliteExecutor<'_>,
    run_id: RunId,
    state: RunState,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE runs SET state = ? WHERE id = ?")
        .bind(run_state_to_db(state))
        .bind(run_id.to_string())
        .execute(exec)
        .await?;
    Ok(())
}

/// The current [`RunState`] of a run, or `None` if there is no such run.
pub async fn load_run_state(pool: &SqlitePool, run_id: RunId) -> anyhow::Result<Option<RunState>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT state FROM runs WHERE id = ?")
        .bind(run_id.to_string())
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(state,)| run_state_from_db(&state)))
}

/// The session a run belongs to, or `None` if there is no such run. The write
/// path uses this to resolve the ledger a run-scoped command (`CancelRun`,
/// `QueueSteering`, ...) must append to.
pub async fn run_session(pool: &SqlitePool, run_id: RunId) -> anyhow::Result<Option<SessionId>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT session_id FROM runs WHERE id = ?")
        .bind(run_id.to_string())
        .fetch_optional(pool)
        .await?;
    row.map(|(session,)| SessionId::from_str(&session))
        .transpose()
        .map_err(Into::into)
}

/// Build the compact [`SessionProjection`] the server sends in a `Catchup`
/// snapshot: session identity, `last_sequence` (`MAX(events.sequence)`), and the
/// ids of runs not in a terminal state. Ordered deterministically (`runs.id`
/// ascending) so the snapshot folds the same as the event ledger.
pub async fn session_projection(
    pool: &SqlitePool,
    session_id: SessionId,
) -> anyhow::Result<SessionProjection> {
    let meta: Option<(String, String)> =
        sqlx::query_as("SELECT title, state FROM sessions WHERE id = ?")
            .bind(session_id.to_string())
            .fetch_optional(pool)
            .await?;
    let (title, closed) = match meta {
        Some((title, state)) => (title, state == "closed"),
        None => (String::new(), false),
    };

    let (last_sequence,): (i64,) =
        sqlx::query_as("SELECT COALESCE(MAX(sequence), 0) FROM events WHERE session_id = ?")
            .bind(session_id.to_string())
            .fetch_one(pool)
            .await?;

    let runs: Vec<(String, String)> =
        sqlx::query_as("SELECT id, state FROM runs WHERE session_id = ? ORDER BY id ASC")
            .bind(session_id.to_string())
            .fetch_all(pool)
            .await?;
    let mut active_runs = Vec::new();
    for (id, state) in runs {
        if !is_terminal(run_state_from_db(&state)) {
            active_runs.push(RunId::from_str(&id)?);
        }
    }

    Ok(SessionProjection {
        session_id,
        title,
        last_sequence: u64::try_from(last_sequence)?,
        active_runs,
        closed,
    })
}

/// Upsert the latest IDE context for a session (Phase 3 STEP 3.4). Latest-wins:
/// a session has at most one row, replaced on every `UpdateIdeContext`. Stored
/// outside the event ledger — IDE context is high-frequency, ephemeral, derived
/// state, not history.
pub async fn upsert_ide_context(
    pool: &SqlitePool,
    session_id: SessionId,
    update: &IdeContextUpdate,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<()> {
    let json = serde_json::to_string(update)?;
    sqlx::query(
        "INSERT INTO ide_context (session_id, update_json, updated_at) VALUES (?, ?, ?) \
         ON CONFLICT(session_id) DO UPDATE SET update_json = excluded.update_json, \
         updated_at = excluded.updated_at",
    )
    .bind(session_id.to_string())
    .bind(json)
    .bind(now.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

/// Load the latest IDE context for a session, if any has been pushed.
pub async fn load_ide_context(
    pool: &SqlitePool,
    session_id: SessionId,
) -> anyhow::Result<Option<IdeContextUpdate>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT update_json FROM ide_context WHERE session_id = ?")
            .bind(session_id.to_string())
            .fetch_optional(pool)
            .await?;
    match row {
        Some((json,)) => Ok(Some(serde_json::from_str(&json)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod ide_context_tests {
    use super::*;
    use codypendent_protocol::ide::{DirtyBufferDigest, IdeContextUpdate};

    #[tokio::test]
    async fn ide_context_is_latest_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = crate::db::open_database(&tmp.path().join("db.sqlite"))
            .await
            .unwrap();
        let session = SessionId::new();
        // A session must exist for the FK.
        crate::ledger::create_session(&pool, session, "ide")
            .await
            .unwrap();

        assert!(load_ide_context(&pool, session).await.unwrap().is_none());

        let first = IdeContextUpdate {
            active_file: Some("a.rs".to_string()),
            ..Default::default()
        };
        upsert_ide_context(&pool, session, &first, chrono::Utc::now())
            .await
            .unwrap();

        let second = IdeContextUpdate {
            active_file: Some("b.rs".to_string()),
            dirty_buffers: vec![DirtyBufferDigest {
                path: "b.rs".to_string(),
                sha256: "abc".to_string(),
                byte_length: 5,
            }],
            ..Default::default()
        };
        upsert_ide_context(&pool, session, &second, chrono::Utc::now())
            .await
            .unwrap();

        // Latest wins: exactly one row, holding the second update.
        let loaded = load_ide_context(&pool, session).await.unwrap().unwrap();
        assert_eq!(loaded.active_file.as_deref(), Some("b.rs"));
        assert_eq!(loaded.dirty_buffers.len(), 1);
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM ide_context")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}
