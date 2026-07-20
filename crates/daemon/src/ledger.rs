//! The append-only event ledger.
//!
//! Phase 0 provides create/append/load/count. Later phases add commands with
//! idempotency keys, the crash-consistency write path, projections, and
//! subscriptions — the storage shape here is already the durable ordering
//! authority they build on.

use chrono::{DateTime, Utc};
use codypendent_protocol::{Actor, EventBody, SessionEvent, SessionId};
use sqlx::SqlitePool;

/// Insert a session row in state `open`.
pub async fn create_session(
    pool: &SqlitePool,
    session_id: SessionId,
    title: &str,
) -> anyhow::Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO sessions (id, title, state, created_at, updated_at, revision) \
         VALUES (?, ?, 'open', ?, ?, 0)",
    )
    .bind(session_id.to_string())
    .bind(title)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
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
    .execute(pool)
    .await?;
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
    let (sequence,): (i64,) = sqlx::query_as(
        "INSERT INTO events \
         (session_id, sequence, occurred_at, actor, body, causation_id, correlation_id, schema_version) \
         SELECT ?, COALESCE(MAX(sequence), 0) + 1, ?, ?, ?, NULL, NULL, 1 \
         FROM events WHERE session_id = ? \
         RETURNING sequence",
    )
    .bind(session_id.to_string())
    .bind(occurred_at.to_rfc3339())
    .bind(serde_json::to_string(actor)?)
    .bind(serde_json::to_string(body)?)
    .bind(session_id.to_string())
    .fetch_one(pool)
    .await?;
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
