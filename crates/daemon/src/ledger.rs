//! The append-only event ledger.
//!
//! Phase 0 provides create/append/load/count. Later phases add commands with
//! idempotency keys, the crash-consistency write path, projections, and
//! subscriptions — the storage shape here is already the durable ordering
//! authority they build on.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use codypendent_protocol::{
    Actor, EventBody, RunDisposition, RunId, RunState, SessionEvent, SessionId,
};
use sqlx::{SqliteConnection, SqlitePool};

/// Insert a session row in state `open`.
pub async fn create_session(
    pool: &SqlitePool,
    session_id: SessionId,
    title: &str,
) -> anyhow::Result<()> {
    let now = Utc::now().to_rfc3339();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO sessions (id, title, state, created_at, updated_at, revision) \
         VALUES (?, ?, 'open', ?, ?, 0)",
    )
    .bind(session_id.to_string())
    .bind(title)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    crate::session_library::index_title_source(&mut *tx, session_id, title, &now).await?;
    tx.commit().await?;
    Ok(())
}

/// Append one event. The caller supplies `event.sequence`; the UNIQUE primary
/// key (session_id, sequence) makes duplicate appends fail loudly instead of
/// silently forking history.
pub async fn append_event(
    pool: &SqlitePool,
    session_id: SessionId,
    event: &SessionEvent,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO events \
         (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 1)",
    )
    .bind(session_id.to_string())
    .bind(i64::try_from(event.sequence)?)
    .bind(event.occurred_at.to_rfc3339())
    .bind(serde_json::to_string(&event.actor)?)
    .bind(serde_json::to_string(&event.body)?)
    .bind(event.causation_id.map(|id| id.to_string()))
    .bind(event.correlation_id.map(|id| id.to_string()))
    .execute(&mut *tx)
    .await?;
    crate::session_library::index_event_sources(
        &mut tx,
        session_id,
        i64::try_from(event.sequence)?,
        &event.body,
        &event.occurred_at.to_rfc3339(),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Row shape of the `events` table used by `load_events`:
/// (sequence, occurred_at, actor, body, causation_id, correlation_id).
type EventRow = (i64, String, String, String, Option<String>, Option<String>);

/// Load every event for a session in sequence order.
pub async fn load_events(
    pool: &SqlitePool,
    session_id: SessionId,
) -> anyhow::Result<Vec<SessionEvent>> {
    let rows: Vec<EventRow> = sqlx::query_as(
        "SELECT sequence, occurred_at, actor, body, causation_id, correlation_id \
         FROM events WHERE session_id = ? ORDER BY sequence ASC",
    )
    .bind(session_id.to_string())
    .fetch_all(pool)
    .await?;
    rows_to_events(rows)
}

/// Load the events with `after < sequence <= through`, in sequence order. The
/// window is filtered in SQL — the `(session_id, sequence)` primary key serves
/// it — so an attach catch-up reads only the gap: a client one event behind on
/// a 100k-event session must not pay a full-history read per reconnect.
pub async fn load_events_between(
    pool: &SqlitePool,
    session_id: SessionId,
    after: u64,
    through: u64,
) -> anyhow::Result<Vec<SessionEvent>> {
    let rows: Vec<EventRow> = sqlx::query_as(
        "SELECT sequence, occurred_at, actor, body, causation_id, correlation_id \
         FROM events WHERE session_id = ? AND sequence > ? AND sequence <= ? \
         ORDER BY sequence ASC",
    )
    .bind(session_id.to_string())
    .bind(i64::try_from(after).unwrap_or(i64::MAX))
    .bind(i64::try_from(through).unwrap_or(i64::MAX))
    .fetch_all(pool)
    .await?;
    rows_to_events(rows)
}

/// Whether `session_id` exists in the sessions table. The attach path uses
/// this to reject a session id the daemon has never seen — an empty catch-up
/// must mean "empty session", never "typo'd id".
pub async fn session_exists(pool: &SqlitePool, session_id: SessionId) -> anyhow::Result<bool> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM sessions WHERE id = ?")
        .bind(session_id.to_string())
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// The principal that created `session_id` (migration 0031), or `None` when the
/// daemon has never seen that session.
///
/// The inner `Option` is the recorded `owner_uid`: `None` there means a row
/// written before 0031, which the caller resolves to the daemon's own uid — a
/// pre-0031 row can only have been created by the single local user the daemon
/// served at the time. Existence and ownership come from ONE query so an
/// ownership check cannot be told apart from an existence check by timing.
pub async fn session_owner_uid(
    pool: &SqlitePool,
    session_id: SessionId,
) -> anyhow::Result<Option<Option<u32>>> {
    let row: Option<(Option<i64>,)> = sqlx::query_as("SELECT owner_uid FROM sessions WHERE id = ?")
        .bind(session_id.to_string())
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(owner,)| owner.and_then(|uid| u32::try_from(uid).ok())))
}

/// Decode raw event rows into [`SessionEvent`]s.
fn rows_to_events(rows: Vec<EventRow>) -> anyhow::Result<Vec<SessionEvent>> {
    let mut events = Vec::with_capacity(rows.len());
    for (sequence, occurred_at, actor, body, causation_id, correlation_id) in rows {
        events.push(SessionEvent {
            sequence: u64::try_from(sequence)?,
            occurred_at: DateTime::parse_from_rfc3339(&occurred_at)?.with_timezone(&Utc),
            causation_id: causation_id.map(|id| id.parse()).transpose()?,
            correlation_id: correlation_id.map(|id| id.parse()).transpose()?,
            actor: serde_json::from_str(&actor)?,
            body: serde_json::from_str(&body)?,
        });
    }
    Ok(events)
}

/// Load only the single most recent event for a session (the highest sequence),
/// or `None` if it has none. Cheaper than [`load_events`] when the caller needs
/// just the latest event rather than the whole history.
pub async fn load_last_event(
    pool: &SqlitePool,
    session_id: SessionId,
) -> anyhow::Result<Option<SessionEvent>> {
    let row: Option<EventRow> = sqlx::query_as(
        "SELECT sequence, occurred_at, actor, body, causation_id, correlation_id \
         FROM events WHERE session_id = ? ORDER BY sequence DESC LIMIT 1",
    )
    .bind(session_id.to_string())
    .fetch_optional(pool)
    .await?;

    match row {
        None => Ok(None),
        Some((sequence, occurred_at, actor, body, causation_id, correlation_id)) => {
            Ok(Some(SessionEvent {
                sequence: u64::try_from(sequence)?,
                occurred_at: DateTime::parse_from_rfc3339(&occurred_at)?.with_timezone(&Utc),
                causation_id: causation_id.map(|id| id.parse()).transpose()?,
                correlation_id: correlation_id.map(|id| id.parse()).transpose()?,
                actor: serde_json::from_str(&actor)?,
                body: serde_json::from_str(&body)?,
            }))
        }
    }
}

/// Atomically claim the next sequence for `session_id` and append an event,
/// returning the persisted [`SessionEvent`].
///
/// The sequence is computed *inside* the INSERT (`COALESCE(MAX(sequence),0)+1`
/// via `INSERT … SELECT … RETURNING`), so the read and the write happen under a
/// single write lock — concurrent appenders on the same session (a live run and
/// a client command such as steering, cancel, or approval resolution) can never
/// claim the same number and trip the `(session_id, sequence)` uniqueness
/// constraint. Prefer this over a separate [`next_sequence`] + [`append_event`],
/// which race. Actor/body are `System`-friendly: no causation/correlation ids.
pub async fn append_next_event(
    pool: &SqlitePool,
    session_id: SessionId,
    actor: &Actor,
    body: &EventBody,
    occurred_at: DateTime<Utc>,
) -> anyhow::Result<SessionEvent> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO events \
         (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
         SELECT ?, (SELECT COALESCE(MAX(sequence), 0) + 1 FROM events WHERE session_id = ?), \
                ?, ?, ?, NULL, NULL, 1 \
         WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ? AND state != 'closed') \
         RETURNING sequence",
    )
    .bind(session_id.to_string())
    .bind(session_id.to_string())
    .bind(occurred_at.to_rfc3339())
    .bind(serde_json::to_string(actor)?)
    .bind(serde_json::to_string(body)?)
    .bind(session_id.to_string())
    .fetch_optional(&mut *tx)
    .await?;
    let (sequence,) = row.ok_or_else(|| anyhow::anyhow!("session is closed"))?;
    crate::session_library::index_event_sources(
        &mut tx,
        session_id,
        sequence,
        body,
        &occurred_at.to_rfc3339(),
    )
    .await?;
    tx.commit().await?;
    Ok(SessionEvent {
        sequence: u64::try_from(sequence)?,
        occurred_at,
        causation_id: None,
        correlation_id: None,
        actor: actor.clone(),
        body: body.clone(),
    })
}

/// Atomically append a `RunStateChanged` event and update the run projection.
/// The conditional projection update also makes terminal states authoritative:
/// a late runtime completion can no longer overwrite a concurrently accepted
/// cancellation.
pub async fn append_run_state_changed(
    pool: &SqlitePool,
    session_id: SessionId,
    actor: &Actor,
    run_id: RunId,
    state: RunState,
    occurred_at: DateTime<Utc>,
) -> anyhow::Result<SessionEvent> {
    let legal_from: &[RunState] = match state {
        RunState::Preparing => &[RunState::Queued],
        RunState::Running => &[
            RunState::Preparing,
            RunState::WaitingForApproval,
            RunState::WaitingForUserInput,
            RunState::Paused,
        ],
        RunState::WaitingForApproval => &[RunState::Running],
        RunState::WaitingForUserInput => &[RunState::Running],
        RunState::Paused => &[RunState::Running],
        RunState::Recovering => &[
            RunState::Preparing,
            RunState::Running,
            RunState::WaitingForApproval,
            RunState::WaitingForUserInput,
            RunState::Paused,
        ],
        RunState::Completed | RunState::Failed | RunState::Cancelled => &[
            RunState::Queued,
            RunState::Preparing,
            RunState::Running,
            RunState::WaitingForApproval,
            RunState::WaitingForUserInput,
            RunState::Paused,
            RunState::Recovering,
        ],
        RunState::Unknown => &[],
        _ => &[],
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let affected =
        crate::projections::set_run_state_if_legal(&mut *tx, run_id, legal_from, state).await?;
    if affected != 1 {
        let current = crate::projections::load_run_state(&mut *tx, run_id).await?;
        return Err(anyhow::anyhow!(
            "refused stale runtime transition for {run_id}: {current:?} -> {state:?}"
        ));
    }
    // Outcome 20: `started_at`/`ended_at` (migration 0002) were declared columns
    // nobody wrote — every completed run read back `None` for both, so no
    // reader could compute latency. This transition is the ONE place every
    // run's state change already passes through under `BEGIN IMMEDIATE`, so it
    // is also the one place that can stamp both timestamps without a second,
    // unsynchronized write path drifting from the transition it describes.
    // `started_at` is set only the FIRST time a run reaches `Running`
    // (`COALESCE` preserves the original moment across a pause/resume cycle,
    // which legally revisits `Running`); `ended_at` is set once, on whichever
    // terminal state the run actually reaches (`legal_from` above admits each
    // run into exactly one, ever).
    match state {
        RunState::Running => {
            sqlx::query("UPDATE runs SET started_at = COALESCE(started_at, ?) WHERE id = ?")
                .bind(occurred_at.to_rfc3339())
                .bind(run_id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        RunState::Completed | RunState::Failed | RunState::Cancelled => {
            sqlx::query("UPDATE runs SET ended_at = ? WHERE id = ?")
                .bind(occurred_at.to_rfc3339())
                .bind(run_id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        _ => {}
    }

    let body = EventBody::RunStateChanged { run_id, state };
    let (sequence,): (i64,) = sqlx::query_as(
        "INSERT INTO events \
         (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
         SELECT ?, COALESCE(MAX(sequence), 0) + 1, ?, ?, ?, NULL, NULL, 1 \
         FROM events WHERE session_id = ? RETURNING sequence",
    )
    .bind(session_id.to_string())
    .bind(occurred_at.to_rfc3339())
    .bind(serde_json::to_string(actor)?)
    .bind(serde_json::to_string(&body)?)
    .bind(session_id.to_string())
    .fetch_one(&mut *tx)
    .await?;
    crate::control_plane_sync::outbox::enqueue_run_snapshot(&mut tx, &run_id.to_string()).await?;
    tx.commit().await?;
    Ok(SessionEvent {
        sequence: u64::try_from(sequence)?,
        occurred_at,
        causation_id: None,
        correlation_id: None,
        actor: actor.clone(),
        body,
    })
}

/// Atomically persist a run's terminal projection, terminal state event, and
/// authoritative `RunCompleted` evidence. If a lifecycle command already
/// persisted the same terminal state, only the missing completion evidence is
/// appended. A different terminal state remains authoritative and is rejected.
pub async fn append_run_terminal(
    pool: &SqlitePool,
    session_id: SessionId,
    completion_actor: &Actor,
    state: RunState,
    completion: &EventBody,
    occurred_at: DateTime<Utc>,
) -> anyhow::Result<Vec<SessionEvent>> {
    let EventBody::RunCompleted {
        run_id,
        disposition,
        ..
    } = completion
    else {
        return Err(anyhow::anyhow!("append_run_terminal requires RunCompleted"));
    };
    let disposition_state = match disposition {
        RunDisposition::Completed { .. } => RunState::Completed,
        RunDisposition::Failed { .. } => RunState::Failed,
        RunDisposition::Cancelled { .. } => RunState::Cancelled,
        RunDisposition::Unknown => {
            return Err(anyhow::anyhow!("unknown terminal disposition"));
        }
        _ => return Err(anyhow::anyhow!("unsupported terminal disposition")),
    };
    if state != disposition_state {
        return Err(anyhow::anyhow!(
            "terminal state {state:?} disagrees with disposition {disposition:?}"
        ));
    }

    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let current: Option<(String,)> = sqlx::query_as(
        "SELECT r.state FROM runs r JOIN sessions s ON s.id = r.session_id \
         WHERE r.id = ? AND r.session_id = ? AND s.state != 'closed'",
    )
    .bind(run_id.to_string())
    .bind(session_id.to_string())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((current,)) = current else {
        return Err(anyhow::anyhow!("run is missing or session is closed"));
    };
    let current = crate::projections::run_state_from_db(&current);

    let (already_completed,): (i64,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM events \
         WHERE session_id = ? AND json_valid(body) \
           AND json_extract(body, '$.type') = 'RunCompleted' \
           AND json_extract(body, '$.run_id') = ?)",
    )
    .bind(session_id.to_string())
    .bind(run_id.to_string())
    .fetch_one(&mut *tx)
    .await?;
    if already_completed != 0 {
        tx.rollback().await?;
        return Ok(Vec::new());
    }

    let mut events = Vec::with_capacity(2);
    if current != state {
        if crate::projections::is_terminal(current) {
            return Err(anyhow::anyhow!(
                "refused contradictory terminal transition for {run_id}: {current:?} -> {state:?}"
            ));
        }
        let legal_from = [
            RunState::Queued,
            RunState::Preparing,
            RunState::Running,
            RunState::WaitingForApproval,
            RunState::WaitingForUserInput,
            RunState::Paused,
            RunState::Recovering,
        ];
        let affected =
            crate::projections::set_run_state_if_legal(&mut *tx, *run_id, &legal_from, state)
                .await?;
        if affected != 1 {
            return Err(anyhow::anyhow!(
                "refused stale terminal transition for {run_id}: {current:?} -> {state:?}"
            ));
        }
        sqlx::query("UPDATE runs SET ended_at = ? WHERE id = ?")
            .bind(occurred_at.to_rfc3339())
            .bind(run_id.to_string())
            .execute(&mut *tx)
            .await?;

        let body = EventBody::RunStateChanged {
            run_id: *run_id,
            state,
        };
        let (sequence,): (i64,) = sqlx::query_as(
            "INSERT INTO events \
             (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
             SELECT ?, COALESCE(MAX(sequence), 0) + 1, ?, ?, ?, NULL, NULL, 1 \
             FROM events WHERE session_id = ? RETURNING sequence",
        )
        .bind(session_id.to_string())
        .bind(occurred_at.to_rfc3339())
        .bind(serde_json::to_string(&Actor::System)?)
        .bind(serde_json::to_string(&body)?)
        .bind(session_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        events.push(SessionEvent {
            sequence: u64::try_from(sequence)?,
            occurred_at,
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body,
        });
    }

    let (sequence,): (i64,) = sqlx::query_as(
        "INSERT INTO events \
         (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
         SELECT ?, COALESCE(MAX(sequence), 0) + 1, ?, ?, ?, NULL, NULL, 1 \
         FROM events WHERE session_id = ? RETURNING sequence",
    )
    .bind(session_id.to_string())
    .bind(occurred_at.to_rfc3339())
    .bind(serde_json::to_string(completion_actor)?)
    .bind(serde_json::to_string(completion)?)
    .bind(session_id.to_string())
    .fetch_one(&mut *tx)
    .await?;
    crate::session_library::index_event_sources(
        &mut tx,
        session_id,
        sequence,
        completion,
        &occurred_at.to_rfc3339(),
    )
    .await?;
    events.push(SessionEvent {
        sequence: u64::try_from(sequence)?,
        occurred_at,
        causation_id: None,
        correlation_id: None,
        actor: completion_actor.clone(),
        body: completion.clone(),
    });

    let completion_tag = match disposition {
        RunDisposition::Completed { .. } => "successful",
        RunDisposition::Failed { .. } => "failed",
        RunDisposition::Cancelled { .. } => "cancelled",
        _ => "incomplete",
    };

    let started_at: Option<String> = sqlx::query_scalar("SELECT started_at FROM runs WHERE id = ?")
        .bind(run_id.to_string())
        .fetch_optional(&mut *tx)
        .await?;
    let latency_ms: Option<i64> = started_at
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|s| {
            (occurred_at - s.with_timezone(&Utc))
                .num_milliseconds()
                .max(0)
        });

    let session_meta: Option<(Option<i64>, Option<String>)> =
        sqlx::query_as("SELECT owner_uid, repository_id FROM sessions WHERE id = ?")
            .bind(session_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
    let (owner_uid_db, repo_id_db) = session_meta.unwrap_or((None, None));
    // `sessions.owner_uid` is nullable only for rows predating 0031_multi_user;
    // `CreateSession` has always bound it since. Defaulting an unknown owner to 0
    // would file the row against uid 0, and every analytics read is
    // `WHERE owner_uid = <principal>` (0043 leads every index with it) — so the
    // measurement would be invisible to whoever produced it and visible to uid 0.
    // Record nothing rather than record it against the wrong principal.
    let owner_uid = owner_uid_db;

    if let Some(owner_uid) = owner_uid {
        sqlx::query(
            "INSERT INTO execution_observations (
            owner_uid, run_id, attempt, node_id, session_id,
            repository_id, completion, latency_ms, observed_at
         ) VALUES (
            ?, ?, 0, '', ?,
            ?, ?, ?, ?
         )
         ON CONFLICT (run_id, attempt, node_id) DO UPDATE SET
            owner_uid = excluded.owner_uid,
            session_id = COALESCE(excluded.session_id, execution_observations.session_id),
            repository_id = COALESCE(excluded.repository_id, execution_observations.repository_id),
            completion = excluded.completion,
            latency_ms = COALESCE(excluded.latency_ms, execution_observations.latency_ms),
            observed_at = excluded.observed_at",
        )
        .bind(owner_uid)
        .bind(run_id.to_string())
        .bind(session_id.to_string())
        .bind(repo_id_db.clone())
        .bind(completion_tag)
        .bind(latency_ms)
        .bind(occurred_at.to_rfc3339())
        .execute(&mut *tx)
        .await?;
    }

    // Resolving existing entries is keyed on `run_id` alone and needs no owner,
    // so it runs whether or not the session's owner could be resolved.
    let _ = crate::inbox::resolve_run_entries(&mut tx, *run_id, occurred_at).await;

    // `migrations/0042_inbox.sql` requires a real `owner_uid` and a resolved
    // `repository_id` ("a producer with no repository context must resolve one
    // before writing rather than inventing a placeholder"). Minting a fresh
    // `RepositoryId` here would file every such entry under a different fake
    // repository, so repository filters would silently never match it.
    let inbox_target = owner_uid
        .and_then(|uid| u32::try_from(uid).ok())
        .zip(repo_id_db.as_deref().and_then(|r| r.parse().ok()));
    if let Some((owner_uid_u32, repo_id_parsed)) = inbox_target {
        match disposition {
            RunDisposition::Completed { .. } => {
                let _ = crate::inbox::produce_run_terminal(
                    &mut tx,
                    owner_uid_u32,
                    repo_id_parsed,
                    session_id,
                    *run_id,
                    None,
                    codypendent_protocol::InboxEntryKind::RunCompleted,
                    "Run completed".to_string(),
                    format!("Run {run_id} completed successfully"),
                    occurred_at,
                )
                .await;
            }
            RunDisposition::Failed { .. } => {
                let _ = crate::inbox::produce_run_terminal(
                    &mut tx,
                    owner_uid_u32,
                    repo_id_parsed,
                    session_id,
                    *run_id,
                    None,
                    codypendent_protocol::InboxEntryKind::RunFailed,
                    "Run failed".to_string(),
                    format!("Run {run_id} failed"),
                    occurred_at,
                )
                .await;
            }
            _ => {}
        }

        // The budget evaluator's production caller. Until this existed,
        // `analytics::evaluate_budgets`, `BudgetAlert`, `derive_budget_dedup_key`
        // and the `BudgetWarning` inbox kind were four halves of a feature that
        // had never been joined: nothing evaluated a threshold outside tests,
        // so no budget could ever raise anything.
        //
        // It runs INSIDE this transaction, after the observation upsert above,
        // so the run that crossed the threshold is part of the sum that decides
        // it. `produce_budget_warning` dedupes on
        // `budget:{id}:{window_start}` (`inbox::derive_budget_dedup_key`), the
        // exact key `BudgetAlert` carries, so re-evaluating on every subsequent
        // run in the same window updates one row instead of minting a new one.
        //
        // The entry is filed against THIS run's repository — the run whose
        // measurement crossed the line — never a placeholder. An owner- or
        // model-scoped budget has no repository of its own, and 0042 forbids
        // inventing one; `inbox_target` above has already established that this
        // run has a real, resolved repository, and a run without one produces
        // no inbox entry at all rather than a fabricated attribution.
        match crate::analytics::evaluate_budgets_in(&mut tx, owner_uid_u32).await {
            Ok(alerts) => {
                for alert in alerts {
                    let summary = format!(
                        "{} usage in this {} reached {} against a threshold of {}",
                        alert.dimension, alert.window, alert.current_value, alert.threshold
                    );
                    let _ = crate::inbox::produce_budget_warning(
                        &mut tx,
                        owner_uid_u32,
                        repo_id_parsed,
                        alert.budget_id,
                        &alert.window_start.to_rfc3339(),
                        Some(session_id),
                        Some(*run_id),
                        None,
                        "Budget threshold exceeded".to_string(),
                        summary,
                        occurred_at,
                    )
                    .await;
                }
            }
            // A budget evaluation failure must not fail the run's terminal
            // write. The run really did finish; refusing to record that
            // because a warning could not be computed would lose the fact.
            Err(error) => {
                tracing::warn!(%error, "could not evaluate analytics budgets at run terminal");
            }
        }
    }

    crate::control_plane_sync::outbox::enqueue_run_snapshot(&mut tx, &run_id.to_string()).await?;
    tx.commit().await?;
    Ok(events)
}

/// Atomically persist a run's MEASURED usage in both its projection and ledger.
///
/// Takes the existing protocol body rather than `codypendent_runtime::ModelUsage`
/// itself: this crate does not (and must not) depend on `codypendent-runtime`.
///
/// `None` means "not measured" and is written as SQL `NULL`, never a
/// fabricated zero a reader could mistake for a genuinely free/silent run:
/// `RunOutcome.usage` is `None` for a wholly unmeasured run (writes three
/// `NULL`s here); `Some(usage)` commonly has real `prompt_tokens` /
/// `completion_tokens` with `cost_micros` still `None` (a live driver measures
/// tokens but the price is applied downstream, if at all — see `ModelUsage`'s
/// own "tokens and cost are decoupled" doc comment).
///
/// This must run before `RunCompleted`, which is the session-close barrier. The
/// open-session predicate and projection/event writes share one write
/// transaction, so closure can neither split the two representations nor seal
/// the ledger between them.
pub async fn append_run_usage(
    pool: &SqlitePool,
    session_id: SessionId,
    actor: &Actor,
    body: &EventBody,
    occurred_at: DateTime<Utc>,
) -> anyhow::Result<SessionEvent> {
    let EventBody::RunUsage {
        run_id,
        prompt_tokens,
        completion_tokens,
        cost_micros,
        coverage,
    } = body
    else {
        return Err(anyhow::anyhow!("append_run_usage requires RunUsage"));
    };
    // SQLite stores signed integers. Reject an unrepresentable measurement
    // rather than saturating only the projection while preserving a larger u64
    // in the event, which would make two atomic representations disagree.
    let to_i64 = |value: Option<u64>| -> anyhow::Result<Option<i64>> {
        value.map(i64::try_from).transpose().map_err(Into::into)
    };
    let prompt_tokens_db = to_i64(*prompt_tokens)?;
    let completion_tokens_db = to_i64(*completion_tokens)?;
    let cost_micros_db = to_i64(*cost_micros)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let affected = sqlx::query(
        "UPDATE runs SET prompt_tokens = ?, completion_tokens = ?, cost_micros = ?, usage_coverage = ? \
         WHERE id = ? AND session_id = ? \
           AND EXISTS (SELECT 1 FROM sessions WHERE id = ? AND state != 'closed')",
    )
    .bind(prompt_tokens_db)
    .bind(completion_tokens_db)
    .bind(cost_micros_db)
    .bind(coverage.map(|value| serde_json::to_string(&value)).transpose()?)
    .bind(run_id.to_string())
    .bind(session_id.to_string())
    .bind(session_id.to_string())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(anyhow::anyhow!("run is missing or session is closed"));
    }

    let (sequence,): (i64,) = sqlx::query_as(
        "INSERT INTO events \
         (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
         SELECT ?, COALESCE(MAX(sequence), 0) + 1, ?, ?, ?, NULL, NULL, 1 \
         FROM events WHERE session_id = ? RETURNING sequence",
    )
    .bind(session_id.to_string())
    .bind(occurred_at.to_rfc3339())
    .bind(serde_json::to_string(actor)?)
    .bind(serde_json::to_string(body)?)
    .bind(session_id.to_string())
    .fetch_one(&mut *tx)
    .await?;

    let session_meta: Option<(Option<i64>, Option<String>)> =
        sqlx::query_as("SELECT owner_uid, repository_id FROM sessions WHERE id = ?")
            .bind(session_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
    let (owner_uid_db, repo_id_db) = session_meta.unwrap_or((None, None));
    // See the sibling observation write above: an unknown owner must not become
    // uid 0, because every analytics index and read predicate leads with
    // `owner_uid`. A usage row filed against the wrong principal is worse than
    // an absent one.
    let owner_uid = owner_uid_db;

    let routing_meta: Option<(String, String, String)> = sqlx::query_as(
        "SELECT model_id, endpoint, task_class FROM model_task_outcomes WHERE run_id = ? LIMIT 1",
    )
    .bind(run_id.to_string())
    .fetch_optional(&mut *tx)
    .await?;
    let (model_id_db, endpoint_db, task_class_db) = match routing_meta {
        Some((m, e, t)) => (Some(m), Some(e), Some(t)),
        None => (None, None, None),
    };

    if let Some(owner_uid) = owner_uid {
        sqlx::query(
            "INSERT INTO execution_observations (
            owner_uid, run_id, attempt, node_id, session_id,
            repository_id, task_class, model_id, endpoint,
            input_tokens, output_tokens, cost_micros, observed_at
         ) VALUES (
            ?, ?, 0, '', ?,
            ?, ?, ?, ?,
            ?, ?, ?, ?
         )
         ON CONFLICT (run_id, attempt, node_id) DO UPDATE SET
            owner_uid = excluded.owner_uid,
            session_id = COALESCE(excluded.session_id, execution_observations.session_id),
            repository_id = COALESCE(excluded.repository_id, execution_observations.repository_id),
            task_class = COALESCE(excluded.task_class, execution_observations.task_class),
            model_id = COALESCE(excluded.model_id, execution_observations.model_id),
            endpoint = COALESCE(excluded.endpoint, execution_observations.endpoint),
            input_tokens = COALESCE(excluded.input_tokens, execution_observations.input_tokens),
            output_tokens = COALESCE(excluded.output_tokens, execution_observations.output_tokens),
            cost_micros = COALESCE(excluded.cost_micros, execution_observations.cost_micros),
            observed_at = excluded.observed_at",
        )
        .bind(owner_uid)
        .bind(run_id.to_string())
        .bind(session_id.to_string())
        .bind(repo_id_db)
        .bind(task_class_db)
        .bind(model_id_db)
        .bind(endpoint_db)
        .bind(prompt_tokens_db)
        .bind(completion_tokens_db)
        .bind(cost_micros_db)
        .bind(
            coverage
                .map(|value| serde_json::to_string(&value))
                .transpose()?,
        )
        .bind(occurred_at.to_rfc3339())
        .execute(&mut *tx)
        .await?;
    }

    crate::control_plane_sync::outbox::enqueue_run_snapshot(&mut tx, &run_id.to_string()).await?;
    tx.commit().await?;

    Ok(SessionEvent {
        sequence: u64::try_from(sequence)?,
        occurred_at,
        causation_id: None,
        correlation_id: None,
        actor: actor.clone(),
        body: body.clone(),
    })
}

/// The next sequence number for a session (1-based).
pub async fn next_sequence(pool: &SqlitePool, session_id: SessionId) -> anyhow::Result<u64> {
    let (max,): (i64,) =
        sqlx::query_as("SELECT COALESCE(MAX(sequence), 0) FROM events WHERE session_id = ?")
            .bind(session_id.to_string())
            .fetch_one(pool)
            .await?;
    Ok(u64::try_from(max)? + 1)
}

pub async fn session_count(pool: &SqlitePool) -> anyhow::Result<i64> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions")
        .fetch_one(pool)
        .await?;
    Ok(count)
}

/// Count of in-flight work — both session runs and durable **workflow** runs —
/// whose `state` is **not** terminal. For `runs` the terminal `RunState`s are
/// `Completed`/`Failed`/`Cancelled` (the DB strings written by
/// [`crate::projections::run_state_to_db`]); for `workflow_runs` they are the
/// lowercase `completed`/`failed`/`cancelled` (see
/// `codypendent_workflow::store::WorkflowRunState`). Every other state —
/// `Queued`/`Preparing`/`Running`/`WaitingForApproval`/`WaitingForUserInput`/
/// `Paused`/`Recovering`/`Unknown` for runs, and `pending`/`running`/`paused`
/// for workflow runs — counts as active.
///
/// Both are included because the auto-restart idle gate must mean *no work is
/// in flight*: a restart while EITHER a session run or a workflow run is live
/// would disrupt in-memory state a stopped daemon cannot recover, so this must
/// never undercount. `workflow_runs` is created by the same migrations
/// (`0010_workflow_runs.sql`) on the same pool, so the two counts sum in one
/// query.
pub async fn active_run_count(pool: &SqlitePool) -> anyhow::Result<i64> {
    let (count,): (i64,) = sqlx::query_as(
        "SELECT \
           (SELECT COUNT(*) FROM runs \
              WHERE state NOT IN ('Completed', 'Failed', 'Cancelled')) \
         + (SELECT COUNT(*) FROM workflow_runs \
              WHERE state NOT IN ('completed', 'failed', 'cancelled'))",
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Copy `events` (already loaded, already ordered) into `target`, remapping
/// run ids through `id_map` and renumbering sequences from 1. Pure over its
/// inputs plus the appends; the source session is untouched.
///
/// Takes a `&mut SqliteConnection` (not a pool) so a fork can copy the whole
/// head **inside a single transaction** — the fork's session row, its copied
/// events, its cloned run rows, and its marker event all commit atomically, so
/// a crash leaves either no fork or a complete one (never a partial orphan).
pub async fn copy_events_remapped(
    conn: &mut SqliteConnection,
    target: SessionId,
    events: &[SessionEvent],
    id_map: &HashMap<RunId, RunId>,
) -> anyhow::Result<u64> {
    let mut sequence = 0u64;
    for event in events {
        sequence += 1;
        let copied = SessionEvent {
            sequence,
            occurred_at: event.occurred_at,
            causation_id: None, // command rows are not copied
            correlation_id: event.correlation_id,
            actor: remap_actor(&event.actor, id_map),
            body: remap_body(&event.body, id_map),
        };
        append_event_conn(&mut *conn, target, &copied).await?;
    }
    Ok(sequence)
}

/// Append one pre-numbered event within a caller-provided transaction/connection.
/// The transactional twin of [`append_event`] (which appends against a pool);
/// used by [`copy_events_remapped`] and session forking so a whole batch of
/// appends commits atomically.
pub async fn append_event_conn(
    conn: &mut SqliteConnection,
    session_id: SessionId,
    event: &SessionEvent,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO events \
         (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 1)",
    )
    .bind(session_id.to_string())
    .bind(i64::try_from(event.sequence)?)
    .bind(event.occurred_at.to_rfc3339())
    .bind(serde_json::to_string(&event.actor)?)
    .bind(serde_json::to_string(&event.body)?)
    .bind(event.causation_id.map(|id| id.to_string()))
    .bind(event.correlation_id.map(|id| id.to_string()))
    .execute(&mut *conn)
    .await?;
    crate::session_library::index_event_sources(
        conn,
        session_id,
        i64::try_from(event.sequence)?,
        &event.body,
        &event.occurred_at.to_rfc3339(),
    )
    .await?;
    Ok(())
}

fn remap(id: RunId, map: &HashMap<RunId, RunId>) -> RunId {
    *map.get(&id).unwrap_or(&id)
}

fn remap_actor(actor: &Actor, map: &HashMap<RunId, RunId>) -> Actor {
    match actor {
        Actor::Agent {
            agent_id,
            run_id,
            model,
        } => Actor::Agent {
            agent_id: *agent_id,
            run_id: remap(*run_id, map),
            model: model.clone(),
        },
        other => other.clone(),
    }
}

fn remap_body(body: &EventBody, map: &HashMap<RunId, RunId>) -> EventBody {
    use EventBody::*;
    match body {
        NoteAppended { text, run_id } => NoteAppended {
            text: text.clone(),
            run_id: run_id.map(|id| remap(id, map)),
        },
        RunStarted {
            run_id,
            objective,
            mode,
        } => RunStarted {
            run_id: remap(*run_id, map),
            objective: objective.clone(),
            mode: *mode,
        },
        RunStateChanged { run_id, state } => RunStateChanged {
            run_id: remap(*run_id, map),
            state: *state,
        },
        ModelStreamDelta {
            run_id,
            text,
            thought,
        } => ModelStreamDelta {
            run_id: remap(*run_id, map),
            text: text.clone(),
            // Carried through: a remap rewrites identifiers, never what the
            // chunk WAS. Dropping this would silently reclassify every piece of
            // reasoning in a remapped transcript as the model's reply.
            thought: *thought,
        },
        ToolProposed {
            run_id,
            approval_id,
            action,
        } => ToolProposed {
            run_id: remap(*run_id, map),
            approval_id: *approval_id,
            action: action.clone(),
        },
        ToolDenied {
            run_id,
            action,
            reasons,
        } => ToolDenied {
            run_id: remap(*run_id, map),
            action: action.clone(),
            reasons: reasons.clone(),
        },
        ToolStarted {
            run_id,
            tool,
            args_digest,
            label,
        } => ToolStarted {
            run_id: remap(*run_id, map),
            tool: tool.clone(),
            args_digest: args_digest.clone(),
            label: label.clone(),
        },
        ToolUpdated {
            run_id,
            tool,
            label,
        } => ToolUpdated {
            run_id: remap(*run_id, map),
            tool: tool.clone(),
            label: label.clone(),
        },
        ToolCompleted {
            run_id,
            tool,
            outcome,
            artifact,
        } => ToolCompleted {
            run_id: remap(*run_id, map),
            tool: tool.clone(),
            outcome: outcome.clone(),
            artifact: artifact.clone(),
        },
        PatchProposed {
            run_id,
            changeset_id,
            artifact,
            files,
            additions,
            deletions,
            preview,
            preview_truncated,
        } => PatchProposed {
            run_id: remap(*run_id, map),
            changeset_id: *changeset_id,
            artifact: artifact.clone(),
            files: files.clone(),
            additions: *additions,
            deletions: *deletions,
            preview: preview.clone(),
            preview_truncated: *preview_truncated,
        },
        SteeringQueued { run_id } => SteeringQueued {
            run_id: remap(*run_id, map),
        },
        SteeringApplied { run_id } => SteeringApplied {
            run_id: remap(*run_id, map),
        },
        BudgetWarning {
            run_id,
            dimension,
            used,
            limit,
        } => BudgetWarning {
            run_id: remap(*run_id, map),
            dimension: *dimension,
            used: *used,
            limit: *limit,
        },
        RunCompleted {
            run_id,
            disposition,
            chronicle,
        } => RunCompleted {
            run_id: remap(*run_id, map),
            disposition: disposition.clone(),
            chronicle: chronicle.clone(),
        },
        RunUsage {
            run_id,
            prompt_tokens,
            completion_tokens,
            cost_micros,
            coverage,
        } => RunUsage {
            run_id: remap(*run_id, map),
            prompt_tokens: *prompt_tokens,
            completion_tokens: *completion_tokens,
            cost_micros: *cost_micros,
            coverage: *coverage,
        },
        LearningsCaptured {
            run_id,
            proposed_count,
            proposed_ids,
            activated_count,
            activated_ids,
        } => LearningsCaptured {
            run_id: remap(*run_id, map),
            proposed_count: *proposed_count,
            proposed_ids: proposed_ids.clone(),
            activated_count: *activated_count,
            activated_ids: activated_ids.clone(),
        },
        QuestionAsked {
            question_id,
            run_id,
            questions,
        } => QuestionAsked {
            question_id: *question_id,
            run_id: remap(*run_id, map),
            questions: questions.clone(),
        },
        CheckpointRecorded {
            run_id,
            checkpoint_id,
            ordinal,
            kind,
            commit,
            base_commit,
        } => CheckpointRecorded {
            run_id: remap(*run_id, map),
            checkpoint_id: *checkpoint_id,
            ordinal: *ordinal,
            kind: *kind,
            commit: commit.clone(),
            base_commit: base_commit.clone(),
        },
        CheckpointRestored {
            run_id,
            checkpoint_id,
            restored,
        } => CheckpointRestored {
            run_id: remap(*run_id, map),
            checkpoint_id: *checkpoint_id,
            restored: *restored,
        },
        // Variants carrying no RunId: SessionCreated, SessionClosed, ApprovalRequested,
        // ApprovalResolved, ClientPresenceChanged, QuestionResolved, SessionForked, Unknown.
        other => other.clone(),
    }
}

/// Find the ledger sequence of the `RunStarted` event for `run_id`.
pub async fn run_started_sequence(
    pool: &SqlitePool,
    session_id: SessionId,
    run_id: RunId,
) -> anyhow::Result<Option<u64>> {
    let events = load_events(pool, session_id).await?;
    for event in events {
        if let EventBody::RunStarted { run_id: r_id, .. } = &event.body {
            if *r_id == run_id {
                return Ok(Some(event.sequence));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::{ArtifactId, ArtifactRef, DataClassification, RunId, SessionId};

    async fn test_pool(dir: &std::path::Path) -> SqlitePool {
        crate::db::open_database(&dir.join("test.db"))
            .await
            .expect("open database")
    }

    /// Insert a session, then a run in `state`, under a fresh id.
    async fn seed_run(pool: &SqlitePool, state: &str) {
        let session_id = SessionId::new();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO sessions (id, title, state, created_at, updated_at, revision) \
             VALUES (?, ?, 'open', ?, ?, 0)",
        )
        .bind(session_id.to_string())
        .bind("active-run-count-test")
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await
        .expect("insert session");

        sqlx::query(
            "INSERT INTO runs (id, session_id, objective, state, mode, model_policy, budget_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(RunId::new().to_string())
        .bind(session_id.to_string())
        .bind("diagnose")
        .bind(state)
        .bind("Build")
        .bind("hosted-default")
        .bind("{}")
        .execute(pool)
        .await
        .expect("insert run");
    }

    /// How many events a session currently holds. Used by the rejection tests
    /// to assert that a refused write appends NOTHING — a refusal that still
    /// journalled an event would leave the ledger claiming a change the
    /// projection never made.
    async fn event_count(pool: &SqlitePool, session_id: SessionId) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events WHERE session_id = ?")
            .bind(session_id.to_string())
            .fetch_one(pool)
            .await?;
        Ok(count)
    }

    /// Insert a session and a `Queued` run under fresh, caller-visible ids.
    async fn seed_queued_run(pool: &SqlitePool) -> (SessionId, RunId) {
        let session_id = SessionId::new();
        let run_id = RunId::new();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO sessions (id, title, state, created_at, updated_at, revision) \
             VALUES (?, ?, 'open', ?, ?, 0)",
        )
        .bind(session_id.to_string())
        .bind("ledger-timing-test")
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await
        .expect("insert session");
        sqlx::query(
            "INSERT INTO runs (id, session_id, objective, state, mode, model_policy, budget_json) \
             VALUES (?, ?, 'diagnose', 'Queued', 'Build', 'hosted-default', '{}')",
        )
        .bind(run_id.to_string())
        .bind(session_id.to_string())
        .execute(pool)
        .await
        .expect("insert run");
        (session_id, run_id)
    }

    async fn append_usage(
        pool: &SqlitePool,
        session_id: SessionId,
        run_id: RunId,
        prompt_tokens: Option<u64>,
        completion_tokens: Option<u64>,
        cost_micros: Option<u64>,
    ) -> anyhow::Result<SessionEvent> {
        append_run_usage(
            pool,
            session_id,
            &Actor::System,
            &EventBody::RunUsage {
                run_id,
                prompt_tokens,
                completion_tokens,
                cost_micros,
                coverage: None,
            },
            Utc::now(),
        )
        .await
    }

    /// `(started_at, ended_at)` off the `runs` row, both possibly `NULL`.
    async fn run_timing(pool: &SqlitePool, run_id: RunId) -> (Option<String>, Option<String>) {
        sqlx::query_as("SELECT started_at, ended_at FROM runs WHERE id = ?")
            .bind(run_id.to_string())
            .fetch_one(pool)
            .await
            .expect("run row")
    }

    #[tokio::test]
    async fn active_run_count_is_zero_with_no_runs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = test_pool(tmp.path()).await;
        assert_eq!(active_run_count(&pool).await.expect("count"), 0);
    }

    #[tokio::test]
    async fn terminal_only_runs_do_not_count_as_active() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = test_pool(tmp.path()).await;
        seed_run(&pool, "Completed").await;
        seed_run(&pool, "Failed").await;
        seed_run(&pool, "Cancelled").await;
        assert_eq!(active_run_count(&pool).await.expect("count"), 0);
    }

    #[tokio::test]
    async fn every_non_terminal_state_counts_as_active() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = test_pool(tmp.path()).await;
        // A settled terminal baseline...
        seed_run(&pool, "Completed").await;
        seed_run(&pool, "Failed").await;
        seed_run(&pool, "Cancelled").await;
        assert_eq!(active_run_count(&pool).await.expect("count"), 0);

        // ...then one non-terminal run at a time, each incrementing the count.
        // Includes waiting/paused states, which must count as active (never
        // undercount: a restart would disrupt them).
        let non_terminal = [
            "Queued",
            "Preparing",
            "Running",
            "WaitingForApproval",
            "WaitingForUserInput",
            "Paused",
            "Recovering",
        ];
        for (i, state) in non_terminal.iter().enumerate() {
            seed_run(&pool, state).await;
            assert_eq!(
                active_run_count(&pool).await.expect("count"),
                (i + 1) as i64,
                "state {state} must count as active"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Outcome 20: `runs.started_at` / `ended_at` actually get written.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_run_transition_stamps_started_at_once_and_ended_at_on_completion() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = test_pool(tmp.path()).await;
        let (session_id, run_id) = seed_queued_run(&pool).await;

        let (started, ended) = run_timing(&pool, run_id).await;
        assert_eq!(started, None, "a Queued run has not started yet");
        assert_eq!(ended, None);

        append_run_state_changed(
            &pool,
            session_id,
            &Actor::System,
            run_id,
            RunState::Preparing,
            Utc::now(),
        )
        .await
        .expect("-> Preparing");
        let (started, ended) = run_timing(&pool, run_id).await;
        assert_eq!(started, None, "Preparing is not Running yet");
        assert_eq!(ended, None);

        let first_running_at = Utc::now();
        append_run_state_changed(
            &pool,
            session_id,
            &Actor::System,
            run_id,
            RunState::Running,
            first_running_at,
        )
        .await
        .expect("-> Running");
        let (started, ended) = run_timing(&pool, run_id).await;
        assert_eq!(
            started.as_deref(),
            Some(first_running_at.to_rfc3339().as_str())
        );
        assert_eq!(ended, None, "a Running run has not ended");

        // A pause/resume cycle legally revisits Running — `started_at` must
        // keep the FIRST moment, not the resume moment.
        append_run_state_changed(
            &pool,
            session_id,
            &Actor::System,
            run_id,
            RunState::Paused,
            Utc::now(),
        )
        .await
        .expect("-> Paused");
        append_run_state_changed(
            &pool,
            session_id,
            &Actor::System,
            run_id,
            RunState::Running,
            Utc::now(),
        )
        .await
        .expect("-> Running again (resume)");
        let (started_after_resume, ended) = run_timing(&pool, run_id).await;
        assert_eq!(
            started_after_resume.as_deref(),
            Some(first_running_at.to_rfc3339().as_str()),
            "started_at must survive a pause/resume cycle unchanged"
        );
        assert_eq!(ended, None);

        let completed_at = Utc::now();
        append_run_state_changed(
            &pool,
            session_id,
            &Actor::System,
            run_id,
            RunState::Completed,
            completed_at,
        )
        .await
        .expect("-> Completed");
        let (started, ended) = run_timing(&pool, run_id).await;
        assert_eq!(
            started.as_deref(),
            Some(first_running_at.to_rfc3339().as_str()),
            "started_at is still the original Running moment"
        );
        assert_eq!(ended.as_deref(), Some(completed_at.to_rfc3339().as_str()));
    }

    #[tokio::test]
    async fn append_run_usage_writes_projection_and_event_without_inventing_measurements() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = test_pool(tmp.path()).await;
        let (session_id, run_id) = seed_queued_run(&pool).await;

        type UsageRow = (Option<i64>, Option<i64>, Option<i64>);
        let read_usage = |pool: SqlitePool, run_id: RunId| async move {
            sqlx::query_as::<_, UsageRow>(
                "SELECT prompt_tokens, completion_tokens, cost_micros FROM runs WHERE id = ?",
            )
            .bind(run_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("run row")
        };

        // A wholly unmeasured run writes three NULLs, not fabricated zeros.
        append_usage(&pool, session_id, run_id, None, None, None)
            .await
            .expect("record unmeasured");
        assert_eq!(read_usage(pool.clone(), run_id).await, (None, None, None));

        // Tokens measured, cost not (the common case: a live driver has no
        // per-token price) — cost_micros stays NULL while tokens are real.
        append_usage(&pool, session_id, run_id, Some(1050), Some(1052), None)
            .await
            .expect("record tokens-only");
        assert_eq!(
            read_usage(pool.clone(), run_id).await,
            (Some(1050), Some(1052), None)
        );

        // A genuinely measured zero cost (e.g. a free local model) is `Some(0)`,
        // distinct from `None` — both round-trip exactly.
        append_usage(&pool, session_id, run_id, Some(200), Some(50), Some(0))
            .await
            .expect("record measured zero cost");
        assert_eq!(
            read_usage(pool.clone(), run_id).await,
            (Some(200), Some(50), Some(0))
        );

        // Fully measured and priced.
        let event = append_usage(
            &pool,
            session_id,
            run_id,
            Some(4000),
            Some(212),
            Some(15_000),
        )
        .await
        .expect("record fully measured");
        assert_eq!(
            read_usage(pool, run_id).await,
            (Some(4000), Some(212), Some(15_000))
        );
        assert!(matches!(
            event.body,
            EventBody::RunUsage {
                run_id: persisted,
                prompt_tokens: Some(4000),
                completion_tokens: Some(212),
                cost_micros: Some(15_000),
                coverage: None,
            } if persisted == run_id
        ));
    }

    #[tokio::test]
    async fn append_run_usage_cannot_mutate_a_closed_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = test_pool(tmp.path()).await;
        let (session_id, run_id) = seed_queued_run(&pool).await;
        let before = event_count(&pool, session_id).await.expect("event count");
        sqlx::query("UPDATE sessions SET state = 'closed' WHERE id = ?")
            .bind(session_id.to_string())
            .execute(&pool)
            .await
            .expect("close session");

        let error = append_usage(&pool, session_id, run_id, Some(10), Some(5), Some(0))
            .await
            .expect_err("closed sessions seal usage writes");
        assert!(error.to_string().contains("session is closed"));

        let usage: (Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT prompt_tokens, completion_tokens, cost_micros FROM runs WHERE id = ?",
        )
        .bind(run_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("run usage projection");
        assert_eq!(usage, (None, None, None));
        assert_eq!(
            event_count(&pool, session_id).await.expect("event count"),
            before,
            "the rejected write appends no event"
        );
    }

    #[tokio::test]
    async fn append_run_usage_rejects_values_sqlite_cannot_represent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = test_pool(tmp.path()).await;
        let (session_id, run_id) = seed_queued_run(&pool).await;
        let before = event_count(&pool, session_id).await.expect("event count");

        append_usage(&pool, session_id, run_id, Some(u64::MAX), Some(1), None)
            .await
            .expect_err("an unrepresentable projection value must not be saturated");

        let usage: (Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT prompt_tokens, completion_tokens, cost_micros FROM runs WHERE id = ?",
        )
        .bind(run_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("run usage projection");
        assert_eq!(usage, (None, None, None));
        assert_eq!(
            event_count(&pool, session_id).await.expect("event count"),
            before
        );
    }

    #[tokio::test]
    async fn append_run_terminal_commits_projection_and_both_events_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = test_pool(tmp.path()).await;
        let (session_id, run_id) = seed_queued_run(&pool).await;
        let completed = EventBody::RunCompleted {
            run_id,
            disposition: RunDisposition::Completed {
                summary: Some("done".into()),
            },
            chronicle: ArtifactRef {
                id: ArtifactId::new(),
                media_type: "application/json".into(),
                byte_length: 2,
                sha256: "00".repeat(32),
                sensitivity: DataClassification::Internal,
            },
        };

        let events = append_run_terminal(
            &pool,
            session_id,
            &Actor::System,
            RunState::Completed,
            &completed,
            Utc::now(),
        )
        .await
        .expect("terminal transaction");
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].body,
            EventBody::RunStateChanged {
                state: RunState::Completed,
                ..
            }
        ));
        assert!(matches!(events[1].body, EventBody::RunCompleted { .. }));
        assert_eq!(events[1].sequence, events[0].sequence + 1);
        assert_eq!(
            crate::projections::load_run_state(&pool, run_id)
                .await
                .unwrap(),
            Some(RunState::Completed)
        );

        let replay = append_run_terminal(
            &pool,
            session_id,
            &Actor::System,
            RunState::Completed,
            &completed,
            Utc::now(),
        )
        .await
        .expect("terminal replay");
        assert!(replay.is_empty());
        assert_eq!(event_count(&pool, session_id).await.unwrap(), 2);
    }
}
