//! The reducer (STEP 1.12 RULE 3): the one pure state transition.
//!
//! `reduce` performs no I/O. Every daemon event and every input-derived action
//! is folded here, deterministically, into [`AppState`]. Commands the daemon
//! must run are appended to [`AppState::outbox`] as [`Intent`]s for the CLI to
//! dispatch — the reducer never touches a socket. Folding [`EventBody`] into
//! transcript/run/approval state is the core, and it is what the unit tests
//! below exercise.

use codypendent_protocol::{
    Actor, ApprovalDecision, ApprovalScope, BudgetDimension, EventBody, ProposedAction,
    RunDisposition, RunState, SessionEvent,
};

use crate::action::{Action, Intent};
use crate::state::{
    AppState, Overlay, Pane, PatchSummary, PendingApproval, RunView, ToolCard, ToolStatus,
    TranscriptEntry,
};

/// Fold a single [`Action`] into the state. Pure: the only side effect is
/// mutating `state` (including appending intents to its outbox).
pub fn reduce(state: &mut AppState, action: Action) {
    match action {
        Action::DaemonEvent(event) => apply_event(state, *event),
        Action::CatchupSnapshot {
            title,
            closed,
            runs,
        } => {
            // Too far behind for an event replay: seed what the snapshot carries.
            // Runs become stubs (their objective/mode fill in from the next live
            // event) so the session is not blank on reopen.
            state.session_title = Some(title);
            state.session_closed = closed;
            let mode = state.default_mode;
            for run_id in runs {
                state.ensure_run(run_id, String::new(), mode);
            }
        }
        Action::Tick => {
            state.tick = state.tick.wrapping_add(1);
            if let Some((_, expires)) = &state.notice {
                if state.tick >= *expires {
                    state.notice = None;
                }
            }
        }
        // ~5 seconds at the 5 fps tick.
        Action::Notice(text) => state.notice = Some((text, state.tick + 25)),

        Action::CyclePane => state.focus = state.focus.next(),
        Action::FocusPane(pane) => state.focus = pane,
        Action::SelectPrev => nav(state, -1),
        Action::SelectNext => nav(state, 1),
        Action::ScrollPageUp => scroll_page(state, true),
        Action::ScrollPageDown => scroll_page(state, false),
        Action::Expand => expand_selected(state),

        Action::PrevRun => cycle_run(state, -1),
        Action::NextRun => cycle_run(state, 1),
        Action::NewRun => state.overlay = Overlay::NewRun(String::new()),
        Action::Pause => pause_or_resume(state),
        Action::Cancel => request_cancel(state),
        Action::ConfirmCancel => confirm_cancel(state),
        Action::Steer => begin_steering(state),

        Action::Approve(scope) => resolve_focused(state, ApprovalDecision::Approve, scope),
        Action::Reject => resolve_focused(state, ApprovalDecision::Reject, ApprovalScope::Once),

        Action::InputChar(c) => input_char(state, c),
        Action::InputPaste(text) => edit_prompt(state, move |buf| buf.push_str(&text)),
        Action::InputBackspace => edit_prompt(state, |buf| {
            buf.pop();
        }),
        Action::InputSubmit => submit_prompt(state),
        Action::InputCancel => input_cancel(state),

        Action::OpenSkills => {
            state.overlay = match state.overlay {
                Overlay::Skills => Overlay::None,
                _ => Overlay::Skills,
            }
        }
        Action::OpenMemory => {
            state.overlay = match state.overlay {
                Overlay::Memory { .. } => Overlay::None,
                _ => Overlay::Memory { source_open: false },
            }
        }
        Action::OpenSource => open_source(state),

        Action::OpenDocs => {
            state.overlay = match state.overlay {
                Overlay::Docs => Overlay::None,
                _ => Overlay::Docs,
            }
        }
        Action::OpenEdges => {
            state.overlay = match state.overlay {
                Overlay::Edges => Overlay::None,
                _ => Overlay::Edges,
            }
        }
        Action::OpenPalette => {
            state.overlay = match state.overlay {
                Overlay::Palette { .. } => Overlay::None,
                _ => Overlay::Palette {
                    query: String::new(),
                    selected: 0,
                },
            }
        }
        Action::ToggleLayout => state.layout = state.layout.toggled(),

        Action::Help => {
            state.overlay = match state.overlay {
                Overlay::Help => Overlay::None,
                _ => Overlay::Help,
            }
        }
        Action::Detach => state.should_detach = true,
        Action::Dismiss => state.overlay = Overlay::None,
        Action::NoOp => {}
    }
}

/// Fold one durable event into run / transcript / approval state.
fn apply_event(state: &mut AppState, event: SessionEvent) {
    let SessionEvent { actor, body, .. } = event;

    // Learn the serving model from any agent-authored event.
    if let Actor::Agent { run_id, model, .. } = &actor {
        let (rid, model) = (*run_id, model.clone());
        if let Some(run) = state.run_mut(rid) {
            run.model = Some(model);
        }
    }

    match body {
        EventBody::SessionCreated { title } => state.session_title = Some(title),
        EventBody::NoteAppended { text, run_id } => {
            // A run-scoped note (context manifest, curated memory) is routed to
            // its own run so it can't land on whatever run happens to be selected
            // when runs interleave (issue #6 item 3); a session-level note (no
            // run_id) still attaches to the focused run.
            let target = match run_id {
                Some(run_id) => state.run_mut(run_id),
                None => state.selected_run_mut(),
            };
            if let Some(run) = target {
                AppState::push_entry(run, TranscriptEntry::Note { text });
            }
        }
        EventBody::SessionClosed => state.session_closed = true,

        EventBody::RunStarted {
            run_id,
            objective,
            mode,
        } => {
            let run = state.ensure_run(run_id, objective, mode);
            run.state = RunState::Preparing;
        }
        EventBody::RunStateChanged { run_id, state: rs } => {
            if let Some(run) = state.run_mut(run_id) {
                run.state = rs;
            }
        }
        EventBody::ModelStreamDelta { run_id, text } => {
            if let Some(run) = state.run_mut(run_id) {
                AppState::append_model_text(run, &text);
            }
        }
        EventBody::ToolProposed {
            run_id,
            approval_id,
            action,
        } => {
            if let Some(run) = state.run_mut(run_id) {
                AppState::push_entry(
                    run,
                    TranscriptEntry::Tool(Box::new(ToolCard {
                        tool: String::new(),
                        status: ToolStatus::Proposed,
                        action: Some(action),
                        args_digest: None,
                        outcome: None,
                        artifact: None,
                        approval_id: Some(approval_id),
                        expanded: false,
                    })),
                );
            }
            // Backfill the run link onto a matching pending approval.
            if let Some(pending) = state
                .pending_approvals
                .iter_mut()
                .find(|p| p.approval_id == approval_id)
            {
                pending.run_id = Some(run_id);
            }
        }
        EventBody::ToolStarted {
            run_id,
            tool,
            args_digest,
        } => {
            if let Some(run) = state.run_mut(run_id) {
                match last_card(run, |c| c.status == ToolStatus::Proposed) {
                    Some(card) => {
                        card.tool = tool;
                        card.args_digest = Some(args_digest);
                        card.status = ToolStatus::Running;
                    }
                    None => run
                        .transcript
                        .push(TranscriptEntry::Tool(Box::new(ToolCard {
                            tool,
                            status: ToolStatus::Running,
                            action: None,
                            args_digest: Some(args_digest),
                            outcome: None,
                            artifact: None,
                            approval_id: None,
                            expanded: false,
                        }))),
                }
            }
        }
        EventBody::ToolCompleted {
            run_id,
            tool,
            outcome,
            artifact,
        } => {
            if let Some(run) = state.run_mut(run_id) {
                match last_card(run, |c| c.status != ToolStatus::Completed) {
                    Some(card) => {
                        if card.tool.is_empty() {
                            card.tool = tool;
                        }
                        card.status = ToolStatus::Completed;
                        card.outcome = Some(outcome);
                        card.artifact = artifact;
                    }
                    None => run
                        .transcript
                        .push(TranscriptEntry::Tool(Box::new(ToolCard {
                            tool,
                            status: ToolStatus::Completed,
                            action: None,
                            args_digest: None,
                            outcome: Some(outcome),
                            artifact,
                            approval_id: None,
                            expanded: false,
                        }))),
                }
            }
        }
        EventBody::PatchProposed {
            run_id,
            changeset_id,
            artifact,
        } => {
            if let Some(run) = state.run_mut(run_id) {
                AppState::push_entry(
                    run,
                    TranscriptEntry::Patch(PatchSummary {
                        changeset_id,
                        artifact,
                        expanded: false,
                    }),
                );
            }
        }
        EventBody::ApprovalRequested {
            approval_id,
            action,
            risk,
        } => {
            let run_id = run_of_approval(state, approval_id);
            state.pending_approvals.push(PendingApproval {
                approval_id,
                action,
                risk,
                run_id,
            });
        }
        EventBody::ApprovalResolved { approval_id, .. } => {
            state
                .pending_approvals
                .retain(|p| p.approval_id != approval_id);
            clamp(&mut state.selected_approval, state.pending_approvals.len());
        }
        EventBody::SteeringQueued { run_id } => {
            if let Some(run) = state.run_mut(run_id) {
                run.transcript
                    .push(TranscriptEntry::Steering { applied: false });
            }
        }
        EventBody::SteeringApplied { run_id } => {
            if let Some(run) = state.run_mut(run_id) {
                let marked = run.transcript.iter_mut().rev().find_map(|e| match e {
                    TranscriptEntry::Steering { applied } if !*applied => Some(applied),
                    _ => None,
                });
                match marked {
                    Some(applied) => *applied = true,
                    None => run
                        .transcript
                        .push(TranscriptEntry::Steering { applied: true }),
                }
            }
        }
        EventBody::BudgetWarning {
            run_id,
            dimension,
            used,
            limit,
        } => {
            if let Some(run) = state.run_mut(run_id) {
                match dimension {
                    BudgetDimension::Tokens => {
                        let pct = used.saturating_mul(100) / limit.max(1);
                        run.context_percent = Some(pct.min(100) as u16);
                    }
                    BudgetDimension::Cost => run.cost_minor = Some(used),
                    _ => {}
                }
                AppState::push_entry(
                    run,
                    TranscriptEntry::Budget {
                        dimension,
                        used,
                        limit,
                    },
                );
            }
        }
        EventBody::RunCompleted {
            run_id,
            disposition,
            ..
        } => {
            if let Some(run) = state.run_mut(run_id) {
                run.state = terminal_state(&disposition);
                AppState::push_entry(
                    run,
                    TranscriptEntry::Completed {
                        disposition: disposition.clone(),
                    },
                );
                run.disposition = Some(disposition);
            }
        }

        // Presence: another client joined or left this session (STEP 3.7). A
        // transient status notice, not a transcript entry — presence is
        // ambient, and the flagship handoff demo must not read as
        // "unsupported event".
        EventBody::ClientPresenceChanged {
            client_id,
            role,
            present,
        } => {
            let id = client_id.to_string();
            let short = id.get(..8).unwrap_or(&id);
            let verb = if present { "joined" } else { "left" };
            state.notice = Some((
                format!("client {short} {verb} ({})", role_label(role)),
                state.tick + 25,
            ));
        }

        // `Unknown` and any future event type this build predates render a
        // placeholder and keep going (protocol RULE 1).
        _ => {
            if let Some(run) = state.selected_run_mut() {
                AppState::push_entry(
                    run,
                    TranscriptEntry::Unsupported {
                        label: "unsupported event".to_owned(),
                    },
                );
            }
        }
    }
}

/// A short human label for a client role (presence notices).
fn role_label(role: codypendent_protocol::ClientRole) -> &'static str {
    use codypendent_protocol::ClientRole;
    match role {
        ClientRole::Observer => "observer",
        ClientRole::Contributor => "contributor",
        ClientRole::Controller => "controller",
        ClientRole::Approver => "approver",
        _ => "unknown role",
    }
}

/// Find the most recent tool card matching `pred`, mutably.
fn last_card(run: &mut RunView, pred: impl Fn(&ToolCard) -> bool) -> Option<&mut ToolCard> {
    run.transcript.iter_mut().rev().find_map(|e| match e {
        TranscriptEntry::Tool(card) if pred(card) => Some(card.as_mut()),
        _ => None,
    })
}

/// Which run (if any) owns a proposed approval, inferred from tool cards.
fn run_of_approval(
    state: &AppState,
    approval_id: codypendent_protocol::ApprovalId,
) -> Option<codypendent_protocol::RunId> {
    state.runs.iter().find_map(|run| {
        run.transcript.iter().find_map(|e| match e {
            TranscriptEntry::Tool(card) if card.approval_id == Some(approval_id) => {
                Some(run.run_id)
            }
            _ => None,
        })
    })
}

fn terminal_state(disposition: &RunDisposition) -> RunState {
    match disposition {
        RunDisposition::Completed { .. } => RunState::Completed,
        RunDisposition::Failed { .. } => RunState::Failed,
        RunDisposition::Cancelled { .. } => RunState::Cancelled,
        _ => RunState::Unknown,
    }
}

/// Move the selection / scroll by `delta` (-1 or +1). When a knowledge browser
/// is open it drives that browser's list; otherwise it drives the focused pane.
fn nav(state: &mut AppState, delta: i32) {
    match state.overlay {
        Overlay::Skills => {
            step(&mut state.selected_skill, state.skills.len(), delta);
            return;
        }
        Overlay::Memory { .. } => {
            step(&mut state.selected_memory, state.memories.len(), delta);
            // Moving to a different memory collapses any revealed source.
            state.overlay = Overlay::Memory { source_open: false };
            return;
        }
        Overlay::Docs => {
            step(&mut state.selected_doc, state.docs.len(), delta);
            return;
        }
        Overlay::Edges => {
            step(&mut state.selected_edge, state.edges.len(), delta);
            return;
        }
        Overlay::Palette {
            ref query,
            ref mut selected,
        } => {
            let count = crate::palette::filtered_len(query);
            step(selected, count, delta);
            return;
        }
        _ => {}
    }
    // Base view: a pending approval owns the arrows (move between stacked
    // approvals). Otherwise the composer is active and the input layer routes
    // arrows to scroll / run-switch, so this legacy pane path is inert.
    if state.show_approval_modal() {
        step(
            &mut state.selected_approval,
            state.pending_approvals.len(),
            delta,
        );
        return;
    }
    match state.focus {
        Pane::Sessions => step(&mut state.selected_run, state.runs.len(), delta),
        Pane::Approvals => step(
            &mut state.selected_approval,
            state.pending_approvals.len(),
            delta,
        ),
        Pane::Transcript => {
            let idx = state.selected_run;
            if let Some(run) = state.runs.get_mut(idx) {
                step(&mut run.transcript_selected, run.transcript.len(), delta);
                run.scroll = run.transcript_selected.min(usize::from(u16::MAX)) as u16;
            }
        }
    }
}

fn scroll_page(state: &mut AppState, up: bool) {
    let idx = state.selected_run;
    if let Some(run) = state.runs.get_mut(idx) {
        const PAGE: u16 = 10;
        run.scroll = if up {
            run.scroll.saturating_sub(PAGE)
        } else {
            run.scroll.saturating_add(PAGE)
        };
    }
}

fn expand_selected(state: &mut AppState) {
    // In the memory browser, `Enter` opens the focused memory's source.
    if matches!(state.overlay, Overlay::Memory { .. }) {
        open_source(state);
        return;
    }
    if state.focus != Pane::Transcript {
        return;
    }
    let idx = state.selected_run;
    if let Some(run) = state.runs.get_mut(idx) {
        if let Some(entry) = run.transcript.get_mut(run.transcript_selected) {
            match entry {
                TranscriptEntry::Tool(card) => card.expanded = !card.expanded,
                TranscriptEntry::Patch(patch) => patch.expanded = !patch.expanded,
                _ => {}
            }
        }
    }
}

/// Reveal the focused memory's source in the memory browser. A no-op unless the
/// memory browser is open with at least one memory to open. The TUI does no I/O,
/// so "open" flips the overlay's `source_open` flag; the renderer then surfaces
/// the full source string (a real file-open is the CLI's job later).
fn open_source(state: &mut AppState) {
    if matches!(state.overlay, Overlay::Memory { .. }) && !state.memories.is_empty() {
        state.overlay = Overlay::Memory { source_open: true };
    }
}

fn pause_or_resume(state: &mut AppState) {
    let Some(run) = state.selected_run() else {
        return;
    };
    let run_id = run.run_id;
    let intent = match run.state {
        RunState::Paused => Some(Intent::ResumeRun { run_id }),
        RunState::Running | RunState::Preparing | RunState::Queued => {
            Some(Intent::PauseRun { run_id })
        }
        _ => None,
    };
    if let Some(intent) = intent {
        state.outbox.push(intent);
    }
}

fn request_cancel(state: &mut AppState) {
    let Some(run) = state.selected_run() else {
        return;
    };
    if !is_terminal(run.state) {
        state.overlay = Overlay::ConfirmCancel;
    }
}

fn confirm_cancel(state: &mut AppState) {
    if !matches!(state.overlay, Overlay::ConfirmCancel) {
        return;
    }
    state.overlay = Overlay::None;
    if let Some(run) = state.selected_run() {
        let run_id = run.run_id;
        state.outbox.push(Intent::CancelRun { run_id });
    }
}

fn begin_steering(state: &mut AppState) {
    if state.selected_run().is_some() {
        state.overlay = Overlay::Steering(String::new());
    }
}

fn resolve_focused(state: &mut AppState, decision: ApprovalDecision, scope: ApprovalScope) {
    if let Some(pending) = state.focused_approval() {
        let approval_id = pending.approval_id;
        state.outbox.push(Intent::ResolveApproval {
            approval_id,
            decision,
            scope,
        });
    }
}

fn edit_prompt(state: &mut AppState, edit: impl FnOnce(&mut String)) {
    match &mut state.overlay {
        Overlay::NewRun(buf) | Overlay::Steering(buf) => edit(buf),
        // Editing the palette query changes the filtered set, so the selection
        // returns to the top rather than pointing past the new results.
        Overlay::Palette { query, selected } => {
            edit(query);
            *selected = 0;
        }
        // The base view: text lands in the persistent composer draft.
        Overlay::None => edit(&mut state.composer),
        _ => {}
    }
}

/// A typed character. In the base view `/` on an *empty* composer opens the
/// command palette (the Codex-style slash entry); every other key extends the
/// active text buffer.
fn input_char(state: &mut AppState, c: char) {
    if c == '/' && matches!(state.overlay, Overlay::None) && state.composer.is_empty() {
        state.overlay = Overlay::Palette {
            query: String::new(),
            selected: 0,
        };
        return;
    }
    edit_prompt(state, |buf| buf.push(c));
}

/// `Esc`: clear the composer draft in the base view, or close the active overlay.
fn input_cancel(state: &mut AppState) {
    if matches!(state.overlay, Overlay::None) {
        state.composer.clear();
    } else {
        state.overlay = Overlay::None;
    }
}

/// Switch the conversation to another run (`Ctrl-↑/↓`), clamping at the ends.
fn cycle_run(state: &mut AppState, delta: i32) {
    step(&mut state.selected_run, state.runs.len(), delta);
}

fn submit_prompt(state: &mut AppState) {
    match std::mem::take(&mut state.overlay) {
        Overlay::NewRun(text) => {
            let objective = text.trim().to_owned();
            if !objective.is_empty() {
                state.outbox.push(Intent::StartRun {
                    objective,
                    mode: state.default_mode,
                });
            }
        }
        Overlay::Steering(text) => {
            let text = text.trim().to_owned();
            let run_id = state.selected_run().map(|r| r.run_id);
            if let (false, Some(run_id)) = (text.is_empty(), run_id) {
                state.outbox.push(Intent::QueueSteering { run_id, text });
            }
        }
        // `mem::take` already closed the palette (left `None`); run the
        // highlighted command, which may open its own overlay.
        Overlay::Palette { query, selected } => {
            if let Some(entry) = crate::palette::filtered(&query).get(selected) {
                run_palette_command(state, entry.command);
            }
        }
        // Base view (`mem::take` left `None`): send the composer. A live run is
        // steered; otherwise the message starts a fresh run. The draft clears
        // either way.
        Overlay::None => {
            let text = state.composer.trim().to_owned();
            if !text.is_empty() {
                if state.selected_run_is_active() {
                    if let Some(run_id) = state.selected_run().map(|r| r.run_id) {
                        state.outbox.push(Intent::QueueSteering { run_id, text });
                    }
                } else {
                    state.outbox.push(Intent::StartRun {
                        objective: text,
                        mode: state.default_mode,
                    });
                }
            }
            state.composer.clear();
        }
        // Nothing to submit; restore the (non-text) overlay we took.
        other => state.overlay = other,
    }
}

/// Run a command chosen from the palette. Each maps onto the same effect its
/// single-key binding produces — the palette is a front door to the existing
/// commands, not a second code path. The palette overlay is already closed when
/// this runs, so a command that opens its own overlay simply sets it.
fn run_palette_command(state: &mut AppState, command: crate::palette::PaletteCommand) {
    use crate::palette::PaletteCommand;
    match command {
        PaletteCommand::NewRun => state.overlay = Overlay::NewRun(String::new()),
        PaletteCommand::Steer => begin_steering(state),
        PaletteCommand::PauseResume => pause_or_resume(state),
        PaletteCommand::Cancel => request_cancel(state),
        PaletteCommand::Skills => state.overlay = Overlay::Skills,
        PaletteCommand::Memory => state.overlay = Overlay::Memory { source_open: false },
        PaletteCommand::Docs => state.overlay = Overlay::Docs,
        PaletteCommand::Edges => state.overlay = Overlay::Edges,
        PaletteCommand::ToggleLayout => state.layout = state.layout.toggled(),
        PaletteCommand::Help => state.overlay = Overlay::Help,
        PaletteCommand::Detach => state.should_detach = true,
    }
}

fn is_terminal(rs: RunState) -> bool {
    matches!(
        rs,
        RunState::Completed | RunState::Failed | RunState::Cancelled
    )
}

/// Move an index within `[0, len)` by `delta`, clamping at the ends.
fn step(index: &mut usize, len: usize, delta: i32) {
    if len == 0 {
        *index = 0;
        return;
    }
    let max = len - 1;
    if delta < 0 {
        *index = index.saturating_sub(1);
    } else {
        *index = (*index + 1).min(max);
    }
}

/// Clamp an index to be a valid selection for a list of `len` items.
fn clamp(index: &mut usize, len: usize) {
    if len == 0 {
        *index = 0;
    } else if *index >= len {
        *index = len - 1;
    }
}

// A convenience the render layer and tests reuse: a human label for a proposed
// action's requested capability. Kept next to the reducer because it mirrors the
// event → state mapping.
#[must_use]
pub(crate) fn capability_label(action: &ProposedAction) -> String {
    match action {
        ProposedAction::ReadFiles { paths } => format!("FileRead ({} path(s))", paths.len()),
        ProposedAction::WritePatch { .. } => "FileWrite (apply patch)".to_owned(),
        ProposedAction::ExecuteCommand { program, .. } => format!("CommandExecute ({program})"),
        ProposedAction::NetworkRequest { destination } => format!("NetworkConnect ({destination})"),
        ProposedAction::GitCommit { repository } => format!("GitCommit ({repository})"),
        ProposedAction::GitPush { remote, branch } => format!("GitPush ({remote} {branch})"),
        _ => "unsupported capability".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use codypendent_protocol::{
        AgentMode, ApprovalId, ArtifactId, ArtifactRef, ChangeSetId, DataClassification, ModelId,
        Risk, RiskLevel, RunId, ToolOutcome,
    };

    fn agent_actor(run_id: RunId) -> Actor {
        Actor::Agent {
            agent_id: codypendent_protocol::AgentId::new(),
            run_id,
            model: ModelId("gpt-5.1-codex".to_owned()),
        }
    }

    fn ev(actor: Actor, body: EventBody) -> Action {
        Action::daemon_event(SessionEvent {
            sequence: 1,
            occurred_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor,
            body,
        })
    }

    fn system_ev(body: EventBody) -> Action {
        ev(Actor::System, body)
    }

    fn artifact() -> ArtifactRef {
        ArtifactRef {
            id: ArtifactId::new(),
            media_type: "text/x-diff".to_owned(),
            byte_length: 10,
            sha256: "0".repeat(64),
            sensitivity: DataClassification::Internal,
        }
    }

    #[test]
    fn run_started_then_state_changed_updates_run_state() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "diagnose".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        assert_eq!(s.runs.len(), 1);
        assert_eq!(s.runs[0].state, RunState::Preparing);
        assert_eq!(s.runs[0].objective, "diagnose");

        reduce(
            &mut s,
            system_ev(EventBody::RunStateChanged {
                run_id,
                state: RunState::Running,
            }),
        );
        assert_eq!(s.runs[0].state, RunState::Running);
    }

    fn note_count(s: &AppState, run_id: RunId) -> usize {
        s.runs
            .iter()
            .find(|r| r.run_id == run_id)
            .map(|r| {
                r.transcript
                    .iter()
                    .filter(|e| matches!(e, TranscriptEntry::Note { .. }))
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn a_run_scoped_note_lands_on_its_run_not_the_selected_one() {
        // Two runs; `ensure_run` selects the most-recently-started, so B is
        // focused. This is exactly the interleaving that misrouted run-scoped
        // notes before issue #6 item 3.
        let mut s = AppState::new();
        let run_a = RunId::new();
        let run_b = RunId::new();
        for (run_id, objective) in [(run_a, "a"), (run_b, "b")] {
            reduce(
                &mut s,
                system_ev(EventBody::RunStarted {
                    run_id,
                    objective: objective.to_owned(),
                    mode: AgentMode::Build,
                }),
            );
        }
        assert_eq!(
            s.selected_run().map(|r| r.run_id),
            Some(run_b),
            "B is the selected run"
        );

        // A run-scoped note for A must attach to A even though B is selected.
        reduce(
            &mut s,
            system_ev(EventBody::NoteAppended {
                text: "context for A".to_owned(),
                run_id: Some(run_a),
            }),
        );
        assert_eq!(note_count(&s, run_a), 1, "A's note landed on A");
        assert_eq!(note_count(&s, run_b), 0, "B did not receive A's note");

        // A session-level note (no run_id) still attaches to the focused run.
        reduce(
            &mut s,
            system_ev(EventBody::NoteAppended {
                text: "session note".to_owned(),
                run_id: None,
            }),
        );
        assert_eq!(
            note_count(&s, run_b),
            1,
            "session note went to the selected run"
        );
        assert_eq!(
            note_count(&s, run_a),
            1,
            "A is unchanged by the session note"
        );
    }

    #[test]
    fn catchup_snapshot_seeds_title_and_run_stubs() {
        // A too-far-behind reopen folds the projection, not events: the title and
        // a stub per active run so the session is not blank.
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            Action::CatchupSnapshot {
                title: "long session".to_owned(),
                closed: false,
                runs: vec![run_id],
            },
        );
        assert_eq!(s.session_title.as_deref(), Some("long session"));
        assert!(!s.session_closed);
        assert_eq!(s.runs.len(), 1);
        assert_eq!(s.runs[0].run_id, run_id);
    }

    #[test]
    fn model_stream_deltas_coalesce_and_learn_model() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "o".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            ev(
                agent_actor(run_id),
                EventBody::ModelStreamDelta {
                    run_id,
                    text: "Hello, ".to_owned(),
                },
            ),
        );
        reduce(
            &mut s,
            ev(
                agent_actor(run_id),
                EventBody::ModelStreamDelta {
                    run_id,
                    text: "world".to_owned(),
                },
            ),
        );
        // Two deltas coalesce into one transcript entry.
        assert_eq!(s.runs[0].transcript.len(), 1);
        match &s.runs[0].transcript[0] {
            TranscriptEntry::Model { text } => assert_eq!(text, "Hello, world"),
            other => panic!("expected coalesced Model entry, got {other:?}"),
        }
        // The serving model was learned from the agent actor.
        assert_eq!(s.runs[0].model, Some(ModelId("gpt-5.1-codex".to_owned())));
    }

    #[test]
    fn approval_requested_adds_and_resolved_removes() {
        let mut s = AppState::new();
        let approval_id = ApprovalId::new();
        reduce(
            &mut s,
            system_ev(EventBody::ApprovalRequested {
                approval_id,
                action: ProposedAction::ExecuteCommand {
                    program: "cargo".to_owned(),
                    args: vec!["test".to_owned()],
                    environment: Vec::new(),
                    cwd: None,
                },
                risk: Risk {
                    level: RiskLevel::Medium,
                    reasons: vec!["runs a command".to_owned()],
                },
            }),
        );
        assert_eq!(s.pending_approvals.len(), 1);
        assert!(s.show_approval_modal());

        reduce(
            &mut s,
            system_ev(EventBody::ApprovalResolved {
                approval_id,
                decision: ApprovalDecision::Approve,
            }),
        );
        assert!(s.pending_approvals.is_empty());
        assert!(!s.show_approval_modal());
    }

    #[test]
    fn tool_lifecycle_folds_into_one_card() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        let approval_id = ApprovalId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "o".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ToolProposed {
                run_id,
                approval_id,
                action: ProposedAction::ExecuteCommand {
                    program: "cargo".to_owned(),
                    args: vec!["test".to_owned()],
                    environment: Vec::new(),
                    cwd: None,
                },
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ToolStarted {
                run_id,
                tool: "shell.run".to_owned(),
                args_digest: "abc".to_owned(),
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ToolCompleted {
                run_id,
                tool: "shell.run".to_owned(),
                outcome: ToolOutcome::Succeeded,
                artifact: Some(artifact()),
            }),
        );
        // Proposed → Started → Completed collapses to a single card.
        let tools: Vec<_> = s.runs[0]
            .transcript
            .iter()
            .filter(|e| matches!(e, TranscriptEntry::Tool(_)))
            .collect();
        assert_eq!(tools.len(), 1);
        let TranscriptEntry::Tool(card) = tools[0] else {
            unreachable!()
        };
        assert_eq!(card.tool, "shell.run");
        assert_eq!(card.status, ToolStatus::Completed);
        assert_eq!(card.outcome, Some(ToolOutcome::Succeeded));
        assert!(card.artifact.is_some());
    }

    #[test]
    fn budget_warning_projects_context_and_cost() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "o".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::BudgetWarning {
                run_id,
                dimension: BudgetDimension::Tokens,
                used: 90_000,
                limit: 100_000,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::BudgetWarning {
                run_id,
                dimension: BudgetDimension::Cost,
                used: 125,
                limit: 500,
            }),
        );
        assert_eq!(s.runs[0].context_percent, Some(90));
        assert_eq!(s.runs[0].cost_minor, Some(125));
        let status = s.status();
        assert_eq!(status.context_percent, Some(90));
        assert_eq!(status.cost_minor, Some(125));
        assert_eq!(status.mode, Some(AgentMode::Build));
    }

    #[test]
    fn run_completed_sets_terminal_state_and_disposition() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "o".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Failed {
                    reason: "boom".to_owned(),
                },
                chronicle: artifact(),
            }),
        );
        assert_eq!(s.runs[0].state, RunState::Failed);
        assert!(matches!(
            s.runs[0].disposition,
            Some(RunDisposition::Failed { .. })
        ));
    }

    #[test]
    fn approve_emits_resolve_intent_but_does_not_remove_locally() {
        let mut s = AppState::new();
        let approval_id = ApprovalId::new();
        reduce(
            &mut s,
            system_ev(EventBody::ApprovalRequested {
                approval_id,
                action: ProposedAction::GitCommit {
                    repository: "acme/widget".to_owned(),
                },
                risk: Risk {
                    level: RiskLevel::High,
                    reasons: vec![],
                },
            }),
        );
        reduce(&mut s, Action::Approve(ApprovalScope::Run));
        // Intent queued for the CLI; state unchanged until the daemon confirms.
        assert_eq!(s.pending_approvals.len(), 1);
        let intents = s.drain_outbox();
        assert_eq!(intents.len(), 1);
        match &intents[0] {
            Intent::ResolveApproval {
                approval_id: id,
                decision,
                scope,
            } => {
                assert_eq!(*id, approval_id);
                assert_eq!(*decision, ApprovalDecision::Approve);
                assert_eq!(*scope, ApprovalScope::Run);
            }
            other => panic!("expected ResolveApproval, got {other:?}"),
        }
        assert!(s.outbox.is_empty(), "outbox drained");
    }

    #[test]
    fn new_run_prompt_submits_start_run_intent() {
        let mut s = AppState::new();
        reduce(&mut s, Action::NewRun);
        assert_eq!(s.input_mode(), crate::state::InputMode::Editing);
        for c in "fix the test".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert!(matches!(s.overlay, Overlay::None));
        let intents = s.drain_outbox();
        assert_eq!(
            intents,
            vec![Intent::StartRun {
                objective: "fix the test".to_owned(),
                mode: AgentMode::Build,
            }]
        );
    }

    #[test]
    fn cancel_requires_confirmation_then_emits_intent() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "o".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunStateChanged {
                run_id,
                state: RunState::Running,
            }),
        );
        reduce(&mut s, Action::Cancel);
        assert!(matches!(s.overlay, Overlay::ConfirmCancel));
        assert!(s.outbox.is_empty(), "no cancel until confirmed");
        reduce(&mut s, Action::ConfirmCancel);
        assert!(matches!(s.overlay, Overlay::None));
        assert_eq!(s.drain_outbox(), vec![Intent::CancelRun { run_id }]);
    }

    #[test]
    fn pause_toggles_between_pause_and_resume() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "o".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunStateChanged {
                run_id,
                state: RunState::Running,
            }),
        );
        reduce(&mut s, Action::Pause);
        assert_eq!(s.drain_outbox(), vec![Intent::PauseRun { run_id }]);
        reduce(
            &mut s,
            system_ev(EventBody::RunStateChanged {
                run_id,
                state: RunState::Paused,
            }),
        );
        reduce(&mut s, Action::Pause);
        assert_eq!(s.drain_outbox(), vec![Intent::ResumeRun { run_id }]);
    }

    #[test]
    fn unknown_event_renders_placeholder_not_crash() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "o".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(&mut s, system_ev(EventBody::Unknown));
        assert!(s.runs[0]
            .transcript
            .iter()
            .any(|e| matches!(e, TranscriptEntry::Unsupported { .. })));
    }

    fn skill(name: &str, permissions: &[&str]) -> crate::state::SkillCard {
        crate::state::SkillCard {
            name: name.to_owned(),
            kind: "skill".to_owned(),
            scope: "repository".to_owned(),
            trust: "first-party".to_owned(),
            status: "active".to_owned(),
            risk: "medium".to_owned(),
            description: "a test skill".to_owned(),
            permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    fn memory(statement: &str, source: &str) -> crate::state::MemoryCard {
        crate::state::MemoryCard {
            statement: statement.to_owned(),
            class: "semantic".to_owned(),
            scope: "repository".to_owned(),
            revision: "79acbf1".to_owned(),
            observed: "2026-07-14".to_owned(),
            confidence: 1.0,
            source: source.to_owned(),
        }
    }

    #[test]
    fn open_skills_toggles_the_studio_overlay() {
        let mut s = AppState::new();
        s.skills = vec![skill("rust.fix-ci", &["command: cargo"])];
        reduce(&mut s, Action::OpenSkills);
        assert_eq!(s.overlay, Overlay::Skills);
        assert_eq!(s.input_mode(), crate::state::InputMode::Normal);
        // Toggling closes it again.
        reduce(&mut s, Action::OpenSkills);
        assert_eq!(s.overlay, Overlay::None);
    }

    #[test]
    fn open_memory_toggles_the_memory_overlay() {
        let mut s = AppState::new();
        s.memories = vec![memory(
            "tests use cargo nextest",
            "events 3..7 of session x",
        )];
        reduce(&mut s, Action::OpenMemory);
        assert_eq!(s.overlay, Overlay::Memory { source_open: false });
        reduce(&mut s, Action::OpenMemory);
        assert_eq!(s.overlay, Overlay::None);
    }

    #[test]
    fn skill_navigation_moves_selection_within_the_studio() {
        let mut s = AppState::new();
        s.skills = vec![
            skill("a", &["command: cargo"]),
            skill("b", &["filesystem_read: $REPOSITORY"]),
        ];
        reduce(&mut s, Action::OpenSkills);
        assert_eq!(s.selected_skill, 0);
        reduce(&mut s, Action::SelectNext);
        assert_eq!(s.selected_skill, 1);
        reduce(&mut s, Action::SelectNext); // clamps at the end
        assert_eq!(s.selected_skill, 1);
        reduce(&mut s, Action::SelectPrev);
        assert_eq!(s.selected_skill, 0);
    }

    #[test]
    fn memory_navigation_moves_selection_and_collapses_source() {
        let mut s = AppState::new();
        s.memories = vec![memory("m0", "src0"), memory("m1", "src1")];
        reduce(&mut s, Action::OpenMemory);
        // Open the first memory's source, then navigate: the source collapses.
        reduce(&mut s, Action::OpenSource);
        assert_eq!(s.overlay, Overlay::Memory { source_open: true });
        reduce(&mut s, Action::SelectNext);
        assert_eq!(s.selected_memory, 1);
        assert_eq!(s.overlay, Overlay::Memory { source_open: false });
    }

    #[test]
    fn open_source_reveals_the_focused_memory_source() {
        let mut s = AppState::new();
        s.memories = vec![memory(
            "tests use cargo nextest",
            "artifact abc (rust-toolchain.toml)",
        )];
        reduce(&mut s, Action::OpenMemory);
        assert_eq!(s.overlay, Overlay::Memory { source_open: false });
        // Both the explicit key and Enter open the source.
        reduce(&mut s, Action::OpenSource);
        assert_eq!(s.overlay, Overlay::Memory { source_open: true });
        // Re-open the browser and use Enter (Expand) this time.
        reduce(&mut s, Action::OpenMemory); // close
        reduce(&mut s, Action::OpenMemory); // reopen, source collapsed
        assert_eq!(s.overlay, Overlay::Memory { source_open: false });
        reduce(&mut s, Action::Expand);
        assert_eq!(s.overlay, Overlay::Memory { source_open: true });
    }

    #[test]
    fn open_source_is_inert_without_the_memory_overlay() {
        let mut s = AppState::new();
        s.memories = vec![memory("m", "src")];
        // No overlay open: opening a source does nothing.
        reduce(&mut s, Action::OpenSource);
        assert_eq!(s.overlay, Overlay::None);
    }

    fn doc(title: &str) -> crate::state::DocCard {
        crate::state::DocCard {
            title: title.to_owned(),
            scope: "organization".to_owned(),
            status: "draft".to_owned(),
            mode: "suggest".to_owned(),
            revision: "r3".to_owned(),
            blocks: vec![crate::state::DocBlockView {
                kind: "heading".to_owned(),
                text: title.to_owned(),
            }],
            suggestions: vec![crate::state::DocSuggestionView {
                status: "pending".to_owned(),
                author: "agent".to_owned(),
                range: "0..4".to_owned(),
                replacement: "new".to_owned(),
                rationale: Some("clearer".to_owned()),
            }],
        }
    }

    fn edge(from: &str, to: &str) -> crate::state::GraphEdgeCard {
        crate::state::GraphEdgeCard {
            from: from.to_owned(),
            to: to.to_owned(),
            relation: "calls".to_owned(),
            confidence: 0.45,
            evidence_kind: "syntax_inferred".to_owned(),
            evidence: "artifact abc (src/lib.rs)".to_owned(),
            revision: "79acbf1".to_owned(),
        }
    }

    #[test]
    fn open_docs_toggles_the_docs_overlay() {
        let mut s = AppState::new();
        s.docs = vec![doc("Payments guide")];
        reduce(&mut s, Action::OpenDocs);
        assert_eq!(s.overlay, Overlay::Docs);
        assert_eq!(s.input_mode(), crate::state::InputMode::Normal);
        reduce(&mut s, Action::OpenDocs);
        assert_eq!(s.overlay, Overlay::None);
    }

    #[test]
    fn open_edges_toggles_the_edge_inspector() {
        let mut s = AppState::new();
        s.edges = vec![edge("a::f", "b::g")];
        reduce(&mut s, Action::OpenEdges);
        assert_eq!(s.overlay, Overlay::Edges);
        assert_eq!(s.input_mode(), crate::state::InputMode::Normal);
        reduce(&mut s, Action::OpenEdges);
        assert_eq!(s.overlay, Overlay::None);
    }

    #[test]
    fn docs_navigation_moves_selection_within_the_tree() {
        let mut s = AppState::new();
        s.docs = vec![doc("a"), doc("b")];
        reduce(&mut s, Action::OpenDocs);
        assert_eq!(s.selected_doc, 0);
        reduce(&mut s, Action::SelectNext);
        assert_eq!(s.selected_doc, 1);
        reduce(&mut s, Action::SelectNext); // clamps at the end
        assert_eq!(s.selected_doc, 1);
        reduce(&mut s, Action::SelectPrev);
        assert_eq!(s.selected_doc, 0);
    }

    #[test]
    fn edge_navigation_moves_selection_within_the_inspector() {
        let mut s = AppState::new();
        s.edges = vec![edge("a::f", "b::g"), edge("c::h", "d::i")];
        reduce(&mut s, Action::OpenEdges);
        assert_eq!(s.selected_edge, 0);
        reduce(&mut s, Action::SelectNext);
        assert_eq!(s.selected_edge, 1);
        reduce(&mut s, Action::SelectNext); // clamps at the end
        assert_eq!(s.selected_edge, 1);
        reduce(&mut s, Action::SelectPrev);
        assert_eq!(s.selected_edge, 0);
    }

    #[test]
    fn opening_one_browser_replaces_another() {
        // The overlays are mutually exclusive: opening Docs over an open Edges
        // inspector swaps rather than stacks.
        let mut s = AppState::new();
        s.docs = vec![doc("a")];
        s.edges = vec![edge("a::f", "b::g")];
        reduce(&mut s, Action::OpenEdges);
        assert_eq!(s.overlay, Overlay::Edges);
        reduce(&mut s, Action::OpenDocs);
        assert_eq!(s.overlay, Overlay::Docs);
    }

    #[test]
    fn palette_opens_filters_and_stays_navigable() {
        let mut s = AppState::new();
        reduce(&mut s, Action::OpenPalette);
        assert_eq!(
            s.overlay,
            Overlay::Palette {
                query: String::new(),
                selected: 0,
            }
        );
        assert_eq!(s.input_mode(), crate::state::InputMode::Palette);

        // Navigation moves the selection within the (unfiltered) command list.
        reduce(&mut s, Action::SelectNext);
        assert_eq!(
            s.overlay,
            Overlay::Palette {
                query: String::new(),
                selected: 1,
            }
        );

        // Typing filters and resets the selection to the top.
        reduce(&mut s, Action::InputChar('d'));
        reduce(&mut s, Action::InputChar('o'));
        reduce(&mut s, Action::InputChar('c'));
        assert_eq!(
            s.overlay,
            Overlay::Palette {
                query: "doc".to_owned(),
                selected: 0,
            }
        );
        // Backspace edits the query too.
        reduce(&mut s, Action::InputBackspace);
        assert_eq!(
            s.overlay,
            Overlay::Palette {
                query: "do".to_owned(),
                selected: 0,
            }
        );
    }

    #[test]
    fn palette_submit_runs_the_highlighted_command() {
        // Filter to "docs" and run it: the palette closes and the Docs browser opens.
        let mut s = AppState::new();
        reduce(&mut s, Action::OpenPalette);
        for c in "docs".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::Docs);
    }

    #[test]
    fn palette_submit_can_open_a_text_prompt() {
        // "new run" routes through the palette to the new-run prompt overlay.
        let mut s = AppState::new();
        reduce(&mut s, Action::OpenPalette);
        for c in "new".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert!(matches!(s.overlay, Overlay::NewRun(_)));
    }

    #[test]
    fn palette_escape_closes_without_running_anything() {
        let mut s = AppState::new();
        reduce(&mut s, Action::OpenPalette);
        reduce(&mut s, Action::InputCancel);
        assert_eq!(s.overlay, Overlay::None);
    }

    #[test]
    fn palette_submit_with_no_match_is_inert() {
        let mut s = AppState::new();
        reduce(&mut s, Action::OpenPalette);
        for c in "zzzz".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        // Closed (mem::take), nothing opened.
        assert_eq!(s.overlay, Overlay::None);
    }

    #[test]
    fn composer_captures_text_and_esc_clears_it() {
        let mut s = AppState::new();
        for c in "fix the bug".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        assert_eq!(s.composer, "fix the bug");
        reduce(&mut s, Action::InputBackspace);
        assert_eq!(s.composer, "fix the bu");
        reduce(&mut s, Action::InputCancel);
        assert!(s.composer.is_empty());
    }

    #[test]
    fn slash_opens_the_palette_only_on_an_empty_composer() {
        // Slash on an empty composer opens the palette.
        let mut s = AppState::new();
        reduce(&mut s, Action::InputChar('/'));
        assert!(matches!(s.overlay, Overlay::Palette { .. }));
        assert!(s.composer.is_empty());

        // Slash after text is a literal character.
        let mut s2 = AppState::new();
        reduce(&mut s2, Action::InputChar('a'));
        reduce(&mut s2, Action::InputChar('/'));
        assert_eq!(s2.composer, "a/");
        assert_eq!(s2.overlay, Overlay::None);
    }

    #[test]
    fn composer_submit_starts_a_run_when_idle() {
        let mut s = AppState::new();
        for c in "diagnose the failing test".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert!(s.composer.is_empty(), "draft cleared after send");
        let intents = s.drain_outbox();
        assert!(
            matches!(
                intents.as_slice(),
                [Intent::StartRun { objective, .. }] if objective == "diagnose the failing test"
            ),
            "expected a StartRun intent, got {intents:?}"
        );
    }

    #[test]
    fn composer_submit_steers_a_live_run() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "o".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        // The run is live (non-terminal), so a message steers rather than restarts.
        assert!(s.selected_run_is_active());
        for c in "also add tests".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        let intents = s.drain_outbox();
        assert!(
            matches!(
                intents.as_slice(),
                [Intent::QueueSteering { text, run_id: r }] if text == "also add tests" && *r == run_id
            ),
            "expected a QueueSteering intent, got {intents:?}"
        );
    }

    #[test]
    fn empty_composer_submit_sends_nothing() {
        let mut s = AppState::new();
        reduce(&mut s, Action::InputSubmit);
        assert!(s.drain_outbox().is_empty());
    }

    #[test]
    fn ctrl_arrows_cycle_between_runs() {
        let mut s = AppState::new();
        for (obj, _) in [("a", ()), ("b", ())] {
            reduce(
                &mut s,
                system_ev(EventBody::RunStarted {
                    run_id: RunId::new(),
                    objective: obj.to_owned(),
                    mode: AgentMode::Build,
                }),
            );
        }
        // The latest run is selected; Ctrl-↑ moves to the previous one.
        assert_eq!(s.selected_run, 1);
        reduce(&mut s, Action::PrevRun);
        assert_eq!(s.selected_run, 0);
        reduce(&mut s, Action::PrevRun); // clamps at the start
        assert_eq!(s.selected_run, 0);
        reduce(&mut s, Action::NextRun);
        assert_eq!(s.selected_run, 1);
    }

    #[test]
    fn f2_toggles_between_chat_and_workspace_layouts() {
        use crate::state::LayoutMode;
        let mut s = AppState::new();
        assert_eq!(s.layout, LayoutMode::Chat);
        reduce(&mut s, Action::ToggleLayout);
        assert_eq!(s.layout, LayoutMode::Workspace);
        reduce(&mut s, Action::ToggleLayout);
        assert_eq!(s.layout, LayoutMode::Chat);
        // The palette command reaches the same toggle.
        reduce(&mut s, Action::OpenPalette);
        for c in "layout".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.layout, LayoutMode::Workspace);
    }

    #[test]
    fn patch_proposed_adds_expandable_summary() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "o".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::PatchProposed {
                run_id,
                changeset_id: ChangeSetId::new(),
                artifact: artifact(),
            }),
        );
        s.focus = Pane::Transcript;
        // The patch is the selected entry; expand toggles it.
        assert!(matches!(
            s.runs[0].transcript.last(),
            Some(TranscriptEntry::Patch(_))
        ));
        s.runs[0].transcript_selected = 0;
        reduce(&mut s, Action::Expand);
        let TranscriptEntry::Patch(p) = &s.runs[0].transcript[0] else {
            unreachable!()
        };
        assert!(p.expanded);
    }
}
