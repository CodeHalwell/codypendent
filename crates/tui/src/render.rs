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
    AppState, Overlay, Pane, PatchSummary, RunView, ToolCard, ToolStatus, TranscriptEntry,
};
use crate::theme::Theme;

/// Draw the whole UI for the current frame.
pub fn render(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.surface.background)),
        area,
    );

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(40),
            Constraint::Percentage(30),
        ])
        .split(rows[0]);

    render_sessions(frame, panes[0], state, theme);
    render_transcript(frame, panes[1], state, theme);
    render_right(frame, panes[2], state, theme);
    render_status_line(frame, rows[1], state, theme);

    render_overlays(frame, area, state, theme);
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

fn render_sessions(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let focused = state.focus == Pane::Sessions;
    let block = pane_block("Sessions & runs", focused, theme);

    let mut items: Vec<ListItem> = Vec::new();
    let title = state
        .session_title
        .clone()
        .unwrap_or_else(|| "(no session)".to_owned());
    items.push(ListItem::new(Line::styled(
        title,
        Style::default()
            .fg(theme.text.secondary)
            .add_modifier(Modifier::BOLD),
    )));

    if state.runs.is_empty() {
        items.push(ListItem::new(Line::styled(
            "  no runs yet — press n",
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
                truncate(&run.objective, 22),
                Style::default().fg(theme.text.primary),
            ),
        ]);
        let item = ListItem::new(line);
        items.push(if selected && focused {
            item.style(theme.selection_style())
        } else {
            item
        });
    }

    frame.render_widget(List::new(items).block(block), area);
}

fn render_transcript(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let focused = state.focus == Pane::Transcript;
    let title = match state.selected_run() {
        Some(run) => format!("Transcript — {}", truncate(&run.objective, 24)),
        None => "Transcript".to_owned(),
    };
    let block = pane_block(&title, focused, theme);

    let Some(run) = state.selected_run() else {
        let hint = Paragraph::new(Line::styled(
            "Nothing to show. Press n to start a run.",
            Style::default().fg(theme.text.muted),
        ))
        .block(block);
        frame.render_widget(hint, area);
        return;
    };

    let lines = transcript_lines(run, theme, focused);
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((run.scroll, 0));
    frame.render_widget(paragraph, area);
}

fn transcript_lines<'a>(run: &'a RunView, theme: &Theme, focused: bool) -> Vec<Line<'a>> {
    let mut lines: Vec<Line> = Vec::new();
    for (idx, entry) in run.transcript.iter().enumerate() {
        let selected = focused && idx == run.transcript_selected;
        entry_lines(entry, theme, selected, &mut lines);
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "(waiting for the agent…)",
            Style::default().fg(theme.text.muted),
        ));
    }
    lines
}

fn entry_lines<'a>(
    entry: &'a TranscriptEntry,
    theme: &Theme,
    selected: bool,
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
        TranscriptEntry::Model { text } => {
            for (i, l) in text.lines().enumerate() {
                let color = theme.agent.model_text;
                let prefix = if i == 0 { "▌ " } else { "  " };
                out.push(head(format!("{prefix}{l}"), color));
            }
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
        TranscriptEntry::Completed { disposition } => {
            let (label, color) = disposition_display(disposition, theme);
            out.push(head(format!("● {label}"), color));
        }
        TranscriptEntry::Note { text } => {
            out.push(head(format!("• note: {text}"), theme.text.secondary));
        }
        TranscriptEntry::Unsupported { label } => {
            out.push(head(format!("? {label}"), theme.text.muted));
        }
    }
}

fn tool_card_lines<'a>(card: &'a ToolCard, theme: &Theme, selected: bool, out: &mut Vec<Line<'a>>) {
    let (status_text, status_color) = match card.status {
        ToolStatus::Proposed => ("proposed", theme.status.warning),
        ToolStatus::Running => ("running", theme.status.running),
        ToolStatus::Completed => match &card.outcome {
            Some(codypendent_protocol::ToolOutcome::Failed { .. }) => {
                ("failed", theme.status.error)
            }
            _ => ("done", theme.status.success),
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
        Span::styled(format!("{marker} ⚙ {name} "), head_style),
        Span::styled(
            format!("[{status_text}]"),
            Style::default().fg(status_color),
        ),
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
    let marker = if patch.expanded { "▾" } else { "▸" };
    let head_style = if selected {
        theme.selection_style()
    } else {
        Style::default().fg(theme.diff.header)
    };
    out.push(Line::styled(
        format!(
            "{marker} ❖ patch proposed ({})",
            short_id(&patch.changeset_id)
        ),
        head_style,
    ));
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

fn render_right(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let focused = state.focus == Pane::Approvals;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // Approvals list.
    let block = pane_block(
        &format!("Approvals ({})", state.pending_approvals.len()),
        focused,
        theme,
    );
    let mut items: Vec<ListItem> = Vec::new();
    if state.pending_approvals.is_empty() {
        items.push(ListItem::new(Line::styled(
            "  none pending",
            Style::default().fg(theme.text.muted),
        )));
    }
    for (idx, approval) in state.pending_approvals.iter().enumerate() {
        let selected = idx == state.selected_approval;
        let line = Line::from(vec![
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
        ]);
        let item = ListItem::new(line);
        items.push(if selected && focused {
            item.style(theme.selection_style())
        } else {
            item
        });
    }
    frame.render_widget(List::new(items).block(block), rows[0]);

    // Run details.
    render_details(frame, rows[1], state, theme);
}

fn render_details(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let block = pane_block("Run details", false, theme);
    let mut lines: Vec<Line> = Vec::new();
    if let Some(run) = state.selected_run() {
        let field = |k: &str, v: String| -> Line {
            Line::from(vec![
                Span::styled(format!("{k}: "), Style::default().fg(theme.text.muted)),
                Span::styled(v, Style::default().fg(theme.text.primary)),
            ])
        };
        lines.push(field("objective", run.objective.clone()));
        lines.push(field("mode", mode_label(run.mode).to_owned()));
        lines.push(Line::from(vec![
            Span::styled("state: ", Style::default().fg(theme.text.muted)),
            Span::styled(
                run_state_label(run.state).to_owned(),
                Style::default().fg(run_state_color(run.state, theme)),
            ),
        ]));
        lines.push(field(
            "model",
            run.model
                .as_ref()
                .map_or("—".to_owned(), ToString::to_string),
        ));
        lines.push(field(
            "worktree",
            run.worktree.clone().unwrap_or_else(|| "—".to_owned()),
        ));
        lines.push(field(
            "context",
            run.context_percent
                .map_or("—".to_owned(), |p| format!("{p}%")),
        ));
        lines.push(field("cost", format_cost(run.cost_minor)));
    } else {
        lines.push(Line::styled(
            "no run selected",
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

fn render_status_line(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let status = state.status();
    let sep = Span::styled("  ", Style::default().fg(theme.text.muted));
    let mut spans: Vec<Span> = Vec::new();

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

    let field = |label: &str, value: String, color: Color| -> Vec<Span> {
        vec![
            Span::styled(format!("{label} "), Style::default().fg(theme.text.muted)),
            Span::styled(value, Style::default().fg(color)),
        ]
    };

    spans.push(Span::raw(" "));
    spans.extend(field(
        "mode",
        status
            .mode
            .map_or("—".to_owned(), |m| mode_label(m).to_owned()),
        theme.status.info,
    ));
    spans.push(sep.clone());
    spans.extend(field(
        "state",
        status
            .run_state
            .map_or("—".to_owned(), |s| run_state_label(s).to_owned()),
        status
            .run_state
            .map_or(theme.text.muted, |s| run_state_color(s, theme)),
    ));
    spans.push(sep.clone());
    spans.extend(field(
        "model",
        status
            .model
            .as_ref()
            .map_or("—".to_owned(), ToString::to_string),
        theme.text.secondary,
    ));
    spans.push(sep.clone());
    spans.extend(field(
        "ctx",
        status
            .context_percent
            .map_or("—".to_owned(), |p| format!("{p}%")),
        theme.status.info,
    ));
    spans.push(sep.clone());
    spans.extend(field(
        "cost",
        format_cost(status.cost_minor),
        theme.status.warning,
    ));
    spans.push(sep.clone());
    spans.extend(field(
        "wt",
        status.worktree.clone().unwrap_or_else(|| "—".to_owned()),
        theme.text.secondary,
    ));
    spans.push(sep);
    spans.extend(field(
        "approvals",
        status.pending_approvals.to_string(),
        if status.pending_approvals > 0 {
            theme.status.warning
        } else {
            theme.text.muted
        },
    ));

    let bg = Style::default().bg(theme.surface.overlay);
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
        Overlay::Palette { query, selected } => {
            render_palette(frame, area, theme, query, *selected);
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
        editor_lines.push(section("Editor rail", theme));
        if doc.blocks.is_empty() {
            editor_lines.push(Line::styled(
                "  (empty document)",
                Style::default().fg(theme.text.muted),
            ));
        }
        for block in &doc.blocks {
            editor_lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<10}", block.kind),
                    Style::default().fg(theme.text.secondary),
                ),
                Span::styled(
                    truncate(&block.text, 60),
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
        review_lines.push(section("Review rail (suggestions)", theme));
        if doc.suggestions.is_empty() {
            review_lines.push(Line::styled(
                "  no pending suggestions",
                Style::default().fg(theme.text.muted),
            ));
        }
        for suggestion in &doc.suggestions {
            review_lines.push(Line::from(vec![
                Span::styled("  • ", Style::default().fg(theme.status.info)),
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
        "  ↑/↓ select · G edges · Esc close",
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
    Line::styled(
        title.to_owned(),
        Style::default()
            .fg(theme.text.heading)
            .add_modifier(Modifier::UNDERLINED),
    )
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

fn disposition_display(disposition: &RunDisposition, theme: &Theme) -> (String, Color) {
    match disposition {
        RunDisposition::Completed { summary } => (
            format!(
                "run completed{}",
                summary.as_ref().map_or(String::new(), |s| format!(": {s}"))
            ),
            theme.status.success,
        ),
        RunDisposition::Failed { reason } => (format!("run failed: {reason}"), theme.status.error),
        RunDisposition::Cancelled { reason } => (
            format!(
                "run cancelled{}",
                reason.as_ref().map_or(String::new(), |s| format!(": {s}"))
            ),
            theme.text.muted,
        ),
        _ => ("run ended".to_owned(), theme.text.muted),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::reduce::reduce;
    use crate::state::{MemoryCard, SkillCard};
    use chrono::Utc;
    use codypendent_protocol::{
        Actor, ApprovalId, ArtifactId, ArtifactRef, DataClassification, EventBody, ModelId,
        ProposedAction, Risk, RiskLevel, RunId, SessionEvent, ToolOutcome,
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
    fn help_overlay_lists_bindings() {
        let mut state = running_build_state();
        reduce(&mut state, Action::Help);
        let text = render_to_string(&state, 110, 34);
        assert!(text.contains("Help"));
        assert!(text.contains("cycle panes"));
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
    fn renders_empty_state_without_panicking() {
        let state = AppState::new();
        let text = render_to_string(&state, 80, 24);
        assert!(text.contains("no runs yet"));
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
            title: "Payments runbook".to_owned(),
            scope: "organization".to_owned(),
            status: "draft".to_owned(),
            mode: "suggest".to_owned(),
            revision: "r7".to_owned(),
            blocks: vec![
                DocBlockView {
                    kind: "heading".to_owned(),
                    text: "Charging a customer".to_owned(),
                },
                DocBlockView {
                    kind: "paragraph".to_owned(),
                    text: "Call charge_customer with an idempotency key.".to_owned(),
                },
            ],
            suggestions: vec![DocSuggestionView {
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
}
