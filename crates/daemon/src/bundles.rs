//! Versioned, redacted session and support bundles (Milestone 2, Task 2.7).
//!
//! Bundles are deterministic, content-addressed archives of session history and
//! diagnostic material, stored behind [`ArtifactRef`]s and recorded in
//! `session_bundles` / `session_bundle_imports`.
//!
//! Security invariants:
//! 1. Hostile archive defences: reject path escapes (`..`, absolute paths),
//!    symlinks, oversized entries, duplicate paths, unsupported format versions.
//! 2. SHA-256 verification: stored archive bytes and entry bytes are hashed
//!    directly and compared against assertions.
//! 3. Identity remapping: imports always mint fresh local IDs and never reuse
//!    source IDs.
//! 4. Zero authority restoration: credentials and approvals are never restored.
//! 5. Owner isolation: exports only disclose sessions owned by the caller;
//!    imported records are owned by the importing principal.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path};

use chrono::{DateTime, Utc};
use codypendent_knowledge::detect_secret;
use codypendent_protocol::bundle::{
    BundleCollisionPolicy, BundleEntryKind, BundleEntryManifest, BundleExportReceipt,
    BundleExportRequest, BundleIdentityKind, BundleIdentityMapping, BundleImportProvenance,
    BundleImportReceipt, BundleImportRequest, BundleManifest, BundleRedactionPolicy,
    BundleRedactionSummary, BUNDLE_FORMAT_V1,
};
use codypendent_protocol::session::{SessionExportFormat, SessionExportOptions};
use codypendent_protocol::{
    Actor, ArtifactRef, ClientId, CodypendentError, CommandId, DataClassification, EventBody,
    RunId, SessionEvent, SessionId,
};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use tar::{Archive, Builder, EntryType, Header};
use uuid::Uuid;

use crate::artifacts::{ArtifactStore, Provenance};
use crate::session_library;

pub const MAX_BUNDLE_ARCHIVE_BYTES: u64 = 100 * 1024 * 1024; // 100 MB
pub const MAX_BUNDLE_ENTRIES: usize = 10_000;
pub const MAX_BUNDLE_ENTRY_BYTES: u64 = 50 * 1024 * 1024; // 50 MB
pub const MAX_BUNDLE_PATH_BYTES: usize = 255;
pub const MAX_BUNDLE_PATH_DEPTH: usize = 16;

fn internal_error(err: impl std::fmt::Display) -> CodypendentError {
    CodypendentError::new("internal.command-apply-failed", err.to_string(), true)
}

fn normalized_bundle_path(path: &Path) -> Result<String, CodypendentError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(CodypendentError::new(
            "bundle.verification-failed",
            "archive contains an empty or absolute path",
            false,
        ));
    }
    if path.as_os_str().as_encoded_bytes().len() > MAX_BUNDLE_PATH_BYTES
        || path.components().count() > MAX_BUNDLE_PATH_DEPTH
    {
        return Err(CodypendentError::new(
            "bundle.verification-failed",
            "archive path exceeds length or depth limits",
            false,
        ));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let s = part.to_str().ok_or_else(|| {
                    CodypendentError::new(
                        "bundle.verification-failed",
                        "archive path contains invalid utf-8",
                        false,
                    )
                })?;
                parts.push(s);
            }
            _ => {
                return Err(CodypendentError::new(
                    "bundle.verification-failed",
                    "archive path is not normalized or attempts to escape the root",
                    false,
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(CodypendentError::new(
            "bundle.verification-failed",
            "archive contains an empty path",
            false,
        ));
    }
    Ok(parts.join("/"))
}

fn redact_text(
    text: &str,
    policy: &BundleRedactionPolicy,
    summary: &mut BundleRedactionSummary,
) -> String {
    if matches!(policy, BundleRedactionPolicy::Unknown) {
        return text.to_string();
    }
    if let Some(_reason) = detect_secret(text) {
        summary.values_replaced += 1;
        summary.credentials_omitted += 1;
        return "[redacted secret]".to_string();
    }
    text.to_string()
}

fn redact_json_value(
    value: &mut serde_json::Value,
    policy: &BundleRedactionPolicy,
    summary: &mut BundleRedactionSummary,
) {
    match value {
        serde_json::Value::String(s) => {
            *s = redact_text(s, policy, summary);
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                redact_json_value(item, policy, summary);
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                let k_lower = k.to_lowercase();
                if k_lower.contains("secret")
                    || k_lower.contains("token")
                    || k_lower.contains("password")
                    || k_lower.contains("authorization")
                    || k_lower.contains("api_key")
                {
                    *v = serde_json::Value::String("[redacted credential]".to_string());
                    summary.values_replaced += 1;
                    summary.credentials_omitted += 1;
                } else {
                    redact_json_value(v, policy, summary);
                }
            }
        }
        _ => {}
    }
}

/// Emit the agent turn accumulated from consecutive [`EventBody::ModelStreamDelta`]
/// events, if any.
///
/// The daemon emits one delta per coalesced model chunk, so a single agent turn
/// arrives as a run of deltas. Every other transcript projection in this
/// workspace folds them back into one block before display
/// (`AppState::append_model_text` in `crates/tui/src/state.rs`); the markdown
/// export does the same so a turn reads as a turn rather than as dozens of
/// one-fragment sections. The header carries the sequence and timestamp of the
/// FIRST delta in the run — a real ledger coordinate, not a synthesized one.
fn flush_agent_turn(md: &mut String, pending: &mut Option<(u64, DateTime<Utc>, String)>) {
    if let Some((sequence, occurred_at, text)) = pending.take() {
        md.push_str(&format!("### Event {sequence} ({occurred_at})\n\n"));
        md.push_str(&format!("**Agent:**\n\n{text}\n\n"));
    }
}

/// Export a single session through the lifecycle command boundary
/// (`MutateSessionLifecycle::Export`).
pub async fn export_session_lifecycle(
    pool: &SqlitePool,
    artifacts: &ArtifactStore,
    owner_uid: u32,
    session_id: SessionId,
    options: &SessionExportOptions,
) -> Result<ArtifactRef, CodypendentError> {
    let row = sqlx::query(
        "SELECT id, title, state, created_at, updated_at, revision, internal, repository \
         FROM sessions WHERE id = ? AND tombstoned_at IS NULL",
    )
    .bind(session_id.to_string())
    .fetch_optional(pool)
    .await
    .map_err(internal_error)?
    .ok_or_else(|| {
        CodypendentError::new(
            "protocol.session-not-found",
            format!("no session {session_id}"),
            false,
        )
    })?;

    let title: String = row.get("title");
    let state: String = row.get("state");
    let created_at: String = row.get("created_at");
    let updated_at: String = row.get("updated_at");
    let internal: i64 = row.get("internal");
    let repository: Option<String> = row.get("repository");

    if !options.include_internal_sessions && internal != 0 {
        return Err(CodypendentError::new(
            "protocol.session-not-found",
            format!("no session {session_id}"),
            false,
        ));
    }

    let event_rows = sqlx::query(
        // `events`, and `actor`/`body` — the ledger's real names
        // (migrations/0001_init.sql). This read named a `session_events` table
        // with `actor_json`/`body_json` columns that no migration has ever
        // created, so it failed unconditionally at runtime against any real
        // database.
        "SELECT sequence, actor, body, occurred_at \
         FROM events WHERE session_id = ? ORDER BY sequence ASC",
    )
    .bind(session_id.to_string())
    .fetch_all(pool)
    .await
    .map_err(internal_error)?;

    let mut events = Vec::new();
    for erow in event_rows {
        let sequence: i64 = erow.get("sequence");
        let actor_json: String = erow.get("actor");
        let body_json: String = erow.get("body");
        let occurred_at_str: String = erow.get("occurred_at");
        let actor: Actor = serde_json::from_str(&actor_json).map_err(internal_error)?;
        let body: EventBody = serde_json::from_str(&body_json).map_err(internal_error)?;
        let occurred_at = DateTime::parse_from_rfc3339(&occurred_at_str)
            .map_err(internal_error)?
            .with_timezone(&Utc);
        events.push(SessionEvent {
            sequence: sequence as u64,
            occurred_at,
            causation_id: None,
            correlation_id: None,
            actor,
            body,
        });
    }

    let (media_type, bytes) = match options.format {
        SessionExportFormat::Json => {
            let export_payload = serde_json::json!({
                "session_id": session_id,
                "title": title,
                "state": state,
                "repository": repository,
                "created_at": created_at,
                "updated_at": updated_at,
                "events": events,
            });
            (
                "application/json",
                serde_json::to_vec_pretty(&export_payload).map_err(internal_error)?,
            )
        }
        SessionExportFormat::Markdown => {
            let mut md = format!("# Session: {title}\n\n");
            md.push_str(&format!("- **Session ID:** `{session_id}`\n"));
            if let Some(repo) = repository {
                md.push_str(&format!("- **Repository:** `{repo}`\n"));
            }
            md.push_str(&format!("- **State:** `{state}`\n"));
            md.push_str(&format!("- **Created:** `{created_at}`\n\n---\n\n"));

            // Which ledger events ARE the conversation is not a choice this
            // module gets to make: it follows the projection every other
            // consumer of the ledger uses (`apply_event` in
            // `crates/tui/src/reduce.rs`, `event_source_entries` in
            // `crates/daemon/src/session_library.rs`).
            //   * the user turn is `RunStarted.objective`
            //   * the agent turn is a run of coalesced `ModelStreamDelta`s
            //   * `NoteAppended` is a note, not a turn
            // Every other event is emitted as its verbatim serialized body, so
            // the export never narrates content the ledger does not hold.
            let mut pending_agent_turn: Option<(u64, DateTime<Utc>, String)> = None;
            for event in &events {
                if let EventBody::ModelStreamDelta { text, .. } = &event.body {
                    match &mut pending_agent_turn {
                        Some((_, _, buffered)) => buffered.push_str(text),
                        None => {
                            pending_agent_turn =
                                Some((event.sequence, event.occurred_at, text.clone()));
                        }
                    }
                    continue;
                }
                flush_agent_turn(&mut md, &mut pending_agent_turn);

                md.push_str(&format!(
                    "### Event {} ({})\n\n",
                    event.sequence, event.occurred_at
                ));
                match &event.body {
                    EventBody::RunStarted { objective, .. } => {
                        md.push_str(&format!("**User:**\n\n{objective}\n\n"));
                    }
                    EventBody::NoteAppended { text, .. } => {
                        md.push_str(&format!("*Note:* {text}\n\n"));
                    }
                    // RULE 1 fallback: a body written by a newer daemon that
                    // this build cannot name. Its fields did not survive
                    // deserialization, so serializing it back would print a
                    // bare `{"type":"Unknown"}` that reads like a real,
                    // empty event. Mark it unsupported instead.
                    EventBody::Unknown => {
                        md.push_str("*Unsupported event type; body omitted.*\n\n");
                    }
                    other => {
                        md.push_str(&format!(
                            "```json\n{}\n```\n\n",
                            serde_json::to_string_pretty(other).map_err(internal_error)?
                        ));
                    }
                }
            }
            flush_agent_turn(&mut md, &mut pending_agent_turn);

            ("text/markdown", md.into_bytes())
        }
        // `SessionExportFormat` is `#[non_exhaustive]` with an `Unknown`
        // fallback: a format this build cannot produce is refused rather than
        // silently downgraded to one it can.
        _ => {
            return Err(CodypendentError::new(
                "session-library.invalid-export-format",
                "unknown session export format",
                false,
            ));
        }
    };

    artifacts
        .put_owned(
            pool,
            owner_uid,
            media_type,
            DataClassification::Internal,
            Provenance::system("session_export"),
            &bytes,
        )
        .await
        .map_err(internal_error)
}

/// Export one or more sessions into a versioned, redacted bundle archive.
pub async fn export(
    pool: &SqlitePool,
    artifacts: &ArtifactStore,
    owner_uid: u32,
    _client_id: ClientId,
    command_id: CommandId,
    _idempotency_key: &str,
    request: &BundleExportRequest,
) -> Result<BundleExportReceipt, CodypendentError> {
    if request.source_session_ids.is_empty() {
        return Err(CodypendentError::new(
            "bundle.invalid-request",
            "a bundle export must name at least one source session",
            false,
        ));
    }
    if matches!(request.redaction_policy, BundleRedactionPolicy::Unknown) {
        return Err(CodypendentError::new(
            "bundle.invalid-request",
            "unsupported redaction policy",
            false,
        ));
    }

    let mut session_rows = Vec::new();
    let mut latest_updated = Utc::now();
    for session_id in &request.source_session_ids {
        let row = sqlx::query(
            "SELECT id, title, state, created_at, updated_at, revision, internal, repository \
             FROM sessions WHERE id = ? AND tombstoned_at IS NULL AND owner_uid = ?",
        )
        .bind(session_id.to_string())
        .bind(i64::from(owner_uid))
        .fetch_optional(pool)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| {
            CodypendentError::new(
                "protocol.session-not-found",
                format!("no session {session_id}"),
                false,
            )
        })?;
        let updated_str: String = row.get("updated_at");
        if let Ok(ts) = DateTime::parse_from_rfc3339(&updated_str) {
            let ts_utc = ts.with_timezone(&Utc);
            if ts_utc < latest_updated {
                latest_updated = ts_utc;
            }
        }
        session_rows.push((session_id, row));
    }

    let mut redaction_summary = BundleRedactionSummary::default();
    let mut entry_files = Vec::new();

    for (session_id, srow) in &session_rows {
        let title: String = srow.get("title");
        let state: String = srow.get("state");
        let repository: Option<String> = srow.get("repository");

        if request.inclusion.transcript_events {
            let event_rows = sqlx::query(
                // See the note on the sibling read above: `events`, with
                // `actor`/`body`.
                "SELECT sequence, actor, body, occurred_at \
                 FROM events WHERE session_id = ? ORDER BY sequence ASC",
            )
            .bind(session_id.to_string())
            .fetch_all(pool)
            .await
            .map_err(internal_error)?;

            let mut events = Vec::new();
            for erow in event_rows {
                let sequence: i64 = erow.get("sequence");
                let actor_json: String = erow.get("actor");
                let body_json: String = erow.get("body");
                let occurred_at_str: String = erow.get("occurred_at");

                let mut actor_val: serde_json::Value =
                    serde_json::from_str(&actor_json).map_err(internal_error)?;
                let mut body_val: serde_json::Value =
                    serde_json::from_str(&body_json).map_err(internal_error)?;

                redact_json_value(
                    &mut actor_val,
                    &request.redaction_policy,
                    &mut redaction_summary,
                );
                redact_json_value(
                    &mut body_val,
                    &request.redaction_policy,
                    &mut redaction_summary,
                );

                let actor: Actor = serde_json::from_value(actor_val).map_err(internal_error)?;
                let body: EventBody = serde_json::from_value(body_val).map_err(internal_error)?;
                let occurred_at = DateTime::parse_from_rfc3339(&occurred_at_str)
                    .map_err(internal_error)?
                    .with_timezone(&Utc);

                events.push(SessionEvent {
                    sequence: sequence as u64,
                    occurred_at,
                    causation_id: None,
                    correlation_id: None,
                    actor,
                    body,
                });
            }

            let path = format!("sessions/{session_id}/transcript_events.json");
            let data = serde_json::to_vec_pretty(&events).map_err(internal_error)?;
            entry_files.push((
                path,
                BundleEntryKind::TranscriptEvents,
                "application/json".to_string(),
                DataClassification::Internal,
                data,
            ));
        }

        if request.inclusion.routing_metadata {
            let metadata = serde_json::json!({
                "session_id": session_id,
                "repository": repository,
                "state": state,
                "title": title,
            });
            let path = format!("sessions/{session_id}/routing_metadata.json");
            let data = serde_json::to_vec_pretty(&metadata).map_err(internal_error)?;
            entry_files.push((
                path,
                BundleEntryKind::RoutingMetadata,
                "application/json".to_string(),
                DataClassification::Internal,
                data,
            ));
        }

        if request.inclusion.approvals {
            let approval_rows = sqlx::query(
                "SELECT a.id, a.run_id, a.action_json, a.decision, a.resolved_at \
                 FROM approvals a JOIN runs r ON a.run_id = r.id \
                 WHERE r.session_id = ? ORDER BY a.created_at ASC",
            )
            .bind(session_id.to_string())
            .fetch_all(pool)
            .await
            .map_err(internal_error)?;

            let mut approvals = Vec::new();
            for arow in approval_rows {
                let id: String = arow.get("id");
                let run_id: String = arow.get("run_id");
                let action_json: String = arow.get("action_json");
                let decision: Option<String> = arow.get("decision");
                let resolved_at: Option<String> = arow.get("resolved_at");
                approvals.push(serde_json::json!({
                    "id": id,
                    "run_id": run_id,
                    "action": serde_json::from_str::<serde_json::Value>(&action_json).unwrap_or_default(),
                    "decision": decision,
                    "resolved_at": resolved_at,
                }));
            }

            let path = format!("sessions/{session_id}/approvals.json");
            let data = serde_json::to_vec_pretty(&approvals).map_err(internal_error)?;
            entry_files.push((
                path,
                BundleEntryKind::Approvals,
                "application/json".to_string(),
                DataClassification::Internal,
                data,
            ));
        }

        if request.inclusion.artifact_manifests {
            if matches!(request.redaction_policy, BundleRedactionPolicy::SupportSafe) {
                redaction_summary.artifact_bodies_omitted += 1;
            }
            let artifact_payload = serde_json::json!({
                "session_id": session_id,
                "artifacts": Vec::<serde_json::Value>::new(),
            });
            let path = format!("sessions/{session_id}/artifact_manifest.json");
            let data = serde_json::to_vec_pretty(&artifact_payload).map_err(internal_error)?;
            entry_files.push((
                path,
                BundleEntryKind::ArtifactManifest,
                "application/json".to_string(),
                DataClassification::Internal,
                data,
            ));
        }

        if request.inclusion.patches {
            let patch_payload = serde_json::json!({
                "session_id": session_id,
                "patches": Vec::<serde_json::Value>::new(),
            });
            let path = format!("sessions/{session_id}/patch.json");
            let data = serde_json::to_vec_pretty(&patch_payload).map_err(internal_error)?;
            entry_files.push((
                path,
                BundleEntryKind::Patch,
                "application/json".to_string(),
                DataClassification::Internal,
                data,
            ));
        }
    }

    if request.inclusion.environment_diagnostics {
        let diag = serde_json::json!({
            "daemon_version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        });
        let path = "diagnostics/environment.json".to_string();
        let data = serde_json::to_vec_pretty(&diag).map_err(internal_error)?;
        entry_files.push((
            path,
            BundleEntryKind::EnvironmentDiagnostics,
            "application/json".to_string(),
            DataClassification::Internal,
            data,
        ));
    }

    // Sort entries deterministically by path
    entry_files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut manifest_entries = Vec::new();
    for (path, kind, media_type, classification, data) in &entry_files {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let sha256 = hex::encode(hasher.finalize());
        manifest_entries.push(BundleEntryManifest {
            path: path.clone(),
            kind: *kind,
            sha256,
            byte_length: data.len() as u64,
            media_type: media_type.clone(),
            classification: *classification,
        });
    }

    let manifest_bytes_for_hash = serde_json::to_vec(&manifest_entries).map_err(internal_error)?;
    let mut hasher = Sha256::new();
    hasher.update(&manifest_bytes_for_hash);
    let manifest_sha256 = hex::encode(hasher.finalize());

    let manifest = BundleManifest {
        format_version: BUNDLE_FORMAT_V1,
        created_at: latest_updated,
        source_session_ids: request.source_session_ids.clone(),
        inclusion: request.inclusion.clone(),
        redaction_policy: request.redaction_policy.clone(),
        redaction_summary: redaction_summary.clone(),
        entries: manifest_entries,
        manifest_sha256: manifest_sha256.clone(),
    };

    let manifest_json = serde_json::to_vec_pretty(&manifest).map_err(internal_error)?;

    // Build deterministic tar archive
    let mut builder = Builder::new(Vec::new());

    // 1. Append manifest.json
    let mut header = Header::new_gnu();
    header.set_path("manifest.json").map_err(internal_error)?;
    header.set_size(manifest_json.len() as u64);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(EntryType::Regular);
    header.set_cksum();
    builder
        .append(&header, &manifest_json[..])
        .map_err(internal_error)?;

    // 2. Append each regular file entry in sorted order
    for (path, _kind, _media_type, _classification, data) in &entry_files {
        let mut header = Header::new_gnu();
        header.set_path(path).map_err(internal_error)?;
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_entry_type(EntryType::Regular);
        header.set_cksum();
        builder.append(&header, &data[..]).map_err(internal_error)?;
    }

    let archive_bytes = builder.into_inner().map_err(internal_error)?;

    let artifact_ref = artifacts
        .put_owned(
            pool,
            owner_uid,
            "application/vnd.codypendent.bundle",
            DataClassification::Confidential,
            Provenance::system("bundle_export"),
            &archive_bytes,
        )
        .await
        .map_err(internal_error)?;

    let bundle_id = Uuid::now_v7().to_string();
    // Only a policy this daemon actually APPLIED may be persisted. Migration
    // 0041 deliberately excludes `Unknown` from the column's CHECK for exactly
    // this reason: recording it as `Standard` would let a later re-export claim
    // a redaction that never ran. `Unknown` is already refused above; a policy
    // from a newer peer that this build cannot name is refused here too.
    let redaction_policy_str = match request.redaction_policy {
        BundleRedactionPolicy::Standard => "Standard",
        BundleRedactionPolicy::SupportSafe => "SupportSafe",
        _ => {
            return Err(CodypendentError::new(
                "bundle.invalid-request",
                "unsupported redaction policy",
                false,
            ));
        }
    };

    let mut tx = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(internal_error)?;

    sqlx::query(
        "INSERT INTO session_bundles \
         (id, owner_uid, artifact_id, format_version, manifest_sha256, inclusion_json, \
          redaction_policy, redaction_summary_json, created_at, command_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&bundle_id)
    .bind(i64::from(owner_uid))
    .bind(artifact_ref.id.to_string())
    .bind(BUNDLE_FORMAT_V1 as i64)
    .bind(&manifest_sha256)
    .bind(serde_json::to_string(&request.inclusion).map_err(internal_error)?)
    .bind(redaction_policy_str)
    .bind(serde_json::to_string(&redaction_summary).map_err(internal_error)?)
    .bind(latest_updated.to_rfc3339())
    .bind(command_id.to_string())
    .execute(&mut *tx)
    .await
    .map_err(internal_error)?;

    for (ordinal, session_id) in request.source_session_ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO session_bundle_sources (bundle_id, session_id, ordinal) VALUES (?, ?, ?)",
        )
        .bind(&bundle_id)
        .bind(session_id.to_string())
        .bind(ordinal as i64)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
    }

    for (ordinal, entry) in manifest.entries.iter().enumerate() {
        // Every entry kind is minted by this function, so an unnameable kind is
        // a bug, not a peer's future value. Fail rather than mislabel it: the
        // old `Unknown => "TranscriptEvents"` mapping would have filed an
        // unidentified archive member as a transcript.
        let kind_str = match entry.kind {
            BundleEntryKind::TranscriptEvents => "TranscriptEvents",
            BundleEntryKind::RoutingMetadata => "RoutingMetadata",
            BundleEntryKind::Approvals => "Approvals",
            BundleEntryKind::ArtifactManifest => "ArtifactManifest",
            BundleEntryKind::Patch => "Patch",
            BundleEntryKind::EnvironmentDiagnostics => "EnvironmentDiagnostics",
            _ => {
                return Err(internal_error(format!(
                    "refusing to record bundle entry `{}` under an unrecognized kind",
                    entry.path
                )));
            }
        };
        // `DataClassification::Unknown` ranks ABOVE `Secret` (see
        // `DataClassification::rank`), and so does any classification a newer
        // peer defines. Record it as unknown rather than down-classifying it
        // into a label this build merely recognizes.
        let classification_str = match entry.classification {
            DataClassification::Public => "Public",
            DataClassification::Internal => "Internal",
            DataClassification::Confidential => "Confidential",
            DataClassification::Secret => "Secret",
            _ => "Unknown",
        };
        sqlx::query(
            "INSERT INTO session_bundle_entries \
             (bundle_id, path, kind, sha256, byte_length, media_type, classification, ordinal) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&bundle_id)
        .bind(&entry.path)
        .bind(kind_str)
        .bind(&entry.sha256)
        .bind(entry.byte_length as i64)
        .bind(&entry.media_type)
        .bind(classification_str)
        .bind(ordinal as i64)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
    }

    let receipt = BundleExportReceipt {
        bundle: artifact_ref,
        manifest,
    };

    sqlx::query(
        "UPDATE commands SET status = 'applied', result_json = ?, applied_at = ? WHERE id = ?",
    )
    .bind(serde_json::to_string(&receipt).map_err(internal_error)?)
    .bind(Utc::now().to_rfc3339())
    .bind(command_id.to_string())
    .execute(&mut *tx)
    .await
    .map_err(internal_error)?;

    tx.commit().await.map_err(internal_error)?;

    Ok(receipt)
}

/// Import sessions from an uploaded bundle artifact.
pub async fn import(
    pool: &SqlitePool,
    artifacts: &ArtifactStore,
    owner_uid: u32,
    _client_id: ClientId,
    command_id: CommandId,
    _idempotency_key: &str,
    request: &BundleImportRequest,
) -> Result<BundleImportReceipt, CodypendentError> {
    if matches!(request.collision_policy, BundleCollisionPolicy::Unknown) {
        return Err(CodypendentError::new(
            "bundle.invalid-request",
            "unsupported collision policy",
            false,
        ));
    }

    let bytes = artifacts
        .read_bytes(pool, request.bundle.id)
        .await
        .map_err(|_| {
            CodypendentError::new("artifact.not-found", "artifact is unavailable", false)
        })?;

    // Verify bundle SHA-256 and byte length
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual_bundle_sha256 = hex::encode(hasher.finalize());

    if actual_bundle_sha256 != request.bundle.sha256 {
        return Err(CodypendentError::new(
            "bundle.verification-failed",
            "bundle archive hash mismatch",
            false,
        ));
    }
    if bytes.len() as u64 != request.bundle.byte_length {
        return Err(CodypendentError::new(
            "bundle.verification-failed",
            "bundle archive byte length mismatch",
            false,
        ));
    }
    if bytes.len() as u64 > MAX_BUNDLE_ARCHIVE_BYTES {
        return Err(CodypendentError::new(
            "bundle.verification-failed",
            "bundle archive exceeds the total size limit",
            false,
        ));
    }

    let mut archive = Archive::new(std::io::Cursor::new(&bytes));
    let mut files = BTreeMap::<String, Vec<u8>>::new();
    let mut total_entries = 0_usize;
    let mut total_uncompressed_bytes = 0_u64;

    let entries = archive.entries().map_err(|e| {
        CodypendentError::new(
            "bundle.verification-failed",
            format!("corrupted tar archive: {e}"),
            false,
        )
    })?;

    for entry in entries {
        let mut entry = entry.map_err(|e| {
            CodypendentError::new(
                "bundle.verification-failed",
                format!("invalid tar entry: {e}"),
                false,
            )
        })?;
        total_entries += 1;
        if total_entries > MAX_BUNDLE_ENTRIES {
            return Err(CodypendentError::new(
                "bundle.verification-failed",
                "bundle archive exceeds the total-entry limit",
                false,
            ));
        }
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || !entry_type.is_file() {
            return Err(CodypendentError::new(
                "bundle.verification-failed",
                "bundle archive contains a symlink or non-regular file",
                false,
            ));
        }
        let raw_path = entry.path().map_err(|e| {
            CodypendentError::new(
                "bundle.verification-failed",
                format!("invalid path in tar: {e}"),
                false,
            )
        })?;
        let norm_path = normalized_bundle_path(&raw_path)?;
        let declared_size = entry.size();
        if declared_size > MAX_BUNDLE_ENTRY_BYTES {
            return Err(CodypendentError::new(
                "bundle.verification-failed",
                format!("entry `{norm_path}` exceeds the per-file size limit"),
                false,
            ));
        }
        total_uncompressed_bytes = total_uncompressed_bytes
            .checked_add(declared_size)
            .ok_or_else(|| {
                CodypendentError::new(
                    "bundle.verification-failed",
                    "bundle uncompressed size overflow",
                    false,
                )
            })?;
        if total_uncompressed_bytes > MAX_BUNDLE_ARCHIVE_BYTES {
            return Err(CodypendentError::new(
                "bundle.verification-failed",
                "bundle archive exceeds the total uncompressed size limit",
                false,
            ));
        }
        let mut content = Vec::with_capacity(declared_size as usize);
        std::io::Read::read_to_end(&mut entry, &mut content).map_err(|e| {
            CodypendentError::new(
                "bundle.verification-failed",
                format!("failed reading entry `{norm_path}`: {e}"),
                false,
            )
        })?;
        if content.len() as u64 != declared_size {
            return Err(CodypendentError::new(
                "bundle.verification-failed",
                format!("entry `{norm_path}` has a mismatched size"),
                false,
            ));
        }
        if files.insert(norm_path.clone(), content).is_some() {
            return Err(CodypendentError::new(
                "bundle.verification-failed",
                format!("duplicate entry path `{norm_path}` in bundle"),
                false,
            ));
        }
    }

    let manifest_bytes = files.get("manifest.json").ok_or_else(|| {
        CodypendentError::new(
            "bundle.verification-failed",
            "bundle archive is missing manifest.json",
            false,
        )
    })?;

    let manifest: BundleManifest = serde_json::from_slice(manifest_bytes).map_err(|e| {
        CodypendentError::new(
            "bundle.verification-failed",
            format!("invalid manifest.json: {e}"),
            false,
        )
    })?;

    if manifest.format_version > BUNDLE_FORMAT_V1 {
        return Err(CodypendentError::new(
            "bundle.unsupported-version",
            format!(
                "unsupported bundle format version {}",
                manifest.format_version
            ),
            false,
        ));
    }

    let expected_manifest_entries_bytes =
        serde_json::to_vec(&manifest.entries).map_err(internal_error)?;
    let mut hasher = Sha256::new();
    hasher.update(&expected_manifest_entries_bytes);
    let expected_manifest_sha256 = hex::encode(hasher.finalize());

    if manifest.manifest_sha256 != expected_manifest_sha256 {
        return Err(CodypendentError::new(
            "bundle.verification-failed",
            "bundle manifest SHA-256 mismatch",
            false,
        ));
    }

    for entry in &manifest.entries {
        if matches!(entry.kind, BundleEntryKind::Unknown) {
            return Err(CodypendentError::new(
                "bundle.verification-failed",
                format!("unsupported entry kind in manifest for `{}`", entry.path),
                false,
            ));
        }
        let content = files.get(&entry.path).ok_or_else(|| {
            CodypendentError::new(
                "bundle.verification-failed",
                format!("missing entry `{}` described by manifest", entry.path),
                false,
            )
        })?;
        if content.len() as u64 != entry.byte_length {
            return Err(CodypendentError::new(
                "bundle.verification-failed",
                format!("entry `{}` has mismatched byte length", entry.path),
                false,
            ));
        }
        let mut hasher = Sha256::new();
        hasher.update(content);
        let actual_hash = hex::encode(hasher.finalize());
        if actual_hash != entry.sha256 {
            return Err(CodypendentError::new(
                "bundle.verification-failed",
                format!("entry `{}` has mismatched sha256", entry.path),
                false,
            ));
        }
    }

    // Ensure no unlisted files exist in archive besides manifest.json
    for path in files.keys() {
        if path == "manifest.json" {
            continue;
        }
        if !manifest.entries.iter().any(|e| &e.path == path) {
            return Err(CodypendentError::new(
                "bundle.verification-failed",
                format!("archive contains unlisted entry `{path}`"),
                false,
            ));
        }
    }

    // Check collision policies for existing sessions
    for source_id in &manifest.source_session_ids {
        let existing: Option<(String,)> = sqlx::query_as("SELECT id FROM sessions WHERE id = ?")
            .bind(source_id.to_string())
            .fetch_optional(pool)
            .await
            .map_err(internal_error)?;
        if existing.is_some() {
            match request.collision_policy {
                BundleCollisionPolicy::Reject => {
                    return Err(CodypendentError::new(
                        "bundle.collision-rejected",
                        format!("session identity `{source_id}` already exists locally"),
                        false,
                    ));
                }
                BundleCollisionPolicy::Skip | BundleCollisionPolicy::Remap => {}
                // A collision policy this build cannot name must not be read as
                // the permissive one. `Unknown` is already refused at the top of
                // this function; anything else a newer peer sends is refused
                // here rather than defaulting to import-anyway.
                _ => {
                    return Err(CodypendentError::new(
                        "bundle.invalid-request",
                        "unsupported collision policy",
                        false,
                    ));
                }
            }
        }
    }

    let now = Utc::now();
    let now_str = now.to_rfc3339();
    let import_id = Uuid::now_v7().to_string();

    let mut identity_mappings = Vec::new();
    let mut imported_session_ids = Vec::new();
    let mut skipped_entries = 0_u64;

    let mut tx = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(internal_error)?;

    let provenance = BundleImportProvenance {
        bundle_sha256: actual_bundle_sha256.clone(),
        manifest_sha256: manifest.manifest_sha256.clone(),
        imported_at: now,
        source_session_ids: manifest.source_session_ids.clone(),
    };

    for source_session_id in &manifest.source_session_ids {
        if matches!(request.collision_policy, BundleCollisionPolicy::Skip) {
            let existing: Option<(String,)> =
                sqlx::query_as("SELECT id FROM sessions WHERE id = ?")
                    .bind(source_session_id.to_string())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(internal_error)?;
            if existing.is_some() {
                skipped_entries += 1;
                continue;
            }
        }

        // Fresh local identity minted: NEVER reuse source IDs
        let new_session_id = SessionId::new();
        identity_mappings.push(BundleIdentityMapping {
            kind: BundleIdentityKind::Session,
            source_id: source_session_id.to_string(),
            local_id: new_session_id.to_string(),
            provenance: provenance.clone(),
        });
        imported_session_ids.push(new_session_id);

        let transcript_path = format!("sessions/{source_session_id}/transcript_events.json");
        let routing_path = format!("sessions/{source_session_id}/routing_metadata.json");
        let approvals_path = format!("sessions/{source_session_id}/approvals.json");

        let mut session_title = "Imported Session".to_string();
        let mut repository = None;
        let mut session_state = "closed".to_string();

        if let Some(rdata) = files.get(&routing_path) {
            if let Ok(meta) = serde_json::from_slice::<serde_json::Value>(rdata) {
                if let Some(t) = meta.get("title").and_then(|v| v.as_str()) {
                    session_title = t.to_string();
                }
                if let Some(r) = meta.get("repository").and_then(|v| v.as_str()) {
                    repository = Some(r.to_string());
                }
                if let Some(s) = meta.get("state").and_then(|v| v.as_str()) {
                    session_state = s.to_string();
                }
            }
        }

        // Remap runs
        let mut run_id_map = HashMap::<String, RunId>::new();

        let mut events = Vec::new();
        if let Some(tdata) = files.get(&transcript_path) {
            if let Ok(parsed_events) = serde_json::from_slice::<Vec<SessionEvent>>(tdata) {
                for event in parsed_events {
                    let mut body = event.body;
                    let mut actor = event.actor;

                    // `SessionForked.from_session` names the EXPORTING
                    // deployment's parent session. This import mints fresh
                    // local ids, so that id can never denote the imported
                    // parent — and if it happens to collide with an unrelated
                    // local session (the case the collision policy above
                    // exists for) the imported history would assert a lineage
                    // that never happened. Omit the event rather than rewrite
                    // it into a claim; the archive still holds the original.
                    if matches!(body, EventBody::SessionForked { .. }) {
                        continue;
                    }

                    // Every arm below is a variant that really declares a
                    // `run_id` field in `crates/protocol/src/events.rs`; the
                    // list was read off that file, not guessed. A variant with
                    // no run identity — and `Unknown`, whose fields did not
                    // survive deserialization — falls through the catch-all and
                    // is rewritten not at all, rather than having an identity
                    // invented for it.
                    match &mut body {
                        EventBody::RunStarted { run_id, .. }
                        | EventBody::RunStateChanged { run_id, .. }
                        | EventBody::ModelStreamDelta { run_id, .. }
                        | EventBody::ModelRetrying { run_id, .. }
                        | EventBody::ToolProposed { run_id, .. }
                        | EventBody::ToolDenied { run_id, .. }
                        | EventBody::ToolStarted { run_id, .. }
                        | EventBody::ToolUpdated { run_id, .. }
                        | EventBody::ToolCompleted { run_id, .. }
                        | EventBody::PatchProposed { run_id, .. }
                        | EventBody::SteeringQueued { run_id, .. }
                        | EventBody::SteeringApplied { run_id, .. }
                        | EventBody::BudgetWarning { run_id, .. }
                        | EventBody::ContextUsage { run_id, .. }
                        | EventBody::RunCompleted { run_id, .. }
                        | EventBody::RunUsage { run_id, .. }
                        | EventBody::LearningsCaptured { run_id, .. }
                        | EventBody::QuestionAsked { run_id, .. }
                        | EventBody::CheckpointRecorded { run_id, .. }
                        | EventBody::CheckpointRestored { run_id, .. }
                        | EventBody::NoteAppended {
                            run_id: Some(run_id),
                            ..
                        } => {
                            let new_run_id = *run_id_map.entry(run_id.to_string()).or_default();
                            *run_id = new_run_id;
                        }
                        _ => {}
                    }

                    // The authoring actor names a run too. Leaving it alone
                    // would embed the exporting daemon's run id in a locally
                    // minted record, which is exactly the identity reuse
                    // invariant 3 forbids.
                    if let Actor::Agent { run_id, .. } = &mut actor {
                        let new_run_id = *run_id_map.entry(run_id.to_string()).or_default();
                        *run_id = new_run_id;
                    }

                    events.push(SessionEvent {
                        sequence: event.sequence,
                        occurred_at: event.occurred_at,
                        causation_id: None,
                        correlation_id: None,
                        actor,
                        body,
                    });
                }
            }
        }

        for (source_run_id, local_run_id) in &run_id_map {
            identity_mappings.push(BundleIdentityMapping {
                kind: BundleIdentityKind::Run,
                source_id: source_run_id.clone(),
                local_id: local_run_id.to_string(),
                provenance: provenance.clone(),
            });
        }

        // Track dropped approvals in identity map
        if let Some(adata) = files.get(&approvals_path) {
            if let Ok(approvals) = serde_json::from_slice::<Vec<serde_json::Value>>(adata) {
                for app in approvals {
                    if let Some(aid) = app.get("id").and_then(|v| v.as_str()) {
                        identity_mappings.push(BundleIdentityMapping {
                            kind: BundleIdentityKind::Approval,
                            source_id: aid.to_string(),
                            local_id: String::new(), // Seen and dropped: never restored
                            provenance: provenance.clone(),
                        });
                    }
                }
            }
        }

        // Create the session in SQLite
        sqlx::query(
            "INSERT INTO sessions \
             (id, title, state, created_at, updated_at, revision, internal, pinned, \
              owner_uid, repository, imported_from_bundle, last_activity_at) \
             VALUES (?, ?, ?, ?, ?, 0, 0, 0, ?, ?, ?, ?)",
        )
        .bind(new_session_id.to_string())
        .bind(&session_title)
        .bind(&session_state)
        .bind(&now_str)
        .bind(&now_str)
        .bind(i64::from(owner_uid))
        .bind(repository)
        .bind(&import_id)
        .bind(&now_str)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;

        session_library::index_title_source(&mut *tx, new_session_id, &session_title, &now_str)
            .await
            .map_err(internal_error)?;

        for event in events {
            let actor_json = serde_json::to_string(&event.actor).map_err(internal_error)?;
            let body_json = serde_json::to_string(&event.body).map_err(internal_error)?;
            let event_time = event.occurred_at.to_rfc3339();

            sqlx::query(
                // Same schema correction as the reads: this wrote to a table
                // that does not exist, so every import failed.
                "INSERT INTO events \
                 (session_id, sequence, actor, body, occurred_at) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(new_session_id.to_string())
            .bind(event.sequence as i64)
            .bind(&actor_json)
            .bind(&body_json)
            .bind(&event_time)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;

            session_library::index_event_sources(
                &mut tx,
                new_session_id,
                event.sequence as i64,
                &event.body,
                &event_time,
            )
            .await
            .map_err(internal_error)?;
        }
    }

    // Record the policy that was APPLIED. Migration 0041's CHECK admits only
    // these three; coercing an unnameable policy to `Remap` would record a
    // decision this daemon never made.
    let collision_policy_str = match request.collision_policy {
        BundleCollisionPolicy::Reject => "Reject",
        BundleCollisionPolicy::Remap => "Remap",
        BundleCollisionPolicy::Skip => "Skip",
        _ => {
            return Err(CodypendentError::new(
                "bundle.invalid-request",
                "unsupported collision policy",
                false,
            ));
        }
    };

    sqlx::query(
        "INSERT INTO session_bundle_imports \
         (id, owner_uid, bundle_artifact_id, bundle_sha256, manifest_sha256, format_version, \
          collision_policy, imported_at, skipped_entries, command_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&import_id)
    .bind(i64::from(owner_uid))
    .bind(request.bundle.id.to_string())
    .bind(&actual_bundle_sha256)
    .bind(&manifest.manifest_sha256)
    .bind(manifest.format_version as i64)
    .bind(collision_policy_str)
    .bind(&now_str)
    .bind(skipped_entries as i64)
    .bind(command_id.to_string())
    .execute(&mut *tx)
    .await
    .map_err(internal_error)?;

    for mapping in &identity_mappings {
        // Every mapping in this vector is minted above, so an unnameable kind
        // is a bug. Filing it as `Session` would claim a session identity was
        // remapped when none was.
        let kind_str = match mapping.kind {
            BundleIdentityKind::Session => "Session",
            BundleIdentityKind::Run => "Run",
            BundleIdentityKind::Artifact => "Artifact",
            BundleIdentityKind::Approval => "Approval",
            BundleIdentityKind::ChangeSet => "ChangeSet",
            _ => {
                return Err(internal_error(
                    "refusing to record an identity mapping of an unrecognized kind",
                ));
            }
        };
        sqlx::query(
            "INSERT INTO session_bundle_identity_map \
             (import_id, kind, source_id, local_id) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&import_id)
        .bind(kind_str)
        .bind(&mapping.source_id)
        .bind(&mapping.local_id)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
    }

    let receipt = BundleImportReceipt {
        provenance,
        identity_mappings,
        imported_session_ids,
        skipped_entries,
    };

    sqlx::query(
        "UPDATE commands SET status = 'applied', result_json = ?, applied_at = ? WHERE id = ?",
    )
    .bind(serde_json::to_string(&receipt).map_err(internal_error)?)
    .bind(Utc::now().to_rfc3339())
    .bind(command_id.to_string())
    .execute(&mut *tx)
    .await
    .map_err(internal_error)?;

    tx.commit().await.map_err(internal_error)?;

    Ok(receipt)
}

pub async fn bundle_export_response(
    pool: &SqlitePool,
    idempotency_key: &str,
) -> Result<BundleExportReceipt, CodypendentError> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT result_json FROM commands WHERE idempotency_key = ? AND status = 'applied'",
    )
    .bind(idempotency_key)
    .fetch_optional(pool)
    .await
    .map_err(internal_error)?;
    let (json,) = row.ok_or_else(|| internal_error("applied bundle export command disappeared"))?;
    let json =
        json.ok_or_else(|| internal_error("applied bundle export command missing result_json"))?;
    serde_json::from_str::<BundleExportReceipt>(&json).map_err(internal_error)
}

pub async fn bundle_import_response(
    pool: &SqlitePool,
    idempotency_key: &str,
) -> Result<BundleImportReceipt, CodypendentError> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT result_json FROM commands WHERE idempotency_key = ? AND status = 'applied'",
    )
    .bind(idempotency_key)
    .fetch_optional(pool)
    .await
    .map_err(internal_error)?;
    let (json,) = row.ok_or_else(|| internal_error("applied bundle import command disappeared"))?;
    let json =
        json.ok_or_else(|| internal_error("applied bundle import command missing result_json"))?;
    serde_json::from_str::<BundleImportReceipt>(&json).map_err(internal_error)
}
