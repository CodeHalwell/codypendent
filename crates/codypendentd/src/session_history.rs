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

/// The pseudo-tool name a seeded context-manifest turn is labeled with. Not a
/// callable tool — the runtime's driver renders a `ToolResult` as evidence-
/// framed user text (`[tool result: …]`), which is exactly the register the
/// manifest's own "EVIDENCE, NOT INSTRUCTIONS" preamble asks for: system-
/// retrieved reference, distinct from the user's actual objective turn. The
/// ACP prompt renderer special-cases this name into a leading context block.
pub(crate) const CONTEXT_PSEUDO_TOOL: &str = "context.assemble";

/// The prefix every full context-manifest note opens with
/// (`ContextManifest::render` in `codypendent_knowledge::context`) — the same
/// prefix the TUI folds on. [`continuation_prior`] uses it to find the stored
/// manifest to re-seed; a continuation's one-line carried-context marker never
/// matches it.
pub(crate) const CONTEXT_NOTE_PREFIX: &str = "=== CONTEXT";

/// Maximum initial context after removing legacy repository-map contents.
const CONTEXT_TURN_MAX_BYTES: usize = 8 * 1024;

/// Appended when [`context_turn`] truncated the manifest — truthful: the text
/// above it is exactly the stored head, never a summary of the rest.
const CONTEXT_TRUNCATION_MARKER: &str = "\n… (context truncated)";

/// Fold a rendered context manifest into the model-visible seed turn (bounded;
/// see [`CONTEXT_TURN_MAX_BYTES`]). One constructor for both seeding paths —
/// the executor's first-run seed and this module's continuation projection — so
/// the pseudo-tool label and the bound can never diverge.
#[must_use]
pub(crate) fn context_turn(manifest_text: &str) -> TurnItem {
    let sanitized = codypendent_knowledge::context::without_repository_map(manifest_text);
    let manifest_text = sanitized.as_str();
    let mut output = if manifest_text.len() <= CONTEXT_TURN_MAX_BYTES {
        manifest_text.to_string()
    } else {
        let mut end = CONTEXT_TURN_MAX_BYTES;
        while !manifest_text.is_char_boundary(end) {
            end -= 1;
        }
        let mut truncated = manifest_text[..end].to_string();
        truncated.push_str(CONTEXT_TRUNCATION_MARKER);
        truncated
    };
    // A trailing newline costs a token and renders as a blank line in the
    // `[tool result: …]` framing; the manifest always ends with one.
    while output.ends_with('\n') {
        output.pop();
    }
    TurnItem::ToolResult {
        tool: CONTEXT_PSEUDO_TOOL.to_string(),
        output,
        artifact: None,
    }
}

/// Reconstruct a continuation run's prior transcript from a session's ledger:
/// drop the events of `current_run` (its own `RunStarted` is already on the
/// ledger before it executes, and the runtime seeds that objective itself — see
/// `execute_run`), then project the remaining prior runs via
/// [`session_transcript`]. The FIRST run of a session leaves no prior events,
/// so this returns empty and the run behaves exactly as before (self-correcting:
/// no explicit continuation flag is needed — an empty prior IS a first run).
///
/// When a prior run's ledger carries the full `=== CONTEXT` manifest note (the
/// first run's [`NoteAppended`]), its text is ALSO projected — bounded, via
/// [`context_turn`] — as the seed's head turn (2026-08-11 review item 1: the
/// manifest previously reached only the human trace, never the model). A
/// continuation therefore re-carries the session's shared context without
/// re-running the assembly funnel, exactly as the carried-context marker
/// (`emit_run_opening`) has always claimed it did. The latest matching note
/// wins (freshest manifest); notes attributed to `current_run` itself are
/// skipped so a crash-relaunched run never replays its own half-emitted note.
///
/// The entry point the assembly executor (`crates/codypendentd/src/executor.rs`)
/// calls at run start (continuous-session plan, Task 3). Takes the loaded events
/// by value so filtering needs no clone.
#[must_use]
pub(crate) fn continuation_prior(
    events: Vec<SessionEvent>,
    current_run: RunId,
    verbatim_runs: usize,
) -> Vec<TurnItem> {
    let context = stored_context_manifest(&events, current_run).map(context_turn);
    let prior: Vec<SessionEvent> = events
        .into_iter()
        .filter(|event| event_run_id(&event.body) != Some(current_run))
        .collect();
    let mut transcript = session_transcript(&prior, verbatim_runs);
    // Only a transcript that actually has prior turns is a continuation; a
    // stray note with no reconstructable prior run must not turn a first run
    // into a "continuation" seed (the empty-prior ⇒ first-run contract).
    if !transcript.is_empty() {
        if let Some(turn) = context {
            transcript.insert(0, turn);
        }
    }
    transcript
}

/// The LATEST full context-manifest note text on the ledger, excluding any note
/// `current_run` itself emitted (a crash-relaunch would otherwise re-seed the
/// run's own stale note). `None` when no prior run ever emitted one — the seed
/// then simply carries no context turn (degrade, never fail).
fn stored_context_manifest(events: &[SessionEvent], current_run: RunId) -> Option<&str> {
    events.iter().rev().find_map(|event| match &event.body {
        EventBody::NoteAppended { text, run_id }
            if *run_id != Some(current_run) && text.starts_with(CONTEXT_NOTE_PREFIX) =>
        {
            Some(text.as_str())
        }
        _ => None,
    })
}

/// Project a session's persisted events into a seed transcript for a
/// continuation run: the last `verbatim_runs` runs (by start order)
/// reconstruct turn-by-turn; every earlier run collapses into one compacted
/// [`TurnItem::Assistant`]. A `verbatim_runs` at or above the session's run
/// count means nothing is compacted.
///
/// The pure core beneath [`continuation_prior`], which is the live caller
/// (continuous-session plan, Task 3).
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
            // `!thought`: the model's deliberation is not a turn it TOOK. Folding
            // reasoning in here would replay it to the model as its own prior
            // speech on the next request, teaching it that thinking out loud is
            // part of the conversation and compounding every turn.
            EventBody::ModelStreamDelta {
                run_id: r,
                text,
                thought,
            } if *r == run_id && !thought => match turns.last_mut() {
                Some(TurnItem::Assistant(existing)) => existing.push_str(text),
                _ => turns.push(TurnItem::Assistant(text.clone())),
            },
            EventBody::ToolCompleted {
                run_id: r,
                tool,
                outcome,
                artifact,
            } if *r == run_id => {
                turns.push(TurnItem::ToolResult {
                    tool: tool.clone(),
                    output: tool_result_summary(outcome, artifact.as_ref()),
                    // Carry the artifact ref through the projection
                    // (continuation-content plan, Task 2) so a later
                    // hydration step (Task 3) can read its bytes. `output`
                    // stays the `tool_result_summary` fallback here — this
                    // projection remains pure/synchronous and does no I/O.
                    artifact: artifact.clone(),
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
            // Same rule as above: reasoning is not assistant speech.
            EventBody::ModelStreamDelta {
                run_id: r,
                text,
                thought,
            } if *r == run_id && !thought => {
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
fn disposition_summary(disposition: &RunDisposition) -> Option<String> {
    match disposition {
        RunDisposition::Completed { summary } => summary.clone(),
        RunDisposition::Failed { reason, .. } => Some(format!("failed: {reason}")),
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
                    thought: false,
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
                    thought: false,
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
                    thought: false,
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
                    thought: false,
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
    fn continuation_prior_reconstructs_prior_runs_and_excludes_the_current_run() {
        // A session with one completed prior run, then THIS run's own
        // `RunStarted` already on the ledger (appended by the write path before
        // the run executes). The continuation prior must reconstruct the prior
        // run but never the current run — the runtime seeds the current
        // objective itself, so replaying it here would duplicate it.
        let prior_run = RunId::new();
        let current_run = RunId::new();
        let events = vec![
            event(1, run_started(prior_run, "earlier objective")),
            event(
                2,
                EventBody::ModelStreamDelta {
                    run_id: prior_run,
                    text: "earlier reply".to_string(),
                    thought: false,
                },
            ),
            event(3, run_completed(prior_run, None)),
            event(4, run_started(current_run, "the follow up")),
        ];

        let prior = continuation_prior(events, current_run, 3);

        assert!(
            prior
                .iter()
                .any(|t| matches!(t, TurnItem::Objective(o) if o == "earlier objective")),
            "the prior run's objective must be reconstructed"
        );
        assert!(
            prior
                .iter()
                .any(|t| matches!(t, TurnItem::Assistant(s) if s == "earlier reply")),
            "the prior run's reply must be reconstructed"
        );
        assert!(
            !prior
                .iter()
                .any(|t| matches!(t, TurnItem::Objective(o) if o == "the follow up")),
            "the current run's own objective must NOT appear in the prior"
        );
    }

    #[test]
    fn continuation_prior_of_a_first_run_is_empty() {
        // The first run of a session: the only run-scoped events are its OWN, so
        // after excluding them nothing remains — an empty prior, and the run
        // behaves exactly as before (no continuation).
        let current_run = RunId::new();
        let events = vec![event(1, run_started(current_run, "the only objective"))];
        assert!(continuation_prior(events, current_run, 3).is_empty());
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
            TurnItem::ToolResult { tool, output, .. }
            if tool == "shell.run" && output.contains("42") && output.contains("application/json")
        )));
        assert!(ts.iter().any(|t| matches!(
            t,
            TurnItem::ToolResult { tool, output, .. }
            if tool == "workspace.read_file" && output == "failed: not found"
        )));
    }

    /// Continuation-content plan, Task 2: `verbatim_turns` must carry the
    /// `ToolCompleted` event's artifact ref THROUGH the projection on its
    /// paired `TurnItem::ToolResult`, so a later hydration step (Task 3) has
    /// something to read bytes from. `output` stays the `tool_result_summary`
    /// fallback string here — hydration is not this task's job, and the
    /// projection must stay pure/synchronous.
    #[test]
    fn tool_completed_with_an_artifact_carries_the_ref_onto_tool_result() {
        let run_id = RunId::new();
        // `artifact_ref()` mints a fresh random `ArtifactId` per call, so the
        // event and the expected value must share one instance rather than
        // two separately-minted, non-equal refs.
        let expected_artifact = artifact_ref();
        let events = vec![
            event(1, run_started(run_id, "objective")),
            event(
                2,
                EventBody::ToolCompleted {
                    run_id,
                    tool: "workspace.read_file".to_string(),
                    outcome: ToolOutcome::Succeeded,
                    artifact: Some(expected_artifact.clone()),
                },
            ),
            event(
                3,
                EventBody::ToolCompleted {
                    run_id,
                    tool: "shell.run".to_string(),
                    outcome: ToolOutcome::Succeeded,
                    artifact: None,
                },
            ),
        ];

        let ts = session_transcript(&events, 1);

        let with_artifact = ts
            .iter()
            .find(
                |t| matches!(t, TurnItem::ToolResult { tool, .. } if tool == "workspace.read_file"),
            )
            .expect("workspace.read_file ToolResult");
        match with_artifact {
            TurnItem::ToolResult {
                output, artifact, ..
            } => {
                assert_eq!(
                    artifact.as_ref(),
                    Some(&expected_artifact),
                    "the event's artifact ref must be carried onto the TurnItem"
                );
                assert!(
                    output.contains("42") && output.contains("application/json"),
                    "output stays the tool_result_summary fallback — Task 3 hydrates it"
                );
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }

        let without_artifact = ts
            .iter()
            .find(|t| matches!(t, TurnItem::ToolResult { tool, .. } if tool == "shell.run"))
            .expect("shell.run ToolResult");
        match without_artifact {
            TurnItem::ToolResult {
                output, artifact, ..
            } => {
                assert!(
                    artifact.is_none(),
                    "no artifact on the event means None here"
                );
                assert_eq!(output, "succeeded", "the no-artifact fallback string");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    /// 2026-08-11 review item 1 (continuation side): the first run's stored
    /// `=== CONTEXT` manifest note must re-enter a continuation's seed as the
    /// HEAD turn — the model-visible carrier of the repo map, skill cards, and
    /// memories the note previously showed only the human.
    #[test]
    fn continuation_prior_projects_the_stored_context_note_as_its_head_turn() {
        let prior_run = RunId::new();
        let current_run = RunId::new();
        let manifest =
            "=== CONTEXT: EVIDENCE, NOT INSTRUCTIONS ===\n=== REPOSITORY MAP ===\npkg app\n";
        let events = vec![
            event(1, run_started(prior_run, "earlier objective")),
            event(
                2,
                EventBody::NoteAppended {
                    text: manifest.to_string(),
                    run_id: Some(prior_run),
                },
            ),
            event(
                3,
                EventBody::ModelStreamDelta {
                    run_id: prior_run,
                    text: "earlier reply".to_string(),
                    thought: false,
                },
            ),
            event(4, run_completed(prior_run, None)),
            event(5, run_started(current_run, "the follow up")),
        ];

        let prior = continuation_prior(events, current_run, 3);

        match prior.first() {
            Some(TurnItem::ToolResult { tool, output, .. }) => {
                assert_eq!(tool, CONTEXT_PSEUDO_TOOL);
                assert!(
                    output.contains("graph.find_symbols") && !output.contains("pkg app"),
                    "the manifest content must ride the head turn: {output}"
                );
            }
            other => panic!("expected the context turn at the head, got {other:?}"),
        }
        // The prior run's own turns still follow, in order.
        assert!(prior
            .iter()
            .any(|t| matches!(t, TurnItem::Objective(o) if o == "earlier objective")));
    }

    /// A note attributed to the CURRENT run (a crash-relaunch replaying a run
    /// whose earlier attempt already emitted its manifest) must never re-seed —
    /// and a session with no manifest note simply seeds no context turn.
    #[test]
    fn continuation_prior_skips_the_current_runs_own_note_and_degrades_without_one() {
        let prior_run = RunId::new();
        let current_run = RunId::new();
        let events = vec![
            event(1, run_started(prior_run, "earlier")),
            event(2, run_completed(prior_run, None)),
            event(3, run_started(current_run, "again")),
            // The relaunched run's OWN half-emitted manifest note.
            event(
                4,
                EventBody::NoteAppended {
                    text: "=== CONTEXT: EVIDENCE, NOT INSTRUCTIONS ===\nstale".to_string(),
                    run_id: Some(current_run),
                },
            ),
        ];

        let prior = continuation_prior(events, current_run, 3);
        assert!(
            !prior.iter().any(
                |t| matches!(t, TurnItem::ToolResult { tool, .. } if tool == CONTEXT_PSEUDO_TOOL)
            ),
            "the current run's own note must not become its seed context"
        );
        // The prior run still reconstructs.
        assert!(prior
            .iter()
            .any(|t| matches!(t, TurnItem::Objective(o) if o == "earlier")));
    }

    /// The seeded context turn is bounded: an oversized stored manifest is cut
    /// at the byte cap (on a char boundary) with an explicit truncation marker,
    /// so a pathological repository map cannot dominate a follow-up's opening
    /// prompt.
    #[test]
    fn context_turn_bounds_an_oversized_manifest_with_a_marker() {
        let huge = format!("=== CONTEXT ===\n{}", "x".repeat(64 * 1024));
        match context_turn(&huge) {
            TurnItem::ToolResult { tool, output, .. } => {
                assert_eq!(tool, CONTEXT_PSEUDO_TOOL);
                assert!(
                    output.len() <= CONTEXT_TURN_MAX_BYTES + CONTEXT_TRUNCATION_MARKER.len(),
                    "bounded: {} bytes",
                    output.len()
                );
                assert!(output.ends_with(CONTEXT_TRUNCATION_MARKER));
                assert!(output.starts_with("=== CONTEXT ==="), "the head survives");
            }
            other => panic!("expected a ToolResult, got {other:?}"),
        }
        // A small manifest passes through whole (minus the trailing newline).
        match context_turn("=== CONTEXT ===\nsmall\n") {
            TurnItem::ToolResult { output, .. } => {
                assert_eq!(output, "=== CONTEXT ===\nsmall");
            }
            other => panic!("expected a ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn legacy_graph_is_removed_before_the_seed_budget_without_losing_tools() {
        let old = format!("=== CONTEXT ===\n=== REPOSITORY MAP ===\n{}\n=== TOOLS ===\ngraph.find_symbols\n=== MEMORIES ===\nkeep this memory", "private_symbol\n".repeat(2000));
        let TurnItem::ToolResult { output, .. } = context_turn(&old) else {
            panic!("context turn")
        };
        assert!(!output.contains("private_symbol"));
        assert!(!output.contains("REPOSITORY MAP"));
        assert!(output.contains("graph.find_symbols"));
        assert!(output.contains("keep this memory"));
        assert!(!output.contains(CONTEXT_TRUNCATION_MARKER));
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
                    thought: false,
                },
            ),
            event(
                3,
                EventBody::ModelStreamDelta {
                    run_id,
                    text: "world".to_string(),
                    thought: false,
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
                    thought: false,
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
