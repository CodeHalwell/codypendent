//! Owner-scoped Session Library queries.
//!
//! Search is a read model over authoritative session metadata. The caller must
//! supply the transport-derived [`PeerPrincipal`]; no owner value from the wire
//! enters this module. Results can originate from titles, durable
//! transcript/tool/patch events, artifact references, changed paths, and typed
//! IDE symbol context, with deterministic ranking and opaque keyset cursors.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::str::FromStr;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use codypendent_protocol::{
    ArtifactId, ArtifactRef, CommandBody, CommandId, EventBody, InputBlock, PageCursor,
    RepositoryId, RunId, RunState, SessionDeepLink, SessionId, SessionSearchPage,
    SessionSearchQuery, SessionSearchResult, SessionSearchScope, SessionSearchSource,
    SessionSummary, WorkspaceId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{QueryBuilder, Sqlite, SqliteConnection, SqlitePool};

use crate::principal::PeerPrincipal;

const DEFAULT_PAGE_SIZE: u32 = 50;
const MAX_PAGE_SIZE: u32 = 200;
const CURSOR_VERSION: u8 = 1;
const NOTE_BASE_SCORE: f64 = 600.0;
const MODEL_STREAM_BASE_SCORE: f64 = 550.0;

/// A Session Library failure safe to translate at the protocol boundary.
#[derive(Debug, thiserror::Error)]
pub enum SessionLibraryError {
    #[error("invalid or stale session search cursor")]
    InvalidCursor,
    #[error("session library database query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("session library contains invalid data: {0}")]
    InvalidData(String),
}

#[derive(Debug, sqlx::FromRow)]
struct SessionRow {
    id: String,
    workspace_id: Option<String>,
    title: String,
    state: String,
    updated_at: String,
    created_at: String,
    internal: i64,
    parent_session_id: Option<String>,
    parent_run_id: Option<String>,
    pinned: i64,
    archived_at: Option<String>,
    repository_id: Option<String>,
    repository: Option<String>,
    workspace: Option<String>,
    last_activity_at: Option<String>,
    last_run_id: Option<String>,
    run_state: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct EventRow {
    session_id: String,
    sequence: i64,
    body: String,
}

#[derive(Debug, sqlx::FromRow)]
struct CommandRow {
    id: String,
    session_id: String,
    body: String,
}

#[derive(Debug)]
struct SearchSourceEntry {
    source_type: &'static str,
    source_id: String,
    content_hash: String,
    event_sequence: Option<i64>,
    run_id: Option<RunId>,
    artifact_id: Option<ArtifactId>,
}

#[derive(Debug)]
struct Candidate {
    result: SessionSearchResult,
    pinned: bool,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SearchCursor {
    version: u8,
    query_hash: String,
    score_bits: u64,
    pinned: bool,
    updated_at: DateTime<Utc>,
    stable_identity: String,
}

/// Search session metadata and durable content visible to `principal`.
///
/// The owner predicate is part of the SQL query, before ranking, counts, or
/// pagination. A caller therefore cannot infer another user's matches from page
/// length or cursor presence. Rows predating owner metadata resolve to the
/// daemon uid, matching the daemon's central ownership gate.
pub async fn search_sessions(
    pool: &SqlitePool,
    daemon_uid: u32,
    principal: PeerPrincipal,
    query: &SessionSearchQuery,
) -> Result<SessionSearchPage, SessionLibraryError> {
    // Keep authorization, lifecycle visibility, source loading, ranking, and
    // paging on one SQLite snapshot. A concurrent owner/tombstone mutation
    // therefore cannot leak sources selected from stale session metadata.
    let mut tx = pool.begin().await?;
    let query_hash = query_hash(principal.uid(), query)?;
    let cursor = query
        .cursor
        .as_ref()
        .map(|cursor| decode_cursor(cursor, &query_hash))
        .transpose()?;
    let limit = usize::try_from(if query.limit == 0 {
        DEFAULT_PAGE_SIZE
    } else {
        query.limit.min(MAX_PAGE_SIZE)
    })
    .unwrap_or(MAX_PAGE_SIZE as usize);
    let rows = load_visible_sessions(&mut tx, daemon_uid, principal, query).await?;
    let sessions = rows
        .into_iter()
        .map(SessionSummary::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    let mut candidates = sessions
        .iter()
        .cloned()
        .filter_map(|session| title_candidate(session, &query.query))
        .collect::<Vec<_>>();
    if !query.query.trim().is_empty() {
        // A title page with an extra higher-ranked result cannot contain these
        // lower-ranked transcripts. Avoid materializing/decoding every matching
        // note in a long history just to sort it below that page. Re-evaluate
        // after the cursor on every request so later pages still include them.
        // The extra result preserves next_cursor without probing skipped rows.
        let transcript_ceiling =
            text_score_ceiling(NOTE_BASE_SCORE.max(MODEL_STREAM_BASE_SCORE), &query.query);
        let skip_transcripts = candidates
            .iter()
            .filter(|candidate| {
                candidate.result.score > transcript_ceiling
                    && cursor
                        .as_ref()
                        .is_none_or(|cursor| candidate_is_after(candidate, cursor))
            })
            .take(limit + 1)
            .count()
            > limit;
        let summaries = sessions
            .iter()
            .map(|session| (session.session_id, session))
            .collect::<HashMap<_, _>>();
        let prefilter = body_prefilter(&query.query);
        for row in
            load_visible_events(&mut tx, &sessions, prefilter.as_ref(), skip_transcripts).await?
        {
            let session_id = row.session_id.parse().map_err(|error| {
                SessionLibraryError::InvalidData(format!("invalid event session id: {error}"))
            })?;
            let Some(session) = summaries.get(&session_id) else {
                continue;
            };
            let sequence = u64::try_from(row.sequence).map_err(|error| {
                SessionLibraryError::InvalidData(format!("invalid event sequence: {error}"))
            })?;
            let body: EventBody = serde_json::from_str(&row.body).map_err(|error| {
                SessionLibraryError::InvalidData(format!("invalid event body: {error}"))
            })?;
            candidates.extend(event_candidates(session, sequence, &body, &query.query));
        }
        for row in load_visible_commands(&mut tx, &sessions, prefilter.as_ref()).await? {
            let session_id = row.session_id.parse().map_err(|error| {
                SessionLibraryError::InvalidData(format!("invalid command session id: {error}"))
            })?;
            let Some(session) = summaries.get(&session_id) else {
                continue;
            };
            let body: CommandBody = serde_json::from_str(&row.body).map_err(|error| {
                SessionLibraryError::InvalidData(format!("invalid command body: {error}"))
            })?;
            candidates.extend(command_candidates(session, &row.id, &body, &query.query));
        }
    }
    candidates.sort_by(compare_candidates);

    if let Some(cursor) = &cursor {
        candidates.retain(|candidate| candidate_is_after(candidate, cursor));
    }

    let has_more = candidates.len() > limit;
    let items = candidates
        .iter()
        .take(limit)
        .map(|candidate| candidate.result.clone())
        .collect::<Vec<_>>();
    let next_cursor = if has_more {
        candidates
            .get(limit.saturating_sub(1))
            .map(|candidate| encode_cursor(candidate, &query_hash))
            .transpose()?
    } else {
        None
    };

    tx.commit().await?;
    Ok(SessionSearchPage { items, next_cursor })
}

/// Record the source-of-truth hash for a session title in the same transaction
/// that creates or renames the session. The text remains in `sessions`; this
/// row is rebuild/freshness bookkeeping for the derived search index.
pub(crate) async fn index_title_source(
    exec: impl sqlx::SqliteExecutor<'_>,
    session_id: SessionId,
    title: &str,
    indexed_at: &str,
) -> Result<(), sqlx::Error> {
    let content_hash = hex::encode(Sha256::digest(title.as_bytes()));
    sqlx::query(
        "INSERT INTO session_search_sources \
         (session_id, source_type, source_id, content_hash, indexed_at) \
         VALUES (?, 'title', 'title', ?, ?) \
         ON CONFLICT(session_id, source_type, source_id) DO UPDATE SET \
         content_hash = excluded.content_hash, indexed_at = excluded.indexed_at",
    )
    .bind(session_id.to_string())
    .bind(content_hash)
    .bind(indexed_at)
    .execute(exec)
    .await?;
    Ok(())
}

/// Index every searchable projection of one newly appended durable event.
///
/// The event row must already exist on `conn`; callers invoke this inside the
/// same transaction as the ledger append. Stable `(source_type, source_id)`
/// keys make retries and deterministic rebuilds safe.
pub(crate) async fn index_event_sources(
    conn: &mut SqliteConnection,
    session_id: SessionId,
    sequence: i64,
    body: &EventBody,
    indexed_at: &str,
) -> Result<usize, sqlx::Error> {
    let entries = event_source_entries(sequence, body);
    for entry in &entries {
        write_source_entry(&mut *conn, session_id, entry, indexed_at).await?;
    }
    Ok(entries.len())
}

/// Index typed IDE context carried by a successfully applied command.
pub(crate) async fn index_command_sources(
    conn: &mut SqliteConnection,
    session_id: SessionId,
    command_id: CommandId,
    body: &CommandBody,
    indexed_at: &str,
) -> Result<usize, sqlx::Error> {
    let entries = command_source_entries(command_id, body);
    for entry in &entries {
        write_source_entry(&mut *conn, session_id, entry, indexed_at).await?;
    }
    Ok(entries.len())
}

/// Atomically reconstruct source bookkeeping from the authoritative session,
/// event, and applied-command rows.
///
/// A crash can therefore interrupt incremental indexing without losing search
/// provenance permanently: the next rebuild either commits the complete,
/// deterministic source set or leaves the previous set untouched.
pub async fn rebuild_search_sources(pool: &SqlitePool) -> Result<usize, SessionLibraryError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    // Whole-table delete first: it is the only step that can retire a row whose
    // session no longer exists at all, which the per-session re-derivation
    // below cannot see.
    sqlx::query("DELETE FROM session_search_sources")
        .execute(&mut *tx)
        .await?;
    let sessions: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, title, updated_at FROM sessions ORDER BY id")
            .fetch_all(&mut *tx)
            .await?;
    // Session at a time rather than `fetch_all` over the events and commands of
    // the WHOLE database: that held every event body of every session in memory
    // at once, on top of the same rows already being read from disk.
    for (session_id, title, indexed_at) in sessions {
        let session_id: SessionId = session_id.parse().map_err(|error| {
            SessionLibraryError::InvalidData(format!("invalid session id: {error}"))
        })?;
        index_session_sources(&mut tx, session_id, &title, &indexed_at).await?;
    }
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM session_search_sources")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    usize::try_from(count).map_err(|error| SessionLibraryError::InvalidData(error.to_string()))
}

/// Index the sessions that have never been indexed, and only those.
///
/// # Why boot does not rebuild everything
///
/// [`rebuild_search_sources`] is a whole-history rebuild under `BEGIN
/// IMMEDIATE` — it read every event body of every session into memory, deleted
/// the entire index, and wrote it again, holding the database's write lock for
/// the duration. Running that at EVERY start paid for a repair that, after the
/// first one, had nothing to repair.
///
/// # What makes "never indexed" the right question
///
/// Every write that produces a search source does so inside the transaction
/// that makes the source durable: `create_session` writes the title row in the
/// same transaction as the session row, and the ledger's appends index the
/// event in the same transaction as the append. So a session either has its
/// title row and an index that kept pace with its ledger, or it predates
/// indexing entirely (migration 0040, or a session created through a path that
/// does not index) and has neither. The anti-join below asks exactly that, one
/// indexed lookup per session — not one per event.
///
/// A session that needs it is then rebuilt whole, deterministically, exactly as
/// the full rebuild would have: its stale rows are deleted and re-derived from
/// its own authoritative rows, inside one transaction.
pub async fn catch_up_search_sources(pool: &SqlitePool) -> Result<usize, SessionLibraryError> {
    // Cheap, read-only, and outside the write lock: the common answer is "no
    // session needs anything", and that answer must not cost a lock.
    let pending: Option<(String,)> = sqlx::query_as(
        "SELECT s.id FROM sessions s WHERE NOT EXISTS \
         (SELECT 1 FROM session_search_sources i \
          WHERE i.session_id = s.id AND i.source_type = 'title') LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    if pending.is_none() {
        return Ok(0);
    }

    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    // Re-read the set under the lock: a session created between the probe and
    // here indexed itself, and one deleted between them must not be indexed at
    // all (its `session_search_sources` rows carry a foreign key to it).
    let stale: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT s.id, s.title, s.updated_at FROM sessions s WHERE NOT EXISTS \
         (SELECT 1 FROM session_search_sources i \
          WHERE i.session_id = s.id AND i.source_type = 'title') ORDER BY s.id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut written = 0_usize;
    for (session_id, title, indexed_at) in stale {
        let session_id: SessionId = session_id.parse().map_err(|error| {
            SessionLibraryError::InvalidData(format!("invalid session id: {error}"))
        })?;
        written += index_session_sources(&mut tx, session_id, &title, &indexed_at).await?;
    }
    tx.commit().await?;
    Ok(written)
}

/// Re-derive every search source for ONE session from its authoritative rows.
///
/// Shared by the whole-history rebuild and the boot catch-up so the two cannot
/// disagree about what a session's sources are.
async fn index_session_sources(
    conn: &mut SqliteConnection,
    session_id: SessionId,
    title: &str,
    title_indexed_at: &str,
) -> Result<usize, SessionLibraryError> {
    sqlx::query("DELETE FROM session_search_sources WHERE session_id = ?")
        .bind(session_id.to_string())
        .execute(&mut *conn)
        .await?;
    index_title_source(&mut *conn, session_id, title, title_indexed_at).await?;
    let mut written = 1;
    let events: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT sequence, body, occurred_at FROM events WHERE session_id = ? ORDER BY sequence",
    )
    .bind(session_id.to_string())
    .fetch_all(&mut *conn)
    .await?;
    for (sequence, body, indexed_at) in events {
        let body = serde_json::from_str(&body).map_err(|error| {
            SessionLibraryError::InvalidData(format!("invalid event body: {error}"))
        })?;
        written +=
            index_event_sources(&mut *conn, session_id, sequence, &body, &indexed_at).await?;
    }
    let commands: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, body, COALESCE(applied_at, received_at) FROM commands \
         WHERE status = 'applied' AND session_id = ? ORDER BY received_at, id",
    )
    .bind(session_id.to_string())
    .fetch_all(&mut *conn)
    .await?;
    for (command_id, body, indexed_at) in commands {
        let command_id = command_id.parse().map_err(|error| {
            SessionLibraryError::InvalidData(format!("invalid command id: {error}"))
        })?;
        let body = serde_json::from_str(&body).map_err(|error| {
            SessionLibraryError::InvalidData(format!("invalid command body: {error}"))
        })?;
        written +=
            index_command_sources(&mut *conn, session_id, command_id, &body, &indexed_at).await?;
    }
    Ok(written)
}

async fn write_source_entry(
    conn: &mut SqliteConnection,
    session_id: SessionId,
    entry: &SearchSourceEntry,
    indexed_at: &str,
) -> Result<(), sqlx::Error> {
    // Event fixtures and imported legacy ledgers can name a run/artifact whose
    // projection row is absent. Preserve the searchable source while only
    // attaching optional foreign-key provenance that is actually resolvable.
    //
    // The two resolutions are correlated subqueries in the INSERT rather than
    // separate round trips: `SELECT id FROM runs WHERE id = ?` yields exactly
    // one row or none, so it produces the id or SQL NULL — the same value the
    // Rust-side `fetch_optional` produced, by the same index seek
    // (`SEARCH runs USING COVERING INDEX sqlite_autoindex_runs_1 (id=?)`), and
    // a bound NULL matches nothing, which is what an absent `entry.run_id`
    // means. Collapsing them costs one statement instead of up to three on a
    // path that runs once per indexable field of every appended event.
    sqlx::query(
        "INSERT INTO session_search_sources \
         (session_id, source_type, source_id, content_hash, indexed_at, \
          event_sequence, run_id, artifact_id) \
         VALUES (?, ?, ?, ?, ?, ?, \
                 (SELECT id FROM runs WHERE id = ?), \
                 (SELECT id FROM artifacts WHERE id = ?)) \
         ON CONFLICT(session_id, source_type, source_id) DO UPDATE SET \
         content_hash = excluded.content_hash, indexed_at = excluded.indexed_at, \
         event_sequence = excluded.event_sequence, run_id = excluded.run_id, \
         artifact_id = excluded.artifact_id",
    )
    .bind(session_id.to_string())
    .bind(entry.source_type)
    .bind(&entry.source_id)
    .bind(&entry.content_hash)
    .bind(indexed_at)
    .bind(entry.event_sequence)
    .bind(entry.run_id.map(|id| id.to_string()))
    .bind(entry.artifact_id.map(|id| id.to_string()))
    .execute(conn)
    .await?;
    Ok(())
}

fn source_entry(
    source_type: &'static str,
    source_id: String,
    content: &str,
    event_sequence: Option<i64>,
    run_id: Option<RunId>,
    artifact_id: Option<ArtifactId>,
) -> SearchSourceEntry {
    SearchSourceEntry {
        source_type,
        source_id,
        content_hash: hex::encode(Sha256::digest(content.as_bytes())),
        event_sequence,
        run_id,
        artifact_id,
    }
}

fn artifact_source_entry(
    sequence: i64,
    artifact: &ArtifactRef,
    run_id: Option<RunId>,
) -> SearchSourceEntry {
    source_entry(
        "artifact",
        format!("event:{sequence}:artifact:{}", artifact.id),
        &format!(
            "{} {} {}",
            artifact.media_type, artifact.id, artifact.sha256
        ),
        Some(sequence),
        run_id,
        Some(artifact.id),
    )
}

fn event_source_entries(sequence: i64, body: &EventBody) -> Vec<SearchSourceEntry> {
    let mut entries = Vec::new();
    match body {
        EventBody::NoteAppended { text, run_id } => entries.push(source_entry(
            "transcript",
            format!("event:{sequence}:transcript"),
            text,
            Some(sequence),
            *run_id,
            None,
        )),
        EventBody::RunStarted {
            run_id, objective, ..
        } => entries.push(source_entry(
            "transcript",
            format!("event:{sequence}:objective"),
            objective,
            Some(sequence),
            Some(*run_id),
            None,
        )),
        // Reasoning is indexed under its own kind rather than as `transcript`,
        // so searching a session's conversation returns what the model SAID and
        // not what it muttered on the way there. Still indexed: the material is
        // real and a search for it should find it, just not mixed in with the
        // reply.
        EventBody::ModelStreamDelta {
            run_id,
            text,
            thought,
        } => entries.push(source_entry(
            if *thought { "reasoning" } else { "transcript" },
            format!("event:{sequence}:model"),
            text,
            Some(sequence),
            Some(*run_id),
            None,
        )),
        EventBody::ToolStarted {
            run_id,
            tool,
            label,
            ..
        } => {
            let content = label
                .as_ref()
                .map_or_else(|| tool.clone(), |label| format!("{tool} {label}"));
            entries.push(source_entry(
                "tool",
                format!("event:{sequence}:tool"),
                &content,
                Some(sequence),
                Some(*run_id),
                None,
            ));
            if let Some(path) = label.as_deref().filter(|label| looks_like_path(label)) {
                entries.push(source_entry(
                    "path",
                    format!("event:{sequence}:path:{path}"),
                    path,
                    Some(sequence),
                    Some(*run_id),
                    None,
                ));
            }
        }
        EventBody::ToolCompleted {
            run_id,
            tool,
            artifact,
            ..
        } => {
            entries.push(source_entry(
                "tool",
                format!("event:{sequence}:tool-result"),
                tool,
                Some(sequence),
                Some(*run_id),
                None,
            ));
            if let Some(artifact) = artifact {
                entries.push(artifact_source_entry(sequence, artifact, Some(*run_id)));
            }
        }
        EventBody::PatchProposed {
            run_id,
            artifact,
            files,
            preview,
            ..
        } => {
            entries.push(source_entry(
                "patch",
                format!("event:{sequence}:patch"),
                preview,
                Some(sequence),
                Some(*run_id),
                None,
            ));
            for path in files {
                entries.push(source_entry(
                    "path",
                    format!("event:{sequence}:path:{path}"),
                    path,
                    Some(sequence),
                    Some(*run_id),
                    None,
                ));
            }
            entries.push(artifact_source_entry(sequence, artifact, Some(*run_id)));
        }
        EventBody::RunCompleted {
            run_id, chronicle, ..
        } => entries.push(artifact_source_entry(sequence, chronicle, Some(*run_id))),
        _ => {}
    }
    entries
}

fn command_source_entries(command_id: CommandId, body: &CommandBody) -> Vec<SearchSourceEntry> {
    let mut entries = Vec::new();
    let CommandBody::SubmitUserInput {
        envelope: Some(envelope),
        ..
    } = body
    else {
        return entries;
    };
    for (index, block) in envelope.blocks.iter().enumerate() {
        match block {
            InputBlock::CodeSymbol(symbol) => entries.push(source_entry(
                "symbol",
                format!("command:{command_id}:symbol:{index}"),
                &format!(
                    "{} {} {}",
                    symbol.symbol,
                    symbol.path,
                    symbol.kind.as_deref().unwrap_or_default()
                ),
                None,
                None,
                None,
            )),
            InputBlock::EditorSelection(selection) => entries.push(source_entry(
                "path",
                format!("command:{command_id}:selection:{index}"),
                &selection.path,
                None,
                None,
                None,
            )),
            _ => {}
        }
    }
    entries
}

/// A row filter that no event or command a Rust-side match could accept will
/// fail — applied in SQL so a keystroke stops loading and JSON-decoding the
/// entire history of every visible session.
///
/// # Why the raw body is filterable at all
///
/// Scoring runs on decoded field text, so the only way to skip a row without
/// decoding it is to match the JSON the row is stored as. That is sound because
/// `serde_json` writes string content verbatim except for `"`, `\` and control
/// characters — a term containing none of those appears in the body exactly as
/// it appears in the field. A term that does contain one produces NO filter and
/// the caller loads everything, rather than silently dropping a result.
///
/// # Why LIKE is not enough on its own
///
/// `LIKE` folds case for ASCII only; scoring folds with Unicode
/// `to_lowercase`. Exactly two non-ASCII characters fold INTO ASCII — U+212A
/// KELVIN SIGN → `k`, and U+0130 (`İ`) → `i` followed by a combining dot — so
/// text containing either scores as a match that `LIKE` would not return. The
/// second variant spells those positions out as `GLOB` character classes, and
/// is reached only for a body that has a non-ASCII character in it at all
/// (`length(text) <> length(bytes)`): `GLOB` with a class per position costs
/// ~5x a `LIKE`, so it is worth guarding rather than running over every row.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BodyPrefilter {
    /// The term cannot be spelled with a folded character, so `LIKE` alone is
    /// already a superset of what scoring accepts.
    Ascii { like: String },
    /// The term contains `i` or `k`, either of which a stored body may spell
    /// with the non-ASCII character that folds to it.
    AsciiOrFolded { like: String, glob: String },
}

impl BodyPrefilter {
    fn like(&self) -> &str {
        match self {
            Self::Ascii { like } | Self::AsciiOrFolded { like, .. } => like,
        }
    }

    fn glob(&self) -> Option<&str> {
        match self {
            Self::Ascii { .. } => None,
            Self::AsciiOrFolded { glob, .. } => Some(glob),
        }
    }
}

/// The filter for one already-lowercased term, or `None` when the term cannot
/// be expressed as one that is guaranteed not to lose a row.
fn term_prefilter(term: &str) -> Option<BodyPrefilter> {
    if term.is_empty() || !term.is_ascii() {
        return None;
    }
    let mut like = String::with_capacity(term.len() + 2);
    let mut glob = String::with_capacity(term.len() * 4 + 2);
    like.push('%');
    glob.push('*');
    let mut folded = false;
    for character in term.chars() {
        if character == '"' || character == '\\' || character.is_ascii_control() {
            return None; // escaped in the stored JSON: no superset exists
        }
        if character == '^' {
            return None; // leads a GLOB class negation; not worth an escape dance
        }
        // `\` is the ESCAPE character on the LIKE side; a term containing one
        // was already refused above.
        if character == '%' || character == '_' {
            like.push('\\');
        }
        like.push(character);

        glob.push('[');
        if character == ']' {
            glob.push(']'); // only legal as the first member of a class
        } else {
            glob.push(character);
            if character.is_ascii_lowercase() {
                glob.push(character.to_ascii_uppercase());
            }
            match character {
                'k' => {
                    glob.push('\u{212a}');
                    folded = true;
                }
                'i' => {
                    glob.push('\u{130}');
                    folded = true;
                }
                _ => {}
            }
        }
        glob.push(']');
    }
    like.push('%');
    glob.push('*');
    Some(if folded {
        BodyPrefilter::AsciiOrFolded { like, glob }
    } else {
        BodyPrefilter::Ascii { like }
    })
}

/// The most selective filter available for `query`, or `None` when no term can
/// be expressed as one.
///
/// Every term has to match for a candidate to score, so filtering on ONE of
/// them keeps every row that could have matched. The longest is chosen because
/// it is the one most likely to be rare.
fn body_prefilter(query: &str) -> Option<BodyPrefilter> {
    query
        .trim()
        .to_lowercase()
        .split_whitespace()
        .filter_map(term_prefilter)
        .max_by_key(|filter| filter.like().len())
}

/// Append the prefilter to a body-bearing query.
fn push_body_prefilter(sql: &mut QueryBuilder<'_, Sqlite>, prefilter: Option<&BodyPrefilter>) {
    let Some(prefilter) = prefilter else {
        return;
    };
    sql.push(" AND (body LIKE ");
    sql.push_bind(prefilter.like().to_string());
    sql.push(" ESCAPE '\\'");
    if let Some(glob) = prefilter.glob() {
        // Only a body that HAS a non-ASCII character can spell the term with a
        // folded one, and that test is far cheaper than the class-wise GLOB.
        sql.push(" OR (length(body) <> length(CAST(body AS BLOB)) AND body GLOB ");
        sql.push_bind(glob.to_string());
        sql.push(")");
    }
    sql.push(")");
}

async fn load_visible_sessions(
    conn: &mut SqliteConnection,
    daemon_uid: u32,
    principal: PeerPrincipal,
    query: &SessionSearchQuery,
) -> Result<Vec<SessionRow>, sqlx::Error> {
    let mut sql = QueryBuilder::<Sqlite>::new(
        "SELECT s.id, s.workspace_id, s.title, s.state, s.updated_at, s.created_at, \
         s.internal, s.parent_session_id, s.parent_run_id, s.pinned, s.archived_at, \
         s.repository_id, s.repository, s.workspace, \
         COALESCE(s.last_activity_at, \
             (SELECT MAX(e.occurred_at) FROM events e WHERE e.session_id = s.id)) \
             AS last_activity_at, \
         COALESCE(s.last_run_id, \
             (SELECT r.id FROM runs r WHERE r.session_id = s.id ORDER BY r.rowid DESC LIMIT 1)) \
             AS last_run_id, \
         COALESCE(s.run_state, \
             (SELECT r.state FROM runs r WHERE r.session_id = s.id ORDER BY r.rowid DESC LIMIT 1)) \
             AS run_state \
         FROM sessions s WHERE COALESCE(s.owner_uid, ",
    );
    sql.push_bind(i64::from(daemon_uid));
    sql.push(") = ");
    sql.push_bind(i64::from(principal.uid()));
    sql.push(" AND s.tombstoned_at IS NULL");
    if query.query.trim().is_empty() {
        sql.push(" AND s.internal = 0 AND s.archived_at IS NULL");
    }

    if !query.filters.repository_ids.is_empty() {
        sql.push(" AND s.repository_id IN (");
        let mut values = sql.separated(", ");
        for repository_id in &query.filters.repository_ids {
            values.push_bind(repository_id.to_string());
        }
        values.push_unseparated(")");
    }
    if let Some(after) = query.filters.created_after {
        sql.push(" AND s.created_at >= ");
        sql.push_bind(after.to_rfc3339());
    }
    if let Some(before) = query.filters.created_before {
        sql.push(" AND s.created_at <= ");
        sql.push_bind(before.to_rfc3339());
    }
    if !query.filters.run_states.is_empty() {
        sql.push(
            " AND COALESCE(s.run_state, \
             (SELECT r.state FROM runs r WHERE r.session_id = s.id \
              ORDER BY r.rowid DESC LIMIT 1)) IN (",
        );
        let mut values = sql.separated(", ");
        for state in &query.filters.run_states {
            values.push_bind(run_state_to_db(*state));
        }
        values.push_unseparated(")");
    }
    if !query.filters.workflow_ids.is_empty() {
        sql.push(
            " AND EXISTS (SELECT 1 FROM workflow_runs wr JOIN runs r ON r.id = wr.run_id \
             WHERE r.session_id = s.id AND wr.workflow_id IN (",
        );
        let mut values = sql.separated(", ");
        for workflow_id in &query.filters.workflow_ids {
            values.push_bind(workflow_id.to_string());
        }
        values.push_unseparated("))");
    }
    if !query.filters.model_ids.is_empty() {
        sql.push(
            " AND EXISTS (SELECT 1 FROM model_task_outcomes mo JOIN runs r ON r.id = mo.run_id \
             WHERE r.session_id = s.id AND mo.model_id IN (",
        );
        let mut values = sql.separated(", ");
        for model_id in &query.filters.model_ids {
            values.push_bind(model_id.to_string());
        }
        values.push_unseparated("))");
    }

    sql.build_query_as::<SessionRow>()
        .fetch_all(&mut *conn)
        .await
}

/// Load the durable events that could match, NOT every event ever appended.
///
/// The row filter is in SQL because it has to be: without it this loaded and
/// JSON-decoded the entire history of every visible session on every keystroke,
/// paged or not — the desktop and the TUI both drive this per keypress. The
/// filter is a superset of what scoring accepts (see
/// [`body_prefilter_pattern`]), so which rows SCORE is decided in exactly the
/// same place as before; only the rows that could never score are left in the
/// database.
async fn load_visible_events(
    conn: &mut SqliteConnection,
    sessions: &[SessionSummary],
    prefilter: Option<&BodyPrefilter>,
    skip_transcripts: bool,
) -> Result<Vec<EventRow>, sqlx::Error> {
    let mut events = Vec::new();
    // Stay comfortably below SQLite's host-parameter ceiling even for a large
    // library. Ownership has already been applied to `sessions`; only those ids
    // are ever materialized here.
    for chunk in sessions.chunks(400) {
        let mut sql = QueryBuilder::<Sqlite>::new(
            "SELECT session_id, sequence, body FROM events WHERE session_id IN (",
        );
        let mut ids = sql.separated(", ");
        for session in chunk {
            ids.push_bind(session.session_id.to_string());
        }
        ids.push_unseparated(")");
        if skip_transcripts {
            sql.push(
                " AND json_extract(body, '$.type') NOT IN ('NoteAppended', 'ModelStreamDelta')",
            );
        }
        push_body_prefilter(&mut sql, prefilter);
        sql.push(" ORDER BY session_id, sequence");
        events.extend(
            sql.build_query_as::<EventRow>()
                .fetch_all(&mut *conn)
                .await?,
        );
    }
    Ok(events)
}

async fn load_visible_commands(
    conn: &mut SqliteConnection,
    sessions: &[SessionSummary],
    prefilter: Option<&BodyPrefilter>,
) -> Result<Vec<CommandRow>, sqlx::Error> {
    let mut commands = Vec::new();
    for chunk in sessions.chunks(400) {
        let mut sql = QueryBuilder::<Sqlite>::new(
            "SELECT id, session_id, body FROM commands WHERE status = 'applied' \
             AND session_id IN (",
        );
        let mut ids = sql.separated(", ");
        for session in chunk {
            ids.push_bind(session.session_id.to_string());
        }
        ids.push_unseparated(")");
        push_body_prefilter(&mut sql, prefilter);
        sql.push(" ORDER BY session_id, received_at, id");
        commands.extend(
            sql.build_query_as::<CommandRow>()
                .fetch_all(&mut *conn)
                .await?,
        );
    }
    Ok(commands)
}

fn title_candidate(session: SessionSummary, raw_query: &str) -> Option<Candidate> {
    let score = title_score(&session.title, raw_query)?;
    let stable_identity = format!("session:{}:title", session.session_id);
    let updated_at = session.updated_at;
    let pinned = session.pinned;
    Some(Candidate {
        result: SessionSearchResult {
            deep_link: SessionDeepLink::Session {
                session_id: session.session_id,
            },
            session,
            source: SessionSearchSource::Title,
            scope: SessionSearchScope::Session,
            stable_identity,
            score,
            excerpt: None,
        },
        pinned,
        updated_at,
    })
}

fn event_candidates(
    session: &SessionSummary,
    sequence: u64,
    body: &EventBody,
    query: &str,
) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let event_link = SessionDeepLink::Event {
        session_id: session.session_id,
        sequence,
    };
    match body {
        EventBody::NoteAppended { text, .. } => push_text_candidate(
            &mut candidates,
            session,
            SessionSearchSource::Transcript,
            format!("event:{sequence}:transcript"),
            event_link,
            text,
            query,
            NOTE_BASE_SCORE,
        ),
        EventBody::RunStarted { objective, .. } => push_text_candidate(
            &mut candidates,
            session,
            SessionSearchSource::Transcript,
            format!("event:{sequence}:objective"),
            event_link,
            objective,
            query,
            650.0,
        ),
        EventBody::ModelStreamDelta { text, .. } => push_text_candidate(
            &mut candidates,
            session,
            SessionSearchSource::Transcript,
            format!("event:{sequence}:model"),
            event_link,
            text,
            query,
            MODEL_STREAM_BASE_SCORE,
        ),
        EventBody::ToolStarted { tool, label, .. } => {
            let text = label
                .as_ref()
                .map_or_else(|| tool.clone(), |label| format!("{tool} {label}"));
            push_text_candidate(
                &mut candidates,
                session,
                SessionSearchSource::ToolObservation,
                format!("event:{sequence}:tool"),
                event_link.clone(),
                &text,
                query,
                700.0,
            );
            if let Some(path) = label.as_deref().filter(|label| looks_like_path(label)) {
                push_text_candidate(
                    &mut candidates,
                    session,
                    SessionSearchSource::ChangedPath,
                    format!("event:{sequence}:path:{path}"),
                    SessionDeepLink::Path {
                        session_id: session.session_id,
                        path: path.to_string(),
                        line: None,
                        column: None,
                    },
                    path,
                    query,
                    750.0,
                );
            }
        }
        EventBody::ToolCompleted { tool, artifact, .. } => {
            push_text_candidate(
                &mut candidates,
                session,
                SessionSearchSource::ToolObservation,
                format!("event:{sequence}:tool-result"),
                event_link,
                tool,
                query,
                680.0,
            );
            if let Some(artifact) = artifact {
                push_artifact_candidate(&mut candidates, session, sequence, artifact, query);
            }
        }
        EventBody::PatchProposed {
            artifact,
            files,
            preview,
            ..
        } => {
            push_text_candidate(
                &mut candidates,
                session,
                SessionSearchSource::Patch,
                format!("event:{sequence}:patch"),
                event_link,
                preview,
                query,
                800.0,
            );
            for path in files {
                push_text_candidate(
                    &mut candidates,
                    session,
                    SessionSearchSource::ChangedPath,
                    format!("event:{sequence}:path:{path}"),
                    SessionDeepLink::Path {
                        session_id: session.session_id,
                        path: path.clone(),
                        line: None,
                        column: None,
                    },
                    path,
                    query,
                    750.0,
                );
            }
            push_artifact_candidate(&mut candidates, session, sequence, artifact, query);
        }
        EventBody::RunCompleted { chronicle, .. } => {
            push_artifact_candidate(&mut candidates, session, sequence, chronicle, query);
        }
        _ => {}
    }
    candidates
}

fn command_candidates(
    session: &SessionSummary,
    command_id: &str,
    body: &CommandBody,
    query: &str,
) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let CommandBody::SubmitUserInput {
        envelope: Some(envelope),
        ..
    } = body
    else {
        return candidates;
    };
    for (index, block) in envelope.blocks.iter().enumerate() {
        match block {
            InputBlock::CodeSymbol(symbol) => {
                let text = format!(
                    "{} {} {}",
                    symbol.symbol,
                    symbol.path,
                    symbol.kind.as_deref().unwrap_or_default()
                );
                push_text_candidate(
                    &mut candidates,
                    session,
                    SessionSearchSource::Symbol,
                    format!("command:{command_id}:symbol:{index}"),
                    SessionDeepLink::Symbol {
                        session_id: session.session_id,
                        symbol: symbol.symbol.clone(),
                        path: Some(symbol.path.clone()),
                    },
                    &text,
                    query,
                    850.0,
                );
            }
            InputBlock::EditorSelection(selection) => push_text_candidate(
                &mut candidates,
                session,
                SessionSearchSource::ChangedPath,
                format!("command:{command_id}:selection:{index}"),
                SessionDeepLink::Path {
                    session_id: session.session_id,
                    path: selection.path.clone(),
                    line: Some(selection.range.start.line),
                    column: Some(selection.range.start.character),
                },
                &selection.path,
                query,
                750.0,
            ),
            _ => {}
        }
    }
    candidates
}

#[allow(clippy::too_many_arguments)]
fn push_text_candidate(
    candidates: &mut Vec<Candidate>,
    session: &SessionSummary,
    source: SessionSearchSource,
    identity_suffix: String,
    deep_link: SessionDeepLink,
    text: &str,
    query: &str,
    base_score: f64,
) {
    let Some(score) = text_score(text, query, base_score) else {
        return;
    };
    candidates.push(Candidate {
        result: SessionSearchResult {
            session: session.clone(),
            source,
            scope: SessionSearchScope::Session,
            stable_identity: format!("session:{}:{identity_suffix}", session.session_id),
            deep_link,
            score,
            excerpt: Some(bounded_excerpt(text)),
        },
        pinned: session.pinned,
        updated_at: session.updated_at,
    });
}

fn push_artifact_candidate(
    candidates: &mut Vec<Candidate>,
    session: &SessionSummary,
    sequence: u64,
    artifact: &ArtifactRef,
    query: &str,
) {
    let text = format!(
        "{} {} {}",
        artifact.media_type, artifact.id, artifact.sha256
    );
    push_text_candidate(
        candidates,
        session,
        SessionSearchSource::Artifact,
        format!("event:{sequence}:artifact:{}", artifact.id),
        SessionDeepLink::Artifact {
            session_id: session.session_id,
            artifact_id: artifact.id,
        },
        &text,
        query,
        500.0,
    );
}

fn text_score(text: &str, query: &str, base: f64) -> Option<f64> {
    let text = text.to_lowercase();
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return None;
    }
    let terms = query.split_whitespace().collect::<Vec<_>>();
    if terms.iter().any(|term| !text.contains(term)) {
        return None;
    }
    let quality = if text == query {
        100.0
    } else if text.starts_with(&query) {
        80.0
    } else if text.contains(&query) {
        60.0
    } else {
        40.0 + terms.len() as f64
    };
    Some(base + quality)
}

fn text_score_ceiling(base: f64, query: &str) -> f64 {
    // Discontiguous multi-term matches use 40 + term count, which can exceed
    // the exact-match bonus for long queries. Keep pruning conservative there.
    base + 100.0_f64.max(40.0 + query.split_whitespace().count() as f64)
}

fn bounded_excerpt(text: &str) -> String {
    const MAX_EXCERPT_CHARS: usize = 240;
    let mut excerpt = text.chars().take(MAX_EXCERPT_CHARS).collect::<String>();
    if text.chars().count() > MAX_EXCERPT_CHARS {
        excerpt.push('…');
    }
    excerpt
}

fn looks_like_path(label: &str) -> bool {
    label.contains('/') || label.contains('\\')
}

fn title_score(title: &str, query: &str) -> Option<f64> {
    let title = title.to_lowercase();
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(0.0);
    }
    let terms = query.split_whitespace().collect::<Vec<_>>();
    if terms.iter().any(|term| !title.contains(term)) {
        return None;
    }
    if title == query {
        return Some(1_000.0);
    }
    if title.starts_with(&query) {
        return Some(800.0);
    }
    if title
        .split(|character: char| !character.is_alphanumeric())
        .any(|token| token == query)
    {
        return Some(700.0);
    }
    Some(500.0 + terms.len() as f64)
}

fn compare_candidates(left: &Candidate, right: &Candidate) -> Ordering {
    right
        .result
        .score
        .total_cmp(&left.result.score)
        .then_with(|| right.pinned.cmp(&left.pinned))
        .then_with(|| right.updated_at.cmp(&left.updated_at))
        .then_with(|| {
            left.result
                .stable_identity
                .cmp(&right.result.stable_identity)
        })
}

fn candidate_is_after(candidate: &Candidate, cursor: &SearchCursor) -> bool {
    let boundary_score = f64::from_bits(cursor.score_bits);
    match candidate.result.score.total_cmp(&boundary_score) {
        Ordering::Greater => false,
        Ordering::Less => true,
        Ordering::Equal => match candidate.pinned.cmp(&cursor.pinned) {
            Ordering::Greater => false,
            Ordering::Less => true,
            Ordering::Equal => match candidate.updated_at.cmp(&cursor.updated_at) {
                Ordering::Greater => false,
                Ordering::Less => true,
                Ordering::Equal => candidate.result.stable_identity > cursor.stable_identity,
            },
        },
    }
}

fn query_hash(
    principal_uid: u32,
    query: &SessionSearchQuery,
) -> Result<String, SessionLibraryError> {
    let payload = serde_json::to_vec(&(principal_uid, &query.query, &query.filters))
        .map_err(|error| SessionLibraryError::InvalidData(error.to_string()))?;
    Ok(hex::encode(Sha256::digest(payload)))
}

fn encode_cursor(
    candidate: &Candidate,
    query_hash: &str,
) -> Result<PageCursor, SessionLibraryError> {
    let cursor = SearchCursor {
        version: CURSOR_VERSION,
        query_hash: query_hash.to_string(),
        score_bits: candidate.result.score.to_bits(),
        pinned: candidate.pinned,
        updated_at: candidate.updated_at,
        stable_identity: candidate.result.stable_identity.clone(),
    };
    let bytes = serde_json::to_vec(&cursor)
        .map_err(|error| SessionLibraryError::InvalidData(error.to_string()))?;
    Ok(PageCursor(URL_SAFE_NO_PAD.encode(bytes)))
}

fn decode_cursor(
    cursor: &PageCursor,
    expected_query_hash: &str,
) -> Result<SearchCursor, SessionLibraryError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(&cursor.0)
        .map_err(|_| SessionLibraryError::InvalidCursor)?;
    let decoded: SearchCursor =
        serde_json::from_slice(&bytes).map_err(|_| SessionLibraryError::InvalidCursor)?;
    if decoded.version != CURSOR_VERSION
        || decoded.query_hash != expected_query_hash
        || !f64::from_bits(decoded.score_bits).is_finite()
    {
        return Err(SessionLibraryError::InvalidCursor);
    }
    Ok(decoded)
}

fn parse_time(value: &str, field: &str) -> Result<DateTime<Utc>, SessionLibraryError> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| SessionLibraryError::InvalidData(format!("invalid {field}: {error}")))
}

fn parse_optional<T>(value: Option<String>, field: &str) -> Result<Option<T>, SessionLibraryError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .map(|raw| {
            raw.parse().map_err(|error| {
                SessionLibraryError::InvalidData(format!("invalid {field}: {error}"))
            })
        })
        .transpose()
}

impl TryFrom<SessionRow> for SessionSummary {
    type Error = SessionLibraryError;

    fn try_from(row: SessionRow) -> Result<Self, Self::Error> {
        Ok(Self {
            session_id: row.id.parse().map_err(|error| {
                SessionLibraryError::InvalidData(format!("invalid session id: {error}"))
            })?,
            workspace_id: parse_optional::<WorkspaceId>(row.workspace_id, "workspace id")?,
            title: row.title,
            state: row.state,
            updated_at: parse_time(&row.updated_at, "updated_at")?,
            created_at: parse_time(&row.created_at, "created_at")?,
            internal: row.internal != 0,
            parent_session_id: parse_optional::<SessionId>(
                row.parent_session_id,
                "parent session id",
            )?,
            parent_run_id: parse_optional::<RunId>(row.parent_run_id, "parent run id")?,
            pinned: row.pinned != 0,
            archived_at: row
                .archived_at
                .as_deref()
                .map(|value| parse_time(value, "archived_at"))
                .transpose()?,
            repository_id: parse_optional::<RepositoryId>(row.repository_id, "repository id")?,
            repository: row.repository,
            workspace: row.workspace,
            last_activity_at: row
                .last_activity_at
                .as_deref()
                .map(|value| parse_time(value, "last_activity_at"))
                .transpose()?,
            last_run_id: parse_optional::<RunId>(row.last_run_id, "last run id")?,
            run_state: row
                .run_state
                .as_deref()
                .map(run_state_from_db)
                .transpose()?,
        })
    }
}

fn run_state_to_db(state: RunState) -> &'static str {
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
        _ => "Unknown",
    }
}

fn run_state_from_db(value: &str) -> Result<RunState, SessionLibraryError> {
    match value {
        "Queued" => Ok(RunState::Queued),
        "Preparing" => Ok(RunState::Preparing),
        "Running" => Ok(RunState::Running),
        "WaitingForApproval" => Ok(RunState::WaitingForApproval),
        "WaitingForUserInput" => Ok(RunState::WaitingForUserInput),
        "Paused" => Ok(RunState::Paused),
        "Recovering" => Ok(RunState::Recovering),
        "Completed" => Ok(RunState::Completed),
        "Failed" => Ok(RunState::Failed),
        "Cancelled" => Ok(RunState::Cancelled),
        "Unknown" => Ok(RunState::Unknown),
        other => Err(SessionLibraryError::InvalidData(format!(
            "invalid run state {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        body_prefilter, load_visible_events, load_visible_sessions, text_score, text_score_ceiling,
        BodyPrefilter, NOTE_BASE_SCORE,
    };
    use crate::principal::PeerPrincipal;
    use codypendent_protocol::{
        EventBody, RunId, SessionId, SessionSearchFilters, SessionSearchQuery, SessionSummary,
    };

    fn query(text: &str) -> SessionSearchQuery {
        SessionSearchQuery {
            query: text.to_string(),
            filters: SessionSearchFilters::default(),
            limit: 50,
            cursor: None,
        }
    }

    #[test]
    fn transcript_score_ceiling_covers_long_discontiguous_queries() {
        let query = std::iter::repeat_n("needle", 200)
            .collect::<Vec<_>>()
            .join(" ");
        let score = text_score("needle x", &query, NOTE_BASE_SCORE).unwrap();
        assert_eq!(score, 840.0);
        assert_eq!(score, text_score_ceiling(NOTE_BASE_SCORE, &query));
        assert_eq!(text_score_ceiling(NOTE_BASE_SCORE, "needle"), 700.0);
    }

    /// The filter exists to skip rows, so what it must never do is skip one that
    /// would have scored. `LIKE` folds case for ASCII only, and exactly two
    /// non-ASCII characters fold INTO ASCII, so a term that could be spelled
    /// with one carries the `GLOB` alternative as well.
    #[test]
    fn a_term_that_can_be_spelled_with_a_folded_character_keeps_the_glob_alternative() {
        let plain = body_prefilter("parser").expect("an ordinary term filters");
        assert!(
            matches!(plain, BodyPrefilter::Ascii { .. }),
            "no letter here folds from outside ASCII: {plain:?}"
        );

        for term in ["kelvin", "index"] {
            let filter = body_prefilter(term).expect("filters");
            let glob = filter
                .glob()
                .unwrap_or_else(|| panic!("`{term}` must keep the folded alternative"));
            assert!(
                glob.contains('\u{212a}') || glob.contains('\u{130}'),
                "the folded character is spelled out: {glob}"
            );
        }
    }

    /// A term the stored JSON would have escaped cannot be matched against the
    /// raw body at all, and guessing would drop results — so it filters nothing
    /// and the caller reads everything, which is only slow.
    #[test]
    fn a_term_that_json_escapes_produces_no_filter_rather_than_a_wrong_one() {
        assert_eq!(body_prefilter("\"quoted\""), None);
        assert_eq!(body_prefilter("back\\slash"), None);
        assert_eq!(body_prefilter("naïve"), None);
        // A multi-term query keeps whichever term IS expressible: every term
        // has to match, so filtering on one of them loses nothing.
        assert_eq!(
            body_prefilter("parser \"quoted\"").expect("filters").like(),
            "%parser%"
        );
        // A wildcard is escaped, not passed through as a wildcard.
        let percent = body_prefilter("100%").expect("filters");
        assert_eq!(percent.like(), "%100\\%%");
    }

    /// The point of the filter: a keystroke reads the rows that could match,
    /// not the entire history of every visible session.
    #[tokio::test]
    async fn the_event_loader_reads_only_rows_that_could_match() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = crate::db::open_database(&temp.path().join("library.db"))
            .await
            .expect("open database");
        let session = SessionId::new();
        sqlx::query(
            "INSERT INTO sessions (id, title, state, created_at, updated_at, revision, owner_uid) \
             VALUES (?, 'session', 'open', ?, ?, 0, 1000)",
        )
        .bind(session.to_string())
        .bind("2026-08-17T10:00:00Z")
        .bind("2026-08-17T10:00:00Z")
        .execute(&pool)
        .await
        .expect("session");
        let total = 500;
        for sequence in 1..=total {
            let body = EventBody::ModelStreamDelta {
                run_id: RunId::new(),
                text: if sequence == 42 {
                    "the needleword is here".to_string()
                } else {
                    format!("ordinary streamed fragment {sequence}")
                },
                thought: false,
            };
            sqlx::query(
                "INSERT INTO events \
                 (session_id, sequence, occurred_at, actor, body, schema_version) \
                 VALUES (?, ?, ?, ?, ?, 1)",
            )
            .bind(session.to_string())
            .bind(sequence)
            .bind("2026-08-17T10:00:00Z")
            .bind(r#""System""#)
            .bind(serde_json::to_string(&body).expect("body"))
            .execute(&pool)
            .await
            .expect("event");
        }

        let search = query("needleword");
        let mut conn = pool.acquire().await.expect("connection");
        let sessions =
            load_visible_sessions(&mut conn, 1000, PeerPrincipal::from_uid(1000), &search)
                .await
                .expect("sessions")
                .into_iter()
                .map(SessionSummary::try_from)
                .collect::<Result<Vec<_>, _>>()
                .expect("summaries");

        let filtered = load_visible_events(
            &mut conn,
            &sessions,
            body_prefilter(&search.query).as_ref(),
            false,
        )
        .await
        .expect("events");
        assert_eq!(
            filtered.len(),
            1,
            "only the one event that can score is read, not all {total}"
        );

        // And with no filter the loader still reads everything, which is what
        // makes the filter — not a changed query — the thing being measured.
        let everything = load_visible_events(&mut conn, &sessions, None, false)
            .await
            .expect("events");
        assert_eq!(everything.len(), total as usize);
        let pruned = load_visible_events(&mut conn, &sessions, None, true)
            .await
            .expect("rank-pruned events");
        assert!(pruned.is_empty(), "low-ranked streams stay in SQLite");
    }
}
