//! The append-only event ledger.
//!
//! Phase 0 provides create/append/load/count. Later phases add commands with
//! idempotency keys, the crash-consistency write path, projections, and
//! subscriptions — the storage shape here is already the durable ordering
//! authority they build on.

use chrono::{DateTime, Utc};
use codypendent_protocol::{SessionEvent, SessionId};
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
