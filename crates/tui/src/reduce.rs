//! The reducer (STEP 1.12 RULE 3): the one pure state transition.
//!
//! `reduce` performs no I/O. Every daemon event and every input-derived action
//! is folded here, deterministically, into [`AppState`]. Commands the daemon
//! must run are appended to [`AppState::outbox`] as [`Intent`]s for the CLI to
//! dispatch — the reducer never touches a socket. Folding [`EventBody`] into
//! transcript/run/approval state is the core, and it is what the unit tests
//! below exercise.

use chrono::Utc;
use codypendent_protocol::{
    Actor, AgentMode, ApprovalDecision, ApprovalScope, BudgetDimension, DocumentId,
    DocumentMutation, EventBody, ModelId, ProposedAction, QuestionOutcome, Risk, RiskLevel,
    RunDisposition, RunState, SessionEvent, ToolOutcome, UiActionBinding, UiDocumentId, UiEvent,
    UiEventId, UiEventModifiers, UiEventType, UiNodeId, UiProtocolVersion, UiResyncRequest,
    UiRevision, UiWireMessage,
};
use codypendent_ui_host::UiSessionUpdate;
use serde_json::{Map, Value};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::action::{
    Action, Intent, KeyTarget, LearningMutation, ProjectionKind, SecretKey, WorkflowNodeUpdate,
};
use crate::remote_ui::{RemoteKey, RemoteUiRenderOutput};
use crate::remote_ui_host::{empty_message, terminal_viewport_message};
use crate::state::{
    filter_council_member_models, filter_key_rows, filter_model_names, filter_models, filter_modes,
    filter_onboard_providers, filter_providers, filter_themes, filter_unsloth_quants,
    filter_unsloth_repos, key_row_target, AddModelRow, AppState, BacktrackState,
    CouncilBuilderState, CouncilBuilderStep, CouncilMemberDraft, DocBlockView, DocEdit, DocFocus,
    DocLeaseState, DocPublishTargetKind, DocSuggestionView, KeyStatus, ModelListOrigin,
    ModelReadiness, OnboardFlow, OnboardProviderClass, OnboardStep, Overlay, Pane, PatchSummary,
    PendingApproval, PendingPromptCard, PendingQuestion, PendingRunStart, QuestionCardState,
    RunActivity, RunStartDraftTarget, RunView, ToolCard, ToolStatus, TranscriptEntry,
    UnslothQuantCard, UnslothRepoCard, DOC_PUBLISH_TARGETS, EDGE_PAGE_SIZE,
};

/// Above this size a message stays on the fast plain path (its single parse
/// would be too costly). 64 KiB — a quarter of `MAX_MODEL_ENTRY_BYTES`.
const RICH_MARKDOWN_MAX_BYTES: usize = 64 * 1024;

/// Logical ticks a first `StartRun` waits for its durable `RunStarted` before
/// the admission guard is released as lost (~10s at the 5 fps tick). Counted
/// in `state.tick`, not wall-clock, so `reduce` stays deterministic.
const PENDING_RUN_START_TIMEOUT_TICKS: u64 = 50;

/// Parse every finalized (non-streaming-tail) `Model` entry into its rich cache
/// exactly once. Runs at the tail of every folded `DaemonEvent`, so it catches
/// all stream-ending transitions without enumerating them. Idempotent (skips any
/// entry already `Some`); bounded (O(total Model entries) cheap `is_none` checks).
pub(crate) fn finalize_streamed_models(state: &mut AppState) {
    invalidate_width_dependent_rich_cache(state);
    let width = usize::from(state.transcript_width.get());
    let last_run = state.runs.len().checked_sub(1);
    for (idx, run) in state.runs.iter_mut().enumerate() {
        // The live streaming tail (only possible in the last run) is skipped.
        let tail = if Some(idx) == last_run && run.activity == RunActivity::Streaming {
            run.transcript.len().checked_sub(1)
        } else {
            None
        };
        for (i, entry) in run.transcript.iter_mut().enumerate() {
            if Some(i) == tail {
                continue;
            }
            if let TranscriptEntry::Model { text, rendered } = entry {
                if rendered.is_none() && text.len() <= RICH_MARKDOWN_MAX_BYTES {
                    *rendered = Some(crate::markdown::parse(text, width));
                }
            }
        }
    }
}

/// Drop the cached rich lines whose layout depended on a width that no longer
/// holds, so the finalize pass above re-lays them for the new pane.
///
/// A markdown TABLE is the only width-dependent product of the parse — its
/// columns are padded into the span text — so only messages that contain one
/// are invalidated. Everything else wraps at draw time and survives a resize
/// untouched, which keeps a resize O(cached lines) rather than O(re-parse) for
/// the ordinary transcript.
fn invalidate_width_dependent_rich_cache(state: &mut AppState) {
    let width = state.transcript_width.get();
    // 0 means "no pane measured yet" (before the first frame); a width that has
    // not changed means every cached table is already laid out correctly.
    if width == 0 || width == state.rich_layout_width {
        return;
    }
    state.rich_layout_width = width;
    for run in &mut state.runs {
        for entry in &mut run.transcript {
            if let TranscriptEntry::Model { rendered, .. } = entry {
                if rendered.as_deref().is_some_and(has_table_row) {
                    *rendered = None;
                }
            }
        }
    }
}

fn has_table_row(lines: &[crate::markdown::RichLine]) -> bool {
    lines.iter().any(|line| {
        line.spans.iter().any(|span| {
            matches!(
                span.role,
                crate::markdown::SpanRole::TableHeader
                    | crate::markdown::SpanRole::TableCell
                    | crate::markdown::SpanRole::TableRule
            )
        })
    })
}

/// Fold a single [`Action`] into the state. Pure: the only side effect is
/// mutating `state` (including appending intents to its outbox).
/// Strip terminal control sequences from every operator-visible string in a
/// proposed action.
///
/// The approval modal is the one surface where the operator AUTHORISES
/// something, and it renders this action's program, arguments, environment and
/// working directory. Those strings are chosen by the model, arrived over the
/// socket unsanitized, and crossterm writes cell symbols verbatim — so a
/// crafted argument could emit OSC 52 to overwrite the clipboard, or reposition
/// the cursor and repaint the dialog to describe a command other than the one
/// being approved. Sanitizing model prose while leaving the approval evidence
/// raw protected the least consequential text in the app and not the most.
fn sanitize_proposed_action(action: ProposedAction) -> ProposedAction {
    use crate::remote_ui::sanitize_terminal_text as clean;
    match action {
        ProposedAction::ReadFiles { paths } => ProposedAction::ReadFiles {
            paths: paths.iter().map(|path| clean(path)).collect(),
        },
        ProposedAction::ExecuteCommand {
            program,
            args,
            environment,
            cwd,
        } => ProposedAction::ExecuteCommand {
            program: clean(&program),
            args: args.iter().map(|arg| clean(arg)).collect(),
            environment: environment
                .iter()
                .map(|(key, value)| (clean(key), clean(value)))
                .collect(),
            cwd: cwd.as_deref().map(clean),
        },
        ProposedAction::NetworkRequest { destination } => ProposedAction::NetworkRequest {
            destination: clean(&destination),
        },
        ProposedAction::GitCommit { repository } => ProposedAction::GitCommit {
            repository: clean(&repository),
        },
        ProposedAction::GitPush { remote, branch } => ProposedAction::GitPush {
            remote: clean(&remote),
            branch: clean(&branch),
        },
        // Variants whose operator-visible fields are ids or enums carry no
        // model-authored free text, and are passed through unchanged rather
        // than rebuilt field by field — a rebuild would silently drop any field
        // a later protocol version adds.
        other => other,
    }
}

/// How long a remote-UI confirmation stays armed, in ticks — the same window
/// its notice is shown for, so the two cannot disagree.
const CONFIRMATION_TICKS: u64 = 10;

pub fn reduce(state: &mut AppState, action: Action) {
    // A held document lease belongs to the visible Docs editing surface. Any
    // action that replaces that surface (another browser, a run prompt, Help,
    // detach, or a session close) must release it immediately rather than
    // leaving collaborators blocked until the server-side TTL expires. The two
    // Docs sub-prompts are part of the same flow and deliberately retain it.
    let docs_surface_was_open = matches!(
        state.overlay,
        Overlay::Docs
            | Overlay::DocEdit { .. }
            | Overlay::DocNew { .. }
            | Overlay::DocInsert { .. }
            | Overlay::DocDeleteConfirm { .. }
            | Overlay::DocPublishTarget { .. }
            | Overlay::DocPublishPath { .. }
            | Overlay::DocPublishBranch { .. }
            | Overlay::DocPublishTitle { .. }
    );
    match action {
        Action::DaemonEvent(event) => {
            apply_event(state, *event);
            finalize_streamed_models(state);
        }
        Action::CatchupSnapshot {
            title,
            closed,
            runs,
            pending_approvals,
            pending_prompts,
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
            state.pending_approvals = pending_approvals
                .into_iter()
                .map(|approval| PendingApproval {
                    approval_id: approval.approval_id,
                    action: approval.action,
                    risk: approval.risk,
                    run_id: Some(approval.run_id),
                    pattern: None,
                })
                .collect();
            clamp(&mut state.selected_approval, state.pending_approvals.len());
            // A wholesale replacement changes what the modal shows.
            state.approval_scroll = 0;
            state.pending_prompts = pending_prompts
                .into_iter()
                .map(|p| PendingPromptCard {
                    id: p.id,
                    text: p.text,
                    delivery: p.delivery,
                })
                .collect();
            if state.pending_prompts.is_empty() {
                state.queue_selected = None;
                state.queue_editing = None;
            } else if let Some(sel) = state.queue_selected {
                if sel >= state.pending_prompts.len() {
                    state.queue_selected = Some(state.pending_prompts.len() - 1);
                }
            }
        }
        Action::Tick => {
            state.tick = state.tick.wrapping_add(1);
            if let Some((_, expires)) = &state.notice {
                if state.tick >= *expires {
                    state.notice = None;
                }
            }
            // A first `StartRun` whose acknowledgement was lost outright (the
            // connection dropped between the intent and the first durable
            // event) must not wedge the composer: past the timeout the guard
            // is released and the retained draft handed back. The CLI's
            // idempotent resend can still attach the run; a late `RunStarted`
            // simply finds no guard and folds as usual.
            if let Some(pending) = state.pending_run_start.take() {
                if state.tick.wrapping_sub(pending.started_tick) >= PENDING_RUN_START_TIMEOUT_TICKS
                {
                    restore_pending_run_draft(state, pending);
                    state.notice = Some((
                        "run start timed out — your draft is retained".to_owned(),
                        state.tick + 40,
                    ));
                } else {
                    state.pending_run_start = Some(pending);
                }
            }
            // A resize is not an event, so without this a table stays laid out
            // for the old pane until the next daemon event arrives — which for
            // a finished conversation is never. The comparison is one `u16`;
            // the walk happens only on the tick after the width actually moved.
            if state.transcript_width.get() != state.rich_layout_width {
                finalize_streamed_models(state);
            }
            if state.tick.is_multiple_of(25) {
                refresh_open_projection(state);
                if matches!(state.overlay, Overlay::Edges) && !state.edge_loading {
                    request_edge_page(state, state.edge_page);
                }
                if matches!(state.overlay, Overlay::Blackboard) {
                    watch_focused_blackboard_run(state);
                }
            }
        }
        // ~5 seconds at the 5 fps tick.
        Action::Notice(text) => state.notice = Some((text, state.tick + 25)),
        Action::OpenOnboard => open_onboard(state),
        Action::RunnableModelsRefreshed {
            model_ids,
            onboard_attempt,
        } => on_runnable_models_refreshed(state, model_ids, onboard_attempt),
        Action::OnboardModelAddFailed { model_id, reason } => {
            on_onboard_model_add_failed(state, model_id, reason);
        }
        Action::RunStartRejected { reason } => {
            if let Some(pending) = state.pending_run_start.take() {
                restore_pending_run_draft(state, pending);
                state.notice = Some((
                    format!("run was not started: {reason} · draft restored"),
                    state.tick + 40,
                ));
            } else {
                state.notice = Some((format!("run was not started: {reason}"), state.tick + 40));
            }
        }
        Action::TerminalFocus(focused) => {
            state.terminal_focused = focused;
        }
        Action::SessionListLoaded(rows) => {
            state.session_list = rows;
        }
        Action::SessionSearchLoaded {
            query,
            rows,
            next_cursor,
            append,
        } => session_search_loaded(state, query, rows, next_cursor, append),
        Action::SessionSearchFailed { query, reason } => {
            session_search_failed(state, query, reason);
        }
        Action::SessionLifecycleApplied(row) => session_lifecycle_applied(state, *row),
        Action::SessionLifecycleDeleted {
            session_id,
            tombstoned,
        } => session_lifecycle_deleted(state, session_id, tombstoned),
        Action::SessionExported { session_id, path } => {
            state.notice = Some((
                format!("exported session {session_id} to {path}"),
                state.tick + 60,
            ));
        }
        Action::SessionLibraryTogglePin => session_library_toggle_pin(state),
        Action::SessionLibraryToggleArchive => session_library_toggle_archive(state),
        Action::SessionLibraryBeginRename => session_library_begin_rename(state),
        Action::SessionLibraryExport => session_library_export(state),
        Action::FileSearchResults {
            query,
            matches,
            truncated: _,
        } => {
            if let Some(popup) = &mut state.mention_popup {
                if popup.query == query {
                    popup.matches = matches;
                    popup.waiting = false;
                }
            }
        }
        Action::MentionSelectPrev => mention_select_prev(state),
        Action::MentionSelectNext => mention_select_next(state),
        Action::MentionSelect => mention_select(state),
        Action::MentionCancel => {
            state.mention_popup = None;
        }
        Action::MentionSelectAt(index) => {
            if let Some(popup) = &mut state.mention_popup {
                if index < popup.matches.len() {
                    popup.selected = index;
                }
            }
            mention_select(state);
        }
        Action::HistorySearchPrev => history_search_prev(state),
        Action::HistorySearchNext => history_search_next(state),
        Action::HistorySearchSelect => history_search_select(state),
        Action::HistorySearchCancel => {
            state.history_search = None;
        }
        // Voice v1 (rubric 8): the host reports capture start/stop; the flag
        // only drives the status-line indicator.
        Action::VoiceRecording(recording) => state.voice.recording = recording,
        Action::Issue(text) => {
            let inserted = !state.issues.iter().any(|issue| issue == &text);
            if inserted {
                state.issues.push(text.clone());
                state.notice = Some((
                    format!("setup needs attention — {} issue(s)", state.issues.len()),
                    state.tick + 40,
                ));
            }
        }
        Action::RemoteUiMessage(message) => apply_remote_ui_message(state, *message),
        Action::RemoteUiSetActive(active) => {
            state.remote_ui.active = active && !state.remote_ui.mounted_documents().is_empty();
            if state.remote_ui.active {
                state.remote_ui.repair_focus();
                focus_remote_document(state, None);
            }
        }
        Action::RemoteUiNextDocument => focus_next_remote_document(state),
        Action::RemoteUiFocusDocument(document_id) => {
            focus_remote_document(state, Some(document_id));
        }
        Action::RemoteUiViewport { width, height } => {
            state.outbox.push(Intent::RemoteUiMessage(Box::new(
                terminal_viewport_message(width, height),
            )));
        }
        Action::UiPluginsLoaded(plugins) => {
            // Mutation replies contain one row; list replies contain the full
            // projection. Merge by id so either shape preserves selection.
            for plugin in plugins {
                if let Some(current) = state.ui_plugins.iter_mut().find(|p| p.id == plugin.id) {
                    *current = plugin;
                } else {
                    state.ui_plugins.push(plugin);
                }
            }
            state.ui_plugins.sort_by(|a, b| a.id.cmp(&b.id));
            clamp(&mut state.selected_ui_plugin, state.ui_plugins.len());
        }
        Action::DocumentCreated { document_id } => {
            state.pending_document_selection = Some(document_id);
            state.outbox.push(Intent::RefreshProjection {
                kind: ProjectionKind::Docs,
            });
            let id = document_id.to_string();
            state.notice = Some((
                format!("document created · {}", id.get(..8).unwrap_or(&id)),
                state.tick + 30,
            ));
        }
        Action::DocumentPublishPrepared {
            approval_id,
            document_id,
            target,
            changed_files,
            git_action,
        } => {
            let pending = PendingApproval {
                approval_id,
                action: ProposedAction::PublishDocument {
                    document_id,
                    target: target.clone(),
                    changed_files,
                    git_action: git_action.clone(),
                },
                risk: Risk {
                    level: RiskLevel::High,
                    reasons: vec!["publishing writes document content to a Git target".to_owned()],
                },
                run_id: None,
                pattern: None,
            };
            if let Some(existing) = state
                .pending_approvals
                .iter_mut()
                .find(|approval| approval.approval_id == approval_id)
            {
                *existing = pending;
            } else {
                state.pending_approvals.push(pending);
            }
            clamp(&mut state.selected_approval, state.pending_approvals.len());
            state.approval_scroll = 0;
            state.notice = Some((
                format!("publish awaiting approval · {target} · {git_action}"),
                state.tick + 50,
            ));
        }
        Action::CouncilCreated {
            name,
            members,
            rounds,
        } => {
            // Return to the browser (rubric 6): the wizard's only entry point
            // is now `n` from inside it, so a successful save should surface
            // the new council in the freshly reloaded list, not drop to the
            // base view.
            if matches!(
                &state.overlay,
                Overlay::CouncilBuilder(builder) if builder.name == name
            ) {
                state.selected_council = state
                    .councils
                    .iter()
                    .position(|council| council.name == name)
                    .unwrap_or(0);
                state.overlay = Overlay::CouncilBrowser;
            }
            state.notice = Some((
                format!("created council `{name}` · {members} members · {rounds} round(s)"),
                state.tick + 50,
            ));
        }
        Action::CouncilCreateFailed { name, error } => {
            state.notice = Some((
                format!("could not create council `{name}`: {error}"),
                state.tick + 80,
            ));
        }
        Action::CouncilDeleted { name } => {
            clamp(&mut state.selected_council, state.councils.len());
            if matches!(
                &state.overlay,
                Overlay::ConfirmCouncilDelete { name: pending } if pending == &name
            ) {
                state.overlay = Overlay::CouncilBrowser;
            }
            state.notice = Some((format!("removed council `{name}`"), state.tick + 40));
        }
        Action::CouncilDeleteFailed { name, error } => {
            state.notice = Some((
                format!("could not remove council `{name}`: {error}"),
                state.tick + 80,
            ));
        }
        Action::CouncilProgress {
            name,
            result_id,
            phase,
            occurred_at,
            message,
            active_subagents,
        } => {
            state.council_subagents = active_subagents;
            if !state
                .council_results
                .iter()
                .any(|result| result.result_id == result_id)
            {
                state.council_results.insert(
                    0,
                    crate::state::CouncilRunSummary {
                        result_id: result_id.clone(),
                        council: name.clone(),
                        status: "running".to_owned(),
                        objective: String::new(),
                        started_at: occurred_at.clone(),
                        finished_at: String::new(),
                        repository: String::new(),
                        origin_session_id: None,
                        evidence: false,
                        warnings: Vec::new(),
                        rounds: Vec::new(),
                        failure: None,
                        synthesis: String::new(),
                        participants: Vec::new(),
                        cost_line: "measured cost pending".to_owned(),
                        report_markdown: "report persists when the run terminates".to_owned(),
                    },
                );
                state.selected_council_result = 0;
            }
            let text = format!(
                "council `{name}` · {} · result {result_id} · {occurred_at}: {message}",
                phase.label()
            );
            if let Some(run) = state.selected_run_mut() {
                AppState::push_entry(
                    run,
                    TranscriptEntry::Note {
                        text,
                        expanded: false,
                    },
                    Utc::now(),
                );
            } else {
                state.notice = Some((text, state.tick + 40));
            }
        }
        Action::CouncilRunFinished { name, result } => {
            state.council_subagents = 0;
            match result {
                Ok(summary) => {
                    if let Some(index) = state
                        .council_results
                        .iter()
                        .position(|stored| stored.result_id == summary.result_id)
                    {
                        state.council_results[index] = (*summary).clone();
                        state.selected_council_result = index;
                    } else {
                        state.council_results.insert(0, (*summary).clone());
                        state.selected_council_result = 0;
                    }
                    state.council_result_scroll = 0;
                    state.council_result_expanded = false;
                    let mut text = format!(
                        "Council `{name}` — {} · result {}\n\n{}",
                        summary.status.to_uppercase(),
                        summary.result_id,
                        summary.synthesis.trim()
                    );
                    if !summary.participants.is_empty() {
                        text.push_str("\n\nParticipants:\n");
                        for line in &summary.participants {
                            text.push_str("  - ");
                            text.push_str(line);
                            text.push('\n');
                        }
                    }
                    text.push_str(&summary.cost_line);
                    text.push_str(&format!("\nreport: {}", summary.report_markdown));
                    if let Some(run) = state.selected_run_mut() {
                        AppState::push_entry(
                            run,
                            TranscriptEntry::Note {
                                text,
                                expanded: false,
                            },
                            Utc::now(),
                        );
                    }
                    state.overlay = Overlay::CouncilResults;
                    state.notice = Some((
                        format!("council `{name}` {} · durable result ready", summary.status),
                        state.tick + 60,
                    ));
                }
                Err(error) => {
                    if let Some(run) = state.selected_run_mut() {
                        AppState::push_entry(
                            run,
                            TranscriptEntry::Note {
                                text: format!("council `{name}` failed: {error}"),
                                expanded: false,
                            },
                            Utc::now(),
                        );
                    }
                    state.notice =
                        Some((format!("council `{name}` failed: {error}"), state.tick + 90));
                }
            }
        }
        Action::CouncilResultsLoaded(results) => {
            state.council_results = results;
            clamp(
                &mut state.selected_council_result,
                state.council_results.len(),
            );
            state.council_result_scroll = 0;
            state.council_result_expanded = false;
            state.overlay = Overlay::CouncilResults;
        }
        Action::CouncilResultsFailed(error) => {
            state.notice = Some((
                format!("could not load council result: {error}"),
                state.tick + 80,
            ));
        }
        Action::OpenCouncils => {
            state.overlay = match state.overlay {
                Overlay::CouncilBrowser => Overlay::None,
                _ => Overlay::CouncilBrowser,
            };
        }
        Action::DeleteCouncil => {
            if matches!(state.overlay, Overlay::Journey) {
                if let Some(card) = state.focused_learning() {
                    state.overlay = Overlay::ConfirmLearningDelete {
                        id: card.id.clone(),
                        revision: card.revision,
                        label: card.statement.clone(),
                    };
                }
            } else {
                begin_delete_council(state)
            }
        }
        Action::RemoteUiActivate {
            document_id,
            revision,
            target_id,
            binding,
        } => {
            state.remote_ui.active = true;
            state.remote_ui.focused_document = Some(document_id.clone());
            state.remote_ui.view.focused_node = Some(target_id.clone());
            emit_remote_ui_event(state, document_id, revision, target_id, *binding, None);
        }
        Action::RemoteUiKey { key, character } => {
            apply_remote_ui_key(state, key, character);
        }
        Action::RemoteUiPaste(text) => edit_remote_ui_field(state, |value| value.push_str(&text)),

        // In the Docs overlay `Tab` cycles the tree / editor / review rail focus;
        // when a pending prompt row is selected in base view, Tab edits it;
        // elsewhere it cycles the (vestigial) pane focus.
        Action::CyclePane => {
            // Tab completes an open @-mention popup (a synonym for Enter)
            // before any of its focus-cycling meanings.
            if state.mention_popup.is_some() {
                mention_select(state);
            } else if matches!(state.overlay, Overlay::Docs) {
                state.doc_focus = state.doc_focus.next();
            } else if matches!(state.overlay, Overlay::None) && state.queue_selected.is_some() {
                if let Some(idx) = state.queue_selected {
                    if let Some(entry) = state.pending_prompts.get(idx) {
                        state.queue_editing = Some(entry.text.clone());
                    }
                }
            } else {
                state.focus = state.focus.next();
            }
        }
        Action::FocusPane(pane) => state.focus = pane,
        Action::ActivateRow(n) => activate_row(state, n),
        Action::ActivateFold { run, entry } => activate_fold(state, run, entry),
        Action::SelectRun(n) => {
            let mut idx = n;
            clamp(&mut idx, state.runs.len());
            state.selected_run = idx;
        }
        Action::SelectDocument(n) => {
            if matches!(state.overlay, Overlay::Docs) {
                let previous = state.selected_doc;
                let mut idx = n;
                clamp(&mut idx, state.docs.len());
                state.selected_doc = idx;
                state.doc_focus = DocFocus::Tree;
                if previous != idx {
                    state.selected_block = 0;
                    state.selected_suggestion = 0;
                    watch_focused_doc(state);
                }
            }
        }
        Action::SelectDocumentBlock(n) => {
            if matches!(state.overlay, Overlay::Docs) {
                let len = state.focused_doc().map_or(0, |doc| doc.blocks.len());
                let mut idx = n;
                clamp(&mut idx, len);
                state.selected_block = idx;
                state.doc_focus = DocFocus::Editor;
            }
        }
        Action::SelectDocumentSuggestion(n) => {
            if matches!(state.overlay, Overlay::Docs) {
                let len = state.focused_doc().map_or(0, |doc| doc.suggestions.len());
                let mut idx = n;
                clamp(&mut idx, len);
                state.selected_suggestion = idx;
                state.doc_focus = DocFocus::Review;
            }
        }
        Action::SelectPrev => nav(state, -1),
        Action::SelectNext => nav(state, 1),
        Action::SelectPagePrev => nav(state, -6),
        Action::SelectPageNext => nav(state, 6),
        Action::SelectFirst => nav_to_edge(state, false),
        Action::SelectLast => nav_to_edge(state, true),
        Action::ScrollPageUp => scroll_page(state, true),
        Action::ScrollPageDown => scroll_page(state, false),
        Action::ScrollLinesUp => scroll_lines(state, true),
        Action::ScrollLinesDown => scroll_lines(state, false),
        Action::Expand => expand_selected(state),
        Action::BrowseFoldPrev => browse_fold(state, -1),
        Action::BrowseFoldNext => browse_fold(state, 1),
        Action::CopyFocusedCard => copy_focused_card(state),
        Action::RetryFailedRun => retry_failed_run(state),
        Action::ReauthenticateFailedModel => reauthenticate_failed_model(state),
        Action::ChooseFailureModel => open_failure_model_picker(state),
        Action::DisableFailureModel => disable_failed_model(state),
        Action::RemoveSelected => begin_remove_selected(state),
        Action::VerifyApiKey => begin_verify_key(state),
        Action::RefreshProviderModels => refresh_provider_models(state),

        Action::PrevRun => cycle_run(state, -1),
        Action::NextRun => cycle_run(state, 1),
        Action::NewRun => {
            if matches!(state.overlay, Overlay::Workflow) {
                start_focused_workflow(state);
            } else if matches!(state.overlay, Overlay::Kanban) {
                state.overlay = Overlay::KanbanNew {
                    buffer: String::new(),
                };
            } else if matches!(state.overlay, Overlay::Blackboard) {
                begin_blackboard_post(state);
            } else if matches!(state.overlay, Overlay::CouncilBrowser) {
                state.overlay = Overlay::CouncilBuilder(CouncilBuilderState::default());
                state.notice = None;
            } else if matches!(state.overlay, Overlay::Docs) {
                // In the Docs Studio, `n` creates a DOCUMENT — the same
                // overlay-contextual routing the workflow browser uses above.
                begin_doc_new(state);
            } else {
                state.overlay = Overlay::NewRun(String::new());
            }
        }
        Action::Pause => {
            if matches!(state.overlay, Overlay::Workflow) {
                pause_or_resume_workflow(state);
            } else if matches!(state.overlay, Overlay::Journey) {
                if let Some(card) = state.focused_learning() {
                    state.outbox.push(Intent::MutateLearning {
                        id: card.id.clone(),
                        revision: card.revision,
                        mutation: LearningMutation::SetPinned(!card.pinned),
                    });
                }
            } else {
                pause_or_resume(state);
            }
        }
        Action::Cancel => {
            if matches!(state.overlay, Overlay::Workflow) {
                request_workflow_cancel(state);
            } else if matches!(state.overlay, Overlay::CouncilResults) {
                if let Some((text, result_id)) = state
                    .focused_council_result()
                    .map(|result| (result.synthesis.clone(), result.result_id.clone()))
                {
                    state.outbox.push(Intent::CopyText { text });
                    state.notice = Some((
                        format!("copied chair synthesis · result {result_id}"),
                        state.tick + 30,
                    ));
                }
            } else {
                request_cancel(state);
            }
        }
        Action::ConfirmCancel => confirm_top(state),
        Action::Steer => {
            if matches!(state.overlay, Overlay::UiPlugins) {
                smoke_test_ui_plugin(state);
            } else {
                begin_steering(state);
            }
        }

        // `a`/`r` resolve a document suggestion when the Docs review rail is
        // focused (going through the same `MutateDocument` accept/reject the daemon
        // gates on the Approver/Controller role); otherwise they resolve a pending
        // approval, exactly as before.
        Action::Approve(scope) => {
            // A pending approval outranks EVERY overlay, because `input_mode`
            // says so — it returns `InputMode::Approval` before it looks at any
            // overlay, so the approval modal is what is on screen. Journey used
            // to be checked first here and nowhere else: with the overlay open,
            // `a` activated a learning while the approval the operator was
            // looking at stayed unresolved. Misrouting the most consequential
            // key in the app, and silently. `UiPlugins` and `Docs` below were
            // always ordered correctly; Journey was the outlier.
            if !state.pending_approvals.is_empty() {
                resolve_focused(state, ApprovalDecision::Approve, scope);
            } else if matches!(state.overlay, Overlay::Journey) {
                if let Some(card) = state.focused_learning() {
                    state.outbox.push(Intent::MutateLearning {
                        id: card.id.clone(),
                        revision: card.revision,
                        mutation: LearningMutation::Activate,
                    });
                }
            } else if matches!(state.overlay, Overlay::UiPlugins) {
                begin_approve_ui_plugin(state);
            } else if matches!(state.overlay, Overlay::Docs) {
                resolve_focused_suggestion(state, true);
            } else {
                resolve_focused(state, ApprovalDecision::Approve, scope);
            }
        }
        Action::Reject => {
            // Same ordering as `Approve` above, and for the same reason.
            if !state.pending_approvals.is_empty() {
                resolve_focused(state, ApprovalDecision::Reject, ApprovalScope::Once);
            } else if matches!(state.overlay, Overlay::Journey) {
                if let Some(card) = state.focused_learning() {
                    state.outbox.push(Intent::MutateLearning {
                        id: card.id.clone(),
                        revision: card.revision,
                        mutation: LearningMutation::Reject,
                    });
                }
            } else if matches!(state.overlay, Overlay::Workflow) {
                retry_focused_workflow_node(state);
            } else if matches!(state.overlay, Overlay::UiPlugins) {
                begin_reject_ui_plugin(state);
            } else if matches!(state.overlay, Overlay::Docs) {
                resolve_focused_suggestion(state, false);
            } else if matches!(state.overlay, Overlay::CouncilBrowser) {
                // Council browser: the same physical `r` key runs the focused
                // council (prompts for an objective) — same pattern as
                // Workflow's `r` meaning "retry" above.
                begin_run_council(state);
            } else {
                resolve_focused(state, ApprovalDecision::Reject, ApprovalScope::Once);
            }
        }

        Action::QuestionNavigate(delta) => question_navigate(state, delta),
        Action::QuestionPickDigit(digit) => question_pick_digit(state, digit),
        Action::QuestionToggleOption => question_toggle_option(state),
        Action::QuestionSelectOrConfirm => question_select_or_confirm(state),
        Action::QuestionInputChar(c) => question_input_char(state, c),
        Action::QuestionInputBackspace => question_input_backspace(state),
        Action::QuestionOpenReject => question_open_reject(state),
        Action::QuestionCancelReject => question_cancel_reject(state),
        Action::QuestionSubmitReject => question_submit_reject(state),

        Action::InputChar(c) => {
            // While the history search popup is open, typed text edits its
            // query (re-matching resets the highlight) instead of landing in
            // the composer draft. The input mapper is stateless, so — like
            // `InputNewline` below — the routing decision lives here.
            if let Some(hs) = &mut state.history_search {
                hs.query.push(c);
                hs.selected = 0;
            } else {
                input_char(state, c);
                check_mention_popup(state);
                sync_session_library(state);
            }
        }
        Action::InputPaste(text) => {
            if matches!(state.overlay, Overlay::None) && state.queue_editing.is_none() {
                if text.lines().count() >= 5 || text.len() >= 1024 {
                    let num = state.pasted_blocks.len() + 1;
                    let lines = text.lines().count().max(1);
                    let marker = format!("[Pasted #{num}: {lines} lines]");
                    state.pasted_blocks.push(crate::state::PasteBlock {
                        marker: marker.clone(),
                        text,
                    });
                    edit_prompt(state, &Edit::Insert(marker));
                } else {
                    edit_prompt(state, &Edit::Insert(text));
                }
            } else {
                edit_prompt(state, &Edit::Insert(text));
            }
            detach_history_on_edit(state);
            check_mention_popup(state);
        }
        Action::InputBackspace => {
            // Same history-search routing as `InputChar`: Backspace shortens
            // the popup's query, not the composer draft.
            if let Some(hs) = &mut state.history_search {
                hs.query.pop();
                hs.selected = 0;
            } else {
                edit_prompt(state, &Edit::Backspace);
                detach_history_on_edit(state);
                check_mention_popup(state);
                sync_session_library(state);
            }
        }
        Action::CursorLeft => move_composer_cursor(state, CursorMove::Left),
        Action::CursorRight => move_composer_cursor(state, CursorMove::Right),
        Action::CursorLineStart => move_composer_cursor(state, CursorMove::LineStart),
        Action::CursorLineEnd => move_composer_cursor(state, CursorMove::LineEnd),
        Action::DeleteWordBack => delete_backwards(state, &Edit::WordBack),
        Action::DeleteToLineStart => delete_backwards(state, &Edit::ToLineStart),
        Action::DeleteSelectedPrompt => {
            if matches!(state.overlay, Overlay::None) && state.queue_editing.is_none() {
                if let Some(idx) = state.queue_selected {
                    if let Some(entry) = state.pending_prompts.get(idx) {
                        state.outbox.push(Intent::DeleteQueuedPrompt {
                            prompt_id: entry.id,
                        });
                    }
                }
            }
        }
        // `Alt-Enter` expands the browsed transcript fold when one is under the
        // cursor (the keyboard path to tool cards and patch diffs), and is a
        // plain line break otherwise. The input mapper is stateless, so the
        // decision lives here, where the browse flag does.
        Action::InputNewline => {
            if state.transcript_browse && matches!(state.overlay, Overlay::None) {
                expand_selected(state);
            } else {
                edit_prompt(state, &Edit::Insert("\n".to_owned()));
                detach_history_on_edit(state);
            }
        }
        // Enter confirms an open composer popup (history search first, then
        // @-mention) instead of submitting the half-edited draft out from
        // under the popup.
        Action::InputSubmit => {
            if state.history_search.is_some() {
                history_search_select(state);
            } else if state.mention_popup.is_some() {
                mention_select(state);
            } else {
                submit_prompt(state);
            }
        }
        // Esc dismisses an open composer popup first, leaving the draft
        // intact — falling through to `input_cancel` here used to clear the
        // draft while the popup stayed open.
        Action::InputCancel => {
            if state.history_search.is_some() {
                state.history_search = None;
            } else if state.mention_popup.is_some() {
                state.mention_popup = None;
            } else {
                input_cancel(state);
            }
        }
        // `↑`/`↓` walk the draft's own lines first and only recall history at
        // its top/bottom edge — a single-line draft is unchanged. While a
        // composer popup is open they navigate its matches instead.
        Action::HistoryPrev => {
            if state.history_search.is_some() {
                history_search_prev(state);
            } else if state.mention_popup.is_some() {
                mention_select_prev(state);
            } else {
                composer_up(state);
            }
        }
        Action::HistoryNext => {
            if state.history_search.is_some() {
                history_search_next(state);
            } else if state.mention_popup.is_some() {
                mention_select_next(state);
            } else {
                composer_down(state);
            }
        }

        Action::OpenSkills => {
            state.overlay = match state.overlay {
                Overlay::Skills => Overlay::None,
                _ => Overlay::Skills,
            };
            if matches!(state.overlay, Overlay::Skills) {
                request_projection(state, ProjectionKind::Skills);
            }
        }
        Action::OpenMemory => {
            state.overlay = match state.overlay {
                Overlay::Memory { .. } => Overlay::None,
                _ => Overlay::Memory { source_open: false },
            };
            if matches!(state.overlay, Overlay::Memory { .. }) {
                request_projection(state, ProjectionKind::Memory);
            }
        }
        Action::OpenJourney => {
            state.overlay = if matches!(state.overlay, Overlay::Journey) {
                Overlay::None
            } else {
                Overlay::Journey
            };
            if matches!(state.overlay, Overlay::Journey) {
                request_projection(state, ProjectionKind::Journey);
            }
        }
        Action::OpenContext => {
            state.overlay = if matches!(state.overlay, Overlay::Context) {
                Overlay::None
            } else {
                Overlay::Context
            };
        }
        Action::OpenSource => open_source(state),

        Action::OpenDocs => {
            if matches!(state.overlay, Overlay::Docs) {
                // Closing the browser releases any block lease this client holds.
                release_doc_lease(state);
                state.overlay = Overlay::None;
            } else {
                state.overlay = Overlay::Docs;
                request_projection(state, ProjectionKind::Docs);
                watch_focused_doc(state);
            }
        }
        Action::OpenEdges => {
            if matches!(state.overlay, Overlay::Edges) {
                state.overlay = Overlay::None;
            } else {
                state.overlay = Overlay::Edges;
                request_edge_page(state, state.edge_page);
            }
        }
        Action::EdgesLoaded {
            edges,
            total,
            query,
            page,
        } => {
            state.edges = edges;
            state.edge_total = total;
            state.edge_query = query;
            state.edge_page = page;
            state.edge_loading = false;
            state.selected_edge = 0;
        }
        Action::OpenWorkflow => {
            if matches!(state.overlay, Overlay::Workflow) {
                state.overlay = Overlay::None;
            } else {
                state.overlay = Overlay::Workflow;
                request_projection(state, ProjectionKind::Workflow);
                watch_focused_workflow(state);
            }
        }
        Action::OpenBlackboard => {
            if matches!(state.overlay, Overlay::Blackboard) {
                state.overlay = Overlay::None;
            } else {
                state.overlay = Overlay::Blackboard;
                watch_focused_blackboard_run(state);
            }
        }
        Action::OpenKanban => open_kanban(state),
        Action::MoveCardForward => move_focused_card(state, 1),
        Action::MoveCardBack => move_focused_card(state, -1),
        Action::OpenUiPlugins => open_ui_plugins(state),
        Action::SmokeTestUiPlugin => smoke_test_ui_plugin(state),
        Action::EnableUiPluginSession => enable_ui_plugin(state, "session"),
        Action::EnableUiPluginUser => enable_ui_plugin(state, "user"),
        Action::RevokeUiPlugin => begin_revoke_ui_plugin(state),
        Action::OpenIssues => {
            state.overlay = match state.overlay {
                Overlay::Issues => Overlay::None,
                _ => Overlay::Issues,
            }
        }
        Action::ClearIssues => {
            if matches!(state.overlay, Overlay::Issues) {
                state.issues.clear();
                state.selected_issue = 0;
                state.overlay = Overlay::None;
            }
        }
        Action::OpenPalette => {
            // Toggling the palette shut has the same return address as `Esc`.
            // Not an early `return`: the post-match Docs-lease release below
            // must run for every action.
            if matches!(state.overlay, Overlay::Palette { .. }) && state.palette_from_onboard {
                open_onboard(state);
            } else {
                state.overlay = match state.overlay {
                    Overlay::Edges => Overlay::EdgeSearch(state.edge_query.clone()),
                    Overlay::Palette { .. } => Overlay::None,
                    _ => Overlay::Palette {
                        query: String::new(),
                        selected: 0,
                    },
                }
            }
        }
        Action::BeginAddModel => begin_add_model(state),
        Action::ToggleLayout => {
            state.layout = state.layout.toggled();
            if matches!(state.layout, crate::state::LayoutMode::Workspace) {
                state.focus = Pane::Transcript;
            }
        }

        Action::Help => {
            state.overlay = match state.overlay {
                Overlay::Help => Overlay::None,
                _ => {
                    // Always open at the top, however the last visit was left.
                    state.help_scroll = 0;
                    Overlay::Help
                }
            }
        }
        Action::Detach => state.should_detach = true,
        Action::SessionForkFailed(err) => {
            state.notice = Some((
                format!("session fork failed: {}", err.message),
                state.tick + 60,
            ));
        }
        Action::Dismiss => {
            state.backtrack_primed = false;
            let overlay = std::mem::take(&mut state.overlay);
            state.overlay = match overlay {
                Overlay::ConfirmWorkflowCancel { .. } => Overlay::Workflow,
                Overlay::ConfirmUiPluginApprove { .. }
                | Overlay::ConfirmUiPluginReject { .. }
                | Overlay::ConfirmUiPluginEnable { .. }
                | Overlay::ConfirmUiPluginRevoke { .. } => Overlay::UiPlugins,
                Overlay::ConfirmCouncilDelete { .. } => Overlay::CouncilBrowser,
                Overlay::ConfirmLearningDelete { .. } | Overlay::LearningEdit { .. } => {
                    Overlay::Journey
                }
                Overlay::ConfirmModelRemove {
                    query, selected, ..
                } => Overlay::ModelPicker { query, selected },
                Overlay::ConfirmCommunityAcpInstall {
                    query,
                    selected,
                    onboard_class,
                    ..
                } => match onboard_class {
                    Some(class) => Overlay::OnboardProviderPicker {
                        class,
                        query,
                        selected,
                    },
                    None => Overlay::ProviderPicker { query, selected },
                },
                // Backing out of the delete confirmation returns to the Docs
                // Studio it floats over, not the base view.
                Overlay::DocDeleteConfirm { .. } => Overlay::Docs,
                Overlay::DocEdit { .. }
                | Overlay::DocNew { .. }
                | Overlay::DocInsert { .. }
                | Overlay::DocPublishTarget { .. }
                | Overlay::DocPublishPath { .. }
                | Overlay::DocPublishBranch { .. }
                | Overlay::DocPublishTitle { .. } => Overlay::Docs,
                Overlay::Backtrack(_) => Overlay::None,
                _ => Overlay::None,
            };
        }

        // --- Docs Studio live editing (Phase 4 STEP 4.3 client wiring) ---
        Action::EditDoc => {
            if matches!(state.overlay, Overlay::Journey) {
                if let Some(card) = state.focused_learning() {
                    state.overlay = Overlay::LearningEdit {
                        id: card.id.clone(),
                        revision: card.revision,
                        buffer: card.statement.clone(),
                    };
                }
            } else {
                begin_doc_edit(state)
            }
        }
        Action::NewDoc => begin_doc_new(state),
        Action::InsertDocBlock => begin_doc_insert(state),
        Action::DeleteDocBlock => begin_doc_delete(state),
        Action::PublishDoc => begin_doc_publish(state),
        Action::DocumentSynced {
            document_id,
            revision,
            blocks,
            suggestions,
        } => apply_document_sync(state, document_id, revision, blocks, suggestions),
        Action::DocumentLeaseGranted {
            document_id,
            lease_id,
        } => on_lease_granted(state, document_id, lease_id),
        Action::DocumentLeaseBlocked => on_lease_blocked(state),

        // --- Workflow-graph live overlay (Phase 5 T9) ---
        Action::WorkflowNodeUpdated {
            workflow_run_id,
            node_id,
            state: node_state,
            cost,
            error,
        } => apply_workflow_node_update(state, &workflow_run_id, &node_id, node_state, cost, error),
        Action::WorkflowSnapshotLoaded {
            workflow_run_id,
            phase,
            nodes,
        } => apply_workflow_snapshot(state, &workflow_run_id, phase, nodes),
        Action::WorkflowPhaseUpdated {
            workflow_run_id,
            phase,
        } => apply_workflow_phase(state, &workflow_run_id, phase),
        Action::BlackboardLoaded {
            workflow_run_id,
            items,
        } => replace_blackboard_run(state, &workflow_run_id, items),
        Action::BlackboardItemUpdated(item) => upsert_blackboard_item(state, item),
        Action::BoardLoaded(cards) => {
            state.kanban = cards;
            let count = state.kanban_in_display_order().len();
            clamp(&mut state.selected_card, count);
        }
        Action::BoardCardUpdated { card, superseded } => {
            // A superseded revision is REMOVED, not merged: the replacement
            // arrives as its own delivery, so the board never shows a card twice
            // (once in its old column and once in its new one).
            state.kanban.retain(|existing| existing.id != card.id);
            if !superseded {
                state.kanban.push(card);
            }
            let count = state.kanban_in_display_order().len();
            clamp(&mut state.selected_card, count);
        }

        // --- model discovery: the harness's fetched-list return path ---
        Action::ProviderModelsLoaded {
            provider_id,
            models,
            origin,
        } => on_provider_models_loaded(state, provider_id, models, origin),
        Action::ProviderModelsFailed {
            provider_id,
            reason,
        } => on_provider_models_failed(state, provider_id, reason),
        Action::ModelKeyVerified {
            model_id,
            ok,
            reason,
        } => on_model_key_verified(state, &model_id, ok, &reason),

        // --- `/keys` (D1): the harness's key-status projection ---
        Action::ApiKeyStatusesLoaded {
            models,
            tavily,
            voice,
        } => {
            state.key_status = models;
            state.tavily_key_status = tavily;
            state.voice_key_rows = voice;
        }

        // --- Local models: Unsloth catalog browse/pull ---
        Action::UnslothReposLoaded(repos) => on_unsloth_repos_loaded(state, repos),
        Action::UnslothReposFailed(reason) => on_unsloth_repos_failed(state, reason),
        Action::UnslothQuantsLoaded { repo_id, quants } => {
            on_unsloth_quants_loaded(state, repo_id, quants)
        }
        Action::UnslothQuantsFailed { repo_id, reason } => {
            on_unsloth_quants_failed(state, repo_id, reason)
        }
        Action::UnslothPullProgress {
            repo_id,
            quant,
            line,
        } => on_unsloth_pull_progress(state, repo_id, quant, line),
        Action::UnslothPullFinished {
            repo_id,
            quant,
            result,
        } => on_unsloth_pull_finished(state, repo_id, quant, result),

        Action::NoOp => {}
    }

    let remains_in_docs_flow = matches!(
        state.overlay,
        Overlay::Docs
            | Overlay::DocEdit { .. }
            | Overlay::DocNew { .. }
            | Overlay::DocInsert { .. }
            | Overlay::DocDeleteConfirm { .. }
            | Overlay::DocPublishTarget { .. }
            | Overlay::DocPublishPath { .. }
            | Overlay::DocPublishBranch { .. }
            | Overlay::DocPublishTitle { .. }
    );
    if docs_surface_was_open
        && (!remains_in_docs_flow || state.should_detach || state.session_closed)
    {
        release_doc_lease(state);
    }
}

fn apply_remote_ui_message(state: &mut AppState, message: UiWireMessage) {
    let document_id = message
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.document.document_id.clone())
        .or_else(|| {
            message
                .patch_batch
                .as_ref()
                .map(|batch| batch.document_id.clone())
        })
        .or_else(|| {
            message
                .error
                .as_ref()
                .and_then(|error| error.document_id.clone())
        });
    match state.remote_ui.handle(message) {
        Ok(UiSessionUpdate::RemoteError(error)) => {
            state.notice = Some((error.message, state.tick + 40));
            if error.recoverable {
                if let Some(document_id) = error.document_id {
                    request_remote_ui_resync(state, document_id);
                } else {
                    // A transport/broker lag error may be route-wide and omit a
                    // document id. Resync every authoritative mounted document
                    // rather than leaving all extension surfaces stale.
                    let documents: Vec<_> = state
                        .remote_ui
                        .host
                        .documents()
                        .documents()
                        .map(|document| document.document_id.clone())
                        .collect();
                    for document_id in documents {
                        request_remote_ui_resync(state, document_id);
                    }
                }
            }
        }
        Ok(UiSessionUpdate::Action(_)) => {
            state.issues.push(
                "Remote UI daemon sent a raw action to the renderer; it was not executed"
                    .to_owned(),
            );
        }
        Ok(_) => state.remote_ui.repair_focus(),
        Err(error) => {
            state.notice = Some((format!("Remote UI rejected: {error}"), state.tick + 40));
            if let Some(document_id) = document_id {
                request_remote_ui_resync(state, document_id);
            }
        }
    }
}

fn request_remote_ui_resync(state: &mut AppState, document_id: UiDocumentId) {
    let known_revision = state
        .remote_ui
        .host
        .documents()
        .document(&document_id)
        .map(|document| document.revision);
    let id = state.remote_ui.next_message_id("resync");
    let mut message = empty_message("resync", id);
    message.resync = Some(UiResyncRequest {
        document_id,
        known_revision,
    });
    state
        .outbox
        .push(Intent::RemoteUiMessage(Box::new(message)));
}

fn current_remote_output(
    state: &AppState,
) -> Option<(UiDocumentId, UiRevision, RemoteUiRenderOutput)> {
    let document_id = state.remote_ui.focused_document.clone()?;
    let revision = state
        .remote_ui
        .host
        .documents()
        .document(&document_id)?
        .revision;
    let output = state
        .remote_ui
        .last_render
        .borrow()
        .get(&document_id)?
        .clone();
    Some((document_id, revision, output))
}

fn remote_focus_order(state: &AppState) -> Vec<(UiDocumentId, UiNodeId)> {
    let outputs = state.remote_ui.last_render.borrow();
    state
        .remote_ui
        .mounted_documents()
        .into_iter()
        .flat_map(|document| {
            outputs
                .get(&document.document_id)
                .into_iter()
                .flat_map(move |output| {
                    output
                        .focus_order
                        .iter()
                        .filter(|descriptor| !descriptor.disabled)
                        .map(move |descriptor| {
                            (document.document_id.clone(), descriptor.node_id.clone())
                        })
                })
        })
        .collect()
}

/// Focus a document as a host operation only. This never emits an extension
/// event: entering a component must be distinct from activating its first
/// control. When render metadata is available, focus begins at the first enabled
/// node in that document; otherwise the document remains focused and the next
/// render/Tab repairs node focus.
fn focus_remote_document(state: &mut AppState, document_id: Option<UiDocumentId>) {
    let document_id = document_id.or_else(|| state.remote_ui.focused_document.clone());
    let Some(document_id) = document_id.or_else(|| {
        state
            .remote_ui
            .mounted_documents()
            .first()
            .map(|document| document.document_id.clone())
    }) else {
        state.remote_ui.active = false;
        state.remote_ui.focused_document = None;
        state.remote_ui.view.focused_node = None;
        return;
    };
    if !state
        .remote_ui
        .mounted_documents()
        .iter()
        .any(|document| document.document_id == document_id)
    {
        return;
    }
    state.remote_ui.active = true;
    state.remote_ui.focused_document = Some(document_id.clone());
    state.remote_ui.view.focused_node =
        remote_focus_order(state)
            .into_iter()
            .find_map(|(candidate_document, node_id)| {
                (candidate_document == document_id).then_some(node_id)
            });
}

fn focus_next_remote_document(state: &mut AppState) {
    let documents: Vec<_> = state
        .remote_ui
        .mounted_documents()
        .into_iter()
        .map(|document| document.document_id.clone())
        .collect();
    if documents.is_empty() {
        state.remote_ui.active = false;
        return;
    }
    let current = state
        .remote_ui
        .focused_document
        .as_ref()
        .and_then(|document_id| {
            documents
                .iter()
                .position(|candidate| candidate == document_id)
        });
    let next = current.map_or(0, |index| (index + 1) % documents.len());
    focus_remote_document(state, Some(documents[next].clone()));
}

fn focus_remote_ui(state: &mut AppState, delta: i32) {
    let focusable = remote_focus_order(state);
    if focusable.is_empty() {
        state.remote_ui.view.focused_node = None;
        return;
    }
    let current = state
        .remote_ui
        .focused_document
        .as_ref()
        .zip(state.remote_ui.view.focused_node.as_ref())
        .and_then(|(document_id, node_id)| {
            focusable
                .iter()
                .position(|(candidate_document, candidate_node)| {
                    candidate_document == document_id && candidate_node == node_id
                })
        });
    let next = match current {
        Some(current) if delta < 0 => current.checked_sub(1).unwrap_or(focusable.len() - 1),
        Some(current) if delta > 0 => (current + 1) % focusable.len(),
        Some(current) => current,
        None if delta < 0 => focusable.len() - 1,
        None => 0,
    };
    let (document_id, node_id) = focusable[next].clone();
    state.remote_ui.focused_document = Some(document_id);
    state.remote_ui.view.focused_node = Some(node_id);
}

fn apply_remote_ui_key(state: &mut AppState, key: RemoteKey, character: Option<char>) {
    match key {
        RemoteKey::Tab | RemoteKey::Down | RemoteKey::Right => focus_remote_ui(state, 1),
        RemoteKey::ShiftTab | RemoteKey::Up | RemoteKey::Left => focus_remote_ui(state, -1),
        RemoteKey::Character => {
            if let Some(character) = character {
                edit_remote_ui_field(state, |value| value.push(character));
            }
        }
        RemoteKey::Backspace => edit_remote_ui_field(state, |value| {
            value.pop();
        }),
        RemoteKey::Delete => edit_remote_ui_field(state, String::clear),
        RemoteKey::PageUp | RemoteKey::PageDown => {
            if let Some(node_id) = state.remote_ui.view.focused_node.clone() {
                let offset = state
                    .remote_ui
                    .view
                    .scroll_offsets
                    .entry(node_id)
                    .or_default();
                if key == RemoteKey::PageUp {
                    *offset = offset.saturating_sub(10);
                } else {
                    *offset = offset.saturating_add(10);
                }
            }
        }
        RemoteKey::Enter | RemoteKey::Space => {
            let Some((document_id, revision, output)) = current_remote_output(state) else {
                return;
            };
            let Some(target_id) = state.remote_ui.view.focused_node.clone() else {
                return;
            };
            let binding = output
                .focus_order
                .iter()
                .find(|descriptor| descriptor.node_id == target_id)
                .and_then(|descriptor| {
                    descriptor
                        .keyboard_actions
                        .iter()
                        .find(|action| action.key == key)
                })
                .map(|action| action.binding.clone());
            if let Some(binding) = binding {
                emit_remote_ui_event(state, document_id, revision, target_id, binding, None);
            }
        }
        RemoteKey::Escape | RemoteKey::Home | RemoteKey::End => {}
    }
}

fn edit_remote_ui_field(state: &mut AppState, edit: impl FnOnce(&mut String)) {
    let Some((document_id, revision, output)) = current_remote_output(state) else {
        return;
    };
    let Some(node_id) = state.remote_ui.view.focused_node.clone() else {
        return;
    };
    let Some(field) = output
        .form_fields
        .iter()
        .find(|field| field.node_id == node_id && !field.disabled && !field.read_only)
    else {
        return;
    };
    let current = state
        .remote_ui
        .view
        .input_values
        .get(&node_id)
        .unwrap_or(&field.value);
    let mut value = current.as_str().unwrap_or_default().to_owned();
    edit(&mut value);
    state
        .remote_ui
        .view
        .input_values
        .insert(node_id.clone(), Value::String(value.clone()));
    let change_binding = output
        .focus_order
        .iter()
        .find(|descriptor| descriptor.node_id == node_id)
        .and_then(|descriptor| {
            descriptor
                .keyboard_actions
                .iter()
                .map(|action| &action.binding)
                .find(|binding| matches!(binding.event.as_str(), "change" | "input"))
        })
        .cloned();
    if let Some(binding) = change_binding {
        emit_remote_ui_event(
            state,
            document_id,
            revision,
            node_id,
            binding,
            Some(serde_json::json!({"value": value})),
        );
    }
}

fn emit_remote_ui_event(
    state: &mut AppState,
    document_id: UiDocumentId,
    revision: UiRevision,
    target_id: UiNodeId,
    binding: UiActionBinding,
    user_payload: Option<Value>,
) {
    if binding.confirmation.is_some() {
        let key = (
            document_id.clone(),
            revision,
            target_id.clone(),
            binding.action_id.clone(),
        );
        // An arming that has outlived its notice is not an arming. The notice
        // faded after ~10 ticks while the armed state lived forever, so a stray
        // Enter long afterwards executed a confirmed action with nothing on
        // screen to say it had been armed. Re-arm instead of firing.
        let still_armed = state.remote_ui.pending_confirmation.as_ref() == Some(&key)
            && state.tick <= state.remote_ui.pending_confirmation_expires;
        if still_armed {
            state.remote_ui.pending_confirmation = None;
            state.remote_ui.pending_confirmation_expires = 0;
        } else {
            state.remote_ui.pending_confirmation = Some(key);
            state.remote_ui.pending_confirmation_expires = state.tick + CONFIRMATION_TICKS;
            state.notice = Some((
                binding
                    .confirmation
                    .clone()
                    .unwrap_or_else(|| "Confirm action".to_owned()),
                state.tick + CONFIRMATION_TICKS,
            ));
            return;
        }
    } else {
        state.remote_ui.pending_confirmation = None;
        state.remote_ui.pending_confirmation_expires = 0;
    }
    let event_type = binding.event.as_str().to_owned();
    // Producer-declared binding payload is resolved again from the live daemon
    // document. The renderer sends user data only and cannot overwrite those
    // constants.
    let mut payload = user_payload
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if event_type == "submit" {
        let form_nodes = state
            .remote_ui
            .host
            .documents()
            .document(&document_id)
            .and_then(|document| form_subtree_ids(&document.root, &target_id));
        if let (Some((_, _, output)), Some(form_nodes)) = (current_remote_output(state), form_nodes)
        {
            let mut form_data = Map::new();
            for field in output
                .form_fields
                .into_iter()
                .filter(|field| form_nodes.contains(&field.node_id))
            {
                let value = state
                    .remote_ui
                    .view
                    .input_values
                    .get(&field.node_id)
                    .cloned()
                    .unwrap_or(field.value);
                form_data.insert(field.name, value);
            }
            payload = form_data;
        }
    }
    let event_id = state.remote_ui.next_message_id("event");
    let mut message = empty_message("event", event_id.clone());
    message.event = Some(UiEvent {
        protocol_version: UiProtocolVersion::V1,
        event_id: UiEventId::from(event_id),
        document_id,
        revision,
        target_id,
        event_type: UiEventType::from(event_type),
        payload: Value::Object(payload),
        modifiers: Some(UiEventModifiers::default()),
        timestamp: None,
        interaction_token: None,
    });
    state
        .outbox
        .push(Intent::RemoteUiMessage(Box::new(message)));
}

fn form_subtree_ids(
    node: &codypendent_protocol::UiNode,
    target: &UiNodeId,
) -> Option<std::collections::HashSet<UiNodeId>> {
    fn contains(node: &codypendent_protocol::UiNode, target: &UiNodeId) -> bool {
        node.id.as_ref() == Some(target)
            || node.children.iter().any(|child| contains(child, target))
            || node
                .fallback
                .as_ref()
                .is_some_and(|fallback| contains(fallback, target))
    }
    fn collect(node: &codypendent_protocol::UiNode, ids: &mut std::collections::HashSet<UiNodeId>) {
        if let Some(id) = &node.id {
            ids.insert(id.clone());
        }
        for child in &node.children {
            collect(child, ids);
        }
        if let Some(fallback) = &node.fallback {
            collect(fallback, ids);
        }
    }
    for child in &node.children {
        if let Some(ids) = form_subtree_ids(child, target) {
            return Some(ids);
        }
    }
    if node
        .node_type
        .as_ref()
        .is_some_and(|kind| kind.as_str() == "Form")
        && contains(node, target)
    {
        let mut ids = std::collections::HashSet::new();
        collect(node, &mut ids);
        return Some(ids);
    }
    None
}

/// Overlay a live workflow node transition onto the graph-view cards (Phase 5 T9):
/// every card matching `node_id` takes the transition's pre-rendered `state` / `cost`
/// / `error`, so the view reflects the run advancing instead of the forever-`pending`
/// pre-run placeholders. Idempotent overwrite (a re-delivered transition writes the
/// same values), keyed by node id — the fold the CLI harness feeds after folding a
/// `Payload::WorkflowEvent`.
fn apply_workflow_node_update(
    state: &mut AppState,
    workflow_run_id: &str,
    node_id: &str,
    node_state: String,
    cost: String,
    error: String,
) {
    for card in state.workflow.iter_mut().filter(|card| {
        card.workflow_run_id.as_deref() == Some(workflow_run_id) && card.id == node_id
    }) {
        card.state = node_state.clone();
        card.cost = cost.clone();
        card.error = error.clone();
    }
}

fn apply_workflow_snapshot(
    state: &mut AppState,
    workflow_run_id: &str,
    phase: String,
    nodes: Vec<WorkflowNodeUpdate>,
) {
    apply_workflow_phase(state, workflow_run_id, phase);
    for node in nodes {
        apply_workflow_node_update(
            state,
            workflow_run_id,
            &node.node_id,
            node.state,
            node.cost,
            node.error,
        );
    }
}

fn apply_workflow_phase(state: &mut AppState, workflow_run_id: &str, phase: String) {
    for card in state
        .workflow
        .iter_mut()
        .filter(|card| card.workflow_run_id.as_deref() == Some(workflow_run_id))
    {
        card.run_phase = phase.clone();
    }
}

fn replace_blackboard_run(
    state: &mut AppState,
    workflow_run_id: &str,
    items: Vec<crate::state::BlackboardItemCard>,
) {
    state
        .blackboard
        .retain(|item| item.workflow_run_id != workflow_run_id);
    state.blackboard.extend(items);
    clamp(&mut state.selected_item, state.blackboard.len());
}

fn upsert_blackboard_item(state: &mut AppState, item: crate::state::BlackboardItemCard) {
    if let Some(existing) = state.blackboard.iter_mut().find(|card| card.id == item.id) {
        *existing = item;
    } else {
        state.blackboard.insert(0, item);
        state.selected_item = 0;
    }
}

/// Fold one durable event into run / transcript / approval state. The event's
/// `occurred_at` rides along to `push_entry`, which timestamps the transcript
/// entry it produces — this is what the transcript's turn-header clocks read.
fn apply_event(state: &mut AppState, event: SessionEvent) {
    let SessionEvent {
        actor,
        body,
        occurred_at: at,
        ..
    } = event;

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
            // Producer text on its way to a `Paragraph` cell: sanitize at
            // ingest for the same reason `append_model_text` does (a raw ESC is
            // one column wide to `unicode-width`, so it survives into a cell and
            // is written to the terminal verbatim). A note carries repository
            // content — the context manifest and curated memory statements —
            // so it is prompt-injectable, not merely daemon-authored.
            let text = crate::remote_ui::sanitize_terminal_text(&text);
            // A run-scoped note (context manifest, curated memory) is routed to
            // its own run so it can't land on whatever run happens to be selected
            // when runs interleave (issue #6 item 3); a session-level note (no
            // run_id) still attaches to the focused run.
            let target = match run_id {
                Some(run_id) => state.run_mut(run_id),
                None => state.selected_run_mut(),
            };
            let Some(run) = target else { return };

            // Backstage fold (Task 2): the context manifest and curated-memory
            // writes are real, but not part of the visible conversation. The
            // daemon labels both by the note's own text prefix (context:
            // `crates/knowledge/src/context.rs`'s `=== CONTEXT` manifest
            // header; memory: `executor.rs`'s `remembered: {statement}`), so
            // classify on that prefix and fold into the run's single
            // `Backstage` entry (find-or-push, update counts) instead of a
            // visible `Note` cell. Every other note falls through to the
            // existing declutter fold below, unchanged.
            let is_context = text.starts_with("=== CONTEXT");
            let is_memory = text.trim_start().starts_with("remembered:");
            if is_context || is_memory {
                let existing = run.transcript.iter_mut().find_map(|entry| match entry {
                    TranscriptEntry::Backstage { .. } => Some(entry),
                    _ => None,
                });
                let backstage = match existing {
                    Some(TranscriptEntry::Backstage {
                        context_lines,
                        memory_updates,
                        raw,
                        ..
                    }) => {
                        if is_context {
                            *context_lines = Some(text.lines().count());
                        }
                        if is_memory {
                            *memory_updates += 1;
                        }
                        raw.push(text);
                        return; // folded into the existing entry — no visible Note
                    }
                    _ => TranscriptEntry::Backstage {
                        context_lines: is_context.then(|| text.lines().count()),
                        memory_updates: is_memory as usize,
                        raw: vec![text],
                        expanded: false,
                    },
                };
                AppState::push_entry(run, backstage, at);
                return;
            }

            AppState::push_entry(
                run,
                TranscriptEntry::Note {
                    text,
                    expanded: false,
                },
                at,
            );
        }
        EventBody::SessionClosed => {
            state.session_closed = true;
            for run in &mut state.runs {
                run.activity = RunActivity::Idle;
            }
            state.notice = Some((
                "Session closed · transcript remains available".to_owned(),
                u64::MAX,
            ));
        }

        EventBody::RunStarted {
            run_id,
            objective,
            mode,
        } => {
            // The objective is echoed as the opening transcript turn AND kept
            // as the run's header label, both of which reach a `Paragraph`
            // cell; another client (or an automation) supplies it, so it is
            // sanitized here exactly like model text.
            let objective = crate::remote_ui::sanitize_terminal_text(&objective);
            // The first durable acknowledgement clears the local admission
            // guard. Replayed/other-client starts are harmless here: with a run
            // now projected, future submits follow the ordinary active/terminal
            // routing rather than the empty-session StartRun path.
            state.pending_run_start = None;
            let already_announced = state
                .runs
                .iter()
                .find(|run| run.run_id == run_id)
                .is_some_and(|run| {
                    !run.objective.is_empty()
                        || run
                            .transcript
                            .iter()
                            .any(|entry| matches!(entry, TranscriptEntry::User { .. }))
                });
            let run = state.ensure_run(run_id, objective.clone(), mode);
            if !already_announced {
                // A snapshot-created stub is filled by the first announcement;
                // replay/catch-up overlap after that is idempotent. In
                // particular, a repeated RunStarted must never resurrect a
                // terminal run or duplicate its opening transcript turn.
                run.objective = objective.clone();
                run.mode = mode;
                if !matches!(
                    run.state,
                    RunState::Completed | RunState::Failed | RunState::Cancelled
                ) {
                    run.state = RunState::Preparing;
                }
                AppState::push_entry(run, TranscriptEntry::User { text: objective }, at);
            }
        }
        EventBody::RunStateChanged { run_id, state: rs } => {
            if let Some(run) = state.run_mut(run_id) {
                run.state = rs;
                // Run state is authoritative over an older streaming/tool
                // activity. In particular, pause and both waiting states must
                // stop the spinner instead of looking like work is continuing.
                run.activity = match rs {
                    RunState::Preparing | RunState::Running | RunState::Recovering => {
                        RunActivity::Thinking
                    }
                    _ => RunActivity::Idle,
                };
            }
        }
        EventBody::ModelStreamDelta {
            run_id,
            text,
            thought,
        } => {
            if let Some(run) = state.run_mut(run_id) {
                // Reasoning coalesces into its own folded entry rather than the
                // speech tail. Both still mark the run as streaming: the model
                // IS producing output either way, and showing Idle while
                // reasoning arrives would read as a stall.
                if thought {
                    AppState::append_reasoning_text(run, &text, at);
                } else {
                    AppState::append_model_text(run, &text, at);
                }
                run.activity = RunActivity::Streaming;
            }
        }
        EventBody::ModelRetrying {
            run_id,
            attempt,
            max_attempts,
            ..
        } => {
            if let Some(run) = state.run_mut(run_id) {
                run.activity = RunActivity::Retrying {
                    attempt,
                    max_attempts,
                };
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
                        label: None,
                        outcome: None,
                        artifact: None,
                        approval_id: Some(approval_id),
                        output_preview: None,
                        expanded: false,
                    })),
                    at,
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
        EventBody::ToolDenied {
            run_id,
            action,
            reasons,
        } => {
            if let Some(run) = state.run_mut(run_id) {
                let outcome = ToolOutcome::Failed {
                    message: if reasons.is_empty() {
                        "denied by policy".to_string()
                    } else {
                        reasons.join("; ")
                    },
                };
                if let Some(card) = last_card(run, |card| {
                    card.status != ToolStatus::Completed && card.action.as_ref() == Some(&action)
                }) {
                    finish_tool_card(card, None, outcome, None);
                } else {
                    AppState::push_entry(
                        run,
                        TranscriptEntry::Tool(Box::new(ToolCard {
                            tool: String::new(),
                            status: ToolStatus::Completed,
                            action: Some(action),
                            args_digest: None,
                            label: None,
                            outcome: Some(outcome),
                            artifact: None,
                            approval_id: None,
                            output_preview: None,
                            expanded: false,
                        })),
                        at,
                    );
                }
                run.activity = RunActivity::Thinking;
            }
        }
        EventBody::ToolStarted {
            run_id,
            tool,
            args_digest,
            label,
        } => {
            // The tool name and its target label are provider-supplied text
            // drawn into the card; sanitize them at ingest like model text.
            let tool = crate::remote_ui::sanitize_terminal_text(&tool);
            let label = label
                .as_deref()
                .map(crate::remote_ui::sanitize_terminal_text);
            if let Some(run) = state.run_mut(run_id) {
                // Cloned before `tool` moves into the card below: the tool
                // card entering `Running` is what `RunActivity::RunningTool`
                // names.
                let tool_name = tool.clone();
                match last_card(run, |c| {
                    c.status == ToolStatus::Proposed
                        && c.action
                            .as_ref()
                            .is_some_and(|action| tool_matches_action(&tool, action))
                }) {
                    Some(card) => {
                        card.tool = tool;
                        card.args_digest = Some(args_digest);
                        card.label = label;
                        card.status = ToolStatus::Running;
                    }
                    None => AppState::push_entry(
                        run,
                        TranscriptEntry::Tool(Box::new(ToolCard {
                            tool,
                            status: ToolStatus::Running,
                            action: None,
                            args_digest: Some(args_digest),
                            label,
                            outcome: None,
                            artifact: None,
                            approval_id: None,
                            output_preview: None,
                            expanded: false,
                        })),
                        at,
                    ),
                }
                run.activity = RunActivity::RunningTool(tool_name);
            }
        }
        EventBody::ToolCompleted {
            run_id,
            tool,
            outcome,
            artifact,
        } => {
            // Sanitized on the same terms as `ToolStarted` — and with the same
            // function, so the name still matches the card it completes.
            let tool = crate::remote_ui::sanitize_terminal_text(&tool);
            if let Some(run) = state.run_mut(run_id) {
                if !reconcile_tool_completion(run, &tool, outcome.clone(), artifact.clone()) {
                    AppState::push_entry(
                        run,
                        TranscriptEntry::Tool(Box::new(ToolCard {
                            tool,
                            status: ToolStatus::Completed,
                            action: None,
                            args_digest: None,
                            label: None,
                            outcome: Some(outcome),
                            artifact,
                            approval_id: None,
                            output_preview: None,
                            expanded: false,
                        })),
                        at,
                    );
                }
                // The tool finished; the agent is back to composing its next
                // step.
                run.activity = RunActivity::Thinking;
            }
        }
        EventBody::PatchProposed {
            run_id,
            changeset_id,
            artifact,
            files,
            additions,
            deletions,
            preview,
            preview_truncated,
        } => {
            if let Some(run) = state.run_mut(run_id) {
                AppState::push_entry(
                    run,
                    TranscriptEntry::Patch(PatchSummary {
                        changeset_id,
                        artifact,
                        files,
                        additions,
                        deletions,
                        preview,
                        preview_truncated,
                        expanded: false,
                    }),
                    at,
                );
            }
        }
        EventBody::ApprovalRequested {
            approval_id,
            action,
            risk,
            pattern,
        } => {
            if !state.terminal_focused {
                state.outbox.push(Intent::Notify {
                    message: format!("Approval requested: {action:?}"),
                });
            }
            let run_id = run_of_approval(state, approval_id);
            // Sanitized at INGEST, like model text — the modal must never be
            // handed strings that can move the cursor or drive the terminal.
            let action = sanitize_proposed_action(action);
            let pending = PendingApproval {
                approval_id,
                action,
                risk,
                run_id,
                pattern,
            };
            if let Some(existing) = state
                .pending_approvals
                .iter_mut()
                .find(|approval| approval.approval_id == approval_id)
            {
                *existing = pending;
            } else {
                state.pending_approvals.push(pending);
            }
            // New or replaced content owns the modal body — start at the top.
            state.approval_scroll = 0;
        }
        EventBody::ApprovalResolved {
            approval_id,
            decision,
        } => {
            state
                .pending_approvals
                .retain(|p| p.approval_id != approval_id);
            clamp(&mut state.selected_approval, state.pending_approvals.len());
            // The resolution hands the modal to the next stacked approval.
            state.approval_scroll = 0;
            if decision == ApprovalDecision::Reject {
                if let Some(card) = state.runs.iter_mut().find_map(|run| {
                    last_card(run, |card| {
                        card.status == ToolStatus::Proposed && card.approval_id == Some(approval_id)
                    })
                }) {
                    finish_tool_card(
                        card,
                        None,
                        ToolOutcome::Failed {
                            message: "approval rejected".to_owned(),
                        },
                        None,
                    );
                }
            }
        }
        EventBody::QuestionAsked {
            question_id,
            run_id,
            questions,
        } => {
            // The question card is the OTHER surface where the operator makes a
            // decision, and its prompts, headers and option labels are chosen
            // by the model. Sanitized at ingest for the same reason as the
            // approval evidence above.
            let questions: Vec<_> = questions
                .into_iter()
                .map(|prompt| codypendent_protocol::question::QuestionPrompt {
                    header: crate::remote_ui::sanitize_terminal_text(&prompt.header),
                    question: crate::remote_ui::sanitize_terminal_text(&prompt.question),
                    options: prompt
                        .options
                        .into_iter()
                        .map(|option| codypendent_protocol::question::QuestionOption {
                            label: crate::remote_ui::sanitize_terminal_text(&option.label),
                            description: crate::remote_ui::sanitize_terminal_text(
                                &option.description,
                            ),
                        })
                        .collect(),
                    ..prompt
                })
                .collect();
            let pending = PendingQuestion {
                question_id,
                run_id,
                questions: questions.clone(),
                asked_at: at,
            };
            // A re-issued question can carry a DIFFERENT number of
            // sub-questions, and the card holds one `picked`/`custom_text` slot
            // per sub-question. Replacing the question without resizing the card
            // left it sized for the previous shape: `custom_text[card.index]`
            // then panicked the whole TUI on a longer question, and a shorter
            // one stranded the cursor past the end where no key could move it.
            // Both were daemon-triggerable, while a question was blocking the
            // operator.
            let replaced_shape = state
                .pending_questions
                .iter_mut()
                .find(|q| q.question_id == question_id)
                .map(|existing| {
                    let previous = existing.questions.len();
                    *existing = pending;
                    previous
                });
            match replaced_shape {
                Some(previous) if previous != questions.len() => {
                    // Answers already given are for a question that no longer
                    // exists in this shape; start the card clean rather than
                    // carry selections across a redefinition.
                    state.question_card_state = Some(QuestionCardState::new(questions.len()));
                }
                Some(_) => {}
                None => {
                    state.pending_questions.push(PendingQuestion {
                        question_id,
                        run_id,
                        questions: questions.clone(),
                        asked_at: at,
                    });
                }
            }
            if state.question_card_state.is_none() {
                state.question_card_state = Some(QuestionCardState::new(questions.len()));
            }
        }
        EventBody::QuestionResolved {
            question_id,
            outcome,
        } => {
            // The event carries no run_id, but the parked PendingQuestion it
            // resolves does — capture it BEFORE retaining so a Rejected outcome
            // closes exactly that run's question card, never another concurrent
            // user.ask run's card (a cross-run "last Running user.ask" scan would).
            let resolved_run_id = state
                .pending_questions
                .iter()
                .find(|p| p.question_id == question_id)
                .map(|p| p.run_id);
            state
                .pending_questions
                .retain(|p| p.question_id != question_id);
            if state.pending_questions.is_empty() {
                state.question_card_state = None;
            } else if let Some(first) = state.pending_questions.first() {
                state.question_card_state = Some(QuestionCardState::new(first.questions.len()));
            }
            if matches!(outcome, QuestionOutcome::Rejected { .. }) {
                if let Some(card) = resolved_run_id.and_then(|run_id| {
                    state.run_mut(run_id).and_then(|run| {
                        last_card(run, |card| {
                            card.tool == "user.ask" && card.status == ToolStatus::Running
                        })
                    })
                }) {
                    finish_tool_card(
                        card,
                        None,
                        ToolOutcome::Failed {
                            message: "question rejected".to_owned(),
                        },
                        None,
                    );
                }
            }
        }
        EventBody::CheckpointRecorded {
            run_id,
            checkpoint_id,
            ordinal,
            ..
        } => {
            if let Some(run) = state.run_mut(run_id) {
                if ordinal == 1 {
                    run.launch_checkpoint = Some(checkpoint_id);
                }
            }
        }
        EventBody::CheckpointRestored {
            run_id,
            checkpoint_id: _,
            restored,
        } => {
            if restored {
                if let Some(run) = state.run_mut(run_id) {
                    AppState::push_entry(
                        run,
                        TranscriptEntry::Note {
                            text: "Restored filesystem checkpoint".to_string(),
                            expanded: false,
                        },
                        at,
                    );
                }
            }
        }
        EventBody::SessionForked { from_session, .. } => {
            state.forked_from = Some(from_session);
        }
        EventBody::SteeringQueued { run_id } => {
            if let Some(run) = state.run_mut(run_id) {
                AppState::push_entry(run, TranscriptEntry::Steering { applied: false }, at);
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
                    None => {
                        AppState::push_entry(run, TranscriptEntry::Steering { applied: true }, at);
                    }
                }
            }
        }
        EventBody::PendingPromptsChanged { prompts } => {
            state.pending_prompts = prompts
                .into_iter()
                .map(|p| PendingPromptCard {
                    id: p.id,
                    text: p.text,
                    delivery: p.delivery,
                })
                .collect();
            if state.pending_prompts.is_empty() {
                state.queue_selected = None;
                state.queue_editing = None;
            } else if let Some(sel) = state.queue_selected {
                if sel >= state.pending_prompts.len() {
                    state.queue_selected = Some(state.pending_prompts.len() - 1);
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
                    at,
                );
            }
        }
        EventBody::RunCompleted {
            run_id,
            disposition,
            ..
        } => {
            if !state.terminal_focused {
                state.outbox.push(Intent::Notify {
                    message: "Run completed".to_string(),
                });
            }
            if let Some(run) = state.run_mut(run_id) {
                terminalize_open_tool_cards(run, &disposition);
                run.state = terminal_state(&disposition);
                AppState::push_entry(
                    run,
                    TranscriptEntry::Completed {
                        disposition: disposition.clone(),
                        expanded: false,
                    },
                    at,
                );
                run.disposition = Some(disposition);
                run.activity = RunActivity::Idle;
            }
        }
        // The run's MEASURED usage, emitted after RunCompleted. Without this arm
        // it fell into the forward-compatibility catch-all below and the product
        // printed `? unsupported event` where the tokens belong — a placeholder
        // for a FUTURE protocol, triggered by an event from this same build.
        // Quiet like `LearningsCaptured` (no card of its own): the numbers land
        // on the run, and the run's own completion row, header, footer and Run
        // detail render them.
        //
        // Each dimension is folded independently and only when present, because
        // an absent one means "the provider did not measure it", not zero — an
        // unpriced local model reports tokens and no money, and must not read as
        // free. A repeat event (a resumed run measured twice) overwrites rather
        // than accumulates: the daemon publishes the run's total, not a delta.
        EventBody::RunUsage {
            run_id,
            prompt_tokens,
            completion_tokens,
            cost_micros,
        } => {
            if let Some(run) = state.run_mut(run_id) {
                if prompt_tokens.is_some() {
                    run.prompt_tokens = prompt_tokens;
                }
                if completion_tokens.is_some() {
                    run.completion_tokens = completion_tokens;
                }
                if cost_micros.is_some() {
                    run.cost_micros = cost_micros;
                }
            }
        }

        EventBody::ContextUsage {
            run_id,
            used_tokens,
            window_tokens,
            system_tokens,
            tool_tokens,
            transcript_tokens,
        } => {
            // Strictly scoped to `run_id`, like every other run-scoped arm: a
            // usage report for a run this client has not materialised (attach
            // mid-stream, trimmed catch-up window, background run) must never
            // be painted onto whichever run happens to be selected.
            if let Some(run) = state.run_mut(run_id) {
                let percent = (used_tokens * 100)
                    .checked_div(window_tokens)
                    .unwrap_or(0)
                    .min(100) as u16;
                run.context_percent = Some(percent);
                run.context_breakdown = Some(crate::state::ContextBreakdown {
                    used_tokens,
                    window_tokens,
                    system_tokens,
                    tool_tokens,
                    transcript_tokens,
                });
            }
        }

        // Capture is intentionally emitted after RunCompleted. Keep it quiet
        // (no transcript card), but always preserve the review count and
        // refresh an already-open Journey even though the run is terminal.
        EventBody::LearningsCaptured {
            proposed_count,
            activated_count,
            ..
        } => {
            state.pending_learning_review =
                state.pending_learning_review.saturating_add(proposed_count);
            if matches!(state.overlay, Overlay::Journey) {
                request_projection(state, ProjectionKind::Journey);
            }
            let _ = activated_count;
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
            // Presence is useful ambient information, but it must never erase a
            // rejected-command/setup notice that needs action.
            if state.notice.is_none() {
                state.notice = Some((
                    format!("client {short} {verb} ({})", role_label(role)),
                    state.tick + 10,
                ));
            }
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
                    at,
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

fn finish_tool_card(
    card: &mut ToolCard,
    tool: Option<&str>,
    outcome: ToolOutcome,
    artifact: Option<codypendent_protocol::ArtifactRef>,
) {
    if card.tool.is_empty() {
        if let Some(tool) = tool {
            card.tool = tool.to_owned();
        }
    }
    card.status = ToolStatus::Completed;
    card.outcome = Some(outcome);
    card.artifact = artifact;
}

/// Reconcile a completion with the most specific preceding lifecycle card.
/// The wire has no invocation id, so precedence is important: exact Running,
/// then capability-compatible Proposed (approval rejected before start), then a
/// just-completed unnamed denial card (the runtime emits ToolDenied followed by
/// ToolCompleted). Returning false asks the caller to append a standalone card.
fn reconcile_tool_completion(
    run: &mut RunView,
    tool: &str,
    outcome: ToolOutcome,
    artifact: Option<codypendent_protocol::ArtifactRef>,
) -> bool {
    if let Some(card) = last_card(run, |card| {
        card.status == ToolStatus::Running && card.tool == tool
    }) {
        finish_tool_card(card, Some(tool), outcome, artifact);
        return true;
    }
    if let Some(card) = last_card(run, |card| {
        card.status == ToolStatus::Proposed
            && card
                .action
                .as_ref()
                .is_some_and(|action| tool_matches_action(tool, action))
    }) {
        finish_tool_card(card, Some(tool), outcome, artifact);
        return true;
    }
    if let Some(card) = last_card(run, |card| {
        card.status == ToolStatus::Completed
            && card.tool.is_empty()
            && card
                .action
                .as_ref()
                .is_some_and(|action| tool_matches_action(tool, action))
    }) {
        finish_tool_card(card, Some(tool), outcome, artifact);
        return true;
    }
    // Final fallback: the wire carries no invocation id, and some drivers —
    // notably the ACP bridge — label a completion by the tool's TARGET (e.g.
    // `apps/frontend/package.json`) while the start named the tool KIND (e.g.
    // `read`), so none of the matches above pair. Pairing with the running card
    // (keeping its start title, since `finish_tool_card` preserves a non-empty
    // name) beats orphaning the start card — an orphan is force-failed by
    // `terminalize_open_tool_cards` at run end, painting a ✗ on a tool that
    // actually succeeded and leaving a duplicate target-titled card behind.
    // Only pair when exactly one tool is running, which makes the attribution
    // unambiguous. With zero running cards the completion belongs to no start
    // at all, and with two or more a guess would hand tool A's outcome to tool
    // B — leaving B's own completion to find nothing and push a duplicate
    // orphan. Both of those fall through to a standalone card instead.
    let running = run
        .transcript
        .iter()
        .filter(|entry| {
            matches!(entry, TranscriptEntry::Tool(card) if card.status == ToolStatus::Running)
        })
        .count();
    if running == 1 {
        if let Some(card) = last_card(run, |card| card.status == ToolStatus::Running) {
            finish_tool_card(card, Some(tool), outcome, artifact);
            return true;
        }
    }
    false
}

/// A terminal run cannot still have an executing/awaiting tool. Close every
/// open card with an honest client-derived failure so cancellation and a lost
/// lifecycle event never leave a permanent spinner in transcript history.
fn terminalize_open_tool_cards(run: &mut RunView, disposition: &RunDisposition) {
    let message = match disposition {
        RunDisposition::Cancelled { .. } => "run cancelled before the tool completed",
        RunDisposition::Failed { .. } => "run failed before the tool completed",
        _ => "run ended before the tool reported completion",
    };
    for entry in &mut run.transcript {
        if let TranscriptEntry::Tool(card) = entry {
            if card.status != ToolStatus::Completed {
                finish_tool_card(
                    card,
                    None,
                    ToolOutcome::Failed {
                        message: message.to_owned(),
                    },
                    None,
                );
            }
        }
    }
}

/// Correlate a started tool with the action shown on its approval card. The
/// wire protocol does not yet expose an invocation id on all three lifecycle
/// events, so matching by capability plus exact tool name is the strongest
/// stable identity available (and avoids mutating an unrelated parallel card
/// merely because it happens to be `Proposed`).
fn tool_matches_action(tool: &str, action: &ProposedAction) -> bool {
    match action {
        ProposedAction::ReadFiles { .. } => {
            matches!(tool, "workspace.read_file" | "workspace.search")
        }
        ProposedAction::WritePatch { .. } => matches!(
            tool,
            "workspace.write_file" | "workspace.edit_file" | "git.apply_patch"
        ),
        ProposedAction::ExecuteCommand { .. } => tool == "shell.run" || tool.starts_with("git."),
        ProposedAction::NetworkRequest { .. } => tool == "web.search",
        ProposedAction::GitCommit { .. } | ProposedAction::GitPush { .. } => {
            tool.starts_with("git.")
        }
        ProposedAction::GitHubMutation { .. } => tool.starts_with("github."),
        ProposedAction::McpToolCall {
            server, tool: name, ..
        } => tool == format!("mcp.{server}.{name}"),
        ProposedAction::PublishDocument { .. } => tool == "document.publish",
        ProposedAction::CouncilCreate { .. } => tool == "council.create",
        ProposedAction::CouncilRun { .. } => tool == "council.run",
        ProposedAction::WorkflowCreate { .. } => tool == "workflow.create",
        ProposedAction::WorkflowRun { .. } => tool == "workflow.run",
        _ => false,
    }
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

pub fn filter_session_rows(sessions: &[crate::state::SessionRow], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    sessions
        .iter()
        .enumerate()
        .filter_map(|(idx, s)| {
            // An internal session (a council member, a workflow node) is a
            // child of some visible run and must never present as a top-level
            // conversation the operator can resume. `ListSessions` reports the
            // flag faithfully but does not exclude the row, so the exclusion
            // lives here, where every picker row passes through.
            if s.internal {
                return None;
            }
            if q.is_empty()
                || s.title.to_lowercase().contains(&q)
                || s.session_id.to_string().to_lowercase().contains(&q)
            {
                Some(idx)
            } else {
                None
            }
        })
        .collect()
}

// --- Session Library ---------------------------------------------------------
//
// Unlike the session PICKER above, the library is server-ranked: the daemon
// applies the owner predicate, the filters, and the ordering, and hands back an
// opaque continuation cursor. The reducer therefore never re-filters
// `session_library` — it only echoes the query and the cursor back out as
// intents, and folds whatever the daemon answered.

/// Open the library and ask for the first (empty-query) page.
fn open_session_library(state: &mut AppState) {
    state.overlay = Overlay::SessionLibrary {
        query: String::new(),
        selected: 0,
        waiting: true,
    };
    state.session_library.clear();
    state.session_library_query = String::new();
    state.session_library_cursor = None;
    state.outbox.push(Intent::SearchSessions {
        query: String::new(),
        cursor: None,
    });
}

/// Re-query when the library's typed query has moved past the query its rows
/// answer. A no-op for every other overlay, so it is safe to call from the
/// shared text-edit path.
fn sync_session_library(state: &mut AppState) {
    let Overlay::SessionLibrary { query, .. } = &state.overlay else {
        return;
    };
    let query = query.clone();
    if query == state.session_library_query {
        return;
    }
    state.session_library_query.clone_from(&query);
    state.session_library.clear();
    state.session_library_cursor = None;
    if let Overlay::SessionLibrary {
        selected, waiting, ..
    } = &mut state.overlay
    {
        *selected = 0;
        *waiting = true;
    }
    state.outbox.push(Intent::SearchSessions {
        query,
        cursor: None,
    });
}

/// Ask for the next page. Silently does nothing at the end of the result set
/// (no cursor) or while a page is already in flight.
fn request_session_library_page(state: &mut AppState) {
    let Some(cursor) = state.session_library_cursor.clone() else {
        return;
    };
    let query = match &mut state.overlay {
        Overlay::SessionLibrary { query, waiting, .. } if !*waiting => {
            *waiting = true;
            query.clone()
        }
        _ => return,
    };
    state.outbox.push(Intent::SearchSessions {
        query,
        cursor: Some(cursor),
    });
}

/// `↑`/`↓` in the library. Landing on the last loaded row pulls the next page,
/// which is how the cursor is consumed — there is no separate "more" key.
fn session_library_nav(state: &mut AppState, delta: i32) {
    let count = state.session_library.len();
    let mut at_end = false;
    if let Overlay::SessionLibrary { selected, .. } = &mut state.overlay {
        step(selected, count, delta);
        at_end = count > 0 && *selected + 1 == count;
    }
    if at_end && delta > 0 {
        request_session_library_page(state);
    }
}

/// The row the library cursor points at, if the library is open.
fn focused_library_row(state: &AppState) -> Option<(usize, crate::state::SessionRow)> {
    let Overlay::SessionLibrary { selected, .. } = &state.overlay else {
        return None;
    };
    state
        .session_library
        .get(*selected)
        .map(|row| (*selected, row.clone()))
}

fn session_search_loaded(
    state: &mut AppState,
    query: String,
    rows: Vec<crate::state::SessionRow>,
    next_cursor: Option<codypendent_protocol::PageCursor>,
    append: bool,
) {
    // A page for a query the operator has already typed past answers a
    // question nobody is asking any more. Dropping it is the only way to keep
    // the heading and the rows honest about each other.
    if query != state.session_library_query {
        return;
    }
    if append {
        state.session_library.extend(rows);
    } else {
        state.session_library = rows;
    }
    state.session_library_cursor = next_cursor;
    if let Overlay::SessionLibrary {
        selected, waiting, ..
    } = &mut state.overlay
    {
        *waiting = false;
        if *selected >= state.session_library.len() {
            *selected = state.session_library.len().saturating_sub(1);
        }
    }
}

/// A refused search for the CURRENT query stops the wait and says why. A
/// refusal for a query already typed past is discarded like a stale page: it
/// answers a question nobody is asking, and surfacing it would attribute the
/// failure to the wrong query.
fn session_search_failed(state: &mut AppState, query: String, reason: String) {
    if query != state.session_library_query {
        return;
    }
    if let Overlay::SessionLibrary { waiting, .. } = &mut state.overlay {
        *waiting = false;
    }
    // The cursor is dropped too: a continuation token whose page was refused
    // must not be retried as though it were still good.
    state.session_library_cursor = None;
    state.notice = Some((format!("session search failed: {reason}"), state.tick + 60));
}

fn session_lifecycle_applied(state: &mut AppState, row: crate::state::SessionRow) {
    let session_id = row.session_id;
    if let Some(existing) = state
        .session_library
        .iter_mut()
        .find(|r| r.session_id == session_id)
    {
        // Keep the ranked hit's excerpt: a lifecycle projection carries none,
        // and coercing it to `None` would silently erase evidence the daemon
        // did return for this row.
        let excerpt = existing.excerpt.clone();
        *existing = crate::state::SessionRow {
            excerpt,
            ..row.clone()
        };
    }
    if let Some(existing) = state
        .session_list
        .iter_mut()
        .find(|r| r.session_id == session_id)
    {
        existing.title.clone_from(&row.title);
        existing.state.clone_from(&row.state);
        existing.pinned = row.pinned;
        existing.archived = row.archived;
    }
}

fn session_lifecycle_deleted(
    state: &mut AppState,
    session_id: codypendent_protocol::SessionId,
    tombstoned: bool,
) {
    state.session_library.retain(|r| r.session_id != session_id);
    state.session_list.retain(|r| r.session_id != session_id);
    if let Overlay::SessionLibrary { selected, .. } = &mut state.overlay {
        if *selected >= state.session_library.len() {
            *selected = state.session_library.len().saturating_sub(1);
        }
    }
    state.notice = Some((
        if tombstoned {
            format!("session {session_id} tombstoned")
        } else {
            format!("session {session_id} deleted")
        },
        state.tick + 40,
    ));
}

fn session_library_toggle_pin(state: &mut AppState) {
    let Some((_, row)) = focused_library_row(state) else {
        return;
    };
    let action = if row.pinned {
        codypendent_protocol::SessionLifecycleAction::Unpin
    } else {
        codypendent_protocol::SessionLifecycleAction::Pin
    };
    state.outbox.push(Intent::MutateSession {
        session_id: row.session_id,
        action,
    });
}

fn session_library_toggle_archive(state: &mut AppState) {
    let Some((_, row)) = focused_library_row(state) else {
        return;
    };
    let action = if row.archived {
        codypendent_protocol::SessionLifecycleAction::Restore
    } else {
        codypendent_protocol::SessionLifecycleAction::Archive
    };
    state.outbox.push(Intent::MutateSession {
        session_id: row.session_id,
        action,
    });
}

fn session_library_begin_rename(state: &mut AppState) {
    let Some((selected, row)) = focused_library_row(state) else {
        return;
    };
    state.overlay = Overlay::SessionRename {
        session_id: row.session_id,
        buffer: row.title,
        selected,
    };
}

fn session_library_export(state: &mut AppState) {
    let Some((_, row)) = focused_library_row(state) else {
        return;
    };
    state.outbox.push(Intent::MutateSession {
        session_id: row.session_id,
        action: codypendent_protocol::SessionLifecycleAction::Export {
            options: codypendent_protocol::SessionExportOptions {
                format: codypendent_protocol::SessionExportFormat::Markdown,
                // Both switches default closed: an export widens what leaves the
                // daemon, so the TUI never opts into more than the transcript.
                include_artifacts: false,
                include_internal_sessions: false,
            },
        },
    });
    state.notice = Some(("exporting session\u{2026}".to_owned(), state.tick + 40));
}

/// Return to the library from a prompt it opened, restoring the cursor.
fn return_to_session_library(state: &mut AppState, selected: usize) {
    state.overlay = Overlay::SessionLibrary {
        query: state.session_library_query.clone(),
        selected: selected.min(state.session_library.len().saturating_sub(1)),
        waiting: false,
    };
}

/// Move the selection / scroll by `delta` (-1 or +1). When a knowledge browser
/// is open it drives that browser's list; otherwise it drives the focused pane.
fn nav(state: &mut AppState, delta: i32) {
    // Handled before the match below because reaching the last loaded row has
    // to push a continuation intent, which cannot happen while `state.overlay`
    // is mutably borrowed by a match arm.
    if matches!(state.overlay, Overlay::SessionLibrary { .. }) {
        session_library_nav(state, delta);
        return;
    }
    match state.overlay {
        Overlay::Issues => {
            step(&mut state.selected_issue, state.issues.len(), delta);
            return;
        }
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
        Overlay::Journey => {
            step(&mut state.selected_learning, state.learnings.len(), delta);
            return;
        }
        Overlay::Docs => {
            match state.doc_focus {
                // The tree drives the document selection (the default rail, so this
                // is the pre-editing behaviour). A different document resets the
                // block/suggestion cursors so they never point past the new lists.
                DocFocus::Tree => {
                    step(&mut state.selected_doc, state.docs.len(), delta);
                    state.selected_block = 0;
                    state.selected_suggestion = 0;
                    watch_focused_doc(state);
                }
                DocFocus::Editor => {
                    let len = state.focused_doc().map_or(0, |d| d.blocks.len());
                    step(&mut state.selected_block, len, delta);
                }
                DocFocus::Review => {
                    let len = state.focused_doc().map_or(0, |d| d.suggestions.len());
                    step(&mut state.selected_suggestion, len, delta);
                }
            }
            return;
        }
        Overlay::Edges => {
            step(&mut state.selected_edge, state.edges.len(), delta);
            return;
        }
        Overlay::Workflow => {
            step(&mut state.selected_node, state.workflow.len(), delta);
            watch_focused_workflow(state);
            return;
        }
        Overlay::Blackboard => {
            step(&mut state.selected_item, state.blackboard.len(), delta);
            watch_focused_blackboard_run(state);
            return;
        }
        Overlay::Kanban => {
            // Selection walks the board's DISPLAY order (column by column), so
            // ↑/↓ runs down a column and then continues into the next one.
            let count = state.kanban_in_display_order().len();
            step(&mut state.selected_card, count, delta);
            return;
        }
        Overlay::UiPlugins => {
            step(&mut state.selected_ui_plugin, state.ui_plugins.len(), delta);
            return;
        }
        Overlay::CouncilBrowser => {
            step(&mut state.selected_council, state.councils.len(), delta);
            return;
        }
        Overlay::CouncilResults => {
            step(
                &mut state.selected_council_result,
                state.council_results.len(),
                delta,
            );
            state.council_result_scroll = 0;
            return;
        }
        Overlay::Backtrack(_) => {
            let count = state.forkable_runs().len();
            if let Overlay::Backtrack(ref mut bt) = state.overlay {
                step(&mut bt.selected, count, delta);
            }
            return;
        }
        Overlay::Onboard {
            step: ref mut onboard,
        } => {
            match onboard {
                OnboardStep::Triage { selected } | OnboardStep::SkipConfirm { selected } => {
                    step(selected, 3, delta);
                }
                OnboardStep::Validating { .. } => {}
            }
            return;
        }
        Overlay::OnboardProviderPicker {
            class,
            ref query,
            ref mut selected,
        } => {
            let indices = filter_onboard_providers(&state.providers, class, query);
            step(selected, indices.len(), delta);
            state.selected_provider = indices.get(*selected).copied().unwrap_or(0);
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
        Overlay::SessionPicker {
            ref query,
            ref mut selected,
        } => {
            let count = filter_session_rows(&state.session_list, query).len();
            step(selected, count, delta);
            return;
        }
        Overlay::ModelPicker {
            ref query,
            ref mut selected,
        } => {
            let indices = filter_models(&state.models, query);
            step(selected, indices.len(), delta);
            // Keep `selected_model` resolved to the same card the filtered
            // cursor points at, so `focused_model()` (the detail panel, and
            // Enter's staging) reads it without re-deriving the filter.
            state.selected_model = indices.get(*selected).copied().unwrap_or(0);
            return;
        }
        // Same shape as the model picker (Task 8): keep `selected_provider`
        // resolved to the same card the filtered cursor points at.
        Overlay::ProviderPicker {
            ref query,
            ref mut selected,
        } => {
            let indices = filter_providers(&state.providers, query);
            step(selected, indices.len(), delta);
            state.selected_provider = indices.get(*selected).copied().unwrap_or(0);
            return;
        }
        // The mode picker (PR C2): same filtered-cursor shape, over the static
        // [`MODE_CARDS`] table — there is no `AppState` list to re-resolve.
        Overlay::ModePicker {
            ref query,
            ref mut selected,
        } => {
            let indices = filter_modes(query);
            step(selected, indices.len(), delta);
            return;
        }
        // The theme picker: the same shape again. Moving the cursor is all the
        // preview needs — the renderer reads the focused row every frame.
        Overlay::ThemePicker {
            ref query,
            ref mut selected,
        } => {
            let indices = filter_themes(&state.themes, query);
            step(selected, indices.len(), delta);
            return;
        }
        // The `/keys` overlay (D1): the same filtered-cursor shape, over the
        // model list plus the final Tavily row — no resolved `AppState` index
        // (like the mode picker).
        Overlay::ApiKeys {
            ref query,
            ref mut selected,
        } => {
            let indices = filter_key_rows(&state.models, &state.voice_key_rows, query);
            step(selected, indices.len(), delta);
            return;
        }
        // A fixed, unfiltered three-row list — the cursor is all there is to move.
        Overlay::DocPublishTarget {
            ref mut selected, ..
        } => {
            step(selected, DOC_PUBLISH_TARGETS.len(), delta);
            return;
        }
        Overlay::CouncilBuilder(ref mut builder) => {
            let count = match builder.step {
                CouncilBuilderStep::MemberModel => {
                    let continue_row =
                        usize::from(builder.members.len() >= 2 && builder.query.trim().is_empty());
                    let remove_row =
                        usize::from(!builder.members.is_empty() && builder.query.trim().is_empty());
                    let available = if builder.members.len() >= 8 {
                        0
                    } else {
                        filter_council_member_models(
                            &state.models,
                            &builder.query,
                            &builder.members,
                        )
                        .len()
                    };
                    continue_row + available + remove_row
                }
                CouncilBuilderStep::Chair => filter_models(&state.models, &builder.query).len(),
                CouncilBuilderStep::Rounds => 3,
                _ => 0,
            };
            step(&mut builder.selected, count, delta);
            if builder.step == CouncilBuilderStep::Rounds {
                builder.rounds = u8::try_from(builder.selected + 1).unwrap_or(3).clamp(1, 3);
            }
            return;
        }
        // The add-model pick-list (model-discovery): the same shape as the
        // model/provider pickers, over the overlay's own `models` field rather
        // than an `AppState` list.
        Overlay::AddModelPick {
            ref query,
            ref mut selected,
            ref models,
            ..
        } => {
            let indices = filter_model_names(models, query);
            step(selected, indices.len(), delta);
            return;
        }
        // The Unsloth repo/quant browsers: the same filterable-list shape as
        // the add-model pick-list, over the overlay's own list field. Safe
        // while `loading` (an empty list steps to 0 and stays there).
        Overlay::UnslothRepos {
            ref query,
            ref mut selected,
            ref repos,
            ..
        } => {
            let indices = filter_unsloth_repos(repos, query);
            step(selected, indices.len(), delta);
            return;
        }
        Overlay::UnslothQuants {
            ref query,
            ref mut selected,
            ref quants,
            ..
        } => {
            let indices = filter_unsloth_quants(quants, query);
            step(selected, indices.len(), delta);
            return;
        }
        _ => {}
    }
    // Base view: a pending approval owns the arrows (move between stacked
    // approvals). Otherwise the composer is active and the input layer routes
    // arrows to scroll / run-switch, so this legacy pane path is inert.
    if state.show_approval_modal() {
        let previous = state.selected_approval;
        step(
            &mut state.selected_approval,
            state.pending_approvals.len(),
            delta,
        );
        // A different approval owns the modal body — start it at the top.
        if state.selected_approval != previous {
            state.approval_scroll = 0;
        }
        return;
    }
    match state.focus {
        Pane::Sessions => step(&mut state.selected_run, state.runs.len(), delta),
        Pane::Approvals => {
            let previous = state.selected_approval;
            step(
                &mut state.selected_approval,
                state.pending_approvals.len(),
                delta,
            );
            if state.selected_approval != previous {
                state.approval_scroll = 0;
            }
        }
        Pane::Transcript => {
            let idx = state.selected_run;
            if let Some(run) = state.runs.get_mut(idx) {
                step(&mut run.transcript_selected, run.transcript.len(), delta);
                run.scroll = u32::try_from(run.transcript_selected).unwrap_or(u32::MAX);
            }
        }
    }
}

/// Jump a filterable picker to its first/last result. Kept separate from
/// transcript paging so `Home`/`End` remain model-list navigation while a
/// palette-mode overlay owns input.
fn nav_to_edge(state: &mut AppState, last: bool) {
    let edge = |len: usize| if last { len.saturating_sub(1) } else { 0 };
    match &mut state.overlay {
        Overlay::Onboard { step } => match step {
            OnboardStep::Triage { selected } | OnboardStep::SkipConfirm { selected } => {
                *selected = edge(3);
            }
            OnboardStep::Validating { .. } => {}
        },
        Overlay::OnboardProviderPicker {
            class,
            query,
            selected,
        } => {
            let indices = filter_onboard_providers(&state.providers, *class, query);
            *selected = edge(indices.len());
            state.selected_provider = indices.get(*selected).copied().unwrap_or(0);
        }
        Overlay::Palette { query, selected } => {
            *selected = edge(crate::palette::filtered_len(query));
        }
        Overlay::ModelPicker { query, selected } => {
            let indices = filter_models(&state.models, query);
            *selected = edge(indices.len());
            state.selected_model = indices.get(*selected).copied().unwrap_or(0);
        }
        Overlay::ProviderPicker { query, selected } => {
            let indices = filter_providers(&state.providers, query);
            *selected = edge(indices.len());
            state.selected_provider = indices.get(*selected).copied().unwrap_or(0);
        }
        Overlay::ModePicker { query, selected } => {
            *selected = edge(filter_modes(query).len());
        }
        Overlay::ThemePicker { query, selected } => {
            *selected = edge(filter_themes(&state.themes, query).len());
        }
        Overlay::ApiKeys { query, selected } => {
            *selected = edge(filter_key_rows(&state.models, &state.voice_key_rows, query).len());
        }
        Overlay::DocPublishTarget { selected, .. } => {
            *selected = edge(DOC_PUBLISH_TARGETS.len());
        }
        Overlay::AddModelPick {
            models,
            query,
            selected,
            ..
        } => {
            *selected = edge(filter_model_names(models, query).len());
        }
        Overlay::UnslothRepos {
            repos,
            query,
            selected,
            ..
        } => {
            *selected = edge(filter_unsloth_repos(repos, query).len());
        }
        Overlay::UnslothQuants {
            quants,
            query,
            selected,
            ..
        } => {
            *selected = edge(filter_unsloth_quants(quants, query).len());
        }
        Overlay::CouncilBuilder(builder) => {
            let len = match builder.step {
                CouncilBuilderStep::MemberModel => {
                    let continue_row =
                        usize::from(builder.members.len() >= 2 && builder.query.trim().is_empty());
                    let remove_row =
                        usize::from(!builder.members.is_empty() && builder.query.trim().is_empty());
                    let available = if builder.members.len() >= 8 {
                        0
                    } else {
                        filter_council_member_models(
                            &state.models,
                            &builder.query,
                            &builder.members,
                        )
                        .len()
                    };
                    continue_row + available + remove_row
                }
                CouncilBuilderStep::Chair => filter_models(&state.models, &builder.query).len(),
                CouncilBuilderStep::Rounds => 3,
                _ => 0,
            };
            builder.selected = edge(len);
            if builder.step == CouncilBuilderStep::Rounds {
                builder.rounds = u8::try_from(builder.selected + 1).unwrap_or(3).clamp(1, 3);
            }
        }
        _ => {}
    }
}

/// `PgUp`/`PgDn`: a viewport-sized-ish jump.
const PAGE: u16 = 10;

/// One wheel notch. Conventional terminals scroll ~3 lines per notch; mapping
/// the wheel to a 10-row page made the conversation lurch.
const WHEEL_LINES: u16 = 3;

fn scroll_lines(state: &mut AppState, up: bool) {
    if matches!(state.overlay, Overlay::Help) {
        state.help_scroll = if up {
            state.help_scroll.saturating_sub(WHEEL_LINES)
        } else {
            state
                .help_scroll
                .saturating_add(WHEEL_LINES)
                .min(state.help_max_scroll.get())
        };
        return;
    }
    if matches!(state.overlay, Overlay::CouncilResults) {
        state.council_result_scroll = if up {
            state.council_result_scroll.saturating_sub(WHEEL_LINES)
        } else {
            state.council_result_scroll.saturating_add(WHEEL_LINES)
        };
    } else {
        scroll_transcript(state, up, WHEEL_LINES);
    }
}

fn scroll_page(state: &mut AppState, up: bool) {
    // The approval modal's body outgrows its card by design (every env
    // binding, every path — verbatim), so while it owns the screen PgUp/PgDn
    // pages that body instead of the transcript or the approval stack.
    if state.show_approval_modal() {
        state.approval_scroll = if up {
            state.approval_scroll.saturating_sub(PAGE)
        } else {
            state
                .approval_scroll
                .saturating_add(PAGE)
                .min(state.approval_max_scroll.get())
        };
        return;
    }
    if matches!(state.overlay, Overlay::Help) {
        state.help_scroll = if up {
            state.help_scroll.saturating_sub(PAGE)
        } else {
            state
                .help_scroll
                .saturating_add(PAGE)
                .min(state.help_max_scroll.get())
        };
        return;
    }
    if matches!(state.overlay, Overlay::CouncilResults) {
        state.council_result_scroll = if up {
            state.council_result_scroll.saturating_sub(PAGE)
        } else {
            state.council_result_scroll.saturating_add(PAGE)
        };
        return;
    }
    // A workspace side pane owns page navigation just as it owns ↑/↓. Those
    // panes are selection-backed rather than pixel-scroll-backed; jumping the
    // selection makes the renderer bring the landing row into view. Chat mode
    // deliberately ignores a retained side focus and keeps paging transcript.
    if matches!(state.overlay, Overlay::None)
        && state.layout == crate::state::LayoutMode::Workspace
        && state.focus != Pane::Transcript
    {
        nav(
            state,
            if up {
                -i32::from(PAGE)
            } else {
                i32::from(PAGE)
            },
        );
        return;
    }
    scroll_transcript(state, up, PAGE);
}

fn scroll_transcript(state: &mut AppState, up: bool, rows: u16) {
    if matches!(state.overlay, Overlay::Edges) {
        let page = if up {
            state.edge_page.saturating_sub(1)
        } else if (state.edge_page + 1) * EDGE_PAGE_SIZE < state.edge_total {
            state.edge_page + 1
        } else {
            state.edge_page
        };
        request_edge_page(state, page);
        return;
    }
    // Scrolling means the user is driving the viewport, not the fold cursor.
    end_browse(state);
    // The renderer cached the true bottom last frame; use it so leaving follow
    // mode starts a page up from the bottom (not a jump to the top), and paging
    // back to the bottom re-enters follow.
    let max = state.transcript_max_scroll.get();
    let idx = state.selected_run;
    if let Some(run) = state.runs.get_mut(idx) {
        if up {
            if run.follow {
                run.follow = false;
                run.scroll = max;
            }
            run.scroll = run.scroll.saturating_sub(u32::from(rows));
        } else {
            run.scroll = run.scroll.saturating_add(u32::from(rows)).min(max);
            if run.scroll >= max {
                run.follow = true;
            }
        }
    }
}

fn request_edge_page(state: &mut AppState, page: usize) {
    state.edge_loading = true;
    state.outbox.push(Intent::SearchEdges {
        query: state.edge_query.clone(),
        page,
    });
}

/// Every foldable transcript entry in the whole session, as `(run, entry)`
/// addresses in the order the conversation stacks them.
///
/// The walk deliberately spans runs: `render_conversation` draws EVERY run in
/// one continuous timeline and each follow-up message opens a new run, so a
/// cursor confined to `selected_run` left every tool card and patch diff from
/// an earlier turn visible-but-inert. The mouse's click targets are exactly
/// this set (`fold_hit_entry` shares `TranscriptEntry::is_foldable`), so
/// keyboard and mouse reach the same cards (RULE 3).
fn session_folds(state: &AppState) -> Vec<(usize, usize)> {
    state
        .runs
        .iter()
        .enumerate()
        .flat_map(|(run_idx, run)| {
            run.transcript
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.is_foldable())
                .map(move |(idx, _)| (run_idx, idx))
        })
        .collect()
}

/// Point the fold cursor at one `(run, entry)` address.
fn set_fold_cursor(state: &mut AppState, (run_idx, entry): (usize, usize)) {
    state.transcript_focus_run = run_idx;
    if let Some(run) = state.runs.get_mut(run_idx) {
        run.transcript_selected = entry;
    }
}

/// The cursor's current address, or `None` when the session has no run.
fn fold_cursor(state: &AppState) -> Option<(usize, usize)> {
    let run_idx = state.fold_focus_run();
    state
        .runs
        .get(run_idx)
        .map(|run| (run_idx, run.transcript_selected))
}

/// `Alt-↑`/`Alt-↓`: walk the session's *foldable* entries — tool cards, patch
/// diffs, the backstage fold, long notes, failed-run errors — across every run,
/// and mark the transcript as being browsed, so the renderer highlights the
/// landing entry, keeps it in the viewport, and `Alt-Enter` expands it.
/// Stepping only over foldable entries means every stop has something to open.
/// A no-op when the session has no foldable entry at all.
fn browse_fold(state: &mut AppState, delta: i32) {
    if !matches!(state.overlay, Overlay::None) {
        return;
    }
    let folds = session_folds(state);
    let Some(&last) = folds.last() else {
        return;
    };
    // The first Alt-↑/Alt-↓ enters browse mode at the newest fold in the whole
    // conversation (the one the tail is showing); later presses walk from there.
    if !state.transcript_browse {
        state.transcript_browse = true;
        set_fold_cursor(state, last);
        return;
    }
    let position = fold_cursor(state)
        .and_then(|current| folds.iter().position(|&fold| fold == current))
        .unwrap_or(folds.len() - 1);
    let next = if delta < 0 {
        position.saturating_sub(1)
    } else {
        (position + 1).min(folds.len() - 1)
    };
    set_fold_cursor(state, folds[next]);
}

/// Leave transcript-browse mode: the selection stops being highlighted and
/// `Alt-Enter` goes back to inserting a line break. Called by every gesture
/// that means "I am driving the composer or the viewport again".
fn end_browse(state: &mut AppState) {
    state.transcript_browse = false;
    // Idle, the fold cursor belongs to the run the composer talks to; the next
    // `Alt-↑` re-enters at the session's newest fold anyway.
    state.transcript_focus_run = state.selected_run;
}

fn expand_selected(state: &mut AppState) {
    if let Overlay::Backtrack(ref bt) = state.overlay {
        let forkable = state.forkable_runs();
        if let Some(target_run) = forkable.get(bt.selected) {
            if let Some(cp_id) = target_run.launch_checkpoint {
                state.composer = target_run.objective.clone();
                state.composer_cursor = state.composer.len();
                state.overlay = Overlay::None;
                state.backtrack_primed = false;
                state.outbox.push(Intent::ForkSession {
                    checkpoint: cp_id,
                    prompt: state.composer.clone(),
                });
            }
        }
        return;
    }
    if matches!(state.overlay, Overlay::CouncilResults) {
        state.council_result_expanded = !state.council_result_expanded;
        state.council_result_scroll = 0;
        return;
    }
    if matches!(state.overlay, Overlay::CouncilBrowser) {
        if let Some(name) = state.focused_council().map(|council| council.name.clone()) {
            state.outbox.push(Intent::LoadCouncilResults {
                selector: Some(name),
            });
        }
        return;
    }
    // In the memory browser, `Enter` opens the focused memory's source.
    if matches!(state.overlay, Overlay::Memory { .. }) {
        open_source(state);
        return;
    }
    // The transcript fold is reachable from the base conversation — by click
    // (`ActivateRow`) or by `Alt-Enter` while browsing — and from the
    // workspace transcript pane. An open browser overlay owns `Enter` for its
    // own list, so it must not silently toggle a fold behind the modal.
    if !matches!(state.overlay, Overlay::None) {
        return;
    }
    // In Workspace a side pane genuinely owns Enter. Chat retains the old
    // conversation-centric behavior even if a stale/remembered pane value is
    // not Transcript.
    if state.layout == crate::state::LayoutMode::Workspace && state.focus != Pane::Transcript {
        return;
    }
    let idx = state.fold_focus_run();
    if let Some(run) = state.runs.get_mut(idx) {
        if let Some(entry) = run.transcript.get_mut(run.transcript_selected) {
            match entry {
                TranscriptEntry::Tool(card) => card.expanded = !card.expanded,
                TranscriptEntry::Patch(patch) => patch.expanded = !patch.expanded,
                TranscriptEntry::Note { expanded, .. } => *expanded = !*expanded,
                TranscriptEntry::Backstage { expanded, .. } => *expanded = !*expanded,
                TranscriptEntry::Completed { expanded, .. } => *expanded = !*expanded,
                _ => {}
            }
        }
    }
}

fn focused_transcript_entry(state: &AppState) -> Option<(&RunView, &TranscriptEntry)> {
    let run = state.fold_focus()?;
    run.transcript
        .get(run.transcript_selected)
        .map(|entry| (run, entry))
}

/// Exact safe text for clipboard export. This deliberately does not include
/// argument values or raw failure chains: cards expose digests and sanitized
/// provider causes, matching the visual/cooked projections.
fn focused_card_copy_text(state: &AppState) -> Option<String> {
    let (run, entry) = focused_transcript_entry(state)?;
    match entry {
        TranscriptEntry::Tool(card) => {
            let mut fields = vec![format!("tool: {}", card.tool)];
            if let Some(label) = &card.label {
                fields.push(format!("target: {label}"));
            }
            if let Some(digest) = &card.args_digest {
                fields.push(format!("args digest: {digest}"));
            }
            if let Some(ToolOutcome::Failed { message }) = &card.outcome {
                fields.push(format!(
                    "failure: {}",
                    crate::state::sanitize_failure_text(message)
                ));
            }
            Some(fields.join("\n"))
        }
        TranscriptEntry::Patch(patch) => {
            Some(format!("{}\n\n{}", patch.files.join("\n"), patch.preview))
        }
        TranscriptEntry::Note { text, .. } => Some(text.clone()),
        TranscriptEntry::Backstage { raw, .. } => Some(raw.join("\n")),
        TranscriptEntry::Completed {
            disposition: RunDisposition::Failed { reason },
            ..
        } => crate::state::acp_failure_summary(run.model.as_ref(), reason).map_or_else(
            || Some(crate::state::sanitize_failure_text(reason)),
            |failure| {
                Some(format!(
                    "ACP failure\nprovider: {}\nmodel: {}\nphase: {}\ncause: {}",
                    failure.provider, failure.model, failure.phase, failure.cause
                ))
            },
        ),
        _ => None,
    }
}

fn copy_focused_card(state: &mut AppState) {
    if matches!(state.overlay, Overlay::CouncilResults) {
        let Some((text, result_id)) = state
            .focused_council_result()
            .map(|result| (result.synthesis.clone(), result.result_id.clone()))
        else {
            state.notice = Some(("no council result selected".to_owned(), state.tick + 25));
            return;
        };
        state.outbox.push(Intent::CopyText { text });
        state.notice = Some((
            format!("copied chair synthesis · result {result_id}"),
            state.tick + 25,
        ));
        return;
    }
    if !state.transcript_browse || !matches!(state.overlay, Overlay::None) {
        state.notice = Some((
            "browse a card with Alt-↑/↓ before copying".to_owned(),
            state.tick + 25,
        ));
        return;
    }
    let Some(text) = focused_card_copy_text(state) else {
        state.notice = Some((
            "this transcript row has no card text to copy".to_owned(),
            state.tick + 25,
        ));
        return;
    };
    state.outbox.push(Intent::CopyText { text });
    state.notice = Some(("copied focused card".to_owned(), state.tick + 25));
}

/// Hand a retained start-draft back to the operator: into the composer when
/// it is free, otherwise into its own New Run prompt so a newer composer
/// draft is never clobbered. Shared by the rejection and timeout paths — the
/// original remains editable either way.
fn restore_pending_run_draft(state: &mut AppState, pending: PendingRunStart) {
    match pending.target {
        RunStartDraftTarget::Composer if state.composer.is_empty() => {
            state.composer = pending.draft;
            state.composer_cursor = state.composer.len();
        }
        // Preserve a newer composer draft by restoring the objective in its
        // own prompt. The original remains editable and the newer draft
        // remains untouched beneath it.
        RunStartDraftTarget::Composer | RunStartDraftTarget::NewRunPrompt => {
            state.overlay = Overlay::NewRun(pending.draft);
        }
    }
}

fn failed_run_context(state: &AppState) -> Option<(String, AgentMode, Option<ModelId>, usize)> {
    let run = state.fold_focus()?;
    let entry = run.transcript.get(run.transcript_selected)?;
    matches!(
        entry,
        TranscriptEntry::Completed {
            disposition: RunDisposition::Failed { .. },
            ..
        }
    )
    .then(|| {
        (
            run.objective.clone(),
            run.mode,
            run.model.clone(),
            state.fold_focus_run(),
        )
    })
}

fn retry_failed_run(state: &mut AppState) {
    let Some((objective, mode, model, _)) = failed_run_context(state) else {
        return;
    };
    if objective.trim().is_empty() || state.pending_run_start.is_some() {
        state.notice = Some((
            "cannot retry while another run is starting".to_owned(),
            state.tick + 25,
        ));
        return;
    }
    state.pending_model = model.clone().or_else(|| state.pending_model.clone());
    state.outbox.push(Intent::StartRun {
        objective: objective.clone(),
        mode,
        model,
    });
    state.pending_run_start = Some(PendingRunStart {
        draft: objective,
        target: RunStartDraftTarget::NewRunPrompt,
        started_tick: state.tick,
    });
    state.notice = Some((
        "retrying failed run with the same model".to_owned(),
        state.tick + 40,
    ));
    end_browse(state);
}

fn failed_model_id(state: &AppState) -> Option<String> {
    let (_, _, model, _) = failed_run_context(state)?;
    model.map(|model| model.0)
}

fn reauthenticate_failed_model(state: &mut AppState) {
    let Some(model_id) = failed_model_id(state) else {
        return;
    };
    if model_id.starts_with("acp/") {
        let supplier = model_id
            .strip_prefix("acp/")
            .and_then(|coordinate| coordinate.split('#').next())
            .unwrap_or("ACP agent");
        state.notice = Some((
            format!("sign in with `{supplier}` in a terminal, then use Alt-R retry"),
            state.tick + 60,
        ));
    } else {
        state.overlay = Overlay::ApiKeySet {
            target: KeyTarget::Model(model_id),
            buffer: SecretKey(String::new()),
        };
        end_browse(state);
    }
}

fn open_failure_model_picker(state: &mut AppState) {
    if failed_run_context(state).is_some() {
        state.overlay = Overlay::ModelPicker {
            query: String::new(),
            selected: 0,
        };
        state.selected_model = 0;
        end_browse(state);
    }
}

fn disable_failed_model(state: &mut AppState) {
    let Some(model_id) = failed_model_id(state) else {
        return;
    };
    let Some(index) = state.models.iter().position(|card| card.id.0 == model_id) else {
        state.notice = Some((
            "this failed model is not a user-configured profile".to_owned(),
            state.tick + 30,
        ));
        return;
    };
    if state
        .pending_model
        .as_ref()
        .is_some_and(|pending| pending.0 == model_id)
    {
        state.pending_model = None;
    }
    state.selected_model = index;
    state.overlay = Overlay::ModelPicker {
        query: model_id,
        selected: 0,
    };
    begin_remove_selected(state);
    end_browse(state);
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

fn start_focused_workflow(state: &mut AppState) {
    let Some(workflow_id) = state.focused_node().map(|card| card.workflow_id.clone()) else {
        state.overlay = Overlay::None;
        state.composer = "Create an executable workflow manifest at .codypendent/workflows/example.yaml with inspect, implement, and verify nodes. Then show me how to run it.".to_owned();
        state.composer_cursor = state.composer.len();
        state.notice = Some((
            "example workflow request drafted — review it, then press Enter".to_owned(),
            state.tick + 40,
        ));
        return;
    };
    state.overlay = Overlay::WorkflowInputs {
        workflow_id,
        buffer: String::new(),
    };
}

fn begin_blackboard_post(state: &mut AppState) {
    let workflow_run_id = state
        .focused_item()
        .map(|item| item.workflow_run_id.clone())
        .or_else(|| {
            state
                .workflow
                .iter()
                .find_map(|node| node.workflow_run_id.clone())
        });
    if let Some(workflow_run_id) = workflow_run_id {
        state.overlay = Overlay::BlackboardPost {
            workflow_run_id,
            buffer: String::new(),
        };
    } else {
        state.overlay = Overlay::Workflow;
        state.notice = Some((
            "start a persisted workflow first; its evidence stream will open here".to_owned(),
            state.tick + 40,
        ));
    }
}

fn pause_or_resume_workflow(state: &mut AppState) {
    let Some(card) = state.focused_node() else {
        return;
    };
    let Some(workflow_run_id) = card.workflow_run_id.clone() else {
        state.notice = Some(("press n to start this workflow".to_owned(), state.tick + 25));
        return;
    };
    let intent = match card.run_phase.as_str() {
        "paused" => Some(Intent::ResumeWorkflow { workflow_run_id }),
        "pending" | "running" => Some(Intent::PauseWorkflow { workflow_run_id }),
        _ => None,
    };
    if let Some(intent) = intent {
        state.outbox.push(intent);
    } else {
        state.notice = Some((
            format!("workflow is {} — start a new run with n", card.run_phase),
            state.tick + 30,
        ));
    }
}

fn retry_focused_workflow_node(state: &mut AppState) {
    let Some(card) = state.focused_node() else {
        return;
    };
    let Some(workflow_run_id) = card.workflow_run_id.clone() else {
        state.notice = Some(("press n to start this workflow".to_owned(), state.tick + 25));
        return;
    };
    let node_id = card.id.clone();
    state.outbox.push(Intent::RetryWorkflowNode {
        workflow_run_id,
        node_id: node_id.clone(),
    });
    state.notice = Some((format!("retrying from node {node_id}…"), state.tick + 30));
}

fn request_workflow_cancel(state: &mut AppState) {
    let Some(card) = state.focused_node() else {
        return;
    };
    let Some(workflow_run_id) = card.workflow_run_id.clone() else {
        return;
    };
    if matches!(card.run_phase.as_str(), "pending" | "running" | "paused") {
        state.overlay = Overlay::ConfirmWorkflowCancel { workflow_run_id };
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

/// `y`/`Enter` on a confirm-style overlay (the shared `InputMode::Confirm` key
/// table maps both to [`Action::ConfirmCancel`]). Dispatches by which confirm
/// is open, including client-only model/key removal. A no-op when no confirm is
/// open.
fn confirm_top(state: &mut AppState) {
    match &state.overlay {
        Overlay::ConfirmCancel => confirm_cancel(state),
        Overlay::ConfirmWorkflowCancel { .. } => {
            if let Overlay::ConfirmWorkflowCancel { workflow_run_id } =
                std::mem::take(&mut state.overlay)
            {
                state
                    .outbox
                    .push(Intent::CancelWorkflow { workflow_run_id });
            }
        }
        Overlay::ApiKeyRemoveConfirm { .. } => {
            if let Overlay::ApiKeyRemoveConfirm { target } = std::mem::take(&mut state.overlay) {
                state.outbox.push(Intent::RemoveApiKey { target });
            }
        }
        Overlay::ConfirmUiPluginApprove { .. } => {
            if let Overlay::ConfirmUiPluginApprove {
                plugin_id,
                receipt,
                permission_diff: _,
            } = std::mem::take(&mut state.overlay)
            {
                state
                    .outbox
                    .push(Intent::ApproveUiPluginUpdate { plugin_id, receipt });
                state.overlay = Overlay::UiPlugins;
            }
        }
        Overlay::ConfirmUiPluginEnable { .. } => {
            if let Overlay::ConfirmUiPluginEnable {
                plugin_id,
                scope,
                permission_summary: _,
            } = std::mem::take(&mut state.overlay)
            {
                state.outbox.push(Intent::EnableUiPlugin {
                    plugin_id,
                    scope: scope.clone(),
                });
                state.overlay = Overlay::UiPlugins;
                state.notice = Some((format!("enabling plugin for {scope}…"), state.tick + 40));
            }
        }
        Overlay::ConfirmUiPluginReject { .. } => {
            if let Overlay::ConfirmUiPluginReject { plugin_id, receipt } =
                std::mem::take(&mut state.overlay)
            {
                state
                    .outbox
                    .push(Intent::RejectUiPluginUpdate { plugin_id, receipt });
                state.overlay = Overlay::UiPlugins;
            }
        }
        Overlay::ConfirmUiPluginRevoke { .. } => {
            if let Overlay::ConfirmUiPluginRevoke { plugin_id } = std::mem::take(&mut state.overlay)
            {
                state.outbox.push(Intent::RevokeUiPlugin { plugin_id });
                state.overlay = Overlay::UiPlugins;
            }
        }
        Overlay::ConfirmCouncilDelete { .. } => {
            if let Overlay::ConfirmCouncilDelete { name } = std::mem::take(&mut state.overlay) {
                state.outbox.push(Intent::DeleteCouncil { name });
                state.overlay = Overlay::CouncilBrowser;
            }
        }
        Overlay::ConfirmLearningDelete { .. } => {
            if let Overlay::ConfirmLearningDelete { id, revision, .. } =
                std::mem::take(&mut state.overlay)
            {
                state.outbox.push(Intent::MutateLearning {
                    id,
                    revision,
                    mutation: LearningMutation::Delete,
                });
                state.overlay = Overlay::Journey;
            }
        }
        Overlay::ConfirmSessionDelete { .. } => {
            if let Overlay::ConfirmSessionDelete {
                session_id,
                selected,
                ..
            } = std::mem::take(&mut state.overlay)
            {
                state.outbox.push(Intent::MutateSession {
                    session_id,
                    // The daemon is the retention authority: the client asks
                    // for its policy, never for a weaker one.
                    action: codypendent_protocol::SessionLifecycleAction::Delete {
                        mode: codypendent_protocol::SessionDeletionMode::RetentionPolicy,
                    },
                });
                return_to_session_library(state, selected);
            }
        }
        Overlay::ConfirmModelRemove { .. } => {
            if let Overlay::ConfirmModelRemove {
                model_id,
                query,
                selected,
                ..
            } = std::mem::take(&mut state.overlay)
            {
                // Re-check after the prompt: another client can stage this
                // model or start a run while the confirmation is visible.
                if let Some(reason) = state.model_removal_blocker(&model_id) {
                    state.notice = Some((
                        format!("cannot remove {model_id}: {reason}"),
                        state.tick + 40,
                    ));
                } else {
                    state.outbox.push(Intent::RemoveModel { model_id });
                }
                state.overlay = Overlay::ModelPicker { query, selected };
            }
        }
        Overlay::ConfirmCommunityAcpInstall { .. } => {
            if let Overlay::ConfirmCommunityAcpInstall { provider_id, .. } =
                std::mem::take(&mut state.overlay)
            {
                state.outbox.push(Intent::QueryProviderModels {
                    provider_id: provider_id.clone(),
                    api_key: None,
                    refresh: false,
                });
                state.overlay = Overlay::AddModelQuerying {
                    provider_id: provider_id.clone(),
                    api_key: None,
                };
                state.notice = Some((
                    format!("installing and testing pinned {provider_id} v1.0.0…"),
                    state.tick + 60,
                ));
            }
        }
        // Confirmed: drive the pull. The overlay moves to the live-progress
        // step; a late `Action::UnslothPullProgress`/`PullFinished` for a
        // repo/quant this operator backed out of before confirming can never
        // arrive (nothing was ever sent), unlike a dismiss of the *next*
        // step's overlay (see `Overlay::UnslothPulling`'s doc comment).
        Overlay::UnslothConfirmPull { .. } => {
            if let Overlay::UnslothConfirmPull { repo_id, quant, .. } =
                std::mem::take(&mut state.overlay)
            {
                state.outbox.push(Intent::PullUnslothModel {
                    repo_id: repo_id.clone(),
                    quant: quant.clone(),
                });
                state.overlay = Overlay::UnslothPulling {
                    repo_id,
                    quant,
                    lines: Vec::new(),
                    done: false,
                    error: None,
                    registered_id: None,
                };
            }
        }
        // Confirmed block deletion. Structural, so it takes the whole-document
        // lease (`block_id: None`) exactly as an insert does.
        Overlay::DocDeleteConfirm { .. } => {
            if let Overlay::DocDeleteConfirm { block_id, .. } = std::mem::take(&mut state.overlay) {
                state.overlay = Overlay::Docs;
                if let Some(document_id) = state.focused_doc().map(|doc| doc.document_id) {
                    start_doc_edit(
                        state,
                        document_id,
                        None,
                        DocumentMutation::Delete { block_id },
                    );
                }
            }
        }
        _ => {}
    }
}

fn open_ui_plugins(state: &mut AppState) {
    state.overlay = Overlay::UiPlugins;
    state.outbox.push(Intent::ListUiPlugins);
}

fn smoke_test_ui_plugin(state: &mut AppState) {
    if !matches!(state.overlay, Overlay::UiPlugins) {
        return;
    }
    if let Some(plugin_id) = state.focused_ui_plugin().map(|p| p.id.clone()) {
        state.outbox.push(Intent::SmokeTestUiPlugin { plugin_id });
        state.notice = Some(("smoke-testing plugin…".to_owned(), state.tick + 40));
    }
}

fn enable_ui_plugin(state: &mut AppState, scope: &str) {
    if !matches!(state.overlay, Overlay::UiPlugins) {
        return;
    }
    if let Some((plugin_id, permission_summary)) = state.focused_ui_plugin().map(|plugin| {
        (
            plugin.id.clone(),
            plugin
                .update_permission_diff
                .clone()
                .unwrap_or_else(|| {
                    "No pending permission expansion. Enabling grants the permissions declared by the verified installed package."
                        .to_owned()
                }),
        )
    }) {
        state.overlay = Overlay::ConfirmUiPluginEnable {
            plugin_id,
            scope: scope.to_owned(),
            permission_summary,
        };
    }
}

fn begin_approve_ui_plugin(state: &mut AppState) {
    let Some((plugin_id, receipt, permission_diff)) =
        state.focused_ui_plugin().and_then(|plugin| {
            plugin.update_approval_receipt.as_ref().map(|receipt| {
                (
                    plugin.id.clone(),
                    receipt.clone(),
                    plugin
                        .update_permission_diff
                        .clone()
                        .unwrap_or_else(|| "No permission changes reported.".to_owned()),
                )
            })
        })
    else {
        state.notice = Some((
            "selected plugin has no pending update".to_owned(),
            state.tick + 25,
        ));
        return;
    };
    state.overlay = Overlay::ConfirmUiPluginApprove {
        plugin_id,
        receipt,
        permission_diff,
    };
}

fn begin_reject_ui_plugin(state: &mut AppState) {
    let Some((plugin_id, receipt)) = state.focused_ui_plugin().and_then(|plugin| {
        plugin
            .update_approval_receipt
            .as_ref()
            .map(|receipt| (plugin.id.clone(), receipt.clone()))
    }) else {
        state.notice = Some((
            "selected plugin has no pending update".to_owned(),
            state.tick + 25,
        ));
        return;
    };
    state.overlay = Overlay::ConfirmUiPluginReject { plugin_id, receipt };
}

fn begin_revoke_ui_plugin(state: &mut AppState) {
    if let Some(plugin_id) = state.focused_ui_plugin().map(|p| p.id.clone()) {
        state.overlay = Overlay::ConfirmUiPluginRevoke { plugin_id };
    }
}

/// `r` in the council browser (rubric 6 TUI wiring): open the objective prompt
/// for the focused council. A no-op with an empty browser.
fn begin_run_council(state: &mut AppState) {
    if !matches!(state.overlay, Overlay::CouncilBrowser) {
        return;
    }
    if let Some(name) = state.focused_council().map(|council| council.name.clone()) {
        state.overlay = Overlay::CouncilRunObjective {
            name,
            buffer: String::new(),
        };
    }
}

/// `d` in the council browser (rubric 6 TUI wiring): open the removal confirm
/// for the focused council. A no-op with an empty browser.
fn begin_delete_council(state: &mut AppState) {
    if !matches!(state.overlay, Overlay::CouncilBrowser) {
        return;
    }
    if let Some(name) = state.focused_council().map(|council| council.name.clone()) {
        state.overlay = Overlay::ConfirmCouncilDelete { name };
    }
}

fn begin_steering(state: &mut AppState) {
    if state.selected_run().is_some() {
        state.overlay = Overlay::Steering(String::new());
    }
}

fn resolve_focused(state: &mut AppState, decision: ApprovalDecision, scope: ApprovalScope) {
    // The host-owned approval card is always drawn above ordinary overlays,
    // and input_mode gives it exclusive decision keys until it is resolved.
    if let Some(pending) = state.focused_approval() {
        if matches!(scope, ApprovalScope::Pattern | ApprovalScope::Repository)
            && pending.pattern.is_none()
        {
            return;
        }
        let approval_id = pending.approval_id;
        state.outbox.push(Intent::ResolveApproval {
            approval_id,
            decision,
            scope,
        });
    }
}

fn resolve_focused_question(state: &mut AppState, outcome: QuestionOutcome) {
    if let Some(pending) = state.pending_questions.first() {
        let question_id = pending.question_id;
        state.outbox.push(Intent::ResolveQuestion {
            question_id,
            outcome,
        });
    }
}

fn question_navigate(state: &mut AppState, delta: isize) {
    let Some(pending) = state.pending_questions.first() else {
        return;
    };
    let questions = pending.questions.clone();
    let Some(card) = &mut state.question_card_state else {
        return;
    };
    if card.feedback.is_some() || card.editing_custom {
        return;
    }
    let Some(prompt) = questions.get(card.index) else {
        return;
    };
    let total_rows = prompt.options.len() + 1;
    if delta > 0 {
        card.selected = (card.selected + 1).min(total_rows.saturating_sub(1));
    } else if delta < 0 {
        card.selected = card.selected.saturating_sub(1);
    }
}

fn question_pick_digit(state: &mut AppState, digit: usize) {
    let Some(pending) = state.pending_questions.first() else {
        return;
    };
    let questions = pending.questions.clone();
    let Some(card) = &mut state.question_card_state else {
        return;
    };
    if card.feedback.is_some() || card.editing_custom {
        let c = char::from_digit(digit as u32, 10).unwrap_or(' ');
        if let Some(feedback) = &mut card.feedback {
            feedback.push(c);
        } else if card.editing_custom {
            if let Some(custom) = card.custom_text.get_mut(card.index) {
                custom.push(c);
            }
        }
        return;
    }
    let Some(prompt) = questions.get(card.index) else {
        return;
    };
    if digit >= 1 && digit <= prompt.options.len() {
        let opt_idx = digit - 1;
        card.selected = opt_idx;
        if prompt.multiple {
            let label = prompt.options[opt_idx].label.clone();
            if let Some(picked) = card.picked.get_mut(card.index) {
                if let Some(pos) = picked.iter().position(|x| x == &label) {
                    picked.remove(pos);
                } else {
                    picked.push(label);
                }
            }
        } else {
            let label = prompt.options[opt_idx].label.clone();
            if let Some(picked) = card.picked.get_mut(card.index) {
                *picked = vec![label];
            }
            if questions.len() == 1 {
                let answers = card.picked.clone();
                resolve_focused_question(state, QuestionOutcome::Answered { answers });
            } else if card.index + 1 < questions.len() {
                card.index += 1;
                card.selected = 0;
            } else {
                let answers = card.picked.clone();
                resolve_focused_question(state, QuestionOutcome::Answered { answers });
            }
        }
    }
}

fn question_toggle_option(state: &mut AppState) {
    let Some(pending) = state.pending_questions.first() else {
        return;
    };
    let questions = pending.questions.clone();
    let Some(card) = &mut state.question_card_state else {
        return;
    };
    if let Some(feedback) = &mut card.feedback {
        feedback.push(' ');
        return;
    }
    if card.editing_custom {
        if let Some(custom) = card.custom_text.get_mut(card.index) {
            custom.push(' ');
        }
        return;
    }
    let Some(prompt) = questions.get(card.index) else {
        return;
    };
    if card.selected < prompt.options.len() {
        let label = prompt.options[card.selected].label.clone();
        if let Some(picked) = card.picked.get_mut(card.index) {
            if prompt.multiple {
                if let Some(pos) = picked.iter().position(|x| x == &label) {
                    picked.remove(pos);
                } else {
                    picked.push(label);
                }
            } else {
                *picked = vec![label];
            }
        }
    }
}

fn question_select_or_confirm(state: &mut AppState) {
    let Some(pending) = state.pending_questions.first() else {
        return;
    };
    let questions = pending.questions.clone();
    let Some(card) = &mut state.question_card_state else {
        return;
    };
    if card.feedback.is_some() {
        let feedback = card.feedback.take();
        resolve_focused_question(state, QuestionOutcome::Rejected { feedback });
        return;
    }
    let Some(prompt) = questions.get(card.index) else {
        return;
    };

    let is_custom_row = card.selected == prompt.options.len();
    if is_custom_row {
        if card.editing_custom {
            // `.get()`, not `[]`. The card and its question are separate pieces
            // of state kept in step by the reducer, and this was the one place
            // a disagreement between them took the whole TUI down — while a
            // question was blocking the operator, on an event the daemon
            // controls. Resizing on redefinition (see `QuestionAsked`) is the
            // fix for the known cause; this is so the next cause cannot crash.
            let Some(text) = card
                .custom_text
                .get(card.index)
                .map(|entry| entry.trim().to_string())
            else {
                return;
            };
            if !text.is_empty() {
                if let Some(picked) = card.picked.get_mut(card.index) {
                    if prompt.multiple {
                        if !picked.contains(&text) {
                            picked.push(text);
                        }
                    } else {
                        *picked = vec![text];
                    }
                }
            }
            card.editing_custom = false;
        } else {
            card.editing_custom = true;
            return;
        }
    } else if card.selected < prompt.options.len() {
        let label = prompt.options[card.selected].label.clone();
        if let Some(picked) = card.picked.get_mut(card.index) {
            if !prompt.multiple {
                *picked = vec![label];
            } else if !picked.contains(&label) {
                picked.push(label);
            }
        }
    }

    if card.index + 1 < questions.len() {
        card.index += 1;
        card.selected = 0;
        card.editing_custom = false;
    } else {
        let answers = card.picked.clone();
        resolve_focused_question(state, QuestionOutcome::Answered { answers });
    }
}

fn question_input_char(state: &mut AppState, c: char) {
    let Some(pending) = state.pending_questions.first() else {
        return;
    };
    let questions = pending.questions.clone();
    let Some(card) = &mut state.question_card_state else {
        return;
    };
    if let Some(feedback) = &mut card.feedback {
        feedback.push(c);
        return;
    }
    let Some(prompt) = questions.get(card.index) else {
        return;
    };
    let is_custom_row = card.selected == prompt.options.len();
    if is_custom_row {
        card.editing_custom = true;
        if let Some(custom) = card.custom_text.get_mut(card.index) {
            custom.push(c);
        }
    }
}

fn question_input_backspace(state: &mut AppState) {
    let Some(card) = &mut state.question_card_state else {
        return;
    };
    if let Some(feedback) = &mut card.feedback {
        feedback.pop();
        return;
    }
    if card.editing_custom {
        if let Some(custom) = card.custom_text.get_mut(card.index) {
            custom.pop();
        }
    }
}

fn question_open_reject(state: &mut AppState) {
    let Some(card) = &mut state.question_card_state else {
        return;
    };
    // When a text input already owns the keystroke — the reject-feedback box or
    // a custom-answer field — 'r'/'R' is literal text, not the open-reject
    // shortcut, so feedback containing 'r' stays enterable. Only an idle
    // question card opens the feedback box.
    if card.feedback.is_some() || card.editing_custom {
        question_input_char(state, 'r');
        return;
    }
    card.feedback = Some(String::new());
}

fn question_cancel_reject(state: &mut AppState) {
    let Some(card) = &mut state.question_card_state else {
        return;
    };
    if card.feedback.is_some() {
        card.feedback = None;
    } else if card.editing_custom {
        card.editing_custom = false;
    } else {
        resolve_focused_question(state, QuestionOutcome::Rejected { feedback: None });
    }
}

fn question_submit_reject(state: &mut AppState) {
    let Some(card) = &mut state.question_card_state else {
        return;
    };
    let feedback = card.feedback.take();
    resolve_focused_question(state, QuestionOutcome::Rejected { feedback });
}

// --- Docs Studio live editing (Phase 4 STEP 4.3 client wiring) ---

/// Begin editing the focused block: open the block-edit prompt **prefilled with
/// the block's current text**. Submit replaces that text wholesale (see
/// [`Overlay::DocEdit`]), so `e` is a real block editor rather than the
/// prepend-only insertion it used to be. Only meaningful with the editor rail
/// focused and a text-bearing block under the cursor — a table, checklist,
/// diagram, or embed block has no single editable text container, and says so
/// rather than opening a prompt whose submit could not be applied.
fn begin_doc_edit(state: &mut AppState) {
    if !matches!(state.overlay, Overlay::Docs) || state.doc_focus != DocFocus::Editor {
        return;
    }
    let Some(block) = state.focused_block() else {
        return;
    };
    let block_id = block.id.clone();
    let Some(original) = block.editable.clone() else {
        state.notice = Some((
            "this block kind has no editable text".to_owned(),
            state.tick + 25,
        ));
        return;
    };
    state.overlay = Overlay::DocEdit {
        block_id,
        buffer: original.clone(),
        original,
    };
}

/// Open the new-document prompt (`n`). Available anywhere in the Docs Studio —
/// creating the first document is exactly the case where no document is focused.
fn begin_doc_new(state: &mut AppState) {
    if matches!(state.overlay, Overlay::Docs) {
        state.overlay = Overlay::DocNew {
            buffer: String::new(),
        };
    }
}

/// Open the insert-block prompt (`i`): a new paragraph directly *below* the
/// focused block, or at the top of a document with no blocks yet.
fn begin_doc_insert(state: &mut AppState) {
    if !matches!(state.overlay, Overlay::Docs) || state.doc_focus != DocFocus::Editor {
        return;
    }
    let Some(doc) = state.focused_doc() else {
        return;
    };
    // Below the cursor; an empty document inserts at 0. `selected_block` is
    // clamped to the block list, so this can never exceed its length.
    let index = if doc.blocks.is_empty() {
        0
    } else {
        (state.selected_block + 1).min(doc.blocks.len())
    };
    state.overlay = Overlay::DocInsert {
        index: index as u32,
        buffer: String::new(),
    };
}

/// Ask to delete the focused block (`X`). Deletion is destructive and has no TUI
/// undo, so it routes through a confirmation rather than firing off the keypress.
fn begin_doc_delete(state: &mut AppState) {
    if !matches!(state.overlay, Overlay::Docs) || state.doc_focus != DocFocus::Editor {
        return;
    }
    if let Some(block) = state.focused_block() {
        state.overlay = Overlay::DocDeleteConfirm {
            block_id: block.id.clone(),
            label: format!("{} — {}", block.kind, block.text),
        };
    }
}

/// A lowercase, dash-separated slug of `title`, for seeding a publish path or
/// branch name. Never empty: an all-punctuation title falls back to `document`.
fn publish_slug(title: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("document");
    }
    slug
}

/// Begin publishing the focused document (`P`). Step 1 is the TARGET, not the
/// path: the daemon accepts a repository-file write, a docs-branch commit, or a
/// documentation PR, and which one it is changes both the fields needed and the
/// risk the approval card will state (outcome 18 F10).
fn begin_doc_publish(state: &mut AppState) {
    if !matches!(state.overlay, Overlay::Docs) {
        return;
    }
    if let Some(doc) = state.focused_doc() {
        state.overlay = Overlay::DocPublishTarget {
            document_id: doc.document_id,
            selected: 0,
        };
    }
}

/// Advance from the target picker to the path prompt, seeding the path from the
/// document's title exactly as the single-target flow used to.
fn choose_doc_publish_target(state: &mut AppState) {
    let Overlay::DocPublishTarget {
        document_id,
        selected,
    } = &state.overlay
    else {
        return;
    };
    let Some(&target) = DOC_PUBLISH_TARGETS.get(*selected) else {
        return;
    };
    let document_id = *document_id;
    let slug = state
        .docs
        .iter()
        .find(|doc| doc.document_id == document_id)
        .map_or_else(|| "document".to_owned(), |doc| publish_slug(&doc.title));
    state.overlay = Overlay::DocPublishPath {
        document_id,
        target,
        buffer: format!("docs/{slug}.md"),
    };
}

/// Acquire `block_id`'s edit lease and queue `mutation` to fire once the daemon
/// grants it. Releases any lease this client already holds first, so switching to
/// a new block never orphans the old lease.
fn start_doc_edit(
    state: &mut AppState,
    document_id: DocumentId,
    block_id: Option<String>,
    mutation: DocumentMutation,
) {
    release_doc_lease(state);
    state.doc_edit = Some(DocEdit {
        document_id,
        block_id: block_id.clone(),
        lease: DocLeaseState::Acquiring,
        lease_id: None,
        pending: Some(mutation),
    });
    state.outbox.push(Intent::AcquireDocumentLease {
        document_id,
        block_id,
    });
}

/// The daemon granted the requested lease: mark the edit held and fire its queued
/// mutation exactly once. A late grant for an edit that is no longer in flight
/// must be released explicitly; otherwise closing Docs while acquisition is in
/// flight leaves collaborators blocked until the server-side lease TTL expires.
fn on_lease_granted(state: &mut AppState, document_id: DocumentId, lease_id: String) {
    let mutation = match state.doc_edit.as_mut() {
        Some(edit) if edit.document_id == document_id => {
            edit.lease = DocLeaseState::Held;
            edit.lease_id = Some(lease_id);
            edit.pending.take()
        }
        _ => {
            state.outbox.push(Intent::ReleaseDocumentLease { lease_id });
            return;
        }
    };
    if let Some(mutation) = mutation {
        state.outbox.push(Intent::MutateDocument {
            document_id,
            mutation,
        });
    }
}

/// The daemon refused the lease (`document.range-leased`): mark the edit blocked,
/// drop its queued mutation, and surface the presence-lite notice.
fn on_lease_blocked(state: &mut AppState) {
    if let Some(edit) = state.doc_edit.as_mut() {
        edit.lease = DocLeaseState::Blocked;
        edit.pending = None;
    }
    state.notice = Some((
        "block is being edited by another writer".to_owned(),
        state.tick + 25,
    ));
}

/// Release a held block lease (if any). Only a *held* lease carries an id to
/// release; an acquiring or blocked one just clears.
fn release_doc_lease(state: &mut AppState) {
    if let Some(edit) = state.doc_edit.take() {
        if let Some(lease_id) = edit.lease_id {
            state.outbox.push(Intent::ReleaseDocumentLease { lease_id });
        }
    }
}

/// Accept (`accept = true`) or reject the focused suggestion in the review rail,
/// through the daemon's `MutateDocument` accept/reject (role-gated there — a
/// resolution needs no edit lease). Only fires with the review rail focused and a
/// suggestion under the cursor.
fn resolve_focused_suggestion(state: &mut AppState, accept: bool) {
    if state.doc_focus != DocFocus::Review {
        return;
    }
    let Some(document_id) = state.focused_doc().map(|doc| doc.document_id) else {
        return;
    };
    let Some(suggestion_id) = state.focused_suggestion().map(|s| s.id.clone()) else {
        return;
    };
    let mutation = if accept {
        DocumentMutation::AcceptSuggestion { suggestion_id }
    } else {
        DocumentMutation::RejectSuggestion { suggestion_id }
    };
    state.outbox.push(Intent::MutateDocument {
        document_id,
        mutation,
    });
}

/// Fold a merged replica update (already projected by the harness) into the
/// matching card, replacing its blocks, suggestions, and revision so the editor
/// reflects the authoritative result, then re-clamp the rail cursors.
fn apply_document_sync(
    state: &mut AppState,
    document_id: DocumentId,
    revision: String,
    blocks: Vec<DocBlockView>,
    suggestions: Vec<DocSuggestionView>,
) {
    let Some(card) = state.docs.iter_mut().find(|d| d.document_id == document_id) else {
        return;
    };
    card.revision = revision;
    card.blocks = blocks;
    card.suggestions = suggestions;
    let blocks_len = card.blocks.len();
    let suggestions_len = card.suggestions.len();
    clamp(&mut state.selected_block, blocks_len);
    clamp(&mut state.selected_suggestion, suggestions_len);
}

/// One text mutation, applied at the active buffer's insertion point. Only the
/// composer has a movable cursor; every other prompt buffer is append-only, so
/// [`append`] applies these at its end and reproduces the old push/pop
/// behaviour exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Edit {
    /// Insert text at the cursor (a typed char, a paste, or a line break).
    Insert(String),
    /// Delete the grapheme before the cursor.
    Backspace,
    /// Delete the word before the cursor (`Ctrl-W`).
    WordBack,
    /// Delete from the start of the cursor's line to the cursor (`Ctrl-U`).
    ToLineStart,
}

/// `[start, end)` byte range of the line containing `cursor` (a line being the
/// text between `\n`s — the composer's `Alt-Enter` breaks).
fn line_bounds(text: &str, cursor: usize) -> (usize, usize) {
    let start = text[..cursor].rfind('\n').map_or(0, |i| i + 1);
    let end = text[cursor..]
        .find('\n')
        .map_or(text.len(), |offset| cursor + offset);
    (start, end)
}

/// Apply `edit` at `cursor`, keeping `cursor` on a `char` boundary and inside
/// `buf`. Deletions step whole GRAPHEMES, so a combining sequence (`e` + U+0301)
/// and a multi-byte or double-width character are removed as one unit rather
/// than being cut in half into invalid text.
fn splice(buf: &mut String, cursor: &mut usize, edit: &Edit) {
    *cursor = (*cursor).min(buf.len());
    while *cursor > 0 && !buf.is_char_boundary(*cursor) {
        *cursor -= 1;
    }
    match edit {
        Edit::Insert(text) => {
            buf.insert_str(*cursor, text);
            *cursor += text.len();
        }
        Edit::Backspace => {
            if let Some(prev) = prev_grapheme(buf, *cursor) {
                buf.replace_range(prev..*cursor, "");
                *cursor = prev;
            }
        }
        Edit::WordBack => {
            let (line_start, _) = line_bounds(buf, *cursor);
            // Skip the whitespace immediately before the cursor, then delete
            // back to the start of the word it trails (readline's `Ctrl-W`).
            let mut at = *cursor;
            while at > line_start {
                let Some(prev) = prev_grapheme(buf, at) else {
                    break;
                };
                if buf[prev..at].trim().is_empty() {
                    at = prev;
                } else {
                    break;
                }
            }
            while at > line_start {
                let Some(prev) = prev_grapheme(buf, at) else {
                    break;
                };
                if buf[prev..at].trim().is_empty() {
                    break;
                }
                at = prev;
            }
            buf.replace_range(at..*cursor, "");
            *cursor = at;
        }
        Edit::ToLineStart => {
            let (line_start, _) = line_bounds(buf, *cursor);
            buf.replace_range(line_start..*cursor, "");
            *cursor = line_start;
        }
    }
}

/// Apply `edit` at the end of an append-only buffer (every prompt except the
/// composer). `Insert` is a push, `Backspace` a pop — exactly what these
/// buffers did before the composer grew a cursor.
fn append(buf: &mut String, edit: &Edit) {
    let mut cursor = buf.len();
    splice(buf, &mut cursor, edit);
}

/// The byte offset of the grapheme boundary before `cursor`, or `None` at the
/// start of the buffer.
fn prev_grapheme(text: &str, cursor: usize) -> Option<usize> {
    UnicodeSegmentation::grapheme_indices(&text[..cursor], true)
        .next_back()
        .map(|(offset, _)| offset)
}

/// The byte offset of the grapheme boundary after `cursor`, or `None` at the
/// end of the buffer.
fn next_grapheme(text: &str, cursor: usize) -> Option<usize> {
    UnicodeSegmentation::grapheme_indices(&text[cursor..], true)
        .next()
        .map(|(_, grapheme)| cursor + grapheme.len())
}

/// Where a cursor key moves the composer's insertion point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorMove {
    Left,
    Right,
    LineStart,
    LineEnd,
}

/// `←`/`→`/`Home`/`End` in the composer. Horizontal motion steps whole
/// graphemes (never landing mid-character); `Home`/`End` are scoped to the
/// cursor's own line, so they stay useful in a multi-line draft.
fn move_composer_cursor(state: &mut AppState, motion: CursorMove) {
    if !matches!(state.overlay, Overlay::None) {
        return;
    }
    // Moving the caret is composing, not browsing.
    end_browse(state);
    let cursor = state.composer_cursor.min(state.composer.len());
    let (line_start, line_end) = line_bounds(&state.composer, cursor);
    state.composer_cursor = match motion {
        CursorMove::Left => prev_grapheme(&state.composer, cursor).unwrap_or(0),
        CursorMove::Right => next_grapheme(&state.composer, cursor).unwrap_or(cursor),
        CursorMove::LineStart => line_start,
        CursorMove::LineEnd => line_end,
    };
}

/// Delete backwards in the composer (`Ctrl-W` / `Ctrl-U`); a no-op elsewhere,
/// where these keys have never meant anything.
fn delete_backwards(state: &mut AppState, edit: &Edit) {
    if !matches!(state.overlay, Overlay::None) {
        return;
    }
    splice(&mut state.composer, &mut state.composer_cursor, edit);
    detach_history_on_edit(state);
}

/// The composer cursor's display column within its own line — the width the
/// terminal actually paints, so `↑`/`↓` keep their column across CJK and emoji.
fn cursor_column(text: &str, cursor: usize) -> usize {
    let (start, _) = line_bounds(text, cursor);
    UnicodeWidthStr::width(&text[start..cursor])
}

/// The byte offset within `[start, end)` closest to display `column` without
/// overshooting it, snapped to a grapheme boundary.
fn offset_for_column(text: &str, start: usize, end: usize, column: usize) -> usize {
    let mut width = 0;
    for (offset, grapheme) in UnicodeSegmentation::grapheme_indices(&text[start..end], true) {
        if width >= column {
            return start + offset;
        }
        width += UnicodeWidthStr::width(grapheme);
    }
    end
}

/// `↑` in the composer: move to the line above, keeping the display column;
/// only at the draft's TOP line does it step up into the pending prompt queue
/// (if non-empty) or fall through to history recall.
fn composer_up(state: &mut AppState) {
    if matches!(state.overlay, Overlay::None) {
        if state.queue_editing.is_some() {
            return;
        }
        if let Some(idx) = state.queue_selected {
            if idx > 0 {
                state.queue_selected = Some(idx - 1);
            }
            return;
        }
        let cursor = state.composer_cursor.min(state.composer.len());
        let (line_start, _) = line_bounds(&state.composer, cursor);
        if line_start > 0 {
            let column = cursor_column(&state.composer, cursor);
            let (prev_start, prev_end) = line_bounds(&state.composer, line_start - 1);
            state.composer_cursor =
                offset_for_column(&state.composer, prev_start, prev_end, column);
            end_browse(state);
            return;
        }
        if !state.pending_prompts.is_empty() {
            state.queue_selected = Some(state.pending_prompts.len().saturating_sub(1));
            end_browse(state);
            return;
        }
    }
    history_prev(state);
}

/// `↓` in the composer: the mirror of [`composer_up`] — the line below, then
/// history at the draft's bottom line.
fn composer_down(state: &mut AppState) {
    if matches!(state.overlay, Overlay::None) {
        if state.queue_editing.is_some() {
            return;
        }
        if let Some(idx) = state.queue_selected {
            if idx + 1 < state.pending_prompts.len() {
                state.queue_selected = Some(idx + 1);
            } else {
                state.queue_selected = None;
            }
            return;
        }
        let cursor = state.composer_cursor.min(state.composer.len());
        let (_, line_end) = line_bounds(&state.composer, cursor);
        if line_end < state.composer.len() {
            let column = cursor_column(&state.composer, cursor);
            let (next_start, next_end) = line_bounds(&state.composer, line_end + 1);
            state.composer_cursor =
                offset_for_column(&state.composer, next_start, next_end, column);
            end_browse(state);
            return;
        }
    }
    history_next(state);
}

fn edit_prompt(state: &mut AppState, edit: &Edit) {
    match &mut state.overlay {
        Overlay::NewRun(buf) | Overlay::Steering(buf) => append(buf, edit),
        Overlay::WorkflowInputs { buffer, .. } => append(buffer, edit),
        Overlay::KanbanNew { buffer } | Overlay::BlackboardPost { buffer, .. } => {
            append(buffer, edit)
        }
        Overlay::CouncilRunObjective { buffer, .. } => append(buffer, edit),
        Overlay::EdgeSearch(buffer) => append(buffer, edit),
        Overlay::DocNew { buffer } => append(buffer, edit),
        Overlay::DocInsert { buffer, .. } => append(buffer, edit),
        Overlay::DocEdit { buffer, .. } => append(buffer, edit),
        Overlay::LearningEdit { buffer, .. } => append(buffer, edit),
        Overlay::DocPublishPath { buffer, .. }
        | Overlay::DocPublishBranch { buffer, .. }
        | Overlay::DocPublishTitle { buffer, .. } => append(buffer, edit),
        Overlay::AddModelId { buffer, .. } => append(buffer, edit),
        // The key buffer is a redacting newtype; edit its inner String.
        Overlay::AddModelKey { buffer, .. } => append(&mut buffer.0, edit),
        // The `/keys` set prompt masks the same redacting newtype (D1).
        Overlay::ApiKeySet { buffer, .. } => append(&mut buffer.0, edit),
        // The key-first prompt masks a redacting newtype, like `AddModelKey`.
        Overlay::AddModelProviderKey { buffer, .. } => append(&mut buffer.0, edit),
        // The pick-list filters like the model picker: editing the query resets
        // the selection to the top of the new filtered set.
        Overlay::AddModelPick {
            query, selected, ..
        } => {
            append(query, edit);
            *selected = 0;
        }
        // Same shape as the add-model pick-list: the Unsloth repo/quant
        // browsers filter on their own query, resetting to the top of the
        // new filtered set. Reachable only once loaded (`InputMode::Palette`
        // — see `AppState::input_mode`); while loading, printable keys never
        // reach here.
        Overlay::UnslothRepos {
            query, selected, ..
        }
        | Overlay::UnslothQuants {
            query, selected, ..
        } => {
            append(query, edit);
            *selected = 0;
        }
        // Editing the palette query changes the filtered set, so the selection
        // returns to the top rather than pointing past the new results.
        Overlay::Palette { query, selected } => {
            append(query, edit);
            *selected = 0;
        }
        Overlay::SessionPicker { query, selected } => {
            append(query, edit);
            *selected = 0;
        }
        // The library's query is answered by the DAEMON, so editing it only
        // updates the buffer here; `sync_session_library` (called from the
        // `InputChar`/`InputBackspace` arms, once this borrow has ended) is
        // what emits the new search.
        Overlay::SessionLibrary { query, .. } => append(query, edit),
        Overlay::SessionRename { buffer, .. } => append(buffer, edit),
        // Same shape as the palette: editing the model picker's query changes
        // the filtered set, so the selection returns to the top.
        Overlay::ModelPicker { query, selected } => {
            append(query, edit);
            *selected = 0;
        }
        // Same shape as the model picker (Task 8): editing the provider
        // picker's query changes the filtered set, so the selection returns
        // to the top.
        Overlay::ProviderPicker { query, selected } => {
            append(query, edit);
            *selected = 0;
        }
        Overlay::OnboardProviderPicker {
            query, selected, ..
        } => {
            append(query, edit);
            *selected = 0;
        }
        // Same shape as the provider picker (PR C2): editing the mode
        // picker's query changes the filtered set, so the selection returns
        // to the top.
        Overlay::ModePicker { query, selected } => {
            append(query, edit);
            *selected = 0;
        }
        // Same shape as the mode picker: editing the theme query changes the
        // filtered set, so the selection (and therefore the live preview)
        // returns to the top of the new results.
        Overlay::ThemePicker { query, selected } => {
            append(query, edit);
            *selected = 0;
        }
        // Same shape as the mode picker (D1): editing the `/keys` query
        // changes the filtered set, so the selection returns to the top.
        Overlay::ApiKeys { query, selected } => {
            append(query, edit);
            *selected = 0;
        }
        Overlay::CouncilBuilder(builder) => match builder.step {
            CouncilBuilderStep::Name => append(&mut builder.name, edit),
            CouncilBuilderStep::Description => append(&mut builder.description, edit),
            CouncilBuilderStep::MemberModel | CouncilBuilderStep::Chair => {
                append(&mut builder.query, edit);
                builder.selected = 0;
            }
            CouncilBuilderStep::MemberRole => append(&mut builder.role, edit),
            CouncilBuilderStep::Rounds | CouncilBuilderStep::Review => {}
        },
        // The base view: text lands in the queue edit buffer if editing a queued
        // prompt, or in the persistent composer draft at its cursor.
        Overlay::None => {
            if let Some(buf) = &mut state.queue_editing {
                append(buf, edit);
            } else {
                splice(&mut state.composer, &mut state.composer_cursor, edit);
            }
        }
        _ => {}
    }
    // Keep `selected_model` resolved to the new top-of-filter card (mirrors
    // the reset above, against the full list — see `AppState::selected_model`).
    if let Overlay::ModelPicker { query, .. } = &state.overlay {
        state.selected_model = filter_models(&state.models, query)
            .first()
            .copied()
            .unwrap_or(0);
    }
    // Same re-resolution for the provider picker (Task 8) — see
    // `AppState::selected_provider`.
    if let Overlay::ProviderPicker { query, .. } = &state.overlay {
        state.selected_provider = filter_providers(&state.providers, query)
            .first()
            .copied()
            .unwrap_or(0);
    }
}

/// A typed character. In the base view `/` on an *empty* composer opens the
/// command palette (the Codex-style slash entry); every other key extends the
/// active text buffer.
fn input_char(state: &mut AppState, c: char) {
    if c == '/'
        && matches!(state.overlay, Overlay::None)
        && state.composer.is_empty()
        && state.queue_editing.is_none()
    {
        state.overlay = Overlay::Palette {
            query: String::new(),
            selected: 0,
        };
        return;
    }
    // First-run setup is `InputMode::Palette` but has no query buffer, so every
    // printable key — including the `/` its own splash advertises — used to fall
    // through `edit_prompt`'s `_ => {}` and vanish. The palette is the product's
    // advertised front door; it must work on the very first screen, and `Esc`
    // brings the gate back (see `input_cancel`).
    if c == '/'
        && matches!(
            state.overlay,
            Overlay::Onboard {
                step: OnboardStep::Triage { .. }
            }
        )
    {
        state.palette_from_onboard = true;
        state.overlay = Overlay::Palette {
            query: String::new(),
            selected: 0,
        };
        return;
    }
    edit_prompt(state, &Edit::Insert(c.to_string()));
    detach_history_on_edit(state);
}

fn check_mention_popup(state: &mut AppState) {
    if !matches!(state.overlay, Overlay::None) || state.queue_editing.is_some() {
        state.mention_popup = None;
        return;
    }
    let cursor = state.composer_cursor;
    let before = &state.composer[..cursor];
    if let Some(at_idx) = before.rfind('@') {
        let candidate = &before[at_idx + 1..];
        if !candidate.contains(char::is_whitespace) {
            let query = candidate.to_string();
            state.mention_popup = Some(crate::state::MentionPopup {
                query: query.clone(),
                selected: 0,
                waiting: true,
                display_query: query.clone(),
                matches: Vec::new(),
            });
            state.outbox.push(Intent::SearchFiles { query });
            return;
        }
    }
    state.mention_popup = None;
}

/// `Up` in the @-mention popup: move the highlight one match toward the top.
fn mention_select_prev(state: &mut AppState) {
    if let Some(popup) = &mut state.mention_popup {
        if !popup.matches.is_empty() {
            popup.selected = popup.selected.saturating_sub(1);
        }
    }
}

/// `Down` in the @-mention popup: move the highlight one match toward the
/// bottom.
fn mention_select_next(state: &mut AppState) {
    if let Some(popup) = &mut state.mention_popup {
        if !popup.matches.is_empty() {
            popup.selected = (popup.selected + 1).min(popup.matches.len() - 1);
        }
    }
}

/// Confirm the highlighted @-mention: replace the `@query` before the cursor
/// with the match's path (plus a trailing space) and close the popup.
fn mention_select(state: &mut AppState) {
    if let Some(popup) = state.mention_popup.take() {
        if let Some(matched) = popup.matches.get(popup.selected) {
            // Replace @query before cursor with matched path
            let cursor = state.composer_cursor;
            let before = &state.composer[..cursor];
            if let Some(at_idx) = before.rfind('@') {
                let after = &state.composer[cursor..];
                let insert = format!("{} ", matched.path);
                state.composer = format!("{}{}{}", &before[..at_idx], insert, after);
                state.composer_cursor = at_idx + insert.len();
            }
        }
    }
}

/// `Ctrl-R` opens the history search over the prompt history; once open it
/// (or `Up`) walks the highlight toward older matches.
fn history_search_prev(state: &mut AppState) {
    if let Some(hs) = &mut state.history_search {
        let matching_count = state
            .prompt_history
            .iter()
            .filter(|item| hs.query.is_empty() || item.contains(&hs.query))
            .count();
        if matching_count > 0 {
            hs.selected = (hs.selected + 1).min(matching_count - 1);
        }
    } else {
        // The two composer popups never stack: opening the history search
        // dismisses an open @-mention popup.
        state.mention_popup = None;
        state.history_search = Some(crate::state::HistorySearch {
            query: String::new(),
            selected: 0,
        });
    }
}

/// `Ctrl-S` (or `Down` while the popup is open): walk the highlight toward
/// newer matches.
fn history_search_next(state: &mut AppState) {
    if let Some(hs) = &mut state.history_search {
        hs.selected = hs.selected.saturating_sub(1);
    }
}

/// `Enter` while the history search is open: load the highlighted match into
/// the composer and close the popup. Matches walk newest-first, mirroring the
/// render order.
fn history_search_select(state: &mut AppState) {
    if let Some(hs) = state.history_search.take() {
        let matches: Vec<&String> = state
            .prompt_history
            .iter()
            .rev()
            .filter(|item| hs.query.is_empty() || item.contains(&hs.query))
            .collect();
        if let Some(&item) = matches.get(hs.selected) {
            state.composer = item.clone();
            state.composer_cursor = state.composer.len();
        }
    }
}

/// `Delete` in the `/keys` overlay (D1): open the remove confirm for the focused
/// row, but only when that row actually has a stored (`auth.json`) key — on a
/// row with no stored key there is nothing to remove, so the key is a no-op
/// rather than a confusing confirm.
fn begin_remove_selected(state: &mut AppState) {
    // `Ctrl-D` / `Delete` in the Session Library asks to delete the focused
    // session. It always goes through a confirmation: the client has no undo.
    if let Some((selected, row)) = focused_library_row(state) {
        state.overlay = Overlay::ConfirmSessionDelete {
            session_id: row.session_id,
            title: row.title,
            selected,
        };
        return;
    }
    if let Overlay::ModelPicker { query, selected } = &state.overlay {
        let query = query.clone();
        let selected = *selected;
        let Some(&idx) = filter_models(&state.models, &query).get(selected) else {
            return;
        };
        let Some(card) = state.models.get(idx) else {
            return;
        };
        let model_id = card.id.0.clone();
        let provider = card.provider.clone();
        if let Some(reason) = state.model_removal_blocker(&model_id) {
            state.notice = Some((
                format!("cannot remove {model_id}: {reason}"),
                state.tick + 40,
            ));
            return;
        }
        state.overlay = Overlay::ConfirmModelRemove {
            model_id,
            provider,
            query,
            selected,
        };
        return;
    }

    let Overlay::ApiKeys { query, selected } = &state.overlay else {
        return;
    };
    let Some(&idx) = filter_key_rows(&state.models, &state.voice_key_rows, query).get(*selected)
    else {
        return;
    };
    let target = key_row_target(&state.models, &state.voice_key_rows, idx);
    let stored = match &target {
        KeyTarget::Model(id) => state
            .key_status
            .iter()
            .any(|(model_id, status)| model_id == id && matches!(status, KeyStatus::Stored)),
        KeyTarget::Tavily => matches!(state.tavily_key_status, KeyStatus::Stored),
        // A voice row carries its own status (it is not in `key_status`, which
        // is keyed by model id) — match on the target, not the row index, so
        // the filtered selection can never point at the wrong row's status.
        voice => state
            .voice_key_rows
            .iter()
            .any(|row| &row.target == voice && matches!(row.status, KeyStatus::Stored)),
    };
    if stored {
        state.overlay = Overlay::ApiKeyRemoveConfirm { target };
    }
}

/// Editing the composer while a recalled history entry is loaded detaches it
/// from history: `composer` becomes an ordinary in-progress draft again, so
/// the next `HistoryPrev` stashes *this* text rather than resuming the old
/// recall walk (shell-style: touching a recalled command loses its history
/// binding). A no-op for every other overlay's buffer (they have no history).
fn detach_history_on_edit(state: &mut AppState) {
    if matches!(state.overlay, Overlay::None) {
        state.history_cursor = None;
        state.backtrack_primed = false;
        // Typing means the composer, not the transcript, has the user's
        // attention: leave fold-browse mode so `Alt-Enter` is a line break again.
        end_browse(state);
    }
}

/// `HistoryPrev` (`Up` in the composer): shell-style recall, walking
/// backward. The first press stashes the in-progress draft — so it is never
/// lost — and loads the newest entry; each subsequent press walks toward
/// older entries, saturating at the oldest. A no-op with empty history.
fn history_prev(state: &mut AppState) {
    if state.composer_history.is_empty() {
        return;
    }
    let idx = match state.history_cursor {
        None => {
            state.composer_stash = Some(state.composer.clone());
            state.composer_history.len() - 1
        }
        Some(idx) => idx.saturating_sub(1),
    };
    state.composer = state.composer_history[idx].clone();
    state.composer_cursor = state.composer.len();
    state.history_cursor = Some(idx);
}

/// `HistoryNext` (`Down` in the composer): walk toward newer entries; moving
/// past the newest restores the stashed in-progress draft and detaches from
/// history entirely. A no-op when not currently recalling.
fn history_next(state: &mut AppState) {
    let Some(idx) = state.history_cursor else {
        return;
    };
    if idx + 1 >= state.composer_history.len() {
        state.composer = state.composer_stash.take().unwrap_or_default();
        state.history_cursor = None;
    } else {
        let idx = idx + 1;
        state.composer = state.composer_history[idx].clone();
        state.history_cursor = Some(idx);
    }
    state.composer_cursor = state.composer.len();
}

fn open_onboard(state: &mut AppState) {
    state.onboard_flow = None;
    state.palette_from_onboard = false;
    state.overlay = Overlay::Onboard {
        step: OnboardStep::Triage { selected: 0 },
    };
}

fn open_onboard_provider_picker(
    state: &mut AppState,
    class: OnboardProviderClass,
    preferred_provider: Option<&str>,
) {
    let indices = filter_onboard_providers(&state.providers, class, "");
    if indices.is_empty() {
        let class_label = match class {
            OnboardProviderClass::Hosted => "hosted",
            OnboardProviderClass::LocalEndpoint => "local endpoint",
            OnboardProviderClass::AcpAgent => "ACP agent",
        };
        state.onboard_flow = None;
        state.notice = Some((
            format!("no available {class_label} providers were discovered"),
            state.tick + 50,
        ));
        state.overlay = Overlay::Onboard {
            step: OnboardStep::Triage {
                selected: match class {
                    OnboardProviderClass::Hosted => 0,
                    OnboardProviderClass::LocalEndpoint => 1,
                    OnboardProviderClass::AcpAgent => 2,
                },
            },
        };
        return;
    }
    let selected = preferred_provider
        .and_then(|id| {
            indices
                .iter()
                .position(|idx| state.providers.get(*idx).is_some_and(|card| card.id == id))
        })
        .unwrap_or(0);
    state.selected_provider = indices[selected];
    state.onboard_flow = Some(OnboardFlow::new(class));
    state.overlay = Overlay::OnboardProviderPicker {
        class,
        query: String::new(),
        selected,
    };
}

fn restore_onboard_provider_picker(state: &mut AppState) {
    let Some(flow) = state.onboard_flow.clone() else {
        open_onboard(state);
        return;
    };
    open_onboard_provider_picker(state, flow.class, flow.provider_id.as_deref());
}

fn on_runnable_models_refreshed(
    state: &mut AppState,
    model_ids: Vec<codypendent_protocol::ModelId>,
    onboard_attempt: Option<codypendent_protocol::ModelId>,
) {
    state.runnable_models = model_ids;
    let Some(attempted) = onboard_attempt else {
        return;
    };
    let matches_attempt = state
        .onboard_flow
        .as_ref()
        .and_then(|flow| flow.awaiting_model.as_ref())
        == Some(&attempted);
    if !matches_attempt {
        return;
    }
    if state.runnable_models.iter().any(|id| id == &attempted) {
        state.pending_model = Some(attempted.clone());
        state.onboard_flow = None;
        state.overlay = Overlay::None;
        state.outbox.push(Intent::SetOnboardComplete);
        state.notice = Some((
            format!("connected {attempted} — ready for your first message"),
            state.tick + 50,
        ));
    } else {
        state.notice = Some((
            format!("{attempted} was saved but is not runnable yet"),
            state.tick + 60,
        ));
        restore_onboard_provider_picker(state);
    }
}

fn on_onboard_model_add_failed(
    state: &mut AppState,
    model_id: codypendent_protocol::ModelId,
    reason: String,
) {
    let matches_attempt = state
        .onboard_flow
        .as_ref()
        .and_then(|flow| flow.awaiting_model.as_ref())
        == Some(&model_id);
    if !matches_attempt {
        return;
    }
    state.notice = Some((
        format!("could not connect {model_id}: {reason}"),
        state.tick + 80,
    ));
    restore_onboard_provider_picker(state);
}

fn queue_add_model(
    state: &mut AppState,
    display_id: String,
    provider_id: String,
    model: String,
    api_key: Option<SecretKey>,
    context_tokens: Option<u64>,
) {
    let model_id = codypendent_protocol::ModelId(display_id.clone());
    state.outbox.push(Intent::AddModel {
        display_id,
        provider_id,
        model,
        api_key,
        context_tokens,
    });
    if let Some(flow) = &mut state.onboard_flow {
        flow.awaiting_model = Some(model_id.clone());
        state.overlay = Overlay::Onboard {
            step: OnboardStep::Validating { model_id },
        };
    } else {
        // The normal provider/model flow has handed the mutation to the host;
        // do not leave its picker sitting open over the resulting notice.
        state.overlay = Overlay::None;
    }
}

/// `Esc`: clear the composer draft in the base view, return the block-edit prompt
/// to the Docs browser it opened from, or close whatever other overlay is active.
fn input_cancel(state: &mut AppState) {
    if let Overlay::CouncilBuilder(builder) = &mut state.overlay {
        match builder.step {
            CouncilBuilderStep::Name => state.overlay = Overlay::None,
            CouncilBuilderStep::Description => builder.step = CouncilBuilderStep::Name,
            CouncilBuilderStep::MemberModel => {
                builder.step = CouncilBuilderStep::Description;
                builder.query.clear();
                builder.selected = 0;
            }
            CouncilBuilderStep::MemberRole => {
                builder.step = CouncilBuilderStep::MemberModel;
                builder.pending_member_model = None;
                builder.role.clear();
                builder.query.clear();
                builder.selected = 0;
            }
            CouncilBuilderStep::Chair => {
                builder.step = CouncilBuilderStep::MemberModel;
                builder.query.clear();
                builder.selected = 0;
            }
            CouncilBuilderStep::Rounds => {
                builder.step = CouncilBuilderStep::Chair;
                builder.query.clear();
                builder.selected = 0;
            }
            CouncilBuilderStep::Review => {
                builder.step = CouncilBuilderStep::Rounds;
                builder.selected = usize::from(builder.rounds.saturating_sub(1));
            }
        }
        return;
    }
    // `Esc` while browsing folds steps out of browse mode first, so an
    // in-progress draft is never destroyed by a keypress the user meant as
    // "stop browsing".
    if state.transcript_browse && matches!(state.overlay, Overlay::None) {
        end_browse(state);
        return;
    }
    // `Esc` while editing or navigating a pending prompt cancels that first before
    // touching composer draft or backtrack priming.
    if matches!(state.overlay, Overlay::None) {
        if state.queue_editing.is_some() {
            state.queue_editing = None;
            return;
        }
        if state.queue_selected.is_some() {
            state.queue_selected = None;
            return;
        }
    }
    // The palette opened over first-run setup returns to it; closing to an inert
    // chat would strand an operator who still has no runnable model.
    if matches!(state.overlay, Overlay::Palette { .. }) && state.palette_from_onboard {
        open_onboard(state);
        return;
    }
    match state.overlay {
        Overlay::None => {
            if state.composer.is_empty() {
                if !state.backtrack_primed {
                    state.backtrack_primed = true;
                } else {
                    state.backtrack_primed = false;
                    let count = state.forkable_runs().len();
                    if count > 0 {
                        state.overlay = Overlay::Backtrack(BacktrackState {
                            selected: count.saturating_sub(1),
                        });
                    }
                }
            } else {
                state.composer.clear();
                state.composer_cursor = 0;
                state.backtrack_primed = false;
            }
        }
        Overlay::Backtrack(_) => {
            state.overlay = Overlay::None;
            state.backtrack_primed = false;
        }
        Overlay::Onboard {
            step: OnboardStep::Triage { .. },
        } => {
            state.overlay = Overlay::Onboard {
                step: OnboardStep::SkipConfirm { selected: 0 },
            };
        }
        Overlay::Onboard {
            step: OnboardStep::SkipConfirm { .. },
        } => open_onboard(state),
        // The host may already be writing/connecting. Esc cannot safely cancel
        // that operation, and closing the wait would expose dead Chat.
        Overlay::Onboard {
            step: OnboardStep::Validating { .. },
        } => {}
        Overlay::OnboardProviderPicker { .. } => open_onboard(state),
        Overlay::AddModelId { .. }
        | Overlay::AddModelKey { .. }
        | Overlay::AddModelProviderKey { .. }
        | Overlay::AddModelQuerying { .. }
        | Overlay::AddModelPick { .. }
            if state.onboard_flow.is_some() =>
        {
            restore_onboard_provider_picker(state);
        }
        // Abandoning the block-edit prompt returns to the browser, not the base
        // view (no lease was taken yet — the acquire only fires on submit).
        Overlay::DocEdit { .. }
        | Overlay::DocNew { .. }
        | Overlay::DocInsert { .. }
        | Overlay::DocDeleteConfirm { .. } => state.overlay = Overlay::Docs,
        // Every publish step abandons back to the browser: nothing is sent
        // until the last one, so there is no partial publish to unwind.
        Overlay::DocPublishTarget { .. }
        | Overlay::DocPublishPath { .. }
        | Overlay::DocPublishBranch { .. }
        | Overlay::DocPublishTitle { .. } => state.overlay = Overlay::Docs,
        Overlay::EdgeSearch(_) => state.overlay = Overlay::Edges,
        Overlay::WorkflowInputs { .. } => state.overlay = Overlay::Workflow,
        Overlay::KanbanNew { .. } => state.overlay = Overlay::Kanban,
        Overlay::BlackboardPost { .. } => state.overlay = Overlay::Blackboard,
        Overlay::ConfirmWorkflowCancel { .. } => state.overlay = Overlay::Workflow,
        Overlay::CouncilRunObjective { .. } => state.overlay = Overlay::CouncilBrowser,
        // Abandoning a rename or a delete confirmation returns to the library
        // with its cursor intact — nothing was sent, so there is nothing to
        // unwind.
        Overlay::SessionRename { selected, .. }
        | Overlay::ConfirmSessionDelete { selected, .. } => {
            return_to_session_library(state, selected);
        }
        _ => state.overlay = Overlay::None,
    }
}

/// Switch the conversation to another run (`Ctrl-↑/↓`), clamping at the ends.
fn cycle_run(state: &mut AppState, delta: i32) {
    step(&mut state.selected_run, state.runs.len(), delta);
    // The browsed fold belonged to the run we just left.
    end_browse(state);
}

/// Set the open list overlay's `selected` to `n`, mirroring `nav`'s picker
/// resolution (keeps `selected_model`/`selected_provider` pointed at the same
/// filtered card). A no-op for a non-list overlay.
fn set_overlay_selected(state: &mut AppState, n: usize) {
    match state.overlay {
        Overlay::Onboard { ref mut step } => match step {
            OnboardStep::Triage { selected } | OnboardStep::SkipConfirm { selected } => {
                *selected = n.min(2);
            }
            OnboardStep::Validating { .. } => {}
        },
        Overlay::Palette {
            ref mut selected, ..
        }
        | Overlay::AddModelPick {
            ref mut selected, ..
        }
        // Same shape: no resolved `AppState` index to keep in sync, exactly
        // like the mode picker / `/keys` below.
        | Overlay::UnslothRepos {
            ref mut selected, ..
        }
        | Overlay::UnslothQuants {
            ref mut selected, ..
        }
        // A fixed three-row list, so the clicked index IS the selection.
        | Overlay::DocPublishTarget {
            ref mut selected, ..
        } => {
            *selected = n;
        }
        Overlay::ModelPicker {
            ref query,
            ref mut selected,
        } => {
            *selected = n;
            let indices = filter_models(&state.models, query);
            state.selected_model = indices.get(n).copied().unwrap_or(0);
        }
        Overlay::ProviderPicker {
            ref query,
            ref mut selected,
        } => {
            *selected = n;
            let indices = filter_providers(&state.providers, query);
            state.selected_provider = indices.get(n).copied().unwrap_or(0);
        }
        Overlay::OnboardProviderPicker {
            class,
            ref query,
            ref mut selected,
        } => {
            *selected = n;
            let indices = filter_onboard_providers(&state.providers, class, query);
            state.selected_provider = indices.get(n).copied().unwrap_or(0);
        }
        // The mode picker keeps no resolved `AppState` index (PR C2) — the
        // cursor alone identifies the row, exactly like the palette.
        Overlay::ModePicker {
            ref mut selected, ..
        } => {
            *selected = n;
        }
        // Same for the theme picker and the `/keys` overlay (D1).
        Overlay::ThemePicker {
            ref mut selected, ..
        }
        | Overlay::ApiKeys {
            ref mut selected, ..
        } => {
            *selected = n;
        }
        Overlay::CouncilBuilder(ref mut builder) => {
            builder.selected = n;
            if builder.step == CouncilBuilderStep::Rounds {
                builder.rounds = u8::try_from(n + 1).unwrap_or(3).clamp(1, 3);
            }
        }
        _ => {}
    }
}

/// A click on row N: activate the open list overlay's row N (same effect as
/// selecting it + `Enter`), or — with no overlay — toggle the transcript fold
/// line at entry N of the selected run (same effect as `Enter` on that entry).
fn activate_row(state: &mut AppState, n: usize) {
    match state.overlay {
        Overlay::Issues => {
            let mut selected = n;
            clamp(&mut selected, state.issues.len());
            state.selected_issue = selected;
        }
        Overlay::Skills => {
            let mut selected = n;
            clamp(&mut selected, state.skills.len());
            state.selected_skill = selected;
        }
        Overlay::Memory { .. } => {
            let mut selected = n;
            clamp(&mut selected, state.memories.len());
            state.selected_memory = selected;
            state.overlay = Overlay::Memory { source_open: true };
        }
        Overlay::Journey => {
            let mut selected = n;
            clamp(&mut selected, state.learnings.len());
            state.selected_learning = selected;
        }
        Overlay::Edges => {
            let mut selected = n;
            clamp(&mut selected, state.edges.len());
            state.selected_edge = selected;
        }
        Overlay::Workflow => {
            let mut selected = n;
            clamp(&mut selected, state.workflow.len());
            state.selected_node = selected;
            watch_focused_workflow(state);
        }
        Overlay::Blackboard => {
            let mut selected = n;
            clamp(&mut selected, state.blackboard.len());
            state.selected_item = selected;
            watch_focused_blackboard_run(state);
        }
        Overlay::Kanban => {
            let mut selected = n;
            clamp(&mut selected, state.kanban_in_display_order().len());
            state.selected_card = selected;
        }
        Overlay::UiPlugins => {
            let mut selected = n;
            clamp(&mut selected, state.ui_plugins.len());
            state.selected_ui_plugin = selected;
        }
        Overlay::CouncilBrowser => {
            let mut selected = n;
            clamp(&mut selected, state.councils.len());
            state.selected_council = selected;
        }
        Overlay::CouncilResults => {
            let mut selected = n;
            clamp(&mut selected, state.council_results.len());
            state.selected_council_result = selected;
            state.council_result_scroll = 0;
        }
        Overlay::Palette { .. }
        | Overlay::Onboard {
            step: OnboardStep::Triage { .. } | OnboardStep::SkipConfirm { .. },
        }
        | Overlay::OnboardProviderPicker { .. }
        | Overlay::ModelPicker { .. }
        | Overlay::ProviderPicker { .. }
        | Overlay::ModePicker { .. }
        | Overlay::ThemePicker { .. }
        | Overlay::ApiKeys { .. }
        | Overlay::DocPublishTarget { .. }
        | Overlay::AddModelPick { .. }
        | Overlay::CouncilBuilder(_)
        | Overlay::UnslothRepos { .. }
        | Overlay::UnslothQuants { .. } => {
            set_overlay_selected(state, n);
            submit_prompt(state);
        }
        Overlay::None => {
            let run_idx = state.selected_run;
            activate_fold(state, run_idx, n);
        }
        _ => {}
    }
}

/// Toggle the transcript fold at `(run, entry)` — the mouse's path to a tool
/// card, patch diff, or long note anywhere in the stacked conversation, not
/// only in the run the composer happens to be pointed at.
fn activate_fold(state: &mut AppState, run_idx: usize, entry: usize) {
    if !matches!(state.overlay, Overlay::None) {
        return;
    }
    state.focus = Pane::Transcript;
    // A click whose address no longer exists (the projection moved under the
    // frame it was drawn from) must not fall through and toggle whatever the
    // cursor happened to be on.
    if state
        .runs
        .get(run_idx)
        .is_none_or(|run| entry >= run.transcript.len())
    {
        return;
    }
    set_fold_cursor(state, (run_idx, entry));
    // A clicked fold becomes the browsed one, so the keyboard can carry on
    // from where the mouse left off (and the row the click landed on is
    // visibly selected).
    state.transcript_browse = true;
    expand_selected(state);
}

fn submit_prompt(state: &mut AppState) {
    // Validation belongs to the current attempted council transition. Clear a
    // stale message first so a corrected value never carries the old error
    // into the next tab; failing arms below install a fresh inline explanation.
    if matches!(state.overlay, Overlay::CouncilBuilder(_)) {
        state.notice = None;
    }
    match std::mem::take(&mut state.overlay) {
        Overlay::Backtrack(bt) => {
            let forkable = state.forkable_runs();
            if let Some(target_run) = forkable.get(bt.selected) {
                if let Some(cp_id) = target_run.launch_checkpoint {
                    state.composer = target_run.objective.clone();
                    state.composer_cursor = state.composer.len();
                    state.overlay = Overlay::None;
                    state.backtrack_primed = false;
                    state.outbox.push(Intent::ForkSession {
                        checkpoint: cp_id,
                        prompt: state.composer.clone(),
                    });
                    return;
                }
            }
            state.overlay = Overlay::None;
            state.backtrack_primed = false;
        }
        Overlay::Onboard { step } => match step {
            OnboardStep::Triage { selected } => {
                let class = match selected.min(2) {
                    0 => OnboardProviderClass::Hosted,
                    1 => OnboardProviderClass::LocalEndpoint,
                    _ => OnboardProviderClass::AcpAgent,
                };
                open_onboard_provider_picker(state, class, None);
            }
            OnboardStep::SkipConfirm { selected } => {
                if selected == 0 {
                    state.onboard_flow = None;
                    state.outbox.push(Intent::SetOnboardSkipped);
                    state.notice = Some((
                        "setup skipped — press Enter here whenever you want to connect a model"
                            .to_owned(),
                        state.tick + 50,
                    ));
                } else {
                    // Both "Continue setup" and "Cancel" are deliberately
                    // safe returns. Neither persists nor clears a prior skip.
                    open_onboard(state);
                }
            }
            OnboardStep::Validating { model_id } => {
                state.overlay = Overlay::Onboard {
                    step: OnboardStep::Validating { model_id },
                };
            }
        },
        Overlay::OnboardProviderPicker {
            class,
            query,
            selected,
        } => {
            let indices = filter_onboard_providers(&state.providers, class, &query);
            let Some(&idx) = indices.get(selected) else {
                state.overlay = Overlay::OnboardProviderPicker {
                    class,
                    query,
                    selected: 0,
                };
                return;
            };
            let Some(card) = state.providers.get(idx) else {
                open_onboard_provider_picker(state, class, None);
                return;
            };
            let provider_id = card.id.clone();
            let protocol = card.protocol.clone();
            let requires_key = card.requires_key;
            let can_list_models = card.can_list_models;
            let catalog_models = card.catalog_models;
            let has_key = card.has_key;
            let requires_community_consent =
                provider_id == "antigravity-acp" && card.auth.contains("third-party ToS risk");
            state.selected_provider = idx;
            state.onboard_flow = Some(OnboardFlow {
                class,
                provider_id: Some(provider_id.clone()),
                awaiting_model: None,
            });
            if requires_community_consent {
                state.overlay = Overlay::ConfirmCommunityAcpInstall {
                    provider_id,
                    query,
                    selected,
                    onboard_class: Some(class),
                };
                return;
            }
            enter_add_model_flow(
                state,
                provider_id,
                protocol,
                requires_key,
                can_list_models,
                catalog_models,
                has_key,
            );
        }
        Overlay::NewRun(text) => {
            let objective = text.trim().to_owned();
            if !objective.is_empty() {
                if state.pending_run_start.is_some() {
                    state.overlay = Overlay::NewRun(text);
                    state.notice = Some((
                        "a run is already starting — wait for it to attach".to_owned(),
                        state.tick + 25,
                    ));
                    return;
                }
                state.outbox.push(Intent::StartRun {
                    objective: objective.clone(),
                    mode: state.default_mode,
                    // Pin the operator's chosen model (STEP MP2). Session-default:
                    // `pending_model` is NOT cleared here, so one pick applies to
                    // this run and every subsequent one until the operator changes
                    // it in the `/model` picker.
                    model: state.pending_model.clone(),
                });
                state.pending_run_start = Some(PendingRunStart {
                    draft: objective,
                    target: RunStartDraftTarget::NewRunPrompt,
                    started_tick: state.tick,
                });
            }
        }
        Overlay::LearningEdit {
            id,
            revision,
            buffer,
        } => {
            let statement = buffer.split_whitespace().collect::<Vec<_>>().join(" ");
            if statement.is_empty() {
                state.overlay = Overlay::LearningEdit {
                    id,
                    revision,
                    buffer,
                };
                state.notice = Some((
                    "a learning statement cannot be empty".into(),
                    state.tick + 30,
                ));
            } else {
                state.outbox.push(Intent::MutateLearning {
                    id,
                    revision,
                    mutation: LearningMutation::EditStatement(statement),
                });
                state.overlay = Overlay::Journey;
            }
        }
        Overlay::Steering(text) => {
            let text = text.trim().to_owned();
            if !text.is_empty() {
                state.outbox.push(Intent::QueuePrompt {
                    text,
                    mode: state.default_mode,
                    delivery: codypendent_protocol::PromptDelivery::Steer,
                });
            }
        }
        Overlay::CouncilRunObjective { name, buffer } => {
            let objective = buffer.trim().to_owned();
            if objective.is_empty() {
                state.overlay = Overlay::CouncilRunObjective { name, buffer };
                state.notice = Some((
                    "council objective must not be empty".to_owned(),
                    state.tick + 30,
                ));
            } else {
                state.outbox.push(Intent::RunCouncil {
                    name: name.clone(),
                    objective,
                });
                state.overlay = Overlay::CouncilBrowser;
                state.notice = Some((format!("council `{name}` running…"), state.tick + 60));
            }
        }
        Overlay::WorkflowInputs {
            workflow_id,
            buffer,
        } => {
            let input = buffer.trim();
            let parsed = if input.is_empty() {
                Ok(serde_json::json!({}))
            } else {
                serde_json::from_str::<serde_json::Value>(input)
            };
            match parsed {
                Ok(inputs) if inputs.is_object() => {
                    state.outbox.push(Intent::StartWorkflow {
                        workflow_id,
                        inputs,
                    });
                    state.overlay = Overlay::Workflow;
                    state.notice = Some(("starting workflow…".to_owned(), state.tick + 40));
                }
                Ok(_) => {
                    state.overlay = Overlay::WorkflowInputs {
                        workflow_id,
                        buffer,
                    };
                    state.notice = Some((
                        "workflow inputs must be a JSON object".to_owned(),
                        state.tick + 30,
                    ));
                }
                Err(error) => {
                    state.overlay = Overlay::WorkflowInputs {
                        workflow_id,
                        buffer,
                    };
                    state.notice = Some((
                        format!("invalid workflow input JSON: {error}"),
                        state.tick + 30,
                    ));
                }
            }
        }
        Overlay::KanbanNew { buffer } => {
            let title = buffer.trim().to_owned();
            state.overlay = Overlay::Kanban;
            if title.is_empty() {
                state.notice = Some(("task title must not be empty".to_owned(), state.tick + 30));
            } else {
                state.outbox.push(Intent::CreateBoardCard { title });
                state.notice = Some(("creating Kanban task…".to_owned(), state.tick + 30));
            }
        }
        Overlay::BlackboardPost {
            workflow_run_id,
            buffer,
        } => {
            let text = buffer.trim().to_owned();
            state.overlay = Overlay::Blackboard;
            if text.is_empty() {
                state.notice = Some(("question must not be empty".to_owned(), state.tick + 30));
            } else {
                state.outbox.push(Intent::PostBlackboardQuestion {
                    workflow_run_id,
                    text,
                });
                state.notice = Some((
                    "posting open question to Blackboard…".to_owned(),
                    state.tick + 30,
                ));
            }
        }
        Overlay::EdgeSearch(query) => {
            state.edge_query = query.trim().to_owned();
            state.overlay = Overlay::Edges;
            request_edge_page(state, 0);
        }
        // Submit the block-edit prompt: acquire the block's lease and queue the
        // replacement to fire once it is granted. `mem::take` left the overlay
        // `None`; restore the Docs browser so the reflected sync lands in view.
        Overlay::DocEdit {
            block_id,
            buffer,
            original,
        } => {
            state.overlay = Overlay::Docs;
            let text = buffer.trim().to_owned();
            let document_id = state.focused_doc().map(|doc| doc.document_id);
            // An unchanged buffer is not an edit — never spend a revision (and a
            // suggestion in Suggest mode) on a no-op submit.
            if text == original.trim() {
                return;
            }
            if let Some(document_id) = document_id {
                // A FULL REPLACE of the block's text: delete exactly the
                // characters the prompt was prefilled with, then insert what the
                // writer typed. `delete_len` counts CHARACTERS (the CRDT's text
                // ops are character-indexed), not bytes. In Edit mode this
                // applies directly; in Suggest mode the daemon turns the same
                // range into a reviewable suggestion.
                let mutation = DocumentMutation::EditText {
                    block_id: block_id.clone(),
                    position: 0,
                    delete_len: original.chars().count() as u32,
                    insert: text,
                };
                start_doc_edit(state, document_id, Some(block_id), mutation);
            }
        }
        // Submit the new-document prompt: the harness sends `CreateDocument` and
        // refreshes the Docs projection, so the document appears in the tree.
        Overlay::DocNew { buffer } => {
            state.overlay = Overlay::Docs;
            let title = buffer.trim().to_owned();
            if title.is_empty() {
                state.notice = Some(("a document needs a title".to_owned(), state.tick + 25));
                return;
            }
            state.outbox.push(Intent::CreateDocument { title });
        }
        // Submit the insert-block prompt: a new paragraph at `index`. A block
        // insert reshapes the block list, so it takes the WHOLE-DOCUMENT lease
        // (`block_id: None`) the daemon's structural gate requires.
        Overlay::DocInsert { index, buffer } => {
            state.overlay = Overlay::Docs;
            let text = buffer.trim().to_owned();
            let document_id = state.focused_doc().map(|doc| doc.document_id);
            if let (false, Some(document_id)) = (text.is_empty(), document_id) {
                let mutation = DocumentMutation::Insert {
                    index,
                    block_id: uuid::Uuid::now_v7().to_string(),
                    content: serde_json::json!({ "type": "paragraph", "text": text }),
                };
                start_doc_edit(state, document_id, None, mutation);
            }
        }
        // Publish step 1: the chosen target decides which prompts follow.
        // `mem::take` already cleared the overlay, so put it back for
        // `choose_doc_publish_target` to read the focused row from.
        Overlay::DocPublishTarget {
            document_id,
            selected,
        } => {
            state.overlay = Overlay::DocPublishTarget {
                document_id,
                selected,
            };
            choose_doc_publish_target(state);
        }
        Overlay::DocPublishPath {
            document_id,
            target,
            buffer,
        } => {
            let path = buffer.trim().to_owned();
            if !valid_publish_path(&path) {
                state.overlay = Overlay::DocPublishPath {
                    document_id,
                    target,
                    buffer,
                };
                state.notice = Some((
                    "enter a repository-relative Markdown (.md) path without parent traversal"
                        .to_owned(),
                    state.tick + 30,
                ));
            } else if target.needs_branch() {
                // The branch defaults to one derived from the path, so the
                // common case is Enter-Enter rather than inventing a name.
                let slug = publish_slug(path.trim_end_matches(".md"));
                state.overlay = Overlay::DocPublishBranch {
                    document_id,
                    target,
                    path,
                    buffer: format!("docs/{slug}"),
                };
            } else {
                state.outbox.push(Intent::PublishDocument {
                    document_id,
                    target: codypendent_protocol::PublishTarget::RepositoryFile { path },
                });
                state.overlay = Overlay::Docs;
                state.notice = Some((
                    "preparing publish plan for approval…".to_owned(),
                    state.tick + 40,
                ));
            }
        }
        // Publish step 3: the branch. Validated with the same conservative
        // shape rule the path uses — a branch name reaches `git` on the daemon
        // side, so shell/refspec metacharacters and traversal never leave here.
        Overlay::DocPublishBranch {
            document_id,
            target,
            path,
            buffer,
        } => {
            let branch = buffer.trim().to_owned();
            if !valid_publish_branch(&branch) {
                state.overlay = Overlay::DocPublishBranch {
                    document_id,
                    target,
                    path,
                    buffer,
                };
                state.notice = Some((
                    "enter a branch name of letters, digits, `.`, `_`, `-` and `/` \
                     (no leading dash, no `..`)"
                        .to_owned(),
                    state.tick + 30,
                ));
            } else if matches!(target, DocPublishTargetKind::DocumentationPr) {
                state.overlay = Overlay::DocPublishTitle {
                    document_id,
                    path,
                    branch,
                    // A PR needs a human title; seed it from the document so an
                    // empty submit is never the fast path.
                    buffer: state
                        .docs
                        .iter()
                        .find(|doc| doc.document_id == document_id)
                        .map_or_else(String::new, |doc| format!("docs: {}", doc.title)),
                };
            } else {
                state.outbox.push(Intent::PublishDocument {
                    document_id,
                    target: codypendent_protocol::PublishTarget::DocsBranchCommit { branch, path },
                });
                state.overlay = Overlay::Docs;
                state.notice = Some((
                    "preparing publish plan for approval…".to_owned(),
                    state.tick + 40,
                ));
            }
        }
        // Publish step 4: the PR title. Only its emptiness is checked — it is
        // prose destined for a PR body, not a path or a ref.
        Overlay::DocPublishTitle {
            document_id,
            path,
            branch,
            buffer,
        } => {
            let title = buffer.trim().to_owned();
            if title.is_empty() {
                state.overlay = Overlay::DocPublishTitle {
                    document_id,
                    path,
                    branch,
                    buffer,
                };
                state.notice = Some((
                    "enter a title for the pull request".to_owned(),
                    state.tick + 30,
                ));
            } else {
                state.outbox.push(Intent::PublishDocument {
                    document_id,
                    target: codypendent_protocol::PublishTarget::DocumentationPr {
                        branch,
                        path,
                        title,
                    },
                });
                state.overlay = Overlay::Docs;
                state.notice = Some((
                    "preparing publish plan for approval…".to_owned(),
                    state.tick + 40,
                ));
            }
        }
        // `mem::take` already closed the palette (left `None`); run the
        // highlighted command, which may open its own overlay.
        Overlay::Palette { query, selected } => {
            // The chosen command owns the flow from here, so `Esc` belongs to
            // whatever it opened rather than to the setup gate behind it.
            state.palette_from_onboard = false;
            if let Some(selector) = parse_council_result_query(&query) {
                state.overlay = Overlay::CouncilResults;
                state.outbox.push(Intent::LoadCouncilResults { selector });
            } else if let Some(entry) = crate::palette::filtered(&query).get(selected) {
                run_palette_command(state, entry.command);
            }
        }
        Overlay::SessionPicker { query, selected } => {
            let matches = filter_session_rows(&state.session_list, &query);
            if let Some(&idx) = matches.get(selected) {
                if let Some(session) = state.session_list.get(idx) {
                    if session.state.eq_ignore_ascii_case("closed") {
                        state.notice =
                            Some(("cannot resume a closed session".to_owned(), state.tick + 40));
                        state.overlay = Overlay::SessionPicker { query, selected };
                    } else {
                        state.outbox.push(Intent::SwitchSession(session.session_id));
                    }
                }
            }
        }
        // Enter on a library row resumes it, with the same closed-session
        // refusal the picker applies — the library ranks more rows, it does not
        // relax what can be resumed.
        Overlay::SessionLibrary {
            query,
            selected,
            waiting,
        } => {
            if let Some(session) = state.session_library.get(selected) {
                if session.state.eq_ignore_ascii_case("closed") {
                    state.notice =
                        Some(("cannot resume a closed session".to_owned(), state.tick + 40));
                    state.overlay = Overlay::SessionLibrary {
                        query,
                        selected,
                        waiting,
                    };
                } else {
                    state.outbox.push(Intent::SwitchSession(session.session_id));
                }
            } else {
                state.overlay = Overlay::SessionLibrary {
                    query,
                    selected,
                    waiting,
                };
            }
        }
        Overlay::SessionRename {
            session_id,
            buffer,
            selected,
        } => {
            let title = buffer.trim().to_owned();
            if title.is_empty() || title.chars().any(char::is_control) {
                state.notice = Some((
                    "a session title must be non-empty and on one line".to_owned(),
                    state.tick + 40,
                ));
                state.overlay = Overlay::SessionRename {
                    session_id,
                    buffer,
                    selected,
                };
            } else {
                state.outbox.push(Intent::MutateSession {
                    session_id,
                    action: codypendent_protocol::SessionLifecycleAction::Rename { title },
                });
                return_to_session_library(state, selected);
            }
        }
        Overlay::CouncilBuilder(mut builder) => match builder.step {
            CouncilBuilderStep::Name => {
                let name = builder.name.trim();
                if name.is_empty()
                    || name.len() > 64
                    || !name.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                    })
                {
                    state.notice = Some((
                        "council name: use 1–64 letters, numbers, dot, dash, or underscore"
                            .to_owned(),
                        state.tick + 40,
                    ));
                } else {
                    builder.name = name.to_owned();
                    builder.step = CouncilBuilderStep::Description;
                }
                state.overlay = Overlay::CouncilBuilder(builder);
            }
            CouncilBuilderStep::Description => {
                let description = builder.description.trim();
                if description.len() > 1024 || description.chars().any(char::is_control) {
                    state.notice = Some((
                        "purpose must be at most 1024 characters on one line".to_owned(),
                        state.tick + 40,
                    ));
                } else if state.models.len() < 2 {
                    state.notice = Some((
                        "configure at least two model profiles before creating a council"
                            .to_owned(),
                        state.tick + 50,
                    ));
                } else {
                    builder.description = description.to_owned();
                    builder.step = CouncilBuilderStep::MemberModel;
                    builder.query.clear();
                    builder.selected = 0;
                }
                state.overlay = Overlay::CouncilBuilder(builder);
            }
            CouncilBuilderStep::MemberModel => {
                let continue_row = builder.members.len() >= 2 && builder.query.trim().is_empty();
                let remove_row = !builder.members.is_empty() && builder.query.trim().is_empty();
                let indices = if builder.members.len() >= 8 {
                    Vec::new()
                } else {
                    filter_council_member_models(&state.models, &builder.query, &builder.members)
                };
                if continue_row && builder.selected == 0 {
                    builder.step = CouncilBuilderStep::Chair;
                    builder.query.clear();
                    builder.selected = 0;
                } else if remove_row
                    && builder.selected == usize::from(continue_row).saturating_add(indices.len())
                {
                    if let Some(removed) = builder.members.pop() {
                        state.notice = Some((
                            format!("removed {} from the draft council", removed.model),
                            state.tick + 25,
                        ));
                    }
                    builder.selected = 0;
                } else if builder.members.len() < 8 {
                    let row = builder.selected.saturating_sub(usize::from(continue_row));
                    if let Some(card) = indices.get(row).and_then(|idx| state.models.get(*idx)) {
                        if let ModelReadiness::Unavailable(reason) = &card.readiness {
                            state.notice =
                                Some((format!("model unavailable — {reason}"), state.tick + 40));
                        } else {
                            builder.pending_member_model = Some(card.id.0.clone());
                            builder.role.clear();
                            builder.step = CouncilBuilderStep::MemberRole;
                        }
                    }
                }
                state.overlay = Overlay::CouncilBuilder(builder);
            }
            CouncilBuilderStep::MemberRole => {
                let role = builder.role.trim();
                let role = if role.is_empty() { "member" } else { role };
                if role.len() > 80 || role.chars().any(char::is_control) {
                    state.notice = Some((
                        "member role must be at most 80 safe characters".to_owned(),
                        state.tick + 40,
                    ));
                } else if let Some(model) = builder.pending_member_model.take() {
                    builder.members.push(CouncilMemberDraft {
                        model,
                        role: role.to_owned(),
                    });
                    builder.role.clear();
                    builder.query.clear();
                    builder.selected = 0;
                    builder.step = CouncilBuilderStep::MemberModel;
                }
                state.overlay = Overlay::CouncilBuilder(builder);
            }
            CouncilBuilderStep::Chair => {
                let indices = filter_models(&state.models, &builder.query);
                if let Some(card) = indices
                    .get(builder.selected)
                    .and_then(|idx| state.models.get(*idx))
                {
                    if let ModelReadiness::Unavailable(reason) = &card.readiness {
                        state.notice = Some((
                            format!("chair model unavailable — {reason}"),
                            state.tick + 40,
                        ));
                    } else {
                        builder.chair = Some(card.id.0.clone());
                        builder.query.clear();
                        builder.selected = usize::from(builder.rounds.saturating_sub(1));
                        builder.step = CouncilBuilderStep::Rounds;
                    }
                }
                state.overlay = Overlay::CouncilBuilder(builder);
            }
            CouncilBuilderStep::Rounds => {
                builder.rounds = u8::try_from(builder.selected + 1).unwrap_or(3).clamp(1, 3);
                builder.step = CouncilBuilderStep::Review;
                state.overlay = Overlay::CouncilBuilder(builder);
            }
            CouncilBuilderStep::Review => {
                let Some(chair) = builder.chair.clone() else {
                    state.notice = Some(("select a chair model".to_owned(), state.tick + 30));
                    builder.step = CouncilBuilderStep::Chair;
                    state.overlay = Overlay::CouncilBuilder(builder);
                    return;
                };
                if !(2..=8).contains(&builder.members.len()) {
                    state.notice = Some(("select 2–8 council members".to_owned(), state.tick + 30));
                    builder.step = CouncilBuilderStep::MemberModel;
                    state.overlay = Overlay::CouncilBuilder(builder);
                    return;
                }
                let member_count = builder.members.len();
                state.outbox.push(Intent::CreateCouncil {
                    name: builder.name.clone(),
                    description: builder.description.clone(),
                    members: builder
                        .members
                        .iter()
                        .map(|member| (member.model.clone(), member.role.clone()))
                        .collect(),
                    chair,
                    rounds: builder.rounds,
                });
                state.notice = Some((
                    format!(
                        "creating council `{}` with {member_count} members…",
                        builder.name
                    ),
                    state.tick + 50,
                ));
                // Keep the reviewed draft visible until the host confirms the
                // private atomic write. A filesystem/duplicate-name failure is
                // therefore correctable without re-entering every member.
                state.overlay = Overlay::CouncilBuilder(builder);
            }
        },
        // Enter stages the focused model on `pending_model` and emits a status
        // notice. `pending_model` now PINS the model for the run(s) the operator
        // starts (STEP MP2 wired it through `Intent::StartRun` → the `StartRun`
        // command's `model` field); as a session default it applies to this run
        // and every subsequent one until changed here.
        // Re-derives the filtered list from the overlay's own `query` /
        // `selected` (mirroring the palette arm above) rather than trusting
        // `selected_model`: that field's `.unwrap_or(0)` fallback (see `nav`
        // / `edit_prompt`) points at the full list's row 0 whenever the
        // filter matches nothing, and a query with zero matches must stage
        // nothing — not silently pick a model the picker isn't even
        // showing. `mem::take` already closed the picker (left the overlay
        // `None`).
        Overlay::ModelPicker { query, selected } => {
            if let Some(&idx) = filter_models(&state.models, &query).get(selected) {
                if let Some(card) = state.models.get(idx) {
                    if let ModelReadiness::Unavailable(reason) = &card.readiness {
                        state.overlay = Overlay::ModelPicker { query, selected };
                        state.notice =
                            Some((format!("model unavailable — {reason}"), state.tick + 40));
                        return;
                    }
                    // A bare ACP row is both the agent's default profile and
                    // the doorway to the models advertised by that agent. Do
                    // not silently stage the default when the user is asking
                    // to choose a model: handshake off-thread, then open the
                    // ordinary searchable AddModelPick list. Pinned ACP rows
                    // (`acp/<agent>#<model>`) still stage directly below.
                    if let Some(provider_id) = card.acp_supplier().map(str::to_owned) {
                        state.outbox.push(Intent::QueryProviderModels {
                            provider_id: provider_id.clone(),
                            api_key: None,
                            refresh: false,
                        });
                        state.overlay = Overlay::AddModelQuerying {
                            provider_id: provider_id.clone(),
                            api_key: None,
                        };
                        state.notice = Some((
                            format!("loading models from {provider_id}…"),
                            state.tick + 50,
                        ));
                        return;
                    }
                    let id = card.id.clone();
                    state.pending_model = Some(id.clone());
                    state.notice = Some((
                        format!("model set to {id} — applies to your next run"),
                        state.tick + 25,
                    ));
                }
            }
        }
        // Enter begins the add-model flow for the focused provider — the same
        // branch `Tab` takes (model-discovery). The old `pending_provider`
        // staging + "applies to your next run" notice are removed: nothing ever
        // consumed the staged value. Re-derives the filtered selection from the
        // overlay's own `query`/`selected` (the zero-match guard the model picker
        // uses); `mem::take` already closed the picker, so `enter_add_model_flow`
        // sets the next overlay directly.
        Overlay::ProviderPicker { query, selected } => {
            if let Some(&idx) = filter_providers(&state.providers, &query).get(selected) {
                if let Some(card) = state.providers.get(idx) {
                    let provider_id = card.id.clone();
                    let protocol = card.protocol.clone();
                    let requires_key = card.requires_key;
                    let can_list_models = card.can_list_models;
                    let available = card.available;
                    let catalog_models = card.catalog_models;
                    let has_key = card.has_key;
                    let requires_community_consent = provider_id == "antigravity-acp"
                        && card.auth.contains("third-party ToS risk");
                    if available {
                        if requires_community_consent {
                            state.overlay = Overlay::ConfirmCommunityAcpInstall {
                                provider_id,
                                query,
                                selected,
                                onboard_class: None,
                            };
                            return;
                        }
                        enter_add_model_flow(
                            state,
                            provider_id,
                            protocol,
                            requires_key,
                            can_list_models,
                            catalog_models,
                            has_key,
                        );
                    } else {
                        state.notice = Some((
                            format!(
                                "{provider_id} is catalog-only — its {} runtime adapter is not installed",
                                card.protocol
                            ),
                            state.tick + 40,
                        ));
                        state.overlay = Overlay::ProviderPicker { query, selected };
                    }
                }
            }
        }
        // Enter sets the submission mode for the next run on `default_mode`
        // (PR C2 — plan mode) and emits a status notice. Outbound intents
        // already read `default_mode`, so a picked mode applies to the very
        // next message — no wire change. Re-derives the filtered selection
        // from the overlay's own `query`/`selected` (the zero-match guard the
        // model picker uses): a query matching nothing sets nothing.
        // `mem::take` already closed the picker.
        Overlay::ModePicker { query, selected } => {
            if let Some(&idx) = filter_modes(&query).get(selected) {
                let card = crate::state::MODE_CARDS[idx];
                state.default_mode = card.mode;
                state.notice = Some((
                    format!("mode set to {} — applies to your next run", card.label),
                    state.tick + 25,
                ));
            }
        }
        // Enter keeps the previewed theme: the renderer already draws in it
        // (`AppState::effective_theme`), so this only makes the choice sticky
        // and asks the harness to remember it for the next launch. Same
        // zero-match guard as every other picker. `mem::take` already closed
        // the picker.
        Overlay::ThemePicker { query, selected } => {
            if let Some(&idx) = filter_themes(&state.themes, &query).get(selected) {
                state.theme_selected = Some(idx);
                let id = state.themes[idx].id.clone();
                state.notice = Some((format!("theme set to {id}"), state.tick + 25));
                state.outbox.push(Intent::SetTheme { id });
            }
        }
        // Enter on a `/keys` row (D1) opens the masked set/replace prompt for
        // that row's target. Re-derives the filtered selection from the
        // overlay's own `query`/`selected` (the zero-match guard the other
        // pickers use): a query matching nothing opens nothing. `mem::take`
        // already closed the picker; the prompt replaces it.
        Overlay::ApiKeys { query, selected } => {
            if let Some(&idx) =
                filter_key_rows(&state.models, &state.voice_key_rows, &query).get(selected)
            {
                state.overlay = Overlay::ApiKeySet {
                    target: key_row_target(&state.models, &state.voice_key_rows, idx),
                    buffer: SecretKey(String::new()),
                };
            }
        }
        // The masked set/replace prompt (D1): emit `Intent::SetApiKey` with the
        // key handed to the harness (client-only — the key never goes on the
        // wire). A blank key is rejected with a notice and nothing is emitted:
        // writing an empty entry would silently shadow a valid `api_key_env`
        // (the `write_add_model` M1 guard's rule).
        Overlay::ApiKeySet { target, buffer } => {
            let key = buffer.0.trim().to_owned();
            if key.is_empty() {
                state.notice = Some(("key not saved (blank)".to_owned(), state.tick + 25));
                // Reopen the prompt rather than dropping the operator back to
                // the base view: a stray `Enter` mid-paste should not discard
                // the flow they were in. Mirrors `AddModelId`, which has always
                // reopened on a blank submit.
                state.overlay = Overlay::ApiKeySet {
                    target,
                    buffer: SecretKey(String::new()),
                };
            } else {
                state.outbox.push(Intent::SetApiKey {
                    target,
                    key: SecretKey(key),
                });
            }
        }
        // Base view (`mem::take` left `None`): send the composer. A live run is
        // queued on the session prompt queue; a terminal run is followed up (continuing
        // the same conversation); with no run at all yet, the message starts the
        // session's first one. The draft clears either way.
        Overlay::None => {
            if let Some(idx) = state.queue_selected {
                if let Some(buf) = state.queue_editing.take() {
                    let text = buf.trim().to_owned();
                    if text.is_empty() {
                        state.notice = Some(("prompt cannot be empty".to_owned(), state.tick + 25));
                        state.queue_editing = Some(buf);
                        return;
                    }
                    if let Some(entry) = state.pending_prompts.get(idx) {
                        state.outbox.push(Intent::UpdateQueuedPrompt {
                            prompt_id: entry.id,
                            text,
                        });
                    }
                } else if let Some(entry) = state.pending_prompts.get(idx) {
                    state.outbox.push(Intent::PromoteQueuedPrompt {
                        prompt_id: entry.id,
                    });
                }
                return;
            }

            let mut text = state.composer.trim().to_owned();
            for block in &state.pasted_blocks {
                text = text.replace(&block.marker, &block.text);
            }
            state.pasted_blocks.clear();

            if text.is_empty() && state.selected_run().is_none() && !state.has_runnable_models() {
                open_onboard(state);
                return;
            }
            if let Some(cmd_text) = text.strip_prefix('!') {
                let cmd = cmd_text.trim().to_owned();
                if !cmd.is_empty() {
                    if state.composer_history.last().map(String::as_str) != Some(text.as_str()) {
                        state.composer_history.push(text.clone());
                    }
                    if state.prompt_history.last().map(String::as_str) != Some(text.as_str()) {
                        state.prompt_history.push(text.clone());
                    }
                    state.history_cursor = None;
                    state.composer_stash = None;
                    state.outbox.push(Intent::RunUserShell { command: cmd });
                }
                state.composer.clear();
                state.composer_cursor = 0;
                if let Some(run) = state.selected_run_mut() {
                    run.follow = true;
                }
                return;
            }
            if !text.contains('\n') && text.starts_with('#') {
                let memory_text = text.trim_start_matches('#').trim().to_owned();
                if !memory_text.is_empty() {
                    if state.composer_history.last().map(String::as_str) != Some(text.as_str()) {
                        state.composer_history.push(text.clone());
                    }
                    if state.prompt_history.last().map(String::as_str) != Some(text.as_str()) {
                        state.prompt_history.push(text.clone());
                    }
                    state.history_cursor = None;
                    state.composer_stash = None;
                    state
                        .outbox
                        .push(Intent::RememberMemory { text: memory_text });
                }
                state.composer.clear();
                state.composer_cursor = 0;
                if let Some(run) = state.selected_run_mut() {
                    run.follow = true;
                }
                return;
            }
            if !text.is_empty() {
                // An empty session has no durable run id with which to route a
                // second message. Retain it as a draft until the first
                // `RunStarted` attaches, instead of accidentally launching a
                // second independent run during a slow round trip.
                if state.selected_run().is_none() && state.pending_run_start.is_some() {
                    state.notice = Some((
                        "a run is already starting — your draft is retained".to_owned(),
                        state.tick + 25,
                    ));
                    return;
                }
                // Shell-style history: record the submission (skip a
                // consecutive duplicate) and end any in-flight recall — the
                // walk-back state from *this* submission is stale now.
                if state.composer_history.last().map(String::as_str) != Some(text.as_str()) {
                    state.composer_history.push(text.clone());
                }
                if state.prompt_history.last().map(String::as_str) != Some(text.as_str()) {
                    state.prompt_history.push(text.clone());
                }
                state.history_cursor = None;
                state.composer_stash = None;
                if state.selected_run_is_active() {
                    state.outbox.push(Intent::QueuePrompt {
                        text,
                        mode: state.default_mode,
                        delivery: codypendent_protocol::PromptDelivery::Queue,
                    });
                } else if state.selected_run().is_some() {
                    // Task 5 (continuous-session plan): a run already exists and
                    // has reached a terminal state — this message continues the
                    // SAME session rather than starting a context-free run, so
                    // the daemon seeds the continuation from the prior turns
                    // (Tasks 1-4). The prior turn stays visible in the render
                    // (all of the session's runs, not just this one).
                    state.outbox.push(Intent::SubmitUserInput {
                        text,
                        mode: state.default_mode,
                        // Carry the current pin so a mid-conversation model
                        // switch is instant: a re-pick applies to THIS very
                        // follow-up, not just a fresh run. `None` (never pinned)
                        // inherits the session's model server-side, unchanged.
                        model: state.pending_model.clone(),
                    });
                } else {
                    // No run yet this session: nothing to continue — start one.
                    state.outbox.push(Intent::StartRun {
                        objective: text.clone(),
                        mode: state.default_mode,
                        // Carry the pinned model (STEP MP2); session-default, so
                        // it is not cleared and applies to subsequent runs too.
                        model: state.pending_model.clone(),
                    });
                    state.pending_run_start = Some(PendingRunStart {
                        draft: text,
                        target: RunStartDraftTarget::Composer,
                        started_tick: state.tick,
                    });
                }
            }
            state.composer.clear();
            state.composer_cursor = 0;
            // Snap the conversation back to the latest so the reply is in view.
            if let Some(run) = state.selected_run_mut() {
                run.follow = true;
            }
        }
        // Add-model free-text fallback: a captured key emits directly; otherwise
        // today's rule (hosted → masked key prompt; local → emit now). A blank
        // name reopens the prompt, carrying any captured key. `mem::take` left
        // the overlay `None`.
        Overlay::AddModelId {
            provider_id,
            requires_key,
            api_key,
            buffer,
        } => {
            let model = buffer.trim().to_owned();
            if model.is_empty() {
                state.notice = Some(("model name cannot be blank".to_owned(), state.tick + 25));
                state.overlay = Overlay::AddModelId {
                    provider_id,
                    requires_key,
                    api_key,
                    buffer: String::new(),
                };
            } else if let Some(key) = api_key.filter(|key| {
                // A captured key only short-circuits the key step when it is
                // actually a key. A BLANK captured key for a key-requiring
                // provider must still route through the masked prompt —
                // otherwise a failed query with an empty key silently writes a
                // keyless model that can only 401 at run time.
                !requires_key || !key.0.trim().is_empty()
            }) {
                // A key was already captured (a can-list provider's failed query
                // fell back here). Emit directly — never re-prompt. A blank inner
                // key normalizes to `None`.
                let display_id = format!("{provider_id}/{model}");
                let inner = key.0.trim().to_owned();
                let api_key = if inner.is_empty() {
                    None
                } else {
                    Some(SecretKey(inner))
                };
                state.notice = Some((format!("adding model {display_id}"), state.tick + 25));
                queue_add_model(state, display_id, provider_id, model, api_key, None);
            } else if requires_key {
                state.overlay = Overlay::AddModelKey {
                    provider_id,
                    model,
                    buffer: SecretKey(String::new()),
                };
            } else {
                let display_id = format!("{provider_id}/{model}");
                state.notice = Some((format!("adding model {display_id}"), state.tick + 25));
                queue_add_model(state, display_id, provider_id, model, None, None);
            }
        }
        // Add-model flow step 3 (masked key): emit `Intent::AddModel` with the key
        // handed to the harness. An empty key emits `api_key: None`.
        Overlay::AddModelKey {
            provider_id,
            model,
            buffer,
        } => {
            let key = buffer.0.trim().to_owned();
            let display_id = format!("{provider_id}/{model}");
            if key.is_empty() {
                state.notice = Some(("API key cannot be blank".to_owned(), state.tick + 30));
                state.overlay = Overlay::AddModelKey {
                    provider_id,
                    model,
                    buffer: SecretKey(String::new()),
                };
                return;
            }
            state.notice = Some((format!("adding model {display_id}"), state.tick + 25));
            queue_add_model(
                state,
                display_id,
                provider_id,
                model,
                Some(SecretKey(key)),
                None,
            );
        }
        // Key-first prompt (can-list hosted): emit the query with the entered key
        // (blank → no key) and open the transient "Fetching…" state, keeping the
        // key in the overlay for the round trip.
        Overlay::AddModelProviderKey {
            provider_id,
            buffer,
        } => {
            let key = buffer.0.trim().to_owned();
            if key.is_empty() {
                state.notice = Some(("API key cannot be blank".to_owned(), state.tick + 30));
                state.overlay = Overlay::AddModelProviderKey {
                    provider_id,
                    buffer: SecretKey(String::new()),
                };
                return;
            }
            let api_key = Some(SecretKey(key));
            state.outbox.push(Intent::QueryProviderModels {
                provider_id: provider_id.clone(),
                api_key: api_key.clone(),
                refresh: false,
            });
            state.overlay = Overlay::AddModelQuerying {
                provider_id,
                api_key,
            };
        }
        // The pick-list: resolve the filtered selection (same zero-match guard as
        // the model picker) and emit `AddModel` for the chosen row, moving the
        // stashed key and the row's known context window into the intent.
        Overlay::AddModelPick {
            provider_id,
            api_key,
            models,
            query,
            selected,
            ..
        } => {
            if let Some(&idx) = filter_model_names(&models, &query).get(selected) {
                if let Some(row) = models.get(idx) {
                    let model = row.id.clone();
                    let context_tokens = row.context_tokens;
                    let display_id = format!("{provider_id}/{model}");
                    state.notice = Some((format!("adding model {display_id}"), state.tick + 25));
                    queue_add_model(
                        state,
                        display_id,
                        provider_id,
                        model,
                        api_key,
                        context_tokens,
                    );
                }
            }
        }
        // Step 1 → 2 of the Unsloth pull flow: resolve the filtered selection
        // (same zero-match-closes convention as `AddModelPick`/`ModelPicker`
        // above — an empty `repos` list during `loading` can never match, so
        // a stray submit while still fetching simply closes) and begin
        // fetching the chosen repo's quant variants.
        Overlay::UnslothRepos {
            repos,
            query,
            selected,
            ..
        } => {
            if let Some(&idx) = filter_unsloth_repos(&repos, &query).get(selected) {
                if let Some(card) = repos.get(idx) {
                    let repo_id = card.id.clone();
                    state.outbox.push(Intent::ListUnslothQuants {
                        repo_id: repo_id.clone(),
                    });
                    state.overlay = Overlay::UnslothQuants {
                        repo_id,
                        quants: Vec::new(),
                        query: String::new(),
                        selected: 0,
                        loading: true,
                    };
                }
            }
        }
        // Step 2 → 3: resolve the filtered selection and open the pull
        // confirm dialog.
        Overlay::UnslothQuants {
            repo_id,
            quants,
            query,
            selected,
            ..
        } => {
            if let Some(&idx) = filter_unsloth_quants(&quants, &query).get(selected) {
                if let Some(card) = quants.get(idx) {
                    state.overlay = Overlay::UnslothConfirmPull {
                        repo_id,
                        quant: card.quant.clone(),
                        size_label: card.size_label.clone(),
                    };
                }
            }
        }
        // Nothing to submit; restore the (non-text) overlay we took.
        other => state.overlay = other,
    }
}

/// Whether `branch` is a safe git branch name to hand the daemon's publish
/// engine (outcome 18 F10). Deliberately a small allowlist rather than a
/// re-implementation of `git check-ref-format`: this value ends up in a
/// `git` invocation and a PR head ref on the daemon side, so anything outside
/// letters, digits, `.`, `_`, `-` and `/` is refused here rather than relied on
/// to be rejected there. `..` is excluded for the same reason a publish path
/// excludes `ParentDir`, and a leading `-` cannot be read as a flag.
fn valid_publish_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && !branch.starts_with('/')
        && !branch.ends_with('/')
        && !branch.ends_with(".lock")
        && !branch.contains("..")
        && !branch.contains("//")
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

fn valid_publish_path(path: &str) -> bool {
    use std::path::Component;

    let path = std::path::Path::new(path);
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path.components().all(|component| {
            !matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        && path
            .components()
            .any(|component| matches!(component, Component::Normal(_)))
        && path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        && !path.to_string_lossy().chars().any(char::is_control)
}

/// The shared add-model entry, called by both `Tab` (`begin_add_model`) and
/// `Enter` (the `ProviderPicker` submit arm). Branches on the focused provider's
/// gates (model-discovery):
/// - can-list + hosted → key-first masked prompt (the key is needed before the
///   model name exists), which on submit queries `<base_url>/models`.
/// - can-list + local/no-auth → query immediately (no key).
/// - cannot-list → today's free-text `AddModelId` flow, unchanged.
///
/// ACP agents branch first: an installed one joins the can-list query path (its
/// models come from the session handshake, not an HTTP endpoint), an
/// uninstalled one connects directly.
/// - can-list + hosted with no stored key → key-first masked prompt (the key is
///   needed before the model name exists), which on submit queries
///   `<base_url>/models`.
/// - can-list + local/no-auth, or a provider whose key `auth.json` already
///   holds → query immediately (the harness resolves the stored key), so the
///   same key is never asked for twice.
/// - cannot-list, but the catalog ships models for it → query anyway: the
///   harness answers from the catalog, so a provider with no listing endpoint
///   (Perplexity) still gets a real pick-list instead of a free-text prompt.
/// - cannot-list and nothing curated → the free-text `AddModelId` flow.
fn enter_add_model_flow(
    state: &mut AppState,
    provider_id: String,
    protocol: String,
    requires_key: bool,
    can_list_models: bool,
    catalog_models: usize,
    has_key: bool,
) {
    if protocol == "acp" {
        // An installed ACP agent advertises its own models over the session
        // handshake, so it takes the SAME query -> pick path a hosted provider
        // does; the harness spawns the agent instead of GETting `/models`, and
        // short-circuits to a plain connect if it advertises no model selector.
        // An agent that is not installed yet has nothing to handshake, so it
        // keeps the connect-then-see path.
        if can_list_models {
            state.outbox.push(Intent::QueryProviderModels {
                provider_id: provider_id.clone(),
                api_key: None,
                // A first query, not the operator's manual refresh — the cache
                // may answer instantly and refresh behind the overlay.
                refresh: false,
            });
            state.overlay = Overlay::AddModelQuerying {
                provider_id,
                api_key: None,
            };
            return;
        }
        let display_id = format!("acp/{provider_id}");
        state.notice = Some((format!("connecting {display_id}"), state.tick + 25));
        queue_add_model(
            state,
            display_id,
            provider_id.clone(),
            provider_id,
            None,
            None,
        );
        return;
    }
    let can_offer = can_list_models || catalog_models > 0;
    state.overlay = if can_offer && requires_key && !has_key {
        Overlay::AddModelProviderKey {
            provider_id,
            buffer: SecretKey(String::new()),
        }
    } else if can_offer {
        state.outbox.push(Intent::QueryProviderModels {
            provider_id: provider_id.clone(),
            api_key: None,
            refresh: false,
        });
        Overlay::AddModelQuerying {
            provider_id,
            api_key: None,
        }
    } else {
        Overlay::AddModelId {
            provider_id,
            requires_key,
            api_key: None,
            buffer: String::new(),
        }
    };
}

/// Re-fetch the open add-model pick-list from the provider (`Ctrl-R`),
/// bypassing the on-disk cache. The stashed key rides the new query so a
/// hosted provider is not asked for it again; the current rows stay on screen
/// (with a "refreshing" marker) until the answer arrives. A no-op in every
/// other overlay.
fn refresh_provider_models(state: &mut AppState) {
    let Overlay::AddModelPick {
        provider_id,
        api_key,
        refreshing,
        ..
    } = &mut state.overlay
    else {
        return;
    };
    if *refreshing {
        return;
    }
    *refreshing = true;
    let intent = Intent::QueryProviderModels {
        provider_id: provider_id.clone(),
        api_key: api_key.clone(),
        refresh: true,
    };
    state.outbox.push(intent);
    state.notice = Some(("refreshing the model list…".to_owned(), state.tick + 25));
}

/// Verify the focused `/keys` row's key against its provider (`Ctrl-T`): emit
/// the one-shot probe intent. A no-op outside the `/keys` overlay and on the
/// Tavily row, which has no model endpoint to probe.
fn begin_verify_key(state: &mut AppState) {
    let Overlay::ApiKeys { query, selected } = &state.overlay else {
        return;
    };
    let Some(&idx) = filter_key_rows(&state.models, &state.voice_key_rows, query).get(*selected)
    else {
        return;
    };
    let Some(card) = state.models.get(idx) else {
        state.notice = Some((
            "verification is available for model rows only".to_owned(),
            state.tick + 25,
        ));
        return;
    };
    let model_id = card.id.0.clone();
    state.notice = Some((format!("verifying {model_id}…"), state.tick + 40));
    state.outbox.push(Intent::VerifyApiKey { model_id });
}

/// Fold a key-verification result back in: the notice reports it, and the
/// model's card readiness is replaced with the honest answer so a hosted model
/// stops claiming `Unverified` once it has actually been probed.
fn on_model_key_verified(state: &mut AppState, model_id: &str, ok: bool, reason: &str) {
    if let Some(card) = state.models.iter_mut().find(|card| card.id.0 == model_id) {
        card.readiness = if ok {
            ModelReadiness::Ready
        } else {
            ModelReadiness::Unavailable(reason.to_owned())
        };
    }
    state.notice = Some((
        if ok {
            format!("{model_id}: key verified")
        } else {
            format!("{model_id}: {reason}")
        },
        state.tick + 40,
    ));
}

/// Begin the add-model flow (`Tab` in the `/provider` picker) for the focused
/// catalog provider. A no-op outside the provider picker, or when the filtered
/// selection matches no provider (the same zero-match guard the Enter arm uses).
fn begin_add_model(state: &mut AppState) {
    // Council creation is also a stepwise picker. `Tab` is advertised and
    // expected as Continue there, so route it through the exact same validated
    // transition as Enter; all non-council/non-provider overlays remain no-ops.
    if matches!(state.overlay, Overlay::CouncilBuilder(_)) {
        submit_prompt(state);
        return;
    }
    let (
        provider_id,
        protocol,
        requires_key,
        can_list_models,
        available,
        catalog_models,
        has_key,
        requires_community_consent,
        query,
        selected,
    ) = {
        let Overlay::ProviderPicker { query, selected } = &state.overlay else {
            return;
        };
        let Some(&idx) = filter_providers(&state.providers, query).get(*selected) else {
            return;
        };
        match state.providers.get(idx) {
            Some(card) => (
                card.id.clone(),
                card.protocol.clone(),
                card.requires_key,
                card.can_list_models,
                card.available,
                card.catalog_models,
                card.has_key,
                card.id == "antigravity-acp" && card.auth.contains("third-party ToS risk"),
                query.clone(),
                *selected,
            ),
            None => return,
        }
    };
    if !available {
        state.notice = Some((
            format!(
                "{provider_id} is catalog-only — its {protocol} runtime adapter is not installed"
            ),
            state.tick + 40,
        ));
        return;
    }
    if requires_community_consent {
        state.overlay = Overlay::ConfirmCommunityAcpInstall {
            provider_id,
            query,
            selected,
            onboard_class: None,
        };
        return;
    }
    enter_add_model_flow(
        state,
        provider_id,
        protocol,
        requires_key,
        can_list_models,
        catalog_models,
        has_key,
    );
}

/// Fold a fetched provider model list into the in-flight query overlay
/// (model-discovery). Moves the stashed `api_key` from `AddModelQuerying` into
/// the pick-list so the round-trip `Action` never carries the key. If the
/// overlay is no longer the matching `AddModelQuerying` (the user dismissed or
/// opened something else, or this is a stale result for another provider), the
/// result is ignored — the race guard.
///
/// A second delivery for a pick-list that is already open (the cached seed's
/// live refresh, or a manual `Ctrl-R`) replaces the rows in place, keeping the
/// operator's filter text and clamping the selection — losing their typing
/// mid-browse would be worse than a stale row.
fn on_provider_models_loaded(
    state: &mut AppState,
    provider_id: String,
    models: Vec<AddModelRow>,
    origin: ModelListOrigin,
) {
    // Already browsing this provider: fold the fresher list in underneath the
    // filter rather than reopening the overlay from scratch.
    if let Overlay::AddModelPick {
        provider_id: pid,
        models: rows,
        query,
        selected,
        origin: current,
        refreshing,
        ..
    } = &mut state.overlay
    {
        if *pid != provider_id {
            return;
        }
        *rows = models;
        *current = origin;
        *refreshing = false;
        let matches = filter_model_names(rows, query).len();
        *selected = (*selected).min(matches.saturating_sub(1));
        return;
    }
    let matched = matches!(
        &state.overlay,
        Overlay::AddModelQuerying { provider_id: pid, .. } if *pid == provider_id
    );
    if !matched {
        return;
    }
    if let Overlay::AddModelQuerying {
        provider_id: pid,
        api_key,
    } = std::mem::replace(&mut state.overlay, Overlay::None)
    {
        state.overlay = Overlay::AddModelPick {
            provider_id: pid,
            api_key,
            models,
            query: String::new(),
            selected: 0,
            origin,
            refreshing: false,
        };
    }
}

/// Fold a failed model-list query into the free-text fallback (model-discovery):
/// move the stashed `api_key` from `AddModelQuerying` into `AddModelId` so a
/// hosted provider is never asked for its key twice, and surface a key-free
/// notice. Ignored (race guard) if the overlay no longer matches.
///
/// `requires_key` is derived from the provider's own catalog card, not from
/// whether this particular query happened to carry a key: a hosted provider
/// queried with a blank key still requires one on the free-text fallback, so
/// the flow re-prompts for it instead of silently adding a keyless model that
/// can only fail later at run time. When the card is missing entirely (the
/// projection has not loaded), the fallback ASSUMES a key is needed rather
/// than inferring it from the query — an extra prompt the operator can dismiss
/// with `Enter` is strictly better than a keyless model that 401s at run time.
fn on_provider_models_failed(state: &mut AppState, provider_id: String, reason: String) {
    // A failed manual refresh leaves the pick-list standing: the rows on
    // screen are still usable, so only the notice changes.
    if let Overlay::AddModelPick {
        provider_id: pid,
        refreshing,
        ..
    } = &mut state.overlay
    {
        if *pid == provider_id {
            *refreshing = false;
            state.notice = Some((format!("refresh failed ({reason})"), state.tick + 25));
        }
        return;
    }
    let matched = matches!(
        &state.overlay,
        Overlay::AddModelQuerying { provider_id: pid, .. } if *pid == provider_id
    );
    if !matched {
        return;
    }
    let is_acp_supplier = state
        .providers
        .iter()
        .any(|card| card.id == provider_id && card.protocol == "acp")
        || state
            .models
            .iter()
            .any(|card| card.acp_supplier() == Some(provider_id.as_str()));
    if let Overlay::AddModelQuerying {
        provider_id: pid,
        api_key,
    } = std::mem::replace(&mut state.overlay, Overlay::None)
    {
        // ACP model ids are agent-owned and cannot be guessed honestly. Keep
        // a failed supplier handshake on a retryable catalogue surface instead
        // of falling through to the generic free-text model-name prompt.
        if is_acp_supplier {
            state.notice = Some((
                format!("{provider_id} model discovery failed ({reason}); Ctrl-R retries"),
                state.tick + 40,
            ));
            state.overlay = Overlay::AddModelPick {
                provider_id: pid,
                api_key,
                models: Vec::new(),
                query: String::new(),
                selected: 0,
                origin: ModelListOrigin::Catalog(format!("connection failed · {reason}")),
                refreshing: false,
            };
            return;
        }
        let requires_key = state
            .providers
            .iter()
            .find(|c| c.id == provider_id)
            .is_none_or(|c| c.requires_key);
        state.notice = Some((
            format!("couldn't fetch models ({reason}); type the model name"),
            state.tick + 25,
        ));
        state.overlay = Overlay::AddModelId {
            provider_id: pid,
            requires_key,
            api_key,
            buffer: String::new(),
        };
    }
}

/// Open the "Local models: browse Unsloth catalog" overlay (palette entry)
/// and kick off the repo listing. Always starts loading — a fresh browse
/// never shows stale results from a prior session.
fn open_unsloth_catalog(state: &mut AppState) {
    state.overlay = Overlay::UnslothRepos {
        repos: Vec::new(),
        query: String::new(),
        selected: 0,
        loading: true,
    };
    state.outbox.push(Intent::ListUnslothRepos);
}

/// Fold the fetched Unsloth repo listing into the in-flight
/// `Overlay::UnslothRepos { loading: true, .. }`. Ignored (race guard) if the
/// operator closed the overlay before the fetch returned.
fn on_unsloth_repos_loaded(state: &mut AppState, repos: Vec<UnslothRepoCard>) {
    if !matches!(state.overlay, Overlay::UnslothRepos { loading: true, .. }) {
        return;
    }
    state.overlay = Overlay::UnslothRepos {
        repos,
        query: String::new(),
        selected: 0,
        loading: false,
    };
}

/// The repo listing failed: close the overlay (this flow has no free-text
/// fallback to offer, unlike model-discovery) and surface a notice. Ignored
/// if the overlay no longer matches.
fn on_unsloth_repos_failed(state: &mut AppState, reason: String) {
    if !matches!(state.overlay, Overlay::UnslothRepos { loading: true, .. }) {
        return;
    }
    state.overlay = Overlay::None;
    state.notice = Some((
        format!("could not browse the Unsloth catalog: {reason}"),
        state.tick + 40,
    ));
}

/// Fold the fetched quant-variant listing into the in-flight
/// `Overlay::UnslothQuants { loading: true, .. }`, guarded by `repo_id` so a
/// stale reply for a repo the operator already navigated away from is
/// dropped (mirrors [`on_provider_models_loaded`]'s `provider_id` guard).
fn on_unsloth_quants_loaded(state: &mut AppState, repo_id: String, quants: Vec<UnslothQuantCard>) {
    let matched = matches!(
        &state.overlay,
        Overlay::UnslothQuants { repo_id: current, loading: true, .. } if *current == repo_id
    );
    if !matched {
        return;
    }
    state.overlay = Overlay::UnslothQuants {
        repo_id,
        quants,
        query: String::new(),
        selected: 0,
        loading: false,
    };
}

/// The quant listing failed: close the overlay and surface a notice, guarded
/// by `repo_id` exactly like [`on_unsloth_quants_loaded`].
fn on_unsloth_quants_failed(state: &mut AppState, repo_id: String, reason: String) {
    let matched = matches!(
        &state.overlay,
        Overlay::UnslothQuants { repo_id: current, loading: true, .. } if *current == repo_id
    );
    if !matched {
        return;
    }
    state.overlay = Overlay::None;
    state.notice = Some((
        format!("could not list quants for {repo_id}: {reason}"),
        state.tick + 40,
    ));
}

/// Append one parsed `ollama pull` progress line to the in-flight
/// `Overlay::UnslothPulling`, guarded by `repo_id`+`quant` so a line from a
/// pull the operator already dismissed (it keeps running detached — see the
/// overlay's doc comment) is dropped rather than corrupting an unrelated
/// view. A no-op once `done` (the terminal signal already arrived).
fn on_unsloth_pull_progress(state: &mut AppState, repo_id: String, quant: String, line: String) {
    if let Overlay::UnslothPulling {
        repo_id: current_repo,
        quant: current_quant,
        lines,
        done,
        ..
    } = &mut state.overlay
    {
        if !*done && *current_repo == repo_id && *current_quant == quant {
            lines.push(line);
        }
    }
}

/// Fold the terminal pull outcome into the in-flight `Overlay::UnslothPulling`,
/// guarded exactly like [`on_unsloth_pull_progress`]. Sets exactly one of
/// `error`/`registered_id` and flips `done` — the render layer reads those to
/// show either the registered-model notice or the failure.
fn on_unsloth_pull_finished(
    state: &mut AppState,
    repo_id: String,
    quant: String,
    result: Result<String, String>,
) {
    if let Overlay::UnslothPulling {
        repo_id: current_repo,
        quant: current_quant,
        done,
        error,
        registered_id,
        ..
    } = &mut state.overlay
    {
        if *current_repo == repo_id && *current_quant == quant {
            *done = true;
            match result {
                Ok(id) => *registered_id = Some(id),
                Err(reason) => *error = Some(reason),
            }
        }
    }
}

/// Run a command chosen from the palette. Each maps onto the same effect its
/// single-key binding produces — the palette is a front door to the existing
/// commands, not a second code path. The palette overlay is already closed when
/// this runs, so a command that opens its own overlay simply sets it.
/// Recognize the natural slash-command form while the palette owns the input.
/// `Some(None)` means "all latest results"; `Some(Some(_))` is a direct
/// council-name/result-id lookup. No workflow or Blackboard fallback exists.
fn parse_council_result_query(query: &str) -> Option<Option<String>> {
    let mut words = query.trim().trim_start_matches('/').split_whitespace();
    if !words
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("council"))
        || !words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("result"))
    {
        return None;
    }
    Some(words.next().map(str::to_owned))
}

fn run_palette_command(state: &mut AppState, command: crate::palette::PaletteCommand) {
    use crate::palette::PaletteCommand;
    match command {
        PaletteCommand::Sessions => {
            state.overlay = Overlay::SessionPicker {
                query: String::new(),
                selected: 0,
            };
            state.outbox.push(Intent::ListSessions);
        }
        PaletteCommand::SessionLibrary => open_session_library(state),
        PaletteCommand::Issues => state.overlay = Overlay::Issues,
        PaletteCommand::NewRun => state.overlay = Overlay::NewRun(String::new()),
        PaletteCommand::Context => state.overlay = Overlay::Context,
        PaletteCommand::Steer => begin_steering(state),
        PaletteCommand::PauseResume => pause_or_resume(state),
        PaletteCommand::Cancel => request_cancel(state),
        PaletteCommand::Skills => {
            state.overlay = Overlay::Skills;
            request_projection(state, ProjectionKind::Skills);
        }
        PaletteCommand::Memory => {
            state.overlay = Overlay::Memory { source_open: false };
            request_projection(state, ProjectionKind::Memory);
        }
        PaletteCommand::Journey => {
            state.overlay = Overlay::Journey;
            request_projection(state, ProjectionKind::Journey);
        }
        PaletteCommand::Docs => {
            state.overlay = Overlay::Docs;
            request_projection(state, ProjectionKind::Docs);
            watch_focused_doc(state);
        }
        PaletteCommand::Edges => {
            state.overlay = Overlay::Edges;
            request_edge_page(state, state.edge_page);
        }
        PaletteCommand::Workflow => {
            state.overlay = Overlay::Workflow;
            request_projection(state, ProjectionKind::Workflow);
            watch_focused_workflow(state);
        }
        PaletteCommand::Blackboard => {
            state.overlay = Overlay::Blackboard;
            watch_focused_blackboard_run(state);
        }
        PaletteCommand::Kanban => open_kanban(state),
        PaletteCommand::UiPlugins => open_ui_plugins(state),
        PaletteCommand::Model => {
            state.selected_model = 0;
            state.overlay = Overlay::ModelPicker {
                query: String::new(),
                selected: 0,
            };
        }
        PaletteCommand::Provider => {
            state.selected_provider = 0;
            state.overlay = Overlay::ProviderPicker {
                query: String::new(),
                selected: 0,
            };
        }
        // PR C2: open the mode picker with the cursor pre-selected on the
        // CURRENT default, so the picker's starting point reflects what the
        // next run would use.
        PaletteCommand::Mode => {
            state.overlay = Overlay::ModePicker {
                query: String::new(),
                selected: crate::state::MODE_CARDS
                    .iter()
                    .position(|card| card.mode == state.default_mode)
                    .unwrap_or(0),
            };
        }
        // Open on the theme in force, so the first thing the cursor previews is
        // what is already on screen.
        PaletteCommand::Theme => {
            state.overlay = Overlay::ThemePicker {
                query: String::new(),
                selected: state.theme_selected.unwrap_or(0),
            };
        }
        // D1: open the `/keys` overlay (rows come from `state.models` +
        // `state.key_status`, already seeded by the harness).
        PaletteCommand::ApiKeys => {
            state.overlay = Overlay::ApiKeys {
                query: String::new(),
                selected: 0,
            };
        }
        // Rubric 6 TUI wiring: `/council` opens the browser (list/run/delete),
        // matching every other workspace command's list-first shape; `n`
        // reaches the creation wizard from inside it.
        PaletteCommand::Council => {
            state.overlay = Overlay::CouncilBrowser;
        }
        PaletteCommand::CouncilResults => {
            state.overlay = Overlay::CouncilResults;
            state
                .outbox
                .push(Intent::LoadCouncilResults { selector: None });
        }
        PaletteCommand::UnslothCatalog => open_unsloth_catalog(state),
        // Voice v1 (rubric 8). The toggle only flips the flag the CLI's voice
        // host reads — the host owns the synthesis and playback subprocesses,
        // and reports back (as a `Notice`) when speech is not configured, so
        // the TUI never has to know what a provider or an audio device is.
        PaletteCommand::VoiceSpeak => {
            state.voice.speak_replies = !state.voice.speak_replies;
            let text = if state.voice.speak_replies {
                "speaking replies aloud"
            } else {
                "stopped speaking replies"
            };
            state.notice = Some((text.to_owned(), state.tick + 25));
        }
        PaletteCommand::ToggleLayout => {
            state.layout = state.layout.toggled();
            if matches!(state.layout, crate::state::LayoutMode::Workspace) {
                state.focus = Pane::Transcript;
            }
        }
        PaletteCommand::Help => {
            state.help_scroll = 0;
            state.overlay = Overlay::Help;
        }
        PaletteCommand::Detach => state.should_detach = true,
        PaletteCommand::NewConversation => {
            release_doc_lease(state);
            state.outbox.push(Intent::NewConversation);
            state.notice = Some(("creating a fresh conversation…".to_owned(), state.tick + 40));
        }
    }
}

/// Ask the harness to seed and subscribe the focused document. The intent is a
/// no-op when the Docs projection is empty and is idempotent in the harness.
fn watch_focused_doc(state: &mut AppState) {
    if let Some(document_id) = state.focused_doc().map(|doc| doc.document_id) {
        state.outbox.push(Intent::WatchDocument { document_id });
    }
}

fn request_projection(state: &mut AppState, kind: ProjectionKind) {
    state.outbox.push(Intent::RefreshProjection { kind });
}

fn refresh_open_projection(state: &mut AppState) {
    if matches!(state.overlay, Overlay::UiPlugins) {
        state.outbox.push(Intent::ListUiPlugins);
        return;
    }
    let kind = match state.overlay {
        Overlay::Skills => Some(ProjectionKind::Skills),
        Overlay::Memory { .. } => Some(ProjectionKind::Memory),
        Overlay::Journey | Overlay::LearningEdit { .. } | Overlay::ConfirmLearningDelete { .. } => {
            Some(ProjectionKind::Journey)
        }
        Overlay::Docs
        | Overlay::DocEdit { .. }
        | Overlay::DocNew { .. }
        | Overlay::DocInsert { .. }
        | Overlay::DocDeleteConfirm { .. }
        | Overlay::DocPublishTarget { .. }
        | Overlay::DocPublishPath { .. }
        | Overlay::DocPublishBranch { .. }
        | Overlay::DocPublishTitle { .. } => Some(ProjectionKind::Docs),
        Overlay::Workflow | Overlay::WorkflowInputs { .. } => Some(ProjectionKind::Workflow),
        _ => None,
    };
    if let Some(kind) = kind {
        request_projection(state, kind);
    }
}

fn watch_focused_workflow(state: &mut AppState) {
    if let Some(workflow_run_id) = state
        .focused_node()
        .and_then(|card| card.workflow_run_id.clone())
    {
        state.outbox.push(Intent::WatchWorkflow { workflow_run_id });
    }
}

/// Open (or close) the repository task board, subscribing to the board's live
/// channel and reading its baseline on the way in (rubric 10). Closing does not
/// unsubscribe — the board is cheap and staying attached means reopening it is
/// already current, exactly as the workflow panes behave.
fn open_kanban(state: &mut AppState) {
    if matches!(state.overlay, Overlay::Kanban) {
        state.overlay = Overlay::None;
        return;
    }
    state.overlay = Overlay::Kanban;
    state.outbox.push(Intent::WatchBoard);
}

/// Move the focused card `delta` columns and emit the daemon write.
///
/// The pane does NOT mutate its own card: the daemon applies the move as a
/// supersession and publishes the replacement on the board channel, which merges
/// back in by id. So the rendered board always shows what is actually stored — a
/// refused move (a concurrent supersede) simply never appears, instead of leaving
/// the pane lying about where the card is.
fn move_focused_card(state: &mut AppState, delta: i32) {
    // The horizontal arrows are global in `Normal` mode, so this MUST check the
    // open overlay: without it, pressing → while reading the blackboard or the
    // workflow graph would silently move a board card the operator cannot see.
    if !matches!(state.overlay, Overlay::Kanban) {
        return;
    }
    let Some(card) = state.focused_card() else {
        return;
    };
    let current = crate::state::KANBAN_COLUMNS
        .iter()
        .position(|column| card.status.eq_ignore_ascii_case(column))
        // A card in a team's own column moves from the first column, matching
        // where `kanban_columns` renders it.
        .unwrap_or(0);
    let target = current.saturating_add_signed(delta as isize);
    let Some(status) = crate::state::KANBAN_COLUMNS.get(target) else {
        // Off either end of the board: nothing to do, and no command sent.
        return;
    };
    if target == current {
        return;
    }
    let item_id = card.id.clone();
    state.outbox.push(Intent::MoveBoardCard {
        item_id,
        status: (*status).to_string(),
    });
}

fn watch_focused_blackboard_run(state: &mut AppState) {
    let workflow_run_id = state
        .focused_item()
        .map(|item| item.workflow_run_id.clone())
        .or_else(|| {
            state
                .focused_node()
                .and_then(|card| card.workflow_run_id.clone())
        });
    if let Some(workflow_run_id) = workflow_run_id {
        state.outbox.push(Intent::WatchWorkflow { workflow_run_id });
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
        *index = index.saturating_sub(delta.unsigned_abs() as usize);
    } else {
        *index = index.saturating_add(delta as usize).min(max);
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
        ProposedAction::PublishDocument { target, .. } => format!("GitCommit ({target})"),
        ProposedAction::McpToolCall { server, tool, .. } => {
            format!("McpToolCall ({server}.{tool})")
        }
        ProposedAction::CouncilCreate { name, .. } => format!("CouncilCreate ({name})"),
        ProposedAction::CouncilRun { name, .. } => format!("CouncilRun ({name})"),
        ProposedAction::WorkflowCreate { workflow_id, .. } => {
            format!("WorkflowCreate ({workflow_id})")
        }
        ProposedAction::WorkflowRun { workflow_id, .. } => {
            format!("WorkflowRun ({workflow_id})")
        }
        _ => "unsupported capability".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::VoiceKeyRow;
    use chrono::Utc;
    use codypendent_protocol::{
        AgentMode, ApprovalId, ArtifactId, ArtifactRef, ChangeSetId, DataClassification, ModelId,
        Risk, RiskLevel, RunId, ToolOutcome, UiActionId, UiContributionId, UiContributionPoint,
        UiContributionRegistration, UiDocument, UiExtensionId, UiNode, UiPrimitive, UiSemanticRole,
        UiSlotId,
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

    fn mount_focus_document(state: &mut AppState, document_id: &str, nodes: &[&str]) {
        let id = UiDocumentId::from(document_id);
        let document = UiDocument {
            protocol_version: UiProtocolVersion::V1,
            document_id: id.clone(),
            revision: UiRevision(1),
            root: UiNode::element("root", UiPrimitive::from("Stack")),
            capabilities: None,
            metadata: Default::default(),
            compatibility: None,
        };
        let mut snapshot = empty_message("snapshot", format!("{document_id}-snapshot"));
        snapshot.snapshot = Some(codypendent_protocol::UiSnapshot {
            document,
            reason: None,
        });
        reduce(state, Action::RemoteUiMessage(Box::new(snapshot)));

        let mut contributions =
            empty_message("contributions", format!("{document_id}-contribution"));
        contributions
            .contributions
            .push(UiContributionRegistration {
                id: UiContributionId::from(format!("{document_id}-registration")),
                extension_id: UiExtensionId::from(format!("{document_id}-extension")),
                point: UiContributionPoint::from("panel"),
                slot: UiSlotId::from("panel"),
                document_id: id.clone(),
                priority: 0,
                when: None,
                requires: Vec::new(),
                metadata: Default::default(),
            });
        reduce(state, Action::RemoteUiMessage(Box::new(contributions)));

        let output = RemoteUiRenderOutput {
            focus_order: nodes
                .iter()
                .enumerate()
                .map(|(index, node)| crate::remote_ui::FocusDescriptor {
                    node_id: UiNodeId::from(*node),
                    area: ratatui::layout::Rect::new(0, index as u16, 10, 1),
                    order: index as i32,
                    role: UiSemanticRole::from("button"),
                    label: (*node).to_owned(),
                    keyboard_hint: Some("Enter".to_owned()),
                    disabled: false,
                    keyboard_actions: Vec::new(),
                })
                .collect(),
            ..RemoteUiRenderOutput::default()
        };
        state.remote_ui.last_render.borrow_mut().insert(id, output);
    }

    #[test]
    fn remote_focus_traverses_nodes_across_documents_without_activating() {
        let mut state = AppState::new();
        reduce(
            &mut state,
            Action::RemoteUiMessage(Box::new(
                crate::remote_ui_host::terminal_capabilities_message(80, 24, 24),
            )),
        );
        mount_focus_document(&mut state, "alpha", &["alpha-one", "alpha-two"]);
        mount_focus_document(&mut state, "beta", &["beta-one"]);
        let outbox_before = state.outbox.len();

        reduce(&mut state, Action::RemoteUiSetActive(true));
        assert_eq!(
            state
                .remote_ui
                .focused_document
                .as_ref()
                .map(UiDocumentId::as_str),
            Some("alpha")
        );
        assert_eq!(
            state
                .remote_ui
                .view
                .focused_node
                .as_ref()
                .map(UiNodeId::as_str),
            Some("alpha-one")
        );
        assert_eq!(state.outbox.len(), outbox_before, "focus is not activation");

        reduce(
            &mut state,
            Action::RemoteUiKey {
                key: RemoteKey::Tab,
                character: None,
            },
        );
        assert_eq!(
            state
                .remote_ui
                .view
                .focused_node
                .as_ref()
                .map(UiNodeId::as_str),
            Some("alpha-two")
        );
        reduce(
            &mut state,
            Action::RemoteUiKey {
                key: RemoteKey::Tab,
                character: None,
            },
        );
        assert_eq!(
            state
                .remote_ui
                .focused_document
                .as_ref()
                .map(UiDocumentId::as_str),
            Some("beta")
        );
        assert_eq!(
            state
                .remote_ui
                .view
                .focused_node
                .as_ref()
                .map(UiNodeId::as_str),
            Some("beta-one")
        );

        reduce(
            &mut state,
            Action::RemoteUiKey {
                key: RemoteKey::ShiftTab,
                character: None,
            },
        );
        assert_eq!(
            state
                .remote_ui
                .focused_document
                .as_ref()
                .map(UiDocumentId::as_str),
            Some("alpha")
        );
        assert_eq!(
            state
                .remote_ui
                .view
                .focused_node
                .as_ref()
                .map(UiNodeId::as_str),
            Some("alpha-two")
        );
        assert_eq!(
            state.outbox.len(),
            outbox_before,
            "traversal is not activation"
        );
    }

    #[test]
    fn route_wide_recoverable_remote_error_resyncs_every_document() {
        let mut state = AppState::new();
        mount_focus_document(&mut state, "alpha", &["a"]);
        mount_focus_document(&mut state, "beta", &["b"]);
        let _ = state.drain_outbox();

        let mut error = empty_message("error", "route-lag");
        error.error = Some(codypendent_protocol::UiRemoteError {
            code: "ui.transport.lagged".to_owned(),
            message: "renderer fell behind".to_owned(),
            recoverable: true,
            document_id: None,
            node_id: None,
            patch_index: None,
            recovery: Some("resync".to_owned()),
            fallback: None,
            details: serde_json::Value::Null,
        });
        reduce(&mut state, Action::RemoteUiMessage(Box::new(error)));

        let mut documents: Vec<_> = state
            .drain_outbox()
            .into_iter()
            .filter_map(|intent| match intent {
                Intent::RemoteUiMessage(message) => message
                    .resync
                    .map(|request| request.document_id.to_string()),
                _ => None,
            })
            .collect();
        documents.sort();
        assert_eq!(documents, vec!["alpha", "beta"]);
    }

    #[test]
    fn shift_f6_cycles_mounted_documents_and_escape_returns_to_composer() {
        let mut state = AppState::new();
        reduce(
            &mut state,
            Action::RemoteUiMessage(Box::new(
                crate::remote_ui_host::terminal_capabilities_message(80, 24, 24),
            )),
        );
        mount_focus_document(&mut state, "alpha", &["alpha-one"]);
        mount_focus_document(&mut state, "beta", &["beta-one"]);

        reduce(&mut state, Action::RemoteUiSetActive(true));
        reduce(&mut state, Action::RemoteUiNextDocument);
        assert_eq!(
            state
                .remote_ui
                .focused_document
                .as_ref()
                .map(UiDocumentId::as_str),
            Some("beta")
        );
        assert_eq!(
            state
                .remote_ui
                .view
                .focused_node
                .as_ref()
                .map(UiNodeId::as_str),
            Some("beta-one")
        );
        reduce(&mut state, Action::RemoteUiNextDocument);
        assert_eq!(
            state
                .remote_ui
                .focused_document
                .as_ref()
                .map(UiDocumentId::as_str),
            Some("alpha")
        );
        reduce(&mut state, Action::RemoteUiSetActive(false));
        assert!(!state.remote_ui.active);
    }

    #[test]
    fn remote_submit_scopes_form_data_to_the_owning_form() {
        use crate::remote_ui::FormFieldDescriptor;

        let mut state = AppState::new();
        reduce(
            &mut state,
            Action::RemoteUiMessage(Box::new(
                crate::remote_ui_host::terminal_capabilities_message(80, 24, 24),
            )),
        );
        let mut first = UiNode::element("form-a", UiPrimitive::from("Form"));
        first
            .children
            .push(UiNode::element("field-a", UiPrimitive::from("TextInput")));
        first
            .children
            .push(UiNode::element("submit-a", UiPrimitive::from("Button")));
        let mut second = UiNode::element("form-b", UiPrimitive::from("Form"));
        second
            .children
            .push(UiNode::element("field-b", UiPrimitive::from("TextInput")));
        let mut root = UiNode::element("root", UiPrimitive::from("Stack"));
        root.children = vec![first, second];
        let document = UiDocument {
            protocol_version: UiProtocolVersion::V1,
            document_id: UiDocumentId::from("forms"),
            revision: UiRevision(1),
            root,
            capabilities: None,
            metadata: Default::default(),
            compatibility: None,
        };
        let mut snapshot = empty_message("snapshot", "forms-snapshot");
        snapshot.snapshot = Some(codypendent_protocol::UiSnapshot {
            document,
            reason: None,
        });
        reduce(&mut state, Action::RemoteUiMessage(Box::new(snapshot)));
        state.remote_ui.focused_document = Some(UiDocumentId::from("forms"));
        let mut output = RemoteUiRenderOutput::default();
        for (node, name, value) in [("field-a", "first", "one"), ("field-b", "second", "two")] {
            output.form_fields.push(FormFieldDescriptor {
                node_id: UiNodeId::from(node),
                name: name.to_owned(),
                input_type: "TextInput".to_owned(),
                value: Value::String(value.to_owned()),
                required: false,
                read_only: false,
                disabled: false,
                validation_message: None,
            });
        }
        state
            .remote_ui
            .last_render
            .borrow_mut()
            .insert(UiDocumentId::from("forms"), output);
        emit_remote_ui_event(
            &mut state,
            UiDocumentId::from("forms"),
            UiRevision(1),
            UiNodeId::from("submit-a"),
            UiActionBinding {
                event: UiEventType::from("submit"),
                action_id: UiActionId::from("save"),
                payload: serde_json::json!({"trusted": true}),
                requires: Vec::new(),
                disabled: false,
                confirmation: None,
            },
            None,
        );
        let event = match state.outbox.last().expect("event intent") {
            Intent::RemoteUiMessage(message) => message.event.as_ref().expect("event"),
            other => panic!("expected remote UI event, got {other:?}"),
        };
        assert_eq!(event.payload, serde_json::json!({"first": "one"}));
        assert!(event.payload.get("trusted").is_none());
    }

    #[test]
    fn sdk_handler_only_text_input_emits_revision_bound_change_event() {
        let mut state = AppState::new();
        reduce(
            &mut state,
            Action::RemoteUiMessage(Box::new(
                crate::remote_ui_host::terminal_capabilities_message(80, 24, 24),
            )),
        );
        let document: UiDocument = serde_json::from_value(serde_json::json!({
            "protocolVersion": {"major": 1, "minor": 0},
            "documentId": "stateful-input",
            "revision": 3,
            "root": {
                "kind": "element", "id": "query", "type": "TextInput",
                "props": {"name": "query", "value": "", "eventHandlers": ["change"]},
                "children": []
            }
        }))
        .expect("SDK-shaped input");
        let mut snapshot = empty_message("snapshot", "stateful-snapshot");
        snapshot.snapshot = Some(codypendent_protocol::UiSnapshot {
            document,
            reason: None,
        });
        reduce(&mut state, Action::RemoteUiMessage(Box::new(snapshot)));
        let document_id = UiDocumentId::from("stateful-input");
        let output = {
            let document = state
                .remote_ui
                .host
                .documents()
                .document(&document_id)
                .expect("mounted document");
            let area = ratatui::layout::Rect::new(0, 0, 40, 4);
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            crate::render_remote_ui(
                &mut buffer,
                area,
                document,
                &crate::Theme::dark(),
                &state.remote_ui.capabilities,
                &state.remote_ui.view,
                crate::RemoteUiRenderOptions::default(),
            )
        };
        state.remote_ui.focused_document = Some(document_id.clone());
        state.remote_ui.view.focused_node = Some(UiNodeId::from("query"));
        state
            .remote_ui
            .last_render
            .borrow_mut()
            .insert(document_id, output);
        reduce(
            &mut state,
            Action::RemoteUiKey {
                key: RemoteKey::Character,
                character: Some('x'),
            },
        );
        let event = match state.outbox.last().expect("change event") {
            Intent::RemoteUiMessage(message) => message.event.as_ref().expect("event"),
            other => panic!("expected remote UI event, got {other:?}"),
        };
        assert_eq!(event.revision, UiRevision(3));
        assert_eq!(event.target_id.as_str(), "query");
        assert_eq!(event.event_type.as_str(), "change");
        assert_eq!(event.payload, serde_json::json!({"value": "x"}));
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

    #[test]
    fn post_terminal_learning_capture_is_kept_quiet_and_refreshes_open_journey() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "remember this preference".into(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Completed { summary: None },
                chronicle: artifact(),
            }),
        );
        let transcript_len = s.runs[0].transcript.len();
        s.overlay = Overlay::Journey;
        reduce(
            &mut s,
            system_ev(EventBody::LearningsCaptured {
                run_id,
                proposed_count: 1,
                proposed_ids: vec![codypendent_protocol::LearningId::new()],
                activated_count: 0,
                activated_ids: Vec::new(),
            }),
        );
        assert_eq!(s.pending_learning_review, 1);
        assert_eq!(
            s.runs[0].transcript.len(),
            transcript_len,
            "capture stays out of chat"
        );
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::RefreshProjection {
                kind: ProjectionKind::Journey
            }]
        );
    }

    #[test]
    fn journey_review_actions_are_optimistic_and_typed() {
        let mut s = AppState::new();
        s.learnings.push(crate::state::LearningCard {
            id: codypendent_protocol::LearningId::new().to_string(),
            statement: "Prefer focused tests".into(),
            kind: "fact".into(),
            state: "proposed".into(),
            scope: "user".into(),
            provenance: "user-confirmed".into(),
            confidence: 0.95,
            pinned: false,
            revision: 3,
        });
        s.overlay = Overlay::Journey;
        reduce(&mut s, Action::Approve(ApprovalScope::Once));
        assert!(matches!(
            s.drain_outbox().as_slice(),
            [Intent::MutateLearning {
                revision: 3,
                mutation: LearningMutation::Activate,
                ..
            }]
        ));
    }

    #[test]
    fn run_started_pushes_a_user_turn_with_the_objective() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "add a test".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        assert!(matches!(
            &s.runs[0].transcript[0],
            TranscriptEntry::User { text } if text == "add a test"
        ));
    }

    /// C13: every transcript-pushing reducer arm routes through `push_entry`,
    /// so a run's transcript is bounded by `MAX_TRANSCRIPT_ENTRIES` regardless
    /// of how many events arrive. The arms the fix converted from a direct
    /// `transcript.push` — tool-started, tool-completed, steering, budget — are
    /// each flooded past the cap here; a regression to a direct push (skipping
    /// the trim) would let the transcript grow without bound.
    #[test]
    fn transcript_entries_respect_the_cap_in_every_formerly_direct_arm() {
        let cap = crate::state::MAX_TRANSCRIPT_ENTRIES;
        let over = cap + 37;

        // Flood one arm with `over` events that each push a fresh transcript
        // entry, and return the resulting transcript length.
        let flood = |make: &dyn Fn(RunId, usize) -> Action| -> usize {
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
            for i in 0..over {
                reduce(&mut s, make(run_id, i));
            }
            s.runs
                .iter()
                .find(|r| r.run_id == run_id)
                .unwrap()
                .transcript
                .len()
        };

        // tool-started with no preceding proposed card → the None (push) branch.
        let tool_started = flood(&|run_id, i| {
            ev(
                agent_actor(run_id),
                EventBody::ToolStarted {
                    run_id,
                    tool: format!("tool.{i}"),
                    args_digest: format!("d{i}"),
                    label: None,
                },
            )
        });
        // tool-completed with no non-completed card → the None (push) branch.
        let tool_completed = flood(&|run_id, i| {
            ev(
                agent_actor(run_id),
                EventBody::ToolCompleted {
                    run_id,
                    tool: format!("tool.{i}"),
                    outcome: ToolOutcome::Succeeded,
                    artifact: None,
                },
            )
        });
        // steering queued → a fresh (unapplied) Steering entry each time.
        let steering =
            flood(&|run_id, _i| ev(agent_actor(run_id), EventBody::SteeringQueued { run_id }));
        // budget warning → a fresh Budget entry each time.
        let budget = flood(&|run_id, i| {
            ev(
                agent_actor(run_id),
                EventBody::BudgetWarning {
                    run_id,
                    dimension: BudgetDimension::Cost,
                    used: i as u64,
                    limit: 100,
                },
            )
        });

        for (arm, len) in [
            ("tool-started", tool_started),
            ("tool-completed", tool_completed),
            ("steering", steering),
            ("budget", budget),
        ] {
            assert_eq!(len, cap, "{arm}: transcript must be trimmed to the cap");
        }
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
    fn a_long_note_folds_by_default_and_expand_toggles_it() {
        // Mirrors the ToolCard/Patch fold pattern (Chapter 07 transcript
        // declutter fix): a NoteAppended folds into Note{expanded:false}
        // regardless of length, and the same Action::Expand that toggles a
        // tool card or patch also toggles a selected note.
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
        let long_text = "line one\nline two\nline three\nline four".to_owned();
        reduce(
            &mut s,
            system_ev(EventBody::NoteAppended {
                text: long_text.clone(),
                run_id: Some(run_id),
            }),
        );
        // transcript[0] is the User turn RunStarted pushes for the objective;
        // the note folds in right after it.
        let TranscriptEntry::Note { text, expanded } = &s.runs[0].transcript[1] else {
            unreachable!("NoteAppended must fold into a Note entry")
        };
        assert_eq!(text, &long_text);
        assert!(!expanded, "a note starts folded, same as a fresh tool card");

        s.focus = Pane::Transcript;
        s.runs[0].transcript_selected = 1;
        reduce(&mut s, Action::Expand);
        let TranscriptEntry::Note { expanded, .. } = &s.runs[0].transcript[1] else {
            unreachable!()
        };
        assert!(*expanded, "Expand toggles a selected note's expanded state");

        reduce(&mut s, Action::Expand);
        let TranscriptEntry::Note { expanded, .. } = &s.runs[0].transcript[1] else {
            unreachable!()
        };
        assert!(!*expanded, "Expand toggles it back off");
    }

    #[test]
    fn a_short_note_folds_the_same_way_as_a_long_one() {
        // `reduce` does not special-case note length — every NoteAppended folds
        // into Note{expanded:false} identically. "A short note stays inline" is
        // purely a render-layer decision (see render.rs's note_lines), not a
        // different shape here; Expand still flips this note's state too.
        // (Not a `remembered:`/`=== CONTEXT` note — those fold into
        // `Backstage` instead, covered by the backstage-fold tests below.)
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
            system_ev(EventBody::NoteAppended {
                text: "the test command is cargo test".to_owned(),
                run_id: Some(run_id),
            }),
        );
        // transcript[0] is the User turn RunStarted pushes for the objective;
        // the note folds in right after it.
        let TranscriptEntry::Note { expanded, .. } = &s.runs[0].transcript[1] else {
            unreachable!("NoteAppended must fold into a Note entry")
        };
        assert!(!expanded, "every note starts unexpanded, short or long");

        s.focus = Pane::Transcript;
        s.runs[0].transcript_selected = 1;
        reduce(&mut s, Action::Expand);
        let TranscriptEntry::Note { expanded, .. } = &s.runs[0].transcript[1] else {
            unreachable!()
        };
        assert!(*expanded, "Expand flips it regardless of length");
    }

    /// Reasoning is not speech. ACP separates `AgentThoughtChunk` from
    /// `AgentMessageChunk` and the daemon now carries that through as
    /// `thought`; before v0.12.2 both merged into the model tail, so a model
    /// that deliberates out loud printed the deliberation as its answer.
    ///
    /// Flip the reducer back to `append_model_text` for thought chunks and this
    /// fails: the reply entry would carry the reasoning text too.
    #[test]
    fn reasoning_chunks_fold_into_their_own_entry_and_never_the_reply() {
        let mut s = AppState::default();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "go".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ModelStreamDelta {
                run_id,
                text: "the user said hello, I should be brief".to_owned(),
                thought: true,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ModelStreamDelta {
                run_id,
                text: "Hello!".to_owned(),
                thought: false,
            }),
        );

        let reasoning: Vec<&String> = s.runs[0]
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Reasoning { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning,
            vec!["the user said hello, I should be brief"],
            "reasoning must land in its own entry"
        );

        let speech: Vec<&String> = s.runs[0]
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Model { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            speech,
            vec!["Hello!"],
            "the reply must carry only the reply"
        );

        // Folded by default: deliberation must never compete with the answer.
        assert!(s.runs[0].transcript.iter().any(|entry| matches!(
            entry,
            TranscriptEntry::Reasoning {
                expanded: false,
                ..
            }
        )));
    }

    /// A re-issued question with MORE sub-questions used to panic the TUI.
    ///
    /// `QuestionAsked` replaces a pending question in place, but the card that
    /// holds one answer slot per sub-question was only ever built when there
    /// was none — so it stayed sized for the previous shape and
    /// `custom_text[card.index]` indexed past the end. Daemon-triggerable, and
    /// it fired while a question was blocking the operator.
    /// `a`/`r` resolve the approval on screen, whatever overlay is open.
    ///
    /// `input_mode` returns `InputMode::Approval` before it considers any
    /// overlay, so a pending approval is what the operator is looking at. The
    /// reducer checked `Overlay::Journey` first — and only Journey — so with
    /// that overlay open, `a` activated a LEARNING while the approval stayed
    /// unresolved. Reorder them back and this fails.
    /// The approval modal and the question card are where the operator
    /// AUTHORISES things, and both render strings the model chose. Crossterm
    /// writes cell symbols verbatim, so a crafted argument or option label
    /// could emit OSC 52 to overwrite the clipboard, or reposition the cursor
    /// and repaint the dialog to describe something other than what is being
    /// approved. Model prose was sanitized at ingest; this evidence was not.
    /// A question is session-scoped, and switching sessions used to leave it
    /// behind. `input_mode` then returned `InputMode::Question` in the NEW
    /// session and captured the composer — and answering sent
    /// `ResolveQuestion` against the new session id, which the daemon rejects.
    /// The rejection clears nothing locally, so the card stayed and so did the
    /// capture: a wedge with no way out but restarting the TUI.
    /// A long session really does pass 65,535 transcript rows: one run holds up
    /// to `MAX_TRANSCRIPT_ENTRIES` (2000) entries and a single model entry may
    /// reach `MAX_MODEL_ENTRY_BYTES` (256 KiB) — around 3,300 wrapped rows on
    /// its own. At `u16` the row counter saturated there, so follow mode pinned
    /// to a bottom that was not the bottom and every row past it was
    /// unreachable — which on screen reads as a hung run.
    /// An arming that has outlived its own notice is not an arming.
    ///
    /// Arming a confirmed remote-UI action showed a notice that faded after
    /// about two seconds, while `pending_confirmation` itself lived forever —
    /// so a stray Enter on the same control an hour later executed a confirmed
    /// action with nothing on screen saying it had been armed. The state and
    /// the signal expire together now.
    #[test]
    fn an_armed_remote_ui_confirmation_expires_with_its_notice() {
        let mut s = AppState::default();
        let key = (
            codypendent_protocol::remote_ui::UiDocumentId::new("doc".to_owned()),
            codypendent_protocol::remote_ui::UiRevision(1),
            codypendent_protocol::remote_ui::UiNodeId("node".to_owned()),
            codypendent_protocol::remote_ui::UiActionId("delete".to_owned()),
        );

        // Armed at tick 0, so it stands until tick 10.
        s.tick = 0;
        s.remote_ui.pending_confirmation = Some(key.clone());
        s.remote_ui.pending_confirmation_expires = CONFIRMATION_TICKS;
        assert!(
            s.tick <= s.remote_ui.pending_confirmation_expires,
            "an arming is live inside its window"
        );

        // An hour later the notice is long gone, and so is the arming.
        s.tick = CONFIRMATION_TICKS + 100_000;
        assert!(
            s.tick > s.remote_ui.pending_confirmation_expires,
            "an arming past its notice must not still count as armed"
        );
    }

    #[test]
    fn the_transcript_scroll_offset_does_not_saturate_at_a_u16() {
        let mut s = AppState::default();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "long".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        // A bottom past what a `u16` can hold.
        let deep: u32 = u32::from(u16::MAX) + 10_000;
        s.transcript_max_scroll.set(deep);
        let run = &mut s.runs[0];
        run.follow = false;
        run.scroll = 0;

        // Page down far enough to land past the old ceiling.
        // Two full `u16` pages, which together clear the old ceiling — the
        // point being that the OFFSET can now go past it, not how many key
        // presses it takes.
        scroll_transcript(&mut s, false, u16::MAX);
        scroll_transcript(&mut s, false, u16::MAX);
        let run = &s.runs[0];
        assert!(
            run.scroll > u32::from(u16::MAX),
            "the offset must be able to exceed 65,535, got {}",
            run.scroll
        );
        assert_eq!(run.scroll, deep, "and it reaches the real bottom");
        assert!(run.follow, "reaching the bottom re-enters follow mode");
    }

    #[test]
    fn beginning_a_new_session_does_not_carry_the_old_ones_question() {
        let mut s = AppState::default();
        reduce(
            &mut s,
            system_ev(EventBody::QuestionAsked {
                question_id: codypendent_protocol::QuestionId::new(),
                run_id: RunId::new(),
                questions: vec![codypendent_protocol::question::QuestionPrompt {
                    header: "Pick".to_owned(),
                    question: "Which one?".to_owned(),
                    options: vec![codypendent_protocol::question::QuestionOption {
                        label: "a".to_owned(),
                        description: String::new(),
                    }],
                    multiple: false,
                    custom: true,
                }],
            }),
        );
        assert_eq!(s.input_mode(), crate::state::InputMode::Question);

        s.begin_new_session();

        assert!(
            s.pending_questions.is_empty(),
            "the question belonged to the old session"
        );
        assert!(s.question_card_state.is_none(), "and so did its card");
        assert!(s.pending_prompts.is_empty(), "and its queued prompts");
        assert_ne!(
            s.input_mode(),
            crate::state::InputMode::Question,
            "the composer must not stay captured in a session with no question"
        );
    }

    #[test]
    fn approval_evidence_and_question_text_are_sanitized_at_ingest() {
        const HOSTILE: &str = "run\u{1b}]52;c;aGVsbG8=\u{7}\u{1b}[2Jspoof";

        let mut s = AppState::default();
        reduce(
            &mut s,
            system_ev(EventBody::ApprovalRequested {
                approval_id: codypendent_protocol::ApprovalId::new(),
                action: ProposedAction::ExecuteCommand {
                    program: HOSTILE.to_owned(),
                    args: vec![HOSTILE.to_owned()],
                    environment: vec![(HOSTILE.to_owned(), HOSTILE.to_owned())],
                    cwd: Some(HOSTILE.to_owned()),
                },
                risk: Risk {
                    level: RiskLevel::Medium,
                    reasons: vec!["runs a command".to_owned()],
                },
                pattern: None,
            }),
        );
        let pending = s.pending_approvals.first().expect("an approval is pending");
        let ProposedAction::ExecuteCommand {
            program,
            args,
            environment,
            cwd,
        } = &pending.action
        else {
            panic!("the fixture proposed a command");
        };
        for field in [program, &args[0], &environment[0].0, &environment[0].1] {
            assert!(
                !field.contains('\u{1b}') && !field.contains('\u{7}'),
                "approval evidence reached the modal with escapes intact: {field:?}"
            );
        }
        assert!(!cwd.as_deref().unwrap_or_default().contains('\u{1b}'));

        let mut s = AppState::default();
        reduce(
            &mut s,
            system_ev(EventBody::QuestionAsked {
                question_id: codypendent_protocol::QuestionId::new(),
                run_id: RunId::new(),
                questions: vec![codypendent_protocol::question::QuestionPrompt {
                    header: HOSTILE.to_owned(),
                    question: HOSTILE.to_owned(),
                    options: vec![codypendent_protocol::question::QuestionOption {
                        label: HOSTILE.to_owned(),
                        description: HOSTILE.to_owned(),
                    }],
                    multiple: false,
                    custom: true,
                }],
            }),
        );
        let prompt = &s.pending_questions.first().expect("a question").questions[0];
        for field in [
            &prompt.header,
            &prompt.question,
            &prompt.options[0].label,
            &prompt.options[0].description,
        ] {
            assert!(
                !field.contains('\u{1b}') && !field.contains('\u{7}'),
                "question text reached the card with escapes intact: {field:?}"
            );
        }
    }

    #[test]
    fn a_pending_approval_outranks_the_journey_overlay_for_approve_and_reject() {
        for (action, expected) in [
            (
                Action::Approve(ApprovalScope::Once),
                ApprovalDecision::Approve,
            ),
            (Action::Reject, ApprovalDecision::Reject),
        ] {
            let mut s = AppState::default();
            let approval_id = codypendent_protocol::ApprovalId::new();
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
                    pattern: None,
                }),
            );
            assert_eq!(
                s.input_mode(),
                crate::state::InputMode::Approval,
                "the approval modal is what is on screen"
            );

            // The operator had the Journey overlay open when it arrived.
            s.overlay = Overlay::Journey;
            s.outbox.clear();
            reduce(&mut s, action);

            let resolved = s.outbox.iter().any(|intent| {
                matches!(
                    intent,
                    Intent::ResolveApproval { decision, .. } if *decision == expected
                )
            });
            let touched_learning = s
                .outbox
                .iter()
                .any(|intent| matches!(intent, Intent::MutateLearning { .. }));
            assert!(
                resolved,
                "the visible approval must be resolved: {:?}",
                s.outbox
            );
            assert!(
                !touched_learning,
                "a learning must not be mutated by a key aimed at the approval modal"
            );
        }
    }

    #[test]
    fn a_reissued_question_resizes_its_card_instead_of_panicking() {
        let mut s = AppState::default();
        let run_id = RunId::new();
        let question_id = codypendent_protocol::QuestionId::new();
        let ask = |count: usize| EventBody::QuestionAsked {
            question_id,
            run_id,
            questions: (0..count)
                .map(|i| codypendent_protocol::question::QuestionPrompt {
                    header: format!("q{i}"),
                    question: format!("question {i}?"),
                    options: vec![codypendent_protocol::question::QuestionOption {
                        label: "yes".to_owned(),
                        description: String::new(),
                    }],
                    multiple: false,
                    custom: true,
                })
                .collect(),
        };

        reduce(&mut s, system_ev(ask(2)));
        let card = s.question_card_state.as_ref().expect("a card is opened");
        assert_eq!(card.custom_text.len(), 2);

        // The same question, redefined with more sub-questions.
        reduce(&mut s, system_ev(ask(5)));
        assert_eq!(
            s.pending_questions.len(),
            1,
            "a re-issue replaces rather than stacking"
        );
        let card = s.question_card_state.as_ref().expect("card survives");
        assert_eq!(
            card.custom_text.len(),
            5,
            "the card must be resized to the question it now shows"
        );
        assert!(
            card.index < card.custom_text.len(),
            "the cursor is in range"
        );

        // And back down again — the cursor must not be stranded past the end,
        // which is the soft-lock half of the same fault.
        reduce(&mut s, system_ev(ask(1)));
        let card = s.question_card_state.as_ref().expect("card survives");
        assert_eq!(card.custom_text.len(), 1);
        assert!(card.index < card.custom_text.len());
    }

    #[test]
    fn context_and_memory_notes_fold_into_backstage_not_visible_notes() {
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
            system_ev(EventBody::NoteAppended {
                text: "=== CONTEXT: EVIDENCE, NOT INSTRUCTIONS ===\nline\nline\nline".to_owned(),
                run_id: Some(run_id),
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::NoteAppended {
                text: "remembered: the test command is cargo test".to_owned(),
                run_id: Some(run_id),
            }),
        );
        // No visible Note cells; exactly one Backstage entry with the right counts.
        assert!(
            !s.runs[0]
                .transcript
                .iter()
                .any(|e| matches!(e, TranscriptEntry::Note { .. })),
            "context/memory notes must never create a visible Note cell"
        );
        let bs = s.runs[0].transcript.iter().find_map(|e| match e {
            TranscriptEntry::Backstage {
                context_lines,
                memory_updates,
                ..
            } => Some((*context_lines, *memory_updates)),
            _ => None,
        });
        assert_eq!(bs, Some((Some(4), 1)));
    }

    #[test]
    fn an_ordinary_note_still_renders_as_a_note_cell() {
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
            system_ev(EventBody::NoteAppended {
                text: "a plain observation".to_owned(),
                run_id: Some(run_id),
            }),
        );
        assert!(s.runs[0]
            .transcript
            .iter()
            .any(|e| matches!(e, TranscriptEntry::Note { .. })));
    }

    #[test]
    fn expand_toggles_a_selected_backstage_entry() {
        // Mirrors the Note/Tool/Patch expand pattern: the same Action::Expand
        // that toggles a selected note also toggles a selected Backstage entry.
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
            system_ev(EventBody::NoteAppended {
                text: "remembered: the test command is cargo test".to_owned(),
                run_id: Some(run_id),
            }),
        );
        let idx = s.runs[0]
            .transcript
            .iter()
            .position(|e| matches!(e, TranscriptEntry::Backstage { .. }))
            .expect("a Backstage entry was folded in");

        s.focus = Pane::Transcript;
        s.runs[0].transcript_selected = idx;
        reduce(&mut s, Action::Expand);
        let TranscriptEntry::Backstage { expanded, .. } = &s.runs[0].transcript[idx] else {
            unreachable!()
        };
        assert!(*expanded, "Expand opens the selected Backstage entry");

        reduce(&mut s, Action::Expand);
        let TranscriptEntry::Backstage { expanded, .. } = &s.runs[0].transcript[idx] else {
            unreachable!()
        };
        assert!(!*expanded, "Expand toggles it back off");
    }

    #[test]
    fn expand_toggles_a_selected_completed_entry() {
        // Task 3: the same Action::Expand that toggles a selected Backstage
        // entry also toggles a failed run's `Completed` entry, revealing the
        // full raw error chain beneath the concise summary (render.rs).
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
        let idx = s.runs[0]
            .transcript
            .iter()
            .position(|e| matches!(e, TranscriptEntry::Completed { .. }))
            .expect("a Completed entry was folded in");

        s.focus = Pane::Transcript;
        s.runs[0].transcript_selected = idx;
        reduce(&mut s, Action::Expand);
        let TranscriptEntry::Completed { expanded, .. } = &s.runs[0].transcript[idx] else {
            unreachable!()
        };
        assert!(*expanded, "Expand opens the selected Completed entry");

        reduce(&mut s, Action::Expand);
        let TranscriptEntry::Completed { expanded, .. } = &s.runs[0].transcript[idx] else {
            unreachable!()
        };
        assert!(!*expanded, "Expand toggles it back off");
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
                pending_approvals: Vec::new(),
                pending_prompts: Vec::new(),
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
                    thought: false,
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
                    thought: false,
                },
            ),
        );
        // Two deltas coalesce into one transcript entry, right after the User
        // turn RunStarted pushes for the objective.
        assert_eq!(s.runs[0].transcript.len(), 2);
        match &s.runs[0].transcript[1] {
            TranscriptEntry::Model { text, .. } => assert_eq!(text, "Hello, world"),
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
                pattern: None,
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

    /// PR B (MCP client): an `McpToolCall` gets its own one-line summary —
    /// `McpToolCall (server.tool)` — never the wildcard "unsupported
    /// capability" fallback.
    #[test]
    fn mcp_tool_call_capability_label_names_server_and_tool() {
        let action = ProposedAction::McpToolCall {
            server: "github".to_owned(),
            tool: "create_issue".to_owned(),
            summary: "create an issue".to_owned(),
            args: "{\"title\":\"bug\"}".to_owned(),
        };
        assert_eq!(
            capability_label(&action),
            "McpToolCall (github.create_issue)"
        );
    }

    #[test]
    fn approval_preempts_an_open_overlay_and_resolves_visibly() {
        let mut s = AppState::new();
        // A browser overlay is open when the approval arrives. The host card
        // must preempt it rather than leaving a run blocked behind invisible
        // normal-mode controls.
        reduce(&mut s, Action::OpenSkills);
        let _ = s.drain_outbox(); // client-only projection refresh
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
                pattern: None,
            }),
        );
        assert!(s.show_approval_modal(), "approval preempts the overlay");
        assert_eq!(s.input_mode(), crate::state::InputMode::Approval);

        reduce(&mut s, Action::Approve(ApprovalScope::Once));
        let intents = s.drain_outbox();
        assert!(
            matches!(intents.as_slice(), [Intent::ResolveApproval { .. }]),
            "the visible card resolves normally, got {intents:?}"
        );
    }

    #[test]
    fn plugin_enable_requires_host_confirmation_before_emitting() {
        let mut s = AppState::new();
        s.overlay = Overlay::UiPlugins;
        s.ui_plugins = vec![codypendent_protocol::UiPluginLifecycleStatus {
            id: "acme.review".to_owned(),
            version: "1.2.3".to_owned(),
            state: "installed".to_owned(),
            enabled_scope: None,
            update_approval_receipt: None,
            update_permission_diff: Some("+ network: api.acme.test".to_owned()),
        }];

        reduce(&mut s, Action::EnableUiPluginSession);
        assert_eq!(
            s.overlay,
            Overlay::ConfirmUiPluginEnable {
                plugin_id: "acme.review".to_owned(),
                scope: "session".to_owned(),
                permission_summary: "+ network: api.acme.test".to_owned(),
            }
        );
        assert!(
            s.outbox.is_empty(),
            "opening the trust prompt grants nothing"
        );

        reduce(&mut s, Action::Dismiss);
        assert_eq!(s.overlay, Overlay::UiPlugins);
        assert!(s.outbox.is_empty());

        reduce(&mut s, Action::EnableUiPluginSession);
        reduce(&mut s, Action::ConfirmCancel);
        assert_eq!(s.overlay, Overlay::UiPlugins);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::EnableUiPlugin {
                plugin_id: "acme.review".to_owned(),
                scope: "session".to_owned(),
            }]
        );
    }

    #[test]
    fn plugin_update_confirmation_retains_the_decisive_permission_diff() {
        let mut s = AppState::new();
        s.overlay = Overlay::UiPlugins;
        s.ui_plugins = vec![codypendent_protocol::UiPluginLifecycleStatus {
            id: "acme.review".to_owned(),
            version: "1.2.3".to_owned(),
            state: "update_pending".to_owned(),
            enabled_scope: Some("session".to_owned()),
            update_approval_receipt: Some("receipt-1".to_owned()),
            update_permission_diff: Some("+ filesystem_write: repo".to_owned()),
        }];

        reduce(&mut s, Action::Approve(ApprovalScope::Once));
        assert_eq!(
            s.overlay,
            Overlay::ConfirmUiPluginApprove {
                plugin_id: "acme.review".to_owned(),
                receipt: "receipt-1".to_owned(),
                permission_diff: "+ filesystem_write: repo".to_owned(),
            }
        );
        assert!(s.outbox.is_empty());

        reduce(&mut s, Action::ConfirmCancel);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::ApproveUiPluginUpdate {
                plugin_id: "acme.review".to_owned(),
                receipt: "receipt-1".to_owned(),
            }]
        );
    }

    #[test]
    fn run_started_does_not_steal_selection_mid_draft() {
        let mut s = AppState::new();
        let mine = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id: mine,
                objective: "mine".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        assert_eq!(s.selected_run, 0);

        // A draft is in progress: another client's RunStarted (shared session)
        // must not move the selection — Enter submits against `selected_run`,
        // so a steal here would retarget the message being composed.
        reduce(&mut s, Action::InputChar('h'));
        let theirs = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id: theirs,
                objective: "theirs".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        assert_eq!(s.runs.len(), 2);
        assert_eq!(s.selected_run, 0, "a mid-draft selection must not move");

        // With an empty composer a new run takes focus (follow the action) —
        // this is also what keeps our own submits selected, since submitting
        // clears the composer before its RunStarted folds back.
        s.composer.clear();
        let third = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id: third,
                objective: "next".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        assert_eq!(s.selected_run, 2);
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
                label: Some("cargo test".to_owned()),
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
        // `ToolStarted.label` (STARTED, not PROPOSED or COMPLETED — neither
        // carries a label) lands on the already-Proposed card unchanged
        // through completion.
        assert_eq!(card.label, Some("cargo test".to_owned()));
        assert_eq!(card.status, ToolStatus::Completed);
        assert_eq!(card.outcome, Some(ToolOutcome::Succeeded));
        assert!(card.artifact.is_some());
    }

    #[test]
    fn completion_named_by_target_reconciles_the_running_card_not_a_cross() {
        // Regression: the ACP bridge labels a completion by the tool's TARGET
        // (e.g. a file path) while the start named the tool KIND (e.g. `read`),
        // so exact-name reconciliation misses. Before the LIFO fallback this
        // orphaned the `read` start card (force-failed at run end → a ✗) and
        // pushed a duplicate target-titled card. It must now fold into ONE
        // Completed card that succeeded — no cross, no duplicate.
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
            system_ev(EventBody::ToolStarted {
                run_id,
                tool: "read".to_owned(),
                args_digest: "d".to_owned(),
                label: Some("apps/frontend/package.json".to_owned()),
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ToolCompleted {
                run_id,
                // Named by the target, not the kind — the mismatch that used to
                // orphan the start card.
                tool: "apps/frontend/package.json".to_owned(),
                outcome: ToolOutcome::Succeeded,
                artifact: None,
            }),
        );
        // Even if the run then reaches a terminal state, the tool must not be
        // force-failed — it already reconciled.
        reduce(
            &mut s,
            system_ev(EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Completed { summary: None },
                chronicle: artifact(),
            }),
        );
        let tools: Vec<_> = s.runs[0]
            .transcript
            .iter()
            .filter_map(|e| match e {
                TranscriptEntry::Tool(card) => Some(card),
                _ => None,
            })
            .collect();
        assert_eq!(tools.len(), 1, "must fold into one card, not orphan + dup");
        assert_eq!(tools[0].tool, "read", "keeps the start (kind) title");
        assert_eq!(tools[0].status, ToolStatus::Completed);
        assert_eq!(tools[0].outcome, Some(ToolOutcome::Succeeded));
    }

    #[test]
    fn unmatched_completion_does_not_steal_either_of_two_running_cards() {
        // The target-named fallback may only fire when the pairing is
        // unambiguous. With two tools genuinely in flight, guessing would hand
        // tool A's outcome to tool B and leave B's own completion to push a
        // duplicate orphan — so an unmatched completion gets its own card and
        // both running cards keep running.
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
        for tool in ["read", "search"] {
            reduce(
                &mut s,
                system_ev(EventBody::ToolStarted {
                    run_id,
                    tool: tool.to_owned(),
                    args_digest: "d".to_owned(),
                    label: None,
                }),
            );
        }
        reduce(
            &mut s,
            system_ev(EventBody::ToolCompleted {
                run_id,
                tool: "apps/frontend/package.json".to_owned(),
                outcome: ToolOutcome::Succeeded,
                artifact: None,
            }),
        );
        let tools: Vec<_> = s.runs[0]
            .transcript
            .iter()
            .filter_map(|e| match e {
                TranscriptEntry::Tool(card) => Some(card),
                _ => None,
            })
            .collect();
        assert_eq!(tools.len(), 3, "the orphan completion gets its own card");
        assert_eq!(tools[0].tool, "read");
        assert_eq!(tools[0].status, ToolStatus::Running, "must not be stolen");
        assert_eq!(tools[1].tool, "search");
        assert_eq!(tools[1].status, ToolStatus::Running, "must not be stolen");
        assert_eq!(tools[2].tool, "apps/frontend/package.json");
        assert_eq!(tools[2].status, ToolStatus::Completed);
    }

    #[test]
    fn denied_tool_then_terminal_completion_reconciles_to_one_card() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        let action = ProposedAction::ExecuteCommand {
            program: "cargo".to_owned(),
            args: vec!["test".to_owned()],
            environment: Vec::new(),
            cwd: None,
        };
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
            system_ev(EventBody::ToolDenied {
                run_id,
                action,
                reasons: vec!["blocked by policy".to_owned()],
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ToolCompleted {
                run_id,
                tool: "shell.run".to_owned(),
                outcome: ToolOutcome::Failed {
                    message: "policy denied".to_owned(),
                },
                artifact: None,
            }),
        );

        let cards: Vec<_> = s.runs[0]
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Tool(card) => Some(card.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(cards.len(), 1, "one invocation must render one card");
        assert_eq!(cards[0].tool, "shell.run");
        assert_eq!(cards[0].status, ToolStatus::Completed);
        assert_eq!(
            cards[0].outcome,
            Some(ToolOutcome::Failed {
                message: "policy denied".to_owned(),
            })
        );
    }

    #[test]
    fn rejected_approval_closes_and_reuses_the_proposed_tool_card() {
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
            system_ev(EventBody::ApprovalResolved {
                approval_id,
                decision: ApprovalDecision::Reject,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ToolCompleted {
                run_id,
                tool: "shell.run".to_owned(),
                outcome: ToolOutcome::Failed {
                    message: "operator rejected".to_owned(),
                },
                artifact: None,
            }),
        );

        let cards: Vec<_> = s.runs[0]
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Tool(card) => Some(card.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].status, ToolStatus::Completed);
        assert_eq!(cards[0].tool, "shell.run");
    }

    #[test]
    fn run_completion_terminalizes_every_open_tool_card() {
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
            system_ev(EventBody::ToolStarted {
                run_id,
                tool: "shell.run".to_owned(),
                args_digest: "abc".to_owned(),
                label: None,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Cancelled {
                    reason: Some("operator cancelled".to_owned()),
                },
                chronicle: artifact(),
            }),
        );

        let card = s.runs[0]
            .transcript
            .iter()
            .find_map(|entry| match entry {
                TranscriptEntry::Tool(card) => Some(card.as_ref()),
                _ => None,
            })
            .expect("tool card");
        assert_eq!(card.status, ToolStatus::Completed);
        assert!(matches!(card.outcome, Some(ToolOutcome::Failed { .. })));
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
    fn budget_warning_tokens_brings_the_dead_context_footer_alive() {
        // Context-window protection (BT5): the plain (non-workflow) loop's
        // new `BudgetWarning{Tokens}` emitter (BT3) must drive the exact same
        // `context_percent` projection the workflow budget engine already
        // did — proving this reducer arm (`reduce.rs:535-546`) needs zero
        // change to bring the footer alive for normal chat.
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
        // Honesty: before any `BudgetWarning{Tokens}` event lands, the
        // footer's source field must stay unknown — never a fabricated
        // percent.
        assert_eq!(s.runs[0].context_percent, None);

        reduce(
            &mut s,
            system_ev(EventBody::BudgetWarning {
                run_id,
                dimension: BudgetDimension::Tokens,
                used: 8_192,
                limit: 32_768,
            }),
        );
        assert_eq!(s.runs[0].context_percent, Some(25));
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

    /// Task 3: `RunActivity` is derived purely from folding run-state,
    /// streaming, and tool-lifecycle events — never fetched. Walks a run
    /// through every transition the reducer owns: `Running` ⇒ `Thinking`, a
    /// model delta ⇒ `Streaming`, a tool starting ⇒ `RunningTool(name)`, that
    /// tool completing ⇒ back to `Thinking`, and the terminal `RunCompleted`
    /// ⇒ `Idle`.
    #[test]
    fn run_activity_tracks_thinking_streaming_tool_and_idle() {
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
        assert_eq!(s.runs[0].activity, RunActivity::Thinking);

        reduce(
            &mut s,
            ev(
                agent_actor(run_id),
                EventBody::ModelStreamDelta {
                    run_id,
                    text: "hi".to_owned(),
                    thought: false,
                },
            ),
        );
        assert_eq!(s.runs[0].activity, RunActivity::Streaming);

        reduce(
            &mut s,
            system_ev(EventBody::ToolStarted {
                run_id,
                tool: "shell.run".to_owned(),
                args_digest: "abc".to_owned(),
                label: None,
            }),
        );
        assert_eq!(
            s.runs[0].activity,
            RunActivity::RunningTool("shell.run".to_owned())
        );

        reduce(
            &mut s,
            system_ev(EventBody::ToolCompleted {
                run_id,
                tool: "shell.run".to_owned(),
                outcome: ToolOutcome::Succeeded,
                artifact: None,
            }),
        );
        assert_eq!(s.runs[0].activity, RunActivity::Thinking);

        reduce(
            &mut s,
            system_ev(EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Completed {
                    summary: Some("done".to_owned()),
                },
                chronicle: artifact(),
            }),
        );
        assert_eq!(s.runs[0].activity, RunActivity::Idle);
    }

    #[test]
    fn paused_and_waiting_run_states_stop_stale_activity_spinners() {
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

        for waiting in [
            RunState::Paused,
            RunState::WaitingForApproval,
            RunState::WaitingForUserInput,
        ] {
            s.runs[0].activity = RunActivity::RunningTool("stale".to_owned());
            reduce(
                &mut s,
                system_ev(EventBody::RunStateChanged {
                    run_id,
                    state: waiting,
                }),
            );
            assert_eq!(
                s.runs[0].activity,
                RunActivity::Idle,
                "{waiting:?} must not keep an old tool spinner"
            );
        }

        reduce(
            &mut s,
            system_ev(EventBody::RunStateChanged {
                run_id,
                state: RunState::Recovering,
            }),
        );
        assert_eq!(s.runs[0].activity, RunActivity::Thinking);
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
                pattern: None,
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
                // No model was staged, so the run carries no pin.
                model: None,
            }]
        );
    }

    #[test]
    fn starting_a_run_after_staging_a_model_carries_the_pin() {
        // STEP MP2: a model picked in the `/model` popup pins the model for the
        // run the operator then starts — the staged `pending_model` flows into
        // the `StartRun` intent. Session-default: the pin also survives on
        // `pending_model` for subsequent runs (it is not cleared on submit).
        let mut s = AppState::new();
        s.models = vec![
            model_card("local-qwen", "openai-compatible"),
            model_card("hosted-gpt", "openai-compatible"),
        ];
        open_model_picker(&mut s);
        reduce(&mut s, Action::SelectNext); // focus "hosted-gpt"
        reduce(&mut s, Action::InputSubmit); // stage it on pending_model
        assert_eq!(s.pending_model, Some(ModelId("hosted-gpt".to_owned())));

        // Start a run via the NewRun overlay.
        reduce(&mut s, Action::NewRun);
        for c in "fix the test".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);

        assert_eq!(
            s.drain_outbox(),
            vec![Intent::StartRun {
                objective: "fix the test".to_owned(),
                mode: AgentMode::Build,
                model: Some(ModelId("hosted-gpt".to_owned())),
            }],
            "the staged model pins the started run"
        );
        assert_eq!(
            s.pending_model,
            Some(ModelId("hosted-gpt".to_owned())),
            "session-default: the pin persists for subsequent runs"
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
            document_id: codypendent_protocol::DocumentId::new(),
            title: title.to_owned(),
            scope: "organization".to_owned(),
            status: "draft".to_owned(),
            mode: "suggest".to_owned(),
            revision: "r3".to_owned(),
            blocks: vec![crate::state::DocBlockView {
                id: "b1".to_owned(),
                kind: "heading".to_owned(),
                text: title.to_owned(),
                editable: Some(title.to_owned()),
            }],
            suggestions: vec![crate::state::DocSuggestionView {
                id: "s1".to_owned(),
                block_id: "b1".to_owned(),
                source_revision: 3,
                status: "pending".to_owned(),
                author: "agent".to_owned(),
                range: "0..4".to_owned(),
                original: title.to_owned(),
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
    fn docs_mouse_rows_focus_the_matching_tree_editor_and_review_items() {
        let mut s = AppState::new();
        let mut first = doc("a");
        first.blocks.push(crate::state::DocBlockView {
            id: "b2".to_owned(),
            kind: "paragraph".to_owned(),
            text: "second block".to_owned(),
            editable: Some("second block".to_owned()),
        });
        first.suggestions.push(crate::state::DocSuggestionView {
            id: "s2".to_owned(),
            block_id: "b2".to_owned(),
            source_revision: 3,
            status: "pending".to_owned(),
            author: "reviewer".to_owned(),
            range: "1..2".to_owned(),
            original: "e".to_owned(),
            replacement: "replacement".to_owned(),
            rationale: None,
        });
        s.docs = vec![first, doc("b")];
        reduce(&mut s, Action::OpenDocs);
        let _ = s.drain_outbox();

        reduce(&mut s, Action::SelectDocumentBlock(1));
        assert_eq!(s.doc_focus, DocFocus::Editor);
        assert_eq!(s.selected_block, 1);
        reduce(&mut s, Action::SelectDocumentSuggestion(1));
        assert_eq!(s.doc_focus, DocFocus::Review);
        assert_eq!(s.selected_suggestion, 1);
        reduce(&mut s, Action::SelectDocument(1));
        assert_eq!(s.doc_focus, DocFocus::Tree);
        assert_eq!(s.selected_doc, 1);
        assert_eq!(s.selected_block, 0);
        assert_eq!(s.selected_suggestion, 0);
        let selected_document_id = s.docs[1].document_id;
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::WatchDocument {
                document_id: selected_document_id,
            }]
        );
    }

    // --- Docs Studio live editing (Phase 4 STEP 4.3 client wiring) ---

    /// Open the Docs browser focused on the review rail (Tree → Editor → Review).
    fn docs_on_review(docs: Vec<crate::state::DocCard>) -> AppState {
        let mut s = AppState::new();
        s.docs = docs;
        reduce(&mut s, Action::OpenDocs);
        let _ = s.drain_outbox(); // refresh + live document watch
        reduce(&mut s, Action::CyclePane); // Editor
        reduce(&mut s, Action::CyclePane); // Review
        s
    }

    #[test]
    fn tab_cycles_the_docs_rail_focus() {
        let mut s = AppState::new();
        s.docs = vec![doc("a")];
        reduce(&mut s, Action::OpenDocs);
        assert_eq!(s.doc_focus, DocFocus::Tree);
        reduce(&mut s, Action::CyclePane);
        assert_eq!(s.doc_focus, DocFocus::Editor);
        reduce(&mut s, Action::CyclePane);
        assert_eq!(s.doc_focus, DocFocus::Review);
        reduce(&mut s, Action::CyclePane);
        assert_eq!(s.doc_focus, DocFocus::Tree);
    }

    #[test]
    fn docs_editor_rail_nav_moves_the_block_cursor_not_the_tree() {
        let mut s = AppState::new();
        let mut card = doc("a");
        card.blocks.push(crate::state::DocBlockView {
            id: "b2".to_owned(),
            kind: "paragraph".to_owned(),
            text: "second".to_owned(),
            editable: Some("second".to_owned()),
        });
        s.docs = vec![card, doc("b")];
        reduce(&mut s, Action::OpenDocs);
        reduce(&mut s, Action::CyclePane); // Editor rail
        assert_eq!(s.selected_doc, 0);
        reduce(&mut s, Action::SelectNext);
        assert_eq!(s.selected_block, 1, "the block cursor moves");
        assert_eq!(s.selected_doc, 0, "the tree selection stays put");
    }

    #[test]
    fn edit_doc_opens_the_block_edit_prompt_prefilled_with_the_block_text() {
        let mut s = AppState::new();
        s.docs = vec![doc("a")];
        reduce(&mut s, Action::OpenDocs);
        reduce(&mut s, Action::CyclePane); // Editor rail
        reduce(&mut s, Action::EditDoc);
        match &s.overlay {
            Overlay::DocEdit {
                block_id,
                buffer,
                original,
            } => {
                assert_eq!(block_id, "b1");
                // Prefilled, not empty — `e` edits the block rather than
                // prepending to it.
                assert_eq!(buffer, "a");
                assert_eq!(original, "a");
            }
            other => panic!("expected the block-edit prompt, got {other:?}"),
        }
        // Outside the editor rail, `e` is inert.
        let mut t = AppState::new();
        t.docs = vec![doc("a")];
        reduce(&mut t, Action::OpenDocs); // Tree focus
        reduce(&mut t, Action::EditDoc);
        assert_eq!(t.overlay, Overlay::Docs);
    }

    #[test]
    fn edit_doc_refuses_a_block_with_no_editable_text() {
        let mut s = AppState::new();
        let mut card = doc("a");
        card.blocks[0].kind = "table".to_owned();
        card.blocks[0].editable = None;
        s.docs = vec![card];
        reduce(&mut s, Action::OpenDocs);
        reduce(&mut s, Action::CyclePane); // Editor rail
        reduce(&mut s, Action::EditDoc);
        // No prompt opens — a structured block has no single text container to
        // replace, so submitting one could never apply.
        assert_eq!(s.overlay, Overlay::Docs);
        assert!(s.notice.is_some());
    }

    #[test]
    fn submitting_a_block_edit_replaces_the_whole_block_text() {
        let mut s = AppState::new();
        s.docs = vec![doc("a")];
        let document_id = s.docs[0].document_id;
        reduce(&mut s, Action::OpenDocs);
        let _ = s.drain_outbox();
        reduce(&mut s, Action::CyclePane); // Editor rail
        reduce(&mut s, Action::EditDoc);
        // Clear the prefilled "a" and type a replacement.
        reduce(&mut s, Action::InputBackspace);
        for c in "hi".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);

        // The prompt closed back to the browser and a lease was requested.
        assert_eq!(s.overlay, Overlay::Docs);
        assert_eq!(
            s.outbox,
            vec![Intent::AcquireDocumentLease {
                document_id,
                block_id: Some("b1".to_owned()),
            }]
        );
        // A FULL REPLACE — delete the one character the block held, then insert
        // the new text — not the prepend-only `delete_len: 0` of before.
        let edit = s.doc_edit.as_ref().expect("an edit is in flight");
        assert_eq!(edit.lease, DocLeaseState::Acquiring);
        assert_eq!(
            edit.pending,
            Some(DocumentMutation::EditText {
                block_id: "b1".to_owned(),
                position: 0,
                delete_len: 1,
                insert: "hi".to_owned(),
            })
        );
    }

    #[test]
    fn an_unchanged_block_edit_submits_nothing() {
        let mut s = AppState::new();
        s.docs = vec![doc("a")];
        reduce(&mut s, Action::OpenDocs);
        let _ = s.drain_outbox();
        reduce(&mut s, Action::CyclePane);
        reduce(&mut s, Action::EditDoc);
        // Submitting the prefilled text unchanged is a no-op: never spend a
        // revision (or, in Suggest mode, a suggestion) on it.
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::Docs);
        assert!(s.outbox.is_empty());
        assert!(s.doc_edit.is_none());
    }

    #[test]
    fn n_creates_a_document_from_the_docs_studio() {
        let mut s = AppState::new();
        s.docs = vec![doc("a")];
        reduce(&mut s, Action::OpenDocs);
        let _ = s.drain_outbox();
        // `n` is contextual: in the Docs Studio it opens the new-DOCUMENT prompt
        // rather than the new-run one.
        reduce(&mut s, Action::NewRun);
        assert!(matches!(s.overlay, Overlay::DocNew { .. }));
        for c in "Runbook".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::Docs);
        assert_eq!(
            s.outbox,
            vec![Intent::CreateDocument {
                title: "Runbook".to_owned(),
            }]
        );

        // Outside the Docs Studio, `n` still opens the new-run prompt.
        let mut t = AppState::new();
        reduce(&mut t, Action::NewRun);
        assert!(matches!(t.overlay, Overlay::NewRun(_)));
    }

    #[test]
    fn insert_adds_a_paragraph_below_the_focused_block_under_a_structural_lease() {
        let mut s = AppState::new();
        let mut card = doc("a");
        card.blocks.push(crate::state::DocBlockView {
            id: "b2".to_owned(),
            kind: "paragraph".to_owned(),
            text: "second".to_owned(),
            editable: Some("second".to_owned()),
        });
        s.docs = vec![card];
        let document_id = s.docs[0].document_id;
        reduce(&mut s, Action::OpenDocs);
        let _ = s.drain_outbox();
        reduce(&mut s, Action::CyclePane); // Editor rail
        reduce(&mut s, Action::InsertDocBlock);
        match &s.overlay {
            Overlay::DocInsert { index, .. } => assert_eq!(*index, 1, "below block 0"),
            other => panic!("expected the insert prompt, got {other:?}"),
        }
        for c in "new".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);

        // A structural mutation takes the WHOLE-DOCUMENT lease.
        assert_eq!(
            s.outbox,
            vec![Intent::AcquireDocumentLease {
                document_id,
                block_id: None,
            }]
        );
        let edit = s.doc_edit.as_ref().expect("an edit is in flight");
        match edit.pending.as_ref().expect("a queued mutation") {
            DocumentMutation::Insert {
                index,
                block_id,
                content,
            } => {
                assert_eq!(*index, 1);
                assert!(!block_id.is_empty(), "a fresh block id is minted");
                assert_eq!(content["type"], "paragraph");
                assert_eq!(content["text"], "new");
            }
            other => panic!("expected a block insert, got {other:?}"),
        }
    }

    #[test]
    fn deleting_a_block_requires_a_confirmation_first() {
        let mut s = AppState::new();
        s.docs = vec![doc("a")];
        let document_id = s.docs[0].document_id;
        reduce(&mut s, Action::OpenDocs);
        let _ = s.drain_outbox();
        reduce(&mut s, Action::CyclePane); // Editor rail
        reduce(&mut s, Action::DeleteDocBlock);
        // Nothing was sent yet — the keypress only opens the confirmation.
        assert!(matches!(s.overlay, Overlay::DocDeleteConfirm { .. }));
        assert!(s.outbox.is_empty());

        // Dismissing it deletes nothing.
        reduce(&mut s, Action::Dismiss);
        assert_eq!(s.overlay, Overlay::Docs);
        assert!(s.outbox.is_empty());

        // Confirming does, under a structural (whole-document) lease.
        reduce(&mut s, Action::DeleteDocBlock);
        reduce(&mut s, Action::ConfirmCancel);
        assert_eq!(s.overlay, Overlay::Docs);
        assert_eq!(
            s.outbox,
            vec![Intent::AcquireDocumentLease {
                document_id,
                block_id: None,
            }]
        );
        assert_eq!(
            s.doc_edit.as_ref().and_then(|e| e.pending.clone()),
            Some(DocumentMutation::Delete {
                block_id: "b1".to_owned(),
            })
        );
    }

    #[test]
    fn docs_publish_uses_a_safe_default_and_emits_an_approval_gated_target() {
        let mut s = AppState::new();
        s.docs = vec![doc("Payments & Retry Guide")];
        let document_id = s.docs[0].document_id;
        reduce(&mut s, Action::OpenDocs);
        let _ = s.drain_outbox(); // projection refresh + live watch

        reduce(&mut s, Action::PublishDoc);
        // Step 1 is the target picker, defaulting to the narrowest target.
        assert_eq!(
            s.overlay,
            Overlay::DocPublishTarget {
                document_id,
                selected: 0,
            }
        );
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.overlay,
            Overlay::DocPublishPath {
                document_id,
                target: DocPublishTargetKind::RepositoryFile,
                buffer: "docs/payments-retry-guide.md".to_owned(),
            }
        );
        reduce(&mut s, Action::InputSubmit);

        assert_eq!(s.overlay, Overlay::Docs);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::PublishDocument {
                document_id,
                target: codypendent_protocol::PublishTarget::RepositoryFile {
                    path: "docs/payments-retry-guide.md".to_owned(),
                },
            }]
        );
    }

    /// Outcome 18 F10: the daemon has always accepted three publish targets and
    /// rates each one's risk on its own approval card, but the Studio could
    /// construct only `RepositoryFile` — the other two were unreachable from
    /// the shipped UI. This drives the longest path (a documentation PR) end to
    /// end through the reducer.
    #[test]
    fn docs_publish_can_reach_a_documentation_pull_request() {
        let mut s = AppState::new();
        s.docs = vec![doc("Payments & Retry Guide")];
        let document_id = s.docs[0].document_id;
        reduce(&mut s, Action::OpenDocs);
        let _ = s.drain_outbox();

        reduce(&mut s, Action::PublishDoc);
        // Move to the third row: the documentation PR.
        reduce(&mut s, Action::SelectNext);
        reduce(&mut s, Action::SelectNext);
        reduce(&mut s, Action::InputSubmit);
        assert!(matches!(
            s.overlay,
            Overlay::DocPublishPath {
                target: DocPublishTargetKind::DocumentationPr,
                ..
            }
        ));
        reduce(&mut s, Action::InputSubmit);
        // The branch step exists only for the two git-branch targets, and it
        // defaults to a name derived from the path.
        assert_eq!(
            s.overlay,
            Overlay::DocPublishBranch {
                document_id,
                target: DocPublishTargetKind::DocumentationPr,
                path: "docs/payments-retry-guide.md".to_owned(),
                buffer: "docs/docs-payments-retry-guide".to_owned(),
            }
        );
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.overlay,
            Overlay::DocPublishTitle {
                document_id,
                path: "docs/payments-retry-guide.md".to_owned(),
                branch: "docs/docs-payments-retry-guide".to_owned(),
                buffer: "docs: Payments & Retry Guide".to_owned(),
            }
        );
        assert!(s.outbox.is_empty(), "nothing is sent until the last step");
        reduce(&mut s, Action::InputSubmit);

        assert_eq!(s.overlay, Overlay::Docs);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::PublishDocument {
                document_id,
                target: codypendent_protocol::PublishTarget::DocumentationPr {
                    branch: "docs/docs-payments-retry-guide".to_owned(),
                    path: "docs/payments-retry-guide.md".to_owned(),
                    title: "docs: Payments & Retry Guide".to_owned(),
                },
            }]
        );
    }

    /// The picker is mouse-reachable too: a click must select the clicked row
    /// and advance, not the row the keyboard cursor happened to be on.
    #[test]
    fn docs_publish_target_row_click_selects_and_advances() {
        let mut s = AppState::new();
        s.docs = vec![doc("Release Notes")];
        reduce(&mut s, Action::OpenDocs);
        let _ = s.drain_outbox();
        reduce(&mut s, Action::PublishDoc);
        reduce(&mut s, Action::ActivateRow(2));
        assert!(matches!(
            s.overlay,
            Overlay::DocPublishPath {
                target: DocPublishTargetKind::DocumentationPr,
                ..
            }
        ));
    }

    /// The docs-branch target stops one step earlier — it needs no PR title.
    #[test]
    fn docs_publish_docs_branch_commit_stops_at_the_branch() {
        let mut s = AppState::new();
        s.docs = vec![doc("Release Notes")];
        let document_id = s.docs[0].document_id;
        reduce(&mut s, Action::OpenDocs);
        let _ = s.drain_outbox();

        reduce(&mut s, Action::PublishDoc);
        reduce(&mut s, Action::SelectNext);
        reduce(&mut s, Action::InputSubmit); // target -> path
        reduce(&mut s, Action::InputSubmit); // path -> branch
        reduce(&mut s, Action::InputSubmit); // branch -> send
        assert_eq!(s.overlay, Overlay::Docs);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::PublishDocument {
                document_id,
                target: codypendent_protocol::PublishTarget::DocsBranchCommit {
                    branch: "docs/docs-release-notes".to_owned(),
                    path: "docs/release-notes.md".to_owned(),
                },
            }]
        );
    }

    /// A branch name reaches `git` on the daemon side, so the shapes that could
    /// be read as a flag, a refspec, or traversal never leave the prompt.
    #[test]
    fn docs_publish_rejects_unsafe_branch_names() {
        let mut s = AppState::new();
        let document_id = codypendent_protocol::DocumentId::new();
        for invalid in [
            "",
            "-force",
            "/leading",
            "trailing/",
            "docs/../main",
            "docs//main",
            "docs/branch.lock",
            "docs/branch;rm -rf /",
            "docs/branch\u{7f}",
        ] {
            s.overlay = Overlay::DocPublishBranch {
                document_id,
                target: DocPublishTargetKind::DocsBranchCommit,
                path: "docs/report.md".to_owned(),
                buffer: invalid.to_owned(),
            };
            reduce(&mut s, Action::InputSubmit);
            assert!(
                matches!(s.overlay, Overlay::DocPublishBranch { .. }),
                "{invalid:?} must remain in the prompt"
            );
            assert!(s.outbox.is_empty(), "{invalid:?} must send nothing");
        }
        assert!(valid_publish_branch("docs/payments-retry_guide.v2"));
    }

    /// A PR with no title is refused rather than sent with an empty one.
    #[test]
    fn docs_publish_rejects_a_blank_pull_request_title() {
        let mut s = AppState::new();
        s.overlay = Overlay::DocPublishTitle {
            document_id: codypendent_protocol::DocumentId::new(),
            path: "docs/report.md".to_owned(),
            branch: "docs/report".to_owned(),
            buffer: "   ".to_owned(),
        };
        reduce(&mut s, Action::InputSubmit);
        assert!(matches!(s.overlay, Overlay::DocPublishTitle { .. }));
        assert!(s.outbox.is_empty());
    }

    #[test]
    fn docs_publish_rejects_absolute_and_parent_traversal_paths() {
        let mut s = AppState::new();
        let document_id = codypendent_protocol::DocumentId::new();
        for invalid in ["/tmp/report.md", "../report.md"] {
            s.overlay = Overlay::DocPublishPath {
                document_id,
                target: DocPublishTargetKind::RepositoryFile,
                buffer: invalid.to_owned(),
            };
            reduce(&mut s, Action::InputSubmit);
            assert!(
                matches!(s.overlay, Overlay::DocPublishPath { .. }),
                "{invalid} must remain in the prompt"
            );
            assert!(s.outbox.is_empty());
        }
        assert!(
            valid_publish_path("docs/release..notes.md"),
            "two dots inside a normal filename are not parent traversal"
        );
        assert!(!valid_publish_path("docs/report.txt"));
    }

    #[test]
    fn a_lease_grant_marks_held_and_fires_the_queued_mutation() {
        let mut s = AppState::new();
        s.docs = vec![doc("a")];
        let document_id = s.docs[0].document_id;
        reduce(&mut s, Action::OpenDocs);
        reduce(&mut s, Action::CyclePane);
        reduce(&mut s, Action::EditDoc);
        // The prompt opens prefilled with the block's text ("a"); typing appends
        // to it, so the submitted edit replaces "a" with "ax".
        reduce(&mut s, Action::InputChar('x'));
        reduce(&mut s, Action::InputSubmit);
        let _ = s.drain_outbox(); // the AcquireDocumentLease intent

        reduce(
            &mut s,
            Action::DocumentLeaseGranted {
                document_id,
                lease_id: "lease-1".to_owned(),
            },
        );

        let edit = s.doc_edit.as_ref().expect("still tracking the edit");
        assert_eq!(edit.lease, DocLeaseState::Held);
        assert_eq!(edit.lease_id.as_deref(), Some("lease-1"));
        assert!(edit.pending.is_none(), "the queued mutation was fired");
        assert_eq!(
            s.outbox,
            vec![Intent::MutateDocument {
                document_id,
                mutation: DocumentMutation::EditText {
                    block_id: "b1".to_owned(),
                    position: 0,
                    delete_len: 1,
                    insert: "ax".to_owned(),
                },
            }]
        );
    }

    #[test]
    fn a_late_or_mismatched_lease_grant_is_released_immediately() {
        let late_document_id = codypendent_protocol::DocumentId::new();
        let mut closed = AppState::new();

        reduce(
            &mut closed,
            Action::DocumentLeaseGranted {
                document_id: late_document_id,
                lease_id: "lease-after-close".to_owned(),
            },
        );

        assert_eq!(
            closed.outbox,
            vec![Intent::ReleaseDocumentLease {
                lease_id: "lease-after-close".to_owned(),
            }],
            "a grant arriving after Docs closed must not live until its TTL"
        );

        let mut acquiring_other = AppState::new();
        let active_document_id = codypendent_protocol::DocumentId::new();
        acquiring_other.overlay = Overlay::Docs;
        acquiring_other.doc_edit = Some(DocEdit {
            document_id: active_document_id,
            block_id: Some("active-block".to_owned()),
            lease: DocLeaseState::Acquiring,
            lease_id: None,
            pending: None,
        });

        reduce(
            &mut acquiring_other,
            Action::DocumentLeaseGranted {
                document_id: late_document_id,
                lease_id: "lease-wrong-document".to_owned(),
            },
        );

        assert_eq!(
            acquiring_other.outbox,
            vec![Intent::ReleaseDocumentLease {
                lease_id: "lease-wrong-document".to_owned(),
            }]
        );
        assert!(matches!(
            acquiring_other.doc_edit,
            Some(DocEdit {
                document_id,
                lease: DocLeaseState::Acquiring,
                ..
            }) if document_id == active_document_id
        ));
    }

    #[test]
    fn a_lease_rejection_blocks_the_edit_and_shows_a_notice() {
        let mut s = AppState::new();
        s.docs = vec![doc("a")];
        let document_id = s.docs[0].document_id;
        reduce(&mut s, Action::OpenDocs);
        reduce(&mut s, Action::CyclePane);
        reduce(&mut s, Action::EditDoc);
        reduce(&mut s, Action::InputChar('x'));
        reduce(&mut s, Action::InputSubmit);
        let _ = s.drain_outbox();

        reduce(&mut s, Action::DocumentLeaseBlocked);

        let edit = s.doc_edit.as_ref().expect("still tracking the edit");
        assert_eq!(edit.lease, DocLeaseState::Blocked);
        assert!(edit.pending.is_none(), "the queued mutation was dropped");
        assert!(s.outbox.is_empty(), "nothing is sent for a blocked lease");
        let notice = s.notice.as_ref().expect("a visible notice").0.clone();
        assert!(
            notice.contains("another writer"),
            "the range-leased notice must be visible: {notice}"
        );
        // Correlation to `document_id` is implicit: the client holds one in-flight
        // edit at a time, so a range-leased rejection is for that edit.
        assert_eq!(edit.document_id, document_id);
    }

    #[test]
    fn accepting_the_focused_suggestion_emits_an_accept_mutation() {
        let mut s = docs_on_review(vec![doc("a")]);
        let document_id = s.docs[0].document_id;
        reduce(&mut s, Action::Approve(ApprovalScope::Once));
        assert_eq!(
            s.outbox,
            vec![Intent::MutateDocument {
                document_id,
                mutation: DocumentMutation::AcceptSuggestion {
                    suggestion_id: "s1".to_owned(),
                },
            }]
        );
    }

    #[test]
    fn rejecting_the_focused_suggestion_emits_a_reject_mutation() {
        let mut s = docs_on_review(vec![doc("a")]);
        let document_id = s.docs[0].document_id;
        reduce(&mut s, Action::Reject);
        assert_eq!(
            s.outbox,
            vec![Intent::MutateDocument {
                document_id,
                mutation: DocumentMutation::RejectSuggestion {
                    suggestion_id: "s1".to_owned(),
                },
            }]
        );
    }

    #[test]
    fn suggestion_resolution_needs_the_review_rail_focused() {
        // On the tree rail, `a`/`r` resolve nothing (and, with the Docs overlay up,
        // they never touch a pending approval either).
        let mut s = AppState::new();
        s.docs = vec![doc("a")];
        reduce(&mut s, Action::OpenDocs); // Tree focus
        let _ = s.drain_outbox();
        reduce(&mut s, Action::Approve(ApprovalScope::Once));
        reduce(&mut s, Action::Reject);
        assert!(s.outbox.is_empty());
    }

    #[test]
    fn a_document_sync_replaces_the_matching_cards_content() {
        let mut s = AppState::new();
        s.docs = vec![doc("a"), doc("b")];
        let target = s.docs[1].document_id;
        s.selected_doc = 1;
        s.selected_block = 5; // stale cursor, must be re-clamped
        reduce(
            &mut s,
            Action::DocumentSynced {
                document_id: target,
                revision: "r9".to_owned(),
                blocks: vec![crate::state::DocBlockView {
                    id: "b1".to_owned(),
                    kind: "paragraph".to_owned(),
                    text: "merged".to_owned(),
                    editable: Some("merged".to_owned()),
                }],
                suggestions: vec![],
            },
        );
        assert_eq!(s.docs[1].revision, "r9");
        assert_eq!(s.docs[1].blocks[0].text, "merged");
        assert!(s.docs[1].suggestions.is_empty());
        assert_eq!(s.selected_block, 0, "the block cursor was re-clamped");
        // The other card is untouched.
        assert_eq!(s.docs[0].revision, "r3");
    }

    #[test]
    fn a_document_sync_for_an_unknown_document_is_inert() {
        let mut s = AppState::new();
        s.docs = vec![doc("a")];
        reduce(
            &mut s,
            Action::DocumentSynced {
                document_id: codypendent_protocol::DocumentId::new(),
                revision: "r9".to_owned(),
                blocks: vec![],
                suggestions: vec![],
            },
        );
        assert_eq!(s.docs[0].revision, "r3", "no card matched, nothing changed");
    }

    #[test]
    fn document_created_refreshes_the_docs_projection_and_notices_the_id() {
        let mut s = AppState::new();
        let document_id = DocumentId::new();
        reduce(&mut s, Action::DocumentCreated { document_id });
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::RefreshProjection {
                kind: ProjectionKind::Docs,
            }]
        );
        assert_eq!(s.pending_document_selection, Some(document_id));
        let notice = s.notice.expect("create acknowledgement is visible").0;
        assert!(notice.contains("document created"));
        assert!(notice.contains(&document_id.to_string()[..8]));
    }

    #[test]
    fn document_publish_prepared_projects_a_full_approval_without_duplication() {
        let mut s = AppState::new();
        let approval_id = ApprovalId::new();
        let document_id = DocumentId::new();
        let prepared = Action::DocumentPublishPrepared {
            approval_id,
            document_id,
            target: "repository file docs/runbook.md".to_owned(),
            changed_files: vec!["docs/runbook.md".to_owned()],
            git_action: "commit on branch docs/publish".to_owned(),
        };
        reduce(&mut s, prepared.clone());
        reduce(&mut s, prepared);

        assert_eq!(s.pending_approvals.len(), 1);
        let approval = &s.pending_approvals[0];
        assert_eq!(approval.approval_id, approval_id);
        assert_eq!(approval.risk.level, RiskLevel::High);
        assert_eq!(approval.run_id, None);
        assert_eq!(
            approval.action,
            ProposedAction::PublishDocument {
                document_id,
                target: "repository file docs/runbook.md".to_owned(),
                changed_files: vec!["docs/runbook.md".to_owned()],
                git_action: "commit on branch docs/publish".to_owned(),
            }
        );
        assert!(s.show_approval_modal());
    }

    #[test]
    fn dismissing_a_docs_subprompt_returns_to_docs_without_dropping_its_lease() {
        let mut s = AppState::new();
        let document_id = DocumentId::new();
        s.overlay = Overlay::DocEdit {
            block_id: "b1".to_owned(),
            buffer: "draft".to_owned(),
            original: "original".to_owned(),
        };
        s.doc_edit = Some(DocEdit {
            document_id,
            block_id: Some("b1".to_owned()),
            lease: DocLeaseState::Held,
            lease_id: Some("lease-keep".to_owned()),
            pending: None,
        });

        reduce(&mut s, Action::Dismiss);

        assert_eq!(s.overlay, Overlay::Docs);
        assert_eq!(
            s.doc_edit
                .as_ref()
                .and_then(|edit| edit.lease_id.as_deref()),
            Some("lease-keep")
        );
        assert!(s.outbox.is_empty(), "returning to Docs retains the lease");
    }

    #[test]
    fn closing_the_docs_browser_releases_a_held_lease() {
        let mut s = AppState::new();
        s.docs = vec![doc("a")];
        let document_id = s.docs[0].document_id;
        reduce(&mut s, Action::OpenDocs);
        reduce(&mut s, Action::CyclePane);
        reduce(&mut s, Action::EditDoc);
        reduce(&mut s, Action::InputChar('x'));
        reduce(&mut s, Action::InputSubmit);
        reduce(
            &mut s,
            Action::DocumentLeaseGranted {
                document_id,
                lease_id: "lease-7".to_owned(),
            },
        );
        let _ = s.drain_outbox();

        // Closing the browser (toggle `D`, or `Esc`) releases the held lease.
        reduce(&mut s, Action::OpenDocs);
        assert_eq!(s.overlay, Overlay::None);
        assert!(s.doc_edit.is_none());
        assert_eq!(
            s.outbox,
            vec![Intent::ReleaseDocumentLease {
                lease_id: "lease-7".to_owned(),
            }]
        );
    }

    #[test]
    fn replacing_or_detaching_from_docs_releases_a_held_lease() {
        for (action, expected_overlay) in [
            (
                Action::OpenPalette,
                Overlay::Palette {
                    query: String::new(),
                    selected: 0,
                },
            ),
            (Action::OpenIssues, Overlay::Issues),
            (Action::Help, Overlay::Help),
            (Action::Detach, Overlay::Docs),
        ] {
            let mut s = AppState::new();
            let document_id = codypendent_protocol::DocumentId::new();
            s.overlay = Overlay::Docs;
            s.doc_edit = Some(DocEdit {
                document_id,
                block_id: Some("block-1".to_owned()),
                lease: DocLeaseState::Held,
                lease_id: Some("lease-9".to_owned()),
                pending: None,
            });

            reduce(&mut s, action);

            assert_eq!(s.overlay, expected_overlay);
            assert!(s.doc_edit.is_none());
            assert_eq!(
                s.outbox,
                vec![Intent::ReleaseDocumentLease {
                    lease_id: "lease-9".to_owned(),
                }]
            );
        }
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
    fn edge_search_and_paging_request_bounded_database_pages() {
        let mut s = AppState::new();
        reduce(&mut s, Action::OpenEdges);
        assert!(s.edge_loading);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::SearchEdges {
                query: String::new(),
                page: 0,
            }]
        );

        reduce(&mut s, Action::OpenPalette); // `/` is graph search in this view
        assert_eq!(s.overlay, Overlay::EdgeSearch(String::new()));
        for c in "parser calls".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::Edges);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::SearchEdges {
                query: "parser calls".to_owned(),
                page: 0,
            }]
        );

        reduce(
            &mut s,
            Action::EdgesLoaded {
                edges: vec![edge("parser::parse", "lexer::next")],
                total: 230,
                query: "parser calls".to_owned(),
                page: 0,
            },
        );
        assert!(!s.edge_loading);
        reduce(&mut s, Action::ScrollPageDown);
        assert!(s.edge_loading);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::SearchEdges {
                query: "parser calls".to_owned(),
                page: 1,
            }]
        );
    }

    fn node(id: &str) -> crate::state::WorkflowNodeCard {
        crate::state::WorkflowNodeCard {
            workflow_id: "repair-github-check".to_owned(),
            workflow: "repair-github-check v1".to_owned(),
            workflow_run_id: Some("workflow-run-1".to_owned()),
            run_phase: "running".to_owned(),
            inputs: "pull_request:github_pull_request*".to_owned(),
            id: id.to_owned(),
            action: "tool repository.test".to_owned(),
            kind: "tool".to_owned(),
            state: "pending".to_owned(),
            agent: "—".to_owned(),
            model_policy: "—".to_owned(),
            workspace: "shared worktree".to_owned(),
            approval: "none".to_owned(),
            retry: "1 attempt".to_owned(),
            depends_on: "—".to_owned(),
            depends_on_ids: Vec::new(),
            outputs: "test_result".to_owned(),
            cost: "—".to_owned(),
            error: "—".to_owned(),
        }
    }

    fn card(id: &str, title: &str, status: &str, ordinal: i64) -> crate::state::KanbanCard {
        crate::state::KanbanCard {
            id: id.to_owned(),
            title: title.to_owned(),
            status: status.to_owned(),
            assignee: "\u{2014}".to_owned(),
            kind: "task".to_owned(),
            author: "agent".to_owned(),
            ordinal,
        }
    }

    #[test]
    fn the_board_lays_cards_out_in_column_order_and_never_hides_an_unknown_column() {
        let mut s = AppState::new();
        s.kanban = vec![
            card("c1", "second in todo", "todo", 1),
            card("c2", "in review", "review", 0),
            card("c3", "first in todo", "todo", 0),
            // A team's own column: shown in the FIRST column rather than dropped.
            card("c4", "triage me", "icebox", 0),
        ];
        let columns = s.kanban_columns();
        assert_eq!(columns.len(), 4);
        assert_eq!(columns[0].0, "todo");
        // Within a column, `ordinal` orders the cards, and a tie falls back to
        // the title so the board is stable rather than arbitrary
        // ("first in todo" < "triage me", both at ordinal 0).
        let todo: Vec<&str> = columns[0].1.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(todo, vec!["c3", "c4", "c1"]);
        assert!(columns[1].1.is_empty(), "doing is empty");
        assert_eq!(columns[2].1.len(), 1, "review holds one card");
        // Display order is column-major, and it is what `selected_card` indexes.
        let order: Vec<&str> = s
            .kanban_in_display_order()
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(order, vec!["c3", "c4", "c1", "c2"]);
        s.selected_card = 3;
        assert_eq!(s.focused_card().unwrap().id, "c2");
    }

    #[test]
    fn moving_a_card_emits_the_write_but_does_not_edit_the_pane() {
        // The pane is a projection of the store: it emits the intent and waits
        // for the daemon's superseding republish. Editing its own copy would let
        // the board show a move the daemon refused.
        let mut s = AppState::new();
        s.overlay = Overlay::Kanban;
        s.kanban = vec![card("c1", "wire the DAG viewer", "todo", 0)];
        reduce(&mut s, Action::MoveCardForward);
        assert_eq!(
            s.outbox,
            vec![Intent::MoveBoardCard {
                item_id: "c1".to_owned(),
                status: "doing".to_owned(),
            }]
        );
        assert_eq!(
            s.kanban[0].status, "todo",
            "the pane must not move the card itself"
        );

        // The ends of the board are no-ops, not wrapped moves.
        s.outbox.clear();
        reduce(&mut s, Action::MoveCardBack);
        assert!(s.outbox.is_empty(), "todo has nothing to its left");
        s.kanban[0].status = "done".to_owned();
        reduce(&mut s, Action::MoveCardForward);
        assert!(s.outbox.is_empty(), "done has nothing to its right");
    }

    #[test]
    fn a_column_move_outside_the_board_is_ignored() {
        // The horizontal arrows are global in `Normal` mode, so a → pressed
        // while reading the blackboard must not move a card off-screen.
        let mut s = AppState::new();
        s.kanban = vec![card("c1", "wire the DAG viewer", "todo", 0)];
        for overlay in [Overlay::None, Overlay::Blackboard, Overlay::Workflow] {
            s.overlay = overlay.clone();
            s.outbox.clear();
            reduce(&mut s, Action::MoveCardForward);
            assert!(
                s.outbox.is_empty(),
                "{overlay:?} must not move a board card"
            );
        }
    }

    #[test]
    fn a_live_board_delivery_merges_by_id_and_drops_a_superseded_revision() {
        let mut s = AppState::new();
        s.kanban = vec![card("c1", "old title", "todo", 0)];
        // The replacement a move produced arrives as its own delivery.
        reduce(
            &mut s,
            Action::BoardCardUpdated {
                card: card("c2", "old title", "doing", 0),
                superseded: false,
            },
        );
        // …and the superseded revision is removed rather than merged, so the
        // board never shows one card in two columns.
        reduce(
            &mut s,
            Action::BoardCardUpdated {
                card: card("c1", "old title", "todo", 0),
                superseded: true,
            },
        );
        assert_eq!(s.kanban.len(), 1);
        assert_eq!(s.kanban[0].id, "c2");
        assert_eq!(s.kanban[0].status, "doing");
    }

    #[test]
    fn opening_the_board_watches_it_and_toggles_closed() {
        let mut s = AppState::new();
        reduce(&mut s, Action::OpenKanban);
        assert_eq!(s.overlay, Overlay::Kanban);
        assert_eq!(s.outbox, vec![Intent::WatchBoard]);
        reduce(&mut s, Action::OpenKanban);
        assert_eq!(s.overlay, Overlay::None);
    }

    #[test]
    fn empty_kanban_create_prompt_emits_a_typed_card_intent() {
        let mut s = AppState::new();
        s.overlay = Overlay::Kanban;
        reduce(&mut s, Action::NewRun);
        assert!(matches!(s.overlay, Overlay::KanbanNew { .. }));
        reduce(
            &mut s,
            Action::InputPaste("Add a regression test for ACP reconnects".to_owned()),
        );
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::Kanban);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::CreateBoardCard {
                title: "Add a regression test for ACP reconnects".to_owned(),
            }]
        );
    }

    #[test]
    fn open_workflow_toggles_the_workflow_view() {
        let mut s = AppState::new();
        s.workflow = vec![node("inspect")];
        reduce(&mut s, Action::OpenWorkflow);
        assert_eq!(s.overlay, Overlay::Workflow);
        assert_eq!(s.input_mode(), crate::state::InputMode::Normal);
        reduce(&mut s, Action::OpenWorkflow);
        assert_eq!(s.overlay, Overlay::None);
    }

    #[test]
    fn empty_workflow_primary_action_drafts_a_reviewable_example() {
        let mut s = AppState::new();
        s.overlay = Overlay::Workflow;
        reduce(&mut s, Action::NewRun);
        assert_eq!(s.overlay, Overlay::None);
        assert!(s.composer.contains(".codypendent/workflows/example.yaml"));
        assert!(s.composer.contains("inspect, implement, and verify"));
        assert!(s.outbox.is_empty(), "drafting must never auto-submit");
    }

    #[test]
    fn workflow_navigation_moves_selection_within_the_graph() {
        let mut s = AppState::new();
        s.workflow = vec![node("inspect"), node("patch")];
        reduce(&mut s, Action::OpenWorkflow);
        assert_eq!(s.selected_node, 0);
        reduce(&mut s, Action::SelectNext);
        assert_eq!(s.selected_node, 1);
        reduce(&mut s, Action::SelectNext); // clamps at the end
        assert_eq!(s.selected_node, 1);
        reduce(&mut s, Action::SelectPrev);
        assert_eq!(s.selected_node, 0);
    }

    #[test]
    fn workflow_start_accepts_json_object_inputs_and_keeps_the_view_open() {
        let mut s = AppState::new();
        s.workflow = vec![node("inspect")];
        reduce(&mut s, Action::OpenWorkflow);
        let _ = s.drain_outbox(); // projection refresh + run watch

        reduce(&mut s, Action::NewRun);
        assert!(matches!(
            s.overlay,
            Overlay::WorkflowInputs { ref workflow_id, .. }
                if workflow_id == "repair-github-check"
        ));
        reduce(
            &mut s,
            Action::InputPaste(r#"{"pull_request":482}"#.to_owned()),
        );
        reduce(&mut s, Action::InputSubmit);

        assert_eq!(s.overlay, Overlay::Workflow);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::StartWorkflow {
                workflow_id: "repair-github-check".to_owned(),
                inputs: serde_json::json!({"pull_request": 482}),
            }]
        );
    }

    #[test]
    fn workflow_controls_emit_pause_resume_retry_and_confirmed_cancel() {
        let mut s = AppState::new();
        s.workflow = vec![node("inspect")];
        s.overlay = Overlay::Workflow;

        reduce(&mut s, Action::Pause);
        reduce(&mut s, Action::Reject); // `r` in the workflow view
        assert_eq!(
            s.drain_outbox(),
            vec![
                Intent::PauseWorkflow {
                    workflow_run_id: "workflow-run-1".to_owned(),
                },
                Intent::RetryWorkflowNode {
                    workflow_run_id: "workflow-run-1".to_owned(),
                    node_id: "inspect".to_owned(),
                },
            ]
        );

        s.workflow[0].run_phase = "paused".to_owned();
        reduce(&mut s, Action::Pause);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::ResumeWorkflow {
                workflow_run_id: "workflow-run-1".to_owned(),
            }]
        );

        reduce(&mut s, Action::Cancel);
        assert!(matches!(s.overlay, Overlay::ConfirmWorkflowCancel { .. }));
        reduce(&mut s, Action::ConfirmCancel);
        assert_eq!(s.overlay, Overlay::None);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::CancelWorkflow {
                workflow_run_id: "workflow-run-1".to_owned(),
            }]
        );
    }

    #[test]
    fn palette_opens_the_workflow_view() {
        // "workflow" routes through the palette to the workflow-graph overlay,
        // the discoverable front door in the conversation shell where a bare `W`
        // composes text.
        let mut s = AppState::new();
        s.workflow = vec![node("inspect")];
        reduce(&mut s, Action::OpenPalette);
        for c in "workflow".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::Workflow);
    }

    #[test]
    fn a_live_workflow_transition_overlays_the_matching_graph_card() {
        // T9: a live node transition folds into the graph view — the forever-`pending`
        // placeholder becomes the run's real state/cost/error, so `node_state_color`'s
        // non-pending branches come alive. Only the matching node id is touched.
        let mut s = AppState::new();
        s.workflow = vec![node("inspect"), node("verify")];

        reduce(
            &mut s,
            Action::WorkflowNodeUpdated {
                workflow_run_id: "workflow-run-1".to_owned(),
                node_id: "inspect".to_owned(),
                state: "completed".to_owned(),
                cost: "12s · 3 tool calls".to_owned(),
                error: "—".to_owned(),
            },
        );

        let inspect = s.workflow.iter().find(|c| c.id == "inspect").unwrap();
        assert_eq!(inspect.state, "completed");
        assert_eq!(inspect.cost, "12s · 3 tool calls");
        assert_eq!(inspect.error, "—");
        // The other node is untouched by the transition.
        let verify = s.workflow.iter().find(|c| c.id == "verify").unwrap();
        assert_eq!(verify.state, "pending");

        // A failing transition carries its reason, and the fold is idempotent — a
        // re-delivered transition writes the same values (overlap is harmless).
        reduce(
            &mut s,
            Action::WorkflowNodeUpdated {
                workflow_run_id: "workflow-run-1".to_owned(),
                node_id: "verify".to_owned(),
                state: "failed".to_owned(),
                cost: "—".to_owned(),
                error: "the test command exited 1".to_owned(),
            },
        );
        reduce(
            &mut s,
            Action::WorkflowNodeUpdated {
                workflow_run_id: "workflow-run-1".to_owned(),
                node_id: "verify".to_owned(),
                state: "failed".to_owned(),
                cost: "—".to_owned(),
                error: "the test command exited 1".to_owned(),
            },
        );
        let verify = s.workflow.iter().find(|c| c.id == "verify").unwrap();
        assert_eq!(verify.state, "failed");
        assert_eq!(verify.error, "the test command exited 1");
    }

    #[test]
    fn workflow_snapshot_updates_phase_and_every_matching_node() {
        let mut s = AppState::new();
        s.workflow = vec![node("inspect"), node("verify")];
        reduce(
            &mut s,
            Action::WorkflowSnapshotLoaded {
                workflow_run_id: "workflow-run-1".to_owned(),
                phase: "completed".to_owned(),
                nodes: vec![
                    crate::action::WorkflowNodeUpdate {
                        node_id: "inspect".to_owned(),
                        state: "completed".to_owned(),
                        cost: "4s · 1 tool call".to_owned(),
                        error: "—".to_owned(),
                    },
                    crate::action::WorkflowNodeUpdate {
                        node_id: "verify".to_owned(),
                        state: "completed".to_owned(),
                        cost: "7s · 2 tool calls".to_owned(),
                        error: "—".to_owned(),
                    },
                ],
            },
        );

        assert!(s.workflow.iter().all(|card| card.run_phase == "completed"));
        assert!(s.workflow.iter().all(|card| card.state == "completed"));
        assert_eq!(s.workflow[1].cost, "7s · 2 tool calls");
    }

    fn item(kind: &str) -> crate::state::BlackboardItemCard {
        crate::state::BlackboardItemCard {
            id: format!("item-{kind}"),
            workflow_run_id: "workflow-run-1".to_owned(),
            run: "repair-github-check · run 0f2a".to_owned(),
            kind: kind.to_owned(),
            summary: "the failing test asserts an off-by-one".to_owned(),
            author: "agent investigator".to_owned(),
            confidence: "0.85".to_owned(),
            evidence: "2 ref(s)".to_owned(),
            revision: "r1".to_owned(),
            superseded: false,
        }
    }

    #[test]
    fn open_blackboard_toggles_the_blackboard_view() {
        let mut s = AppState::new();
        s.blackboard = vec![item("finding")];
        reduce(&mut s, Action::OpenBlackboard);
        assert_eq!(s.overlay, Overlay::Blackboard);
        assert_eq!(s.input_mode(), crate::state::InputMode::Normal);
        reduce(&mut s, Action::OpenBlackboard);
        assert_eq!(s.overlay, Overlay::None);
    }

    #[test]
    fn blackboard_primary_action_posts_only_an_explicit_open_question() {
        let mut s = AppState::new();
        s.blackboard = vec![item("finding")];
        s.overlay = Overlay::Blackboard;
        reduce(&mut s, Action::NewRun);
        assert!(matches!(s.overlay, Overlay::BlackboardPost { .. }));
        reduce(
            &mut s,
            Action::InputPaste("What should independent review verify?".to_owned()),
        );
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::PostBlackboardQuestion {
                workflow_run_id: "workflow-run-1".to_owned(),
                text: "What should independent review verify?".to_owned(),
            }]
        );
    }

    #[test]
    fn blackboard_navigation_moves_selection_within_the_board() {
        let mut s = AppState::new();
        s.blackboard = vec![item("finding"), item("decision")];
        reduce(&mut s, Action::OpenBlackboard);
        assert_eq!(s.selected_item, 0);
        reduce(&mut s, Action::SelectNext);
        assert_eq!(s.selected_item, 1);
        reduce(&mut s, Action::SelectNext); // clamps at the end
        assert_eq!(s.selected_item, 1);
        reduce(&mut s, Action::SelectPrev);
        assert_eq!(s.selected_item, 0);
    }

    #[test]
    fn blackboard_baselines_replace_one_run_and_live_items_upsert_by_id() {
        let mut s = AppState::new();
        let mut old = item("finding");
        old.id = "stable-item".to_owned();
        let mut other_run = item("decision");
        other_run.id = "other-item".to_owned();
        other_run.workflow_run_id = "workflow-run-2".to_owned();
        s.blackboard = vec![old, other_run.clone()];

        let mut baseline = item("evidence");
        baseline.id = "stable-item".to_owned();
        baseline.summary = "authoritative baseline".to_owned();
        reduce(
            &mut s,
            Action::BlackboardLoaded {
                workflow_run_id: "workflow-run-1".to_owned(),
                items: vec![baseline.clone()],
            },
        );
        assert_eq!(s.blackboard, vec![other_run, baseline]);

        let mut live = item("evidence");
        live.id = "stable-item".to_owned();
        live.revision = "r9".to_owned();
        live.summary = "live revision".to_owned();
        reduce(&mut s, Action::BlackboardItemUpdated(live.clone()));
        assert_eq!(
            s.blackboard
                .iter()
                .find(|card| card.id == "stable-item")
                .unwrap(),
            &live
        );
        assert_eq!(s.blackboard.len(), 2, "upsert must not duplicate the item");
    }

    #[test]
    fn palette_opens_the_blackboard_view() {
        let mut s = AppState::new();
        s.blackboard = vec![item("finding")];
        reduce(&mut s, Action::OpenPalette);
        for c in "blackboard".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::Blackboard);
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
    fn palette_new_conversation_requests_an_in_place_session_swap() {
        let mut s = AppState::new();
        reduce(&mut s, Action::OpenPalette);
        for c in "conversation".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert!(!s.should_detach, "the TUI remains open");
        assert_eq!(s.drain_outbox(), vec![Intent::NewConversation]);
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
    fn a_second_empty_session_submit_is_retained_until_run_started() {
        let mut s = AppState::new();
        for c in "first".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.drain_outbox().len(), 1);
        assert!(s.pending_run_start.is_some());

        for c in "second".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert!(s.outbox.is_empty(), "must not launch a duplicate run");
        assert_eq!(s.composer, "second", "the second message remains editable");

        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "first".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        assert!(s.pending_run_start.is_none());
        assert_eq!(s.composer, "second");
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::QueuePrompt {
                text: "second".to_owned(),
                mode: AgentMode::Build,
                delivery: codypendent_protocol::PromptDelivery::Queue,
            }]
        );
    }

    #[test]
    fn an_unacknowledged_start_guard_times_out_and_returns_the_draft() {
        let mut s = AppState::new();
        s.composer = "first".to_owned();
        s.composer_cursor = s.composer.len();
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.drain_outbox().len(), 1);
        assert!(s.pending_run_start.is_some());
        assert!(
            s.composer.is_empty(),
            "the submitted draft left the composer"
        );

        // Ordinary latency keeps the guard...
        for _ in 0..PENDING_RUN_START_TIMEOUT_TICKS - 1 {
            reduce(&mut s, Action::Tick);
        }
        assert!(s.pending_run_start.is_some());

        // ...but an acknowledgement lost outright (connection dropped before
        // the first durable event) releases it instead of wedging every
        // future submit behind "a run is already starting".
        reduce(&mut s, Action::Tick);
        assert!(s.pending_run_start.is_none());
        assert_eq!(s.composer, "first", "the retained draft is handed back");
        assert_eq!(s.composer_cursor, s.composer.len());
        assert!(s
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("timed out")));

        // Submit works again immediately, and the retry restarts the clock.
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.drain_outbox().len(),
            1,
            "a fresh StartRun is dispatched after the timeout"
        );
        assert!(s.pending_run_start.is_some());
        for _ in 0..PENDING_RUN_START_TIMEOUT_TICKS - 1 {
            reduce(&mut s, Action::Tick);
        }
        assert!(
            s.pending_run_start.is_some(),
            "the retried start gets its own full timeout window"
        );
    }

    #[test]
    fn a_correlated_start_rejection_restores_the_original_composer_draft() {
        let mut s = AppState::new();
        s.composer = "first objective".to_owned();
        s.composer_cursor = s.composer.len();
        reduce(&mut s, Action::InputSubmit);
        let _ = s.drain_outbox();
        assert!(s.composer.is_empty());
        assert!(s.pending_run_start.is_some());

        reduce(
            &mut s,
            Action::RunStartRejected {
                reason: "session is closed".to_owned(),
            },
        );

        assert!(s.pending_run_start.is_none());
        assert_eq!(s.composer, "first objective");
        assert_eq!(s.composer_cursor, s.composer.len());
        assert!(s
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("draft restored")));
    }

    #[test]
    fn start_rejection_preserves_a_newer_composer_draft() {
        let mut s = AppState::new();
        s.composer = "first objective".to_owned();
        s.composer_cursor = s.composer.len();
        reduce(&mut s, Action::InputSubmit);
        let _ = s.drain_outbox();
        s.composer = "newer draft".to_owned();
        s.composer_cursor = s.composer.len();

        reduce(
            &mut s,
            Action::RunStartRejected {
                reason: "not admitted".to_owned(),
            },
        );

        assert_eq!(s.composer, "newer draft");
        assert_eq!(s.overlay, Overlay::NewRun("first objective".to_owned()));
    }

    #[test]
    fn alt_enter_inserts_a_newline_without_submitting() {
        let mut s = AppState::new();
        for c in "line one".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputNewline);
        for c in "line two".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        assert_eq!(s.composer, "line one\nline two");
        // Nothing was submitted — no run started, draft still intact.
        assert!(s.drain_outbox().is_empty());
        assert!(!s.composer.is_empty());
    }

    #[test]
    fn submitting_pushes_to_history_skipping_consecutive_dupes() {
        let mut s = AppState::new();
        for c in "first message".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.composer_history, vec!["first message".to_owned()]);

        // A repeat of the very same message is not pushed again.
        // No daemon is running in this reducer-only history test, so simulate
        // the durable acknowledgement that would clear admission in production.
        s.pending_run_start = None;
        for c in "first message".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.composer_history,
            vec!["first message".to_owned()],
            "consecutive duplicate must be skipped"
        );

        // A genuinely new message is appended.
        s.pending_run_start = None;
        for c in "second message".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.composer_history,
            vec!["first message".to_owned(), "second message".to_owned()]
        );
    }

    #[test]
    fn history_prev_stashes_the_in_progress_draft_and_walks_backward() {
        let mut s = AppState::new();
        for text in ["oldest", "newest"] {
            s.pending_run_start = None;
            for c in text.chars() {
                reduce(&mut s, Action::InputChar(c));
            }
            reduce(&mut s, Action::InputSubmit);
        }
        assert_eq!(
            s.composer_history,
            vec!["oldest".to_owned(), "newest".to_owned()]
        );

        // Start a fresh, in-progress draft — this must never be lost.
        for c in "in progress".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        assert_eq!(s.composer, "in progress");

        // First Up: stashes the in-progress draft, loads the newest entry.
        reduce(&mut s, Action::HistoryPrev);
        assert_eq!(s.composer, "newest");
        assert_eq!(s.composer_stash, Some("in progress".to_owned()));

        // Second Up: walks to the older entry.
        reduce(&mut s, Action::HistoryPrev);
        assert_eq!(s.composer, "oldest");

        // A third Up saturates at the oldest entry (no history before it).
        reduce(&mut s, Action::HistoryPrev);
        assert_eq!(s.composer, "oldest");
    }

    #[test]
    fn history_next_walks_forward_and_restores_the_stash_past_the_newest() {
        let mut s = AppState::new();
        for text in ["oldest", "newest"] {
            s.pending_run_start = None;
            for c in text.chars() {
                reduce(&mut s, Action::InputChar(c));
            }
            reduce(&mut s, Action::InputSubmit);
        }
        for c in "in progress".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::HistoryPrev); // -> "newest" (stash "in progress")
        reduce(&mut s, Action::HistoryPrev); // -> "oldest"
        assert_eq!(s.composer, "oldest");

        // Down walks back toward newer entries.
        reduce(&mut s, Action::HistoryNext);
        assert_eq!(s.composer, "newest");

        // Down again moves past the newest: the stashed draft comes back,
        // verbatim, and the walk is over (further Down is a no-op).
        reduce(&mut s, Action::HistoryNext);
        assert_eq!(s.composer, "in progress");
        assert_eq!(s.history_cursor, None);

        reduce(&mut s, Action::HistoryNext);
        assert_eq!(
            s.composer, "in progress",
            "Down with no active recall must be a no-op"
        );
    }

    #[test]
    fn history_prev_is_a_noop_with_empty_history() {
        let mut s = AppState::new();
        for c in "draft".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::HistoryPrev);
        assert_eq!(s.composer, "draft", "no history yet — nothing to recall");
        assert_eq!(s.history_cursor, None);
    }

    #[test]
    fn editing_a_recalled_entry_detaches_it_so_the_next_up_restashes() {
        let mut s = AppState::new();
        for c in "old one".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);

        for c in "working draft".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::HistoryPrev);
        assert_eq!(s.composer, "old one");
        assert_eq!(s.history_cursor, Some(0));

        // Typing into the recalled entry detaches it from history.
        reduce(&mut s, Action::InputChar('!'));
        assert_eq!(s.composer, "old one!");
        assert_eq!(s.history_cursor, None);

        // The next Up re-stashes *this* edited text, not the original stash.
        reduce(&mut s, Action::HistoryPrev);
        assert_eq!(s.composer, "old one");
        reduce(&mut s, Action::HistoryNext);
        assert_eq!(s.composer, "old one!", "the edited draft must not be lost");
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
                [Intent::QueuePrompt { text, mode: AgentMode::Build, delivery: codypendent_protocol::PromptDelivery::Queue }] if text == "also add tests"
            ),
            "expected a QueuePrompt intent, got {intents:?}"
        );
    }

    #[test]
    fn a_follow_up_after_a_run_completes_continues_the_conversation() {
        // Task 5 (continuous-session plan): once the selected run reaches a
        // terminal state, the composer's next message must continue the SAME
        // session — pushing `SubmitUserInput`, not a context-free `StartRun` —
        // so the daemon (Tasks 1-4) seeds it with the prior turns instead of
        // starting cold. The prior turn must stay visible; it is the render
        // side (not this reducer path) that keeps it in view.
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "fix the bug".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Completed {
                    summary: Some("done".to_owned()),
                },
                chronicle: artifact(),
            }),
        );
        assert!(!s.selected_run_is_active(), "the run is terminal");

        for c in "follow up".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);

        let intents = s.drain_outbox();
        assert!(
            matches!(
                intents.as_slice(),
                [Intent::SubmitUserInput {
                    text,
                    mode: AgentMode::Build,
                    // No model was ever pinned in this session, so the follow-up
                    // inherits the session's model server-side (carries None).
                    model: None,
                }] if text == "follow up"
            ),
            "expected a SubmitUserInput intent, got {intents:?}"
        );
    }

    #[test]
    fn follow_up_carries_the_pinned_model_for_an_instant_switch() {
        // The mid-conversation model switch: with a run already terminal, a
        // model pinned via the `/model` picker must ride on the very next
        // follow-up (`SubmitUserInput.model`), so the switch is instant and
        // applies in the SAME session rather than being silently dropped.
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "fix the bug".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Completed {
                    summary: Some("done".to_owned()),
                },
                chronicle: artifact(),
            }),
        );
        assert!(!s.selected_run_is_active(), "the run is terminal");

        // The operator re-picks a model mid-conversation.
        s.pending_model = Some(codypendent_protocol::ModelId("pinned-model-x".to_owned()));

        for c in "use the big model now".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);

        let intents = s.drain_outbox();
        assert!(
            matches!(
                intents.as_slice(),
                [Intent::SubmitUserInput { model: Some(m), .. }]
                    if m.0 == "pinned-model-x"
            ),
            "the follow-up must carry the current pin, got {intents:?}"
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
    fn paging_leaves_and_re_enters_follow_mode() {
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
        // The renderer would cache the bottom offset; simulate a tall transcript.
        s.transcript_max_scroll.set(50);
        assert!(s.runs[0].follow, "runs follow by default");

        // Paging up leaves follow, starting a page up from the true bottom.
        reduce(&mut s, Action::ScrollPageUp);
        assert!(!s.runs[0].follow);
        assert_eq!(s.runs[0].scroll, 40);

        // Paging back down to the bottom re-enters follow.
        reduce(&mut s, Action::ScrollPageDown);
        assert_eq!(s.runs[0].scroll, 50);
        assert!(s.runs[0].follow);
    }

    #[test]
    fn sending_a_message_re_follows_the_latest() {
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
        s.transcript_max_scroll.set(50);
        reduce(&mut s, Action::ScrollPageUp);
        assert!(!s.runs[0].follow);

        // Sending snaps the conversation back to the latest.
        for c in "keep going".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert!(s.runs[0].follow);
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
    fn workspace_side_pane_arrows_and_pages_move_that_panes_selection() {
        use crate::state::LayoutMode;

        let mut s = AppState::new();
        for index in 0..25 {
            reduce(
                &mut s,
                system_ev(EventBody::RunStarted {
                    run_id: RunId::new(),
                    objective: format!("run {index}"),
                    mode: AgentMode::Build,
                }),
            );
        }
        s.layout = LayoutMode::Workspace;
        s.focus = Pane::Sessions;
        s.selected_run = 0;
        s.composer = "draft must survive side-pane navigation".to_owned();
        s.composer_cursor = s.composer.len();

        reduce(&mut s, Action::SelectNext);
        assert_eq!(s.selected_run, 1);
        reduce(&mut s, Action::ScrollPageDown);
        assert_eq!(s.selected_run, 11, "PgDn jumps through the run pane");
        reduce(&mut s, Action::ScrollPageUp);
        assert_eq!(s.selected_run, 1);
        assert_eq!(s.composer, "draft must survive side-pane navigation");

        // Once focus returns to the center, the same page action scrolls the
        // selected transcript rather than moving the run cursor.
        s.focus = Pane::Transcript;
        s.transcript_max_scroll.set(50);
        reduce(&mut s, Action::ScrollPageUp);
        assert_eq!(s.selected_run, 1);
        assert_eq!(s.runs[1].scroll, 40);
        assert!(!s.runs[1].follow);
    }

    #[test]
    fn approval_page_keys_jump_the_preempting_approval_stack() {
        let mut s = AppState::new();
        for index in 0..15 {
            reduce(
                &mut s,
                system_ev(EventBody::ApprovalRequested {
                    approval_id: ApprovalId::new(),
                    action: ProposedAction::GitCommit {
                        repository: format!("acme/repo-{index}"),
                    },
                    risk: Risk {
                        level: RiskLevel::High,
                        reasons: vec!["writes Git history".to_owned()],
                    },
                    pattern: None,
                }),
            );
        }
        assert_eq!(s.input_mode(), crate::state::InputMode::Approval);
        reduce(&mut s, Action::SelectPageNext);
        assert_eq!(s.selected_approval, 6);
        reduce(&mut s, Action::SelectPagePrev);
        assert_eq!(s.selected_approval, 0);
    }

    #[test]
    fn approval_scroll_pages_the_modal_body_and_resets_on_focus_change() {
        let mut s = AppState::new();
        for index in 0..2 {
            reduce(
                &mut s,
                system_ev(EventBody::ApprovalRequested {
                    approval_id: ApprovalId::new(),
                    action: ProposedAction::GitCommit {
                        repository: format!("acme/repo-{index}"),
                    },
                    risk: Risk {
                        level: RiskLevel::High,
                        reasons: vec!["writes Git history".to_owned()],
                    },
                    pattern: None,
                }),
            );
        }
        assert!(s.show_approval_modal());
        // The renderer publishes the body's overflow each frame; simulate a
        // body taller than its card.
        s.approval_max_scroll.set(25);

        reduce(&mut s, Action::ScrollPageDown);
        assert_eq!(s.approval_scroll, 10);
        reduce(&mut s, Action::ScrollPageDown);
        reduce(&mut s, Action::ScrollPageDown);
        assert_eq!(
            s.approval_scroll, 25,
            "paging clamps at the largest offset that still fills the modal"
        );
        reduce(&mut s, Action::ScrollPageUp);
        assert_eq!(s.approval_scroll, 15);

        // Moving to a different stacked approval restarts its body at the top.
        reduce(&mut s, Action::SelectNext);
        assert_eq!(s.selected_approval, 1);
        assert_eq!(s.approval_scroll, 0);

        // So does a resolution handing the modal to the next approval.
        s.approval_scroll = 7;
        let focused = s
            .focused_approval()
            .expect("a focused approval")
            .approval_id;
        reduce(
            &mut s,
            system_ev(EventBody::ApprovalResolved {
                approval_id: focused,
                decision: ApprovalDecision::Approve,
            }),
        );
        assert_eq!(s.approval_scroll, 0);
    }

    fn model_card(id: &str, provider: &str) -> crate::state::ModelCard {
        crate::state::ModelCard {
            id: ModelId(id.to_owned()),
            provider: provider.to_owned(),
            readiness: ModelReadiness::Ready,
            location: None,
            cost_per_1k_usd: None,
            context_tokens: None,
        }
    }

    /// Open the model picker via the palette front door: `/` → filter "model"
    /// → Enter. Every other test below starts from this.
    fn open_model_picker(s: &mut AppState) {
        reduce(s, Action::OpenPalette);
        for c in "model".chars() {
            reduce(s, Action::InputChar(c));
        }
        reduce(s, Action::InputSubmit);
    }

    #[test]
    fn model_picker_pages_and_jumps_across_the_full_catalog() {
        let mut state = AppState::new();
        state.models = (0..30)
            .map(|index| model_card(&format!("model-{index:02}"), "catalog"))
            .collect();
        open_model_picker(&mut state);

        reduce(&mut state, Action::SelectPageNext);
        assert!(matches!(
            state.overlay,
            Overlay::ModelPicker { selected: 6, .. }
        ));
        assert_eq!(state.selected_model, 6);

        reduce(&mut state, Action::SelectLast);
        assert!(matches!(
            state.overlay,
            Overlay::ModelPicker { selected: 29, .. }
        ));
        assert_eq!(state.selected_model, 29);

        reduce(&mut state, Action::SelectFirst);
        assert!(matches!(
            state.overlay,
            Overlay::ModelPicker { selected: 0, .. }
        ));
        assert_eq!(state.selected_model, 0);
    }

    #[test]
    fn model_picker_delete_confirms_exact_row_then_emits_removal() {
        let mut state = AppState::new();
        state.models = vec![
            model_card("ollama/qwen", "openai-compatible"),
            model_card("acp/codex-acp#gpt-5.6-sol", "acp"),
        ];
        open_model_picker(&mut state);
        reduce(&mut state, Action::SelectNext);
        reduce(&mut state, Action::RemoveSelected);

        assert_eq!(
            state.overlay,
            Overlay::ConfirmModelRemove {
                model_id: "acp/codex-acp#gpt-5.6-sol".to_owned(),
                provider: "acp".to_owned(),
                query: String::new(),
                selected: 1,
            }
        );
        assert_eq!(state.input_mode(), crate::state::InputMode::Confirm);

        reduce(&mut state, Action::ConfirmCancel);
        assert_eq!(
            state.overlay,
            Overlay::ModelPicker {
                query: String::new(),
                selected: 1,
            }
        );
        assert_eq!(
            state.drain_outbox(),
            vec![Intent::RemoveModel {
                model_id: "acp/codex-acp#gpt-5.6-sol".to_owned(),
            }]
        );
    }

    #[test]
    fn model_picker_remove_cancel_restores_filter_and_cursor() {
        let mut state = AppState::new();
        state.models = vec![
            model_card("ollama/qwen", "openai-compatible"),
            model_card("ollama/coder", "openai-compatible"),
        ];
        state.overlay = Overlay::ModelPicker {
            query: "ollama".to_owned(),
            selected: 1,
        };
        state.selected_model = 1;

        reduce(&mut state, Action::RemoveSelected);
        reduce(&mut state, Action::Dismiss);

        assert_eq!(
            state.overlay,
            Overlay::ModelPicker {
                query: "ollama".to_owned(),
                selected: 1,
            }
        );
        assert!(state.drain_outbox().is_empty());
    }

    #[test]
    fn model_picker_blocks_pending_and_active_model_removal_and_rechecks_confirmation() {
        let mut pending = AppState::new();
        pending.models = vec![model_card("ollama/qwen", "openai-compatible")];
        pending.pending_model = Some(ModelId("ollama/qwen".to_owned()));
        open_model_picker(&mut pending);
        reduce(&mut pending, Action::RemoveSelected);
        assert!(matches!(pending.overlay, Overlay::ModelPicker { .. }));
        assert!(pending
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("switch models first")));
        assert!(pending.outbox.is_empty());

        let mut active = AppState::new();
        active.models = vec![model_card("acp/codex-acp#gpt", "acp")];
        let run = active.ensure_run(RunId::new(), "work".to_owned(), AgentMode::Build);
        run.state = RunState::Running;
        run.model = Some(ModelId("acp/codex-acp#gpt".to_owned()));
        open_model_picker(&mut active);
        reduce(&mut active, Action::RemoveSelected);
        assert!(matches!(active.overlay, Overlay::ModelPicker { .. }));
        assert!(active
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("active run")));
        assert!(active.outbox.is_empty());

        let mut raced = AppState::new();
        raced.models = vec![model_card("ollama/coder", "openai-compatible")];
        open_model_picker(&mut raced);
        reduce(&mut raced, Action::RemoveSelected);
        assert!(matches!(raced.overlay, Overlay::ConfirmModelRemove { .. }));
        raced.pending_model = Some(ModelId("ollama/coder".to_owned()));
        reduce(&mut raced, Action::ConfirmCancel);
        assert!(
            raced.outbox.is_empty(),
            "the confirmation must re-check live routing"
        );
        assert!(matches!(raced.overlay, Overlay::ModelPicker { .. }));
    }

    #[test]
    fn bare_acp_model_row_opens_the_supplier_model_catalogue() {
        let mut state = AppState::new();
        state.models = vec![model_card("acp/codex-acp", "acp")];
        open_model_picker(&mut state);

        reduce(&mut state, Action::InputSubmit);

        assert_eq!(
            state.outbox,
            vec![Intent::QueryProviderModels {
                provider_id: "codex-acp".to_owned(),
                api_key: None,
                refresh: false,
            }]
        );
        assert!(matches!(
            state.overlay,
            Overlay::AddModelQuerying {
                ref provider_id,
                api_key: None,
            } if provider_id == "codex-acp"
        ));
        assert_eq!(state.pending_model, None);
    }

    #[test]
    fn acp_supplier_failure_stays_retryable_and_live_choice_adds_concrete_model() {
        let mut state = AppState::new();
        state.models = vec![model_card("acp/kimi-code", "acp")];
        open_model_picker(&mut state);
        reduce(&mut state, Action::InputSubmit);
        state.drain_outbox();

        reduce(
            &mut state,
            Action::ProviderModelsFailed {
                provider_id: "kimi-code".to_owned(),
                reason: "login required".to_owned(),
            },
        );
        assert!(matches!(
            state.overlay,
            Overlay::AddModelPick {
                ref provider_id,
                ref models,
                ..
            } if provider_id == "kimi-code" && models.is_empty()
        ));
        reduce(&mut state, Action::RefreshProviderModels);
        assert_eq!(
            state.drain_outbox(),
            vec![Intent::QueryProviderModels {
                provider_id: "kimi-code".to_owned(),
                api_key: None,
                refresh: true,
            }]
        );
        reduce(
            &mut state,
            Action::ProviderModelsLoaded {
                provider_id: "kimi-code".to_owned(),
                models: vec![AddModelRow::live("kimi-k2.5")],
                origin: ModelListOrigin::Live,
            },
        );
        reduce(&mut state, Action::InputSubmit);
        assert!(matches!(
            state.drain_outbox().as_slice(),
            [Intent::AddModel {
                display_id,
                provider_id,
                model,
                ..
            }] if display_id == "kimi-code/kimi-k2.5"
                && provider_id == "kimi-code"
                && model == "kimi-k2.5"
        ));
    }

    #[test]
    fn pinned_acp_model_row_stages_the_concrete_model() {
        let mut state = AppState::new();
        state.models = vec![model_card("acp/codex-acp#gpt-5.6-sol", "acp")];
        open_model_picker(&mut state);

        reduce(&mut state, Action::InputSubmit);

        assert_eq!(
            state.pending_model,
            Some(ModelId("acp/codex-acp#gpt-5.6-sol".to_owned()))
        );
        assert!(state.outbox.is_empty());
        assert_eq!(state.overlay, Overlay::None);
    }

    fn open_council_builder(s: &mut AppState) {
        reduce(s, Action::OpenPalette);
        for c in "council".chars() {
            reduce(s, Action::InputChar(c));
        }
        // `/council` now opens the BROWSER (list / run / delete); the builder is
        // reached from it with `n`, the same overlay-contextual "new" the
        // workflow and docs browsers use.
        reduce(s, Action::InputSubmit);
        assert!(
            matches!(s.overlay, Overlay::CouncilBrowser),
            "the palette's council command should open the browser"
        );
        reduce(s, Action::NewRun);
    }

    #[test]
    fn council_builder_creates_a_typed_multi_model_intent() {
        let mut s = AppState::new();
        s.models = vec![
            model_card("claude-reviewer", "acp"),
            model_card("kimi-architect", "acp"),
            model_card("amp-chair", "acp"),
        ];
        open_council_builder(&mut s);
        assert!(matches!(
            s.overlay,
            Overlay::CouncilBuilder(CouncilBuilderState {
                step: CouncilBuilderStep::Name,
                ..
            })
        ));

        for c in "design-council".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit); // name -> description
        for c in "Challenge an architecture from independent perspectives".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit); // description -> first member

        reduce(&mut s, Action::InputSubmit); // claude model -> role
        for c in "security reviewer".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit); // add claude
        reduce(&mut s, Action::InputSubmit); // kimi model -> role
        for c in "systems architect".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit); // add kimi

        // With two members the first row is the explicit Continue action.
        reduce(&mut s, Action::InputSubmit); // members -> chair
        reduce(&mut s, Action::SelectNext);
        reduce(&mut s, Action::SelectNext);
        reduce(&mut s, Action::InputSubmit); // amp chair -> rounds
        reduce(&mut s, Action::SelectNext); // two rounds
        reduce(&mut s, Action::InputSubmit); // rounds -> review
        reduce(&mut s, Action::InputSubmit); // create

        let intents = s.drain_outbox();
        assert_eq!(
            intents,
            vec![Intent::CreateCouncil {
                name: "design-council".to_owned(),
                description: "Challenge an architecture from independent perspectives".to_owned(),
                members: vec![
                    ("claude-reviewer".to_owned(), "security reviewer".to_owned()),
                    ("kimi-architect".to_owned(), "systems architect".to_owned()),
                ],
                chair: "amp-chair".to_owned(),
                rounds: 2,
            }]
        );
        assert!(matches!(
            s.overlay,
            Overlay::CouncilBuilder(CouncilBuilderState {
                step: CouncilBuilderStep::Review,
                ..
            })
        ));
        reduce(
            &mut s,
            Action::CouncilCreated {
                name: "design-council".to_owned(),
                members: 2,
                rounds: 2,
            },
        );
        // Rubric 6: a successful save returns to the browser (its only entry
        // point is `n` from inside it), not the base view.
        assert_eq!(s.overlay, Overlay::CouncilBrowser);
    }

    #[test]
    fn council_builder_home_and_end_cover_each_list_step() {
        let mut state = AppState::new();
        state.models = vec![
            model_card("claude", "acp"),
            model_card("kimi", "acp"),
            model_card("amp", "acp"),
        ];
        state.overlay = Overlay::CouncilBuilder(CouncilBuilderState {
            step: CouncilBuilderStep::MemberModel,
            members: vec![
                CouncilMemberDraft {
                    model: "claude".to_owned(),
                    role: "reviewer".to_owned(),
                },
                CouncilMemberDraft {
                    model: "kimi".to_owned(),
                    role: "architect".to_owned(),
                },
            ],
            ..CouncilBuilderState::default()
        });
        reduce(&mut state, Action::SelectLast);
        assert!(matches!(
            state.overlay,
            Overlay::CouncilBuilder(CouncilBuilderState { selected: 2, .. })
        ));
        reduce(&mut state, Action::SelectFirst);
        assert!(matches!(
            state.overlay,
            Overlay::CouncilBuilder(CouncilBuilderState { selected: 0, .. })
        ));

        if let Overlay::CouncilBuilder(builder) = &mut state.overlay {
            builder.step = CouncilBuilderStep::Chair;
        }
        reduce(&mut state, Action::SelectLast);
        assert!(matches!(
            state.overlay,
            Overlay::CouncilBuilder(CouncilBuilderState { selected: 2, .. })
        ));

        if let Overlay::CouncilBuilder(builder) = &mut state.overlay {
            builder.step = CouncilBuilderStep::Rounds;
        }
        reduce(&mut state, Action::SelectLast);
        assert!(matches!(
            state.overlay,
            Overlay::CouncilBuilder(CouncilBuilderState {
                selected: 2,
                rounds: 3,
                ..
            })
        ));
        reduce(&mut state, Action::SelectFirst);
        assert!(matches!(
            state.overlay,
            Overlay::CouncilBuilder(CouncilBuilderState {
                selected: 0,
                rounds: 1,
                ..
            })
        ));
    }

    #[test]
    fn council_persistence_failure_keeps_the_reviewed_draft_open() {
        let mut s = AppState::new();
        s.overlay = Overlay::CouncilBuilder(CouncilBuilderState {
            step: CouncilBuilderStep::Review,
            name: "existing".to_owned(),
            description: String::new(),
            members: vec![
                CouncilMemberDraft {
                    model: "claude".to_owned(),
                    role: "reviewer".to_owned(),
                },
                CouncilMemberDraft {
                    model: "kimi".to_owned(),
                    role: "architect".to_owned(),
                },
            ],
            chair: Some("amp".to_owned()),
            rounds: 1,
            query: String::new(),
            selected: 0,
            pending_member_model: None,
            role: String::new(),
        });
        reduce(
            &mut s,
            Action::CouncilCreateFailed {
                name: "existing".to_owned(),
                error: "already exists".to_owned(),
            },
        );
        assert!(matches!(
            s.overlay,
            Overlay::CouncilBuilder(CouncilBuilderState {
                step: CouncilBuilderStep::Review,
                ..
            })
        ));
        assert!(s
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("already exists")));
    }

    #[test]
    fn council_builder_requires_two_profiles_and_supports_back_navigation() {
        let mut s = AppState::new();
        s.models = vec![model_card("only-model", "acp")];
        open_council_builder(&mut s);
        for c in "small".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        reduce(&mut s, Action::InputSubmit);
        assert!(matches!(
            s.overlay,
            Overlay::CouncilBuilder(CouncilBuilderState {
                step: CouncilBuilderStep::Description,
                ..
            })
        ));
        assert!(s
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("at least two")));

        reduce(&mut s, Action::InputCancel);
        assert!(matches!(
            s.overlay,
            Overlay::CouncilBuilder(CouncilBuilderState {
                step: CouncilBuilderStep::Name,
                ..
            })
        ));
        reduce(&mut s, Action::InputCancel);
        assert_eq!(s.overlay, Overlay::None);
    }

    fn council_card(name: &str, chair: &str, evidence: bool) -> crate::state::CouncilCard {
        crate::state::CouncilCard {
            name: name.to_owned(),
            description: String::new(),
            chair: chair.to_owned(),
            rounds: 1,
            evidence,
            members: vec![
                ("claude".to_owned(), "architect".to_owned()),
                ("codex".to_owned(), "critic".to_owned()),
            ],
        }
    }

    /// Rubric 6 (TUI wiring): `/council` opens the browser, not the builder —
    /// the builder is reached with `n` from inside it (a separate test below).
    #[test]
    fn palette_opens_the_council_browser() {
        let mut s = AppState::new();
        s.councils = vec![council_card("review-board", "chair", false)];
        reduce(&mut s, Action::OpenPalette);
        for c in "council".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::CouncilBrowser);
        assert_eq!(s.input_mode(), crate::state::InputMode::Normal);
    }

    #[test]
    fn council_browser_navigation_and_new_council_key() {
        let mut s = AppState::new();
        s.councils = vec![
            council_card("alpha", "chair", false),
            council_card("beta", "chair", true),
        ];
        s.overlay = Overlay::CouncilBrowser;
        assert_eq!(s.focused_council().map(|c| c.name.as_str()), Some("alpha"));
        reduce(&mut s, Action::SelectNext);
        assert_eq!(s.selected_council, 1);
        assert_eq!(s.focused_council().map(|c| c.name.as_str()), Some("beta"));
        reduce(&mut s, Action::SelectNext); // saturates at the last row
        assert_eq!(s.selected_council, 1);
        reduce(&mut s, Action::SelectPrev);
        assert_eq!(s.selected_council, 0);

        // `n` (Action::NewRun) opens the existing creation wizard from inside
        // the browser instead of starting a new conversation run.
        reduce(&mut s, Action::NewRun);
        assert!(matches!(s.overlay, Overlay::CouncilBuilder(_)));
    }

    #[test]
    fn council_browser_run_prompts_for_objective_and_emits_intent() {
        let mut s = AppState::new();
        s.councils = vec![council_card("review-board", "chair", false)];
        s.overlay = Overlay::CouncilBrowser;
        // `r` reuses the physical Reject key, disambiguated by overlay (the
        // same pattern Workflow already uses for `r` = retry).
        reduce(&mut s, Action::Reject);
        assert_eq!(
            s.overlay,
            Overlay::CouncilRunObjective {
                name: "review-board".to_owned(),
                buffer: String::new(),
            }
        );
        assert_eq!(s.input_mode(), crate::state::InputMode::Editing);

        // An empty objective is rejected with the prompt kept open.
        reduce(&mut s, Action::InputSubmit);
        assert!(matches!(s.overlay, Overlay::CouncilRunObjective { .. }));
        assert!(s
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("must not be empty")));
        assert!(s.drain_outbox().is_empty());

        for c in "Choose the storage engine".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::CouncilBrowser);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::RunCouncil {
                name: "review-board".to_owned(),
                objective: "Choose the storage engine".to_owned(),
            }]
        );
    }

    #[test]
    fn council_browser_delete_confirms_then_emits_intent() {
        let mut s = AppState::new();
        s.councils = vec![council_card("review-board", "chair", false)];
        s.overlay = Overlay::CouncilBrowser;
        reduce(&mut s, Action::DeleteCouncil);
        assert_eq!(
            s.overlay,
            Overlay::ConfirmCouncilDelete {
                name: "review-board".to_owned(),
            }
        );
        assert_eq!(s.input_mode(), crate::state::InputMode::Confirm);

        // Esc/`n` returns to the browser without deleting anything.
        reduce(&mut s, Action::Dismiss);
        assert_eq!(s.overlay, Overlay::CouncilBrowser);
        assert!(s.drain_outbox().is_empty());

        reduce(&mut s, Action::DeleteCouncil);
        // `y`/Enter (Action::ConfirmCancel — the shared confirm-mode key,
        // exactly like every other confirm overlay) commits the removal.
        reduce(&mut s, Action::ConfirmCancel);
        assert_eq!(s.overlay, Overlay::CouncilBrowser);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::DeleteCouncil {
                name: "review-board".to_owned(),
            }]
        );
    }

    #[test]
    fn council_deleted_action_closes_the_pending_confirm_and_clamps_selection() {
        let mut s = AppState::new();
        s.councils = vec![council_card("only-one", "chair", false)];
        s.overlay = Overlay::ConfirmCouncilDelete {
            name: "only-one".to_owned(),
        };
        // The harness reloads `councils` from disk before folding this
        // action (mirrors `load_model_cards` after `AddModel`), so the
        // deleted council is already gone by the time the reducer sees it.
        s.councils.clear();
        reduce(
            &mut s,
            Action::CouncilDeleted {
                name: "only-one".to_owned(),
            },
        );
        assert_eq!(s.overlay, Overlay::CouncilBrowser);
        assert_eq!(s.selected_council, 0);
        assert!(s
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("removed council")));
    }

    #[test]
    fn council_progress_folds_into_the_active_runs_transcript() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "unrelated conversation".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            Action::CouncilProgress {
                name: "review-board".to_owned(),
                result_id: "result-1".to_owned(),
                phase: crate::state::CouncilProgressPhase::RoundStarted,
                occurred_at: "2026-08-12T12:00:00Z".to_owned(),
                message: "round 1/1 — launching 2 member(s)".to_owned(),
                active_subagents: 2,
            },
        );
        assert_eq!(s.council_subagents, 2);
        let note = s.runs[0]
            .transcript
            .iter()
            .find_map(|entry| match entry {
                TranscriptEntry::Note { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("a Note entry for the council progress line");
        assert!(note.contains("review-board"));
        assert!(note.contains("launching 2 member(s)"));
    }

    #[test]
    fn council_run_finished_renders_the_chair_synthesis_into_the_transcript() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "unrelated conversation".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            Action::CouncilRunFinished {
                name: "review-board".to_owned(),
                result: Ok(Box::new(crate::state::CouncilRunSummary {
                    result_id: "result-1".to_owned(),
                    council: "review-board".to_owned(),
                    status: "completed".to_owned(),
                    objective: "choose storage".to_owned(),
                    started_at: "2026-08-12T12:00:00Z".to_owned(),
                    finished_at: "2026-08-12T12:01:00Z".to_owned(),
                    repository: "/repo".to_owned(),
                    origin_session_id: Some("s0".to_owned()),
                    evidence: false,
                    warnings: Vec::new(),
                    rounds: Vec::new(),
                    failure: None,
                    synthesis: "Adopt sqlite with a WAL-mode connection pool.".to_owned(),
                    participants: vec!["claude · architect · session s1 · run r1".to_owned()],
                    cost_line: "cost: 1200 tokens measured across 2/2 runs".to_owned(),
                    report_markdown: "/data/councils/review-board/report.md".to_owned(),
                })),
            },
        );
        let note = s.runs[0]
            .transcript
            .iter()
            .find_map(|entry| match entry {
                TranscriptEntry::Note { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("a Note entry for the chair synthesis");
        assert!(note.contains("Adopt sqlite"));
        assert!(note.contains("claude · architect"));
        assert!(note.contains("cost: 1200 tokens"));
        assert!(note.contains("report.md"));
        assert!(s
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("completed")
                && notice.contains("durable result ready")));

        // A failed run still surfaces the failure in the transcript rather
        // than being silently lost.
        reduce(
            &mut s,
            Action::CouncilRunFinished {
                name: "review-board".to_owned(),
                result: Err("council round 1 failed quorum (1 of 2 completed)".to_owned()),
            },
        );
        let failures: Vec<_> = s.runs[0]
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Note { text, .. } if text.contains("failed quorum") => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn palette_opens_the_model_picker() {
        let mut s = AppState::new();
        s.models = vec![model_card("local-qwen", "openai-compatible")];
        open_model_picker(&mut s);
        assert_eq!(
            s.overlay,
            Overlay::ModelPicker {
                query: String::new(),
                selected: 0,
            }
        );
        assert_eq!(s.input_mode(), crate::state::InputMode::Palette);
    }

    #[test]
    fn model_picker_navigation_moves_selection_and_resolves_the_focused_card() {
        let mut s = AppState::new();
        s.models = vec![
            model_card("local-qwen", "openai-compatible"),
            model_card("hosted-gpt", "openai-compatible"),
        ];
        open_model_picker(&mut s);
        assert_eq!(s.selected_model, 0);

        reduce(&mut s, Action::SelectNext);
        assert_eq!(
            s.overlay,
            Overlay::ModelPicker {
                query: String::new(),
                selected: 1,
            }
        );
        assert_eq!(
            s.selected_model, 1,
            "the resolved index tracks the filtered cursor"
        );
        assert_eq!(
            s.focused_model().map(|c| c.id.0.as_str()),
            Some("hosted-gpt")
        );

        reduce(&mut s, Action::SelectNext); // clamps at the end
        assert_eq!(s.selected_model, 1);
        reduce(&mut s, Action::SelectPrev);
        assert_eq!(s.selected_model, 0);
        assert_eq!(
            s.focused_model().map(|c| c.id.0.as_str()),
            Some("local-qwen")
        );
    }

    #[test]
    fn model_picker_filters_by_id_substring_and_resets_selection() {
        let mut s = AppState::new();
        s.models = vec![
            model_card("local-qwen", "openai-compatible"),
            model_card("hosted-gpt", "openai-compatible"),
        ];
        open_model_picker(&mut s);
        reduce(&mut s, Action::SelectNext); // move onto "hosted-gpt" first
        assert_eq!(s.selected_model, 1);

        for c in "qwen".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        // Filtering narrows the list to "local-qwen" and resets the cursor to
        // its top, resolving `selected_model` back to the matching full-list
        // index rather than leaving it pointing at the no-longer-visible row.
        match &s.overlay {
            Overlay::ModelPicker { query, selected } => {
                assert_eq!(query, "qwen");
                assert_eq!(*selected, 0);
            }
            other => panic!("expected the model picker, got {other:?}"),
        }
        assert_eq!(s.selected_model, 0);
        assert_eq!(
            s.focused_model().map(|c| c.id.0.as_str()),
            Some("local-qwen")
        );
    }

    #[test]
    fn model_picker_enter_stages_the_focused_model_and_emits_a_notice() {
        let mut s = AppState::new();
        s.models = vec![
            model_card("local-qwen", "openai-compatible"),
            model_card("hosted-gpt", "openai-compatible"),
        ];
        open_model_picker(&mut s);
        reduce(&mut s, Action::SelectNext); // focus "hosted-gpt"
        reduce(&mut s, Action::InputSubmit); // stage it

        assert_eq!(s.overlay, Overlay::None, "the picker closes on select");
        assert_eq!(s.pending_model, Some(ModelId("hosted-gpt".to_owned())));
        let notice = s.notice.as_ref().expect("a visible notice").0.clone();
        assert!(
            notice.contains("hosted-gpt"),
            "the notice names the staged model: {notice}"
        );
        assert!(
            notice.contains("next run"),
            "the notice explains staging is advisory: {notice}"
        );
    }

    #[test]
    fn model_picker_keeps_an_unavailable_model_open_and_refuses_to_stage_it() {
        let mut s = AppState::new();
        let mut unavailable = model_card("missing-local-model", "openai-compatible");
        unavailable.readiness =
            ModelReadiness::Unavailable("provider did not list this model".to_owned());
        s.models = vec![unavailable];
        open_model_picker(&mut s);

        reduce(&mut s, Action::InputSubmit);

        assert!(
            matches!(s.overlay, Overlay::ModelPicker { .. }),
            "the picker stays open so another model can be chosen"
        );
        assert_eq!(s.pending_model, None);
        assert!(
            s.notice
                .as_ref()
                .is_some_and(|(notice, _)| notice.contains("model unavailable")),
            "the refusal explains why the model cannot be staged"
        );
    }

    #[test]
    fn model_picker_enter_with_zero_matches_stages_nothing() {
        // Regression: `selected_model`'s `.unwrap_or(0)` fallback (see `nav`
        // and `edit_prompt`) points at the full list's row 0 whenever the
        // live query matches nothing — Enter must NOT silently stage that
        // row (the list is showing "no matching model", not row 0).
        let mut s = AppState::new();
        s.models = vec![
            model_card("local-qwen", "openai-compatible"),
            model_card("hosted-gpt", "openai-compatible"),
        ];
        open_model_picker(&mut s);
        for c in "zzz-no-such-model".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        assert!(
            crate::state::filter_models(&s.models, "zzz-no-such-model").is_empty(),
            "precondition: the query must match nothing"
        );

        reduce(&mut s, Action::InputSubmit);

        assert_eq!(
            s.overlay,
            Overlay::None,
            "the picker still closes (mirrors the palette's no-match submit)"
        );
        assert!(
            s.pending_model.is_none(),
            "a zero-match submit must not silently stage models[0]"
        );
        assert!(
            s.notice.is_none(),
            "a zero-match submit must not emit a staging notice"
        );
    }

    #[test]
    fn model_picker_escape_closes_without_staging() {
        let mut s = AppState::new();
        s.models = vec![model_card("local-qwen", "openai-compatible")];
        open_model_picker(&mut s);
        reduce(&mut s, Action::InputCancel);
        assert_eq!(s.overlay, Overlay::None);
        assert!(s.pending_model.is_none(), "Esc must not stage anything");
    }

    // --- Task 8: the `/provider` picker (mirrors the model-picker tests above) ---

    /// Bare live pick-list rows (ids only) — the shape a provider that answers
    /// with ids alone produces, which is what most flow tests care about.
    fn rows(ids: &[&str]) -> Vec<AddModelRow> {
        ids.iter().map(|id| AddModelRow::live(*id)).collect()
    }

    fn provider_card(
        id: &str,
        name: &str,
        protocol: &str,
        auth: &str,
        local: bool,
    ) -> crate::state::ProviderCard {
        crate::state::ProviderCard {
            id: id.to_owned(),
            name: name.to_owned(),
            protocol: protocol.to_owned(),
            auth: auth.to_owned(),
            local,
            requires_key: auth.starts_with("api-key"),
            // Mirrors the harness gate closely enough for reducer tests: an
            // OpenAI-compatible provider with an api-key/none auth badge lists.
            can_list_models: protocol == "openai-chat"
                && (auth.starts_with("api-key") || auth == "none"),
            available: protocol == "openai-chat" && (auth.starts_with("api-key") || auth == "none"),
            // The default card ships no curated models and no stored key; the
            // tests that exercise those paths set them explicitly.
            catalog_models: 0,
            has_key: false,
        }
    }

    /// Open the provider picker via the palette front door: `/` → filter
    /// "provider" → Enter. Every other test below starts from this.
    fn open_provider_picker(s: &mut AppState) {
        reduce(s, Action::OpenPalette);
        for c in "provider".chars() {
            reduce(s, Action::InputChar(c));
        }
        reduce(s, Action::InputSubmit);
    }

    #[test]
    fn onboard_triage_and_skip_confirm_are_palette_navigable() {
        let mut s = AppState::new();
        reduce(&mut s, Action::OpenOnboard);
        assert_eq!(s.input_mode(), crate::state::InputMode::Palette);
        assert!(matches!(
            s.overlay,
            Overlay::Onboard {
                step: OnboardStep::Triage { selected: 0 }
            }
        ));

        reduce(&mut s, Action::SelectNext);
        assert!(matches!(
            s.overlay,
            Overlay::Onboard {
                step: OnboardStep::Triage { selected: 1 }
            }
        ));
        reduce(&mut s, Action::InputCancel);
        assert!(matches!(
            s.overlay,
            Overlay::Onboard {
                step: OnboardStep::SkipConfirm { selected: 0 }
            }
        ));
        assert_eq!(s.input_mode(), crate::state::InputMode::Palette);

        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::None);
        assert_eq!(s.drain_outbox(), vec![Intent::SetOnboardSkipped]);
    }

    /// First-run setup is `InputMode::Palette` with no query buffer, so every
    /// printable key fell through `edit_prompt`'s `_ => {}` and was swallowed —
    /// typing `/model` ate six keystrokes and then `Enter` activated whatever
    /// triage row happened to be highlighted. The splash advertises `/`, and it
    /// is the only front door to the rest of the product.
    #[test]
    fn slash_opens_the_command_palette_from_first_run_setup() {
        let mut s = AppState::new();
        reduce(&mut s, Action::OpenOnboard);

        reduce(&mut s, Action::InputChar('/'));
        assert!(
            matches!(s.overlay, Overlay::Palette { .. }),
            "`/` opens the palette instead of vanishing: {:?}",
            s.overlay
        );

        // The query then filters normally rather than steering the triage list.
        for c in "model".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        let Overlay::Palette { query, .. } = &s.overlay else {
            unreachable!("still the palette")
        };
        assert_eq!(query, "model", "the keystrokes reached the filter");

        reduce(&mut s, Action::InputSubmit);
        assert!(
            matches!(s.overlay, Overlay::ModelPicker { .. }),
            "the highlighted command ran: {:?}",
            s.overlay
        );
    }

    #[test]
    fn escaping_that_palette_returns_to_first_run_setup() {
        let mut s = AppState::new();
        reduce(&mut s, Action::OpenOnboard);
        reduce(&mut s, Action::InputChar('/'));

        reduce(&mut s, Action::InputCancel);
        assert!(
            matches!(
                s.overlay,
                Overlay::Onboard {
                    step: OnboardStep::Triage { .. }
                }
            ),
            "Esc must not strand a zero-model operator in an inert chat: {:?}",
            s.overlay
        );
        assert!(!s.palette_from_onboard, "the return address is consumed");

        // A palette opened the ordinary way still closes to the base view.
        reduce(&mut s, Action::InputCancel); // triage → skip confirm
        reduce(&mut s, Action::InputSubmit); // skip setup
        reduce(&mut s, Action::InputChar('/'));
        assert!(matches!(s.overlay, Overlay::Palette { .. }));
        reduce(&mut s, Action::InputCancel);
        assert_eq!(s.overlay, Overlay::None);
    }

    #[test]
    fn onboard_provider_classes_are_mutually_exclusive_and_cannot_roam() {
        let mut hosted = provider_card("groq", "Groq", "openai-chat", "api-key: GROQ", false);
        hosted.available = true;
        let mut local = provider_card("ollama", "Ollama", "openai-chat", "none", true);
        local.available = true;
        let mut kimi = provider_card("kimi-code", "Kimi Code", "acp", "acp: local", true);
        kimi.available = true;
        s_assert_class(&hosted, OnboardProviderClass::Hosted);
        s_assert_class(&local, OnboardProviderClass::LocalEndpoint);
        s_assert_class(&kimi, OnboardProviderClass::AcpAgent);

        let mut s = AppState::new();
        s.providers = vec![hosted, local, kimi];
        reduce(&mut s, Action::OpenOnboard);
        // Select Local Endpoint. The scoped picker has exactly Ollama, even
        // though Kimi also advertises `local=true`.
        reduce(&mut s, Action::SelectNext);
        reduce(&mut s, Action::InputSubmit);
        assert!(matches!(
            s.overlay,
            Overlay::OnboardProviderPicker {
                class: OnboardProviderClass::LocalEndpoint,
                selected: 0,
                ..
            }
        ));
        reduce(&mut s, Action::SelectNext);
        assert_eq!(s.selected_provider, 1, "selection stays on Ollama");
        for c in "kimi".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert!(matches!(
            s.overlay,
            Overlay::OnboardProviderPicker {
                class: OnboardProviderClass::LocalEndpoint,
                ..
            }
        ));
    }

    fn s_assert_class(card: &crate::state::ProviderCard, expected: OnboardProviderClass) {
        for class in [
            OnboardProviderClass::Hosted,
            OnboardProviderClass::LocalEndpoint,
            OnboardProviderClass::AcpAgent,
        ] {
            assert_eq!(card.is_onboard_class(class), class == expected);
        }
    }

    #[test]
    fn esc_from_reused_add_model_flow_returns_to_onboard_provider_class() {
        let mut s = AppState::new();
        let mut hosted = provider_card("groq", "Groq", "openai-chat", "api-key: GROQ", false);
        hosted.available = true;
        s.providers = vec![hosted];
        reduce(&mut s, Action::OpenOnboard);
        reduce(&mut s, Action::InputSubmit); // Hosted -> scoped provider picker
        reduce(&mut s, Action::InputSubmit); // Groq -> key prompt
        assert!(matches!(s.overlay, Overlay::AddModelProviderKey { .. }));
        assert!(s.onboard_flow.is_some());

        reduce(&mut s, Action::InputCancel);
        assert!(matches!(
            s.overlay,
            Overlay::OnboardProviderPicker {
                class: OnboardProviderClass::Hosted,
                ..
            }
        ));
        assert!(s.onboard_flow.is_some());
    }

    #[test]
    fn onboard_completion_waits_for_matching_authoritative_runnable_refresh() {
        let mut s = AppState::new();
        let expected = ModelId("acp/kimi-code".to_owned());
        s.onboard_flow = Some(OnboardFlow {
            class: OnboardProviderClass::AcpAgent,
            provider_id: Some("kimi-code".to_owned()),
            awaiting_model: Some(expected.clone()),
        });
        s.overlay = Overlay::Onboard {
            step: OnboardStep::Validating {
                model_id: expected.clone(),
            },
        };

        reduce(
            &mut s,
            Action::RunnableModelsRefreshed {
                model_ids: vec![expected.clone()],
                onboard_attempt: Some(ModelId("acp/old-attempt".to_owned())),
            },
        );
        assert!(s.onboard_flow.is_some(), "stale refresh is ignored");
        assert!(s.drain_outbox().is_empty());

        reduce(
            &mut s,
            Action::RunnableModelsRefreshed {
                model_ids: vec![expected.clone()],
                onboard_attempt: Some(expected.clone()),
            },
        );
        assert_eq!(s.overlay, Overlay::None);
        assert_eq!(s.pending_model, Some(expected));
        assert_eq!(s.drain_outbox(), vec![Intent::SetOnboardComplete]);
    }

    #[test]
    fn failed_or_non_runnable_onboard_attempt_returns_to_scoped_picker() {
        let mut s = AppState::new();
        let mut kimi = provider_card("kimi-code", "Kimi Code", "acp", "acp: local", true);
        kimi.available = true;
        s.providers = vec![kimi];
        let expected = ModelId("acp/kimi-code".to_owned());
        s.onboard_flow = Some(OnboardFlow {
            class: OnboardProviderClass::AcpAgent,
            provider_id: Some("kimi-code".to_owned()),
            awaiting_model: Some(expected.clone()),
        });
        s.overlay = Overlay::Onboard {
            step: OnboardStep::Validating {
                model_id: expected.clone(),
            },
        };

        reduce(
            &mut s,
            Action::RunnableModelsRefreshed {
                model_ids: vec![],
                onboard_attempt: Some(expected.clone()),
            },
        );
        assert!(matches!(
            s.overlay,
            Overlay::OnboardProviderPicker {
                class: OnboardProviderClass::AcpAgent,
                ..
            }
        ));
        assert!(s.drain_outbox().is_empty());

        // Start another correlated attempt and verify a host write/connect
        // failure takes the same safe return path.
        if let Some(flow) = &mut s.onboard_flow {
            flow.awaiting_model = Some(expected.clone());
        }
        reduce(
            &mut s,
            Action::OnboardModelAddFailed {
                model_id: expected,
                reason: "agent exited during handshake".to_owned(),
            },
        );
        assert!(matches!(
            s.overlay,
            Overlay::OnboardProviderPicker {
                class: OnboardProviderClass::AcpAgent,
                ..
            }
        ));
    }

    #[test]
    fn empty_submit_reopens_onboard_for_configured_but_unrunnable_models() {
        let mut s = AppState::new();
        s.models = vec![model_card(
            "configured-but-missing-key",
            "openai-compatible",
        )];
        assert!(s.runnable_models.is_empty());

        reduce(&mut s, Action::InputSubmit);

        assert!(matches!(
            s.overlay,
            Overlay::Onboard {
                step: OnboardStep::Triage { .. }
            }
        ));
    }

    #[test]
    fn palette_opens_the_provider_picker() {
        let mut s = AppState::new();
        s.providers = vec![provider_card(
            "groq",
            "Groq",
            "openai-chat",
            "api-key: GROQ_API_KEY",
            false,
        )];
        open_provider_picker(&mut s);
        assert_eq!(
            s.overlay,
            Overlay::ProviderPicker {
                query: String::new(),
                selected: 0,
            }
        );
        assert_eq!(s.input_mode(), crate::state::InputMode::Palette);
    }

    #[test]
    fn antigravity_requires_explicit_risk_consent_before_install_or_probe() {
        let mut s = AppState::new();
        let mut antigravity = provider_card(
            "antigravity-acp",
            "Google Antigravity (community bridge)",
            "acp",
            "acp: verified install · third-party ToS risk",
            true,
        );
        antigravity.available = true;
        antigravity.can_list_models = true;
        s.providers = vec![antigravity];
        s.overlay = Overlay::ProviderPicker {
            query: "anti".to_owned(),
            selected: 0,
        };

        reduce(&mut s, Action::InputSubmit);

        assert!(matches!(
            s.overlay,
            Overlay::ConfirmCommunityAcpInstall {
                ref provider_id,
                ref query,
                selected: 0,
                onboard_class: None,
            } if provider_id == "antigravity-acp" && query == "anti"
        ));
        assert!(
            s.drain_outbox().is_empty(),
            "opening consent downloads nothing"
        );

        reduce(&mut s, Action::Dismiss);
        assert_eq!(
            s.overlay,
            Overlay::ProviderPicker {
                query: "anti".to_owned(),
                selected: 0,
            }
        );
    }

    #[test]
    fn antigravity_consent_enters_the_verified_acp_model_probe() {
        let mut s = AppState::new();
        s.overlay = Overlay::ConfirmCommunityAcpInstall {
            provider_id: "antigravity-acp".to_owned(),
            query: "anti".to_owned(),
            selected: 0,
            onboard_class: None,
        };

        reduce(&mut s, Action::ConfirmCancel);

        assert_eq!(
            s.overlay,
            Overlay::AddModelQuerying {
                provider_id: "antigravity-acp".to_owned(),
                api_key: None,
            }
        );
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::QueryProviderModels {
                provider_id: "antigravity-acp".to_owned(),
                api_key: None,
                refresh: false,
            }]
        );
    }

    #[test]
    fn installed_antigravity_bridge_does_not_repeat_the_consent_prompt() {
        let mut s = AppState::new();
        let mut antigravity = provider_card(
            "antigravity-acp",
            "Google Antigravity (community bridge)",
            "acp",
            "acp: local · pinned community bridge",
            true,
        );
        antigravity.available = true;
        antigravity.can_list_models = true;
        s.providers = vec![antigravity];
        s.overlay = Overlay::ProviderPicker {
            query: String::new(),
            selected: 0,
        };

        reduce(&mut s, Action::InputSubmit);

        assert!(matches!(s.overlay, Overlay::AddModelQuerying { .. }));
        assert!(matches!(
            s.drain_outbox().as_slice(),
            [Intent::QueryProviderModels { provider_id, .. }] if provider_id == "antigravity-acp"
        ));
    }

    #[test]
    fn provider_picker_navigation_moves_selection_and_resolves_the_focused_card() {
        let mut s = AppState::new();
        s.providers = vec![
            provider_card(
                "groq",
                "Groq",
                "openai-chat",
                "api-key: GROQ_API_KEY",
                false,
            ),
            provider_card("ollama", "Ollama (local)", "openai-chat", "none", true),
        ];
        open_provider_picker(&mut s);
        assert_eq!(s.selected_provider, 0);

        reduce(&mut s, Action::SelectNext);
        assert_eq!(
            s.overlay,
            Overlay::ProviderPicker {
                query: String::new(),
                selected: 1,
            }
        );
        assert_eq!(
            s.selected_provider, 1,
            "the resolved index tracks the filtered cursor"
        );
        assert_eq!(s.focused_provider().map(|c| c.id.as_str()), Some("ollama"));

        reduce(&mut s, Action::SelectNext); // clamps at the end
        assert_eq!(s.selected_provider, 1);
        reduce(&mut s, Action::SelectPrev);
        assert_eq!(s.selected_provider, 0);
        assert_eq!(s.focused_provider().map(|c| c.id.as_str()), Some("groq"));
    }

    #[test]
    fn provider_picker_filters_by_id_substring_and_resets_selection() {
        let mut s = AppState::new();
        s.providers = vec![
            provider_card(
                "groq",
                "Groq",
                "openai-chat",
                "api-key: GROQ_API_KEY",
                false,
            ),
            provider_card("ollama", "Ollama (local)", "openai-chat", "none", true),
        ];
        open_provider_picker(&mut s);
        reduce(&mut s, Action::SelectNext); // move onto "ollama" first
        assert_eq!(s.selected_provider, 1);

        for c in "groq".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        // Filtering narrows the list to "groq" and resets the cursor to its
        // top, resolving `selected_provider` back to the matching full-list
        // index rather than leaving it pointing at the no-longer-visible row.
        match &s.overlay {
            Overlay::ProviderPicker { query, selected } => {
                assert_eq!(query, "groq");
                assert_eq!(*selected, 0);
            }
            other => panic!("expected the provider picker, got {other:?}"),
        }
        assert_eq!(s.selected_provider, 0);
        assert_eq!(s.focused_provider().map(|c| c.id.as_str()), Some("groq"));
    }

    #[test]
    fn provider_picker_enter_begins_the_flow_for_the_focused_provider() {
        let mut s = AppState::new();
        s.providers = vec![
            provider_card(
                "groq",
                "Groq",
                "openai-chat",
                "api-key: GROQ_API_KEY",
                false,
            ),
            provider_card("ollama", "Ollama (local)", "openai-chat", "none", true),
        ];
        open_provider_picker(&mut s);
        reduce(&mut s, Action::SelectNext); // focus "ollama" (can-list local)
        reduce(&mut s, Action::InputSubmit); // Enter begins the flow

        assert_eq!(
            s.overlay,
            Overlay::AddModelQuerying {
                provider_id: "ollama".to_owned(),
                api_key: None,
            },
            "the picker gives way to the add-model flow, not a staged marker"
        );
        assert_eq!(
            s.outbox,
            vec![Intent::QueryProviderModels {
                provider_id: "ollama".to_owned(),
                api_key: None,
                refresh: false,
            }]
        );
    }

    #[test]
    fn provider_picker_enter_with_zero_matches_begins_nothing() {
        // Regression (mirrors the model-picker's own regression test):
        // `selected_provider`'s `.unwrap_or(0)` fallback (see `nav` and
        // `edit_prompt`) points at the full list's row 0 whenever the live
        // query matches nothing — Enter must NOT silently begin the flow for
        // that row.
        let mut s = AppState::new();
        s.providers = vec![
            provider_card(
                "groq",
                "Groq",
                "openai-chat",
                "api-key: GROQ_API_KEY",
                false,
            ),
            provider_card("ollama", "Ollama (local)", "openai-chat", "none", true),
        ];
        open_provider_picker(&mut s);
        for c in "zzz-no-such-provider".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        assert!(
            crate::state::filter_providers(&s.providers, "zzz-no-such-provider").is_empty(),
            "precondition: the query must match nothing"
        );

        reduce(&mut s, Action::InputSubmit);

        assert_eq!(s.overlay, Overlay::None, "the picker still closes");
        assert!(
            s.outbox.is_empty(),
            "a zero-match submit must begin no flow"
        );
    }

    #[test]
    fn provider_picker_escape_closes_the_picker() {
        let mut s = AppState::new();
        s.providers = vec![provider_card(
            "groq",
            "Groq",
            "openai-chat",
            "api-key: GROQ_API_KEY",
            false,
        )];
        open_provider_picker(&mut s);
        reduce(&mut s, Action::InputCancel);
        assert_eq!(s.overlay, Overlay::None);
        assert!(s.outbox.is_empty(), "Esc begins no flow");
    }

    // --- PR C2 (plan mode): the `/mode` picker (mirrors the pickers above) ---

    /// Open the mode picker via the palette front door: `/` → filter
    /// "mode picker" → Enter. ("mode" alone also substring-matches the Model
    /// picker's title, so the full row title is the unambiguous query.) Every
    /// other test below starts from this.
    fn open_mode_picker(s: &mut AppState) {
        reduce(s, Action::OpenPalette);
        for c in "mode picker".chars() {
            reduce(s, Action::InputChar(c));
        }
        reduce(s, Action::InputSubmit);
    }

    #[test]
    fn palette_opens_the_mode_picker_on_the_current_default() {
        let mut s = AppState::new();
        open_mode_picker(&mut s);
        // The cursor pre-selects the current `default_mode` (Build, the
        // fourth row) rather than the top of the list.
        assert_eq!(
            s.overlay,
            Overlay::ModePicker {
                query: String::new(),
                selected: 3,
            }
        );
        assert_eq!(s.input_mode(), crate::state::InputMode::Palette);
    }

    #[test]
    fn mode_picker_navigation_moves_the_selection() {
        let mut s = AppState::new();
        open_mode_picker(&mut s);
        reduce(&mut s, Action::SelectPrev); // Build (3) -> Plan (2)
        assert_eq!(
            s.overlay,
            Overlay::ModePicker {
                query: String::new(),
                selected: 2,
            }
        );
        reduce(&mut s, Action::SelectNext);
        reduce(&mut s, Action::SelectNext); // Plan -> Build -> Review (4)
        match &s.overlay {
            Overlay::ModePicker { selected, .. } => assert_eq!(*selected, 4),
            other => panic!("expected the mode picker, got {other:?}"),
        }
        reduce(&mut s, Action::SelectNext); // clamps at the end
        match &s.overlay {
            Overlay::ModePicker { selected, .. } => assert_eq!(*selected, 4),
            other => panic!("expected the mode picker, got {other:?}"),
        }
    }

    #[test]
    fn mode_picker_filters_by_label_and_resets_selection() {
        let mut s = AppState::new();
        open_mode_picker(&mut s);
        for c in "plan".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        match &s.overlay {
            Overlay::ModePicker { query, selected } => {
                assert_eq!(query, "plan");
                assert_eq!(*selected, 0, "the cursor resets to the filtered top");
            }
            other => panic!("expected the mode picker, got {other:?}"),
        }
        // "plan" matches only the Plan card (its summary names a "plan").
        assert_eq!(crate::state::filter_modes("plan"), vec![2]);
    }

    #[test]
    fn mode_picker_enter_sets_default_mode_and_emits_a_notice() {
        let mut s = AppState::new();
        open_mode_picker(&mut s);
        reduce(&mut s, Action::SelectPrev); // Build -> Plan
        reduce(&mut s, Action::InputSubmit);

        assert_eq!(s.overlay, Overlay::None, "the picker closes on select");
        assert_eq!(s.default_mode, AgentMode::Plan);
        let notice = s.notice.as_ref().expect("a visible notice").0.clone();
        assert!(
            notice.contains("Plan"),
            "the notice names the mode: {notice}"
        );
        assert!(
            notice.contains("next run"),
            "the notice explains when it applies: {notice}"
        );
    }

    #[test]
    fn mode_picker_enter_with_zero_matches_changes_nothing() {
        let mut s = AppState::new();
        open_mode_picker(&mut s);
        for c in "zzz-no-such-mode".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        assert!(
            crate::state::filter_modes("zzz-no-such-mode").is_empty(),
            "precondition: the query must match nothing"
        );

        reduce(&mut s, Action::InputSubmit);

        assert_eq!(s.overlay, Overlay::None, "the picker still closes");
        assert_eq!(
            s.default_mode,
            AgentMode::Build,
            "a zero-match submit must not change the mode"
        );
        assert!(
            s.notice.is_none(),
            "a zero-match submit must not emit a notice"
        );
    }

    #[test]
    fn mode_picker_escape_closes_without_changing_the_default() {
        let mut s = AppState::new();
        open_mode_picker(&mut s);
        reduce(&mut s, Action::SelectPrev); // move onto Plan, then abandon
        reduce(&mut s, Action::InputCancel);
        assert_eq!(s.overlay, Overlay::None);
        assert_eq!(
            s.default_mode,
            AgentMode::Build,
            "Esc must not stage anything"
        );
    }

    // -- `/keys` (D1): API key management ------------------------------------

    /// Seed two models + statuses and open the `/keys` overlay through the
    /// palette, mirroring `open_mode_picker`. `voice_rows` seeds the
    /// `[transcription]`/`[speech]` rows the harness contributes for whichever
    /// voice table `models.toml` configures — empty for the common case.
    fn open_api_keys_with_voice(s: &mut AppState, voice_rows: Vec<VoiceKeyRow>) {
        s.models = vec![
            crate::state::ModelCard {
                id: ModelId("groq/llama".to_owned()),
                provider: "openai-compatible".to_owned(),
                readiness: ModelReadiness::Ready,
                location: None,
                cost_per_1k_usd: None,
                context_tokens: None,
            },
            crate::state::ModelCard {
                id: ModelId("openai/gpt".to_owned()),
                provider: "openai-compatible".to_owned(),
                readiness: ModelReadiness::Ready,
                location: None,
                cost_per_1k_usd: None,
                context_tokens: None,
            },
        ];
        reduce(
            s,
            Action::ApiKeyStatusesLoaded {
                models: vec![
                    ("groq/llama".to_owned(), KeyStatus::Stored),
                    (
                        "openai/gpt".to_owned(),
                        KeyStatus::Env("OPENAI_API_KEY".to_owned()),
                    ),
                ],
                tavily: KeyStatus::Missing,
                voice: voice_rows,
            },
        );
        reduce(s, Action::OpenPalette);
        for c in "api keys".chars() {
            reduce(s, Action::InputChar(c));
        }
        reduce(s, Action::InputSubmit);
    }

    /// The common case: no voice table configured, so no voice rows.
    fn open_api_keys(s: &mut AppState) {
        open_api_keys_with_voice(s, Vec::new());
    }

    /// A `[transcription]` row exactly as the harness projects one.
    fn transcription_row(status: KeyStatus) -> VoiceKeyRow {
        VoiceKeyRow {
            target: KeyTarget::Transcription,
            label: "Voice input (speech-to-text)".to_owned(),
            detail: "whisper-large-v3-turbo · https://api.groq.com/openai/v1".to_owned(),
            status,
        }
    }

    #[test]
    fn palette_opens_the_api_keys_overlay_with_a_row_per_model_plus_tavily() {
        let mut s = AppState::new();
        open_api_keys(&mut s);
        assert_eq!(
            s.overlay,
            Overlay::ApiKeys {
                query: String::new(),
                selected: 0,
            }
        );
        assert_eq!(s.input_mode(), crate::state::InputMode::Palette);
        // Two model rows + the final Tavily row, in list order.
        assert_eq!(
            crate::state::filter_key_rows(&s.models, &s.voice_key_rows, ""),
            vec![0, 1, 2],
            "the rows are built from state (models, then Tavily)"
        );
    }

    #[test]
    fn api_keys_filters_by_model_id_and_resets_the_selection() {
        let mut s = AppState::new();
        open_api_keys(&mut s);
        reduce(&mut s, Action::SelectNext);
        for c in "gpt".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        match &s.overlay {
            Overlay::ApiKeys { query, selected } => {
                assert_eq!(query, "gpt");
                assert_eq!(*selected, 0, "the cursor resets to the filtered top");
            }
            other => panic!("expected the /keys overlay, got {other:?}"),
        }
        assert_eq!(
            crate::state::filter_key_rows(&s.models, &s.voice_key_rows, "gpt"),
            vec![1]
        );
        // The provider substring filters too (both models share it here).
        assert_eq!(
            crate::state::filter_key_rows(&s.models, &s.voice_key_rows, "openai-compatible"),
            vec![0, 1]
        );
        // The Tavily row matches its own label.
        assert_eq!(
            crate::state::filter_key_rows(&s.models, &s.voice_key_rows, "tavily"),
            vec![2]
        );
    }

    #[test]
    fn api_keys_enter_on_a_model_row_opens_the_masked_set_prompt() {
        let mut s = AppState::new();
        open_api_keys(&mut s);
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.overlay,
            Overlay::ApiKeySet {
                target: KeyTarget::Model("groq/llama".to_owned()),
                buffer: SecretKey(String::new()),
            },
            "Enter opens the masked set/replace prompt for the focused model"
        );
        assert_eq!(s.input_mode(), crate::state::InputMode::Editing);
    }

    #[test]
    fn api_keys_enter_on_the_tavily_row_targets_tavily() {
        let mut s = AppState::new();
        open_api_keys(&mut s);
        reduce(&mut s, Action::SelectNext);
        reduce(&mut s, Action::SelectNext); // row 2: Tavily
        reduce(&mut s, Action::InputSubmit);
        match &s.overlay {
            Overlay::ApiKeySet { target, .. } => {
                assert_eq!(*target, KeyTarget::Tavily)
            }
            other => panic!("expected the set prompt, got {other:?}"),
        }
    }

    /// The finding this closes (audio review F3): a `[transcription]`/`[speech]`
    /// table deserializes into its own `AudioModelConfig`, never a `ModelCard`,
    /// so before the voice rows existed `/keys` could name the STT/TTS
    /// credential from no index at all — while the user guide said it could.
    #[test]
    fn api_keys_enter_on_the_voice_row_targets_the_transcription_table() {
        let mut s = AppState::new();
        open_api_keys_with_voice(&mut s, vec![transcription_row(KeyStatus::Missing)]);
        // Two model rows, Tavily, then the one configured voice row.
        assert_eq!(
            crate::state::filter_key_rows(&s.models, &s.voice_key_rows, ""),
            vec![0, 1, 2, 3]
        );
        for _ in 0..3 {
            reduce(&mut s, Action::SelectNext);
        }
        reduce(&mut s, Action::InputSubmit);
        match &s.overlay {
            Overlay::ApiKeySet { target, .. } => assert_eq!(*target, KeyTarget::Transcription),
            other => panic!("expected the set prompt, got {other:?}"),
        }
    }

    /// The voice row is reachable by its own label/detail, not only by scrolling
    /// past every model — and filtering must not shift which target a row index
    /// resolves to.
    #[test]
    fn api_keys_filter_matches_a_voice_row_and_keeps_its_target() {
        let mut s = AppState::new();
        open_api_keys_with_voice(
            &mut s,
            vec![
                transcription_row(KeyStatus::Missing),
                VoiceKeyRow {
                    target: KeyTarget::Speech,
                    label: "Voice output (text-to-speech)".to_owned(),
                    detail: "tts-1 · https://api.openai.com/v1".to_owned(),
                    status: KeyStatus::Stored,
                },
            ],
        );
        assert_eq!(
            crate::state::filter_key_rows(&s.models, &s.voice_key_rows, "speech"),
            vec![3, 4],
            "both voice labels carry the word `speech`"
        );
        assert_eq!(
            crate::state::filter_key_rows(&s.models, &s.voice_key_rows, "text-to-speech"),
            vec![4]
        );
        assert_eq!(
            key_row_target(&s.models, &s.voice_key_rows, 4),
            KeyTarget::Speech
        );
    }

    /// `Delete` opens the remove confirm only for a row that HAS a stored key.
    /// A voice row's status lives on the row itself, not in `key_status` (which
    /// is keyed by model id), so this is the arm that could silently read the
    /// wrong row's status.
    #[test]
    fn api_keys_delete_on_a_voice_row_confirms_only_when_a_key_is_stored() {
        let mut s = AppState::new();
        open_api_keys_with_voice(&mut s, vec![transcription_row(KeyStatus::Missing)]);
        for _ in 0..3 {
            reduce(&mut s, Action::SelectNext);
        }
        reduce(&mut s, Action::RemoveSelected);
        assert!(
            matches!(s.overlay, Overlay::ApiKeys { .. }),
            "nothing is stored for this row, so there is nothing to remove"
        );

        let mut s = AppState::new();
        open_api_keys_with_voice(&mut s, vec![transcription_row(KeyStatus::Stored)]);
        for _ in 0..3 {
            reduce(&mut s, Action::SelectNext);
        }
        reduce(&mut s, Action::RemoveSelected);
        assert_eq!(
            s.overlay,
            Overlay::ApiKeyRemoveConfirm {
                target: KeyTarget::Transcription
            }
        );
    }

    /// End of the client-side round trip: the intent the harness turns into an
    /// `auth.json` write names the voice table, so the key lands where the
    /// runtime's `audio_api_key` looks for it.
    #[test]
    fn api_key_set_submit_on_a_voice_row_emits_a_transcription_intent() {
        let mut s = AppState::new();
        open_api_keys_with_voice(&mut s, vec![transcription_row(KeyStatus::Missing)]);
        for _ in 0..3 {
            reduce(&mut s, Action::SelectNext);
        }
        reduce(&mut s, Action::InputSubmit);
        for c in "sk-stt".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.outbox,
            vec![Intent::SetApiKey {
                target: KeyTarget::Transcription,
                key: SecretKey("sk-stt".to_owned()),
            }]
        );
    }

    #[test]
    fn api_key_set_submit_emits_the_intent_and_masks_the_buffer() {
        let mut s = AppState::new();
        open_api_keys(&mut s);
        reduce(&mut s, Action::InputSubmit); // -> ApiKeySet for groq/llama
        for c in "sk-new-key".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        // The buffer holds the typed key (rendered masked); the overlay's Debug
        // never shows it.
        match &s.overlay {
            Overlay::ApiKeySet { buffer, .. } => assert_eq!(buffer.0, "sk-new-key"),
            other => panic!("expected the set prompt, got {other:?}"),
        }
        assert!(!format!("{:?}", s.overlay).contains("sk-new-key"));

        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::None, "submitting closes the prompt");
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::SetApiKey {
                target: KeyTarget::Model("groq/llama".to_owned()),
                key: SecretKey("sk-new-key".to_owned()),
            }]
        );
    }

    #[test]
    fn api_key_set_submit_with_a_blank_key_emits_nothing() {
        let mut s = AppState::new();
        open_api_keys(&mut s);
        reduce(&mut s, Action::InputSubmit); // -> ApiKeySet
        reduce(&mut s, Action::InputSubmit); // blank buffer
        assert!(
            s.drain_outbox().is_empty(),
            "a blank key must never be written (the M1 shadow guard)"
        );
        let notice = s.notice.as_ref().expect("a visible notice").0.clone();
        assert!(notice.contains("blank"), "the notice says why: {notice}");
    }

    #[test]
    fn api_keys_delete_on_a_stored_row_confirms_then_emits_remove() {
        let mut s = AppState::new();
        open_api_keys(&mut s);
        // Row 0 (groq/llama) has KeyStatus::Stored.
        reduce(&mut s, Action::RemoveSelected);
        assert_eq!(
            s.overlay,
            Overlay::ApiKeyRemoveConfirm {
                target: KeyTarget::Model("groq/llama".to_owned()),
            },
            "Delete opens the remove confirm"
        );
        assert_eq!(s.input_mode(), crate::state::InputMode::Confirm);

        reduce(&mut s, Action::ConfirmCancel); // `y` maps here in Confirm mode
        assert_eq!(s.overlay, Overlay::None);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::RemoveApiKey {
                target: KeyTarget::Model("groq/llama".to_owned()),
            }]
        );
    }

    #[test]
    fn api_keys_delete_on_a_row_without_a_stored_key_is_a_no_op() {
        let mut s = AppState::new();
        open_api_keys(&mut s);
        reduce(&mut s, Action::SelectNext); // openai/gpt: Env, not Stored
        reduce(&mut s, Action::RemoveSelected);
        assert_eq!(
            s.overlay,
            Overlay::ApiKeys {
                query: String::new(),
                selected: 1,
            },
            "nothing stored → no confirm"
        );
        assert!(s.drain_outbox().is_empty());

        // Ordinary letters, including `d`, remain usable in the live filter.
        reduce(&mut s, Action::InputChar('d'));
        assert!(matches!(s.overlay, Overlay::ApiKeys { query, .. } if query == "d"));
    }

    #[test]
    fn api_key_remove_confirm_dismisses_without_an_intent() {
        let mut s = AppState::new();
        open_api_keys(&mut s);
        reduce(&mut s, Action::RemoveSelected);
        reduce(&mut s, Action::Dismiss); // `n`/Esc
        assert_eq!(s.overlay, Overlay::None);
        assert!(s.drain_outbox().is_empty());
    }

    #[test]
    fn a_run_started_after_picking_a_mode_carries_it() {
        // PR C2: the picked `default_mode` flows into the `StartRun` intent —
        // the plan → build handoff needs no wire change.
        let mut s = AppState::new();
        open_mode_picker(&mut s);
        reduce(&mut s, Action::SelectPrev); // Build -> Plan
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.default_mode, AgentMode::Plan);

        reduce(&mut s, Action::NewRun);
        for c in "plan the fix".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);

        assert_eq!(
            s.drain_outbox(),
            vec![Intent::StartRun {
                objective: "plan the fix".to_owned(),
                mode: AgentMode::Plan,
                model: None,
            }],
            "the started run carries the picked mode"
        );
    }

    #[test]
    fn a_follow_up_after_picking_a_mode_carries_it() {
        // A continuation (`SubmitUserInput`) reads the same `default_mode`:
        // reviewing the plan in Build is "switch mode, submit 'implement it'".
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "plan the fix".to_owned(),
                mode: AgentMode::Plan,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunStateChanged {
                run_id,
                state: RunState::Completed,
            }),
        );

        open_mode_picker(&mut s);
        reduce(&mut s, Action::SelectPrev); // cursor starts on Build (3) -> Plan (2)
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.default_mode, AgentMode::Plan);
        // Now flip to Build for the execution handoff: the picker reopens
        // with the cursor on Plan, so one step lands on Build.
        open_mode_picker(&mut s);
        reduce(&mut s, Action::SelectNext); // Plan -> Build
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.default_mode, AgentMode::Build);

        for c in "implement it".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);

        assert_eq!(
            s.drain_outbox(),
            vec![Intent::SubmitUserInput {
                text: "implement it".to_owned(),
                mode: AgentMode::Build,
                model: None,
            }],
            "the follow-up carries the re-picked mode"
        );
    }

    // --- Task 4: the add-model flow (pick provider -> name -> masked key -> emit) ---

    #[test]
    fn provider_picker_tab_begins_the_add_model_flow_for_the_focused_provider() {
        let mut s = AppState::new();
        s.providers = vec![
            provider_card(
                "groq",
                "Groq",
                "openai-chat",
                "api-key: GROQ_API_KEY",
                false,
            ),
            provider_card("ollama", "Ollama (local)", "openai-chat", "none", true),
        ];
        open_provider_picker(&mut s); // focuses row 0 (groq)
        reduce(&mut s, Action::BeginAddModel);
        assert_eq!(
            s.overlay,
            Overlay::AddModelProviderKey {
                provider_id: "groq".to_owned(),
                buffer: SecretKey(String::new()),
            }
        );
        assert_eq!(s.input_mode(), crate::state::InputMode::Editing);
    }

    #[test]
    fn catalog_only_hosted_provider_is_disabled_without_emitting() {
        let mut s = AppState::new();
        s.providers = vec![provider_card(
            "anthropic",
            "Anthropic",
            "anthropic",
            "api-key: ANTHROPIC_API_KEY",
            false,
        )];
        open_provider_picker(&mut s);
        reduce(&mut s, Action::BeginAddModel);
        assert!(matches!(s.overlay, Overlay::ProviderPicker { .. }));
        assert!(s.outbox.is_empty());
        assert!(s
            .notice
            .as_ref()
            .is_some_and(|(text, _)| text.contains("catalog-only")));
    }

    #[test]
    fn catalog_only_acp_provider_is_disabled_without_emitting() {
        let mut s = AppState::new();
        s.providers = vec![provider_card(
            "claude-code",
            "Claude Code (ACP)",
            "acp",
            "acp: npx",
            false,
        )];
        open_provider_picker(&mut s);
        reduce(&mut s, Action::BeginAddModel);
        assert!(matches!(s.overlay, Overlay::ProviderPicker { .. }));
        assert!(s.outbox.is_empty());
    }

    #[test]
    fn an_uninstalled_acp_provider_connects_without_asking_for_a_model_or_key() {
        let mut s = AppState::new();
        s.providers = vec![provider_card(
            "mistral-vibe",
            "Mistral Vibe",
            "acp",
            "acp: binary",
            false,
        )];
        // The shared helper derives runtime availability for native chat
        // providers; this fixture represents the CLI's verified ACP install
        // that is NOT yet launchable, so there is nothing to handshake for a
        // model list.
        s.providers[0].available = true;
        s.providers[0].can_list_models = false;
        open_provider_picker(&mut s);
        reduce(&mut s, Action::BeginAddModel);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::AddModel {
                display_id: "acp/mistral-vibe".to_string(),
                provider_id: "mistral-vibe".to_string(),
                model: "mistral-vibe".to_string(),
                api_key: None,
                context_tokens: None,
            }]
        );
        assert!(matches!(s.overlay, Overlay::None));
    }

    #[test]
    fn an_installed_acp_provider_queries_its_models_before_connecting() {
        // An installed agent can be handshaken, so it takes the same
        // query -> pick path a hosted provider does. The harness spawns the
        // agent instead of GETting `/models`; the overlay cannot tell.
        let mut s = AppState::new();
        s.providers = vec![provider_card(
            "mistral-vibe",
            "Mistral Vibe",
            "acp",
            "acp: binary",
            false,
        )];
        s.providers[0].available = true;
        s.providers[0].can_list_models = true;
        open_provider_picker(&mut s);
        reduce(&mut s, Action::BeginAddModel);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::QueryProviderModels {
                provider_id: "mistral-vibe".to_string(),
                // An ACP agent is never asked for an API key.
                api_key: None,
                refresh: false,
            }]
        );
        assert!(matches!(
            &s.overlay,
            Overlay::AddModelQuerying { provider_id, api_key: None } if provider_id == "mistral-vibe"
        ));
    }

    #[test]
    fn picking_an_acp_agents_model_adds_it_as_that_agents_model() {
        // The pick overlay carries the agent's OWN model ids; the harness turns
        // the chosen one into a pinned ACP profile.
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelQuerying {
            provider_id: "mistral-vibe".to_string(),
            api_key: None,
        };
        reduce(
            &mut s,
            Action::ProviderModelsLoaded {
                provider_id: "mistral-vibe".to_string(),
                // An agent advertises ids only — no catalog metadata exists for
                // a model that lives inside someone else's agent.
                models: vec![
                    AddModelRow::live("agent-model-1"),
                    AddModelRow::live("agent-model-2"),
                ],
                origin: ModelListOrigin::Live,
            },
        );
        assert!(matches!(s.overlay, Overlay::AddModelPick { .. }));
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::AddModel {
                display_id: "mistral-vibe/agent-model-1".to_string(),
                provider_id: "mistral-vibe".to_string(),
                model: "agent-model-1".to_string(),
                api_key: None,
                context_tokens: None,
            }],
            "the picked agent model must reach the harness verbatim"
        );
    }

    #[test]
    fn add_model_rejects_a_blank_model_name() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelId {
            provider_id: "custom".to_owned(),
            requires_key: true,
            api_key: None,
            buffer: String::new(),
        };
        reduce(&mut s, Action::InputSubmit); // empty buffer
        assert!(
            matches!(s.overlay, Overlay::AddModelId { .. }),
            "the prompt stays open on a blank name"
        );
        assert!(s.outbox.is_empty(), "no intent for a blank model name");
        assert!(s.notice.is_some(), "a notice explains the rejection");
    }

    #[test]
    fn add_model_escape_abandons_the_flow_without_emitting() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelId {
            provider_id: "custom".to_owned(),
            requires_key: true,
            api_key: None,
            buffer: String::new(),
        };
        for c in "x".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputCancel); // Esc on the model-name prompt
        assert_eq!(s.overlay, Overlay::None);
        assert!(s.outbox.is_empty());
    }

    // --- model discovery: Enter/Tab begin the add-model flow ---

    #[test]
    fn provider_picker_enter_can_list_hosted_opens_the_key_prompt() {
        let mut s = AppState::new();
        // groq: openai-chat + api-key → can_list + requires_key.
        s.providers = vec![provider_card(
            "groq",
            "Groq",
            "openai-chat",
            "api-key: GROQ_API_KEY",
            false,
        )];
        open_provider_picker(&mut s); // focuses groq
        reduce(&mut s, Action::InputSubmit); // Enter begins the flow
        assert_eq!(
            s.overlay,
            Overlay::AddModelProviderKey {
                provider_id: "groq".to_owned(),
                buffer: SecretKey(String::new()),
            }
        );
        assert!(s.outbox.is_empty(), "no query until the key is entered");
    }

    #[test]
    fn provider_picker_enter_can_list_local_queries_immediately() {
        let mut s = AppState::new();
        // ollama: openai-chat + none → can_list, no key.
        s.providers = vec![provider_card(
            "ollama",
            "Ollama (local)",
            "openai-chat",
            "none",
            true,
        )];
        open_provider_picker(&mut s);
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.outbox,
            vec![Intent::QueryProviderModels {
                provider_id: "ollama".to_owned(),
                api_key: None,
                refresh: false,
            }]
        );
        assert_eq!(
            s.overlay,
            Overlay::AddModelQuerying {
                provider_id: "ollama".to_owned(),
                api_key: None,
            }
        );
    }

    #[test]
    fn provider_picker_enter_on_catalog_only_provider_stays_open() {
        let mut s = AppState::new();
        // anthropic: native protocol → cannot list, but needs a key.
        s.providers = vec![provider_card(
            "anthropic",
            "Anthropic",
            "anthropic",
            "api-key: ANTHROPIC_API_KEY",
            false,
        )];
        open_provider_picker(&mut s);
        reduce(&mut s, Action::InputSubmit);
        assert!(matches!(s.overlay, Overlay::ProviderPicker { .. }));
        assert!(s.outbox.is_empty());
        assert!(s
            .notice
            .as_ref()
            .is_some_and(|(text, _)| text.contains("catalog-only")));
    }

    #[test]
    fn provider_picker_tab_and_enter_take_the_same_branch() {
        let providers = vec![provider_card(
            "groq",
            "Groq",
            "openai-chat",
            "api-key: GROQ_API_KEY",
            false,
        )];

        let mut via_enter = AppState::new();
        via_enter.providers = providers.clone();
        open_provider_picker(&mut via_enter);
        reduce(&mut via_enter, Action::InputSubmit);

        let mut via_tab = AppState::new();
        via_tab.providers = providers;
        open_provider_picker(&mut via_tab);
        reduce(&mut via_tab, Action::BeginAddModel);

        assert_eq!(via_enter.overlay, via_tab.overlay);
        assert!(matches!(
            via_enter.overlay,
            Overlay::AddModelProviderKey { .. }
        ));
    }

    // --- model discovery: Action handlers (isolated, no Enter/Tab flow) ---

    #[test]
    fn provider_models_loaded_opens_the_pick_list_carrying_the_key() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelQuerying {
            provider_id: "groq".to_owned(),
            api_key: Some(SecretKey("sk-secret".to_owned())),
        };
        reduce(
            &mut s,
            Action::ProviderModelsLoaded {
                provider_id: "groq".to_owned(),
                models: rows(&["llama-3.1-8b", "llama-3.3-70b"]),
                origin: ModelListOrigin::Live,
            },
        );
        assert_eq!(
            s.overlay,
            Overlay::AddModelPick {
                provider_id: "groq".to_owned(),
                api_key: Some(SecretKey("sk-secret".to_owned())),
                models: rows(&["llama-3.1-8b", "llama-3.3-70b"]),
                query: String::new(),
                selected: 0,
                origin: ModelListOrigin::Live,
                refreshing: false,
            }
        );
    }

    #[test]
    fn provider_models_loaded_for_a_mismatched_provider_is_ignored() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelQuerying {
            provider_id: "groq".to_owned(),
            api_key: None,
        };
        reduce(
            &mut s,
            Action::ProviderModelsLoaded {
                provider_id: "ollama".to_owned(),
                models: rows(&["qwen"]),
                origin: ModelListOrigin::Live,
            },
        );
        assert_eq!(
            s.overlay,
            Overlay::AddModelQuerying {
                provider_id: "groq".to_owned(),
                api_key: None,
            },
            "a stale result for another provider must not replace the overlay"
        );
    }

    #[test]
    fn provider_models_failed_falls_back_to_free_text_carrying_the_key() {
        let mut s = AppState::new();
        // Hosted + listable, and it already has a real key on the failed query —
        // the card's own `requires_key: true` must agree with `api_key.is_some()`.
        s.providers = vec![provider_card(
            "groq",
            "Groq",
            "openai-chat",
            "api-key: GROQ_API_KEY",
            false,
        )];
        s.overlay = Overlay::AddModelQuerying {
            provider_id: "groq".to_owned(),
            api_key: Some(SecretKey("sk-secret".to_owned())),
        };
        reduce(
            &mut s,
            Action::ProviderModelsFailed {
                provider_id: "groq".to_owned(),
                reason: "HTTP 401".to_owned(),
            },
        );
        assert_eq!(
            s.overlay,
            Overlay::AddModelId {
                provider_id: "groq".to_owned(),
                requires_key: true,
                api_key: Some(SecretKey("sk-secret".to_owned())),
                buffer: String::new(),
            }
        );
        let notice = s.notice.as_ref().expect("a fallback notice").0.clone();
        assert!(
            notice.contains("HTTP 401"),
            "the notice explains why: {notice}"
        );
    }

    #[test]
    fn provider_models_failed_for_a_hosted_provider_blank_key_still_requires_key() {
        // Regression for the add-model bug: a hosted+listable provider (e.g.
        // groq) queried with a BLANK key (`api_key: None`) fails (401), and the
        // free-text fallback must still know this provider needs a key — derived
        // from the provider's own catalog card, not from whether a key was typed
        // on this particular query. Getting this wrong lets a keyless, unrunnable
        // hosted model be added with no way back to the key prompt.
        let mut s = AppState::new();
        s.providers = vec![provider_card(
            "groq",
            "Groq",
            "openai-chat",
            "api-key: GROQ_API_KEY",
            false,
        )];
        s.overlay = Overlay::AddModelQuerying {
            provider_id: "groq".to_owned(),
            api_key: None,
        };
        reduce(
            &mut s,
            Action::ProviderModelsFailed {
                provider_id: "groq".to_owned(),
                reason: "HTTP 401".to_owned(),
            },
        );
        assert_eq!(
            s.overlay,
            Overlay::AddModelId {
                provider_id: "groq".to_owned(),
                requires_key: true,
                api_key: None,
                buffer: String::new(),
            },
            "a hosted provider must still require a key on fallback even though \
             this particular query carried none"
        );
    }

    #[test]
    fn provider_models_failed_for_a_local_provider_falls_back_with_no_key() {
        let mut s = AppState::new();
        // Local + listable + no-auth — the card's `requires_key: false` must
        // still hold on fallback.
        s.providers = vec![provider_card(
            "ollama",
            "Ollama (local)",
            "openai-chat",
            "none",
            true,
        )];
        s.overlay = Overlay::AddModelQuerying {
            provider_id: "ollama".to_owned(),
            api_key: None,
        };
        reduce(
            &mut s,
            Action::ProviderModelsFailed {
                provider_id: "ollama".to_owned(),
                reason: "could not connect to the provider".to_owned(),
            },
        );
        assert_eq!(
            s.overlay,
            Overlay::AddModelId {
                provider_id: "ollama".to_owned(),
                requires_key: false,
                api_key: None,
                buffer: String::new(),
            }
        );
    }

    #[test]
    fn provider_models_failed_for_a_mismatched_provider_is_ignored() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelQuerying {
            provider_id: "groq".to_owned(),
            api_key: None,
        };
        reduce(
            &mut s,
            Action::ProviderModelsFailed {
                provider_id: "ollama".to_owned(),
                reason: "x".to_owned(),
            },
        );
        assert!(matches!(s.overlay, Overlay::AddModelQuerying { .. }));
    }

    // --- model discovery: new overlay submit arms (isolated) ---

    #[test]
    fn add_model_provider_key_submit_queries_with_the_key() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelProviderKey {
            provider_id: "groq".to_owned(),
            buffer: SecretKey("sk-secret".to_owned()),
        };
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.outbox,
            vec![Intent::QueryProviderModels {
                provider_id: "groq".to_owned(),
                api_key: Some(SecretKey("sk-secret".to_owned())),
                refresh: false,
            }]
        );
        assert_eq!(
            s.overlay,
            Overlay::AddModelQuerying {
                provider_id: "groq".to_owned(),
                api_key: Some(SecretKey("sk-secret".to_owned())),
            }
        );
    }

    #[test]
    fn add_model_provider_key_blank_stays_in_the_required_key_prompt() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelProviderKey {
            provider_id: "groq".to_owned(),
            buffer: SecretKey(String::new()),
        };
        reduce(&mut s, Action::InputSubmit);
        assert!(s.outbox.is_empty());
        assert_eq!(
            s.overlay,
            Overlay::AddModelProviderKey {
                provider_id: "groq".to_owned(),
                buffer: SecretKey(String::new()),
            }
        );
        assert!(s
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("cannot be blank")));
    }

    #[test]
    fn add_model_pick_submit_emits_add_model_with_the_key() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelPick {
            provider_id: "groq".to_owned(),
            api_key: Some(SecretKey("sk-secret".to_owned())),
            models: rows(&["llama-3.1-8b", "llama-3.3-70b"]),
            query: String::new(),
            selected: 1,
            origin: ModelListOrigin::Live,
            refreshing: false,
        };
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::None);
        assert_eq!(
            s.outbox,
            vec![Intent::AddModel {
                display_id: "groq/llama-3.3-70b".to_owned(),
                provider_id: "groq".to_owned(),
                model: "llama-3.3-70b".to_owned(),
                api_key: Some(SecretKey("sk-secret".to_owned())),
                context_tokens: None,
            }]
        );
    }

    #[test]
    fn add_model_pick_zero_match_emits_nothing() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelPick {
            provider_id: "groq".to_owned(),
            api_key: None,
            models: rows(&["llama-3.1-8b"]),
            query: "zzz-nope".to_owned(),
            selected: 0,
            origin: ModelListOrigin::Live,
            refreshing: false,
        };
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::None, "the picker still closes");
        assert!(s.outbox.is_empty(), "a zero-match submit adds nothing");
    }

    #[test]
    fn add_model_pick_filters_and_navigates() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelPick {
            provider_id: "groq".to_owned(),
            api_key: None,
            models: rows(&["llama-3.1-8b", "gpt-oss-20b"]),
            query: String::new(),
            selected: 1,
            origin: ModelListOrigin::Live,
            refreshing: false,
        };
        // Typing resets the selection to the top of the new filtered set.
        for c in "gpt".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        match &s.overlay {
            Overlay::AddModelPick {
                query, selected, ..
            } => {
                assert_eq!(query, "gpt");
                assert_eq!(*selected, 0);
            }
            other => panic!("expected the pick-list, got {other:?}"),
        }
        // Down clamps at the single filtered row.
        reduce(&mut s, Action::SelectNext);
        match &s.overlay {
            Overlay::AddModelPick { selected, .. } => assert_eq!(*selected, 0),
            other => panic!("expected the pick-list, got {other:?}"),
        }
    }

    #[test]
    fn add_model_id_with_a_captured_key_emits_directly_without_re_prompting() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelId {
            provider_id: "groq".to_owned(),
            requires_key: true,
            api_key: Some(SecretKey("sk-secret".to_owned())),
            buffer: "llama-3.1-8b".to_owned(),
        };
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.overlay,
            Overlay::None,
            "no AddModelKey step — key already held"
        );
        assert_eq!(
            s.outbox,
            vec![Intent::AddModel {
                display_id: "groq/llama-3.1-8b".to_owned(),
                provider_id: "groq".to_owned(),
                model: "llama-3.1-8b".to_owned(),
                api_key: Some(SecretKey("sk-secret".to_owned())),
                context_tokens: None,
            }]
        );
    }

    /// The picked row's context window rides the intent, so the model is
    /// persisted with a real context instead of the `None` every discovered
    /// model used to get.
    #[test]
    fn add_model_pick_carries_the_rows_context_window() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelPick {
            provider_id: "nebius".to_owned(),
            api_key: None,
            models: vec![AddModelRow {
                id: "deepseek-ai/DeepSeek-V3".to_owned(),
                name: Some("DeepSeek V3".to_owned()),
                context_tokens: Some(128_000),
                cost_per_1m_input_usd: Some(0.5),
                cost_per_1m_output_usd: Some(1.5),
                live: true,
            }],
            query: String::new(),
            selected: 0,
            origin: ModelListOrigin::Live,
            refreshing: false,
        };
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.outbox,
            vec![Intent::AddModel {
                display_id: "nebius/deepseek-ai/DeepSeek-V3".to_owned(),
                provider_id: "nebius".to_owned(),
                model: "deepseek-ai/DeepSeek-V3".to_owned(),
                api_key: None,
                context_tokens: Some(128_000),
            }]
        );
    }

    /// The requires_key fallback bug: a hosted provider whose query failed with
    /// a BLANK key must still be asked for one. Writing a keyless model here is
    /// a guaranteed 401 at run time, discovered only when a run fails.
    #[test]
    fn add_model_id_with_a_blank_captured_key_still_prompts_a_key_requiring_provider() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelId {
            provider_id: "groq".to_owned(),
            requires_key: true,
            api_key: Some(SecretKey("   ".to_owned())),
            buffer: "llama-3.1-8b".to_owned(),
        };
        reduce(&mut s, Action::InputSubmit);
        assert!(
            matches!(
                &s.overlay,
                Overlay::AddModelKey { provider_id, model, .. }
                    if provider_id == "groq" && model == "llama-3.1-8b"
            ),
            "a blank captured key must route through the masked prompt, got {:?}",
            s.overlay
        );
        assert!(
            s.outbox.is_empty(),
            "nothing is added until a key decision is made"
        );
    }

    /// The same bug from the other side: when the provider card is not loaded,
    /// the failure fallback assumes a key IS needed rather than inferring it
    /// from whether this particular query carried one.
    #[test]
    fn provider_models_failed_assumes_a_key_is_needed_for_an_unknown_provider() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelQuerying {
            provider_id: "mystery".to_owned(),
            api_key: None,
        };
        reduce(
            &mut s,
            Action::ProviderModelsFailed {
                provider_id: "mystery".to_owned(),
                reason: "HTTP 401".to_owned(),
            },
        );
        assert!(
            matches!(
                &s.overlay,
                Overlay::AddModelId { requires_key, .. } if *requires_key
            ),
            "an unknown provider falls back to requiring a key, got {:?}",
            s.overlay
        );
    }

    /// A provider with no live listing but curated catalog rows still opens the
    /// pick-list (via the harness's catalog answer) instead of the free-text
    /// prompt — the Perplexity case.
    #[test]
    fn a_catalog_only_provider_queries_instead_of_falling_back_to_free_text() {
        let mut s = AppState::new();
        let mut card = provider_card(
            "perplexity",
            "Perplexity",
            "openai-chat",
            "api-key: PERPLEXITY_API_KEY",
            false,
        );
        card.can_list_models = false;
        card.available = true;
        card.catalog_models = 7;
        card.has_key = true; // a key is already stored: no re-prompt
        s.providers = vec![card];
        open_provider_picker(&mut s);
        reduce(&mut s, Action::BeginAddModel);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::QueryProviderModels {
                provider_id: "perplexity".to_owned(),
                api_key: None,
                refresh: false,
            }],
            "curated rows are requested rather than a model name typed blind"
        );
        assert!(matches!(s.overlay, Overlay::AddModelQuerying { .. }));
    }

    /// A stored provider key skips the key-first prompt entirely (adding a
    /// second model from a provider must not ask for the same key again).
    #[test]
    fn a_provider_with_a_stored_key_skips_the_key_prompt() {
        let mut s = AppState::new();
        let mut card = provider_card(
            "groq",
            "Groq",
            "openai-chat",
            "api-key: GROQ_API_KEY",
            false,
        );
        card.has_key = true;
        s.providers = vec![card];
        open_provider_picker(&mut s);
        reduce(&mut s, Action::BeginAddModel);
        assert!(
            matches!(s.overlay, Overlay::AddModelQuerying { .. }),
            "the flow goes straight to the query, got {:?}",
            s.overlay
        );
    }

    /// `Ctrl-R` on an open pick-list re-queries with the stashed key and marks
    /// the overlay refreshing; the fresher answer replaces the rows under the
    /// operator's filter without losing it.
    #[test]
    fn refresh_re_queries_and_folds_the_answer_under_the_live_filter() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelPick {
            provider_id: "groq".to_owned(),
            api_key: Some(SecretKey("sk-secret".to_owned())),
            models: rows(&["llama-3.1-8b"]),
            query: "llama".to_owned(),
            selected: 0,
            origin: ModelListOrigin::Cached("2h ago".to_owned()),
            refreshing: false,
        };
        reduce(&mut s, Action::RefreshProviderModels);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::QueryProviderModels {
                provider_id: "groq".to_owned(),
                api_key: Some(SecretKey("sk-secret".to_owned())),
                refresh: true,
            }]
        );
        assert!(
            matches!(&s.overlay, Overlay::AddModelPick { refreshing, .. } if *refreshing),
            "the overlay marks the in-flight refresh"
        );

        reduce(
            &mut s,
            Action::ProviderModelsLoaded {
                provider_id: "groq".to_owned(),
                models: rows(&["llama-3.1-8b", "llama-3.3-70b", "gpt-oss-20b"]),
                origin: ModelListOrigin::Live,
            },
        );
        match &s.overlay {
            Overlay::AddModelPick {
                models,
                query,
                origin,
                refreshing,
                ..
            } => {
                assert_eq!(models.len(), 3, "the fresher list replaced the rows");
                assert_eq!(query, "llama", "the operator's filter survives");
                assert_eq!(*origin, ModelListOrigin::Live);
                assert!(!refreshing);
            }
            other => panic!("expected the pick-list, got {other:?}"),
        }
    }

    /// A failed manual refresh leaves the rows on screen: they are still
    /// usable, so only the notice changes.
    #[test]
    fn a_failed_refresh_keeps_the_pick_list_open() {
        let mut s = AppState::new();
        s.overlay = Overlay::AddModelPick {
            provider_id: "groq".to_owned(),
            api_key: None,
            models: rows(&["llama-3.1-8b"]),
            query: String::new(),
            selected: 0,
            origin: ModelListOrigin::Live,
            refreshing: true,
        };
        reduce(
            &mut s,
            Action::ProviderModelsFailed {
                provider_id: "groq".to_owned(),
                reason: "request timed out".to_owned(),
            },
        );
        match &s.overlay {
            Overlay::AddModelPick {
                models, refreshing, ..
            } => {
                assert_eq!(models.len(), 1, "the usable rows stay");
                assert!(!refreshing);
            }
            other => panic!("expected the pick-list, got {other:?}"),
        }
    }

    /// `Ctrl-T` in `/keys` probes the focused model, and the answer replaces
    /// that card's readiness — a hosted model stops claiming "Unverified" once
    /// it has actually been checked.
    #[test]
    fn verify_api_key_probes_the_focused_model_and_folds_the_result() {
        let mut s = AppState::new();
        s.models = vec![crate::state::ModelCard {
            id: ModelId("groq/llama".to_owned()),
            provider: "openai-compatible".to_owned(),
            readiness: ModelReadiness::Unverified,
            location: None,
            cost_per_1k_usd: None,
            context_tokens: None,
        }];
        s.overlay = Overlay::ApiKeys {
            query: String::new(),
            selected: 0,
        };
        reduce(&mut s, Action::VerifyApiKey);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::VerifyApiKey {
                model_id: "groq/llama".to_owned(),
            }]
        );
        reduce(
            &mut s,
            Action::ModelKeyVerified {
                model_id: "groq/llama".to_owned(),
                ok: true,
                reason: String::new(),
            },
        );
        assert_eq!(s.models[0].readiness, ModelReadiness::Ready);

        reduce(
            &mut s,
            Action::ModelKeyVerified {
                model_id: "groq/llama".to_owned(),
                ok: false,
                reason: "provider returned HTTP 401 from /models".to_owned(),
            },
        );
        assert!(
            matches!(&s.models[0].readiness, ModelReadiness::Unavailable(reason) if reason.contains("401")),
            "a rejected key is reported honestly, got {:?}",
            s.models[0].readiness
        );
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
                files: vec!["src/lib.rs".to_owned()],
                additions: 2,
                deletions: 1,
                preview: "@@ -1 +1 @@\n-old\n+new".to_owned(),
                preview_truncated: false,
            }),
        );
        s.focus = Pane::Transcript;
        // The patch is the selected entry; expand toggles it.
        assert!(matches!(
            s.runs[0].transcript.last(),
            Some(TranscriptEntry::Patch(_))
        ));
        // transcript[0] is the User turn RunStarted pushes for the objective;
        // the patch is the next entry.
        s.runs[0].transcript_selected = 1;
        reduce(&mut s, Action::Expand);
        let TranscriptEntry::Patch(p) = &s.runs[0].transcript[1] else {
            unreachable!()
        };
        assert!(p.expanded);
    }

    #[test]
    fn select_run_sets_the_selected_run_clamped() {
        let mut s = AppState::new();
        for obj in ["a", "b", "c"] {
            reduce(
                &mut s,
                system_ev(EventBody::RunStarted {
                    run_id: RunId::new(),
                    objective: obj.to_owned(),
                    mode: AgentMode::Build,
                }),
            );
        }
        reduce(&mut s, Action::SelectRun(1));
        assert_eq!(s.selected_run, 1);
        reduce(&mut s, Action::SelectRun(99)); // clamps to last
        assert_eq!(s.selected_run, 2);
    }

    #[test]
    fn expand_is_inert_while_any_overlay_owns_enter() {
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
            system_ev(EventBody::NoteAppended {
                text: "folded detail".to_owned(),
                run_id: Some(run_id),
            }),
        );
        s.focus = Pane::Transcript;
        s.runs[0].transcript_selected = 1;
        s.overlay = Overlay::Skills;

        reduce(&mut s, Action::Expand);

        let TranscriptEntry::Note { expanded, .. } = &s.runs[0].transcript[1] else {
            unreachable!()
        };
        assert!(
            !expanded,
            "an overlay must not toggle transcript content behind it"
        );
    }

    #[test]
    fn activate_row_with_no_overlay_selects_and_toggles_the_transcript_fold() {
        // A click on a transcript row (no overlay open) is "select it + Enter":
        // it focuses the transcript, moves the selection to entry N, and toggles
        // its fold — mirroring `a_short_note_folds_the_same_way_as_a_long_one`.
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
            system_ev(EventBody::NoteAppended {
                text: "the test command is cargo test".to_owned(),
                run_id: Some(run_id),
            }),
        );
        // transcript[0] is the User turn RunStarted pushes for the objective;
        // the note folds in right after it, starting collapsed.
        s.focus = Pane::Sessions; // not on the transcript yet — the click must focus it
        reduce(&mut s, Action::ActivateRow(1));
        assert_eq!(s.focus, Pane::Transcript, "a click focuses the transcript");
        assert_eq!(
            s.runs[0].transcript_selected, 1,
            "the click selects entry N"
        );
        let TranscriptEntry::Note { expanded, .. } = &s.runs[0].transcript[1] else {
            unreachable!("NoteAppended must fold into a Note entry")
        };
        assert!(
            *expanded,
            "ActivateRow toggles the fold, exactly like Enter"
        );

        reduce(&mut s, Action::ActivateRow(1));
        let TranscriptEntry::Note { expanded, .. } = &s.runs[0].transcript[1] else {
            unreachable!()
        };
        assert!(!*expanded, "a second click toggles it back off");
    }

    #[test]
    fn activate_row_in_an_overlay_selects_and_runs_that_row() {
        // A click on overlay row N is "select it + Enter": it must move the
        // overlay's own `selected` to N (not just activate whatever was already
        // selected) and then run it — mirroring
        // `palette_submit_runs_the_highlighted_command`.
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
        reduce(&mut s, Action::OpenPalette);
        for c in "run".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        // "run" filters (in table order) to [New run, Steer run, Pause/resume
        // run, Cancel run, Model picker, Detach]; row 1 is "Steer run".
        reduce(&mut s, Action::ActivateRow(1));
        assert_eq!(
            s.overlay,
            Overlay::Steering(String::new()),
            "row 1 of the filtered list ('Steer run') ran, not row 0"
        );
    }

    #[test]
    fn finalize_leaves_streaming_tail_plain_then_snaps_on_stop() {
        use crate::state::TranscriptEntry;
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "go".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ModelStreamDelta {
                run_id,
                text: "# Title\n**bold**".to_owned(),
                thought: false,
            }),
        );
        // Still streaming ⇒ the tail Model stays plain (rendered None).
        let model = s.runs[0]
            .transcript
            .iter()
            .rev()
            .find(|e| matches!(e, TranscriptEntry::Model { .. }))
            .unwrap();
        assert!(matches!(
            model,
            TranscriptEntry::Model { rendered: None, .. }
        ));

        // Stream ends (activity leaves Streaming) ⇒ finalize parses it once.
        reduce(
            &mut s,
            system_ev(EventBody::RunStateChanged {
                run_id,
                state: RunState::Completed,
            }),
        );
        let model = s.runs[0]
            .transcript
            .iter()
            .rev()
            .find(|e| matches!(e, TranscriptEntry::Model { .. }))
            .unwrap();
        match model {
            TranscriptEntry::Model {
                rendered: Some(lines),
                ..
            } => assert!(!lines.is_empty()),
            other => panic!("expected finalized Model, got {other:?}"),
        }
    }

    #[test]
    fn finalize_is_idempotent() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "go".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ModelStreamDelta {
                run_id,
                text: "hello".to_owned(),
                thought: false,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunStateChanged {
                run_id,
                state: RunState::Completed,
            }),
        );
        crate::markdown::reset_parse_calls();
        // Further events run the sweep again; the finalized entry is not re-parsed.
        reduce(
            &mut s,
            system_ev(EventBody::RunStateChanged {
                run_id,
                state: RunState::Completed,
            }),
        );
        assert_eq!(
            crate::markdown::parse_calls(),
            0,
            "already-cached entry re-parsed"
        );
    }

    // --- Local models: browse the Unsloth GGUF catalog ---

    fn unsloth_repo(id: &str) -> UnslothRepoCard {
        UnslothRepoCard {
            id: id.to_owned(),
            downloads_label: "6.6M downloads".to_owned(),
            likes_label: "891 likes".to_owned(),
            updated_label: "updated 2026-01-30".to_owned(),
        }
    }

    fn unsloth_quant(quant: &str) -> UnslothQuantCard {
        UnslothQuantCard {
            quant: quant.to_owned(),
            size_label: "18.7 GB".to_owned(),
            file_count: 1,
            size_bytes: 18_700_000_000,
        }
    }

    #[test]
    fn palette_opens_the_unsloth_catalog_loading_and_requests_the_repo_listing() {
        let mut s = AppState::new();
        run_palette_command(&mut s, crate::palette::PaletteCommand::UnslothCatalog);
        assert_eq!(
            s.overlay,
            Overlay::UnslothRepos {
                repos: Vec::new(),
                query: String::new(),
                selected: 0,
                loading: true,
            }
        );
        assert_eq!(s.drain_outbox(), vec![Intent::ListUnslothRepos]);
    }

    #[test]
    fn unsloth_repos_loaded_fills_the_loading_overlay() {
        let mut s = AppState::new();
        s.overlay = Overlay::UnslothRepos {
            repos: Vec::new(),
            query: String::new(),
            selected: 0,
            loading: true,
        };
        let repos = vec![unsloth_repo("unsloth/Qwen3-32B-GGUF")];
        reduce(&mut s, Action::UnslothReposLoaded(repos.clone()));
        assert_eq!(
            s.overlay,
            Overlay::UnslothRepos {
                repos,
                query: String::new(),
                selected: 0,
                loading: false,
            }
        );
    }
    // --- Alt-↑/↓ + Alt-Enter: the keyboard path to tool cards and diffs ---

    /// Builds a run whose transcript is: [0] User objective, [1] Tool card,
    /// [2] Model prose, [3] Patch — two folds separated by a non-foldable
    /// entry, so a walk that stepped one *entry* at a time would land on
    /// something with nothing to open.
    fn run_with_two_folds() -> AppState {
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
            system_ev(EventBody::ToolStarted {
                run_id,
                tool: "shell.run".to_owned(),
                args_digest: "abc".to_owned(),
                label: Some("cargo test".to_owned()),
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ModelStreamDelta {
                run_id,
                text: "on it".to_owned(),
                thought: false,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::PatchProposed {
                run_id,
                changeset_id: ChangeSetId::new(),
                artifact: artifact(),
                files: vec!["src/lib.rs".to_owned()],
                additions: 2,
                deletions: 1,
                preview: "@@ -1 +1 @@\n-old\n+new".to_owned(),
                preview_truncated: false,
            }),
        );
        assert!(matches!(s.runs[0].transcript[1], TranscriptEntry::Tool(_)));
        assert!(matches!(
            s.runs[0].transcript[2],
            TranscriptEntry::Model { .. }
        ));
        assert!(matches!(s.runs[0].transcript[3], TranscriptEntry::Patch(_)));
        s
    }

    fn tool_expanded(s: &AppState) -> bool {
        let TranscriptEntry::Tool(card) = &s.runs[0].transcript[1] else {
            unreachable!()
        };
        card.expanded
    }

    fn patch_expanded(s: &AppState) -> bool {
        let TranscriptEntry::Patch(patch) = &s.runs[0].transcript[3] else {
            unreachable!()
        };
        patch.expanded
    }

    #[test]
    fn alt_arrows_walk_only_foldable_entries_and_alt_enter_expands_them() {
        let mut s = run_with_two_folds();
        assert!(!s.transcript_browse, "the base view composes by default");

        // The first Alt-↑ enters browse mode at the NEWEST fold — the one the
        // tail of the conversation is already showing.
        reduce(&mut s, Action::BrowseFoldPrev);
        assert!(s.transcript_browse);
        assert_eq!(
            s.runs[0].transcript_selected, 3,
            "entered at the newest fold"
        );

        // Alt-Enter expands it: the diff renderer was unreachable before this.
        reduce(&mut s, Action::InputNewline);
        assert!(patch_expanded(&s), "Alt-Enter expands the browsed patch");
        assert!(
            s.composer.is_empty(),
            "Alt-Enter while browsing must not type a newline into the draft"
        );
        reduce(&mut s, Action::InputNewline);
        assert!(!patch_expanded(&s), "Alt-Enter toggles it back");

        // Alt-↑ SKIPS the model prose at index 2 (nothing to open there).
        reduce(&mut s, Action::BrowseFoldPrev);
        assert_eq!(
            s.runs[0].transcript_selected, 1,
            "skipped the non-foldable model entry"
        );
        reduce(&mut s, Action::InputNewline);
        assert!(tool_expanded(&s), "Alt-Enter expands the browsed tool card");

        // Saturates at the oldest fold rather than wrapping into the objective…
        reduce(&mut s, Action::BrowseFoldPrev);
        assert_eq!(s.runs[0].transcript_selected, 1);
        // …and walks forward again.
        reduce(&mut s, Action::BrowseFoldNext);
        assert_eq!(s.runs[0].transcript_selected, 3);
        reduce(&mut s, Action::BrowseFoldNext);
        assert_eq!(
            s.runs[0].transcript_selected, 3,
            "saturates at the newest fold"
        );
    }

    /// A follow-up message: the daemon seeds a continuation run, whose
    /// `RunStarted` moves `selected_run` onto it. Everything turn 1 produced is
    /// still drawn — the conversation stacks every run — so it must stay live.
    fn add_a_second_turn(s: &mut AppState) {
        let run_id = RunId::new();
        reduce(
            s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "and now the tests".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            s,
            system_ev(EventBody::ToolStarted {
                run_id,
                tool: "workspace.read_file".to_owned(),
                args_digest: "def".to_owned(),
                label: Some("README.md".to_owned()),
            }),
        );
    }

    #[test]
    fn alt_arrows_walk_folds_across_runs_not_just_the_selected_one() {
        let mut s = run_with_two_folds();
        add_a_second_turn(&mut s);
        assert_eq!(s.selected_run, 1, "the follow-up run is selected");
        assert_eq!(s.runs.len(), 2);

        // Entry point is the newest fold in the whole conversation.
        reduce(&mut s, Action::BrowseFoldPrev);
        assert_eq!((s.fold_focus_run(), s.runs[1].transcript_selected), (1, 1));

        // Alt-↑ crosses the run boundary into turn 1's patch — before this it
        // saturated inside run 1 and every earlier card was unreachable.
        reduce(&mut s, Action::BrowseFoldPrev);
        assert_eq!(s.fold_focus_run(), 0, "the walk crossed into the older run");
        assert_eq!(s.runs[0].transcript_selected, 3);
        reduce(&mut s, Action::InputNewline);
        assert!(
            patch_expanded(&s),
            "Alt-Enter expands a diff from an earlier turn"
        );
        assert!(
            s.composer.is_empty(),
            "…instead of inserting a newline in the composer"
        );

        reduce(&mut s, Action::BrowseFoldPrev);
        assert_eq!((s.fold_focus_run(), s.runs[0].transcript_selected), (0, 1));
        reduce(&mut s, Action::InputNewline);
        assert!(tool_expanded(&s), "and expands its tool card");

        // Alt-↓ walks forward over the same boundary.
        reduce(&mut s, Action::BrowseFoldNext);
        assert_eq!((s.fold_focus_run(), s.runs[0].transcript_selected), (0, 3));
        reduce(&mut s, Action::BrowseFoldNext);
        assert_eq!(s.fold_focus_run(), 1, "and back into the newest run");
        reduce(&mut s, Action::BrowseFoldNext);
        assert_eq!(
            s.fold_focus_run(),
            1,
            "saturating at the session's newest fold"
        );

        // The composer still submits against the selected run: browsing an old
        // card must not silently re-target the next message.
        assert_eq!(s.selected_run, 1);
    }

    #[test]
    fn clicking_a_fold_in_an_earlier_run_expands_that_run_s_card() {
        let mut s = run_with_two_folds();
        add_a_second_turn(&mut s);

        reduce(&mut s, Action::ActivateFold { run: 0, entry: 1 });
        assert!(tool_expanded(&s), "the click toggled turn 1's tool card");
        assert!(
            s.transcript_browse,
            "the clicked fold becomes the browsed one"
        );
        assert_eq!((s.fold_focus_run(), s.runs[0].transcript_selected), (0, 1));
        assert_eq!(s.selected_run, 1, "clicking a card does not switch runs");

        // Alt-Y copies the card the mouse just focused, not the selected run's.
        reduce(&mut s, Action::CopyFocusedCard);
        let copied = s
            .drain_outbox()
            .into_iter()
            .find_map(|intent| match intent {
                Intent::CopyText { text } => Some(text),
                _ => None,
            })
            .expect("the focused card was copied");
        assert!(copied.contains("shell.run"), "{copied}");
    }

    #[test]
    fn an_out_of_range_fold_click_is_inert() {
        let mut s = run_with_two_folds();
        reduce(&mut s, Action::ActivateFold { run: 9, entry: 0 });
        assert!(!s.transcript_browse);
        assert!(!tool_expanded(&s));
        assert!(!patch_expanded(&s));
    }

    #[test]
    fn unsloth_repos_loaded_after_the_overlay_closed_is_ignored() {
        let mut s = AppState::new();
        s.overlay = Overlay::None;
        reduce(
            &mut s,
            Action::UnslothReposLoaded(vec![unsloth_repo("unsloth/Qwen3-32B-GGUF")]),
        );
        assert_eq!(s.overlay, Overlay::None, "a late reply must not reopen it");
    }

    #[test]
    fn unsloth_repos_failed_closes_with_a_notice() {
        let mut s = AppState::new();
        s.overlay = Overlay::UnslothRepos {
            repos: Vec::new(),
            query: String::new(),
            selected: 0,
            loading: true,
        };
        reduce(
            &mut s,
            Action::UnslothReposFailed("network error".to_owned()),
        );
        assert_eq!(s.overlay, Overlay::None);
        assert!(s.notice.is_some());
        assert!(s.notice.unwrap().0.contains("network error"));
    }

    #[test]
    fn entering_a_repo_row_begins_fetching_its_quants() {
        let mut s = AppState::new();
        s.overlay = Overlay::UnslothRepos {
            repos: vec![unsloth_repo("unsloth/Qwen3-32B-GGUF")],
            query: String::new(),
            selected: 0,
            loading: false,
        };
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.overlay,
            Overlay::UnslothQuants {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quants: Vec::new(),
                query: String::new(),
                selected: 0,
                loading: true,
            }
        );
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::ListUnslothQuants {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
            }]
        );
    }

    #[test]
    fn entering_a_repo_row_with_a_filtered_query_resolves_the_visible_selection() {
        // Regression guard for the zero-match convention: `selected` indexes
        // the FILTERED list, not the full one.
        let mut s = AppState::new();
        s.overlay = Overlay::UnslothRepos {
            repos: vec![
                unsloth_repo("unsloth/Qwen3-32B-GGUF"),
                unsloth_repo("unsloth/gpt-oss-20b-GGUF"),
            ],
            query: "gpt-oss".to_owned(),
            selected: 0,
            loading: false,
        };
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::ListUnslothQuants {
                repo_id: "unsloth/gpt-oss-20b-GGUF".to_owned(),
            }]
        );
    }

    #[test]
    fn unsloth_quants_loaded_fills_the_matching_repo() {
        let mut s = AppState::new();
        s.overlay = Overlay::UnslothQuants {
            repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
            quants: Vec::new(),
            query: String::new(),
            selected: 0,
            loading: true,
        };
        let quants = vec![unsloth_quant("Q4_K_M"), unsloth_quant("UD-Q4_K_XL")];
        reduce(
            &mut s,
            Action::UnslothQuantsLoaded {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quants: quants.clone(),
            },
        );
        assert_eq!(
            s.overlay,
            Overlay::UnslothQuants {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quants,
                query: String::new(),
                selected: 0,
                loading: false,
            }
        );
    }

    #[test]
    fn unsloth_quants_loaded_for_a_stale_repo_is_ignored() {
        // The operator navigated back and picked a DIFFERENT repo before this
        // reply for the first one landed.
        let mut s = AppState::new();
        s.overlay = Overlay::UnslothQuants {
            repo_id: "unsloth/gpt-oss-20b-GGUF".to_owned(),
            quants: Vec::new(),
            query: String::new(),
            selected: 0,
            loading: true,
        };
        reduce(
            &mut s,
            Action::UnslothQuantsLoaded {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quants: vec![unsloth_quant("Q4_K_M")],
            },
        );
        assert_eq!(
            s.overlay,
            Overlay::UnslothQuants {
                repo_id: "unsloth/gpt-oss-20b-GGUF".to_owned(),
                quants: Vec::new(),
                query: String::new(),
                selected: 0,
                loading: true,
            },
            "a stale reply for a different repo must not overwrite the current browse"
        );
    }

    #[test]
    fn unsloth_quants_failed_closes_with_a_notice_naming_the_repo() {
        let mut s = AppState::new();
        s.overlay = Overlay::UnslothQuants {
            repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
            quants: Vec::new(),
            query: String::new(),
            selected: 0,
            loading: true,
        };
        reduce(
            &mut s,
            Action::UnslothQuantsFailed {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                reason: "404".to_owned(),
            },
        );
        assert_eq!(s.overlay, Overlay::None);
        let notice = s.notice.expect("a notice is set");
        assert!(notice.0.contains("unsloth/Qwen3-32B-GGUF"));
        assert!(notice.0.contains("404"));
    }

    #[test]
    fn entering_a_quant_row_opens_the_pull_confirm() {
        let mut s = AppState::new();
        s.overlay = Overlay::UnslothQuants {
            repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
            quants: vec![unsloth_quant("UD-Q4_K_XL")],
            query: String::new(),
            selected: 0,
            loading: false,
        };
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.overlay,
            Overlay::UnslothConfirmPull {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quant: "UD-Q4_K_XL".to_owned(),
                size_label: "18.7 GB".to_owned(),
            }
        );
    }

    #[test]
    fn confirming_the_pull_starts_it_and_emits_the_pull_intent() {
        let mut s = AppState::new();
        s.overlay = Overlay::UnslothConfirmPull {
            repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
            quant: "UD-Q4_K_XL".to_owned(),
            size_label: "18.7 GB".to_owned(),
        };
        reduce(&mut s, Action::ConfirmCancel);
        assert_eq!(
            s.overlay,
            Overlay::UnslothPulling {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quant: "UD-Q4_K_XL".to_owned(),
                lines: Vec::new(),
                done: false,
                error: None,
                registered_id: None,
            }
        );
        assert_eq!(
            s.drain_outbox(),
            vec![Intent::PullUnslothModel {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quant: "UD-Q4_K_XL".to_owned(),
            }]
        );
    }

    #[test]
    fn declining_the_pull_confirm_closes_without_emitting_anything() {
        let mut s = AppState::new();
        s.overlay = Overlay::UnslothConfirmPull {
            repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
            quant: "UD-Q4_K_XL".to_owned(),
            size_label: "18.7 GB".to_owned(),
        };
        reduce(&mut s, Action::Dismiss);
        assert_eq!(s.overlay, Overlay::None);
        assert!(s.drain_outbox().is_empty());
    }

    #[test]
    fn pull_progress_lines_append_only_for_the_matching_repo_and_quant() {
        let mut s = AppState::new();
        s.overlay = Overlay::UnslothPulling {
            repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
            quant: "UD-Q4_K_XL".to_owned(),
            lines: Vec::new(),
            done: false,
            error: None,
            registered_id: None,
        };
        reduce(
            &mut s,
            Action::UnslothPullProgress {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quant: "UD-Q4_K_XL".to_owned(),
                line: "pulling manifest".to_owned(),
            },
        );
        // A line for a DIFFERENT quant of a pull the operator already moved
        // past must not land in this view.
        reduce(
            &mut s,
            Action::UnslothPullProgress {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quant: "Q4_K_M".to_owned(),
                line: "should not appear".to_owned(),
            },
        );
        match &s.overlay {
            Overlay::UnslothPulling { lines, .. } => {
                assert_eq!(lines, &vec!["pulling manifest".to_owned()]);
            }
            other => panic!("expected UnslothPulling, got {other:?}"),
        }
    }

    #[test]
    fn pull_finished_success_sets_done_and_the_registered_id() {
        let mut s = AppState::new();
        s.overlay = Overlay::UnslothPulling {
            repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
            quant: "UD-Q4_K_XL".to_owned(),
            lines: vec!["pulling manifest".to_owned()],
            done: false,
            error: None,
            registered_id: None,
        };
        reduce(
            &mut s,
            Action::UnslothPullFinished {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quant: "UD-Q4_K_XL".to_owned(),
                result: Ok("hf.co/unsloth/Qwen3-32B-GGUF:UD-Q4_K_XL".to_owned()),
            },
        );
        assert_eq!(
            s.overlay,
            Overlay::UnslothPulling {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quant: "UD-Q4_K_XL".to_owned(),
                lines: vec!["pulling manifest".to_owned()],
                done: true,
                error: None,
                registered_id: Some("hf.co/unsloth/Qwen3-32B-GGUF:UD-Q4_K_XL".to_owned()),
            }
        );
    }

    #[test]
    fn pull_finished_failure_sets_done_and_the_error() {
        let mut s = AppState::new();
        s.overlay = Overlay::UnslothPulling {
            repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
            quant: "UD-Q4_K_XL".to_owned(),
            lines: Vec::new(),
            done: false,
            error: None,
            registered_id: None,
        };
        reduce(
            &mut s,
            Action::UnslothPullFinished {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quant: "UD-Q4_K_XL".to_owned(),
                result: Err("ollama not found".to_owned()),
            },
        );
        match &s.overlay {
            Overlay::UnslothPulling {
                done,
                error,
                registered_id,
                ..
            } => {
                assert!(*done);
                assert_eq!(error.as_deref(), Some("ollama not found"));
                assert_eq!(*registered_id, None);
            }
            other => panic!("expected UnslothPulling, got {other:?}"),
        }
    }

    #[test]
    fn pull_finished_for_a_dismissed_overlay_is_dropped() {
        // Mirrors `AddModelQuerying`'s documented "a late result is ignored"
        // behavior: the operator closed the progress view, the detached pull
        // keeps running, and its terminal result has nowhere to land.
        let mut s = AppState::new();
        s.overlay = Overlay::None;
        reduce(
            &mut s,
            Action::UnslothPullFinished {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quant: "UD-Q4_K_XL".to_owned(),
                result: Ok("hf.co/unsloth/Qwen3-32B-GGUF:UD-Q4_K_XL".to_owned()),
            },
        );
        assert_eq!(s.overlay, Overlay::None);
    }

    #[test]
    fn esc_closes_every_step_of_the_unsloth_flow() {
        let overlays = vec![
            Overlay::UnslothRepos {
                repos: vec![unsloth_repo("unsloth/Qwen3-32B-GGUF")],
                query: String::new(),
                selected: 0,
                loading: false,
            },
            Overlay::UnslothQuants {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quants: vec![unsloth_quant("Q4_K_M")],
                query: String::new(),
                selected: 0,
                loading: false,
            },
            Overlay::UnslothConfirmPull {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quant: "Q4_K_M".to_owned(),
                size_label: "18.7 GB".to_owned(),
            },
            Overlay::UnslothPulling {
                repo_id: "unsloth/Qwen3-32B-GGUF".to_owned(),
                quant: "Q4_K_M".to_owned(),
                lines: Vec::new(),
                done: false,
                error: None,
                registered_id: None,
            },
        ];
        for overlay in overlays {
            let mut s = AppState::new();
            s.overlay = overlay.clone();
            reduce(&mut s, Action::Dismiss);
            assert_eq!(s.overlay, Overlay::None, "Esc must close {overlay:?}");
        }
    }

    #[test]
    fn unsloth_repo_and_quant_filter_functions_match_case_insensitive_substrings() {
        let repos = vec![
            unsloth_repo("unsloth/Qwen3-32B-GGUF"),
            unsloth_repo("unsloth/gpt-oss-20b-GGUF"),
        ];
        assert_eq!(filter_unsloth_repos(&repos, ""), vec![0, 1]);
        assert_eq!(filter_unsloth_repos(&repos, "GPT-OSS"), vec![1]);
        assert!(filter_unsloth_repos(&repos, "zzz").is_empty());

        let quants = vec![unsloth_quant("Q4_K_M"), unsloth_quant("UD-Q4_K_XL")];
        assert_eq!(filter_unsloth_quants(&quants, "ud-"), vec![1]);
    }

    #[test]
    fn alt_enter_is_still_a_line_break_when_not_browsing() {
        let mut s = run_with_two_folds();
        reduce(&mut s, Action::InputChar('h'));
        reduce(&mut s, Action::InputNewline);
        reduce(&mut s, Action::InputChar('i'));
        assert_eq!(s.composer, "h\ni");
        assert!(!tool_expanded(&s) && !patch_expanded(&s));
    }

    #[test]
    fn typing_scrolling_esc_and_run_switching_leave_browse_mode() {
        // Browse mode is a transient "the transcript has my attention" state:
        // every gesture meaning "I am driving the composer or the viewport
        // again" ends it, so Alt-Enter is a line break once more.
        let mut s = run_with_two_folds();

        reduce(&mut s, Action::BrowseFoldPrev);
        reduce(&mut s, Action::InputChar('x'));
        assert!(!s.transcript_browse, "typing returns to composing");
        assert_eq!(s.composer, "x");

        reduce(&mut s, Action::BrowseFoldPrev);
        reduce(&mut s, Action::ScrollPageUp);
        assert!(
            !s.transcript_browse,
            "scrolling drives the viewport by hand"
        );

        // Esc steps out of browse mode WITHOUT destroying the draft; a second
        // Esc then clears the draft as it always did.
        reduce(&mut s, Action::BrowseFoldPrev);
        reduce(&mut s, Action::InputCancel);
        assert!(!s.transcript_browse);
        assert_eq!(s.composer, "x", "Esc out of browse mode keeps the draft");
        reduce(&mut s, Action::InputCancel);
        assert_eq!(s.composer, "", "a second Esc clears the draft");

        // Switching runs abandons a selection that belonged to the other run.
        reduce(&mut s, Action::BrowseFoldPrev);
        reduce(&mut s, Action::PrevRun);
        assert!(!s.transcript_browse);
    }

    #[test]
    fn browsing_is_a_no_op_without_folds_or_under_an_overlay() {
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
        // Only the User objective — nothing foldable.
        reduce(&mut s, Action::BrowseFoldPrev);
        assert!(!s.transcript_browse, "no fold to browse, no browse mode");

        // An open overlay owns the arrows; browsing must not run underneath it.
        let mut s = run_with_two_folds();
        s.overlay = Overlay::Help;
        reduce(&mut s, Action::BrowseFoldNext);
        assert!(!s.transcript_browse);
    }

    #[test]
    fn chat_expand_ignores_retained_focus_but_workspace_side_focus_owns_enter() {
        // Chat is conversation-centric, so a pane value retained from a prior
        // Workspace visit cannot make folds unreachable.
        let mut s = run_with_two_folds();
        s.focus = Pane::Sessions;
        s.runs[0].transcript_selected = 1;
        reduce(&mut s, Action::Expand);
        assert!(tool_expanded(&s));

        // In Workspace that same focus is real: Enter belongs to the run list
        // and must not toggle transcript content behind it.
        let mut s = run_with_two_folds();
        s.layout = crate::state::LayoutMode::Workspace;
        s.focus = Pane::Sessions;
        s.runs[0].transcript_selected = 1;
        reduce(&mut s, Action::Expand);
        assert!(!tool_expanded(&s));

        // A browser overlay still owns Enter for its own list: no silent
        // toggling of a fold hidden behind the modal.
        let mut s = run_with_two_folds();
        s.focus = Pane::Sessions;
        s.overlay = Overlay::Skills;
        s.runs[0].transcript_selected = 1;
        reduce(&mut s, Action::Expand);
        assert!(!tool_expanded(&s));
    }

    #[test]
    fn clicking_a_fold_row_hands_the_keyboard_the_same_selection() {
        // RULE 3 both ways: a click selects + toggles and leaves the fold
        // browsable, so Alt-↑ carries on from the clicked row.
        let mut s = run_with_two_folds();
        reduce(&mut s, Action::ActivateRow(3));
        assert!(patch_expanded(&s));
        assert!(s.transcript_browse, "the clicked fold is the browsed fold");
        assert_eq!(s.runs[0].transcript_selected, 3);
        reduce(&mut s, Action::BrowseFoldPrev);
        assert_eq!(
            s.runs[0].transcript_selected, 1,
            "the keyboard continues from the click"
        );
    }

    #[test]
    fn every_foldable_entry_kind_is_reachable_by_both_click_and_keyboard() {
        // `TranscriptEntry::is_foldable` is the single predicate the renderer's
        // click targets and the Alt-↑/↓ walk share, so this list IS the parity
        // guarantee.
        let tool = TranscriptEntry::Tool(Box::new(crate::state::ToolCard {
            tool: "shell.run".to_owned(),
            status: crate::state::ToolStatus::Running,
            action: None,
            args_digest: None,
            label: None,
            outcome: None,
            artifact: None,
            approval_id: None,
            output_preview: None,
            expanded: false,
        }));
        assert!(tool.is_foldable(), "tool cards were the dead feature");
        assert!(TranscriptEntry::Patch(PatchSummary {
            changeset_id: ChangeSetId::new(),
            artifact: artifact(),
            files: vec!["a.rs".to_owned()],
            additions: 1,
            deletions: 0,
            preview: "@@".to_owned(),
            preview_truncated: false,
            expanded: false,
        })
        .is_foldable());
        assert!(TranscriptEntry::Backstage {
            context_lines: Some(3),
            memory_updates: 0,
            raw: vec!["x".to_owned()],
            expanded: false,
        }
        .is_foldable());
        assert!(TranscriptEntry::Note {
            text: "a\nb\nc".to_owned(),
            expanded: false,
        }
        .is_foldable());
        assert!(
            !TranscriptEntry::Note {
                text: "one liner".to_owned(),
                expanded: false,
            }
            .is_foldable(),
            "a short note renders inline — there is nothing to unfold"
        );
        assert!(TranscriptEntry::Completed {
            disposition: RunDisposition::Failed {
                reason: "boom".to_owned()
            },
            expanded: false,
        }
        .is_foldable());
        assert!(!TranscriptEntry::User {
            text: "hi".to_owned()
        }
        .is_foldable());
    }

    // --- composer cursor: a real text field, not an append-only buffer ---

    /// Type `text` into an empty composer one action at a time, exactly as the
    /// input layer would.
    fn typed(text: &str) -> AppState {
        let mut s = AppState::new();
        for c in text.chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        assert_eq!(s.composer_cursor, s.composer.len());
        s
    }

    #[test]
    fn arrows_home_and_end_move_the_insertion_point_and_edits_splice() {
        let mut s = typed("helo world");
        // ← ← ← ← ← ← puts the caret between "hel" and "o world".
        for _ in 0..7 {
            reduce(&mut s, Action::CursorLeft);
        }
        assert_eq!(s.composer_cursor, 3);
        reduce(&mut s, Action::InputChar('l'));
        assert_eq!(s.composer, "hello world", "typing splices at the cursor");
        assert_eq!(s.composer_cursor, 4, "the cursor follows the inserted text");

        // Backspace deletes BEFORE the cursor, not at the end of the draft.
        reduce(&mut s, Action::InputBackspace);
        assert_eq!(s.composer, "helo world");
        assert_eq!(s.composer_cursor, 3);

        reduce(&mut s, Action::CursorLineEnd);
        assert_eq!(s.composer_cursor, s.composer.len());
        reduce(&mut s, Action::CursorLineStart);
        assert_eq!(s.composer_cursor, 0);
        // Motion saturates at both ends instead of underflowing.
        reduce(&mut s, Action::CursorLeft);
        assert_eq!(s.composer_cursor, 0);
        reduce(&mut s, Action::CursorLineEnd);
        reduce(&mut s, Action::CursorRight);
        assert_eq!(s.composer_cursor, s.composer.len());
    }

    #[test]
    fn ctrl_w_deletes_a_word_and_ctrl_u_deletes_to_the_line_start() {
        let mut s = typed("cargo test --all-targets");
        reduce(&mut s, Action::DeleteWordBack);
        assert_eq!(s.composer, "cargo test ");
        assert_eq!(s.composer_cursor, s.composer.len());
        // Trailing whitespace is skipped before the word itself is eaten.
        reduce(&mut s, Action::DeleteWordBack);
        assert_eq!(s.composer, "cargo ");
        reduce(&mut s, Action::DeleteToLineStart);
        assert_eq!(s.composer, "");
        assert_eq!(s.composer_cursor, 0);
        // Both are inert on an empty draft rather than panicking.
        reduce(&mut s, Action::DeleteWordBack);
        reduce(&mut s, Action::DeleteToLineStart);
        assert_eq!(s.composer, "");
    }

    #[test]
    fn ctrl_w_and_ctrl_u_are_scoped_to_the_cursors_own_line() {
        let mut s = typed("first line");
        reduce(&mut s, Action::InputNewline);
        for c in "second line".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::DeleteToLineStart);
        assert_eq!(
            s.composer, "first line\n",
            "Ctrl-U clears the current line, never the whole draft"
        );
        reduce(&mut s, Action::DeleteWordBack);
        assert_eq!(
            s.composer, "first line\n",
            "Ctrl-W stops at the line start rather than eating the line above"
        );
    }

    #[test]
    fn up_and_down_walk_the_drafts_own_lines_before_recalling_history() {
        let mut s = AppState::new();
        s.composer_history = vec!["recalled".to_owned()];
        for c in "alpha".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputNewline);
        for c in "bravo".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        // On the second line: ↑ moves within the draft, keeping the column.
        reduce(&mut s, Action::HistoryPrev);
        assert_eq!(s.composer, "alpha\nbravo", "the draft is untouched");
        assert_eq!(s.composer_cursor, 5, "same column, line above");
        // ↓ comes back down to the same column.
        reduce(&mut s, Action::HistoryNext);
        assert_eq!(s.composer_cursor, 11);

        // At the TOP line, ↑ falls through to history recall as before.
        reduce(&mut s, Action::HistoryPrev);
        reduce(&mut s, Action::HistoryPrev);
        assert_eq!(s.composer, "recalled");
        assert_eq!(
            s.composer_cursor,
            s.composer.len(),
            "recall lands at the end"
        );
        // ...and ↓ past the newest entry restores the stashed draft.
        reduce(&mut s, Action::HistoryNext);
        assert_eq!(s.composer, "alpha\nbravo");
        assert_eq!(s.composer_cursor, s.composer.len());
    }

    #[test]
    fn a_single_line_draft_recalls_history_exactly_as_before() {
        // The vertical-motion change must not alter the shell-style contract
        // for the ordinary one-line draft.
        let mut s = AppState::new();
        s.composer_history = vec!["older".to_owned(), "newer".to_owned()];
        for c in "draft".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::HistoryPrev);
        assert_eq!(s.composer, "newer");
        reduce(&mut s, Action::HistoryPrev);
        assert_eq!(s.composer, "older");
        reduce(&mut s, Action::HistoryNext);
        assert_eq!(s.composer, "newer");
        reduce(&mut s, Action::HistoryNext);
        assert_eq!(s.composer, "draft", "the stashed draft comes back");
    }

    #[test]
    fn the_cursor_steps_whole_graphemes_over_multibyte_and_wide_text() {
        // Multi-byte (é as e + U+0301), wide CJK, and an emoji: every motion
        // and deletion must land on a grapheme boundary, never inside one —
        // slicing a `String` off-boundary would panic.
        let mut s = typed("e\u{301}日本🚀");
        assert_eq!(s.composer_cursor, s.composer.len());

        reduce(&mut s, Action::CursorLeft);
        assert_eq!(&s.composer[s.composer_cursor..], "🚀");
        reduce(&mut s, Action::CursorLeft);
        assert_eq!(&s.composer[s.composer_cursor..], "本🚀");
        reduce(&mut s, Action::CursorLeft);
        assert_eq!(&s.composer[s.composer_cursor..], "日本🚀");
        reduce(&mut s, Action::CursorLeft);
        assert_eq!(
            s.composer_cursor, 0,
            "the combining sequence is one grapheme, not two steps"
        );
        reduce(&mut s, Action::CursorRight);
        assert_eq!(s.composer_cursor, "e\u{301}".len());

        // Typing splices between graphemes and backspace removes a whole one.
        reduce(&mut s, Action::InputChar('X'));
        assert_eq!(s.composer, "e\u{301}X日本🚀");
        reduce(&mut s, Action::CursorLineEnd);
        reduce(&mut s, Action::InputBackspace);
        assert_eq!(s.composer, "e\u{301}X日本", "the emoji is deleted whole");
        reduce(&mut s, Action::InputBackspace);
        assert_eq!(s.composer, "e\u{301}X日");
        for _ in 0..3 {
            reduce(&mut s, Action::InputBackspace);
        }
        assert_eq!(
            s.composer, "",
            "the combining sequence is deleted as one grapheme"
        );
        assert_eq!(s.composer_cursor, 0);
    }

    #[test]
    fn vertical_motion_keeps_the_display_column_across_wide_glyphs() {
        // "日本語" is 6 display columns wide but 9 bytes; ↑ from the ASCII line
        // must land on the glyph boundary at (or before) the same COLUMN.
        let mut s = typed("日本語");
        reduce(&mut s, Action::InputNewline);
        for c in "abcdef".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        // Cursor after "abcd" — column 4 on the second line.
        reduce(&mut s, Action::CursorLeft);
        reduce(&mut s, Action::CursorLeft);
        reduce(&mut s, Action::HistoryPrev);
        // Column 4 on "日本語" falls inside the third glyph, so the cursor
        // snaps to the boundary before it (after 日本 = 4 columns).
        assert_eq!(&s.composer[..s.composer_cursor], "日本");
        // Coming back down restores the column, not the byte offset.
        reduce(&mut s, Action::HistoryNext);
        assert_eq!(
            &s.composer[s.composer.find('\n').unwrap() + 1..s.composer_cursor],
            "abcd"
        );
    }

    #[test]
    fn other_prompt_buffers_stay_append_only() {
        // Only the composer grew a cursor: every other prompt keeps today's
        // push/pop behaviour, and the cursor keys do nothing there.
        let mut s = AppState::new();
        s.overlay = Overlay::NewRun(String::new());
        for c in "abc".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::CursorLeft);
        reduce(&mut s, Action::CursorLineStart);
        reduce(&mut s, Action::InputChar('d'));
        reduce(&mut s, Action::InputBackspace);
        assert_eq!(s.overlay, Overlay::NewRun("abc".to_owned()));
        assert_eq!(s.composer_cursor, 0, "the composer cursor is untouched");
    }

    #[test]
    fn submitting_and_clearing_reset_the_cursor() {
        let mut s = typed("hello");
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.composer, "");
        assert_eq!(s.composer_cursor, 0);

        let mut s = typed("hello");
        reduce(&mut s, Action::InputCancel);
        assert_eq!(s.composer, "");
        assert_eq!(s.composer_cursor, 0);
    }

    #[test]
    fn a_paste_lands_at_the_cursor() {
        let mut s = typed("ab");
        reduce(&mut s, Action::CursorLeft);
        reduce(&mut s, Action::InputPaste("XY".to_owned()));
        assert_eq!(s.composer, "aXYb");
        assert_eq!(s.composer_cursor, 3);
    }

    // --- wheel granularity, blank `/keys` submit, live mode chip ---

    /// A wheel notch scrolls a few lines; `PgUp`/`PgDn` still move a page. Both
    /// share the follow-mode contract (leaving at the true bottom, re-entering
    /// when scrolled back down).
    #[test]
    fn the_wheel_scrolls_lines_while_page_keys_scroll_a_page() {
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
        // The renderer's cached bottom (a tall transcript).
        s.transcript_max_scroll.set(100);
        assert!(s.runs[0].follow);

        reduce(&mut s, Action::ScrollLinesUp);
        assert!(!s.runs[0].follow, "scrolling up leaves follow mode");
        assert_eq!(
            s.runs[0].scroll, 97,
            "a wheel notch is a few lines from the true bottom, not a page"
        );
        reduce(&mut s, Action::ScrollLinesUp);
        assert_eq!(s.runs[0].scroll, 94);

        reduce(&mut s, Action::ScrollPageUp);
        assert_eq!(s.runs[0].scroll, 84, "a page is still a page");

        // Scrolling back to the bottom re-enters follow either way.
        for _ in 0..6 {
            reduce(&mut s, Action::ScrollLinesDown);
        }
        assert_eq!(s.runs[0].scroll, 100);
        assert!(
            s.runs[0].follow,
            "reaching the bottom re-enters follow mode"
        );
    }

    #[test]
    fn council_results_wheel_scrolls_the_long_form_detail() {
        let mut state = AppState::new();
        state.overlay = Overlay::CouncilResults;

        reduce(&mut state, Action::ScrollLinesDown);
        assert_eq!(state.council_result_scroll, WHEEL_LINES);
        reduce(&mut state, Action::ScrollLinesUp);
        assert_eq!(state.council_result_scroll, 0);
    }

    /// A blank `/keys` submit must reopen the masked prompt rather than
    /// dropping the operator back to the base view — the same rule
    /// `AddModelId` has always followed.
    #[test]
    fn a_blank_key_submit_reopens_the_prompt() {
        let mut s = AppState::new();
        let target = KeyTarget::Model("groq/llama".to_owned());
        s.overlay = Overlay::ApiKeySet {
            target: target.clone(),
            buffer: SecretKey("   ".to_owned()),
        };
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.overlay,
            Overlay::ApiKeySet {
                target,
                buffer: SecretKey(String::new()),
            },
            "the prompt reopens, cleared, instead of closing"
        );
        assert!(s.outbox.is_empty(), "nothing is written for a blank key");
        assert!(s.notice.is_some(), "and the operator is told why");
    }

    // --- turn timestamps: `SessionEvent.occurred_at` survives the fold ---

    /// The event's wall-clock time used to be dropped at the fold. It now rides
    /// along to the entry it produced, one timestamp per entry, in lockstep.
    #[test]
    fn folding_an_event_records_when_it_happened() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        let started = Utc::now() - chrono::Duration::minutes(5);
        reduce(
            &mut s,
            Action::daemon_event(SessionEvent {
                sequence: 1,
                occurred_at: started,
                causation_id: None,
                correlation_id: None,
                actor: Actor::System,
                body: EventBody::RunStarted {
                    run_id,
                    objective: "o".to_owned(),
                    mode: AgentMode::Build,
                },
            }),
        );
        let replied = Utc::now();
        reduce(
            &mut s,
            Action::daemon_event(SessionEvent {
                sequence: 2,
                occurred_at: replied,
                causation_id: None,
                correlation_id: None,
                actor: Actor::System,
                body: EventBody::ModelStreamDelta {
                    run_id,
                    text: "hello".to_owned(),
                    thought: false,
                },
            }),
        );
        assert_eq!(
            s.runs[0].entry_time(0),
            Some(started),
            "the user turn's time"
        );
        assert_eq!(s.runs[0].entry_time(1), Some(replied), "the reply's time");

        // A coalesced stream keeps the time of its FIRST delta — when the turn
        // began, which is what the turn header shows.
        reduce(
            &mut s,
            Action::daemon_event(SessionEvent {
                sequence: 3,
                occurred_at: Utc::now() + chrono::Duration::seconds(30),
                causation_id: None,
                correlation_id: None,
                actor: Actor::System,
                body: EventBody::ModelStreamDelta {
                    run_id,
                    text: " world".to_owned(),
                    thought: false,
                },
            }),
        );
        assert_eq!(s.runs[0].transcript.len(), 2, "the deltas coalesced");
        assert_eq!(s.runs[0].entry_time(1), Some(replied));
    }

    /// The two vectors are written only by `push_entry`, including through the
    /// transcript cap's oldest-entry drop — this asserts they never desync.
    #[test]
    fn transcript_and_entry_times_stay_in_lockstep() {
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
        // Well past MAX_TRANSCRIPT_ENTRIES, so the cap's drop path runs.
        for i in 0..1_200 {
            reduce(
                &mut s,
                system_ev(EventBody::NoteAppended {
                    text: format!("note {i}"),
                    run_id: Some(run_id),
                }),
            );
            reduce(&mut s, system_ev(EventBody::SteeringQueued { run_id }));
        }
        let run = &s.runs[0];
        assert_eq!(
            run.transcript.len(),
            run.entry_times.len(),
            "one timestamp per entry, even after the cap dropped the oldest"
        );
        assert!(run.entry_time(run.transcript.len() - 1).is_some());
        assert_eq!(
            run.entry_time(run.transcript.len()),
            None,
            "no time past the end"
        );
    }

    /// The interactive client redraws every tick while this holds, so the
    /// answer must be true exactly when something on screen is turning.
    #[test]
    fn is_animating_tracks_every_spinner_surface() {
        let mut s = AppState::new();
        assert!(!s.is_animating(), "an idle shell needs no frames");

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
        assert_eq!(s.runs[0].activity, RunActivity::Thinking);
        assert!(s.is_animating(), "a thinking run shows the working spinner");

        reduce(
            &mut s,
            system_ev(EventBody::RunStateChanged {
                run_id,
                state: RunState::Completed,
            }),
        );
        assert!(!s.is_animating(), "a finished run stops the frames");

        // The code-graph loading modal and the model-list fetch box each spin.
        s.edge_loading = true;
        assert!(s.is_animating());
        s.edge_loading = false;
        s.overlay = Overlay::AddModelQuerying {
            provider_id: "groq".to_owned(),
            api_key: None,
        };
        assert!(s.is_animating());
        s.overlay = Overlay::None;
        assert!(!s.is_animating());
    }

    // --- /theme: pick a theme at runtime, preview it live, keep it ---

    /// Open the theme picker through the palette front door, exactly as an
    /// operator does: `/` → filter → Enter.
    fn open_theme_picker(s: &mut AppState) {
        reduce(s, Action::OpenPalette);
        for c in "theme picker".chars() {
            reduce(s, Action::InputChar(c));
        }
        reduce(s, Action::InputSubmit);
    }

    #[test]
    fn the_theme_palette_entry_opens_the_picker_on_the_current_theme() {
        let mut s = AppState::new();
        assert_eq!(
            s.themes.len(),
            7,
            "the seven built-in variants are always offered"
        );
        s.theme_selected = Some(2);
        open_theme_picker(&mut s);
        assert_eq!(
            s.overlay,
            Overlay::ThemePicker {
                query: String::new(),
                selected: 2,
            },
            "the picker opens on the theme already in force"
        );
        assert_eq!(s.input_mode(), crate::state::InputMode::Palette);
    }

    #[test]
    fn moving_the_theme_cursor_previews_and_enter_keeps_it() {
        let mut s = AppState::new();
        let boot = crate::Theme::dark();
        assert_eq!(
            s.effective_theme(&boot),
            boot,
            "with no choice, the harness's boot theme stands"
        );

        open_theme_picker(&mut s);
        reduce(&mut s, Action::SelectNext); // → light
        assert_eq!(
            s.effective_theme(&boot),
            s.themes[1].theme,
            "moving the cursor previews that theme across the whole shell"
        );
        assert_eq!(
            s.theme_selected, None,
            "previewing is not choosing — nothing is kept until Enter"
        );

        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.overlay, Overlay::None);
        assert_eq!(s.theme_selected, Some(1));
        assert_eq!(
            s.effective_theme(&boot),
            s.themes[1].theme,
            "the kept theme survives the picker closing"
        );
        assert_eq!(
            s.outbox,
            vec![Intent::SetTheme {
                id: "light".to_owned()
            }],
            "the harness is asked to remember it for the next launch"
        );

        // Esc abandons a preview: the kept theme comes back.
        open_theme_picker(&mut s);
        reduce(&mut s, Action::SelectNext);
        reduce(&mut s, Action::SelectNext);
        reduce(&mut s, Action::InputCancel);
        assert_eq!(s.theme_selected, Some(1));
        assert_eq!(s.effective_theme(&boot), s.themes[1].theme);
    }

    #[test]
    fn the_theme_picker_filters_and_guards_a_zero_match_submit() {
        let mut s = AppState::new();
        open_theme_picker(&mut s);
        for c in "mono".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        let Overlay::ThemePicker { query, selected } = &s.overlay else {
            unreachable!("the picker stays open while filtering")
        };
        assert_eq!(query, "mono");
        assert_eq!(*selected, 0, "editing the query returns to the top");
        let matches = crate::state::filter_themes(&s.themes, "mono");
        assert_eq!(matches.len(), 1);
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.themes[s.theme_selected.expect("kept")].id, "monochrome");

        // A query matching nothing keeps nothing (the zero-match guard every
        // other picker uses).
        let before = s.theme_selected;
        s.outbox.clear();
        open_theme_picker(&mut s);
        for c in "zzzz".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(s.theme_selected, before);
        assert!(s.outbox.is_empty());
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn an_installed_pack_is_offered_and_previewed_like_a_builtin() {
        // The harness parses packs and hands them over as rows; the reducer
        // treats them exactly like the built-ins.
        let mut s = AppState::new();
        let mut pack_theme = crate::Theme::light();
        pack_theme.focus.active = ratatui::style::Color::Rgb(1, 2, 3);
        s.themes.push(crate::state::ThemeChoice {
            id: "solarized".to_owned(),
            summary: "installed theme pack".to_owned(),
            theme: pack_theme,
            pack: true,
        });
        open_theme_picker(&mut s);
        for c in "solar".chars() {
            reduce(&mut s, Action::InputChar(c));
        }
        assert_eq!(
            s.effective_theme(&crate::Theme::dark()),
            pack_theme,
            "live preview"
        );
        reduce(&mut s, Action::InputSubmit);
        assert_eq!(
            s.outbox,
            vec![Intent::SetTheme {
                id: "solarized".to_owned()
            }]
        );
    }

    #[test]
    fn duplicate_integration_issues_do_not_reflash_the_notice() {
        let mut state = AppState::new();
        reduce(
            &mut state,
            Action::Issue("MCP server unavailable".to_owned()),
        );
        assert_eq!(state.issues, vec!["MCP server unavailable"]);
        state.notice = None;

        reduce(
            &mut state,
            Action::Issue("MCP server unavailable".to_owned()),
        );

        assert_eq!(state.issues, vec!["MCP server unavailable"]);
        assert!(state.notice.is_none());
    }

    /// `RunUsage` is published by this same build's daemon after every measured
    /// run. Without an arm it fell into the forward-compatibility catch-all and
    /// pushed `TranscriptEntry::Unsupported` — the product telling the user its
    /// own measurement was unsupported.
    #[test]
    fn run_usage_lands_on_the_run_and_pushes_no_unsupported_card() {
        let mut state = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut state,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "summarise".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut state,
            system_ev(EventBody::RunUsage {
                run_id,
                prompt_tokens: Some(10_000),
                completion_tokens: Some(642),
                cost_micros: Some(3_400),
            }),
        );
        let run = state.runs.first().expect("the run");
        assert_eq!(run.prompt_tokens, Some(10_000));
        assert_eq!(run.completion_tokens, Some(642));
        assert_eq!(run.cost_micros, Some(3_400));
        assert!(
            !run.transcript
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Unsupported { .. })),
            "usage is not an unsupported event: {:?}",
            run.transcript
        );
        let status = state.status();
        assert_eq!(status.prompt_tokens, Some(10_000));
        assert_eq!(status.cost_micros, Some(3_400));
    }

    /// An absent dimension means "the provider did not measure it", never zero,
    /// and a later event must not erase what an earlier one measured.
    #[test]
    fn run_usage_folds_only_the_dimensions_that_were_measured() {
        let mut state = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut state,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "summarise".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut state,
            system_ev(EventBody::RunUsage {
                run_id,
                prompt_tokens: Some(1_234),
                completion_tokens: Some(567),
                cost_micros: None,
            }),
        );
        reduce(
            &mut state,
            system_ev(EventBody::RunUsage {
                run_id,
                prompt_tokens: None,
                completion_tokens: None,
                cost_micros: Some(2_500),
            }),
        );
        let run = state.runs.first().expect("the run");
        assert_eq!(
            run.prompt_tokens,
            Some(1_234),
            "an unmeasured field is not a reset"
        );
        assert_eq!(run.completion_tokens, Some(567));
        assert_eq!(run.cost_micros, Some(2_500));
    }

    /// An event this build genuinely does not know still renders the RULE-1
    /// placeholder — the catch-all must keep working for a NEWER daemon.
    #[test]
    fn a_genuinely_unknown_event_still_reaches_the_forward_compat_placeholder() {
        let mut state = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut state,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "summarise".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(&mut state, system_ev(EventBody::Unknown));
        assert!(
            state
                .runs
                .first()
                .expect("the run")
                .transcript
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Unsupported { .. })),
            "protocol RULE 1 still holds for an event from a newer daemon"
        );
    }

    /// A markdown table is laid out into the span text, so the cache has to be
    /// rebuilt when the pane changes — otherwise a table parsed at 118 columns
    /// shears on a 70-column terminal.
    #[test]
    fn a_narrower_pane_relays_out_a_cached_markdown_table() {
        let mut state = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut state,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "table".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        state.transcript_width.set(118);
        reduce(
            &mut state,
            system_ev(EventBody::ModelStreamDelta {
                run_id,
                text: "| column-one-is-long | column-two-is-long | column-three-is-long | column-four-is-long |\n\
                       | --- | --- | --- | --- |\n\
                       | aaaaaaaaaaaaaaaaaa | bbbbbbbbbbbbbbbbbb | cccccccccccccccccccc | dddddddddddddddddd |\n"
                    .to_owned(),
                thought: false,
            }),
        );
        reduce(
            &mut state,
            system_ev(EventBody::RunStateChanged {
                run_id,
                state: RunState::Completed,
            }),
        );

        let widest = |state: &AppState| -> usize {
            state
                .runs
                .iter()
                .flat_map(|run| run.transcript.iter())
                .filter_map(|entry| match entry {
                    TranscriptEntry::Model { rendered, .. } => rendered.as_ref(),
                    _ => None,
                })
                .flat_map(|lines| lines.iter())
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| unicode_width::UnicodeWidthStr::width(span.text.as_str()))
                        .sum::<usize>()
                })
                .max()
                .unwrap_or(0)
        };
        let wide = widest(&state);
        assert!(
            wide > 70,
            "precondition: the table is wide at 118 columns ({wide})"
        );

        // The user drags the terminal narrower; the renderer publishes the new
        // pane and the next tick re-lays the cache out for it.
        state.transcript_width.set(70);
        reduce(&mut state, Action::Tick);
        let narrow = widest(&state);
        assert!(
            narrow <= 70,
            "a table must fit the pane it is drawn into, got {narrow} columns for a 70-column pane"
        );
        assert!(narrow > 0, "the cache was rebuilt, not merely dropped");
    }

    #[test]
    fn test_question_asked_and_pick_digit_resolves() {
        use codypendent_protocol::{QuestionId, QuestionOption, QuestionPrompt};

        let mut state = AppState::new();
        let q_id = QuestionId::new();
        let run_id = RunId::new();

        let prompt = QuestionPrompt {
            header: "Select target".to_string(),
            question: "Which database to migrate?".to_string(),
            options: vec![
                QuestionOption {
                    label: "PostgreSQL".to_string(),
                    description: "Primary relational store".to_string(),
                },
                QuestionOption {
                    label: "SQLite".to_string(),
                    description: "Embedded file store".to_string(),
                },
            ],
            multiple: false,
            custom: true,
        };

        reduce(
            &mut state,
            system_ev(EventBody::QuestionAsked {
                question_id: q_id,
                run_id,
                questions: vec![prompt],
            }),
        );

        assert_eq!(state.input_mode(), crate::state::InputMode::Question);
        assert!(state.show_question_modal());
        assert_eq!(state.pending_questions.len(), 1);

        // Pick option 2 ("SQLite") directly via digit shortcut
        reduce(&mut state, Action::QuestionPickDigit(2));

        assert_eq!(state.outbox.len(), 1);
        match &state.outbox[0] {
            Intent::ResolveQuestion {
                question_id,
                outcome,
            } => {
                assert_eq!(*question_id, q_id);
                assert_eq!(
                    *outcome,
                    QuestionOutcome::Answered {
                        answers: vec![vec!["SQLite".to_string()]]
                    }
                );
            }
            other => panic!("expected ResolveQuestion intent, got {other:?}"),
        }

        // When the event resolves back, pending questions clears
        reduce(
            &mut state,
            system_ev(EventBody::QuestionResolved {
                question_id: q_id,
                outcome: QuestionOutcome::Answered {
                    answers: vec![vec!["SQLite".to_string()]],
                },
            }),
        );
        assert!(state.pending_questions.is_empty());
        assert_eq!(state.input_mode(), crate::state::InputMode::Composer);
    }

    #[test]
    fn test_question_reject_with_feedback() {
        use codypendent_protocol::{QuestionId, QuestionOption, QuestionPrompt};

        let mut state = AppState::new();
        let q_id = QuestionId::new();
        let run_id = RunId::new();

        let prompt = QuestionPrompt {
            header: "Confirmation".to_string(),
            question: "Proceed with rollout?".to_string(),
            options: vec![QuestionOption {
                label: "Yes".to_string(),
                description: "Deploy immediately".to_string(),
            }],
            multiple: false,
            custom: false,
        };

        reduce(
            &mut state,
            system_ev(EventBody::QuestionAsked {
                question_id: q_id,
                run_id,
                questions: vec![prompt],
            }),
        );

        // Open reject input with 'r'
        reduce(&mut state, Action::QuestionOpenReject);
        assert!(state
            .question_card_state
            .as_ref()
            .unwrap()
            .feedback
            .is_some());

        // Type "need review"
        for c in "need review".chars() {
            reduce(&mut state, Action::QuestionInputChar(c));
        }
        assert_eq!(
            state
                .question_card_state
                .as_ref()
                .unwrap()
                .feedback
                .as_deref(),
            Some("need review")
        );

        // Enter submits rejection
        reduce(&mut state, Action::QuestionSelectOrConfirm);

        assert_eq!(state.outbox.len(), 1);
        match &state.outbox[0] {
            Intent::ResolveQuestion {
                question_id,
                outcome,
            } => {
                assert_eq!(*question_id, q_id);
                assert_eq!(
                    *outcome,
                    QuestionOutcome::Rejected {
                        feedback: Some("need review".to_string())
                    }
                );
            }
            other => panic!("expected ResolveQuestion intent, got {other:?}"),
        }
    }

    /// FIX 2: with two concurrent `user.ask` runs, rejecting one question must
    /// close *that run's* question card — identified through the resolved
    /// PendingQuestion's run_id — never a cross-run "last Running user.ask" scan
    /// that could close the other run's still-live card.
    #[test]
    fn question_rejection_closes_only_the_resolved_runs_card() {
        use codypendent_protocol::{QuestionId, QuestionOption, QuestionPrompt};

        fn ask_prompt() -> QuestionPrompt {
            QuestionPrompt {
                header: "Confirm".to_string(),
                question: "Proceed?".to_string(),
                options: vec![QuestionOption {
                    label: "Yes".to_string(),
                    description: "go".to_string(),
                }],
                multiple: false,
                custom: false,
            }
        }

        fn ask_card(state: &AppState, run_id: RunId) -> &ToolCard {
            let run = state
                .runs
                .iter()
                .find(|r| r.run_id == run_id)
                .expect("run present");
            run.transcript
                .iter()
                .rev()
                .find_map(|e| match e {
                    TranscriptEntry::Tool(card) if card.tool == "user.ask" => Some(card.as_ref()),
                    _ => None,
                })
                .expect("user.ask card present")
        }

        let mut state = AppState::new();
        let run_a = RunId::new();
        let run_b = RunId::new();
        let q_a = QuestionId::new();
        let q_b = QuestionId::new();

        for run_id in [run_a, run_b] {
            reduce(
                &mut state,
                system_ev(EventBody::RunStarted {
                    run_id,
                    objective: "ask".to_owned(),
                    mode: AgentMode::Build,
                }),
            );
            reduce(
                &mut state,
                system_ev(EventBody::ToolStarted {
                    run_id,
                    tool: "user.ask".to_owned(),
                    args_digest: "d".to_owned(),
                    label: None,
                }),
            );
        }
        reduce(
            &mut state,
            system_ev(EventBody::QuestionAsked {
                question_id: q_a,
                run_id: run_a,
                questions: vec![ask_prompt()],
            }),
        );
        reduce(
            &mut state,
            system_ev(EventBody::QuestionAsked {
                question_id: q_b,
                run_id: run_b,
                questions: vec![ask_prompt()],
            }),
        );

        assert_eq!(ask_card(&state, run_a).status, ToolStatus::Running);
        assert_eq!(ask_card(&state, run_b).status, ToolStatus::Running);

        // Reject run B's question: only run B's card closes.
        reduce(
            &mut state,
            system_ev(EventBody::QuestionResolved {
                question_id: q_b,
                outcome: QuestionOutcome::Rejected { feedback: None },
            }),
        );

        assert_eq!(
            ask_card(&state, run_a).status,
            ToolStatus::Running,
            "the other concurrent run's question card must stay live"
        );
        assert!(
            ask_card(&state, run_a).outcome.is_none(),
            "the untouched run keeps no failure outcome"
        );
        let closed = ask_card(&state, run_b);
        assert_eq!(
            closed.status,
            ToolStatus::Completed,
            "the resolved run's question card closes"
        );
        assert_eq!(
            closed.outcome,
            Some(ToolOutcome::Failed {
                message: "question rejected".to_owned(),
            }),
            "closed with the question-rejected outcome"
        );
    }

    /// FIX 3: while the reject-feedback text input is active, printable 'r'/'R'
    /// must be entered as text (the open-reject shortcut is suppressed in that
    /// mode), so feedback like "retry please" is enterable rather than wiping
    /// the buffer on every 'r'.
    #[test]
    fn reject_feedback_accepts_letters_including_r() {
        use codypendent_protocol::{QuestionId, QuestionOption, QuestionPrompt};

        // Mirror the input layer (`map_question_key`): a bare 'r'/'R' is the
        // open-reject shortcut, space toggles the option, other chars are text.
        fn key_action(c: char) -> Action {
            match c {
                'r' | 'R' => Action::QuestionOpenReject,
                ' ' => Action::QuestionToggleOption,
                other => Action::QuestionInputChar(other),
            }
        }

        let mut state = AppState::new();
        reduce(
            &mut state,
            system_ev(EventBody::QuestionAsked {
                question_id: QuestionId::new(),
                run_id: RunId::new(),
                questions: vec![QuestionPrompt {
                    header: "Confirm".to_string(),
                    question: "Proceed?".to_string(),
                    options: vec![QuestionOption {
                        label: "Yes".to_string(),
                        description: "go".to_string(),
                    }],
                    multiple: false,
                    custom: false,
                }],
            }),
        );

        // The first 'r' opens the reject-feedback box (the shortcut, empty buffer).
        reduce(&mut state, key_action('r'));
        assert!(
            state
                .question_card_state
                .as_ref()
                .unwrap()
                .feedback
                .as_deref()
                == Some(""),
            "the shortcut opens an empty feedback box"
        );
        // Now type "retry please" INTO the box — every 'r' must land as text,
        // not re-trigger the shortcut and wipe the buffer.
        for c in "retry please".chars() {
            reduce(&mut state, key_action(c));
        }

        assert_eq!(
            state
                .question_card_state
                .as_ref()
                .unwrap()
                .feedback
                .as_deref(),
            Some("retry please"),
            "every keystroke, 'r' included, must accumulate as feedback text"
        );
    }

    #[test]
    fn test_checkpoint_recorded_and_restored_events() {
        use codypendent_protocol::{CheckpointId, CheckpointKind};

        let mut state = AppState::new();
        let run_id = RunId::new();
        let cp_id = CheckpointId::new();

        reduce(
            &mut state,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "build".to_string(),
                mode: AgentMode::Build,
            }),
        );

        reduce(
            &mut state,
            system_ev(EventBody::CheckpointRecorded {
                run_id,
                checkpoint_id: cp_id,
                ordinal: 1,
                kind: CheckpointKind::Commit,
                commit: "abc1234".to_string(),
                base_commit: "abc1234".to_string(),
            }),
        );

        assert_eq!(
            state
                .runs
                .iter()
                .find(|r| r.run_id == run_id)
                .and_then(|r| r.launch_checkpoint),
            Some(cp_id)
        );

        reduce(
            &mut state,
            system_ev(EventBody::CheckpointRestored {
                run_id,
                checkpoint_id: cp_id,
                restored: true,
            }),
        );

        let last_entry = state
            .runs
            .iter()
            .find(|r| r.run_id == run_id)
            .and_then(|r| r.transcript.last());
        assert!(matches!(
            last_entry,
            Some(crate::state::TranscriptEntry::Note { text, .. }) if text == "Restored filesystem checkpoint"
        ));
    }

    #[test]
    fn test_backtrack_esc_esc_and_fork_intent() {
        use codypendent_protocol::{CheckpointId, CheckpointKind, SessionId};

        let mut state = AppState::new();
        let run_id_1 = RunId::new();
        let cp_id_1 = CheckpointId::new();
        let run_id_2 = RunId::new();
        let cp_id_2 = CheckpointId::new();

        // Add two runs with launch checkpoints
        reduce(
            &mut state,
            system_ev(EventBody::RunStarted {
                run_id: run_id_1,
                objective: "first prompt".to_string(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut state,
            system_ev(EventBody::CheckpointRecorded {
                run_id: run_id_1,
                checkpoint_id: cp_id_1,
                ordinal: 1,
                kind: CheckpointKind::Commit,
                commit: "sha1".to_string(),
                base_commit: "base1".to_string(),
            }),
        );

        reduce(
            &mut state,
            system_ev(EventBody::RunStarted {
                run_id: run_id_2,
                objective: "second prompt".to_string(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut state,
            system_ev(EventBody::CheckpointRecorded {
                run_id: run_id_2,
                checkpoint_id: cp_id_2,
                ordinal: 1,
                kind: CheckpointKind::Commit,
                commit: "sha2".to_string(),
                base_commit: "base2".to_string(),
            }),
        );

        assert_eq!(state.forkable_runs().len(), 2);
        assert!(!state.backtrack_primed);
        assert_eq!(state.overlay, Overlay::None);

        // First Esc on empty composer primes backtrack
        reduce(&mut state, Action::InputCancel);
        assert!(state.backtrack_primed);
        assert_eq!(state.overlay, Overlay::None);

        // Typing a character disarms backtrack_primed
        reduce(&mut state, Action::InputChar('a'));
        assert!(!state.backtrack_primed);
        assert_eq!(state.composer, "a");

        // Clear composer via Esc
        reduce(&mut state, Action::InputCancel);
        assert_eq!(state.composer, "");
        assert!(!state.backtrack_primed);

        // First Esc primes
        reduce(&mut state, Action::InputCancel);
        assert!(state.backtrack_primed);

        // Second Esc opens Overlay::Backtrack with newest selected (index 1)
        reduce(&mut state, Action::InputCancel);
        assert!(!state.backtrack_primed);
        assert_eq!(
            state.overlay,
            Overlay::Backtrack(BacktrackState { selected: 1 })
        );

        // Navigate up (SelectPrev) steps to index 0
        reduce(&mut state, Action::SelectPrev);
        assert_eq!(
            state.overlay,
            Overlay::Backtrack(BacktrackState { selected: 0 })
        );

        // Press Enter (InputSubmit) to fork from run 1
        reduce(&mut state, Action::InputSubmit);
        assert_eq!(state.overlay, Overlay::None);
        assert_eq!(state.composer, "first prompt");
        assert_eq!(state.outbox.len(), 1);
        assert_eq!(
            state.outbox[0],
            Intent::ForkSession {
                checkpoint: cp_id_1,
                prompt: "first prompt".to_string(),
            }
        );

        // Folding EventBody::SessionForked sets forked_from
        let from_session = SessionId::new();
        reduce(
            &mut state,
            system_ev(EventBody::SessionForked {
                from_session,
                checkpoint: cp_id_1,
            }),
        );
        assert_eq!(state.forked_from, Some(from_session));
    }

    #[test]
    fn prompt_queue_tui_lifecycle_and_interactions() {
        use codypendent_protocol::{PendingPromptView, PromptDelivery, PromptId};

        let mut state = AppState::new();
        let run_id = RunId::new();

        // 1. When a run is active, typing and submitting queues a prompt
        state.ensure_run(run_id, "active run".into(), AgentMode::Build);
        if let Some(r) = state.run_mut(run_id) {
            r.state = RunState::Running;
        }

        reduce(&mut state, Action::InputChar('h'));
        reduce(&mut state, Action::InputChar('i'));
        reduce(&mut state, Action::InputSubmit);

        assert_eq!(state.composer, "");
        assert_eq!(state.outbox.len(), 1);
        assert_eq!(
            state.outbox[0],
            Intent::QueuePrompt {
                text: "hi".into(),
                mode: AgentMode::Build,
                delivery: PromptDelivery::Queue,
            }
        );
        state.outbox.clear();

        // 2. Folding PendingPromptsChanged updates pending_prompts
        let p1 = PromptId::new();
        let p2 = PromptId::new();
        reduce(
            &mut state,
            system_ev(EventBody::PendingPromptsChanged {
                prompts: vec![
                    PendingPromptView {
                        id: p1,
                        text: "first queued".into(),
                        mode: AgentMode::Build,
                        delivery: PromptDelivery::Queue,
                    },
                    PendingPromptView {
                        id: p2,
                        text: "second queued".into(),
                        mode: AgentMode::Build,
                        delivery: PromptDelivery::Queue,
                    },
                ],
            }),
        );
        assert_eq!(state.pending_prompts.len(), 2);
        assert_eq!(state.queue_selected, None);

        // 3. Up arrow from empty composer enters queue selection at bottom item (index 1)
        reduce(&mut state, Action::HistoryPrev);
        assert_eq!(state.queue_selected, Some(1));

        // Up arrow again moves to index 0
        reduce(&mut state, Action::HistoryPrev);
        assert_eq!(state.queue_selected, Some(0));

        // Down arrow moves back to index 1
        reduce(&mut state, Action::HistoryNext);
        assert_eq!(state.queue_selected, Some(1));

        // Tab enters in-place edit mode for selected item
        reduce(&mut state, Action::CyclePane);
        assert_eq!(state.queue_editing, Some("second queued".into()));

        // Typing in edit mode appends to queue_editing
        reduce(&mut state, Action::InputChar('!'));
        assert_eq!(state.queue_editing, Some("second queued!".into()));

        // Enter saves edit via Intent::UpdateQueuedPrompt
        reduce(&mut state, Action::InputSubmit);
        assert_eq!(state.queue_editing, None);
        assert_eq!(state.outbox.len(), 1);
        assert_eq!(
            state.outbox[0],
            Intent::UpdateQueuedPrompt {
                prompt_id: p2,
                text: "second queued!".into(),
            }
        );
        state.outbox.clear();

        // Enter while selected (not editing) promotes prompt to steer
        reduce(&mut state, Action::InputSubmit);
        assert_eq!(state.outbox.len(), 1);
        assert_eq!(
            state.outbox[0],
            Intent::PromoteQueuedPrompt { prompt_id: p2 }
        );
        state.outbox.clear();

        // Delete key emits Intent::DeleteQueuedPrompt
        reduce(&mut state, Action::DeleteSelectedPrompt);
        assert_eq!(state.outbox.len(), 1);
        assert_eq!(
            state.outbox[0],
            Intent::DeleteQueuedPrompt { prompt_id: p2 }
        );
        state.outbox.clear();

        // Esc clears queue_selected and does NOT prime backtrack
        reduce(&mut state, Action::InputCancel);
        assert_eq!(state.queue_selected, None);
        assert!(!state.backtrack_primed);
    }

    #[test]
    fn p_and_shift_p_emit_pattern_and_repository_scopes() {
        let mut state = AppState::default();
        let app_id = ApprovalId::new();
        state.pending_approvals.push(PendingApproval {
            approval_id: app_id,
            action: ProposedAction::ExecuteCommand {
                program: "git".to_string(),
                args: vec!["checkout".to_string(), "main".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
            risk: Risk {
                level: RiskLevel::Medium,
                reasons: vec![],
            },
            run_id: None,
            pattern: Some("git checkout *".to_string()),
        });

        // 'p' emits Pattern scope
        reduce(&mut state, Action::Approve(ApprovalScope::Pattern));
        assert_eq!(state.outbox.len(), 1);
        assert_eq!(
            state.outbox[0],
            Intent::ResolveApproval {
                approval_id: app_id,
                decision: ApprovalDecision::Approve,
                scope: ApprovalScope::Pattern,
            }
        );
        state.outbox.clear();

        // 'P' emits Repository scope
        reduce(&mut state, Action::Approve(ApprovalScope::Repository));
        assert_eq!(state.outbox.len(), 1);
        assert_eq!(
            state.outbox[0],
            Intent::ResolveApproval {
                approval_id: app_id,
                decision: ApprovalDecision::Approve,
                scope: ApprovalScope::Repository,
            }
        );
    }

    #[test]
    fn pattern_keys_are_noops_without_a_learnable_pattern() {
        let mut state = AppState::default();
        let app_id = ApprovalId::new();
        state.pending_approvals.push(PendingApproval {
            approval_id: app_id,
            action: ProposedAction::ExecuteCommand {
                program: "python".to_string(),
                args: vec!["script.py".to_string()],
                environment: Vec::new(),
                cwd: None,
            },
            risk: Risk {
                level: RiskLevel::Medium,
                reasons: vec![],
            },
            run_id: None,
            pattern: None,
        });

        reduce(&mut state, Action::Approve(ApprovalScope::Pattern));
        assert!(state.outbox.is_empty());

        reduce(&mut state, Action::Approve(ApprovalScope::Repository));
        assert!(state.outbox.is_empty());
    }

    #[test]
    fn submit_prompt_with_bang_emits_run_user_shell() {
        let mut state = AppState::new();
        state.composer = "!cargo check".to_string();
        reduce(&mut state, Action::InputSubmit);
        assert_eq!(state.outbox.len(), 1);
        assert_eq!(
            state.outbox[0],
            Intent::RunUserShell {
                command: "cargo check".to_string(),
            }
        );
        assert!(state.composer.is_empty());
        assert_eq!(
            state.composer_history.last().map(String::as_str),
            Some("!cargo check")
        );
    }

    #[test]
    fn submit_prompt_with_bang_alone_is_noop() {
        let mut state = AppState::new();
        state.composer = "!   ".to_string();
        reduce(&mut state, Action::InputSubmit);
        assert!(state.outbox.is_empty());
        assert!(state.composer.is_empty());
    }

    #[test]
    fn submit_prompt_with_hash_single_line_emits_remember_memory() {
        let mut state = AppState::new();
        state.composer = "# remember this critical fact".to_string();
        reduce(&mut state, Action::InputSubmit);
        assert_eq!(state.outbox.len(), 1);
        assert_eq!(
            state.outbox[0],
            Intent::RememberMemory {
                text: "remember this critical fact".to_string(),
            }
        );
        assert!(state.composer.is_empty());
    }

    #[test]
    fn submit_prompt_with_hash_multiline_is_not_intercepted_as_memory() {
        let mut state = AppState::new();
        state.composer = "# Header\nSome markdown body".to_string();
        reduce(&mut state, Action::InputSubmit);
        // It starts a run rather than emitting RememberMemory
        assert_eq!(state.outbox.len(), 1);
        assert!(matches!(state.outbox[0], Intent::StartRun { .. }));
    }

    #[test]
    fn context_usage_event_updates_run_breakdown_and_percent() {
        let mut state = AppState::default();
        let run_id = RunId::new();
        reduce(
            &mut state,
            Action::daemon_event(SessionEvent {
                sequence: 1,
                occurred_at: Utc::now(),
                causation_id: None,
                correlation_id: None,
                actor: Actor::System,
                body: EventBody::RunStarted {
                    run_id,
                    objective: "test".to_owned(),
                    mode: AgentMode::Build,
                },
            }),
        );
        reduce(
            &mut state,
            Action::daemon_event(SessionEvent {
                sequence: 2,
                occurred_at: Utc::now(),
                causation_id: None,
                correlation_id: None,
                actor: Actor::System,
                body: EventBody::ContextUsage {
                    run_id,
                    used_tokens: 50_000,
                    window_tokens: 200_000,
                    system_tokens: 10_000,
                    tool_tokens: 15_000,
                    transcript_tokens: 25_000,
                },
            }),
        );
        let run = &state.runs[0];
        assert_eq!(run.context_percent, Some(25));
        let breakdown = run.context_breakdown.unwrap();
        assert_eq!(breakdown.used_tokens, 50_000);
        assert_eq!(breakdown.window_tokens, 200_000);
        assert_eq!(breakdown.system_tokens, 10_000);
        assert_eq!(breakdown.tool_tokens, 15_000);
        assert_eq!(breakdown.transcript_tokens, 25_000);
    }

    #[test]
    fn context_usage_for_an_unknown_run_leaves_the_selected_run_untouched() {
        // Regression: a usage report for a run this client never materialised
        // (attach mid-stream, trimmed catch-up window, a background run) used to
        // fall back onto the SELECTED run, so `/context` showed another run's
        // numbers as if they were this run's.
        let mut state = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut state,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "visible".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut state,
            system_ev(EventBody::ContextUsage {
                run_id,
                used_tokens: 50_000,
                window_tokens: 200_000,
                system_tokens: 10_000,
                tool_tokens: 15_000,
                transcript_tokens: 25_000,
            }),
        );
        reduce(
            &mut state,
            system_ev(EventBody::ContextUsage {
                run_id: RunId::new(),
                used_tokens: 190_000,
                window_tokens: 200_000,
                system_tokens: 90_000,
                tool_tokens: 50_000,
                transcript_tokens: 50_000,
            }),
        );
        let run = &state.runs[0];
        assert_eq!(
            run.context_percent,
            Some(25),
            "a foreign run's usage must not repaint the selected run"
        );
        assert_eq!(run.context_breakdown.unwrap().used_tokens, 50_000);
        assert_eq!(state.runs.len(), 1, "an unknown run is not materialised");
    }

    #[test]
    fn open_context_action_and_palette_command_toggle_overlay() {
        let mut state = AppState::default();
        reduce(&mut state, Action::OpenContext);
        assert_eq!(state.overlay, Overlay::Context);
        reduce(&mut state, Action::OpenContext);
        assert_eq!(state.overlay, Overlay::None);

        run_palette_command(&mut state, crate::palette::PaletteCommand::Context);
        assert_eq!(state.overlay, Overlay::Context);
    }

    fn file_match(path: &str) -> codypendent_protocol::command::FileMatchWire {
        codypendent_protocol::command::FileMatchWire {
            path: path.to_owned(),
            indices: Vec::new(),
            score: 0,
        }
    }

    #[test]
    fn history_search_typing_then_enter_loads_the_match_and_closes() {
        let mut state = AppState::new();
        state.prompt_history = vec![
            "cargo build".to_owned(),
            "cargo test".to_owned(),
            "git status".to_owned(),
        ];
        reduce(&mut state, Action::HistorySearchPrev);
        assert!(state.history_search.is_some(), "Ctrl-R opens the popup");

        for c in "cargo".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        let hs = state.history_search.as_ref().expect("popup stays open");
        assert_eq!(hs.query, "cargo");
        assert!(
            state.composer.is_empty(),
            "the query must not land in the composer draft"
        );

        // Matches walk newest-first: ["cargo test", "cargo build"]; `Up`
        // moves toward older matches.
        reduce(&mut state, Action::HistoryPrev);
        assert_eq!(state.history_search.as_ref().unwrap().selected, 1);

        reduce(&mut state, Action::InputSubmit);
        assert!(state.history_search.is_none(), "Enter closes the popup");
        assert_eq!(state.composer, "cargo build");
        assert_eq!(state.composer_cursor, state.composer.len());
    }

    #[test]
    fn history_search_esc_cancels_without_eating_the_draft() {
        let mut state = AppState::new();
        state.prompt_history = vec!["old prompt".to_owned()];
        for c in "wip draft".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        reduce(&mut state, Action::HistorySearchPrev);
        reduce(&mut state, Action::InputChar('x'));
        reduce(&mut state, Action::InputCancel);
        assert!(state.history_search.is_none(), "Esc closes the popup");
        assert_eq!(
            state.composer, "wip draft",
            "Esc must close the popup, not clear the in-progress draft"
        );
    }

    #[test]
    fn history_search_backspace_edits_the_query_and_resets_the_highlight() {
        let mut state = AppState::new();
        state.prompt_history = vec!["cargo build".to_owned(), "cargo test".to_owned()];
        reduce(&mut state, Action::HistorySearchPrev);
        for c in "cargx".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        reduce(&mut state, Action::InputBackspace);
        let hs = state.history_search.as_ref().unwrap();
        assert_eq!(hs.query, "carg");
        assert_eq!(hs.selected, 0);

        // Editing the query after navigating re-matches and resets the
        // highlight to the newest match.
        reduce(&mut state, Action::HistoryPrev);
        assert_eq!(state.history_search.as_ref().unwrap().selected, 1);
        reduce(&mut state, Action::InputChar('o'));
        let hs = state.history_search.as_ref().unwrap();
        assert_eq!(hs.query, "cargo");
        assert_eq!(hs.selected, 0);
    }

    #[test]
    fn history_search_arrows_clamp_within_the_matches() {
        let mut state = AppState::new();
        state.prompt_history = vec!["one".to_owned(), "two".to_owned()];
        reduce(&mut state, Action::HistorySearchPrev);
        // Newest-first: ["two", "one"]. Down at the top stays put; Up clamps
        // at the oldest match.
        reduce(&mut state, Action::HistoryNext);
        assert_eq!(state.history_search.as_ref().unwrap().selected, 0);
        reduce(&mut state, Action::HistoryPrev);
        reduce(&mut state, Action::HistoryPrev);
        assert_eq!(state.history_search.as_ref().unwrap().selected, 1);
        // Ctrl-R / Ctrl-S remain aliases while the popup is open.
        reduce(&mut state, Action::HistorySearchNext);
        assert_eq!(state.history_search.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn mention_popup_arrows_enter_and_esc_route_while_open() {
        let mut state = AppState::new();
        for c in "see @sr".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        assert!(state.mention_popup.is_some(), "typing @ opens the popup");
        reduce(
            &mut state,
            Action::FileSearchResults {
                query: "sr".to_owned(),
                matches: vec![file_match("src/main.rs"), file_match("src/lib.rs")],
                truncated: false,
            },
        );

        // Up/Down navigate the matches, not composer history.
        reduce(&mut state, Action::HistoryNext);
        assert_eq!(state.mention_popup.as_ref().unwrap().selected, 1);
        reduce(&mut state, Action::HistoryPrev);
        assert_eq!(state.mention_popup.as_ref().unwrap().selected, 0);

        // Enter completes the highlighted match.
        reduce(&mut state, Action::InputSubmit);
        assert!(state.mention_popup.is_none(), "Enter closes the popup");
        assert_eq!(state.composer, "see src/main.rs ");
        assert_eq!(state.composer_cursor, state.composer.len());

        // Esc closes a reopened popup and leaves the draft intact.
        for c in "@li".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        assert!(state.mention_popup.is_some());
        let draft = state.composer.clone();
        reduce(&mut state, Action::InputCancel);
        assert!(state.mention_popup.is_none(), "Esc closes the popup");
        assert_eq!(state.composer, draft, "Esc must not eat the draft");
    }

    #[test]
    fn mention_popup_typing_and_backspace_keep_filtering() {
        let mut state = AppState::new();
        for c in "@sr".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        reduce(&mut state, Action::InputChar('c'));
        let popup = state.mention_popup.as_ref().unwrap();
        assert_eq!(popup.query, "src");
        assert_eq!(popup.selected, 0);
        assert!(
            state.outbox.iter().any(|intent| matches!(
                intent,
                Intent::SearchFiles { query } if query == "src"
            )),
            "a growing @query re-issues the file search"
        );
        reduce(&mut state, Action::InputBackspace);
        assert_eq!(state.mention_popup.as_ref().unwrap().query, "sr");
    }

    #[test]
    fn mention_popup_tab_and_click_complete_the_highlighted_or_clicked_match() {
        let mut state = AppState::new();
        for c in "@sr".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        reduce(
            &mut state,
            Action::FileSearchResults {
                query: "sr".to_owned(),
                matches: vec![file_match("src/main.rs"), file_match("src/lib.rs")],
                truncated: false,
            },
        );
        // Tab is a synonym for Enter while the popup is open.
        reduce(&mut state, Action::CyclePane);
        assert!(state.mention_popup.is_none());
        assert_eq!(state.composer, "src/main.rs ");

        // A row click completes that row directly (MentionSelectAt).
        for c in "@sr".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        reduce(
            &mut state,
            Action::FileSearchResults {
                query: "sr".to_owned(),
                matches: vec![file_match("src/main.rs"), file_match("src/lib.rs")],
                truncated: false,
            },
        );
        reduce(&mut state, Action::MentionSelectAt(1));
        assert!(state.mention_popup.is_none());
        assert_eq!(state.composer, "src/main.rs src/lib.rs ");
    }

    #[test]
    fn opening_history_search_dismisses_an_open_mention_popup() {
        let mut state = AppState::new();
        state.prompt_history = vec!["old".to_owned()];
        for c in "@sr".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        assert!(state.mention_popup.is_some());
        reduce(&mut state, Action::HistorySearchPrev);
        assert!(state.history_search.is_some());
        assert!(
            state.mention_popup.is_none(),
            "the two composer popups never stack"
        );
    }

    // --- Session Library ------------------------------------------------------

    fn library_row(title: &str) -> crate::state::SessionRow {
        crate::state::SessionRow {
            session_id: codypendent_protocol::SessionId::new(),
            workspace_id: None,
            title: title.to_owned(),
            state: "active".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            internal: false,
            pinned: false,
            archived: false,
            excerpt: None,
        }
    }

    fn summary(
        session_id: codypendent_protocol::SessionId,
        title: &str,
    ) -> codypendent_protocol::SessionSummary {
        codypendent_protocol::SessionSummary {
            session_id,
            workspace_id: None,
            title: title.to_owned(),
            state: "active".to_owned(),
            updated_at: Utc::now(),
            created_at: Utc::now(),
            internal: false,
            parent_session_id: None,
            parent_run_id: None,
            pinned: false,
            archived_at: None,
            repository_id: None,
            repository: None,
            workspace: None,
            last_activity_at: None,
            last_run_id: None,
            run_state: None,
        }
    }

    /// Open the library with `rows` already folded for the empty query.
    fn open_library_with(rows: Vec<crate::state::SessionRow>) -> AppState {
        let mut state = AppState::new();
        run_palette_command(&mut state, crate::palette::PaletteCommand::SessionLibrary);
        state.outbox.clear();
        reduce(
            &mut state,
            Action::SessionSearchLoaded {
                query: String::new(),
                rows,
                next_cursor: None,
                append: false,
            },
        );
        state
    }

    #[test]
    fn opening_the_session_library_asks_the_daemon_for_the_first_page() {
        let mut state = AppState::new();
        run_palette_command(&mut state, crate::palette::PaletteCommand::SessionLibrary);

        assert!(matches!(
            state.overlay,
            Overlay::SessionLibrary {
                ref query,
                selected: 0,
                waiting: true,
            } if query.is_empty()
        ));
        // The library is SERVER-ranked: opening it must emit a search, not
        // filter a locally cached `ListSessions` projection.
        assert_eq!(state.outbox.len(), 1);
        assert!(matches!(
            &state.outbox[0],
            Intent::SearchSessions { query, cursor } if query.is_empty() && cursor.is_none()
        ));
    }

    #[test]
    fn typing_in_the_session_library_re_queries_the_daemon() {
        let mut state = AppState::new();
        run_palette_command(&mut state, crate::palette::PaletteCommand::SessionLibrary);
        state.outbox.clear();
        reduce(
            &mut state,
            Action::SessionSearchLoaded {
                query: String::new(),
                rows: vec![library_row("old page")],
                next_cursor: None,
                append: false,
            },
        );

        reduce(&mut state, Action::InputChar('m'));
        reduce(&mut state, Action::InputChar('i'));

        assert_eq!(state.session_library_query, "mi");
        // The stale page is dropped the moment the query moves on; leaving it
        // standing would show it under a heading it does not answer.
        assert!(state.session_library.is_empty());
        assert!(matches!(
            state.outbox.last(),
            Some(Intent::SearchSessions { query, cursor }) if query == "mi" && cursor.is_none()
        ));

        reduce(&mut state, Action::InputBackspace);
        assert_eq!(state.session_library_query, "m");
        assert!(matches!(
            state.outbox.last(),
            Some(Intent::SearchSessions { query, .. }) if query == "m"
        ));
    }

    #[test]
    fn a_page_for_an_abandoned_query_is_discarded() {
        let mut state = AppState::new();
        run_palette_command(&mut state, crate::palette::PaletteCommand::SessionLibrary);
        reduce(&mut state, Action::InputChar('z'));

        // A page answering the query the operator has already typed past.
        reduce(
            &mut state,
            Action::SessionSearchLoaded {
                query: String::new(),
                rows: vec![library_row("stale")],
                next_cursor: None,
                append: false,
            },
        );

        assert!(
            state.session_library.is_empty(),
            "a page for another query must never be folded under the current one"
        );
        assert!(matches!(
            state.overlay,
            Overlay::SessionLibrary { waiting: true, .. }
        ));
    }

    #[test]
    fn reaching_the_last_loaded_row_pulls_the_next_page_exactly_once() {
        let mut state = AppState::new();
        run_palette_command(&mut state, crate::palette::PaletteCommand::SessionLibrary);
        state.outbox.clear();
        reduce(
            &mut state,
            Action::SessionSearchLoaded {
                query: String::new(),
                rows: vec![library_row("a"), library_row("b"), library_row("c")],
                next_cursor: Some(codypendent_protocol::PageCursor("c1".to_owned())),
                append: false,
            },
        );

        reduce(&mut state, Action::SelectNext);
        assert!(state.outbox.is_empty(), "row 0 -> 1 is not the end yet");
        reduce(&mut state, Action::SelectNext);
        assert_eq!(state.outbox.len(), 1);
        assert!(matches!(
            &state.outbox[0],
            Intent::SearchSessions { query, cursor }
                if query.is_empty()
                    && cursor.as_ref().map(|c| c.0.as_str()) == Some("c1")
        ));

        // A second nav while the page is still in flight must not duplicate it.
        reduce(&mut state, Action::SelectNext);
        assert_eq!(state.outbox.len(), 1, "one continuation per in-flight page");

        reduce(
            &mut state,
            Action::SessionSearchLoaded {
                query: String::new(),
                rows: vec![library_row("d")],
                next_cursor: None,
                append: true,
            },
        );
        assert_eq!(state.session_library.len(), 4, "the page appended");
        assert!(state.session_library_cursor.is_none());
    }

    #[test]
    fn pin_and_archive_send_the_verb_the_row_is_not_already_in() {
        let pinned = crate::state::SessionRow {
            pinned: true,
            ..library_row("pinned")
        };
        let mut state = open_library_with(vec![library_row("plain"), pinned]);

        reduce(&mut state, Action::SessionLibraryTogglePin);
        assert!(matches!(
            state.outbox.last(),
            Some(Intent::MutateSession {
                action: codypendent_protocol::SessionLifecycleAction::Pin,
                ..
            })
        ));

        reduce(&mut state, Action::SelectNext);
        reduce(&mut state, Action::SessionLibraryTogglePin);
        assert!(matches!(
            state.outbox.last(),
            Some(Intent::MutateSession {
                action: codypendent_protocol::SessionLifecycleAction::Unpin,
                ..
            })
        ));

        reduce(&mut state, Action::SessionLibraryToggleArchive);
        assert!(matches!(
            state.outbox.last(),
            Some(Intent::MutateSession {
                action: codypendent_protocol::SessionLifecycleAction::Archive,
                ..
            })
        ));

        let archived = crate::state::SessionRow {
            archived: true,
            ..library_row("archived")
        };
        let mut state = open_library_with(vec![archived]);
        reduce(&mut state, Action::SessionLibraryToggleArchive);
        assert!(matches!(
            state.outbox.last(),
            Some(Intent::MutateSession {
                action: codypendent_protocol::SessionLifecycleAction::Restore,
                ..
            })
        ));
    }

    #[test]
    fn lifecycle_verbs_on_an_empty_library_send_nothing() {
        let mut state = open_library_with(Vec::new());
        for action in [
            Action::SessionLibraryTogglePin,
            Action::SessionLibraryToggleArchive,
            Action::SessionLibraryBeginRename,
            Action::SessionLibraryExport,
        ] {
            reduce(&mut state, action);
        }
        assert!(
            state.outbox.is_empty(),
            "there is no focused row to name, so nothing may be sent"
        );
        assert!(matches!(state.overlay, Overlay::SessionLibrary { .. }));
    }

    #[test]
    fn lifecycle_verbs_do_nothing_outside_the_library() {
        let mut state = AppState::new();
        state.session_library = vec![library_row("not visible")];
        // The Alt-chords are shared by every palette-mode overlay, so they must
        // be inert wherever the library itself is not open.
        state.overlay = Overlay::SessionPicker {
            query: String::new(),
            selected: 0,
        };
        reduce(&mut state, Action::SessionLibraryTogglePin);
        reduce(&mut state, Action::SessionLibraryExport);
        assert!(state.outbox.is_empty());
        assert!(matches!(state.overlay, Overlay::SessionPicker { .. }));
    }

    #[test]
    fn rename_round_trips_through_a_prompt_and_refuses_an_empty_title() {
        let mut state = open_library_with(vec![library_row("before")]);
        let session_id = state.session_library[0].session_id;

        reduce(&mut state, Action::SessionLibraryBeginRename);
        assert!(matches!(
            state.overlay,
            Overlay::SessionRename { buffer: ref b, .. } if b == "before"
        ));

        // Clearing the buffer and submitting must refuse rather than send a
        // rename to the empty string.
        for _ in 0.."before".len() {
            reduce(&mut state, Action::InputBackspace);
        }
        reduce(&mut state, Action::InputSubmit);
        assert!(state.outbox.is_empty());
        assert!(matches!(state.overlay, Overlay::SessionRename { .. }));

        for c in "after".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        reduce(&mut state, Action::InputSubmit);
        assert!(matches!(
            state.outbox.last(),
            Some(Intent::MutateSession {
                session_id: sent,
                action: codypendent_protocol::SessionLifecycleAction::Rename { title },
            }) if *sent == session_id && title == "after"
        ));
        assert!(matches!(state.overlay, Overlay::SessionLibrary { .. }));
    }

    #[test]
    fn cancelling_a_rename_returns_to_the_library_and_sends_nothing() {
        let mut state = open_library_with(vec![library_row("keep me")]);
        reduce(&mut state, Action::SessionLibraryBeginRename);
        reduce(&mut state, Action::InputChar('x'));
        reduce(&mut state, Action::InputCancel);
        assert!(state.outbox.is_empty());
        assert!(matches!(state.overlay, Overlay::SessionLibrary { .. }));
        assert_eq!(state.session_library[0].title, "keep me");
    }

    #[test]
    fn deleting_is_always_confirmed_and_asks_for_the_daemons_retention_policy() {
        let mut state = open_library_with(vec![library_row("doomed")]);
        let session_id = state.session_library[0].session_id;

        reduce(&mut state, Action::RemoveSelected);
        assert!(matches!(
            state.overlay,
            Overlay::ConfirmSessionDelete { .. }
        ));
        assert!(state.outbox.is_empty(), "the confirm alone sends nothing");

        // Cancelling unwinds cleanly.
        reduce(&mut state, Action::InputCancel);
        assert!(state.outbox.is_empty());
        assert!(matches!(state.overlay, Overlay::SessionLibrary { .. }));

        reduce(&mut state, Action::RemoveSelected);
        reduce(&mut state, Action::ConfirmCancel);
        assert!(matches!(
            state.outbox.last(),
            Some(Intent::MutateSession {
                session_id: sent,
                action: codypendent_protocol::SessionLifecycleAction::Delete {
                    mode: codypendent_protocol::SessionDeletionMode::RetentionPolicy,
                },
            }) if *sent == session_id
        ));
    }

    #[test]
    fn export_asks_for_the_narrowest_options() {
        let mut state = open_library_with(vec![library_row("exportable")]);
        reduce(&mut state, Action::SessionLibraryExport);
        match state.outbox.last() {
            Some(Intent::MutateSession {
                action: codypendent_protocol::SessionLifecycleAction::Export { options },
                ..
            }) => {
                // An export widens what leaves the daemon, so both switches
                // stay closed unless something explicitly opens them.
                assert!(!options.include_artifacts);
                assert!(!options.include_internal_sessions);
            }
            other => panic!("expected an Export intent, got {other:?}"),
        }
    }

    #[test]
    fn a_lifecycle_projection_replaces_the_row_but_keeps_its_excerpt() {
        let mut row = library_row("before");
        row.excerpt = Some("matched text".to_owned());
        let session_id = row.session_id;
        let mut state = open_library_with(vec![row]);
        state.session_list = vec![crate::state::SessionRow {
            session_id,
            ..library_row("before")
        }];

        let mut applied = library_row("after");
        applied.session_id = session_id;
        applied.pinned = true;
        reduce(
            &mut state,
            Action::SessionLifecycleApplied(Box::new(applied)),
        );

        assert_eq!(state.session_library[0].title, "after");
        assert!(state.session_library[0].pinned);
        assert_eq!(
            state.session_library[0].excerpt.as_deref(),
            Some("matched text"),
            "a lifecycle projection carries no excerpt; erasing the ranked \
             hit's evidence would be a measurement the daemon never made"
        );
        // The picker's own projection of the same session follows along.
        assert_eq!(state.session_list[0].title, "after");
        assert!(state.session_list[0].pinned);
    }

    #[test]
    fn a_deletion_removes_the_row_from_both_projections_and_reports_the_outcome() {
        let row = library_row("gone");
        let session_id = row.session_id;
        let mut state = open_library_with(vec![row.clone(), library_row("kept")]);
        state.session_list = vec![row];
        reduce(&mut state, Action::SelectNext);

        reduce(
            &mut state,
            Action::SessionLifecycleDeleted {
                session_id,
                tombstoned: true,
            },
        );

        assert_eq!(state.session_library.len(), 1);
        assert_eq!(state.session_library[0].title, "kept");
        assert!(state.session_list.is_empty());
        // The notice reports what the daemon DID, not what was asked for.
        let notice = state.notice.as_ref().expect("a deletion notice").0.clone();
        assert!(notice.contains("tombstoned"), "got {notice}");
        assert!(matches!(
            state.overlay,
            Overlay::SessionLibrary { selected: 0, .. }
        ));
    }

    #[test]
    fn enter_on_a_closed_library_row_refuses_to_resume_it() {
        let closed = crate::state::SessionRow {
            state: "closed".to_owned(),
            ..library_row("closed one")
        };
        let mut state = open_library_with(vec![closed]);
        reduce(&mut state, Action::InputSubmit);

        assert!(state.outbox.is_empty(), "a closed session is not resumable");
        assert!(matches!(state.overlay, Overlay::SessionLibrary { .. }));
        assert!(state
            .notice
            .as_ref()
            .is_some_and(|(text, _)| text.contains("closed")));
    }

    #[test]
    fn enter_on_a_live_library_row_switches_to_it() {
        let mut state = open_library_with(vec![library_row("resume me")]);
        let session_id = state.session_library[0].session_id;
        reduce(&mut state, Action::InputSubmit);
        assert!(matches!(
            state.outbox.last(),
            Some(Intent::SwitchSession(target)) if *target == session_id
        ));
    }

    #[test]
    fn the_session_picker_hides_internal_child_sessions() {
        let rows = vec![
            library_row("top level"),
            crate::state::SessionRow {
                internal: true,
                ..library_row("council member")
            },
        ];
        // `ListSessions` reports `internal` faithfully but does not exclude the
        // row, so the picker's own filter has to.
        let visible = filter_session_rows(&rows, "");
        assert_eq!(visible, vec![0]);
        assert!(filter_session_rows(&rows, "council").is_empty());
    }

    #[test]
    fn session_rows_project_from_the_wire_summary_without_inventing_an_excerpt() {
        let id = codypendent_protocol::SessionId::new();
        let mut wire = summary(id, "wire title");
        wire.pinned = true;
        wire.archived_at = Some(Utc::now());
        wire.internal = true;

        let row = crate::state::SessionRow::from_summary(wire.clone(), None);
        assert_eq!(row.session_id, id);
        assert!(row.pinned && row.archived && row.internal);
        assert!(row.excerpt.is_none(), "an absent excerpt stays absent");

        let hit = crate::state::SessionRow::from_summary(wire, Some("quoted".to_owned()));
        assert_eq!(hit.excerpt.as_deref(), Some("quoted"));
    }

    #[test]
    fn a_refused_search_stops_the_wait_and_says_why() {
        let mut state = AppState::new();
        run_palette_command(&mut state, crate::palette::PaletteCommand::SessionLibrary);
        state.session_library_cursor = Some(codypendent_protocol::PageCursor("stale".to_owned()));

        reduce(
            &mut state,
            Action::SessionSearchFailed {
                query: String::new(),
                reason: "the session library could not be queried \
                         (session-library.query-failed)"
                    .to_owned(),
            },
        );

        assert!(matches!(
            state.overlay,
            Overlay::SessionLibrary { waiting: false, .. }
        ));
        // A cursor whose page was refused must not be retried as if it were good.
        assert!(state.session_library_cursor.is_none());
        assert!(state
            .notice
            .as_ref()
            .is_some_and(|(text, _)| text.contains("session-library.query-failed")));
    }

    #[test]
    fn a_refusal_for_an_abandoned_query_is_discarded() {
        let mut state = AppState::new();
        run_palette_command(&mut state, crate::palette::PaletteCommand::SessionLibrary);
        reduce(&mut state, Action::InputChar('q'));

        reduce(
            &mut state,
            Action::SessionSearchFailed {
                query: String::new(),
                reason: "old failure".to_owned(),
            },
        );

        assert!(
            matches!(state.overlay, Overlay::SessionLibrary { waiting: true, .. }),
            "the CURRENT query is still in flight; an older query's refusal \
             must not end its wait"
        );
        assert!(state.notice.is_none());
    }

    /// A writer that keeps every byte a real backend would have sent to the
    /// terminal — the only place an escape sequence can actually do harm.
    #[derive(Clone, Default)]
    struct RecordingWriter(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl std::io::Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Draw `state` through the SAME backend the binary uses (crossterm over a
    /// byte sink) and return the bytes. A fixed viewport is what lets this run
    /// without a tty: `Terminal::with_options` only asks the backend for its
    /// size when the viewport is full-screen or inline.
    fn rendered_terminal_bytes(state: &AppState) -> Vec<u8> {
        let writer = RecordingWriter::default();
        let sink = std::rc::Rc::clone(&writer.0);
        let mut terminal = ratatui::Terminal::with_options(
            ratatui::prelude::CrosstermBackend::new(writer),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
            },
        )
        .expect("fixed viewport needs no terminal size");
        let theme = crate::theme::Theme::dark();
        terminal
            .draw(|frame| crate::render::render(frame, state, &theme))
            .expect("draw");
        let bytes = sink.borrow().clone();
        drop(terminal);
        bytes
    }

    /// Model output is the highest-volume untrusted text in the product, and it
    /// is drawn by a `Paragraph` — which, unlike `Buffer::set_stringn`, keeps
    /// every grapheme `unicode-width` scores non-zero, and `ESC` scores ONE.
    /// Before `append_model_text` sanitized, a raw `\x1b]52;c;<base64>\x07` in a
    /// stream delta reached a cell and `CrosstermBackend` wrote it straight out,
    /// so a prompt-injected repository could overwrite the user's clipboard or
    /// retitle the window. The assertion is on the terminal's byte stream, not
    /// on the stored string: that is where the damage would happen.
    #[test]
    fn terminal_escapes_in_producer_text_never_reach_the_terminal() {
        let mut state = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut state,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "review the diff".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        // OSC 52 (clipboard), OSC 0 (window title, BEL-terminated), OSC 2
        // (window title, ST-terminated), and a CSI screen-clear — split across
        // deltas, because the stream arrives in arbitrary chunks.
        for delta in [
            "here you go\u{1b}]52;c;cHduZWQ=\u{7}",
            "\u{1b}]0;pwned-by-osc0\u{7}\u{1b}]2;pwned-by-osc2\u{1b}\\",
            "\u{1b}[2Jhéllo 世界 🎉 done",
        ] {
            reduce(
                &mut state,
                ev(
                    agent_actor(run_id),
                    EventBody::ModelStreamDelta {
                        run_id,
                        text: delta.to_owned(),
                        thought: false,
                    },
                ),
            );
        }
        // The same class of producer text on the note path.
        reduce(
            &mut state,
            system_ev(EventBody::NoteAppended {
                text: "note\u{1b}]52;c;bm90ZS1wd24=\u{7}".to_owned(),
                run_id: Some(run_id),
            }),
        );

        let bytes = rendered_terminal_bytes(&state);
        assert!(
            !bytes.windows(2).any(|pair| pair == b"\x1b]"),
            "an OSC introducer reached the terminal: {:?}",
            String::from_utf8_lossy(&bytes)
        );
        let text = String::from_utf8_lossy(&bytes);
        for payload in ["cHduZWQ=", "pwned-by-osc0", "pwned-by-osc2", "bm90ZS1wd24="] {
            assert!(
                !text.contains(payload),
                "the sequence was only defanged, not consumed: {payload} still reached the terminal"
            );
        }

        // Legitimate content is untouched — the sanitizer must not become a
        // reason to distrust the transcript.
        let TranscriptEntry::Model { text: stored, .. } = &state.runs[0].transcript[1] else {
            panic!("expected the coalesced Model entry");
        };
        assert_eq!(
            stored, "here you gohéllo 世界 🎉 done",
            "only the control sequences are removed"
        );
        // Painted, and painted intact — otherwise this test would pass by
        // rendering nothing at all. Each wide glyph is checked on its own:
        // crossterm emits a cursor jump between a double-width cell and the
        // reserved cell after it, so the two CJK glyphs are not adjacent in the
        // byte stream even though they are adjacent on screen.
        for glyph in ["here you gohéllo", "世", "界", "🎉", "done", "note: note"] {
            assert!(
                text.contains(glyph),
                "sanitizing must not cost legitimate content: {glyph} is missing"
            );
        }
    }
}
