//! Rendering (STEP 1.12 RULE 4/5, and RULE 7 no hard-coded colors).
//!
//! Every function here is a pure projection of [`AppState`] onto a `ratatui`
//! frame. Widgets read colors exclusively from the [`Theme`] tokens — there is
//! not one literal color in this module. No function performs I/O; the render
//! thread only ever draws (RULE 2).
//!
//! Layout: left pane = session/run list; center = transcript (streamed model
//! text, tool cards, patch summaries); right = pending approvals + run details;
//! a one-row status line spans the bottom. Overlays (help, prompts, confirm,
//! and the approval modal) draw on top.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use codypendent_protocol::{
    AgentMode, BudgetDimension, ProposedAction, Risk, RiskLevel, RunDisposition, RunState,
};

use crate::reduce::capability_label;
use crate::state::{
    filter_models, AppState, DocFocus, DocLeaseState, LayoutMode, ModelCard, ModelLocationLabel,
    Overlay, PatchSummary, RunActivity, RunView, StatusProjection, ToolCard, ToolStatus,
    TranscriptEntry,
};
use crate::theme::Theme;

/// Draw the whole UI for the current frame.
pub fn render(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.surface.background)),
        area,
    );

    // A conversation-centred shell: the transcript is the workspace, a
    // persistent composer sits beneath it, and a one-row status footer spans the
    // bottom. Every other surface (runs, approvals, docs, skills, memory, edges)
    // is a centered overlay or the approval modal — minimal permanent chrome.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),                  // conversation transcript
            Constraint::Length(COMPOSER_HEIGHT), // persistent composer
            Constraint::Length(1),               // status footer
        ])
        .split(area);

    // The region above the composer depends on the layout; the composer and
    // status footer are identical in both.
    match state.layout {
        LayoutMode::Chat => render_conversation(frame, rows[0], state, theme),
        LayoutMode::Workspace => render_workspace(frame, rows[0], state, theme),
    }
    render_composer(frame, rows[1], state, theme);
    render_status_line(frame, rows[2], state, theme);

    render_overlays(frame, area, state, theme);
}

/// The workspace layout: a runs pane, the conversation, and an approvals + run
/// detail pane. The panes are at-a-glance context — interaction stays the same
/// (composer, palette, approval modal), so no pane needs its own input focus.
fn render_workspace(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(26),
            Constraint::Percentage(48),
            Constraint::Percentage(26),
        ])
        .split(area);
    render_runs_pane(frame, cols[0], state, theme);
    render_conversation(frame, cols[1], state, theme);
    render_context_pane(frame, cols[2], state, theme);
}

/// The runs pane (workspace layout): every run with its state and objective, the
/// selected one marked. Read-only — switch runs with Ctrl-↑/↓.
fn render_runs_pane(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let block = pane_block(&format!("Runs ({})", state.runs.len()), false, theme);
    let mut items: Vec<ListItem> = Vec::new();
    if state.runs.is_empty() {
        items.push(ListItem::new(Line::styled(
            "  no runs yet",
            Style::default().fg(theme.text.muted),
        )));
    }
    for (idx, run) in state.runs.iter().enumerate() {
        let selected = idx == state.selected_run;
        let marker = if selected { "› " } else { "  " };
        let line = Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.focus.active)),
            Span::styled(
                run_state_dot(run.state),
                Style::default().fg(run_state_color(run.state, theme)),
            ),
            Span::raw(" "),
            Span::styled(
                truncate(&run.objective, 18),
                Style::default().fg(theme.text.primary),
            ),
        ]);
        let item = ListItem::new(line);
        items.push(if selected {
            item.style(theme.selection_style())
        } else {
            item
        });
    }
    frame.render_widget(List::new(items).block(block), area);
}

/// The context pane (workspace layout): pending approvals over the selected run's
/// details. Read-only — approvals are resolved through the modal that pops when
/// one is pending.
fn render_context_pane(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let block = pane_block(
        &format!("Approvals ({})", state.pending_approvals.len()),
        false,
        theme,
    );
    let mut lines: Vec<Line> = Vec::new();

    if state.pending_approvals.is_empty() {
        lines.push(Line::styled(
            "  none pending",
            Style::default().fg(theme.text.muted),
        ));
    }
    for (idx, approval) in state.pending_approvals.iter().enumerate() {
        let selected = idx == state.selected_approval;
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(theme.focus.active),
            ),
            Span::styled(
                risk_label(approval.risk.level).to_owned(),
                Style::default().fg(risk_color(approval.risk.level, theme)),
            ),
            Span::raw(" "),
            Span::styled(
                action_kind(&approval.action).to_owned(),
                Style::default().fg(theme.text.primary),
            ),
        ]));
    }

    lines.push(Line::raw(""));
    lines.push(section("Run", theme));
    if let Some(run) = state.selected_run() {
        let field = |k: &str, v: String, color: Color| -> Line {
            Line::from(vec![
                Span::styled(format!("  {k}: "), Style::default().fg(theme.text.muted)),
                Span::styled(v, Style::default().fg(color)),
            ])
        };
        lines.push(field(
            "state",
            run_state_label(run.state).to_owned(),
            run_state_color(run.state, theme),
        ));
        lines.push(field(
            "mode",
            mode_label(run.mode).to_owned(),
            theme.text.secondary,
        ));
        lines.push(field(
            "model",
            run.model
                .as_ref()
                .map_or("—".to_owned(), ToString::to_string),
            theme.text.secondary,
        ));
        lines.push(field(
            "ctx",
            run.context_percent
                .map_or("—".to_owned(), |p| format!("{p}%")),
            theme.status.info,
        ));
        lines.push(field(
            "cost",
            format_cost(run.cost_minor),
            theme.status.warning,
        ));
        lines.push(field(
            "wt",
            run.worktree.clone().unwrap_or_else(|| "—".to_owned()),
            theme.text.secondary,
        ));
    } else {
        lines.push(Line::styled(
            "  no run selected",
            Style::default().fg(theme.text.muted),
        ));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The composer's height in rows (a bordered box holding one input line).
const COMPOSER_HEIGHT: u16 = 3;

/// The conversation: every run in the session, in order, as one continuous
/// scroll (Task 5, continuous-session plan) — the primary surface, full
/// width. Before this task, the pane showed only the *selected* run, so a
/// follow-up's new run made the previous turn disappear the instant it
/// started; `conversation_lines` now walks all of `state.runs`. The title
/// names the session + the newest turn (and a turn count once the session has
/// more than one), plus the header chrome (Task 4, codex chat shell) naming
/// what's serving it: `model · mode[ · cost]`.
fn render_conversation(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let session = state.session_title.as_deref().unwrap_or("Codypendent");
    let title = match state.selected_run() {
        // More than one run this session: name the turn count rather than a
        // "[selected/total] run selector" — every run always renders now, so
        // there is nothing left to select between.
        Some(run) if state.runs.len() > 1 => format!(
            "{session} — {} · {} turns{}",
            truncate(&run.objective, 36),
            state.runs.len(),
            header_chrome(run, &state.status()),
        ),
        Some(run) => format!(
            "{session} — {}{}",
            truncate(&run.objective, 44),
            header_chrome(run, &state.status()),
        ),
        None => session.to_owned(),
    };
    let block = pane_block(&title, true, theme);
    let inner = block.inner(area);

    if state.runs.is_empty() {
        let hint = Paragraph::new(vec![
            Line::styled("No runs yet.", Style::default().fg(theme.text.secondary)),
            Line::styled(
                "Type a message below and press Enter to start one.",
                Style::default().fg(theme.text.muted),
            ),
        ])
        .block(block);
        frame.render_widget(hint, area);
        return;
    }

    let lines = conversation_lines(&state.runs, theme);

    // Auto-scroll: measure the wrapped height, cache the bottom offset (so the
    // reducer's paging can leave/enter follow mode precisely), and pin the view to
    // the tail while following; otherwise honor the manual offset. The selected
    // run's follow/scroll fields govern the WHOLE scroll — `AppState::ensure_run`
    // keeps `selected_run` on the newest run by default, so "follow" still means
    // "stick to the conversation's live tail."
    let max_scroll = max_scroll_offset(&lines, inner.width, inner.height);
    state.transcript_max_scroll.set(max_scroll);
    let (follow, scroll) = state
        .selected_run()
        .map_or((true, 0), |run| (run.follow, run.scroll));
    let offset = if follow {
        max_scroll
    } else {
        scroll.min(max_scroll)
    };

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((offset, 0));
    frame.render_widget(paragraph, area);
}

/// The conversation header's `model · mode[ · cost]` chrome (Task 4, codex
/// chat shell), appended to the pane title after the session/objective so
/// the operator sees what's serving the run without opening the run-detail
/// pane. `mode` is always known once a run exists (`RunView::mode` isn't
/// optional) and is the floor; `model` (learned from the agent actor) and
/// `cost` (from the status projection's cost budget) are each left out
/// entirely — never a `—`/`$0.00` placeholder — until known.
fn header_chrome(run: &RunView, status: &StatusProjection) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(model) = &run.model {
        parts.push(model.to_string());
    }
    parts.push(mode_label(run.mode).to_owned());
    if let Some(cost) = status.cost_minor {
        parts.push(format_cost(Some(cost)));
    }
    format!(" · {}", parts.join(" · "))
}

/// The largest useful scroll offset: total wrapped rows minus the viewport
/// height (0 when everything fits). Wrapped rows are estimated as
/// `ceil(line_width / inner_width)` per line — close enough for scrolling; the
/// exact word-wrap boundary differs by at most a row.
fn max_scroll_offset(lines: &[Line], width: u16, height: u16) -> u16 {
    let inner_width = width.max(1) as usize;
    let total: usize = lines
        .iter()
        .map(|line| {
            let w = line.width();
            if w == 0 {
                1
            } else {
                w.div_ceil(inner_width)
            }
        })
        .sum();
    let total = u16::try_from(total).unwrap_or(u16::MAX);
    total.saturating_sub(height)
}

/// The persistent composer: an always-present input line. Empty, it shows a
/// context-aware placeholder (start a run vs. steer the live one); with a draft,
/// it shows the text and a cursor.
fn render_composer(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let steering = state.selected_run_is_active();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            if steering { " Steer " } else { " Message " },
            Style::default().fg(theme.text.muted),
        ))
        .border_style(Style::default().fg(theme.focus.active))
        .style(Style::default().bg(theme.surface.panel));

    let mut spans = vec![Span::styled(
        "› ",
        Style::default()
            .fg(theme.focus.active)
            .add_modifier(Modifier::BOLD),
    )];
    if state.composer.is_empty() {
        let hint = if steering {
            "steer the run · Enter sends · / for commands"
        } else {
            "message the agent to start a run · Enter sends · / for commands"
        };
        spans.push(Span::styled(hint, Style::default().fg(theme.text.muted)));
    } else {
        spans.push(Span::styled(
            state.composer.as_str(),
            Style::default().fg(theme.text.primary),
        ));
        spans.push(Span::styled("▏", Style::default().fg(theme.focus.active)));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn pane_block(title: &str, focused: bool, theme: &Theme) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(theme.text.heading)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme.border_color(focused)))
        .style(theme.panel_style())
}

/// The whole session's transcript, in run order, as one continuous scroll
/// (Task 5, continuous-session plan): every run's entries walk through
/// exactly the per-turn rendering a single run used to get alone, so a
/// follow-up's new run is appended after the prior one instead of replacing
/// it. `awaiting_header` and `seen_user_turn` (see their notes below) thread
/// continuously across a run boundary — a new run's opening `User` entry is
/// just the conversation's next turn — so the assistant header and the
/// between-turns blank line both land exactly where they would if the whole
/// session had always been one run's transcript.
fn conversation_lines<'a>(runs: &'a [RunView], theme: &Theme) -> Vec<Line<'a>> {
    let mut lines: Vec<Line> = Vec::new();
    // Assistant-turn header (codex chat shell Task 3): a `⏺ codypendent`
    // line announces the first agent cell (Model/Tool/Patch) of each turn,
    // so the transcript reads as "you asked → codypendent answered" rather
    // than an undifferentiated stream. `awaiting_header` tracks whether the
    // next agent cell is still the first one since the most recent `User`
    // entry; every other cell kind (Steering, Budget, Note, Backstage,
    // Completed, Unsupported) leaves it untouched, so a run that ends before
    // producing any agent cell never emits a lone header with nothing under
    // it.
    let mut awaiting_header = false;
    // Turn spacing (Task 4, codex chat shell): a blank line before every
    // `User` turn after the first, so consecutive turns breathe instead of
    // reading as one undifferentiated scroll. The opening turn needs no
    // leading gap.
    let mut seen_user_turn = false;
    // The last run's index: only the conversation's newest run can still be
    // live, so the streaming caret (below) never lands on an earlier,
    // necessarily-terminal run.
    let last_run_idx = runs.len().checked_sub(1);
    for (run_idx, run) in runs.iter().enumerate() {
        let is_last_run = Some(run_idx) == last_run_idx;
        let last_entry_idx = run.transcript.len().checked_sub(1);
        let before = lines.len();
        for (idx, entry) in run.transcript.iter().enumerate() {
            // Task 4: the streaming caret belongs on the newest entry of the
            // newest run only, and only while that run is actively streaming
            // into it — never mid-transcript, never on an earlier run, and
            // never once the run has moved on to Idle/Thinking/RunningTool (a
            // tool call, a thinking pause, or completion all drop it).
            let streaming_tail = is_last_run
                && last_entry_idx == Some(idx)
                && run.activity == RunActivity::Streaming;
            let is_agent_cell = matches!(
                entry,
                TranscriptEntry::Model { .. }
                    | TranscriptEntry::Tool(_)
                    | TranscriptEntry::Patch(_)
            );
            if matches!(entry, TranscriptEntry::User { .. }) {
                if seen_user_turn {
                    lines.push(Line::raw(""));
                }
                seen_user_turn = true;
                awaiting_header = true;
            } else if is_agent_cell && awaiting_header {
                lines.push(Line::styled(
                    "⏺ codypendent",
                    Style::default().fg(theme.focus.active),
                ));
                awaiting_header = false;
            }
            // `selected = false`: the conversation shows no per-entry
            // selection highlight (there is no in-transcript cursor in the
            // composer-driven shell — `render_conversation` never focuses it).
            entry_lines(entry, theme, false, streaming_tail, &mut lines);
        }
        // A run with no transcript entries yet (shouldn't happen in practice —
        // `RunStarted`'s fold pushes the objective as the very first entry —
        // but kept for the same defend-in-depth reason the single-run
        // renderer always had it) still occupies its place in the scroll.
        if lines.len() == before {
            lines.push(Line::styled(
                "(waiting for the agent…)",
                Style::default().fg(theme.text.muted),
            ));
        }
        // The live "working" status row (Task 3): appended right after this
        // run's own entries — from this run's own activity — so it reads as
        // that run's newest line. Idle (every terminal run) renders nothing.
        if let Some(status) = activity_status_line(&run.activity, theme) {
            lines.push(status);
        }
    }
    lines
}

/// The dim status row a run's derived [`RunActivity`] renders as, so a run
/// between visible transcript updates never looks silently paused.
/// `Streaming` needs no row of its own (the growing model text is itself the
/// live signal) and `Idle` renders nothing.
fn activity_status_line(activity: &RunActivity, theme: &Theme) -> Option<Line<'static>> {
    let text = match activity {
        RunActivity::Thinking => "working…".to_owned(),
        RunActivity::RunningTool(tool) => format!("running {tool}…"),
        RunActivity::Streaming | RunActivity::Idle => return None,
    };
    Some(Line::styled(text, Style::default().fg(theme.text.muted)))
}

fn entry_lines<'a>(
    entry: &'a TranscriptEntry,
    theme: &Theme,
    selected: bool,
    streaming_tail: bool,
    out: &mut Vec<Line<'a>>,
) {
    let head = |text: String, color: Color| -> Line<'a> {
        let style = if selected {
            theme.selection_style()
        } else {
            Style::default().fg(color)
        };
        Line::styled(text, style)
    };

    match entry {
        TranscriptEntry::User { text } => {
            out.push(head(format!("› {text}"), theme.focus.active));
        }
        TranscriptEntry::Model { text } => {
            model_entry_lines(text, theme, selected, streaming_tail, out);
        }
        TranscriptEntry::Tool(card) => tool_card_lines(card, theme, selected, out),
        TranscriptEntry::Patch(patch) => patch_lines(patch, theme, selected, out),
        TranscriptEntry::Steering { applied } => {
            let label = if *applied {
                "➤ steering applied"
            } else {
                "➤ steering queued"
            };
            out.push(head(label.to_owned(), theme.status.info));
        }
        TranscriptEntry::Budget {
            dimension,
            used,
            limit,
        } => {
            out.push(head(
                format!("⚠ budget {}: {used}/{limit}", budget_label(*dimension)),
                theme.status.warning,
            ));
        }
        TranscriptEntry::Completed { disposition } => match disposition {
            // Success: the streamed model prose already ended the turn —
            // render nothing here, so the reply is never echoed a second
            // (or, with the old status line plus this one, third) time.
            RunDisposition::Completed { .. } => {}
            RunDisposition::Failed { reason } => {
                out.push(head(format!("✗ {reason}"), theme.status.error));
            }
            RunDisposition::Cancelled { reason } => {
                let text = reason
                    .as_ref()
                    .map_or_else(|| "✗ cancelled".to_owned(), |r| format!("✗ cancelled: {r}"));
                out.push(head(text, theme.text.muted));
            }
            // Protocol RULE 1 (render, do not crash): `RunDisposition` is
            // `#[non_exhaustive]` — this also catches the `Unknown` variant a
            // disposition kind this build predates deserializes to.
            _ => {
                out.push(head("✗ run ended".to_owned(), theme.text.muted));
            }
        },
        TranscriptEntry::Note { text, expanded } => {
            note_lines(text, *expanded, theme, selected, out)
        }
        TranscriptEntry::Backstage {
            context_lines,
            memory_updates,
            raw,
            expanded,
        } => backstage_lines(
            *context_lines,
            *memory_updates,
            raw,
            *expanded,
            theme,
            selected,
            out,
        ),
        TranscriptEntry::Unsupported { label } => {
            out.push(head(format!("? {label}"), theme.text.muted));
        }
    }
}

/// Renders one coalesced model-text entry. While `streaming_tail` is set —
/// this is the run's newest transcript entry and the run's derived activity
/// is [`RunActivity::Streaming`] — a muted `▋` caret is appended directly
/// after the accumulated text on its last line (Task 4), so a mid-stream cell
/// visibly reads as still-writing instead of silently paused. The caret is
/// drawn fresh from `run.activity` every frame — it is never stored on the
/// entry — so it disappears the instant the run leaves `Streaming` (a tool
/// call starting, a thinking pause, or the run completing).
///
/// Folding the caret into the same `Line` that both the transcript
/// `Paragraph` and [`max_scroll_offset`]'s measurement read (see
/// `render_conversation`) means the measured bottom already accounts for it —
/// "follow latest" pins to the caret's row with no separate adjustment.
fn model_entry_lines<'a>(
    text: &'a str,
    theme: &Theme,
    selected: bool,
    streaming_tail: bool,
    out: &mut Vec<Line<'a>>,
) {
    let color = theme.agent.model_text;
    let text_style = if selected {
        theme.selection_style()
    } else {
        Style::default().fg(color)
    };
    let mut rows: Vec<&str> = text.lines().collect();
    if rows.is_empty() {
        // A `Model` entry is only ever created alongside its first delta's
        // text (`AppState::append_model_text`), so empty text here is
        // defensive rather than expected — but a caret still needs a row.
        rows.push("");
    }
    let last = rows.len() - 1;
    for (i, l) in rows.into_iter().enumerate() {
        let prefix = if i == 0 { "▌ " } else { "  " };
        if streaming_tail && i == last {
            out.push(Line::from(vec![
                Span::styled(format!("{prefix}{l}"), text_style),
                Span::styled("▋", Style::default().fg(theme.text.muted)),
            ]));
        } else {
            out.push(Line::styled(format!("{prefix}{l}"), text_style));
        }
    }
}

fn tool_card_lines<'a>(card: &'a ToolCard, theme: &Theme, selected: bool, out: &mut Vec<Line<'a>>) {
    // Task 5 (codex chat shell): the collapsed head is one compact line — a
    // run glyph, the tool's verb/name, and a terse outcome mark — instead of
    // a `[status]` bracket; `card.status`/`card.outcome` drive the mark
    // exactly as they drove the old bracket text.
    let (outcome_mark, outcome_color) = match card.status {
        ToolStatus::Proposed => ("⟳ review", theme.status.warning),
        ToolStatus::Running => ("running", theme.status.running),
        ToolStatus::Completed => match &card.outcome {
            Some(codypendent_protocol::ToolOutcome::Failed { .. }) => ("✗", theme.status.error),
            _ => ("✓", theme.status.success),
        },
    };
    let name = if card.tool.is_empty() {
        card.action.as_ref().map_or("tool", action_kind)
    } else {
        card.tool.as_str()
    };
    let marker = if card.expanded { "▾" } else { "▸" };
    let head_style = if selected {
        theme.selection_style()
    } else {
        Style::default().fg(theme.agent.tool)
    };
    out.push(Line::from(vec![
        Span::styled(format!("{marker} ⏺ {name} "), head_style),
        Span::styled(outcome_mark, Style::default().fg(outcome_color)),
    ]));

    if card.expanded {
        if let Some(action) = &card.action {
            for detail in describe_action(action) {
                out.push(Line::styled(
                    format!("    {detail}"),
                    Style::default().fg(theme.text.secondary),
                ));
            }
        }
        if let Some(digest) = &card.args_digest {
            out.push(Line::styled(
                format!("    args-digest: {digest}"),
                Style::default().fg(theme.text.muted),
            ));
        }
        if let Some(codypendent_protocol::ToolOutcome::Failed { message }) = &card.outcome {
            out.push(Line::styled(
                format!("    error: {message}"),
                Style::default().fg(theme.status.error),
            ));
        }
        if let Some(artifact) = &card.artifact {
            out.push(Line::styled(
                format!(
                    "    output: {} ({} bytes)",
                    artifact.media_type, artifact.byte_length
                ),
                Style::default().fg(theme.text.muted),
            ));
        }
    }
}

fn patch_lines<'a>(
    patch: &'a PatchSummary,
    theme: &Theme,
    selected: bool,
    out: &mut Vec<Line<'a>>,
) {
    // Task 5 (codex chat shell): the collapsed head is one compact line — a
    // patch glyph, the change set's short id standing in for a target name
    // (`PatchSummary` carries no file path or add/delete line counts yet —
    // that needs a protocol change, out of scope here), and a `⟳ review`
    // marker. The protocol has no `PatchApplied`/`PatchRejected` event, so a
    // `PatchProposed` change set never resolves on the wire: every patch
    // card sits in the transcript for manual review for its entire
    // lifetime, so the marker is unconditional rather than derived from a
    // per-instance status field.
    let marker = if patch.expanded { "▾" } else { "▸" };
    let head_style = if selected {
        theme.selection_style()
    } else {
        Style::default().fg(theme.diff.header)
    };
    out.push(Line::from(vec![
        Span::styled(
            format!("{marker} ❖ patch {} ", short_id(&patch.changeset_id)),
            head_style,
        ),
        Span::styled("⟳ review", Style::default().fg(theme.status.warning)),
    ]));
    if patch.expanded {
        out.push(Line::styled(
            format!(
                "    change set {} — {} ({} bytes)",
                patch.changeset_id, patch.artifact.media_type, patch.artifact.byte_length
            ),
            Style::default().fg(theme.diff.context),
        ));
        out.push(Line::styled(
            "    review as a change set; applies only via approval",
            Style::default().fg(theme.text.muted),
        ));
    }
}

/// Notes at or under this many lines render inline, unchanged; a longer note
/// folds (mirrors [`ToolCard`]/[`PatchSummary`] — the Chapter 07
/// transcript-declutter fix). Applies to ANY note generically — nothing here
/// special-cases the run-context manifest or a curated-memory note.
const NOTE_INLINE_LINE_THRESHOLD: usize = 2;

fn note_lines<'a>(
    text: &'a str,
    expanded: bool,
    theme: &Theme,
    selected: bool,
    out: &mut Vec<Line<'a>>,
) {
    let head_style = if selected {
        theme.selection_style()
    } else {
        Style::default().fg(theme.text.secondary)
    };
    let line_count = text.lines().count();
    if line_count <= NOTE_INLINE_LINE_THRESHOLD {
        out.push(Line::styled(format!("• note: {text}"), head_style));
        return;
    }
    let marker = if expanded { "▾" } else { "▸" };
    out.push(Line::styled(
        format!(
            "{marker} note: {} ({line_count} lines)",
            first_non_empty_line(text)
        ),
        head_style,
    ));
    if expanded {
        for line in text.lines() {
            out.push(Line::styled(
                format!("    {line}"),
                Style::default().fg(theme.text.secondary),
            ));
        }
    }
}

/// Renders the folded backstage line (Task 2): the context manifest and
/// curated-memory writes for the run, summarized in one dim, expandable line
/// instead of the visible `Note` cells they'd otherwise be. Each half
/// (`context …`, `memory …`) is omitted when its count is empty (`None`/`0`);
/// if both are empty (defensive — the reducer never creates the entry
/// without at least one), nothing renders. `⋯` marks the folded line; once
/// expanded, the full text of every folded note follows, dim and indented,
/// same as an expanded [`note_lines`] body.
fn backstage_lines<'a>(
    context_lines: Option<usize>,
    memory_updates: usize,
    raw: &'a [String],
    expanded: bool,
    theme: &Theme,
    selected: bool,
    out: &mut Vec<Line<'a>>,
) {
    let mut parts = Vec::new();
    if let Some(n) = context_lines {
        let noun = if n == 1 { "line" } else { "lines" };
        parts.push(format!("context · {n} {noun}"));
    }
    if memory_updates > 0 {
        if memory_updates == 1 {
            parts.push("memory updated".to_owned());
        } else {
            parts.push(format!("memory updated ×{memory_updates}"));
        }
    }
    if parts.is_empty() {
        return;
    }
    let head_style = if selected {
        theme.selection_style()
    } else {
        Style::default().fg(theme.text.muted)
    };
    let marker = if expanded { "▾" } else { "⋯" };
    out.push(Line::styled(
        format!("{marker} {}", parts.join(" · ")),
        head_style,
    ));
    if expanded {
        for note in raw {
            for line in note.lines() {
                out.push(Line::styled(
                    format!("    {line}"),
                    Style::default().fg(theme.text.muted),
                ));
            }
        }
    }
}

fn render_status_line(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let bg = Style::default().bg(theme.surface.overlay);

    // A transient notice (rejected command, presence change) takes the line:
    // it is the only channel for "the daemon said no", so it must not compete
    // with the ambient fields for attention.
    if let Some((notice, _)) = &state.notice {
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(notice.clone(), Style::default().fg(theme.status.warning)),
        ]);
        frame.render_widget(
            Paragraph::new(line).style(Style::default().bg(theme.surface.panel)),
            area,
        );
        return;
    }

    let status = state.status();
    let width = area.width;
    // Two tiers: full fields on a wide terminal, then progressively fewer as the
    // width shrinks, so mode/state/attention always survive.
    let full = width >= 96;
    let mid = width >= 64;

    let field = |label: &str, value: String, color: Color| -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("{label} "), Style::default().fg(theme.text.muted)),
            Span::styled(value, Style::default().fg(color)),
        ]
    };
    let sep = || Span::styled("  ", Style::default().fg(theme.text.muted));

    // --- ambient state (left) ---
    let mut ambient: Vec<Vec<Span>> = Vec::new();
    if mid {
        ambient.push(field(
            "mode",
            status
                .mode
                .map_or("—".to_owned(), |m| mode_label(m).to_owned()),
            theme.status.info,
        ));
    }
    ambient.push(field(
        "state",
        status
            .run_state
            .map_or("—".to_owned(), |s| run_state_label(s).to_owned()),
        status
            .run_state
            .map_or(theme.text.muted, |s| run_state_color(s, theme)),
    ));
    if full {
        ambient.push(field(
            "model",
            status
                .model
                .as_ref()
                .map_or("—".to_owned(), ToString::to_string),
            theme.text.secondary,
        ));
    }
    if mid {
        ambient.push(field(
            "ctx",
            status
                .context_percent
                .map_or("—".to_owned(), |p| format!("{p}%")),
            theme.status.info,
        ));
    }
    if full {
        ambient.push(field(
            "cost",
            format_cost(status.cost_minor),
            theme.status.warning,
        ));
        ambient.push(field(
            "wt",
            status.worktree.clone().unwrap_or_else(|| "—".to_owned()),
            theme.text.secondary,
        ));
    }
    ambient.push(field(
        "approvals",
        status.pending_approvals.to_string(),
        if status.pending_approvals > 0 {
            theme.status.warning
        } else {
            theme.text.muted
        },
    ));

    let mut left: Vec<Span> = vec![Span::raw(" ")];
    for (i, group) in ambient.into_iter().enumerate() {
        if i > 0 {
            left.push(sep());
        }
        left.extend(group);
    }

    // --- instructional hint (right), by what the user should do next ---
    let key = |k: &str| Span::styled(k.to_owned(), Style::default().fg(theme.focus.active));
    let word = |w: &str| Span::styled(w.to_owned(), Style::default().fg(theme.text.muted));
    let scrolled_up = state.selected_run().is_some_and(|r| !r.follow);
    let hint: Vec<Span> = if status.pending_approvals > 0 {
        vec![
            key("a"),
            word(" approve  "),
            key("A"),
            word(" run  "),
            key("r"),
            word(" reject"),
        ]
    } else if scrolled_up {
        vec![key("PgDn"), word(" ↧ latest")]
    } else if !state.composer.is_empty() {
        vec![key("⏎"), word(" send  "), key("Esc"), word(" clear")]
    } else {
        vec![key("/"), word(" cmds  "), key("F2"), word(" layout")]
    };
    // Right-align the hint by padding between it and the ambient fields. This
    // renders every frame, so measure widths from the spans directly rather than
    // cloning `left` and wrapping `hint` in a `Line` just to call `.width()`.
    let left_width: usize = left.iter().map(|span| span.width()).sum();
    let hint_width: usize = hint.iter().map(|span| span.width()).sum();
    let pad = (width as usize).saturating_sub(left_width + hint_width + 1);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(hint);
    spans.push(Span::raw(" "));

    frame.render_widget(Paragraph::new(Line::from(spans)).style(bg), area);
}

fn render_overlays(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    match &state.overlay {
        Overlay::Help => render_help(frame, area, theme),
        Overlay::NewRun(buffer) => {
            render_prompt(frame, area, theme, "New run objective", buffer);
        }
        Overlay::Steering(buffer) => {
            render_prompt(
                frame,
                area,
                theme,
                "Steer the run (queued for a safe point)",
                buffer,
            );
        }
        Overlay::ConfirmCancel => render_confirm(frame, area, theme),
        Overlay::Skills => render_skills(frame, area, state, theme),
        Overlay::Memory { source_open } => {
            render_memory(frame, area, state, theme, *source_open);
        }
        Overlay::Docs => render_docs(frame, area, state, theme),
        Overlay::Edges => render_edges(frame, area, state, theme),
        Overlay::Workflow => render_workflow(frame, area, state, theme),
        Overlay::Blackboard => render_blackboard(frame, area, state, theme),
        Overlay::Palette { query, selected } => {
            render_palette(frame, area, theme, query, *selected);
        }
        Overlay::ModelPicker { query, selected } => {
            render_model_picker(frame, area, state, theme, query, *selected);
        }
        // The block-edit prompt floats over the Docs browser it opened from, so the
        // editor stays in view while the writer types the insertion.
        Overlay::DocEdit { buffer, .. } => {
            render_docs(frame, area, state, theme);
            render_prompt(
                frame,
                area,
                theme,
                "Insert text into the focused block",
                buffer,
            );
        }
        Overlay::None => {
            if state.show_approval_modal() {
                render_approval_modal(frame, area, state, theme);
            }
        }
    }
}

/// The Skill Studio browser (STEP 2.6): a scrollable list of registered items on
/// the left, and a detail panel on the right that renders the selected skill's
/// metadata, description, risk, and — the exit-criterion payload — its requested
/// **permissions verbatim**. Colors are Theme tokens only (RULE 7).
fn render_skills(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let rect = centered_rect(84, 84, area);
    frame.render_widget(Clear, rect);

    let outer = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" Skill Studio ({}) ", state.skills.len()),
            Style::default()
                .fg(theme.text.heading)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme.focus.active))
        .style(
            Style::default()
                .bg(theme.surface.overlay)
                .fg(theme.text.primary),
        );
    let inner = outer.inner(rect);
    frame.render_widget(outer, rect);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(inner);

    // Left: the item list (name + scope · trust · status).
    let mut items: Vec<ListItem> = Vec::new();
    if state.skills.is_empty() {
        items.push(ListItem::new(Line::styled(
            "  no skills registered",
            Style::default().fg(theme.text.muted),
        )));
    }
    for (idx, skill) in state.skills.iter().enumerate() {
        let selected = idx == state.selected_skill;
        let marker = if selected { "› " } else { "  " };
        let head = Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.focus.active)),
            Span::styled(
                truncate(&skill.name, 26),
                Style::default().fg(theme.text.primary),
            ),
        ]);
        let meta = Line::styled(
            format!("    {} · {} · {}", skill.scope, skill.trust, skill.status),
            Style::default().fg(theme.text.muted),
        );
        let item = ListItem::new(vec![head, meta]);
        items.push(if selected {
            item.style(theme.selection_style())
        } else {
            item
        });
    }
    frame.render_widget(
        List::new(items).style(Style::default().bg(theme.surface.overlay)),
        cols[0],
    );

    // Right: the detail panel for the focused skill.
    let detail_block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(theme.focus.inactive))
        .style(Style::default().bg(theme.surface.overlay));
    let mut lines: Vec<Line> = Vec::new();
    if let Some(skill) = state.focused_skill() {
        lines.push(Line::styled(
            format!("{} — {}", skill.name, skill.kind),
            Style::default()
                .fg(theme.text.heading)
                .add_modifier(Modifier::BOLD),
        ));
        let field = |k: &str, v: &str, color: Color| -> Line {
            Line::from(vec![
                Span::styled(format!("  {k}: "), Style::default().fg(theme.text.muted)),
                Span::styled(v.to_owned(), Style::default().fg(color)),
            ])
        };
        lines.push(field("scope", &skill.scope, theme.text.primary));
        lines.push(field("trust", &skill.trust, theme.text.secondary));
        lines.push(field("status", &skill.status, theme.text.secondary));
        lines.push(field(
            "risk",
            &skill.risk,
            skill_risk_color(&skill.risk, theme),
        ));
        lines.push(Line::raw(""));
        lines.push(section("Description", theme));
        lines.push(Line::styled(
            format!("  {}", skill.description),
            Style::default().fg(theme.text.primary),
        ));
        lines.push(Line::raw(""));
        lines.push(section("Permissions", theme));
        if skill.permissions.is_empty() {
            lines.push(Line::styled(
                "  (no permissions requested)",
                Style::default().fg(theme.text.muted),
            ));
        } else {
            // Verbatim: each requested capability exactly as the package declared
            // it — never paraphrased ("skill permissions are visible").
            for permission in &skill.permissions {
                lines.push(Line::from(vec![
                    Span::styled("  • ", Style::default().fg(theme.status.warning)),
                    Span::styled(permission.clone(), Style::default().fg(theme.text.primary)),
                ]));
            }
        }
    } else {
        lines.push(Line::styled(
            "  no skill selected",
            Style::default().fg(theme.text.muted),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  ↑/↓ select · M memory · Esc close",
        Style::default().fg(theme.text.muted),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(detail_block)
            .wrap(Wrap { trim: false }),
        cols[1],
    );
}

/// The model picker (MP1): a filter line (the command-palette shape) over a
/// two-column list+detail view (the [`render_skills`] template) — the
/// selectable models on the left (current run's serving model marked), and a
/// detail panel for the focused model's provider/location/cost/context on the
/// right. Selecting a row stages it on [`AppState::pending_model`], which PINS
/// the model for the run(s) the operator starts (STEP MP2 — a session default:
/// one pick applies to this run and every subsequent one until changed). Colors
/// are Theme tokens only (RULE 7).
fn render_model_picker(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    query: &str,
    selected: usize,
) {
    let rect = centered_rect(84, 84, area);
    frame.render_widget(Clear, rect);

    let outer = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" Model picker ({}) ", state.models.len()),
            Style::default()
                .fg(theme.text.heading)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme.focus.active))
        .style(
            Style::default()
                .bg(theme.surface.overlay)
                .fg(theme.text.primary),
        );
    let inner = outer.inner(rect);
    frame.render_widget(outer, rect);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    // The filter line, with a block cursor so it reads as an input (the
    // command palette's shape).
    let filter = Line::from(vec![
        Span::styled("› ", Style::default().fg(theme.focus.active)),
        Span::styled(query.to_owned(), Style::default().fg(theme.text.primary)),
        Span::styled("▏", Style::default().fg(theme.focus.active)),
    ]);
    frame.render_widget(
        Paragraph::new(filter).style(Style::default().bg(theme.surface.overlay)),
        rows[0],
    );

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(rows[1]);

    // The active run's serving model, if any — marks the current row/detail.
    let current = state.selected_run().and_then(|run| run.model.as_ref());

    // Left: the filtered model list (id, current marker, provider + badges).
    let matches = filter_models(&state.models, query);
    let mut items: Vec<ListItem> = Vec::new();
    if state.models.is_empty() {
        items.push(ListItem::new(Line::styled(
            "  no models configured",
            Style::default().fg(theme.text.muted),
        )));
    } else if matches.is_empty() {
        items.push(ListItem::new(Line::styled(
            "  no matching model",
            Style::default().fg(theme.text.muted),
        )));
    }
    for (row, &idx) in matches.iter().enumerate() {
        let card = &state.models[idx];
        let is_selected = row == selected;
        let is_current = current == Some(&card.id);
        let head = Line::from(vec![
            Span::styled(
                if is_selected { "› " } else { "  " },
                Style::default().fg(theme.focus.active),
            ),
            Span::styled(
                if is_current { "● " } else { "  " },
                Style::default().fg(theme.status.success),
            ),
            Span::styled(
                truncate(&card.id.0, 26),
                Style::default().fg(theme.text.primary),
            ),
        ]);
        // Provider and badges each get their own line (rather than one long
        // joined line) so they survive the list's fixed-width column without
        // truncating the trailing badges off a narrow terminal.
        let provider_line = Line::styled(
            format!("      {}", card.provider),
            Style::default().fg(theme.text.muted),
        );
        let badges_line = Line::styled(
            format!("      {}", model_badges(card)),
            Style::default().fg(theme.text.muted),
        );
        let item = ListItem::new(vec![head, provider_line, badges_line]);
        items.push(if is_selected {
            item.style(theme.selection_style())
        } else {
            item
        });
    }
    frame.render_widget(
        List::new(items).style(Style::default().bg(theme.surface.overlay)),
        cols[0],
    );

    // Right: the detail panel for the focused model.
    let detail_block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(theme.focus.inactive))
        .style(Style::default().bg(theme.surface.overlay));
    let mut lines: Vec<Line> = Vec::new();
    if let Some(card) = state.focused_model() {
        let is_current = current == Some(&card.id);
        lines.push(Line::from(vec![
            Span::styled(
                card.id.0.clone(),
                Style::default()
                    .fg(theme.text.heading)
                    .add_modifier(Modifier::BOLD),
            ),
            if is_current {
                Span::styled(
                    "  ● current".to_owned(),
                    Style::default().fg(theme.status.success),
                )
            } else {
                Span::raw("")
            },
        ]));
        let field = |k: &str, v: String, color: Color| -> Line {
            Line::from(vec![
                Span::styled(format!("  {k}: "), Style::default().fg(theme.text.muted)),
                Span::styled(v, Style::default().fg(color)),
            ])
        };
        lines.push(field(
            "provider",
            card.provider.clone(),
            theme.text.secondary,
        ));
        lines.push(field(
            "location",
            location_label(card.location).to_owned(),
            theme.text.secondary,
        ));
        lines.push(field(
            "cost",
            cost_label(card.cost_per_1k_usd),
            theme.status.warning,
        ));
        lines.push(field(
            "context",
            context_label(card.context_tokens),
            theme.status.info,
        ));
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "  Enter stages this model for your next run",
            Style::default().fg(theme.text.muted),
        ));
    } else {
        lines.push(Line::styled(
            "  no model selected",
            Style::default().fg(theme.text.muted),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  ↑/↓ select · Enter stage · Esc close",
        Style::default().fg(theme.text.muted),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(detail_block)
            .wrap(Wrap { trim: false }),
        cols[1],
    );
}

/// A model card's badges, space-joined: `local ✓` / `hosted` (or nothing when
/// unprofiled), the measured cost per 1K tokens, and the declared context
/// window — each rendered `—` when the model has no measured profile
/// (best-effort; `models.toml` is the authoritative selectable list).
fn model_badges(card: &ModelCard) -> String {
    format!(
        "{} · {} · {}",
        location_label(card.location),
        cost_label(card.cost_per_1k_usd),
        context_label(card.context_tokens)
    )
}

fn location_label(location: Option<ModelLocationLabel>) -> &'static str {
    match location {
        Some(ModelLocationLabel::Local) => "local ✓",
        Some(ModelLocationLabel::Hosted) => "hosted",
        None => "—",
    }
}

fn cost_label(cost_per_1k_usd: Option<f64>) -> String {
    match cost_per_1k_usd {
        Some(cost) => format!("${cost}/1k"),
        None => "—".to_owned(),
    }
}

fn context_label(context_tokens: Option<u64>) -> String {
    match context_tokens {
        Some(tokens) => format!("{}k", tokens / 1000),
        None => "—".to_owned(),
    }
}

/// The memory browser (STEP 2.6): the visible-scope memories on the left, and a
/// Chapter 06 provenance card for the focused memory on the right (fact, source,
/// revision, observed, scope, confidence), with an "open source" affordance.
/// When `source_open`, the full source string is surfaced in place — the TUI
/// does no I/O, so opening reveals rather than launches a file.
fn render_memory(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    source_open: bool,
) {
    let rect = centered_rect(84, 84, area);
    frame.render_widget(Clear, rect);

    let outer = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" Memory ({}) ", state.memories.len()),
            Style::default()
                .fg(theme.text.heading)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme.focus.active))
        .style(
            Style::default()
                .bg(theme.surface.overlay)
                .fg(theme.text.primary),
        );
    let inner = outer.inner(rect);
    frame.render_widget(outer, rect);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(inner);

    // Left: the memory list (statement + class · scope).
    let mut items: Vec<ListItem> = Vec::new();
    if state.memories.is_empty() {
        items.push(ListItem::new(Line::styled(
            "  no memories in scope",
            Style::default().fg(theme.text.muted),
        )));
    }
    for (idx, memory) in state.memories.iter().enumerate() {
        let selected = idx == state.selected_memory;
        let marker = if selected { "› " } else { "  " };
        let head = Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.focus.active)),
            Span::styled(
                truncate(&memory.statement, 26),
                Style::default().fg(theme.text.primary),
            ),
        ]);
        let meta = Line::styled(
            format!("    {} · {}", memory.class, memory.scope),
            Style::default().fg(theme.text.muted),
        );
        let item = ListItem::new(vec![head, meta]);
        items.push(if selected {
            item.style(theme.selection_style())
        } else {
            item
        });
    }
    frame.render_widget(
        List::new(items).style(Style::default().bg(theme.surface.overlay)),
        cols[0],
    );

    // Right: the provenance card for the focused memory.
    let card_block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(theme.focus.inactive))
        .style(Style::default().bg(theme.surface.overlay));
    let mut lines: Vec<Line> = Vec::new();
    if let Some(memory) = state.focused_memory() {
        let field = |k: &str, v: &str, color: Color| -> Line {
            Line::from(vec![
                Span::styled(format!("  {k}: "), Style::default().fg(theme.text.muted)),
                Span::styled(v.to_owned(), Style::default().fg(color)),
            ])
        };
        lines.push(section("Provenance card", theme));
        lines.push(field("Fact", &memory.statement, theme.text.primary));
        lines.push(field("Source", &memory.source, theme.text.secondary));
        lines.push(field("Revision", &memory.revision, theme.text.secondary));
        lines.push(field("Observed", &memory.observed, theme.text.secondary));
        lines.push(field("Scope", &memory.scope, theme.text.secondary));
        lines.push(field(
            "Confidence",
            &format!("{:.2}", memory.confidence),
            theme.status.info,
        ));
        lines.push(Line::raw(""));
        if source_open {
            // Opened: surface the full source string, marked as revealed.
            lines.push(Line::styled(
                "  ▼ source opened",
                Style::default()
                    .fg(theme.status.success)
                    .add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::styled(
                format!("    {}", memory.source),
                Style::default().fg(theme.text.primary),
            ));
        } else {
            lines.push(Line::styled(
                "  [o] open source",
                Style::default().fg(theme.status.info),
            ));
        }
    } else {
        lines.push(Line::styled(
            "  no memory selected",
            Style::default().fg(theme.text.muted),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  ↑/↓ select · S skills · Esc close",
        Style::default().fg(theme.text.muted),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(card_block)
            .wrap(Wrap { trim: false }),
        cols[1],
    );
}

/// The Docs Studio browser (Phase 4 client wiring): a document **tree** on the
/// left; on the right, the focused document's **editor rail** (its blocks in
/// order) over its **review rail** (pending suggestions). Read-only — the live
/// CRDT edit transport is a separate follow-up. Colors are Theme tokens only
/// (RULE 7).
fn render_docs(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let rect = centered_rect(86, 86, area);
    frame.render_widget(Clear, rect);

    let outer = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" Docs Studio ({}) ", state.docs.len()),
            Style::default()
                .fg(theme.text.heading)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme.focus.active))
        .style(
            Style::default()
                .bg(theme.surface.overlay)
                .fg(theme.text.primary),
        );
    let inner = outer.inner(rect);
    frame.render_widget(outer, rect);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
        .split(inner);

    // Left: the document tree (title + scope · status · mode).
    let mut items: Vec<ListItem> = Vec::new();
    if state.docs.is_empty() {
        items.push(ListItem::new(Line::styled(
            "  no documents in scope",
            Style::default().fg(theme.text.muted),
        )));
    }
    for (idx, doc) in state.docs.iter().enumerate() {
        let selected = idx == state.selected_doc;
        let marker = if selected { "› " } else { "  " };
        let head = Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.focus.active)),
            Span::styled(
                truncate(&doc.title, 28),
                Style::default().fg(theme.text.primary),
            ),
        ]);
        let meta = Line::styled(
            format!("    {} · {} · {}", doc.scope, doc.status, doc.mode),
            Style::default().fg(theme.text.muted),
        );
        let item = ListItem::new(vec![head, meta]);
        items.push(if selected {
            item.style(theme.selection_style())
        } else {
            item
        });
    }
    frame.render_widget(
        List::new(items).style(Style::default().bg(theme.surface.overlay)),
        cols[0],
    );

    // Right: the editor rail (blocks) over the review rail (suggestions).
    let rails = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(cols[1]);

    let editor_block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(theme.focus.inactive))
        .style(Style::default().bg(theme.surface.overlay));
    let mut editor_lines: Vec<Line> = Vec::new();
    if let Some(doc) = state.focused_doc() {
        editor_lines.push(Line::styled(
            format!("{} ({})", doc.title, doc.revision),
            Style::default()
                .fg(theme.text.heading)
                .add_modifier(Modifier::BOLD),
        ));
        // The editor rail header carries the presence-lite lease indicator: whether
        // this client holds / is acquiring / is blocked on a block lease.
        let editing = state.doc_focus == DocFocus::Editor;
        editor_lines.push(Line::from(vec![
            section_span("Editor rail", theme),
            Span::styled(
                if editing { "  [focused]" } else { "" }.to_owned(),
                Style::default().fg(theme.focus.active),
            ),
            lease_span(state, theme),
        ]));
        if doc.blocks.is_empty() {
            editor_lines.push(Line::styled(
                "  (empty document)",
                Style::default().fg(theme.text.muted),
            ));
        }
        for (idx, block) in doc.blocks.iter().enumerate() {
            let focused = editing && idx == state.selected_block;
            let marker = if focused { "› " } else { "  " };
            let kind_style = if focused {
                Style::default().fg(theme.focus.active)
            } else {
                Style::default().fg(theme.text.secondary)
            };
            editor_lines.push(Line::from(vec![
                Span::styled(format!("{marker}{:<10}", block.kind), kind_style),
                Span::styled(
                    truncate(&block.text, 58),
                    Style::default().fg(theme.text.primary),
                ),
            ]));
        }
    } else {
        editor_lines.push(Line::styled(
            "  no document selected",
            Style::default().fg(theme.text.muted),
        ));
    }
    frame.render_widget(
        Paragraph::new(editor_lines)
            .block(editor_block)
            .wrap(Wrap { trim: false }),
        rails[0],
    );

    let review_block = Block::default()
        .borders(Borders::LEFT | Borders::TOP)
        .border_style(Style::default().fg(theme.focus.inactive))
        .style(Style::default().bg(theme.surface.overlay));
    let mut review_lines: Vec<Line> = Vec::new();
    if let Some(doc) = state.focused_doc() {
        let reviewing = state.doc_focus == DocFocus::Review;
        review_lines.push(Line::from(vec![
            section_span("Review rail (suggestions)", theme),
            Span::styled(
                if reviewing { "  [focused]" } else { "" }.to_owned(),
                Style::default().fg(theme.focus.active),
            ),
        ]));
        if doc.suggestions.is_empty() {
            review_lines.push(Line::styled(
                "  no pending suggestions",
                Style::default().fg(theme.text.muted),
            ));
        }
        for (idx, suggestion) in doc.suggestions.iter().enumerate() {
            let focused = reviewing && idx == state.selected_suggestion;
            let bullet = if focused { "› " } else { "  • " };
            let bullet_style = if focused {
                Style::default().fg(theme.focus.active)
            } else {
                Style::default().fg(theme.status.info)
            };
            review_lines.push(Line::from(vec![
                Span::styled(bullet, bullet_style),
                Span::styled(
                    format!("{} @ {} ", suggestion.author, suggestion.range),
                    Style::default().fg(theme.text.muted),
                ),
                Span::styled(
                    format!("→ {}", truncate(&suggestion.replacement, 40)),
                    Style::default().fg(theme.text.primary),
                ),
            ]));
            if let Some(rationale) = &suggestion.rationale {
                review_lines.push(Line::styled(
                    format!("      {rationale}"),
                    Style::default().fg(theme.text.secondary),
                ));
            }
        }
    }
    review_lines.push(Line::raw(""));
    review_lines.push(Line::styled(
        "  Tab rail · ↑/↓ select · e edit · a/r accept/reject · Esc close",
        Style::default().fg(theme.text.muted),
    ));
    frame.render_widget(
        Paragraph::new(review_lines)
            .block(review_block)
            .wrap(Wrap { trim: false }),
        rails[1],
    );
}

/// The code-graph edge inspector (Phase 4 exit criterion 4): the repository's
/// edges on the left, and for the focused edge its relation, confidence,
/// evidence kind + source, and revision on the right — the evidence-and-revision
/// payload the criterion calls for. Colors are Theme tokens only (RULE 7).
fn render_edges(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let rect = centered_rect(86, 86, area);
    frame.render_widget(Clear, rect);

    let outer = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" Code-graph edges ({}) ", state.edges.len()),
            Style::default()
                .fg(theme.text.heading)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme.focus.active))
        .style(
            Style::default()
                .bg(theme.surface.overlay)
                .fg(theme.text.primary),
        );
    let inner = outer.inner(rect);
    frame.render_widget(outer, rect);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
        .split(inner);

    // Left: the edge list (relation, then from → to).
    let mut items: Vec<ListItem> = Vec::new();
    if state.edges.is_empty() {
        items.push(ListItem::new(Line::styled(
            "  no edges in this repository",
            Style::default().fg(theme.text.muted),
        )));
    }
    for (idx, edge) in state.edges.iter().enumerate() {
        let selected = idx == state.selected_edge;
        let marker = if selected { "› " } else { "  " };
        let head = Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.focus.active)),
            Span::styled(
                truncate(&edge.relation, 14),
                Style::default().fg(theme.text.secondary),
            ),
        ]);
        let meta = Line::styled(
            format!(
                "    {} → {}",
                truncate(&edge.from, 16),
                truncate(&edge.to, 16)
            ),
            Style::default().fg(theme.text.muted),
        );
        let item = ListItem::new(vec![head, meta]);
        items.push(if selected {
            item.style(theme.selection_style())
        } else {
            item
        });
    }
    frame.render_widget(
        List::new(items).style(Style::default().bg(theme.surface.overlay)),
        cols[0],
    );

    // Right: the detail for the focused edge — relation, confidence, and the
    // exit-criterion payload: evidence kind + source + revision.
    let detail_block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(theme.focus.inactive))
        .style(Style::default().bg(theme.surface.overlay));
    let mut lines: Vec<Line> = Vec::new();
    if let Some(edge) = state.focused_edge() {
        let field = |k: &str, v: &str, color: Color| -> Line {
            Line::from(vec![
                Span::styled(format!("  {k}: "), Style::default().fg(theme.text.muted)),
                Span::styled(v.to_owned(), Style::default().fg(color)),
            ])
        };
        lines.push(section("Edge", theme));
        lines.push(field("from", &edge.from, theme.text.primary));
        lines.push(field("to", &edge.to, theme.text.primary));
        lines.push(field("relation", &edge.relation, theme.text.secondary));
        lines.push(field(
            "confidence",
            &format!("{:.2}", edge.confidence),
            edge_confidence_color(edge.confidence, theme),
        ));
        lines.push(Line::raw(""));
        lines.push(section("Evidence", theme));
        lines.push(field("kind", &edge.evidence_kind, theme.status.info));
        lines.push(field("source", &edge.evidence, theme.text.secondary));
        lines.push(field("revision", &edge.revision, theme.text.secondary));
    } else {
        lines.push(Line::styled(
            "  no edge selected",
            Style::default().fg(theme.text.muted),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  ↑/↓ select · D docs · Esc close",
        Style::default().fg(theme.text.muted),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(detail_block)
            .wrap(Wrap { trim: false }),
        cols[1],
    );
}

/// The workflow-graph view (Phase 5 STEP 5.2, exit criterion 3): a list of the
/// compiled workflow's nodes on the left — grouped by workflow, in topological
/// order — and, for the focused node, its action, state, agent, workspace,
/// approval, retry, dependencies, and declared outputs on the right. Read-only:
/// a projection of the compiled graph, with per-node state/cost overlaid from a
/// durable run when one exists (`pending` / `—` otherwise). Colors are Theme
/// tokens only (RULE 7).
fn render_workflow(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let rect = centered_rect(86, 86, area);
    frame.render_widget(Clear, rect);

    let outer = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" Workflow ({} node(s)) ", state.workflow.len()),
            Style::default()
                .fg(theme.text.heading)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme.focus.active))
        .style(
            Style::default()
                .bg(theme.surface.overlay)
                .fg(theme.text.primary),
        );
    let inner = outer.inner(rect);
    frame.render_widget(outer, rect);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
        .split(inner);

    // Left: the node list, in topological order. A workflow-label header is
    // folded into the first node of each group so item↔card stays 1:1 (the
    // selection indexes `state.workflow` directly) while the graph still reads
    // as grouped when a repository declares more than one workflow.
    let mut items: Vec<ListItem> = Vec::new();
    if state.workflow.is_empty() {
        items.push(ListItem::new(Line::styled(
            "  no workflow manifests in this repository",
            Style::default().fg(theme.text.muted),
        )));
    }
    let mut previous_workflow: Option<&str> = None;
    for (idx, node) in state.workflow.iter().enumerate() {
        let selected = idx == state.selected_node;
        let marker = if selected { "› " } else { "  " };
        let mut lines: Vec<Line> = Vec::new();
        if previous_workflow != Some(node.workflow.as_str()) {
            lines.push(Line::styled(
                node.workflow.clone(),
                Style::default()
                    .fg(theme.text.heading)
                    .add_modifier(Modifier::BOLD),
            ));
            previous_workflow = Some(node.workflow.as_str());
        }
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.focus.active)),
            Span::styled(
                truncate(&node.id, 20),
                Style::default().fg(theme.text.primary),
            ),
            Span::raw("  "),
            Span::styled(
                node.state.clone(),
                Style::default().fg(node_state_color(&node.state, theme)),
            ),
        ]));
        lines.push(Line::styled(
            format!("    {}", truncate(&node.action, 34)),
            Style::default().fg(theme.text.muted),
        ));
        let item = ListItem::new(lines);
        items.push(if selected {
            item.style(theme.selection_style())
        } else {
            item
        });
    }
    frame.render_widget(
        List::new(items).style(Style::default().bg(theme.surface.overlay)),
        cols[0],
    );

    // Right: the detail for the focused node — the exit-criterion payload
    // (state, agent, worktree, cost) plus the graph edges and declared outputs.
    let detail_block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(theme.focus.inactive))
        .style(Style::default().bg(theme.surface.overlay));
    let mut lines: Vec<Line> = Vec::new();
    if let Some(node) = state.focused_node() {
        let field = |k: &str, v: &str, color: Color| -> Line {
            Line::from(vec![
                Span::styled(format!("  {k}: "), Style::default().fg(theme.text.muted)),
                Span::styled(v.to_owned(), Style::default().fg(color)),
            ])
        };
        lines.push(section("Node", theme));
        lines.push(field("workflow", &node.workflow, theme.text.secondary));
        lines.push(field("id", &node.id, theme.text.primary));
        lines.push(field(
            "state",
            &node.state,
            node_state_color(&node.state, theme),
        ));
        lines.push(Line::raw(""));
        lines.push(section("Action", theme));
        lines.push(field("action", &node.action, theme.text.secondary));
        lines.push(field("agent", &node.agent, theme.text.primary));
        lines.push(field("model policy", &node.model_policy, theme.text.muted));
        lines.push(Line::raw(""));
        lines.push(section("Execution", theme));
        lines.push(field("worktree", &node.workspace, theme.status.info));
        lines.push(field("approval", &node.approval, theme.text.secondary));
        lines.push(field("retry", &node.retry, theme.text.secondary));
        lines.push(field("cost", &node.cost, theme.text.secondary));
        // The durable failure/block reason, when a run recorded one (P5-D4) —
        // shown in the mode's error color so a blocked/failed node explains itself.
        if node.error != "\u{2014}" {
            lines.push(field("error", &node.error, theme.status.error));
        }
        lines.push(Line::raw(""));
        lines.push(section("Graph", theme));
        lines.push(field("depends on", &node.depends_on, theme.text.secondary));
        lines.push(field("outputs", &node.outputs, theme.text.secondary));
    } else {
        lines.push(Line::styled(
            "  no node selected",
            Style::default().fg(theme.text.muted),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  ↑/↓ select · Esc close",
        Style::default().fg(theme.text.muted),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(detail_block)
            .wrap(Wrap { trim: false }),
        cols[1],
    );
}

/// Color for a workflow node's lifecycle state. Terminal-success reads calm;
/// active states draw the eye; failure/blocked read as error; not-yet-run
/// (`pending`) and `skipped` stay quiet.
fn node_state_color(state: &str, theme: &Theme) -> Color {
    match state {
        "completed" => theme.status.success,
        "running" => theme.status.running,
        "waiting_approval" => theme.status.warning,
        "failed" | "blocked" => theme.status.error,
        "pending" => theme.status.info,
        _ => theme.text.muted,
    }
}

/// The blackboard view (Phase 5 STEP 5.3): the typed artifacts agents share
/// within a workflow run — a list on the left, grouped by run, and, for the
/// focused item, its kind, author, confidence, evidence, revision, and payload
/// summary on the right. Read-only. Colors are Theme tokens only (RULE 7).
fn render_blackboard(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let rect = centered_rect(86, 86, area);
    frame.render_widget(Clear, rect);

    let outer = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" Blackboard ({} item(s)) ", state.blackboard.len()),
            Style::default()
                .fg(theme.text.heading)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme.focus.active))
        .style(
            Style::default()
                .bg(theme.surface.overlay)
                .fg(theme.text.primary),
        );
    let inner = outer.inner(rect);
    frame.render_widget(outer, rect);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
        .split(inner);

    // Left: the artifact list, grouped by run (the run header is folded into the
    // first item of each group so item↔card stays 1:1 with the selection index).
    let mut items: Vec<ListItem> = Vec::new();
    if state.blackboard.is_empty() {
        items.push(ListItem::new(Line::styled(
            "  no blackboard artifacts on the active runs",
            Style::default().fg(theme.text.muted),
        )));
    }
    let mut previous_run: Option<&str> = None;
    for (idx, card) in state.blackboard.iter().enumerate() {
        let selected = idx == state.selected_item;
        let marker = if selected { "› " } else { "  " };
        let mut lines: Vec<Line> = Vec::new();
        if previous_run != Some(card.run.as_str()) {
            lines.push(Line::styled(
                card.run.clone(),
                Style::default()
                    .fg(theme.text.heading)
                    .add_modifier(Modifier::BOLD),
            ));
            previous_run = Some(card.run.as_str());
        }
        // A superseded artifact is dimmed; the live one reads normally.
        let kind_color = if card.superseded {
            theme.text.muted
        } else {
            theme.status.info
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.focus.active)),
            Span::styled(truncate(&card.kind, 16), Style::default().fg(kind_color)),
            if card.superseded {
                Span::styled(" (superseded)", Style::default().fg(theme.text.muted))
            } else {
                Span::raw("")
            },
        ]));
        lines.push(Line::styled(
            format!("    {}", truncate(&card.summary, 34)),
            Style::default().fg(theme.text.muted),
        ));
        let item = ListItem::new(lines);
        items.push(if selected {
            item.style(theme.selection_style())
        } else {
            item
        });
    }
    frame.render_widget(
        List::new(items).style(Style::default().bg(theme.surface.overlay)),
        cols[0],
    );

    // Right: the detail for the focused artifact — kind, author, confidence, the
    // evidence that grounds it (claim-like kinds always carry it), revision, and a
    // payload summary.
    let detail_block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(theme.focus.inactive))
        .style(Style::default().bg(theme.surface.overlay));
    let mut lines: Vec<Line> = Vec::new();
    if let Some(card) = state.focused_item() {
        let field = |k: &str, v: &str, color: Color| -> Line {
            Line::from(vec![
                Span::styled(format!("  {k}: "), Style::default().fg(theme.text.muted)),
                Span::styled(v.to_owned(), Style::default().fg(color)),
            ])
        };
        lines.push(section("Artifact", theme));
        lines.push(field("run", &card.run, theme.text.secondary));
        lines.push(field("kind", &card.kind, theme.status.info));
        lines.push(field("revision", &card.revision, theme.text.secondary));
        if card.superseded {
            lines.push(field("status", "superseded", theme.text.muted));
        }
        lines.push(Line::raw(""));
        lines.push(section("Provenance", theme));
        lines.push(field("author", &card.author, theme.text.primary));
        lines.push(field("confidence", &card.confidence, theme.text.secondary));
        lines.push(field("evidence", &card.evidence, theme.text.secondary));
        lines.push(Line::raw(""));
        lines.push(section("Payload", theme));
        for line in textwrap_summary(&card.summary) {
            lines.push(Line::styled(
                format!("  {line}"),
                Style::default().fg(theme.text.secondary),
            ));
        }
    } else {
        lines.push(Line::styled(
            "  no artifact selected",
            Style::default().fg(theme.text.muted),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  ↑/↓ select · Esc close",
        Style::default().fg(theme.text.muted),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(detail_block)
            .wrap(Wrap { trim: false }),
        cols[1],
    );
}

/// Split a one-line summary into wrapped display lines for the payload panel. A
/// plain char-count wrap (the summary is already a single pre-rendered line, so a
/// word-aware wrap is unnecessary here) keeping each chunk within the panel.
fn textwrap_summary(summary: &str) -> Vec<String> {
    const WIDTH: usize = 48;
    if summary.is_empty() {
        return vec!["(empty)".to_owned()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in summary.split_whitespace() {
        // A single word wider than the panel (a long path, URL, or hash) is
        // hard-split into width-sized chunks so no produced line overflows.
        if word.chars().count() > WIDTH {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            let mut chars = word.chars().peekable();
            while chars.peek().is_some() {
                let chunk: String = chars.by_ref().take(WIDTH).collect();
                // Push full-width chunks; keep the short remainder in `current` so
                // a following word can still join it.
                if chars.peek().is_some() {
                    lines.push(chunk);
                } else {
                    current = chunk;
                }
            }
            continue;
        }
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > WIDTH {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// The command palette: a filter line over a searchable list of every command,
/// so the growing feature set is reachable without a permanent pane or a
/// single-key binding each. Colors are Theme tokens only (RULE 7).
fn render_palette(frame: &mut Frame, area: Rect, theme: &Theme, query: &str, selected: usize) {
    let rect = centered_rect(72, 70, area);
    frame.render_widget(Clear, rect);

    let outer = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " Command palette ",
            Style::default()
                .fg(theme.text.heading)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(theme.focus.active))
        .style(
            Style::default()
                .bg(theme.surface.overlay)
                .fg(theme.text.primary),
        );
    let inner = outer.inner(rect);
    frame.render_widget(outer, rect);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(inner);

    // The filter line, with a block cursor so it reads as an input.
    let filter = Line::from(vec![
        Span::styled("› ", Style::default().fg(theme.focus.active)),
        Span::styled(query.to_owned(), Style::default().fg(theme.text.primary)),
        Span::styled("▏", Style::default().fg(theme.focus.active)),
    ]);
    frame.render_widget(
        Paragraph::new(vec![
            filter,
            Line::styled(
                "  ↑/↓ select · Enter run · Esc close",
                Style::default().fg(theme.text.muted),
            ),
        ])
        .style(Style::default().bg(theme.surface.overlay)),
        rows[0],
    );

    // The filtered command list.
    let matches = crate::palette::filtered(query);
    let mut items: Vec<ListItem> = Vec::new();
    if matches.is_empty() {
        items.push(ListItem::new(Line::styled(
            "  no matching command",
            Style::default().fg(theme.text.muted),
        )));
    }
    for (idx, entry) in matches.iter().enumerate() {
        let is_selected = idx == selected;
        let marker = if is_selected { "› " } else { "  " };
        let head = Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.focus.active)),
            Span::styled(
                format!("{:<20}", entry.title),
                Style::default().fg(theme.text.primary),
            ),
            Span::styled(
                entry.description.to_owned(),
                Style::default().fg(theme.text.muted),
            ),
            Span::styled(
                format!("  [{}]", entry.key),
                Style::default().fg(theme.status.info),
            ),
        ]);
        let item = ListItem::new(head);
        items.push(if is_selected {
            item.style(theme.selection_style())
        } else {
            item
        });
    }
    frame.render_widget(
        List::new(items).style(Style::default().bg(theme.surface.overlay)),
        rows[1],
    );
}

/// Color an edge's confidence by tier (Chapter 07): a syntax-inferred call
/// (~0.45) reads as tentative; an LSP/compiler-resolved edge (≥0.90) as trusted.
fn edge_confidence_color(confidence: f32, theme: &Theme) -> Color {
    if confidence >= 0.90 {
        theme.status.success
    } else if confidence >= 0.60 {
        theme.status.warning
    } else {
        theme.text.muted
    }
}

/// Color for a skill's coarse risk label (`safe` / `low` / `medium` / `high`).
fn skill_risk_color(risk: &str, theme: &Theme) -> Color {
    match risk {
        "safe" | "low" => theme.status.success,
        "medium" => theme.status.warning,
        "high" => theme.status.error,
        _ => theme.text.muted,
    }
}

fn render_approval_modal(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(approval) = state.focused_approval() else {
        return;
    };
    let rect = centered_rect(70, 60, area);
    frame.render_widget(Clear, rect);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::styled(
        "Approval required",
        Style::default()
            .fg(theme.text.heading)
            .add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::raw(""));

    lines.push(section("Action", theme));
    for detail in describe_action(&approval.action) {
        lines.push(Line::styled(
            format!("  {detail}"),
            Style::default().fg(theme.text.primary),
        ));
    }
    lines.push(Line::raw(""));

    lines.push(section("Risk", theme));
    lines.extend(risk_lines(&approval.risk, theme));
    lines.push(Line::raw(""));

    lines.push(section("Requested capabilities", theme));
    lines.push(Line::styled(
        format!("  {}", capability_label(&approval.action)),
        Style::default().fg(theme.text.primary),
    ));
    lines.push(Line::raw(""));

    lines.push(Line::from(vec![
        Span::styled(
            "[a] approve once   ",
            Style::default().fg(theme.status.success),
        ),
        Span::styled(
            "[A] approve for run   ",
            Style::default().fg(theme.status.success),
        ),
        Span::styled("[r] reject", Style::default().fg(theme.status.error)),
    ]));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Approval ")
        .border_style(Style::default().fg(theme.status.warning))
        .style(
            Style::default()
                .bg(theme.surface.overlay)
                .fg(theme.text.primary),
        );
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn render_help(frame: &mut Frame, area: Rect, theme: &Theme) {
    let rect = centered_rect(70, 80, area);
    frame.render_widget(Clear, rect);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::styled(
        "Keys — every mouse action has a keyboard equivalent",
        Style::default()
            .fg(theme.text.heading)
            .add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::raw(""));
    for binding in crate::input::KEY_BINDINGS {
        let mut spans = vec![
            Span::styled(
                format!("  {:<12}", binding.keys),
                Style::default()
                    .fg(theme.status.info)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                binding.description.to_owned(),
                Style::default().fg(theme.text.primary),
            ),
        ];
        if let Some(mouse) = binding.mouse {
            spans.push(Span::styled(
                format!("  (mouse: {mouse})"),
                Style::default().fg(theme.text.muted),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "q detaches this client — it never stops the run.  ? or Esc closes.",
        Style::default().fg(theme.text.secondary),
    ));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(theme.focus.active))
        .style(
            Style::default()
                .bg(theme.surface.overlay)
                .fg(theme.text.primary),
        );
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn render_prompt(frame: &mut Frame, area: Rect, theme: &Theme, title: &str, buffer: &str) {
    let rect = centered_rect(70, 20, area);
    frame.render_widget(Clear, rect);
    let lines = vec![
        Line::styled(title, Style::default().fg(theme.text.heading)),
        Line::from(vec![
            Span::styled("› ", Style::default().fg(theme.focus.active)),
            Span::styled(buffer.to_owned(), Style::default().fg(theme.text.primary)),
            Span::styled("█", Style::default().fg(theme.focus.active)),
        ]),
        Line::styled(
            "Enter to submit · Esc to cancel",
            Style::default().fg(theme.text.muted),
        ),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.focus.active))
        .style(
            Style::default()
                .bg(theme.surface.overlay)
                .fg(theme.text.primary),
        );
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn render_confirm(frame: &mut Frame, area: Rect, theme: &Theme) {
    let rect = centered_rect(60, 20, area);
    frame.render_widget(Clear, rect);
    let lines = vec![
        Line::styled(
            "Cancel this run?",
            Style::default()
                .fg(theme.text.heading)
                .add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            "Cancelling stops the run; a chronicle and any artifacts are kept.",
            Style::default().fg(theme.text.secondary),
        ),
        Line::from(vec![
            Span::styled(
                "[y] yes, cancel   ",
                Style::default().fg(theme.status.error),
            ),
            Span::styled("[n] no", Style::default().fg(theme.status.success)),
        ]),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Confirm ")
        .border_style(Style::default().fg(theme.status.error))
        .style(
            Style::default()
                .bg(theme.surface.overlay)
                .fg(theme.text.primary),
        );
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn section(title: &str, theme: &Theme) -> Line<'static> {
    Line::from(section_span(title, theme))
}

/// The `section` heading as a [`Span`], for composing into a header line that also
/// carries trailing status (e.g. the Docs editor-rail lease indicator).
fn section_span(title: &str, theme: &Theme) -> Span<'static> {
    Span::styled(
        title.to_owned(),
        Style::default()
            .fg(theme.text.heading)
            .add_modifier(Modifier::UNDERLINED),
    )
}

/// The presence-lite edit-lease indicator for the Docs editor rail: whether this
/// client holds, is acquiring, or is blocked on a block lease. Empty when there is
/// no in-flight edit (the common read-only state).
fn lease_span(state: &AppState, theme: &Theme) -> Span<'static> {
    match state.doc_edit.as_ref().map(|edit| edit.lease) {
        Some(DocLeaseState::Held) => Span::styled(
            "  lease: held".to_owned(),
            Style::default().fg(theme.status.success),
        ),
        Some(DocLeaseState::Acquiring) => Span::styled(
            "  lease: acquiring…".to_owned(),
            Style::default().fg(theme.status.warning),
        ),
        Some(DocLeaseState::Blocked) => Span::styled(
            "  lease: blocked (another writer)".to_owned(),
            Style::default().fg(theme.status.error),
        ),
        None => Span::raw(""),
    }
}

fn risk_lines<'a>(risk: &'a Risk, theme: &Theme) -> Vec<Line<'a>> {
    let mut lines = vec![Line::from(vec![
        Span::styled("  level: ", Style::default().fg(theme.text.muted)),
        Span::styled(
            risk_label(risk.level).to_owned(),
            Style::default()
                .fg(risk_color(risk.level, theme))
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    for reason in &risk.reasons {
        lines.push(Line::styled(
            format!("  - {reason}"),
            Style::default().fg(theme.text.secondary),
        ));
    }
    lines
}

/// Verbatim rendering of a proposed action's fields (approval modal).
fn describe_action(action: &ProposedAction) -> Vec<String> {
    match action {
        ProposedAction::ReadFiles { paths } => {
            let mut v = vec!["read files:".to_owned()];
            v.extend(paths.iter().map(|p| format!("  {p}")));
            v
        }
        ProposedAction::WritePatch { patch } => vec![format!("apply patch: {patch}")],
        ProposedAction::ExecuteCommand {
            program,
            args,
            environment,
            cwd,
        } => {
            // Render the FULL environment and cwd: an unshown binding could
            // smuggle an execution-hijacking variable past a benign-looking
            // command line, so the approver must see every one verbatim.
            let mut v = vec![format!("command: {program} {}", args.join(" "))];
            if let Some(cwd) = cwd {
                v.push(format!("cwd: {cwd}"));
            }
            for (name, value) in environment {
                v.push(format!("env: {name}={value}"));
            }
            v
        }
        ProposedAction::NetworkRequest { destination } => {
            vec![format!("network request: {destination}")]
        }
        ProposedAction::GitCommit { repository } => vec![format!("git commit: {repository}")],
        ProposedAction::GitPush { remote, branch } => {
            vec![format!("git push: {remote} {branch}")]
        }
        // STEP 4.4.2: every publish displays target, changed files, and the
        // resulting Git action before approval — render all three verbatim
        // from the plan, exactly as computed (never re-derived here).
        ProposedAction::PublishDocument {
            target,
            changed_files,
            git_action,
            ..
        } => {
            let mut v = vec![format!("publish document: {target}")];
            v.push("changed files:".to_owned());
            v.extend(changed_files.iter().map(|f| format!("  {f}")));
            v.push(format!("git action: {git_action}"));
            v
        }
        _ => vec!["unsupported action".to_owned()],
    }
}

/// A short kind label for a proposed action (list rows).
fn action_kind(action: &ProposedAction) -> &'static str {
    match action {
        ProposedAction::ReadFiles { .. } => "read files",
        ProposedAction::WritePatch { .. } => "apply patch",
        ProposedAction::ExecuteCommand { .. } => "run command",
        ProposedAction::NetworkRequest { .. } => "network",
        ProposedAction::GitCommit { .. } => "git commit",
        ProposedAction::GitPush { .. } => "git push",
        ProposedAction::PublishDocument { .. } => "publish document",
        _ => "unsupported",
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn mode_label(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Ask => "Ask",
        AgentMode::Explore => "Explore",
        AgentMode::Plan => "Plan",
        AgentMode::Build => "Build",
        AgentMode::Review => "Review",
        _ => "Unknown",
    }
}

fn run_state_label(state: RunState) -> &'static str {
    match state {
        RunState::Queued => "Queued",
        RunState::Preparing => "Preparing",
        RunState::Running => "Running",
        RunState::WaitingForApproval => "WaitingForApproval",
        RunState::WaitingForUserInput => "WaitingForInput",
        RunState::Paused => "Paused",
        RunState::Recovering => "Recovering",
        RunState::Completed => "Completed",
        RunState::Failed => "Failed",
        RunState::Cancelled => "Cancelled",
        _ => "Unknown",
    }
}

fn run_state_dot(state: RunState) -> &'static str {
    match state {
        RunState::Completed => "✓",
        RunState::Failed => "✗",
        RunState::Cancelled => "⊘",
        RunState::WaitingForApproval | RunState::WaitingForUserInput => "◆",
        RunState::Paused => "⏸",
        _ => "●",
    }
}

fn run_state_color(state: RunState, theme: &Theme) -> Color {
    match state {
        RunState::Running | RunState::Preparing => theme.status.running,
        RunState::Completed => theme.status.success,
        RunState::Failed => theme.status.error,
        RunState::Cancelled => theme.text.muted,
        RunState::WaitingForApproval | RunState::WaitingForUserInput => theme.status.warning,
        RunState::Paused => theme.status.info,
        _ => theme.status.idle,
    }
}

fn risk_label(level: RiskLevel) -> &'static str {
    match level {
        RiskLevel::Low => "LOW",
        RiskLevel::Medium => "MED",
        RiskLevel::High => "HIGH",
        RiskLevel::Critical => "CRIT",
        _ => "????",
    }
}

fn risk_color(level: RiskLevel, theme: &Theme) -> Color {
    match level {
        RiskLevel::Low => theme.status.success,
        RiskLevel::Medium => theme.status.warning,
        RiskLevel::High | RiskLevel::Critical => theme.status.error,
        _ => theme.text.muted,
    }
}

fn budget_label(dimension: BudgetDimension) -> &'static str {
    match dimension {
        BudgetDimension::Tokens => "tokens",
        BudgetDimension::Cost => "cost",
        BudgetDimension::WallClock => "wall-clock",
        BudgetDimension::ToolCalls => "tool-calls",
        _ => "budget",
    }
}

fn format_cost(cost_minor: Option<u64>) -> String {
    match cost_minor {
        Some(c) => format!("${}.{:02}", c / 100, c % 100),
        None => "—".to_owned(),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let kept: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

fn short_id(id: &impl std::fmt::Display) -> String {
    let s = id.to_string();
    s.chars().take(8).collect()
}

/// The first non-blank line of `text`, or `""` if every line is blank — the
/// label a folded note's collapsed head shows.
fn first_non_empty_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::reduce::reduce;
    use crate::state::{MemoryCard, ModelCard, ModelLocationLabel, Pane, SkillCard};
    use chrono::Utc;
    use codypendent_protocol::{
        Actor, ApprovalId, ArtifactId, ArtifactRef, ChangeSetId, DataClassification, EventBody,
        ModelId, ProposedAction, Risk, RiskLevel, RunId, SessionEvent, ToolOutcome,
    };
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn buffer_text(buf: &Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn system_ev(body: EventBody) -> Action {
        Action::daemon_event(SessionEvent {
            sequence: 1,
            occurred_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body,
        })
    }

    fn render_to_string(state: &AppState, w: u16, h: u16) -> String {
        let theme = Theme::dark();
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| render(f, state, &theme)).expect("draw");
        buffer_text(terminal.backend().buffer())
    }

    fn running_build_state() -> AppState {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::SessionCreated {
                title: "fix-tests".to_owned(),
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "diagnose the failing test".to_owned(),
                mode: codypendent_protocol::AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunStateChanged {
                run_id,
                state: RunState::Running,
            }),
        );
        reduce(
            &mut s,
            Action::daemon_event(SessionEvent {
                sequence: 2,
                occurred_at: Utc::now(),
                causation_id: None,
                correlation_id: None,
                actor: Actor::Agent {
                    agent_id: codypendent_protocol::AgentId::new(),
                    run_id,
                    model: ModelId("gpt-5.1-codex".to_owned()),
                },
                body: EventBody::ModelStreamDelta {
                    run_id,
                    text: "Reading the test to see why it fails.".to_owned(),
                },
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ToolStarted {
                run_id,
                tool: "shell.run".to_owned(),
                args_digest: "abc123".to_owned(),
            }),
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
        reduce(
            &mut s,
            system_ev(EventBody::BudgetWarning {
                run_id,
                dimension: BudgetDimension::Tokens,
                used: 42_000,
                limit: 100_000,
            }),
        );
        s
    }

    #[test]
    fn transcript_snapshot_shows_model_tool_and_status() {
        let state = running_build_state();
        let text = render_to_string(&state, 110, 30);

        // Transcript content.
        assert!(text.contains("shell.run"), "tool card missing:\n{text}");
        assert!(
            text.contains("diagnose the failing"),
            "objective missing:\n{text}"
        );
        // Status line projections.
        assert!(text.contains("Build"), "mode missing:\n{text}");
        assert!(text.contains("Running"), "run state missing:\n{text}");
        assert!(text.contains("gpt-5.1-codex"), "model missing:\n{text}");
        assert!(text.contains("42%"), "context %% missing:\n{text}");
        assert!(
            text.contains("approvals"),
            "approval count missing:\n{text}"
        );
    }

    /// Task 5 (codex chat shell): the collapsed tool card head restyles into
    /// one compact Codex-style line — a run glyph (`⏺`) and the tool's
    /// verb/name, with a terse outcome mark instead of the old `[status]`
    /// bracket.
    #[test]
    fn a_completed_tool_card_renders_compact_with_a_run_glyph_and_check() {
        let state = running_build_state();
        let text = render_to_string(&state, 110, 30);
        assert!(
            text.contains("⏺ shell.run"),
            "run glyph + name missing:\n{text}"
        );
        assert!(text.contains('✓'), "success outcome mark missing:\n{text}");
        assert!(
            !text.contains("[done]"),
            "old bracket style must be gone:\n{text}"
        );
    }

    /// A tool still awaiting a decision (`ToolStatus::Proposed`) shows the
    /// same `⟳ review` marker a patch does, instead of the old `[proposed]`.
    #[test]
    fn a_proposed_tool_card_shows_a_review_marker() {
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
            system_ev(EventBody::ToolProposed {
                run_id,
                approval_id: ApprovalId::new(),
                action: ProposedAction::ExecuteCommand {
                    program: "cargo".to_owned(),
                    args: vec!["test".to_owned()],
                    environment: Vec::new(),
                    cwd: None,
                },
            }),
        );
        let out = render_to_string(&s, 80, 20);
        assert!(out.contains("⟳ review"), "review marker missing:\n{out}");
        assert!(
            !out.contains("[proposed]"),
            "old bracket style must be gone:\n{out}"
        );
    }

    /// A failed tool card shows a terse `✗` in the collapsed head; the
    /// failure message itself stays in the expanded detail (unchanged).
    #[test]
    fn a_failed_tool_card_shows_a_cross_mark() {
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
                args_digest: "d".to_owned(),
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ToolCompleted {
                run_id,
                tool: "shell.run".to_owned(),
                outcome: ToolOutcome::Failed {
                    message: "exit 1".to_owned(),
                },
                artifact: None,
            }),
        );
        let out = render_to_string(&s, 80, 20);
        assert!(out.contains('✗'), "failure outcome mark missing:\n{out}");
        assert!(
            !out.contains("[failed]"),
            "old bracket style must be gone:\n{out}"
        );
    }

    /// Task 5: a patch card's collapsed head is `❖ patch {short id}` plus a
    /// `⟳ review` marker. The protocol has no `PatchApplied`/`PatchRejected`
    /// event — a `PatchProposed` change set never resolves on the wire, so
    /// every patch card sits in the transcript for manual review for its
    /// entire lifetime; the marker is unconditional rather than derived from
    /// a per-instance status field (`PatchSummary` carries none).
    #[test]
    fn a_patch_card_renders_compact_with_a_patch_glyph_and_review_marker() {
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
                artifact: filler_chronicle(),
            }),
        );
        let out = render_to_string(&s, 80, 20);
        assert!(out.contains("❖ patch"), "patch glyph missing:\n{out}");
        assert!(out.contains("⟳ review"), "review marker missing:\n{out}");
        assert!(
            !out.contains("patch proposed ("),
            "old verbose label must be gone:\n{out}"
        );
    }

    /// Task 3: a run that is `Preparing`/`Running` with no model text
    /// streaming yet shows a dim "working…" row, so it never looks silently
    /// paused between transcript updates.
    #[test]
    fn a_thinking_run_shows_a_working_status_row() {
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
        let out = render_to_string(&s, 80, 20);
        assert!(out.contains("working…"), "status row missing:\n{out}");
    }

    /// Task 3: while a tool is executing, the status row names it instead of
    /// the generic "working…" — e.g. "running shell.run…".
    #[test]
    fn a_running_tool_status_row_names_the_tool() {
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
        reduce(
            &mut s,
            system_ev(EventBody::ToolStarted {
                run_id,
                tool: "shell.run".to_owned(),
                args_digest: "abc".to_owned(),
            }),
        );
        let out = render_to_string(&s, 80, 20);
        assert!(
            out.contains("running shell.run…"),
            "tool status row missing:\n{out}"
        );
    }

    /// Task 3: a fresh run (no `RunStateChanged` yet) is `Idle`, and `Idle`
    /// renders no status row at all — the row must not appear by default.
    #[test]
    fn an_idle_run_shows_no_status_row() {
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
        let out = render_to_string(&s, 80, 20);
        assert!(!out.contains("working…"), "unexpected status row:\n{out}");
        assert!(!out.contains("running "), "unexpected status row:\n{out}");
    }

    /// Task 4: while a run's activity is `Streaming`, the model cell shows a
    /// muted `▋` caret right after the accumulated text, so the mid-stream
    /// cell reads as still-writing rather than silently paused; the caret is
    /// derived render state, never stored, so it drops the instant the run
    /// completes.
    #[test]
    fn a_streaming_cell_shows_a_caret_then_drops_it_on_completion() {
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
        reduce(
            &mut s,
            system_ev(EventBody::ModelStreamDelta {
                run_id,
                text: "partial".to_owned(),
            }),
        );
        let mid = render_to_string(&s, 80, 20);
        assert!(mid.contains("partial"), "streamed text missing:\n{mid}");
        assert!(mid.contains('▋'), "streaming cell shows a caret:\n{mid}");

        reduce(
            &mut s,
            system_ev(EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Completed {
                    summary: Some("partial".to_owned()),
                },
                chronicle: ArtifactRef {
                    id: ArtifactId::new(),
                    media_type: "application/json".to_owned(),
                    byte_length: 10,
                    sha256: "0".repeat(64),
                    sensitivity: DataClassification::Internal,
                },
            }),
        );
        let done = render_to_string(&s, 80, 20);
        assert!(
            !done.contains('▋'),
            "caret is gone once the run completes:\n{done}"
        );
    }

    /// A completed helper: `RunCompleted` needs a `chronicle` artifact
    /// reference alongside the disposition; every disposition test below
    /// uses this filler ref, only the disposition itself is under test.
    fn filler_chronicle() -> ArtifactRef {
        ArtifactRef {
            id: ArtifactId::new(),
            media_type: "application/json".to_owned(),
            byte_length: 10,
            sha256: "0".repeat(64),
            sensitivity: DataClassification::Internal,
        }
    }

    /// Task 3 (codex chat shell): a successful run's reply already ended the
    /// turn as streamed model prose, so the `Completed` cell must render
    /// nothing — no second/third echo of the same reply — and the turn's
    /// first agent cell gets a `⏺ codypendent` header.
    #[test]
    fn a_completed_success_shows_the_reply_once_no_echo() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "hi".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ModelStreamDelta {
                run_id,
                text: "hello there".to_owned(),
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Completed {
                    summary: Some("hello there".to_owned()),
                },
                chronicle: filler_chronicle(),
            }),
        );
        let out = render_to_string(&s, 80, 20);
        assert_eq!(
            out.matches("hello there").count(),
            1,
            "reply appears exactly once, not echoed by Completed:\n{out}"
        );
        assert!(!out.contains("run completed"), "no completed echo:\n{out}");
        assert!(
            out.contains("⏺ codypendent"),
            "assistant header shown before the reply:\n{out}"
        );
    }

    /// A run that fails before producing any prose has no `Model`/`Tool`/
    /// `Patch` cell — so it must not render a lone `⏺ codypendent` header
    /// with nothing under it. It shows its failure reason, tersely (no
    /// leftover "run failed:" verbiage from the old always-visible echo).
    #[test]
    fn a_failed_run_shows_its_reason() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "hi".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Failed {
                    reason: "no model configured".to_owned(),
                },
                chronicle: filler_chronicle(),
            }),
        );
        let out = render_to_string(&s, 80, 20);
        assert!(
            out.contains("no model configured"),
            "failure reason shown:\n{out}"
        );
        assert!(
            !out.contains("run failed:"),
            "terse reason, not the old verbose echo:\n{out}"
        );
        assert!(
            !out.contains("⏺ codypendent"),
            "no agent cell ever ran, so no lone header:\n{out}"
        );
    }

    /// `RunDisposition::Cancelled` carries an optional reason (unlike the
    /// unit-variant the design sketch assumed) — it renders tersely too, and
    /// still surfaces the reason when one is given.
    #[test]
    fn a_cancelled_run_shows_its_reason_tersely() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "hi".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunCompleted {
                run_id,
                disposition: RunDisposition::Cancelled {
                    reason: Some("budget exceeded".to_owned()),
                },
                chronicle: filler_chronicle(),
            }),
        );
        let out = render_to_string(&s, 80, 20);
        assert!(out.contains("cancelled"), "cancellation shown:\n{out}");
        assert!(out.contains("budget exceeded"), "reason shown:\n{out}");
        assert!(
            !out.contains("run cancelled:"),
            "terse form, not the old verbose echo:\n{out}"
        );
    }

    /// The assistant header announces only the first agent cell of a turn —
    /// a tool call followed by more model text in the same turn must not
    /// repeat it, and a `Tool` cell (not just `Model`) triggers it.
    #[test]
    fn the_assistant_header_appears_once_per_turn_even_with_multiple_agent_cells() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "hi".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ToolStarted {
                run_id,
                tool: "shell.run".to_owned(),
                args_digest: "abc123".to_owned(),
            }),
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
        reduce(
            &mut s,
            system_ev(EventBody::ModelStreamDelta {
                run_id,
                text: "done".to_owned(),
            }),
        );
        let out = render_to_string(&s, 80, 20);
        assert_eq!(
            out.matches("⏺ codypendent").count(),
            1,
            "header appears exactly once for the whole turn:\n{out}"
        );
    }

    /// Task 4 (codex chat shell): the conversation header names the serving
    /// model and the run's mode, joined `model · mode`, so the operator sees
    /// both without opening the run-detail pane. `running_build_state` learns
    /// "gpt-5.1-codex" from the agent actor but never fires a cost budget
    /// event, so cost stays unknown — it must be left out entirely rather
    /// than shown as a `$0.00`/`—` placeholder.
    #[test]
    fn the_conversation_header_shows_model_and_mode() {
        let state = running_build_state();
        let text = render_to_string(&state, 100, 30);
        assert!(
            text.contains("gpt-5.1-codex · Build"),
            "model · mode header:\n{text}"
        );
        assert!(
            !text.contains('$'),
            "unknown cost omitted, not a placeholder:\n{text}"
        );
    }

    /// A fresh run has a mode but no model learned yet — the header shows
    /// the mode alone (joined onto the title by one separator), with no
    /// extra slot or separator standing in for the still-unknown model/cost.
    #[test]
    fn the_header_shows_mode_alone_before_a_model_is_learned() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "o".to_owned(),
                mode: AgentMode::Ask,
            }),
        );
        let out = render_to_string(&s, 100, 20);
        let header_row = out.lines().next().expect("a top row");
        assert!(
            header_row.contains("· Ask"),
            "mode shown in the header:\n{header_row}"
        );
        assert_eq!(
            header_row.matches('·').count(),
            1,
            "one separator for the one known field — no slot for model/cost:\n{header_row}"
        );
    }

    /// Task 4 (codex chat shell): a blank line separates turns after the
    /// first so the conversation breathes, instead of reading as one
    /// undifferentiated scroll. The reducer doesn't yet drive a second
    /// `User` turn onto a live run (steering acks as `Steering`, not
    /// `User` — see `TranscriptEntry::User`'s doc comment), so this pushes
    /// the follow-up turn directly to exercise the render-side spacing rule
    /// in isolation from that reducer wiring.
    #[test]
    fn a_blank_line_separates_turns_after_the_first() {
        let mut s = AppState::new();
        let run_id = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id,
                objective: "alpha".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        s.runs[0].transcript.push(TranscriptEntry::User {
            text: "beta".to_owned(),
        });
        let out = render_to_string(&s, 80, 20);
        let rows: Vec<&str> = out.lines().collect();
        // Search for the turn marker itself (not just the bare word), so a
        // match can't land on the pane title — which also shows the
        // objective ("alpha") and would otherwise be mistaken for the
        // transcript row.
        let alpha_row = rows
            .iter()
            .position(|r| r.contains("› alpha"))
            .expect("first turn rendered");
        let beta_row = rows
            .iter()
            .position(|r| r.contains("› beta"))
            .expect("second turn rendered");
        assert_eq!(
            beta_row,
            alpha_row + 2,
            "exactly one blank row separates the turns:\n{out}"
        );
    }

    /// Task 5 (continuous-session plan): the bug this task fixes — each
    /// message spawned a new run, and the conversation showed only the
    /// selected run, so the previous turn disappeared the moment a new one
    /// started. `render_conversation` must now walk every run in the session,
    /// in order, as one continuous scroll.
    #[test]
    fn the_conversation_renders_every_run_in_one_continuous_scroll() {
        let mut s = AppState::new();
        let run1 = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id: run1,
                objective: "alpha".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ModelStreamDelta {
                run_id: run1,
                text: "alpha reply".to_owned(),
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::RunCompleted {
                run_id: run1,
                disposition: RunDisposition::Completed {
                    summary: Some("alpha reply".to_owned()),
                },
                chronicle: filler_chronicle(),
            }),
        );

        // A follow-up: a second run in the SAME session — the bug made the
        // first turn vanish the instant this one started.
        let run2 = RunId::new();
        reduce(
            &mut s,
            system_ev(EventBody::RunStarted {
                run_id: run2,
                objective: "beta".to_owned(),
                mode: AgentMode::Build,
            }),
        );
        reduce(
            &mut s,
            system_ev(EventBody::ModelStreamDelta {
                run_id: run2,
                text: "beta reply".to_owned(),
            }),
        );

        let out = render_to_string(&s, 100, 30);
        assert!(
            out.contains("› alpha") && out.contains("alpha reply"),
            "the first (completed) run's turn must still be visible:\n{out}"
        );
        assert!(
            out.contains("› beta") && out.contains("beta reply"),
            "the second (live) run's turn must also be visible:\n{out}"
        );
        assert_eq!(
            out.matches("⏺ codypendent").count(),
            2,
            "each turn gets its own assistant header:\n{out}"
        );
        assert!(
            out.contains("2 turns"),
            "the header names the turn count (the old [n/n] run-selector \
             counter no longer applies once every run always renders):\n{out}"
        );
    }

    #[test]
    fn approval_modal_snapshot_shows_action_risk_and_capabilities() {
        let mut state = running_build_state();
        reduce(
            &mut state,
            system_ev(EventBody::ApprovalRequested {
                approval_id: ApprovalId::new(),
                action: ProposedAction::ExecuteCommand {
                    program: "cargo".to_owned(),
                    args: vec!["test".to_owned(), "--all".to_owned()],
                    environment: Vec::new(),
                    cwd: None,
                },
                risk: Risk {
                    level: RiskLevel::High,
                    reasons: vec!["runs an arbitrary command".to_owned()],
                },
            }),
        );
        assert!(state.show_approval_modal());
        let text = render_to_string(&state, 110, 34);

        assert!(text.contains("Approval required"), "title missing:\n{text}");
        // Action verbatim.
        assert!(
            text.contains("cargo test --all"),
            "verbatim command missing:\n{text}"
        );
        // Risk verbatim.
        assert!(text.contains("HIGH"), "risk level missing:\n{text}");
        assert!(
            text.contains("runs an arbitrary command"),
            "risk reason missing:\n{text}"
        );
        // Requested capabilities (derived label).
        assert!(
            text.contains("CommandExecute"),
            "capability missing:\n{text}"
        );
        // Decision keys present.
        assert!(text.contains("approve once"), "keys missing:\n{text}");
    }

    #[test]
    fn approval_modal_snapshot_shows_publish_document_plan_verbatim() {
        // STEP 4.4.2: every publish displays target, changed files, and the
        // resulting Git action before approval — the generic approval card
        // (not a bespoke docs-publish UI) must render a `PublishDocument`
        // proposal's plan content verbatim, exactly as it renders any other
        // action.
        let mut state = running_build_state();
        reduce(
            &mut state,
            system_ev(EventBody::ApprovalRequested {
                approval_id: ApprovalId::new(),
                action: ProposedAction::PublishDocument {
                    document_id: codypendent_protocol::DocumentId::new(),
                    target: "repository file docs/architecture.md".to_owned(),
                    changed_files: vec!["docs/architecture.md".to_owned()],
                    git_action: "write docs/architecture.md in the working tree \
                                 (approval-gated change set)"
                        .to_owned(),
                },
                risk: Risk {
                    level: RiskLevel::Medium,
                    reasons: vec!["writes docs/architecture.md and commits it".to_owned()],
                },
            }),
        );
        assert!(state.show_approval_modal());
        let text = render_to_string(&state, 110, 34);

        assert!(text.contains("Approval required"), "title missing:\n{text}");
        assert!(
            text.contains("repository file docs/architecture.md"),
            "target missing verbatim:\n{text}"
        );
        assert!(
            text.contains("docs/architecture.md"),
            "changed file missing verbatim:\n{text}"
        );
        assert!(
            text.contains("write docs/architecture.md in the working tree"),
            "git action missing verbatim:\n{text}"
        );
        assert!(text.contains("MED"), "risk level missing:\n{text}");
        assert!(
            text.contains("GitCommit (repository file docs/architecture.md)"),
            "capability label missing:\n{text}"
        );
    }

    #[test]
    fn help_overlay_lists_bindings() {
        let mut state = running_build_state();
        reduce(&mut state, Action::Help);
        let text = render_to_string(&state, 110, 34);
        assert!(text.contains("Help"));
        assert!(text.contains("command palette"));
        assert!(text.contains("detach"));
    }

    #[test]
    fn expanded_tool_card_shows_detail() {
        let mut state = running_build_state();
        let art = ArtifactRef {
            id: ArtifactId::new(),
            media_type: "text/plain".to_owned(),
            byte_length: 2048,
            sha256: "0".repeat(64),
            sensitivity: DataClassification::Internal,
        };
        let run_id = state.runs[0].run_id;
        reduce(
            &mut state,
            system_ev(EventBody::ToolProposed {
                run_id,
                approval_id: ApprovalId::new(),
                action: ProposedAction::ReadFiles {
                    paths: vec!["src/lib.rs".to_owned()],
                },
            }),
        );
        // Complete it with an artifact, then expand the selected entry.
        reduce(
            &mut state,
            system_ev(EventBody::ToolCompleted {
                run_id,
                tool: "workspace.read_file".to_owned(),
                outcome: ToolOutcome::Succeeded,
                artifact: Some(art),
            }),
        );
        state.focus = Pane::Transcript;
        let last = state.runs[0].transcript.len() - 1;
        state.runs[0].transcript_selected = last;
        reduce(&mut state, Action::Expand);

        let text = render_to_string(&state, 110, 34);
        assert!(text.contains("workspace.read_file"), "tool name:\n{text}");
        assert!(text.contains("2048 bytes"), "artifact detail:\n{text}");
    }

    #[test]
    fn long_note_folds_by_default_and_expand_reveals_the_body() {
        let mut state = running_build_state();
        let run_id = state.runs[0].run_id;
        let note = "first line of the note\nsecond line\nthird line\nfourth line".to_owned();
        reduce(
            &mut state,
            system_ev(EventBody::NoteAppended {
                text: note,
                run_id: Some(run_id),
            }),
        );
        state.focus = Pane::Transcript;
        let last = state.runs[0].transcript.len() - 1;
        state.runs[0].transcript_selected = last;

        let collapsed = render_to_string(&state, 110, 34);
        assert!(
            collapsed.contains("▸ note: first line of the note (4 lines)"),
            "collapsed head:\n{collapsed}"
        );
        assert!(
            !collapsed.contains("fourth line"),
            "the full body must not show while collapsed:\n{collapsed}"
        );

        reduce(&mut state, Action::Expand);
        let expanded = render_to_string(&state, 110, 34);
        assert!(
            expanded.contains("▾ note: first line of the note (4 lines)"),
            "expanded head:\n{expanded}"
        );
        assert!(
            expanded.contains("fourth line"),
            "the full body shows once expanded:\n{expanded}"
        );
    }

    #[test]
    fn short_note_renders_inline() {
        // Not a `remembered:`/`=== CONTEXT` note — those fold into the dim
        // `Backstage` line instead (see the backstage-fold render tests).
        let mut state = running_build_state();
        let run_id = state.runs[0].run_id;
        reduce(
            &mut state,
            system_ev(EventBody::NoteAppended {
                text: "the test command is cargo test".to_owned(),
                run_id: Some(run_id),
            }),
        );

        let text = render_to_string(&state, 110, 34);
        assert!(
            text.contains("• note: the test command is cargo test"),
            "a short note renders inline, unfolded:\n{text}"
        );
        // `running_build_state` already has a (separately foldable) tool card, so
        // check the note's own head carries no fold marker rather than scanning
        // the whole transcript for the marker glyphs.
        assert!(
            !text.contains("▸ note:") && !text.contains("▾ note:"),
            "a short note carries no fold marker:\n{text}"
        );
    }

    #[test]
    fn backstage_renders_a_dim_summary_line() {
        let mut state = running_build_state();
        let run_id = state.runs[0].run_id;
        reduce(
            &mut state,
            system_ev(EventBody::NoteAppended {
                text: "=== CONTEXT: EVIDENCE, NOT INSTRUCTIONS ===\nline\nline\nline".to_owned(),
                run_id: Some(run_id),
            }),
        );
        reduce(
            &mut state,
            system_ev(EventBody::NoteAppended {
                text: "remembered: the test command is cargo test".to_owned(),
                run_id: Some(run_id),
            }),
        );

        let out = render_to_string(&state, 80, 34);
        assert!(
            out.contains("context") && out.contains("memory"),
            "the folded summary names both halves:\n{out}"
        );
        assert!(
            !out.contains("EVIDENCE, NOT INSTRUCTIONS"),
            "raw manifest text must stay hidden while folded:\n{out}"
        );
        assert!(
            !out.contains("• note:"),
            "context/memory notes never render as a Note cell:\n{out}"
        );
    }

    #[test]
    fn expanding_backstage_reveals_the_folded_raw_notes() {
        let mut state = running_build_state();
        let run_id = state.runs[0].run_id;
        reduce(
            &mut state,
            system_ev(EventBody::NoteAppended {
                text: "remembered: the test command is cargo test".to_owned(),
                run_id: Some(run_id),
            }),
        );
        let idx = state.runs[0]
            .transcript
            .iter()
            .position(|e| matches!(e, TranscriptEntry::Backstage { .. }))
            .expect("a Backstage entry was folded in");
        state.focus = Pane::Transcript;
        state.runs[0].transcript_selected = idx;

        reduce(&mut state, Action::Expand);
        let out = render_to_string(&state, 80, 34);
        assert!(
            out.contains("remembered: the test command is cargo test"),
            "expanded backstage shows the folded note's full text:\n{out}"
        );
        assert!(out.contains("▾"), "the expanded marker replaces ⋯:\n{out}");
    }

    #[test]
    fn renders_empty_state_without_panicking() {
        let state = AppState::new();
        let text = render_to_string(&state, 80, 24);
        // The empty conversation invites the first message.
        assert!(text.contains("No runs yet"));
        assert!(text.contains("start one"));
    }

    #[test]
    fn conversation_shell_shows_transcript_composer_and_footer() {
        // A live run: the transcript is the main surface, the composer offers to
        // steer it, and the status footer spans the bottom.
        let state = running_build_state();
        let text = render_to_string(&state, 100, 30);

        // Conversation title names the session + active run objective.
        assert!(text.contains("fix-tests"), "session in title:\n{text}");
        assert!(
            text.contains("diagnose the failing test"),
            "run objective:\n{text}"
        );
        // The persistent composer + its steering placeholder (the run is live).
        assert!(text.contains("›"), "composer prompt:\n{text}");
        assert!(text.contains("Enter sends"), "composer hint:\n{text}");
        assert!(text.contains("steer the run"), "steer placeholder:\n{text}");
        // The status footer is still present.
        assert!(text.contains("mode"), "status footer:\n{text}");
    }

    #[test]
    fn a_user_turn_renders_with_a_caret_marker() {
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
        let out = render_to_string(&s, 80, 12);
        assert!(out.contains("› add a test") || out.contains("> add a test"));
    }

    #[test]
    fn composer_shows_a_typed_draft() {
        let mut state = running_build_state();
        for c in "add a boundary check".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        let text = render_to_string(&state, 100, 30);
        assert!(
            text.contains("add a boundary check"),
            "draft not shown:\n{text}"
        );
    }

    #[test]
    fn workspace_layout_adds_runs_and_approvals_panes() {
        // Toggling to the workspace layout flanks the conversation with a runs
        // pane and an approvals + detail pane — the composer/footer are unchanged.
        let mut state = running_build_state();
        reduce(&mut state, Action::ToggleLayout);
        let text = render_to_string(&state, 120, 30);

        assert!(text.contains("Runs"), "runs pane missing:\n{text}");
        assert!(
            text.contains("Approvals"),
            "approvals pane missing:\n{text}"
        );
        // The conversation is still the centre surface.
        assert!(text.contains("fix-tests"), "conversation title:\n{text}");
        // The composer and status footer persist across the toggle.
        assert!(text.contains("›"), "composer:\n{text}");
        assert!(text.contains("mode"), "status footer:\n{text}");

        // Toggling back returns to the single-column chat (no Runs pane title).
        reduce(&mut state, Action::ToggleLayout);
        let chat = render_to_string(&state, 120, 30);
        assert!(!chat.contains("Runs ("), "should be single-column:\n{chat}");
    }

    #[test]
    fn contextual_footer_switches_hint_by_context() {
        // Idle: full ambient fields + a command hint.
        let mut state = running_build_state();
        let idle = render_to_string(&state, 120, 30);
        assert!(idle.contains("mode"), "ambient fields:\n{idle}");
        assert!(idle.contains("model"), "model field at full width:\n{idle}");
        assert!(
            idle.contains("cmds") || idle.contains("F2"),
            "command hint:\n{idle}"
        );

        // Drafting: the hint invites sending.
        for c in "hello".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        let drafting = render_to_string(&state, 120, 30);
        assert!(
            drafting.contains("send"),
            "send hint while drafting:\n{drafting}"
        );
    }

    #[test]
    fn contextual_footer_narrows_by_dropping_low_priority_fields() {
        let state = running_build_state();
        let narrow = render_to_string(&state, 50, 30);
        // State survives; the model field is dropped at a narrow width.
        assert!(narrow.contains("state"), "state kept:\n{narrow}");
        assert!(
            !narrow.contains("model"),
            "model dropped when narrow:\n{narrow}"
        );
    }

    #[test]
    fn skill_studio_snapshot_shows_permissions_verbatim() {
        let mut state = running_build_state();
        state.skills = vec![SkillCard {
            name: "rust.fix-ci".to_owned(),
            kind: "skill".to_owned(),
            scope: "repository".to_owned(),
            trust: "first-party".to_owned(),
            status: "active".to_owned(),
            risk: "medium".to_owned(),
            description: "diagnose and fix a failing CI run".to_owned(),
            permissions: vec![
                "filesystem_read: $REPOSITORY".to_owned(),
                "command: cargo".to_owned(),
            ],
        }];
        reduce(&mut state, Action::OpenSkills);
        let text = render_to_string(&state, 120, 40);

        assert!(text.contains("Skill Studio"), "title missing:\n{text}");
        assert!(text.contains("rust.fix-ci"), "skill name missing:\n{text}");
        assert!(text.contains("Permissions"), "section missing:\n{text}");
        // The exit criterion: requested capabilities render verbatim.
        assert!(
            text.contains("filesystem_read: $REPOSITORY"),
            "verbatim fs permission missing:\n{text}"
        );
        assert!(
            text.contains("command: cargo"),
            "verbatim command permission missing:\n{text}"
        );
    }

    #[test]
    fn memory_browser_snapshot_shows_the_provenance_card() {
        let mut state = running_build_state();
        state.memories = vec![MemoryCard {
            statement: "This repository requires Rust nightly".to_owned(),
            class: "semantic".to_owned(),
            scope: "repository".to_owned(),
            revision: "79acbf1".to_owned(),
            observed: "2026-07-14".to_owned(),
            confidence: 1.0,
            source: "artifact 3f2a (rust-toolchain.toml)".to_owned(),
        }];
        reduce(&mut state, Action::OpenMemory);
        let text = render_to_string(&state, 120, 40);

        assert!(
            text.contains("Provenance card"),
            "card title missing:\n{text}"
        );
        assert!(
            text.contains("This repository requires Rust nightly"),
            "fact missing:\n{text}"
        );
        // Every retrieved memory opens its source: the source is on the card.
        assert!(
            text.contains("rust-toolchain.toml"),
            "source missing:\n{text}"
        );
        assert!(text.contains("79acbf1"), "revision missing:\n{text}");
        assert!(text.contains("Confidence"), "confidence missing:\n{text}");
        // Before opening, the affordance is offered.
        assert!(text.contains("open source"), "affordance missing:\n{text}");
    }

    #[test]
    fn memory_browser_open_source_reveals_the_full_ref() {
        let mut state = running_build_state();
        state.memories = vec![MemoryCard {
            statement: "tests use cargo nextest".to_owned(),
            class: "procedural".to_owned(),
            scope: "repository".to_owned(),
            revision: "abc1234".to_owned(),
            observed: "2026-07-15".to_owned(),
            confidence: 0.9,
            source: "events 3..7 of session 51ee".to_owned(),
        }];
        reduce(&mut state, Action::OpenMemory);
        reduce(&mut state, Action::OpenSource);
        let text = render_to_string(&state, 120, 40);

        assert!(
            text.contains("source opened"),
            "opened marker missing:\n{text}"
        );
        assert!(
            text.contains("events 3..7 of session 51ee"),
            "revealed source missing:\n{text}"
        );
    }

    #[test]
    fn docs_studio_snapshot_shows_tree_editor_and_review_rails() {
        use crate::state::{DocBlockView, DocCard, DocSuggestionView};
        let mut state = running_build_state();
        state.docs = vec![DocCard {
            document_id: codypendent_protocol::DocumentId::new(),
            title: "Payments runbook".to_owned(),
            scope: "organization".to_owned(),
            status: "draft".to_owned(),
            mode: "suggest".to_owned(),
            revision: "r7".to_owned(),
            blocks: vec![
                DocBlockView {
                    id: "b1".to_owned(),
                    kind: "heading".to_owned(),
                    text: "Charging a customer".to_owned(),
                },
                DocBlockView {
                    id: "b2".to_owned(),
                    kind: "paragraph".to_owned(),
                    text: "Call charge_customer with an idempotency key.".to_owned(),
                },
            ],
            suggestions: vec![DocSuggestionView {
                id: "s1".to_owned(),
                status: "pending".to_owned(),
                author: "agent".to_owned(),
                range: "0..8".to_owned(),
                replacement: "Charging a customer safely".to_owned(),
                rationale: Some("match the code path".to_owned()),
            }],
        }];
        reduce(&mut state, Action::OpenDocs);
        let text = render_to_string(&state, 120, 40);

        assert!(text.contains("Docs Studio"), "title missing:\n{text}");
        // Tree rail: the document title + its scope/status/mode.
        assert!(
            text.contains("Payments runbook"),
            "tree title missing:\n{text}"
        );
        assert!(text.contains("organization"), "tree scope missing:\n{text}");
        // Editor rail: block kinds and the revision badge.
        assert!(text.contains("Editor rail"), "editor rail missing:\n{text}");
        assert!(text.contains("heading"), "block kind missing:\n{text}");
        assert!(text.contains("r7"), "revision badge missing:\n{text}");
        // Review rail: the pending suggestion with its author and rationale.
        assert!(text.contains("Review rail"), "review rail missing:\n{text}");
        assert!(text.contains("agent"), "suggestion author missing:\n{text}");
        assert!(
            text.contains("match the code path"),
            "suggestion rationale missing:\n{text}"
        );
    }

    #[test]
    fn command_palette_snapshot_lists_and_filters_commands() {
        let mut state = running_build_state();
        reduce(&mut state, Action::OpenPalette);
        let all = render_to_string(&state, 120, 40);
        assert!(all.contains("Command palette"), "title missing:\n{all}");
        // Unfiltered, it lists commands with their key hints.
        assert!(all.contains("New run"), "command missing:\n{all}");
        assert!(all.contains("Docs Studio"), "command missing:\n{all}");
        assert!(all.contains("[n]"), "key hint missing:\n{all}");

        // Typing filters the list down.
        for c in "docs".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        let filtered = render_to_string(&state, 120, 40);
        assert!(
            filtered.contains("Docs Studio"),
            "match missing:\n{filtered}"
        );
        assert!(
            !filtered.contains("New run"),
            "non-match should be filtered out:\n{filtered}"
        );
    }

    #[test]
    fn model_picker_snapshot_shows_rows_current_marker_and_badges() {
        let mut state = running_build_state();
        // `running_build_state` serves the run from "gpt-5.1-codex" (its
        // ModelStreamDelta actor) — that row must render marked current.
        state.models = vec![
            ModelCard {
                id: ModelId("gpt-5.1-codex".to_owned()),
                provider: "openai-compatible".to_owned(),
                location: Some(ModelLocationLabel::Hosted),
                cost_per_1k_usd: Some(0.03),
                context_tokens: Some(200_000),
            },
            ModelCard {
                id: ModelId("qwen2.5-coder".to_owned()),
                provider: "openai-compatible".to_owned(),
                location: Some(ModelLocationLabel::Local),
                cost_per_1k_usd: None,
                context_tokens: Some(32_000),
            },
        ];
        reduce(&mut state, Action::OpenPalette);
        for c in "model".chars() {
            reduce(&mut state, Action::InputChar(c));
        }
        reduce(&mut state, Action::InputSubmit);
        // Focus the SECOND row (qwen) — deliberately NOT the current model
        // (gpt) — so the current-marker assertions below can only be
        // satisfied by the list rows themselves, never by the (qwen-focused)
        // detail panel.
        reduce(&mut state, Action::SelectNext);
        assert!(matches!(state.overlay, Overlay::ModelPicker { .. }));

        let text = render_to_string(&state, 120, 40);
        assert!(text.contains("Model picker"), "title missing:\n{text}");
        assert!(text.contains("gpt-5.1-codex"), "first row missing:\n{text}");
        assert!(
            text.contains("qwen2.5-coder"),
            "second row missing:\n{text}"
        );

        // Row-scoped: the list's per-row current marker is the span
        // immediately BEFORE the id ("● " then the id, contiguous — see the
        // list-row `head` `Line`), distinct from the detail panel's "<id>  ●
        // current" (marker AFTER the id) when the FOCUSED model happens to be
        // current. Checking this precise adjacency — rather than whether a
        // whole terminal LINE contains '●' — matters because ratatui lays
        // the list and detail panel out as side-by-side columns sharing the
        // same rows, so an unscoped whole-line check would also pass with the
        // marker misapplied to the wrong row (or every row): here gpt is
        // current and qwen is merely focused (by the `SelectNext` above), so
        // only gpt's list row may show the leading marker.
        assert!(
            text.contains("● gpt-5.1-codex"),
            "the list's current marker is missing from gpt-5.1-codex's row:\n{text}"
        );
        assert!(
            !text.contains("● qwen2.5-coder"),
            "the list must not mark the non-current model's row current:\n{text}"
        );

        // Local/hosted + cost + context badges.
        assert!(text.contains("hosted"), "hosted badge missing:\n{text}");
        assert!(
            text.contains("local \u{2713}"),
            "local badge missing:\n{text}"
        );
        assert!(text.contains("$0.03/1k"), "cost badge missing:\n{text}");
        assert!(text.contains("200k"), "context badge missing:\n{text}");
        // The unprofiled model's badges are omitted gracefully, never crash.
        assert!(
            text.contains("32k"),
            "the profiled model's context missing:\n{text}"
        );
    }

    #[test]
    fn edge_inspector_snapshot_shows_evidence_and_revision() {
        use crate::state::GraphEdgeCard;
        let mut state = running_build_state();
        state.edges = vec![GraphEdgeCard {
            from: "billing::charge".to_owned(),
            to: "gateway::submit".to_owned(),
            relation: "calls".to_owned(),
            confidence: 0.45,
            evidence_kind: "syntax_inferred".to_owned(),
            evidence: "artifact 3f2a (src/billing.rs)".to_owned(),
            revision: "79acbf1".to_owned(),
        }];
        reduce(&mut state, Action::OpenEdges);
        let text = render_to_string(&state, 120, 40);

        assert!(text.contains("Code-graph edges"), "title missing:\n{text}");
        assert!(
            text.contains("billing::charge"),
            "from symbol missing:\n{text}"
        );
        assert!(
            text.contains("gateway::submit"),
            "to symbol missing:\n{text}"
        );
        assert!(text.contains("calls"), "relation missing:\n{text}");
        // The exit-criterion payload: evidence kind + source + revision on show.
        assert!(
            text.contains("Evidence"),
            "evidence section missing:\n{text}"
        );
        assert!(
            text.contains("syntax_inferred"),
            "evidence kind missing:\n{text}"
        );
        assert!(
            text.contains("src/billing.rs"),
            "evidence source missing:\n{text}"
        );
        assert!(text.contains("79acbf1"), "revision missing:\n{text}");
    }

    #[test]
    fn workflow_view_snapshot_shows_node_state_agent_and_worktree() {
        use crate::state::WorkflowNodeCard;
        let mut state = running_build_state();
        state.workflow = vec![
            WorkflowNodeCard {
                workflow: "repair-github-check v1".to_owned(),
                id: "patch".to_owned(),
                action: "agent implementer \u{b7} skill code.repair".to_owned(),
                kind: "agent".to_owned(),
                state: "pending".to_owned(),
                agent: "implementer".to_owned(),
                model_policy: "coding".to_owned(),
                workspace: "isolated worktree".to_owned(),
                approval: "before write".to_owned(),
                retry: "1 attempt".to_owned(),
                depends_on: "\u{2014}".to_owned(),
                outputs: "proposed_patch".to_owned(),
                cost: "\u{2014}".to_owned(),
                error: "\u{2014}".to_owned(),
            },
            WorkflowNodeCard {
                workflow: "repair-github-check v1".to_owned(),
                id: "verify".to_owned(),
                action: "tool repository.test".to_owned(),
                kind: "tool".to_owned(),
                state: "pending".to_owned(),
                agent: "\u{2014}".to_owned(),
                model_policy: "\u{2014}".to_owned(),
                workspace: "shared worktree".to_owned(),
                approval: "none".to_owned(),
                retry: "2 attempts \u{b7} 5s backoff".to_owned(),
                depends_on: "patch".to_owned(),
                outputs: "test_result".to_owned(),
                cost: "\u{2014}".to_owned(),
                error: "\u{2014}".to_owned(),
            },
        ];
        reduce(&mut state, Action::OpenWorkflow);
        let text = render_to_string(&state, 120, 40);

        assert!(text.contains("Workflow"), "title missing:\n{text}");
        // The workflow group header and the node ids in the list.
        assert!(
            text.contains("repair-github-check v1"),
            "group header missing:\n{text}"
        );
        assert!(text.contains("patch"), "node id missing:\n{text}");
        // The focused (first) node's detail — the exit-criterion payload: state,
        // agent, worktree, approval, and declared outputs.
        assert!(text.contains("pending"), "state missing:\n{text}");
        assert!(text.contains("implementer"), "agent missing:\n{text}");
        assert!(
            text.contains("isolated worktree"),
            "worktree missing:\n{text}"
        );
        assert!(text.contains("before write"), "approval missing:\n{text}");
        assert!(text.contains("proposed_patch"), "outputs missing:\n{text}");
    }

    #[test]
    fn blackboard_view_snapshot_shows_artifact_provenance() {
        use crate::state::BlackboardItemCard;
        let mut state = running_build_state();
        state.blackboard = vec![BlackboardItemCard {
            run: "repair-github-check \u{b7} run 0f2a".to_owned(),
            kind: "finding".to_owned(),
            summary: "the failing test asserts an off-by-one in paginate()".to_owned(),
            author: "agent investigator".to_owned(),
            confidence: "0.85".to_owned(),
            evidence: "2 ref(s)".to_owned(),
            revision: "r1".to_owned(),
            superseded: false,
        }];
        reduce(&mut state, Action::OpenBlackboard);
        let text = render_to_string(&state, 120, 40);

        assert!(text.contains("Blackboard"), "title missing:\n{text}");
        // Run group header + the artifact kind.
        assert!(
            text.contains("repair-github-check"),
            "run header missing:\n{text}"
        );
        assert!(text.contains("finding"), "kind missing:\n{text}");
        // The provenance payload the exit criterion wants visible.
        assert!(
            text.contains("agent investigator"),
            "author missing:\n{text}"
        );
        assert!(text.contains("0.85"), "confidence missing:\n{text}");
        assert!(text.contains("2 ref(s)"), "evidence missing:\n{text}");
        assert!(
            text.contains("off-by-one"),
            "payload summary missing:\n{text}"
        );
    }
}
