//! The memory observer (Chapter 06, STEP 2.4).
//!
//! Memory is an always-on service, not a tool the model must remember to call.
//! The observer watches a session's event stream and extracts
//! [`CandidateMemory`] proposals; the [`curate`](crate::memory::MemoryStore::curate)
//! pipeline then decides which become durable.
//!
//! This module is the **pure extraction** half — [`extract_candidates`] over a
//! slice of protocol events. The live daemon *subscription* that feeds it is a
//! later integration step; keeping extraction pure makes it directly testable.
//!
//! Every candidate carries at least one [`EvidenceRef`] so the curator's
//! provenance gate never rejects an observer-produced candidate:
//! - a **repeated, successful shell command** (`ToolStarted` paired with a
//!   succeeding `ToolCompleted`) → a [`Procedural`](MemoryClass::Procedural)
//!   candidate citing the [`EventRange`](EvidenceRef::EventRange) that spans the
//!   occurrences;
//! - a **`RunCompleted` chronicle** → an [`Episodic`](MemoryClass::Episodic) (or
//!   [`Failure`](MemoryClass::Failure)) candidate citing the chronicle
//!   [`Artifact`](EvidenceRef::Artifact);
//! - an **explicit `memory.propose` note** → a [`Semantic`](MemoryClass::Semantic)
//!   candidate citing the note's event.
//!
//! ## Evidence and the session id
//!
//! An [`EventRange`](EvidenceRef::EventRange) needs the `SessionId` its
//! sequences belong to, but events do not carry it (the ledger is per-session).
//! The observer therefore takes it from `scope` when that is a
//! [`Scope::Session`] — the natural case, since a subscription is per session.
//! Event-range candidates are only emitted when the session id is available;
//! artifact-cited candidates (the `RunCompleted` chronicle) are always emitted.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use codypendent_protocol::{
    ArtifactRef, DataClassification, EventBody, RunDisposition, RunId, SessionEvent, SessionId,
    ToolOutcome,
};

use crate::memory::CandidateMemory;
use crate::types::{EvidenceRef, MemoryClass, RetentionPolicy, Revision, Scope};

/// The default confidence stamped on an observed candidate (below a model- or
/// user-asserted fact; the curator and later learning can adjust).
const OBSERVED_CONFIDENCE: f32 = 0.6;

/// How many successful runs of the *same* command make it a durable procedure.
const MIN_REPEATS: usize = 2;

/// The tool whose repeated success yields a procedural memory.
const SHELL_TOOL: &str = "shell.run";

/// Markers that flag a note as an explicit memory proposal.
const PROPOSE_MARKERS: [&str; 2] = ["memory.propose:", "memory:"];

/// The maximum number of characters kept from a completed run's summary when
/// building the breadcrumb statement (M0: a bounded crumb, not the whole
/// reply).
const SUMMARY_BREADCRUMB_MAX: usize = 200;

/// Retention, in days, for a completed-run breadcrumb candidate.
const BREADCRUMB_TTL_DAYS: u32 = 30;

/// Per-class caps on heuristic chronicle extraction (M1) — a chronicle can
/// carry an unbounded number of investigation lines / changes / actions; these
/// keep a single run from flooding the curator with hundreds of candidates.
const MAX_CHRONICLE_FINDINGS: usize = 8;
const MAX_CHRONICLE_CHANGES: usize = 8;
const MAX_CHRONICLE_FAILURES: usize = 8;

/// Case-insensitive prefixes that mark an `investigations` line as a decision
/// rather than a plain finding.
const DECISION_MARKERS: [&str; 4] = ["decided", "chose", "will use", "because"];

/// Truncate `s` to at most `max` **characters** (never splitting a multibyte
/// char), appending `…` only when truncation actually occurred.
fn cap_chars(s: &str, max: usize) -> String {
    let mut indices = s.char_indices();
    if let Some((idx, _)) = indices.nth(max) {
        let mut truncated = String::with_capacity(idx + '…'.len_utf8());
        truncated.push_str(&s[..idx]);
        truncated.push('…');
        truncated
    } else {
        s.to_string()
    }
}

/// The canonical orderable revision for a ledger sequence. Delegates to
/// [`Revision::sequence`] so the `seq:` + fixed-width-zero-padded format (which
/// the memory query relies on to compare revisions as ordered text) is defined
/// in exactly one place.
fn seq_revision(sequence: u64) -> Revision {
    Revision::sequence(sequence)
}

/// Extract [`CandidateMemory`] proposals from a slice of session events under
/// `scope`. Pure and side-effect-free; see the module docs for the extraction
/// rules and how the evidence session id is sourced.
#[must_use]
pub fn extract_candidates(events: &[SessionEvent], scope: Scope) -> Vec<CandidateMemory> {
    let session = match &scope {
        Scope::Session(id) => Some(*id),
        _ => None,
    };
    let mut candidates = Vec::new();
    candidates.extend(repeated_command_candidates(events, &scope, session));
    candidates.extend(run_outcome_candidates(events, &scope));
    candidates.extend(explicit_proposal_candidates(events, &scope, session));
    candidates
}

/// One successfully-completed shell command, paired from its `ToolStarted` /
/// `ToolCompleted` events.
struct SuccessfulRun {
    digest: String,
    start_sequence: u64,
    complete_sequence: u64,
    completed_at: chrono::DateTime<chrono::Utc>,
}

/// Procedural candidates: a `shell.run` command whose argument digest succeeded
/// [`MIN_REPEATS`] or more times is a repeatable step, cited by the event range
/// spanning its occurrences. Requires a session id for the range.
fn repeated_command_candidates(
    events: &[SessionEvent],
    scope: &Scope,
    session: Option<SessionId>,
) -> Vec<CandidateMemory> {
    let Some(session) = session else {
        return Vec::new();
    };

    // Pair each `shell.run` ToolStarted (which carries the args digest) with its
    // own run's ToolCompleted, keyed by `RunId`. A plain stack mispairs when
    // runs interleave and strands the start of any run whose tool *failed*
    // (leaving stale entries); keying by run id — and removing the pending entry
    // on ANY completion, recording a success only when the outcome succeeded —
    // pairs correctly regardless of concurrency or failure.
    let mut pending: HashMap<RunId, (String, u64)> = HashMap::new();
    let mut runs: Vec<SuccessfulRun> = Vec::new();
    for event in events {
        match &event.body {
            EventBody::ToolStarted {
                run_id,
                tool,
                args_digest,
                ..
            } if tool == SHELL_TOOL => {
                pending.insert(*run_id, (args_digest.clone(), event.sequence));
            }
            EventBody::ToolCompleted {
                run_id,
                tool,
                outcome,
                ..
            } if tool == SHELL_TOOL => {
                if let Some((digest, start_sequence)) = pending.remove(run_id) {
                    if matches!(outcome, ToolOutcome::Succeeded) {
                        runs.push(SuccessfulRun {
                            digest,
                            start_sequence,
                            complete_sequence: event.sequence,
                            completed_at: event.occurred_at,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    // Group by argument digest; a digest with enough successes is a procedure.
    let mut groups: BTreeMap<String, Vec<&SuccessfulRun>> = BTreeMap::new();
    for run in &runs {
        groups.entry(run.digest.clone()).or_default().push(run);
    }

    let mut candidates = Vec::new();
    for (digest, group) in groups {
        if group.len() < MIN_REPEATS {
            continue;
        }
        let from = group.iter().map(|r| r.start_sequence).min().unwrap_or(0);
        let to = group.iter().map(|r| r.complete_sequence).max().unwrap_or(0);
        let observed_at = group
            .iter()
            .map(|r| r.completed_at)
            .max()
            .unwrap_or_else(chrono::Utc::now);
        candidates.push(CandidateMemory {
            class: MemoryClass::Procedural,
            scope: Some(scope.clone()),
            statement: format!(
                "`{SHELL_TOOL}` with argument digest {digest} is a repeatable, \
                 reliably-succeeding step ({} runs).",
                group.len()
            ),
            structured_value: Some(serde_json::json!({
                "tool": SHELL_TOOL,
                "args_digest": digest,
                "successes": group.len(),
            })),
            provenance: vec![EvidenceRef::EventRange {
                session_id: session,
                from_sequence: from,
                to_sequence: to,
            }],
            confidence: OBSERVED_CONFIDENCE,
            observed_at,
            valid_from: seq_revision(to),
            sensitivity: DataClassification::Internal,
            retention: None,
        });
    }
    candidates
}

/// Episodic / failure candidates from `RunCompleted` chronicles, cited by the
/// chronicle artifact (so evidence is always present, no session id needed).
fn run_outcome_candidates(events: &[SessionEvent], scope: &Scope) -> Vec<CandidateMemory> {
    let mut candidates = Vec::new();
    for event in events {
        let EventBody::RunCompleted {
            run_id,
            disposition,
            chronicle,
        } = &event.body
        else {
            continue;
        };
        let (class, statement, retention) = match disposition {
            RunDisposition::Completed { summary } => {
                let first = summary
                    .as_deref()
                    .and_then(|s| s.lines().map(str::trim).find(|l| !l.is_empty()))
                    .unwrap_or("(no summary)");
                (
                    MemoryClass::Episodic,
                    format!(
                        "Run {run_id} completed: {}",
                        cap_chars(first, SUMMARY_BREADCRUMB_MAX)
                    ),
                    Some(RetentionPolicy {
                        ttl_days: Some(BREADCRUMB_TTL_DAYS),
                    }),
                )
            }
            RunDisposition::Failed { reason, .. } => (
                MemoryClass::Failure,
                format!("Run {run_id} failed: {reason}"),
                None,
            ),
            RunDisposition::Cancelled { reason } => (
                MemoryClass::Episodic,
                format!(
                    "Run {run_id} cancelled{}",
                    reason
                        .as_ref()
                        .map(|r| format!(": {r}"))
                        .unwrap_or_default()
                ),
                None,
            ),
            _ => continue,
        };
        candidates.push(CandidateMemory {
            class,
            scope: Some(scope.clone()),
            statement,
            structured_value: None,
            provenance: vec![EvidenceRef::Artifact {
                artifact: chronicle.clone(),
                source_path: None,
            }],
            confidence: OBSERVED_CONFIDENCE,
            observed_at: event.occurred_at,
            valid_from: seq_revision(event.sequence),
            // Inherit the chronicle's classification so a sensitive run does not
            // become a less-restricted memory.
            sensitivity: chronicle.sensitivity,
            retention,
        });
    }
    candidates
}

/// Semantic candidates from explicit `memory.propose`-style notes, cited by the
/// note's own event. Requires a session id for the range.
fn explicit_proposal_candidates(
    events: &[SessionEvent],
    scope: &Scope,
    session: Option<SessionId>,
) -> Vec<CandidateMemory> {
    let Some(session) = session else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for event in events {
        let EventBody::NoteAppended { text, .. } = &event.body else {
            continue;
        };
        let trimmed = text.trim_start();
        let lower = trimmed.to_lowercase();
        let Some(marker) = PROPOSE_MARKERS.into_iter().find(|m| lower.starts_with(m)) else {
            continue;
        };
        let rest = &trimmed[marker.len()..];
        // M2: an optional structured value tail, delimited by the first ASCII
        // Record Separator (`\u{1e}`). No separator ⇒ exactly today's behavior
        // (backward compatible with existing `memory:` / `memory.propose:`
        // notes): the whole remainder is the statement, no structured value. A
        // present separator whose tail fails to parse as JSON also falls back
        // to the whole remainder as the statement, never a panic.
        let (statement, structured_value) = match rest.split_once('\u{1e}') {
            Some((stmt, tail)) => match serde_json::from_str::<serde_json::Value>(tail.trim()) {
                Ok(value) => (stmt.trim(), Some(value)),
                Err(_) => (rest.trim(), None),
            },
            None => (rest.trim(), None),
        };
        if statement.is_empty() {
            continue;
        }
        candidates.push(CandidateMemory {
            class: MemoryClass::Semantic,
            scope: Some(scope.clone()),
            statement: statement.to_string(),
            structured_value,
            provenance: vec![EvidenceRef::EventRange {
                session_id: session,
                from_sequence: event.sequence,
                to_sequence: event.sequence,
            }],
            confidence: OBSERVED_CONFIDENCE,
            observed_at: event.occurred_at,
            valid_from: seq_revision(event.sequence),
            sensitivity: DataClassification::Internal,
            retention: None,
        });
    }
    candidates
}

/// The first `<path>.<ext>:<line>` token in `line` (e.g. `src/a.rs:42`), or
/// `None`. Regex-free (`crates/knowledge` forbids the `regex` dep): scan
/// whitespace-split tokens for one that has a `:` followed by ASCII digits,
/// with a `.<ext>` before the `:`.
fn code_ref(line: &str) -> Option<&str> {
    line.split_whitespace().find(|tok| {
        let Some((path, line_no)) = tok.rsplit_once(':') else {
            return false;
        };
        !line_no.is_empty()
            && line_no.bytes().all(|b| b.is_ascii_digit())
            && path.rsplit_once('.').is_some_and(|(_, ext)| {
                !ext.is_empty() && ext.bytes().all(|b| b.is_ascii_alphanumeric())
            })
    })
}

/// Discrete heuristic facts from a parsed run chronicle (M1). Pure: data in →
/// candidates out; a missing or misshaped field simply yields no candidates
/// from that field — never a panic. Every candidate cites the chronicle
/// artifact itself (`EvidenceRef::Artifact`), so provenance is always present
/// without needing a session id.
///
/// Field reality (`crates/runtime/src/agent.rs build_chronicle`):
/// `investigations` is an array of plain strings (findings/decisions),
/// `changes` entries are `{changeset_id, artifact, byte_length}`, and
/// `actions` entries are `{tool, outcome, artifact}`. `decisions` is always
/// `[]` in practice, so decision-shaped facts are pulled out of
/// `investigations` lines instead (lines starting with a marker in
/// [`DECISION_MARKERS`]).
#[must_use]
pub fn chronicle_candidates(
    chronicle: &serde_json::Value,
    scope: &Scope,
    chronicle_ref: &ArtifactRef,
    run_id: RunId,
    observed_at: DateTime<Utc>,
    valid_from: Revision,
    sensitivity: DataClassification,
) -> Vec<CandidateMemory> {
    let build = |class: MemoryClass, raw: &str, valid_from: Revision| CandidateMemory {
        class,
        scope: Some(scope.clone()),
        statement: cap_chars(raw, SUMMARY_BREADCRUMB_MAX),
        structured_value: None,
        provenance: vec![EvidenceRef::Artifact {
            artifact: chronicle_ref.clone(),
            source_path: None,
        }],
        confidence: OBSERVED_CONFIDENCE,
        observed_at,
        valid_from,
        sensitivity,
        retention: None,
    };

    let mut candidates = Vec::new();

    // `investigations`: plain-string findings/decisions.
    if let Some(lines) = chronicle.get("investigations").and_then(|v| v.as_array()) {
        for line in lines
            .iter()
            .filter_map(|v| v.as_str())
            .take(MAX_CHRONICLE_FINDINGS)
        {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(reference) = code_ref(trimmed) {
                candidates.push(build(
                    MemoryClass::Code,
                    &format!("{reference} — {trimmed}"),
                    valid_from.clone(),
                ));
                continue;
            }
            let lower = trimmed.to_lowercase();
            let is_decision = DECISION_MARKERS.iter().any(|m| lower.starts_with(m));
            let word_count = trimmed.split_whitespace().count();
            if is_decision || word_count >= 4 {
                candidates.push(build(MemoryClass::Semantic, trimmed, valid_from.clone()));
            }
        }
    }

    // `changes`: applied changesets.
    if let Some(entries) = chronicle.get("changes").and_then(|v| v.as_array()) {
        for entry in entries.iter().take(MAX_CHRONICLE_CHANGES) {
            let Some(changeset_id) = entry.get("changeset_id").and_then(|v| v.as_str()) else {
                continue;
            };
            let bytes_clause = entry
                .get("byte_length")
                .and_then(|v| v.as_u64())
                .map(|n| format!(" ({n} bytes)"))
                .unwrap_or_default();
            candidates.push(build(
                MemoryClass::Episodic,
                &format!("Applied changeset {changeset_id}{bytes_clause} in run {run_id}"),
                valid_from.clone(),
            ));
        }
    }

    // `actions`: failed/denied/rejected tool invocations.
    if let Some(entries) = chronicle.get("actions").and_then(|v| v.as_array()) {
        for entry in entries.iter().take(MAX_CHRONICLE_FAILURES) {
            let (Some(tool), Some(outcome)) = (
                entry.get("tool").and_then(|v| v.as_str()),
                entry.get("outcome").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            if matches!(outcome, "failed" | "denied" | "rejected") {
                candidates.push(build(
                    MemoryClass::Failure,
                    &format!("{tool} {outcome} in run {run_id}"),
                    valid_from.clone(),
                ));
            }
        }
    }

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::{Actor, ArtifactId, ArtifactRef, RunId};

    fn artifact() -> ArtifactRef {
        ArtifactRef {
            id: ArtifactId::new(),
            media_type: "application/json".to_string(),
            byte_length: 42,
            sha256: "0".repeat(64),
            sensitivity: DataClassification::Internal,
        }
    }

    fn event(sequence: u64, body: EventBody) -> SessionEvent {
        SessionEvent {
            sequence,
            occurred_at: chrono::Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body,
        }
    }

    // ----------------------------------------------------------------
    // cap_chars
    // ----------------------------------------------------------------

    #[test]
    fn cap_chars_leaves_short_strings_untouched() {
        assert_eq!(cap_chars("hello", 200), "hello");
        assert_eq!(cap_chars("", 200), "");
        // Exactly at the limit: still no ellipsis.
        assert_eq!(cap_chars("abcde", 5), "abcde");
    }

    #[test]
    fn cap_chars_truncates_and_appends_ellipsis_only_when_cut() {
        let capped = cap_chars("abcdefghij", 5);
        assert_eq!(capped, "abcde…");
    }

    #[test]
    fn cap_chars_is_char_boundary_safe_on_multibyte_input() {
        // Each "é" is a single char but multiple bytes in UTF-8; a byte-index
        // truncation would panic or split the char. Chars 0..=2 are "a", "é",
        // "b" — capping at 2 chars must keep exactly "aé" plus the ellipsis.
        let s = "aébcdéfg";
        let capped = cap_chars(s, 2);
        assert_eq!(capped, "aé…");
    }

    // ----------------------------------------------------------------
    // run_outcome_candidates — Completed arm
    // ----------------------------------------------------------------

    #[test]
    fn completed_run_yields_bounded_breadcrumb_not_whole_reply() {
        let session = SessionId::new();
        let run = RunId::new();
        let long_first_line = "a".repeat(250);
        let long_tail = "x".repeat(500);
        let summary = format!("{long_first_line}\n{long_tail}");

        let events = vec![event(
            1,
            EventBody::RunCompleted {
                run_id: run,
                disposition: RunDisposition::Completed {
                    summary: Some(summary),
                },
                chronicle: artifact(),
            },
        )];

        let candidates = run_outcome_candidates(&events, &Scope::Session(session));
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];

        assert_eq!(candidate.class, MemoryClass::Episodic);
        let prefix = format!("Run {run} completed: ");
        assert!(
            candidate.statement.starts_with(&prefix),
            "got {:?}",
            candidate.statement
        );
        let max_len = prefix.len() + SUMMARY_BREADCRUMB_MAX + '…'.len_utf8();
        assert!(
            candidate.statement.len() <= max_len,
            "statement of length {} exceeds bound {max_len}: {:?}",
            candidate.statement.len(),
            candidate.statement
        );
        // Only the first line survived — none of the 500-char tail leaked in.
        assert!(!candidate.statement.contains('x'));
        assert!(
            candidate.statement.ends_with('…'),
            "got {:?}",
            candidate.statement
        );
        assert_eq!(
            candidate.retention,
            Some(RetentionPolicy {
                ttl_days: Some(BREADCRUMB_TTL_DAYS)
            })
        );
    }

    #[test]
    fn completed_run_none_summary_uses_placeholder() {
        let session = SessionId::new();
        let run = RunId::new();

        let events = vec![event(
            1,
            EventBody::RunCompleted {
                run_id: run,
                disposition: RunDisposition::Completed { summary: None },
                chronicle: artifact(),
            },
        )];

        let candidates = run_outcome_candidates(&events, &Scope::Session(session));
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];

        assert_eq!(
            candidate.statement,
            format!("Run {run} completed: (no summary)")
        );
        assert_eq!(
            candidate.retention,
            Some(RetentionPolicy {
                ttl_days: Some(BREADCRUMB_TTL_DAYS)
            })
        );
    }

    // ----------------------------------------------------------------
    // run_outcome_candidates — Failed / Cancelled arms unchanged
    // ----------------------------------------------------------------

    #[test]
    fn failed_run_statement_and_retention_unchanged() {
        let session = SessionId::new();
        let run = RunId::new();

        let events = vec![event(
            1,
            EventBody::RunCompleted {
                run_id: run,
                disposition: RunDisposition::Failed {
                    reason: "clippy denied 3 lints".to_string(),
                    error: None,
                },
                chronicle: artifact(),
            },
        )];

        let candidates = run_outcome_candidates(&events, &Scope::Session(session));
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];

        assert_eq!(candidate.class, MemoryClass::Failure);
        assert_eq!(
            candidate.statement,
            format!("Run {run} failed: clippy denied 3 lints")
        );
        assert_eq!(candidate.retention, None);
    }

    #[test]
    fn cancelled_run_statement_and_retention_unchanged() {
        let session = SessionId::new();
        let run = RunId::new();

        let events = vec![event(
            1,
            EventBody::RunCompleted {
                run_id: run,
                disposition: RunDisposition::Cancelled {
                    reason: Some("user aborted".to_string()),
                },
                chronicle: artifact(),
            },
        )];

        let candidates = run_outcome_candidates(&events, &Scope::Session(session));
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];

        assert_eq!(candidate.class, MemoryClass::Episodic);
        assert_eq!(
            candidate.statement,
            format!("Run {run} cancelled: user aborted")
        );
        assert_eq!(candidate.retention, None);
    }

    // ----------------------------------------------------------------
    // chronicle_candidates
    // ----------------------------------------------------------------

    fn chronicle_args() -> (
        Scope,
        ArtifactRef,
        RunId,
        chrono::DateTime<chrono::Utc>,
        Revision,
    ) {
        (
            Scope::Repository(codypendent_protocol::RepositoryId::new()),
            artifact(),
            RunId::new(),
            chrono::Utc::now(),
            Revision::sequence(1),
        )
    }

    #[test]
    fn chronicle_code_ref_finding_yields_code_candidate() {
        let (scope, chronicle_ref, run_id, observed_at, valid_from) = chronicle_args();
        let chronicle = serde_json::json!({
            "investigations": ["crates/x/src/a.rs:42 the guard is inverted"],
        });

        let candidates = chronicle_candidates(
            &chronicle,
            &scope,
            &chronicle_ref,
            run_id,
            observed_at,
            valid_from,
            DataClassification::Internal,
        );

        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.class, MemoryClass::Code);
        assert!(
            candidate.statement.contains("crates/x/src/a.rs:42"),
            "got {:?}",
            candidate.statement
        );
        assert_eq!(
            candidate.provenance,
            vec![EvidenceRef::Artifact {
                artifact: chronicle_ref,
                source_path: None,
            }]
        );
    }

    #[test]
    fn chronicle_prose_finding_yields_semantic_candidate() {
        let (scope, chronicle_ref, run_id, observed_at, valid_from) = chronicle_args();
        let chronicle = serde_json::json!({
            "investigations": ["the retry loop never terminates on timeout"],
        });

        let candidates = chronicle_candidates(
            &chronicle,
            &scope,
            &chronicle_ref,
            run_id,
            observed_at,
            valid_from,
            DataClassification::Internal,
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].class, MemoryClass::Semantic);
    }

    #[test]
    fn chronicle_decision_marker_finding_yields_semantic_candidate() {
        let (scope, chronicle_ref, run_id, observed_at, valid_from) = chronicle_args();
        let chronicle = serde_json::json!({
            "investigations": ["decided to use sqlx over diesel"],
        });

        let candidates = chronicle_candidates(
            &chronicle,
            &scope,
            &chronicle_ref,
            run_id,
            observed_at,
            valid_from,
            DataClassification::Internal,
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].class, MemoryClass::Semantic);
    }

    #[test]
    fn chronicle_change_entry_yields_episodic_candidate_mentioning_run() {
        let (scope, chronicle_ref, run_id, observed_at, valid_from) = chronicle_args();
        let chronicle = serde_json::json!({
            "changes": [{"changeset_id": "cs-1", "artifact": "diff.patch", "byte_length": 128}],
        });

        let candidates = chronicle_candidates(
            &chronicle,
            &scope,
            &chronicle_ref,
            run_id,
            observed_at,
            valid_from,
            DataClassification::Internal,
        );

        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.class, MemoryClass::Episodic);
        assert!(candidate.statement.contains("cs-1"));
        assert!(candidate.statement.contains(&run_id.to_string()));
    }

    #[test]
    fn chronicle_failed_action_yields_failure_candidate_succeeded_yields_none() {
        let (scope, chronicle_ref, run_id, observed_at, valid_from) = chronicle_args();
        let chronicle = serde_json::json!({
            "actions": [
                {"tool": "shell.run", "outcome": "failed", "artifact": null},
                {"tool": "shell.run", "outcome": "succeeded", "artifact": null},
            ],
        });

        let candidates = chronicle_candidates(
            &chronicle,
            &scope,
            &chronicle_ref,
            run_id,
            observed_at,
            valid_from,
            DataClassification::Internal,
        );

        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.class, MemoryClass::Failure);
        assert_eq!(
            candidate.statement,
            format!("shell.run failed in run {run_id}")
        );
    }

    #[test]
    fn chronicle_missing_or_misshaped_fields_yield_no_candidates() {
        let (scope, chronicle_ref, run_id, observed_at, valid_from) = chronicle_args();

        // Completely empty chronicle.
        let empty = serde_json::json!({});
        assert!(chronicle_candidates(
            &empty,
            &scope,
            &chronicle_ref,
            run_id,
            observed_at,
            valid_from.clone(),
            DataClassification::Internal,
        )
        .is_empty());

        // `investigations` is a string, not an array; `changes`/`actions` entries
        // missing their expected keys.
        let malformed = serde_json::json!({
            "investigations": "not an array",
            "changes": [{"artifact": "x"}],
            "actions": [{"tool": "shell.run"}, {"outcome": "failed"}],
        });
        assert!(chronicle_candidates(
            &malformed,
            &scope,
            &chronicle_ref,
            run_id,
            observed_at,
            valid_from,
            DataClassification::Internal,
        )
        .is_empty());
    }

    #[test]
    fn chronicle_per_class_caps_are_enforced() {
        let (scope, chronicle_ref, run_id, observed_at, valid_from) = chronicle_args();
        let findings: Vec<String> = (0..20)
            .map(|i| format!("finding number {i} is a prose sentence worth keeping"))
            .collect();
        let changes: Vec<serde_json::Value> = (0..20)
            .map(|i| serde_json::json!({"changeset_id": format!("cs-{i}"), "byte_length": 1}))
            .collect();
        let actions: Vec<serde_json::Value> = (0..20)
            .map(|i| serde_json::json!({"tool": format!("tool-{i}"), "outcome": "failed"}))
            .collect();
        let chronicle = serde_json::json!({
            "investigations": findings,
            "changes": changes,
            "actions": actions,
        });

        let candidates = chronicle_candidates(
            &chronicle,
            &scope,
            &chronicle_ref,
            run_id,
            observed_at,
            valid_from,
            DataClassification::Internal,
        );

        let count = |class: MemoryClass| candidates.iter().filter(|c| c.class == class).count();
        assert_eq!(count(MemoryClass::Semantic), MAX_CHRONICLE_FINDINGS);
        assert_eq!(count(MemoryClass::Episodic), MAX_CHRONICLE_CHANGES);
        assert_eq!(count(MemoryClass::Failure), MAX_CHRONICLE_FAILURES);
    }

    #[test]
    fn chronicle_statements_are_length_bounded() {
        let (scope, chronicle_ref, run_id, observed_at, valid_from) = chronicle_args();
        let long_line = format!("a prose finding that goes on {}", "x".repeat(500));
        let chronicle = serde_json::json!({ "investigations": [long_line] });

        let candidates = chronicle_candidates(
            &chronicle,
            &scope,
            &chronicle_ref,
            run_id,
            observed_at,
            valid_from,
            DataClassification::Internal,
        );

        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].statement.chars().count() <= SUMMARY_BREADCRUMB_MAX + 1);
    }

    // ----------------------------------------------------------------
    // explicit_proposal_candidates — M2 `\u{1e}` structured-value tail
    // ----------------------------------------------------------------

    #[test]
    fn explicit_proposal_plain_note_unchanged() {
        let session = SessionId::new();
        let events = vec![event(
            1,
            EventBody::NoteAppended {
                text: "memory.propose: use ripgrep".to_string(),
                run_id: None,
            },
        )];

        let candidates =
            explicit_proposal_candidates(&events, &Scope::Session(session), Some(session));
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.class, MemoryClass::Semantic);
        assert_eq!(candidate.statement, "use ripgrep");
        assert_eq!(candidate.structured_value, None);
    }

    #[test]
    fn explicit_proposal_value_tail_populates_structured_value() {
        let session = SessionId::new();
        let events = vec![event(
            1,
            EventBody::NoteAppended {
                text: "memory.propose: db is postgres\u{1e}{\"engine\":\"postgres\"}".to_string(),
                run_id: None,
            },
        )];

        let candidates =
            explicit_proposal_candidates(&events, &Scope::Session(session), Some(session));
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.statement, "db is postgres");
        assert_eq!(
            candidate.structured_value,
            Some(serde_json::json!({"engine": "postgres"}))
        );
    }

    #[test]
    fn explicit_proposal_bad_json_tail_falls_back_to_statement() {
        let session = SessionId::new();
        let events = vec![event(
            1,
            EventBody::NoteAppended {
                text: "memory.propose: x\u{1e}not json".to_string(),
                run_id: None,
            },
        )];

        let candidates =
            explicit_proposal_candidates(&events, &Scope::Session(session), Some(session));
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.structured_value, None);
        assert_eq!(candidate.statement, "x\u{1e}not json");
    }

    #[test]
    fn explicit_proposal_legacy_marker_still_works() {
        let session = SessionId::new();
        let events = vec![event(
            1,
            EventBody::NoteAppended {
                text: "memory: prefer sqlx over diesel".to_string(),
                run_id: None,
            },
        )];

        let candidates =
            explicit_proposal_candidates(&events, &Scope::Session(session), Some(session));
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].statement, "prefer sqlx over diesel");
    }
}
