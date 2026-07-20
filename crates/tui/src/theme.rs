//! Semantic theme tokens (STEP 1.12 RULE 7).
//!
//! Widgets must never hard-code colors — every color they draw is read from a
//! [`Theme`] token. That keeps the palette swappable (dark, high-contrast,
//! color-blind-safe variants can be added later without touching a single
//! widget) and matches the [Chapter 10](../../docs/docs/10-ide-github-and-inputs.md)
//! `Theme` shape (`surface / text / status / syntax / diff / agent`), extended
//! here with explicit `focus` and `selection` groups the layout needs.

use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};

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

    /// A true-color **light** variant, for light terminals. The same semantic
    /// tokens, inverted for a bright background.
    #[must_use]
    pub const fn light() -> Self {
        Self {
            surface: SurfaceTokens {
                background: Color::Rgb(0xfa, 0xfb, 0xfc),
                panel: Color::Rgb(0xff, 0xff, 0xff),
                border: Color::Rgb(0xd0, 0xd7, 0xde),
                overlay: Color::Rgb(0xf0, 0xf2, 0xf5),
            },
            text: TextTokens {
                primary: Color::Rgb(0x1f, 0x23, 0x28),
                secondary: Color::Rgb(0x4a, 0x52, 0x5e),
                muted: Color::Rgb(0x7a, 0x82, 0x91),
                heading: Color::Rgb(0x0d, 0x11, 0x17),
            },
            status: StatusTokens {
                info: Color::Rgb(0x0a, 0x5a, 0xd0),
                success: Color::Rgb(0x1a, 0x7f, 0x4b),
                warning: Color::Rgb(0x9a, 0x6b, 0x00),
                error: Color::Rgb(0xc0, 0x2a, 0x2a),
                running: Color::Rgb(0x0a, 0x6a, 0xa8),
                idle: Color::Rgb(0x6b, 0x73, 0x82),
            },
            syntax: SyntaxTokens {
                keyword: Color::Rgb(0x7a, 0x30, 0xc0),
                literal: Color::Rgb(0x9a, 0x6b, 0x00),
                string: Color::Rgb(0x1a, 0x7f, 0x4b),
                comment: Color::Rgb(0x8a, 0x92, 0xa1),
            },
            diff: DiffTokens {
                added: Color::Rgb(0x1a, 0x7f, 0x4b),
                removed: Color::Rgb(0xc0, 0x2a, 0x2a),
                context: Color::Rgb(0x4a, 0x52, 0x5e),
                header: Color::Rgb(0x0a, 0x5a, 0xd0),
            },
            agent: AgentTokens {
                model_text: Color::Rgb(0x1f, 0x23, 0x28),
                tool: Color::Rgb(0x0a, 0x6a, 0xa8),
                thinking: Color::Rgb(0x6b, 0x73, 0x82),
            },
            focus: FocusTokens {
                active: Color::Rgb(0x0a, 0x5a, 0xd0),
                inactive: Color::Rgb(0xd0, 0xd7, 0xde),
            },
            selection: SelectionTokens {
                foreground: Color::Rgb(0xff, 0xff, 0xff),
                background: Color::Rgb(0x0a, 0x5a, 0xd0),
            },
        }
    }

    /// A **high-contrast** variant: pure black background, pure white text, and
    /// maximally saturated status colors — the accessibility baseline for low
    /// vision. Every token is deliberately far from every other in luminance.
    #[must_use]
    pub const fn high_contrast() -> Self {
        Self {
            surface: SurfaceTokens {
                background: Color::Rgb(0x00, 0x00, 0x00),
                panel: Color::Rgb(0x00, 0x00, 0x00),
                border: Color::Rgb(0xff, 0xff, 0xff),
                overlay: Color::Rgb(0x0a, 0x0a, 0x0a),
            },
            text: TextTokens {
                primary: Color::Rgb(0xff, 0xff, 0xff),
                secondary: Color::Rgb(0xe0, 0xe0, 0xe0),
                muted: Color::Rgb(0xc0, 0xc0, 0xc0),
                heading: Color::Rgb(0xff, 0xff, 0xff),
            },
            status: StatusTokens {
                info: Color::Rgb(0x00, 0xd7, 0xff),
                success: Color::Rgb(0x00, 0xff, 0x5f),
                warning: Color::Rgb(0xff, 0xd7, 0x00),
                error: Color::Rgb(0xff, 0x30, 0x30),
                running: Color::Rgb(0x00, 0xd7, 0xff),
                idle: Color::Rgb(0xc0, 0xc0, 0xc0),
            },
            syntax: SyntaxTokens {
                keyword: Color::Rgb(0xff, 0x80, 0xff),
                literal: Color::Rgb(0xff, 0xd7, 0x00),
                string: Color::Rgb(0x00, 0xff, 0x5f),
                comment: Color::Rgb(0xc0, 0xc0, 0xc0),
            },
            diff: DiffTokens {
                added: Color::Rgb(0x00, 0xff, 0x5f),
                removed: Color::Rgb(0xff, 0x30, 0x30),
                context: Color::Rgb(0xe0, 0xe0, 0xe0),
                header: Color::Rgb(0x00, 0xd7, 0xff),
            },
            agent: AgentTokens {
                model_text: Color::Rgb(0xff, 0xff, 0xff),
                tool: Color::Rgb(0x00, 0xd7, 0xff),
                thinking: Color::Rgb(0xc0, 0xc0, 0xc0),
            },
            focus: FocusTokens {
                active: Color::Rgb(0xff, 0xff, 0x00),
                inactive: Color::Rgb(0x80, 0x80, 0x80),
            },
            selection: SelectionTokens {
                foreground: Color::Rgb(0x00, 0x00, 0x00),
                background: Color::Rgb(0xff, 0xff, 0x00),
            },
        }
    }

    /// A **color-blind-safe** variant using the Okabe–Ito palette — hues chosen
    /// to stay distinct under deuteranopia/protanopia/tritanopia. Notably it
    /// avoids the red/green pairing for added/removed, using vermillion vs.
    /// bluish-green (distinguishable) rather than pure red vs. green.
    #[must_use]
    pub const fn color_blind_safe() -> Self {
        // Okabe–Ito: orange E69F00, sky-blue 56B4E9, bluish-green 009E73,
        // yellow F0E442, blue 0072B2, vermillion D55E00, reddish-purple CC79A7.
        Self {
            surface: SurfaceTokens {
                background: Color::Rgb(0x11, 0x13, 0x17),
                panel: Color::Rgb(0x1b, 0x1e, 0x24),
                border: Color::Rgb(0x3a, 0x40, 0x4a),
                overlay: Color::Rgb(0x23, 0x27, 0x2f),
            },
            text: TextTokens {
                primary: Color::Rgb(0xed, 0xf0, 0xf5),
                secondary: Color::Rgb(0xbc, 0xc2, 0xce),
                muted: Color::Rgb(0x86, 0x8e, 0x9d),
                heading: Color::Rgb(0xf5, 0xf7, 0xfb),
            },
            status: StatusTokens {
                info: Color::Rgb(0x56, 0xb4, 0xe9),    // sky blue
                success: Color::Rgb(0x00, 0x9e, 0x73), // bluish green
                warning: Color::Rgb(0xe6, 0x9f, 0x00), // orange
                error: Color::Rgb(0xd5, 0x5e, 0x00),   // vermillion
                running: Color::Rgb(0x56, 0xb4, 0xe9),
                idle: Color::Rgb(0x94, 0x9c, 0xac),
            },
            syntax: SyntaxTokens {
                keyword: Color::Rgb(0xcc, 0x79, 0xa7), // reddish purple
                literal: Color::Rgb(0xe6, 0x9f, 0x00),
                string: Color::Rgb(0x00, 0x9e, 0x73),
                comment: Color::Rgb(0x86, 0x8e, 0x9d),
            },
            diff: DiffTokens {
                added: Color::Rgb(0x00, 0x9e, 0x73), // bluish green (not pure green)
                removed: Color::Rgb(0xd5, 0x5e, 0x00), // vermillion (not pure red)
                context: Color::Rgb(0xbc, 0xc2, 0xce),
                header: Color::Rgb(0x56, 0xb4, 0xe9),
            },
            agent: AgentTokens {
                model_text: Color::Rgb(0xed, 0xf0, 0xf5),
                tool: Color::Rgb(0x56, 0xb4, 0xe9),
                thinking: Color::Rgb(0x94, 0x9c, 0xac),
            },
            focus: FocusTokens {
                active: Color::Rgb(0x56, 0xb4, 0xe9),
                inactive: Color::Rgb(0x3a, 0x40, 0x4a),
            },
            selection: SelectionTokens {
                foreground: Color::Rgb(0x11, 0x13, 0x17),
                background: Color::Rgb(0x56, 0xb4, 0xe9),
            },
        }
    }

    /// A **256-color** variant built from the xterm-256 indexed palette, for
    /// terminals without 24-bit color. Uses `Color::Indexed` throughout.
    #[must_use]
    pub const fn ansi256() -> Self {
        Self {
            surface: SurfaceTokens {
                background: Color::Indexed(234),
                panel: Color::Indexed(235),
                border: Color::Indexed(240),
                overlay: Color::Indexed(237),
            },
            text: TextTokens {
                primary: Color::Indexed(253),
                secondary: Color::Indexed(250),
                muted: Color::Indexed(244),
                heading: Color::Indexed(255),
            },
            status: StatusTokens {
                info: Color::Indexed(75),
                success: Color::Indexed(78),
                warning: Color::Indexed(179),
                error: Color::Indexed(203),
                running: Color::Indexed(81),
                idle: Color::Indexed(245),
            },
            syntax: SyntaxTokens {
                keyword: Color::Indexed(141),
                literal: Color::Indexed(179),
                string: Color::Indexed(114),
                comment: Color::Indexed(242),
            },
            diff: DiffTokens {
                added: Color::Indexed(78),
                removed: Color::Indexed(203),
                context: Color::Indexed(249),
                header: Color::Indexed(75),
            },
            agent: AgentTokens {
                model_text: Color::Indexed(252),
                tool: Color::Indexed(81),
                thinking: Color::Indexed(245),
            },
            focus: FocusTokens {
                active: Color::Indexed(75),
                inactive: Color::Indexed(240),
            },
            selection: SelectionTokens {
                foreground: Color::Indexed(234),
                background: Color::Indexed(75),
            },
        }
    }

    /// A **16-color** variant using only the basic ANSI palette, so every widget
    /// stays legible on a 16-color terminal (STEP 6.6 fallback). Bright variants
    /// separate accents from body text.
    #[must_use]
    pub const fn ansi16() -> Self {
        Self {
            surface: SurfaceTokens {
                background: Color::Black,
                panel: Color::Black,
                border: Color::DarkGray,
                overlay: Color::Black,
            },
            text: TextTokens {
                primary: Color::White,
                secondary: Color::Gray,
                muted: Color::DarkGray,
                heading: Color::White,
            },
            status: StatusTokens {
                info: Color::LightBlue,
                success: Color::LightGreen,
                warning: Color::LightYellow,
                error: Color::LightRed,
                running: Color::LightCyan,
                idle: Color::Gray,
            },
            syntax: SyntaxTokens {
                keyword: Color::LightMagenta,
                literal: Color::LightYellow,
                string: Color::LightGreen,
                comment: Color::DarkGray,
            },
            diff: DiffTokens {
                added: Color::LightGreen,
                removed: Color::LightRed,
                context: Color::Gray,
                header: Color::LightBlue,
            },
            agent: AgentTokens {
                model_text: Color::White,
                tool: Color::LightCyan,
                thinking: Color::Gray,
            },
            focus: FocusTokens {
                active: Color::LightBlue,
                inactive: Color::DarkGray,
            },
            selection: SelectionTokens {
                foreground: Color::Black,
                background: Color::LightBlue,
            },
        }
    }

    /// A **monochrome** variant: no color at all, only white/gray/black. Widgets
    /// stay legible on a monochrome terminal; distinction comes from luminance and
    /// the text modifiers the render layer applies (bold selection, etc.).
    #[must_use]
    pub const fn monochrome() -> Self {
        Self {
            surface: SurfaceTokens {
                background: Color::Black,
                panel: Color::Black,
                border: Color::Gray,
                overlay: Color::Black,
            },
            text: TextTokens {
                primary: Color::White,
                secondary: Color::Gray,
                muted: Color::DarkGray,
                heading: Color::White,
            },
            status: StatusTokens {
                info: Color::White,
                success: Color::White,
                warning: Color::Gray,
                error: Color::White,
                running: Color::White,
                idle: Color::DarkGray,
            },
            syntax: SyntaxTokens {
                keyword: Color::White,
                literal: Color::Gray,
                string: Color::Gray,
                comment: Color::DarkGray,
            },
            diff: DiffTokens {
                added: Color::White,
                removed: Color::Gray,
                context: Color::DarkGray,
                header: Color::White,
            },
            agent: AgentTokens {
                model_text: Color::White,
                tool: Color::Gray,
                thinking: Color::DarkGray,
            },
            focus: FocusTokens {
                active: Color::White,
                inactive: Color::DarkGray,
            },
            selection: SelectionTokens {
                foreground: Color::Black,
                background: Color::White,
            },
        }
    }

    /// Construct the theme for a named [`ThemeVariant`].
    #[must_use]
    pub const fn variant(v: ThemeVariant) -> Self {
        match v {
            ThemeVariant::Dark => Self::dark(),
            ThemeVariant::Light => Self::light(),
            ThemeVariant::HighContrast => Self::high_contrast(),
            ThemeVariant::ColorBlindSafe => Self::color_blind_safe(),
            ThemeVariant::Ansi256 => Self::ansi256(),
            ThemeVariant::Ansi16 => Self::ansi16(),
            ThemeVariant::Monochrome => Self::monochrome(),
        }
    }

    /// Pick the best theme for a terminal's detected [`ColorDepth`] and the user's
    /// [`ThemePreferences`]. A manual override always wins (STEP 6.6: "capability
    /// detection picks the best variant with manual override"); otherwise
    /// accessibility preferences take precedence over depth, then depth chooses
    /// the fidelity.
    #[must_use]
    pub fn select(depth: ColorDepth, prefs: ThemePreferences) -> Self {
        if let Some(override_variant) = prefs.override_variant {
            return Self::variant(override_variant);
        }
        // Accessibility needs come before aesthetics — but only where the terminal
        // can render the distinct colors they rely on.
        if depth == ColorDepth::Monochrome {
            return Self::monochrome();
        }
        if prefs.high_contrast {
            return Self::high_contrast();
        }
        match depth {
            ColorDepth::TrueColor => {
                if prefs.color_blind_safe {
                    Self::color_blind_safe()
                } else if prefs.prefer_light {
                    Self::light()
                } else {
                    Self::dark()
                }
            }
            ColorDepth::Ansi256 => Self::ansi256(),
            ColorDepth::Ansi16 => Self::ansi16(),
            ColorDepth::Monochrome => Self::monochrome(),
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

/// The color fidelity a terminal supports, detected from the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit direct color (`COLORTERM=truecolor`).
    TrueColor,
    /// 256 indexed colors (`TERM=*-256color`).
    Ansi256,
    /// The 16 basic ANSI colors.
    Ansi16,
    /// No color (`NO_COLOR` set, or `TERM=dumb`).
    Monochrome,
}

impl ColorDepth {
    /// Detect the terminal's color depth from environment variables, following
    /// the de-facto conventions: `NO_COLOR` disables color entirely; `COLORTERM`
    /// of `truecolor`/`24bit` means direct color; a `256color` `TERM` means 256;
    /// a `dumb`/empty `TERM` means monochrome; otherwise assume 16.
    #[must_use]
    pub fn detect() -> Self {
        Self::from_env(
            std::env::var("NO_COLOR").ok().as_deref(),
            std::env::var("COLORTERM").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
        )
    }

    /// The pure detection rule, over explicit values (so it is testable without
    /// mutating the process environment).
    #[must_use]
    pub fn from_env(no_color: Option<&str>, colorterm: Option<&str>, term: Option<&str>) -> Self {
        // NO_COLOR (any non-empty value) forces monochrome — the user opted out.
        if no_color.is_some_and(|v| !v.is_empty()) {
            return ColorDepth::Monochrome;
        }
        if let Some(ct) = colorterm {
            if ct.eq_ignore_ascii_case("truecolor") || ct.eq_ignore_ascii_case("24bit") {
                return ColorDepth::TrueColor;
            }
        }
        match term {
            None => ColorDepth::Ansi16,
            Some(t) if t.is_empty() || t == "dumb" => ColorDepth::Monochrome,
            Some(t) if t.contains("256color") => ColorDepth::Ansi256,
            Some(t) if t.contains("truecolor") || t.contains("direct") => ColorDepth::TrueColor,
            Some(_) => ColorDepth::Ansi16,
        }
    }
}

/// A named built-in theme variant. STEP 6.6 ships six: true-color dark, light,
/// high-contrast, color-blind-safe, 256-color, 16-color, and monochrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeVariant {
    Dark,
    Light,
    HighContrast,
    ColorBlindSafe,
    Ansi256,
    Ansi16,
    Monochrome,
}

/// User theme preferences layered over terminal detection. A manual
/// `override_variant` wins outright; otherwise accessibility flags steer the
/// choice within what the terminal can render.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThemePreferences {
    /// Prefer the high-contrast variant (low-vision accessibility).
    pub high_contrast: bool,
    /// Prefer the color-blind-safe (Okabe–Ito) palette.
    pub color_blind_safe: bool,
    /// Prefer the light variant on a true-color terminal.
    pub prefer_light: bool,
    /// An explicit manual override — always honored.
    pub override_variant: Option<ThemeVariant>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant used by real terminals must keep body text visible against
    /// the panel background, or the UI is unreadable — the core legibility
    /// invariant behind "every variant renders every widget legibly".
    #[test]
    fn text_is_never_the_same_color_as_its_background() {
        for v in [
            ThemeVariant::Dark,
            ThemeVariant::Light,
            ThemeVariant::HighContrast,
            ThemeVariant::ColorBlindSafe,
            ThemeVariant::Ansi256,
            ThemeVariant::Ansi16,
            ThemeVariant::Monochrome,
        ] {
            let t = Theme::variant(v);
            assert_ne!(
                t.text.primary, t.surface.panel,
                "{v:?}: primary text invisible"
            );
            assert_ne!(
                t.text.primary, t.surface.background,
                "{v:?}: primary text invisible"
            );
            assert_ne!(
                t.selection.foreground, t.selection.background,
                "{v:?}: selection text invisible"
            );
            // A focused pane must be distinguishable from an unfocused one.
            assert_ne!(t.focus.active, t.focus.inactive, "{v:?}: focus indistinct");
        }
    }

    /// In the colored variants, added and removed diff lines must not collapse to
    /// the same hue, and success/error must differ — the semantic contrast the
    /// tokens exist to guarantee.
    #[test]
    fn colored_variants_keep_semantic_pairs_distinct() {
        for v in [
            ThemeVariant::Dark,
            ThemeVariant::Light,
            ThemeVariant::HighContrast,
            ThemeVariant::ColorBlindSafe,
            ThemeVariant::Ansi256,
            ThemeVariant::Ansi16,
        ] {
            let t = Theme::variant(v);
            assert_ne!(
                t.diff.added, t.diff.removed,
                "{v:?}: added/removed identical"
            );
            assert_ne!(
                t.status.success, t.status.error,
                "{v:?}: success/error identical"
            );
        }
    }

    /// The color-blind-safe variant must avoid the pure red/green diff pairing —
    /// that is the whole point of the Okabe–Ito palette.
    #[test]
    fn color_blind_safe_avoids_pure_red_green_for_diffs() {
        let t = Theme::color_blind_safe();
        assert_ne!(t.diff.added, Color::Rgb(0x00, 0xff, 0x00));
        assert_ne!(t.diff.removed, Color::Rgb(0xff, 0x00, 0x00));
        // Added is bluish-green, removed is vermillion (both from Okabe–Ito).
        assert_eq!(t.diff.added, Color::Rgb(0x00, 0x9e, 0x73));
        assert_eq!(t.diff.removed, Color::Rgb(0xd5, 0x5e, 0x00));
    }

    /// The monochrome variant must use only grayscale (white/gray/black) — no
    /// chromatic color at all.
    #[test]
    fn monochrome_is_purely_grayscale() {
        let t = Theme::monochrome();
        let grayscale = [Color::White, Color::Gray, Color::DarkGray, Color::Black];
        for c in [
            t.status.info,
            t.status.success,
            t.status.warning,
            t.status.error,
            t.diff.added,
            t.diff.removed,
            t.syntax.keyword,
            t.agent.tool,
            t.focus.active,
        ] {
            assert!(
                grayscale.contains(&c),
                "monochrome used a chromatic color: {c:?}"
            );
        }
    }

    #[test]
    fn detect_reads_env_conventions() {
        assert_eq!(
            ColorDepth::from_env(None, Some("truecolor"), Some("xterm-256color")),
            ColorDepth::TrueColor,
            "COLORTERM=truecolor wins over TERM"
        );
        assert_eq!(
            ColorDepth::from_env(None, None, Some("xterm-256color")),
            ColorDepth::Ansi256
        );
        assert_eq!(
            ColorDepth::from_env(None, None, Some("xterm")),
            ColorDepth::Ansi16
        );
        assert_eq!(
            ColorDepth::from_env(None, None, Some("dumb")),
            ColorDepth::Monochrome
        );
        // NO_COLOR overrides everything.
        assert_eq!(
            ColorDepth::from_env(Some("1"), Some("truecolor"), Some("xterm-256color")),
            ColorDepth::Monochrome
        );
        // Empty NO_COLOR does NOT disable color (the spec: any non-empty value).
        assert_eq!(
            ColorDepth::from_env(Some(""), Some("truecolor"), None),
            ColorDepth::TrueColor
        );
    }

    #[test]
    fn select_picks_by_depth() {
        let none = ThemePreferences::default();
        assert_eq!(Theme::select(ColorDepth::TrueColor, none), Theme::dark());
        assert_eq!(Theme::select(ColorDepth::Ansi256, none), Theme::ansi256());
        assert_eq!(Theme::select(ColorDepth::Ansi16, none), Theme::ansi16());
        assert_eq!(
            Theme::select(ColorDepth::Monochrome, none),
            Theme::monochrome()
        );
    }

    #[test]
    fn select_honors_accessibility_prefs_over_depth() {
        let hc = ThemePreferences {
            high_contrast: true,
            ..Default::default()
        };
        assert_eq!(
            Theme::select(ColorDepth::TrueColor, hc),
            Theme::high_contrast()
        );
        let cb = ThemePreferences {
            color_blind_safe: true,
            ..Default::default()
        };
        assert_eq!(
            Theme::select(ColorDepth::TrueColor, cb),
            Theme::color_blind_safe()
        );
        // But a monochrome terminal cannot render high-contrast color — depth wins
        // there, since the distinct colors accessibility relies on aren't available.
        assert_eq!(
            Theme::select(ColorDepth::Monochrome, hc),
            Theme::monochrome()
        );
    }

    #[test]
    fn manual_override_always_wins() {
        let prefs = ThemePreferences {
            high_contrast: true,
            override_variant: Some(ThemeVariant::Light),
            ..Default::default()
        };
        // Override beats both the high-contrast pref and the true-color depth.
        assert_eq!(Theme::select(ColorDepth::TrueColor, prefs), Theme::light());
        assert_eq!(Theme::select(ColorDepth::Monochrome, prefs), Theme::light());
    }
}
