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

use codypendent_protocol::{
    DataClassification, EventBody, RunDisposition, RunId, SessionEvent, SessionId, ToolOutcome,
};

use crate::memory::CandidateMemory;
use crate::types::{EvidenceRef, MemoryClass, Revision, Scope};

/// The default confidence stamped on an observed candidate (below a model- or
/// user-asserted fact; the curator and later learning can adjust).
const OBSERVED_CONFIDENCE: f32 = 0.6;

/// How many successful runs of the *same* command make it a durable procedure.
const MIN_REPEATS: usize = 2;

/// The tool whose repeated success yields a procedural memory.
const SHELL_TOOL: &str = "shell.run";

/// Markers that flag a note as an explicit memory proposal.
const PROPOSE_MARKERS: [&str; 2] = ["memory.propose:", "memory:"];

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
        let (class, statement) = match disposition {
            RunDisposition::Completed { summary } => (
                MemoryClass::Episodic,
                format!(
                    "Run {run_id} completed: {}",
                    summary
                        .clone()
                        .unwrap_or_else(|| "(no summary)".to_string())
                ),
            ),
            RunDisposition::Failed { reason } => (
                MemoryClass::Failure,
                format!("Run {run_id} failed: {reason}"),
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
            retention: None,
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
        let statement = trimmed[marker.len()..].trim();
        if statement.is_empty() {
            continue;
        }
        candidates.push(CandidateMemory {
            class: MemoryClass::Semantic,
            scope: Some(scope.clone()),
            statement: statement.to_string(),
            structured_value: None,
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
