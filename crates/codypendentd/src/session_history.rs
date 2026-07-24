//! Ledger to seed-transcript projection (continuous-session plan, Task 1).
//!
//! [`session_transcript`] is the pure core of a continuation run's seed: it
//! turns a session's persisted [`SessionEvent`]s into the `Vec<TurnItem>` a
//! later task hands the model as a continuation's starting transcript. Pure
//! by construction — events in, `Vec<TurnItem>` out, no pool, no I/O — so it
//! is unit-testable without a database.
//!
//! ## Hybrid: verbatim recent, compacted older
//!
//! Replaying every run's full transcript forever would make each
//! continuation re-pay the entire session's token cost on every follow-up.
//! The last `verbatim_runs` runs (by start order) are reconstructed
//! turn-by-turn; every earlier run collapses into a single compacted
//! [`TurnItem::Assistant`]. Order is preserved throughout: oldest run first,
//! ledger sequence order within a run.
//!
//! ## Why `RunCompleted.chronicle` is never dereferenced
//!
//! `chronicle` is an [`ArtifactRef`] — a pointer into the artifact store, not
//! inline text — so a pure function over `&[SessionEvent]` cannot read it
//! without I/O. `codypendent_knowledge`'s `run_outcome_candidates` (the memory
//! observer) lives with the identical constraint: it cites the chronicle
//! artifact as evidence but never reads its bytes. Compaction here always
//! takes the equivalent fallback: the run's objective, its coalesced
//! assistant reply (concatenated `ModelStreamDelta` text), and — when present
//! — the `RunDisposition`'s own inline summary/reason text. All of it is real
//! ledger text; nothing is fabricated (the T1/T7 cost-honesty ethos extended
//! to transcript content, not just token counts).
//!
//! ## Why a compacted `Steering` turn carries no text
//!
//! Steering text is delivered to a *live* run over an in-process channel
//! (`RunContext::steering`, drained by `drain_steering` in
//! `codypendent_runtime::agent`) and is never written back into the event
//! body: `EventBody::SteeringApplied` carries only the `run_id`. The TUI's own
//! reducer has the identical gap — `TranscriptEntry::Steering { applied }`
//! has no text field either (`crates/tui/src/state.rs`). A replayed
//! `TurnItem::Steering` is therefore an honest empty-string marker — "steering
//! happened here" — never invented wording.

use std::collections::HashSet;

use codypendent_protocol::{
    ArtifactRef, EventBody, RunDisposition, RunId, SessionEvent, ToolOutcome,
};
use codypendent_runtime::agent::TurnItem;

/// Project a session's persisted events into a seed transcript for a
/// continuation run: the last `verbatim_runs` runs (by start order)
/// reconstruct turn-by-turn; every earlier run collapses into one compacted
/// [`TurnItem::Assistant`]. A `verbatim_runs` at or above the session's run
/// count means nothing is compacted.
///
/// Exercised by this module's tests; not yet called outside them — a later
/// task in the continuous-session plan
/// (`docs/superpowers/plans/2026-07-24-continuous-session.md`) wires a real
/// caller once `RunContext`/`RunLaunch` carry a seed transcript.
/// `cfg_attr(not(test), ...)` (this file and its helpers throughout) mirrors
/// the same not-yet-driven-code idiom already used for
/// `RoutingSelection::node` in `routing.rs`.
#[cfg_attr(not(test), allow(dead_code))]
#[must_use]
pub(crate) fn session_transcript(events: &[SessionEvent], verbatim_runs: usize) -> Vec<TurnItem> {
    let order = run_order(events);
    let verbatim_start = order.len().saturating_sub(verbatim_runs);

    let mut transcript = Vec::new();
    for (index, run_id) in order.into_iter().enumerate() {
        if index < verbatim_start {
            transcript.push(compacted_turn(events, run_id));
        } else {
            transcript.extend(verbatim_turns(events, run_id));
        }
    }
    transcript
}

/// Distinct `run_id`s in first-appearance order. `load_events`
/// (`codypendent_daemon::ledger::load_events`) selects `ORDER BY sequence
/// ASC`, so the slice is already in ledger order and first appearance doubles
/// as run start order.
#[cfg_attr(not(test), allow(dead_code))]
fn run_order(events: &[SessionEvent]) -> Vec<RunId> {
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    for event in events {
        if let Some(run_id) = event_run_id(&event.body) {
            if seen.insert(run_id) {
                order.push(run_id);
            }
        }
    }
    order
}

/// The run a turn-contributing event belongs to, or `None` for an event kind
/// this projection does not consume (session lifecycle, approvals, patches,
/// budget warnings, presence, `RunStateChanged`, `SteeringQueued`, `Unknown`,
/// ...). Scoped to exactly the five variants the plan names.
#[cfg_attr(not(test), allow(dead_code))]
fn event_run_id(body: &EventBody) -> Option<RunId> {
    match body {
        EventBody::RunStarted { run_id, .. }
        | EventBody::ModelStreamDelta { run_id, .. }
        | EventBody::ToolCompleted { run_id, .. }
        | EventBody::SteeringApplied { run_id }
        | EventBody::RunCompleted { run_id, .. } => Some(*run_id),
        _ => None,
    }
}

/// Reconstruct one recent run turn-by-turn, in ledger order: `Objective` at
/// `RunStarted`; `Assistant` text coalesced from consecutive
/// `ModelStreamDelta`s (mirrors the TUI's fold — `AppState::append_model_text`
/// in `crates/tui/src/state.rs` — text extends the trailing `Assistant` turn
/// only when it immediately follows one, so a tool call in between starts a
/// fresh `Assistant` turn afterward); one `ToolResult` summary per
/// `ToolCompleted`; one empty-string `Steering` marker per `SteeringApplied`.
/// `RunCompleted` contributes nothing here (see the module doc) — its
/// disposition is only used when *compacting* an older run.
#[cfg_attr(not(test), allow(dead_code))]
fn verbatim_turns(events: &[SessionEvent], run_id: RunId) -> Vec<TurnItem> {
    let mut turns: Vec<TurnItem> = Vec::new();
    for event in events {
        match &event.body {
            EventBody::RunStarted {
                run_id: r,
                objective,
                ..
            } if *r == run_id => {
                turns.push(TurnItem::Objective(objective.clone()));
            }
            EventBody::ModelStreamDelta { run_id: r, text } if *r == run_id => {
                match turns.last_mut() {
                    Some(TurnItem::Assistant(existing)) => existing.push_str(text),
                    _ => turns.push(TurnItem::Assistant(text.clone())),
                }
            }
            EventBody::ToolCompleted {
                run_id: r,
                tool,
                outcome,
                artifact,
            } if *r == run_id => {
                turns.push(TurnItem::ToolResult {
                    tool: tool.clone(),
                    output: tool_result_summary(outcome, artifact.as_ref()),
                });
            }
            EventBody::SteeringApplied { run_id: r } if *r == run_id => {
                turns.push(TurnItem::Steering(String::new()));
            }
            _ => {}
        }
    }
    turns
}

/// A non-fabricated summary of a tool's outcome: the failure message when it
/// failed (real inline ledger text), otherwise a note of success plus the
/// bulk-output artifact's size/type when one was recorded — never the
/// artifact's actual bytes, which would need I/O this pure function cannot
/// do.
#[cfg_attr(not(test), allow(dead_code))]
fn tool_result_summary(outcome: &ToolOutcome, artifact: Option<&ArtifactRef>) -> String {
    match outcome {
        ToolOutcome::Succeeded => match artifact {
            Some(artifact) => format!(
                "succeeded ({} bytes of {})",
                artifact.byte_length, artifact.media_type
            ),
            None => "succeeded".to_string(),
        },
        ToolOutcome::Failed { message } => format!("failed: {message}"),
        _ => "unknown outcome".to_string(),
    }
}

/// Compact one older run to a single [`TurnItem::Assistant`]: the objective,
/// the run's coalesced assistant reply (all `ModelStreamDelta` text, in
/// order), and — when present — the `RunDisposition`'s own inline text.
/// Never reads `RunCompleted.chronicle`'s bytes (see the module doc).
#[cfg_attr(not(test), allow(dead_code))]
fn compacted_turn(events: &[SessionEvent], run_id: RunId) -> TurnItem {
    let mut objective = String::new();
    let mut assistant = String::new();
    let mut disposition_note: Option<String> = None;

    for event in events {
        match &event.body {
            EventBody::RunStarted {
                run_id: r,
                objective: o,
                ..
            } if *r == run_id => {
                objective = o.clone();
            }
            EventBody::ModelStreamDelta { run_id: r, text } if *r == run_id => {
                assistant.push_str(text);
            }
            EventBody::RunCompleted {
                run_id: r,
                disposition,
                ..
            } if *r == run_id => {
                disposition_note = disposition_summary(disposition);
            }
            _ => {}
        }
    }

    let mut summary = objective;
    if !assistant.is_empty() {
        if !summary.is_empty() {
            summary.push_str(": ");
        }
        summary.push_str(&assistant);
    }
    if let Some(note) = disposition_note {
        if summary.is_empty() {
            summary = note;
        } else {
            summary.push_str(" (");
            summary.push_str(&note);
            summary.push(')');
        }
    }
    TurnItem::Assistant(summary)
}

/// The `RunDisposition`'s own inline text, if it carries any: `Completed`'s
/// optional summary verbatim, or a `failed:`/`cancelled:` note built from a
/// `Failed`'s (always-present) reason or a `Cancelled`'s optional one. `None`
/// when the disposition carries no text of its own (a bare `Completed` or
/// `Cancelled`, or a forward-compat `Unknown`).
#[cfg_attr(not(test), allow(dead_code))]
fn disposition_summary(disposition: &RunDisposition) -> Option<String> {
    match disposition {
        RunDisposition::Completed { summary } => summary.clone(),
        RunDisposition::Failed { reason } => Some(format!("failed: {reason}")),
        RunDisposition::Cancelled { reason } => reason.as_ref().map(|r| format!("cancelled: {r}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use codypendent_protocol::{Actor, AgentMode, ArtifactId, DataClassification};

    use super::*;

    fn event(sequence: u64, body: EventBody) -> SessionEvent {
        SessionEvent {
            sequence,
            occurred_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body,
        }
    }

    fn artifact_ref() -> ArtifactRef {
        ArtifactRef {
            id: ArtifactId::new(),
            media_type: "application/json".to_string(),
            byte_length: 42,
            sha256: "0".repeat(64),
            sensitivity: DataClassification::Internal,
        }
    }

    fn run_started(run_id: RunId, objective: &str) -> EventBody {
        EventBody::RunStarted {
            run_id,
            objective: objective.to_string(),
            mode: AgentMode::Build,
        }
    }

    fn run_completed(run_id: RunId, summary: Option<&str>) -> EventBody {
        EventBody::RunCompleted {
            run_id,
            disposition: RunDisposition::Completed {
                summary: summary.map(str::to_string),
            },
            chronicle: artifact_ref(),
        }
    }

    /// The plan's Task 1 test: a 3-run session where the last 2 runs (B, C)
    /// project verbatim and the oldest (A) compacts to a single summary turn.
    /// `RunCompleted.chronicle` is a bare `ArtifactRef` in the real wire type
    /// (never inline text), so — unlike the plan's pseudocode — run A's "A did
    /// X" travels through `RunDisposition::Completed.summary`, the one inline,
    /// non-fabricated text `RunCompleted` actually carries.
    #[test]
    fn session_transcript_is_verbatim_recent_and_compacted_older() {
        let run_a = RunId::new();
        let run_b = RunId::new();
        let run_c = RunId::new();

        let events = vec![
            // Run A (oldest): compacted — beyond the 2-run verbatim window.
            event(1, run_started(run_a, "first")),
            event(
                2,
                EventBody::ModelStreamDelta {
                    run_id: run_a,
                    text: "A-reply".to_string(),
                },
            ),
            event(3, run_completed(run_a, Some("A did X"))),
            // Run B: verbatim.
            event(4, run_started(run_b, "second")),
            event(
                5,
                EventBody::ModelStreamDelta {
                    run_id: run_b,
                    text: "B-reply".to_string(),
                },
            ),
            event(6, run_completed(run_b, None)),
            // Run C (newest): verbatim.
            event(7, run_started(run_c, "third")),
            event(
                8,
                EventBody::ModelStreamDelta {
                    run_id: run_c,
                    text: "C-reply".to_string(),
                },
            ),
            event(9, run_completed(run_c, None)),
        ];

        let ts = session_transcript(&events, 2);

        // Older run A compacted to a single summary turn carrying its
        // disposition summary.
        assert!(ts
            .iter()
            .any(|t| matches!(t, TurnItem::Assistant(s) if s.contains("A did X"))));
        // Recent runs B & C verbatim: objectives appear as `Objective` turns
        // and replies appear verbatim as `Assistant` turns.
        assert!(ts
            .iter()
            .any(|t| matches!(t, TurnItem::Objective(o) if o == "second")));
        assert!(ts
            .iter()
            .any(|t| matches!(t, TurnItem::Assistant(s) if s == "C-reply")));
        // Run A's objective must NOT appear as its own `Objective` turn — it
        // was compacted, not replayed.
        assert!(!ts
            .iter()
            .any(|t| matches!(t, TurnItem::Objective(o) if o == "first")));

        // Order preserved: compacted A, then B's turns, then C's turns.
        let a_pos = ts
            .iter()
            .position(|t| matches!(t, TurnItem::Assistant(s) if s.contains("A did X")))
            .expect("compacted A turn");
        let b_pos = ts
            .iter()
            .position(|t| matches!(t, TurnItem::Objective(o) if o == "second"))
            .expect("B objective turn");
        let c_pos = ts
            .iter()
            .position(|t| matches!(t, TurnItem::Objective(o) if o == "third"))
            .expect("C objective turn");
        assert!(a_pos < b_pos, "compacted A must precede verbatim B");
        assert!(b_pos < c_pos, "B must precede C");
    }

    #[test]
    fn verbatim_runs_at_or_above_total_compacts_nothing() {
        let run_a = RunId::new();
        let events = vec![
            event(1, run_started(run_a, "only run")),
            event(
                2,
                EventBody::ModelStreamDelta {
                    run_id: run_a,
                    text: "reply".to_string(),
                },
            ),
            event(3, run_completed(run_a, Some("done"))),
        ];

        let ts = session_transcript(&events, 5);

        assert!(ts
            .iter()
            .any(|t| matches!(t, TurnItem::Objective(o) if o == "only run")));
        assert!(ts
            .iter()
            .any(|t| matches!(t, TurnItem::Assistant(s) if s == "reply")));
        // Not compacted: the disposition summary text never appears standalone.
        assert!(!ts
            .iter()
            .any(|t| matches!(t, TurnItem::Assistant(s) if s.contains("done"))));
    }

    #[test]
    fn empty_events_project_an_empty_transcript() {
        assert_eq!(session_transcript(&[], 2), Vec::new());
    }

    #[test]
    fn tool_completed_summarizes_without_fabricating_output() {
        let run_id = RunId::new();
        let events = vec![
            event(1, run_started(run_id, "objective")),
            event(
                2,
                EventBody::ToolCompleted {
                    run_id,
                    tool: "shell.run".to_string(),
                    outcome: ToolOutcome::Succeeded,
                    artifact: Some(artifact_ref()),
                },
            ),
            event(
                3,
                EventBody::ToolCompleted {
                    run_id,
                    tool: "workspace.read_file".to_string(),
                    outcome: ToolOutcome::Failed {
                        message: "not found".to_string(),
                    },
                    artifact: None,
                },
            ),
        ];

        let ts = session_transcript(&events, 1);

        assert!(ts.iter().any(|t| matches!(
            t,
            TurnItem::ToolResult { tool, output }
            if tool == "shell.run" && output.contains("42") && output.contains("application/json")
        )));
        assert!(ts.iter().any(|t| matches!(
            t,
            TurnItem::ToolResult { tool, output }
            if tool == "workspace.read_file" && output == "failed: not found"
        )));
    }

    #[test]
    fn steering_applied_projects_an_empty_marker_never_fabricated_text() {
        let run_id = RunId::new();
        let events = vec![
            event(1, run_started(run_id, "objective")),
            event(2, EventBody::SteeringApplied { run_id }),
        ];

        let ts = session_transcript(&events, 1);

        assert!(ts
            .iter()
            .any(|t| matches!(t, TurnItem::Steering(s) if s.is_empty())));
    }

    #[test]
    fn model_stream_deltas_coalesce_and_a_tool_call_breaks_the_run() {
        let run_id = RunId::new();
        let events = vec![
            event(1, run_started(run_id, "objective")),
            event(
                2,
                EventBody::ModelStreamDelta {
                    run_id,
                    text: "Hello, ".to_string(),
                },
            ),
            event(
                3,
                EventBody::ModelStreamDelta {
                    run_id,
                    text: "world".to_string(),
                },
            ),
            event(
                4,
                EventBody::ToolCompleted {
                    run_id,
                    tool: "shell.run".to_string(),
                    outcome: ToolOutcome::Succeeded,
                    artifact: None,
                },
            ),
            event(
                5,
                EventBody::ModelStreamDelta {
                    run_id,
                    text: "done".to_string(),
                },
            ),
        ];

        let ts = session_transcript(&events, 1);

        // Two contiguous deltas coalesce into one Assistant turn...
        assert!(ts
            .iter()
            .any(|t| matches!(t, TurnItem::Assistant(s) if s == "Hello, world")));
        // ...while a delta after an intervening tool call starts a fresh one.
        assert!(ts
            .iter()
            .any(|t| matches!(t, TurnItem::Assistant(s) if s == "done")));
    }
}
