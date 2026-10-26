//! The opening identity: linked agents, a luminous wordmark, and an honest
//! startup state. Pure rendering; no timers, terminal queries, or boot delays.
use codypendent_protocol::BUILD_ID;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
    Frame,
};

use crate::{remote_ui::sanitize_terminal_text, render::truncate_display_width, Theme};

pub(crate) const MAX_SPLASH_WARNINGS: usize = 4;
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Render the boot screen and its ready gate. Existing boot ticks drive the
/// small highlight; readiness is completely still, regardless of tick. Reserve
/// space for status, warnings, and Enter before choosing decorative content.
pub fn render_splash(
    frame: &mut Frame,
    tick: u64,
    stage: &str,
    warnings: &[String],
    ready: bool,
    needs_setup: bool,
    theme: &Theme,
) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.surface.background)),
        area,
    );
    if area.width == 0 || area.height == 0 {
        return;
    }
    let framed = area.width >= 70 && area.height >= 18;
    let width = area
        .width
        .saturating_sub(if framed { 6 } else { 0 })
        .min(88);
    let capacity = usize::from(area.height.saturating_sub(if framed { 4 } else { 0 }));
    let action_rows = if ready {
        if capacity >= 8 {
            2
        } else {
            1
        }
    } else {
        0
    };
    let footer_rows = if ready && width >= 65 && capacity >= 18 {
        2
    } else {
        0
    };
    let warning_capacity = capacity.saturating_sub(2 + action_rows + footer_rows);
    let desired_warnings =
        warnings.len().min(MAX_SPLASH_WARNINGS) + usize::from(warnings.len() > MAX_SPLASH_WARNINGS);
    let warning_rows = desired_warnings.min(warning_capacity);
    let decoration_budget = capacity.saturating_sub(1 + action_rows + footer_rows + warning_rows);
    let expanded = width >= 65 && decoration_budget >= 9;
    let rich = expanded && decoration_budget >= 11;
    let mut lines = Vec::new();

    if framed {
        atmosphere(frame, area, theme);
    }
    if rich {
        lines.push(Line::styled(
            "C O L L A B O R A T I V E   I N T E L L I G E N C E",
            theme.muted(),
        ));
        lines.push(Line::raw(""));
        let emblem_budget = decoration_budget - 11;
        if emblem_budget >= 6 {
            lines.extend(emblem(tick, ready, theme));
            lines.push(Line::raw(""));
        } else if emblem_budget >= 4 {
            lines.push(Line::styled(
                "╭──────╮   ╭──────╮",
                Style::default().fg(theme.syntax.keyword),
            ));
            lines.push(Line::styled(
                "◇────┤   ╭──┼─◆─┼──╮   ├────◇",
                Style::default().fg(theme.status.running),
            ));
            lines.push(Line::styled(
                "╰──────╯   ╰──────╯",
                Style::default().fg(theme.syntax.keyword),
            ));
            lines.push(Line::raw(""));
        }
    }
    if expanded {
        lines.extend(wordmark(if ready { None } else { Some(tick) }, theme));
        lines.push(Line::raw(""));
    } else if decoration_budget > 0 {
        lines.push(Line::styled("codypendent", theme.heading()));
    }
    if expanded || (area.height >= 8 && decoration_budget >= 4) {
        lines.push(Line::styled(
            if width >= 53 {
                "Many agents. One shared workspace. You stay in control."
            } else {
                "Many agents. One workspace."
            },
            Style::default().fg(theme.text.secondary),
        ));
        let version = format!("v{BUILD_ID}");
        let workflow = Line::from(vec![
            Span::styled(format!("v{BUILD_ID}"), theme.muted()),
            Span::styled("   /   PLAN", Style::default().fg(theme.status.running)),
            Span::styled("  ·  BUILD", Style::default().fg(theme.status.info)),
            Span::styled("  ·  REVIEW", Style::default().fg(theme.syntax.keyword)),
        ]);
        lines.push(if workflow.width() <= usize::from(width) {
            workflow
        } else {
            Line::styled(one_line(&version, width), theme.muted())
        });
        lines.push(Line::raw(""));
    }

    if capacity > action_rows {
        let (glyph, color) = if !ready {
            (
                SPINNER[(tick % SPINNER.len() as u64) as usize].to_string(),
                theme.status.running,
            )
        } else if needs_setup {
            ("▲".to_owned(), theme.status.warning)
        } else {
            ("✓".to_owned(), theme.status.success)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph}  "), Style::default().fg(color)),
            Span::styled(one_line(stage, width.saturating_sub(3)), theme.heading()),
        ]));
    }
    let shown = if warnings.len() > warning_rows.min(MAX_SPLASH_WARNINGS) {
        warning_rows.saturating_sub(1).min(MAX_SPLASH_WARNINGS)
    } else {
        warnings.len()
    };
    for warning in warnings.iter().take(shown) {
        lines.push(Line::styled(
            format!("! {}", one_line(warning, width.saturating_sub(2))),
            Style::default().fg(theme.status.warning),
        ));
    }
    if warning_rows > shown {
        lines.push(Line::styled(
            format!("… +{} more · details in Issues", warnings.len() - shown),
            theme.muted(),
        ));
    }
    if ready {
        if action_rows == 2 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(vec![
            Span::styled(
                " ENTER ",
                Style::default()
                    .fg(theme.surface.background)
                    .bg(theme.focus.active)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if needs_setup {
                    "  connect a model  →"
                } else {
                    "  open workspace  →"
                },
                theme.heading(),
            ),
        ]));
        if footer_rows > 0 {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "/ commands   ·   F2 workspace   ·   F6 extension UI   ·   Esc quit",
                theme.muted(),
            ));
        }
    }
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    let content = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    // Clear the dot field behind the content without introducing a panel box.
    frame.render_widget(Clear, content);
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.surface.background)),
        content,
    );
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), content);
}

fn one_line(text: &str, width: u16) -> String {
    let bounded: String = text.chars().take(4096).collect();
    truncate_display_width(
        &sanitize_terminal_text(&bounded).replace('\n', " "),
        usize::from(width),
    )
}

/// Sparse registration marks suggest a connected field, not a boxed dialog.
fn atmosphere(frame: &mut Frame, area: Rect, theme: &Theme) {
    let style = Style::default().fg(theme.surface.border);
    for y in (area.y + 2..area.bottom().saturating_sub(2)).step_by(3) {
        for x in (area.x + 3..area.right().saturating_sub(3)).step_by(6) {
            frame.buffer_mut()[(x, y)].set_symbol("·").set_style(style);
        }
    }
    let inset = 2;
    for (x, y, symbol) in [
        (area.x + inset, area.y + 1, "╭─"),
        (area.right() - inset - 2, area.y + 1, "─╮"),
        (area.x + inset, area.bottom() - 2, "╰─"),
        (area.right() - inset - 2, area.bottom() - 2, "─╯"),
    ] {
        frame.render_widget(Paragraph::new(symbol).style(style), Rect::new(x, y, 2, 1));
    }
}

fn emblem(tick: u64, ready: bool, theme: &Theme) -> Vec<Line<'static>> {
    let rows = [
        ("       ╭───────╮", "   ╭───────╮       "),
        (" ◇─────┤  ╭────┼", "───┼────╮  ├─────◇ "),
        ("       │  │    │", " ◆ │    │  │       "),
        (" ◇─────┤  ╰────┼", "───┼────╯  ├─────◇ "),
        ("       ╰───────╯", "   ╰───────╯       "),
    ];
    rows.into_iter()
        .enumerate()
        .map(|(row, (left, right))| {
            let active = !ready && (tick / 3) % 5 == row as u64;
            Line::from(vec![
                Span::styled(
                    left,
                    Style::default().fg(if active {
                        theme.text.heading
                    } else {
                        theme.status.running
                    }),
                ),
                Span::styled(
                    right,
                    Style::default().fg(if active {
                        theme.text.heading
                    } else {
                        theme.syntax.keyword
                    }),
                ),
            ])
        })
        .collect()
}

fn wordmark(tick: Option<u64>, theme: &Theme) -> Vec<Line<'static>> {
    let palette = [
        theme.text.heading,
        theme.text.heading,
        theme.status.running,
        theme.status.info,
        theme.syntax.keyword,
    ];
    (0..WORDMARK_GLYPH_ROWS)
        .map(|row| {
            let mut spans = Vec::new();
            for (index, ch) in "CODYPENDENT".chars().enumerate() {
                if index > 0 {
                    spans.push(Span::raw(" "));
                }
                let shine = tick.is_some_and(|tick| (tick / 2) % 17 == index as u64);
                spans.push(Span::styled(
                    wordmark_glyph(ch)[row],
                    Style::default()
                        .fg(if shine {
                            theme.text.heading
                        } else {
                            palette[row]
                        })
                        .add_modifier(Modifier::BOLD),
                ));
            }
            Line::from(spans)
        })
        .collect()
}
/// The splash wordmark's per-glyph height in rows.
const WORDMARK_GLYPH_ROWS: usize = 5;

/// One 5×5 block-letter glyph of the splash wordmark (hand-drawn — no figlet
/// dependency). Only the letters of `CODYPENDENT` are defined.
fn wordmark_glyph(ch: char) -> [&'static str; WORDMARK_GLYPH_ROWS] {
    match ch {
        'C' => [" ███ ", "█   █", "█    ", "█   █", " ███ "],
        'O' => [" ███ ", "█   █", "█   █", "█   █", " ███ "],
        'D' => ["████ ", "█   █", "█   █", "█   █", "████ "],
        'Y' => ["█   █", "█   █", " █ █ ", "  █  ", "  █  "],
        'P' => ["████ ", "█   █", "████ ", "█    ", "█    "],
        'E' => ["█████", "█    ", "████ ", "█    ", "█████"],
        'N' => ["█   █", "██  █", "█ █ █", "█  ██", "█   █"],
        'T' => ["█████", "  █  ", "  █  ", "  █  ", "  █  "],
        _ => ["     "; WORDMARK_GLYPH_ROWS],
    }
}
