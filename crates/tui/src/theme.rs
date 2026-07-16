//! Semantic theme tokens (STEP 1.12 RULE 7).
//!
//! Widgets must never hard-code colors — every color they draw is read from a
//! [`Theme`] token. That keeps the palette swappable (dark, high-contrast,
//! color-blind-safe variants can be added later without touching a single
//! widget) and matches the [Chapter 10](../../docs/docs/10-ide-github-and-inputs.md)
//! `Theme` shape (`surface / text / status / syntax / diff / agent`), extended
//! here with explicit `focus` and `selection` groups the layout needs.

use ratatui::style::{Color, Modifier, Style};

/// Backgrounds and structural chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceTokens {
    /// The overall terminal background.
    pub background: Color,
    /// A raised panel / pane body.
    pub panel: Color,
    /// A pane border when the pane is not focused.
    pub border: Color,
    /// The background of an overlay / modal.
    pub overlay: Color,
}

/// Foreground text roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextTokens {
    /// Body text.
    pub primary: Color,
    /// Supporting / dimmer text.
    pub secondary: Color,
    /// De-emphasized text (timestamps, hints).
    pub muted: Color,
    /// Section headings / titles.
    pub heading: Color,
}

/// Status roles used by the status line, run state, and notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusTokens {
    pub info: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    /// An actively running / working state.
    pub running: Color,
    /// An idle / terminal-but-fine state.
    pub idle: Color,
}

/// Syntax roles (used when rendering code / commands in tool cards).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxTokens {
    pub keyword: Color,
    pub literal: Color,
    pub string: Color,
    pub comment: Color,
}

/// Diff / patch roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffTokens {
    pub added: Color,
    pub removed: Color,
    pub context: Color,
    pub header: Color,
}

/// Agent-activity roles (model text, tool cards, thinking).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentTokens {
    /// Streamed model prose.
    pub model_text: Color,
    /// A tool card accent.
    pub tool: Color,
    /// Thinking / internal reasoning markers.
    pub thinking: Color,
}

/// Focus indication (which pane the keyboard drives).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusTokens {
    /// The border/accent of the focused pane.
    pub active: Color,
    /// The border/accent of an unfocused pane.
    pub inactive: Color,
}

/// Selection highlight (selected list row / transcript entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionTokens {
    pub foreground: Color,
    pub background: Color,
}

/// A complete set of semantic tokens. Constructed once and threaded through
/// every render call; widgets only ever read from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub surface: SurfaceTokens,
    pub text: TextTokens,
    pub status: StatusTokens,
    pub syntax: SyntaxTokens,
    pub diff: DiffTokens,
    pub agent: AgentTokens,
    pub focus: FocusTokens,
    pub selection: SelectionTokens,
}

impl Theme {
    /// The built-in dark theme (STEP 1.12 RULE 7: ship at least a dark theme).
    #[must_use]
    pub const fn dark() -> Self {
        Self {
            surface: SurfaceTokens {
                background: Color::Rgb(0x12, 0x14, 0x18),
                panel: Color::Rgb(0x1a, 0x1d, 0x23),
                border: Color::Rgb(0x33, 0x38, 0x42),
                overlay: Color::Rgb(0x22, 0x26, 0x2e),
            },
            text: TextTokens {
                primary: Color::Rgb(0xe6, 0xe9, 0xef),
                secondary: Color::Rgb(0xb4, 0xba, 0xc6),
                muted: Color::Rgb(0x7a, 0x82, 0x91),
                heading: Color::Rgb(0xf2, 0xf4, 0xf8),
            },
            status: StatusTokens {
                info: Color::Rgb(0x5c, 0x9d, 0xff),
                success: Color::Rgb(0x5d, 0xd6, 0x9a),
                warning: Color::Rgb(0xe6, 0xb4, 0x50),
                error: Color::Rgb(0xef, 0x6d, 0x6d),
                running: Color::Rgb(0x74, 0xc0, 0xf0),
                idle: Color::Rgb(0x8a, 0x93, 0xa3),
            },
            syntax: SyntaxTokens {
                keyword: Color::Rgb(0xc6, 0x92, 0xff),
                literal: Color::Rgb(0xe6, 0xb4, 0x50),
                string: Color::Rgb(0x9c, 0xd6, 0x7a),
                comment: Color::Rgb(0x6b, 0x74, 0x84),
            },
            diff: DiffTokens {
                added: Color::Rgb(0x5d, 0xd6, 0x9a),
                removed: Color::Rgb(0xef, 0x6d, 0x6d),
                context: Color::Rgb(0x9a, 0xa2, 0xb1),
                header: Color::Rgb(0x5c, 0x9d, 0xff),
            },
            agent: AgentTokens {
                model_text: Color::Rgb(0xd7, 0xdc, 0xe4),
                tool: Color::Rgb(0x74, 0xc0, 0xf0),
                thinking: Color::Rgb(0x8a, 0x93, 0xa3),
            },
            focus: FocusTokens {
                active: Color::Rgb(0x5c, 0x9d, 0xff),
                inactive: Color::Rgb(0x33, 0x38, 0x42),
            },
            selection: SelectionTokens {
                foreground: Color::Rgb(0x12, 0x14, 0x18),
                background: Color::Rgb(0x5c, 0x9d, 0xff),
            },
        }
    }

    /// Base style for pane bodies (panel background, primary text).
    #[must_use]
    pub fn panel_style(&self) -> Style {
        Style::default()
            .bg(self.surface.panel)
            .fg(self.text.primary)
    }

    /// Border color for a pane, depending on whether it is focused.
    #[must_use]
    pub fn border_color(&self, focused: bool) -> Color {
        if focused {
            self.focus.active
        } else {
            self.focus.inactive
        }
    }

    /// Highlight style for the selected row / entry.
    #[must_use]
    pub fn selection_style(&self) -> Style {
        Style::default()
            .fg(self.selection.foreground)
            .bg(self.selection.background)
            .add_modifier(Modifier::BOLD)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}
