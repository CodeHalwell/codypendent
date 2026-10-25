//! An optional, thin terminal driver (STEP 1.12: "you MAY add a thin optional
//! terminal-driver helper using crossterm").
//!
//! This is the *only* place the crate touches the real terminal, and it does so
//! synchronously — no async, no network. The CLI owns the protocol connection
//! and the event loop; it may use [`TerminalGuard`] to enter/leave raw mode and
//! the alternate screen, and to obtain a `ratatui` terminal to draw into. RAII
//! guarantees the terminal is restored even on panic.

use std::fmt;
use std::io::{self, IsTerminal, Stdout, Write};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{event, execute, Command};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::Terminal;

/// Practical upper bound on terminal title length.
pub const MAX_TERMINAL_TITLE_CHARS: usize = 240;

/// Outcome of a [`set_terminal_title`] call.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TitleOutcome {
    /// A sanitized title was written, or stdout is not a terminal.
    Applied,
    /// Sanitization removed every visible character, so no title was emitted.
    NoVisibleContent,
}

/// Notification delivery method (Adoption 11 S4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyMethod {
    /// OSC 9 desktop notification (Ghostty, iTerm2, Kitty, WezTerm, Warp).
    Osc9 { tmux_passthrough: bool },
    /// Terminal bell fallback (ASCII 0x07).
    Bel,
}

/// A RAII handle that puts the terminal into raw mode + alternate screen on
/// construction and restores it on drop.
pub struct TerminalGuard {
    terminal: Terminal<HyperlinkBackend<Stdout>>,
    title: TerminalTitle,
}

impl TerminalGuard {
    /// Enter raw mode and the alternate screen, enabling mouse capture,
    /// focus change reporting, and bracketed paste. Returns a ready-to-draw terminal.
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            event::EnableBracketedPaste,
            event::EnableFocusChange,
            event::PushKeyboardEnhancementFlags(
                event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        ) {
            Self::cleanup_after_failed_enter();
            return Err(e);
        }
        let backend = HyperlinkBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self {
                terminal,
                title: TerminalTitle::default(),
            }),
            Err(e) => {
                Self::cleanup_after_failed_enter();
                Err(e)
            }
        }
    }

    /// Best-effort teardown for a failed [`enter`](Self::enter): raw mode is on
    /// but no guard was constructed, so restore the terminal here.
    fn cleanup_after_failed_enter() {
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            event::PopKeyboardEnhancementFlags,
            LeaveAlternateScreen,
            DisableMouseCapture,
            event::DisableBracketedPaste,
            event::DisableFocusChange
        );
        let _ = disable_raw_mode();
    }

    /// Mutable access to the underlying `ratatui` terminal (to call `draw`).
    pub fn terminal_mut(&mut self) -> &mut Terminal<HyperlinkBackend<Stdout>> {
        &mut self.terminal
    }

    /// Update active hyperlink regions before next render.
    pub fn set_hyperlink_regions(&mut self, regions: Vec<(ratatui::layout::Rect, String)>) {
        self.terminal.backend_mut().set_regions(regions);
    }

    /// Update the tab title only when it changes, preserving the shell's title
    /// on terminals that support the standard xterm title stack.
    pub fn set_title(&mut self, title: &str) -> io::Result<()> {
        self.title.update(self.terminal.backend_mut(), title)
    }

    fn restore(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut().backend_mut(),
            event::PopKeyboardEnhancementFlags,
            LeaveAlternateScreen,
            DisableMouseCapture,
            event::DisableBracketedPaste,
            event::DisableFocusChange
        )?;
        let _ = self.title.restore(self.terminal.backend_mut());
        self.terminal.show_cursor()
    }
}

#[derive(Default)]
struct TerminalTitle {
    last: Option<String>,
}

impl TerminalTitle {
    fn update(&mut self, writer: &mut impl Write, title: &str) -> io::Result<()> {
        let sanitized = sanitize_terminal_title(title);
        if sanitized.is_empty() || self.last.as_deref() == Some(sanitized.as_str()) {
            return Ok(());
        }
        if self.last.is_none() {
            // Save window and icon titles; ignored by terminals without a stack.
            writer.write_all(b"\x1b[22;0t")?;
        }
        write!(writer, "\x1b]0;{sanitized}\x07")?;
        writer.flush()?;
        self.last = Some(sanitized);
        Ok(())
    }

    fn restore(&mut self, writer: &mut impl Write) -> io::Result<()> {
        if self.last.take().is_some() {
            writer.write_all(b"\x1b]0;\x07\x1b[23;0t")?;
            writer.flush()?;
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort restore; nothing useful to do if it fails during unwind.
        let _ = self.restore();
    }
}

/// Wraps CrosstermBackend to inject OSC-8 hyperlink sequences (Adoption 11 M5).
pub struct HyperlinkBackend<W: Write> {
    inner: CrosstermBackend<W>,
    regions: Vec<(ratatui::layout::Rect, String)>,
}

impl<W: Write> HyperlinkBackend<W> {
    pub fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            regions: Vec::new(),
        }
    }

    pub fn set_regions(&mut self, regions: Vec<(ratatui::layout::Rect, String)>) {
        self.regions = regions;
    }

    pub fn backend_mut(&mut self) -> &mut CrosstermBackend<W> {
        &mut self.inner
    }
}

impl<W: Write> Backend for HyperlinkBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        if self.regions.is_empty() {
            return self.inner.draw(content);
        }

        let mut current_link: Option<&str> = None;
        let mut batch: Vec<(u16, u16, &'a ratatui::buffer::Cell)> = Vec::new();

        for (x, y, cell) in content {
            let link = self.regions.iter().find_map(|(rect, url)| {
                if x >= rect.x
                    && x < rect.x.saturating_add(rect.width)
                    && y >= rect.y
                    && y < rect.y.saturating_add(rect.height)
                {
                    Some(url.as_str())
                } else {
                    None
                }
            });

            if link != current_link {
                if !batch.is_empty() {
                    if let Some(ref url) = current_link {
                        write!(self.inner, "\x1b]8;;{url}\x1b\\")?;
                    }
                    self.inner.draw(batch.drain(..))?;
                    if current_link.is_some() {
                        write!(self.inner, "\x1b]8;;\x1b\\")?;
                    }
                }
                current_link = link;
            }
            batch.push((x, y, cell));
        }

        if !batch.is_empty() {
            if let Some(ref url) = current_link {
                write!(self.inner, "\x1b]8;;{url}\x1b\\")?;
            }
            self.inner.draw(batch.drain(..))?;
            if current_link.is_some() {
                write!(self.inner, "\x1b]8;;\x1b\\")?;
            }
        }

        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<ratatui::layout::Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn size(&self) -> io::Result<ratatui::layout::Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<ratatui::backend::WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

impl<W: Write> Write for HyperlinkBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        std::io::Write::flush(&mut self.inner)
    }
}

/// Normalizes untrusted title text into a single bounded display line (Adoption 11 S4).
pub fn sanitize_terminal_title(title: &str) -> String {
    let mut sanitized = String::new();
    let mut chars_written = 0;
    let mut pending_space = false;

    for ch in title.chars() {
        if ch.is_whitespace() {
            pending_space = !sanitized.is_empty();
            continue;
        }

        if is_disallowed_terminal_title_char(ch) {
            continue;
        }

        if pending_space {
            let remaining = MAX_TERMINAL_TITLE_CHARS.saturating_sub(chars_written);
            if remaining > 1 {
                sanitized.push(' ');
                chars_written += 1;
                pending_space = false;
            }
        }

        if chars_written >= MAX_TERMINAL_TITLE_CHARS {
            break;
        }

        sanitized.push(ch);
        chars_written += 1;
    }

    sanitized
}

/// Returns whether `ch` should be dropped from terminal-title output.
fn is_disallowed_terminal_title_char(ch: char) -> bool {
    if ch.is_control() {
        return true;
    }
    matches!(
        ch,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{E0100}'..='\u{E01EF}'
    )
}

/// Writes a sanitized OSC window-title sequence to stdout.
pub fn set_terminal_title(title: &str) -> io::Result<TitleOutcome> {
    if !io::stdout().is_terminal() {
        return Ok(TitleOutcome::Applied);
    }

    let title = sanitize_terminal_title(title);
    if title.is_empty() {
        return Ok(TitleOutcome::NoVisibleContent);
    }

    execute!(io::stdout(), SetWindowTitle(title))?;
    Ok(TitleOutcome::Applied)
}

/// Clears the current terminal title by writing an empty OSC title payload.
pub fn clear_terminal_title() -> io::Result<()> {
    if !io::stdout().is_terminal() {
        return Ok(());
    }

    execute!(io::stdout(), SetWindowTitle(String::new()))
}

#[derive(Debug, Clone)]
struct SetWindowTitle(String);

impl Command for SetWindowTitle {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b]0;{}\x07", self.0)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "tried to execute SetWindowTitle using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Detects whether the current terminal environment supports OSC 9 notifications.
#[must_use]
pub fn detect_notify_method() -> NotifyMethod {
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term = std::env::var("TERM").unwrap_or_default();
    let in_tmux = std::env::var_os("TMUX").is_some();

    let supports_osc9 = matches!(
        term_program.to_lowercase().as_str(),
        "ghostty" | "iterm.app" | "iterm2" | "kitty" | "wezterm" | "warp"
    ) || term.contains("kitty")
        || term.contains("ghostty");

    if supports_osc9 {
        NotifyMethod::Osc9 {
            tmux_passthrough: in_tmux,
        }
    } else {
        NotifyMethod::Bel
    }
}

/// Emits a notification message via OSC 9 or BEL (Adoption 11 S4).
pub fn notify(message: &str, method: NotifyMethod) -> io::Result<()> {
    let mut stdout = io::stdout();
    if !stdout.is_terminal() {
        return Ok(());
    }

    match method {
        NotifyMethod::Osc9 { tmux_passthrough } => {
            let sanitized = sanitize_terminal_title(message);
            if tmux_passthrough {
                let escaped = sanitized.replace('\u{1b}', "\u{1b}\u{1b}");
                write!(stdout, "\x1bPtmux;\x1b\x1b]9;{escaped}\x07\x1b\\")?;
            } else {
                write!(stdout, "\x1b]9;{sanitized}\x07")?;
            }
            stdout.flush()
        }
        NotifyMethod::Bel => {
            write!(stdout, "\x07")?;
            stdout.flush()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_updates_are_deduplicated_and_restore_the_shell_title() {
        let mut title = TerminalTitle::default();
        let mut output = Vec::new();
        title.update(&mut output, "codypendent — café").unwrap();
        let initial = output.len();
        for _ in 0..100 {
            title.update(&mut output, "codypendent — café").unwrap();
        }
        assert_eq!(output.len(), initial);
        title.update(&mut output, "codypendent — next").unwrap();
        title.restore(&mut output).unwrap();
        let restored = output.len();
        title.restore(&mut output).unwrap();
        assert_eq!(output.len(), restored);
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("\x1b[22;0t").count(), 1);
        assert_eq!(output.matches("\x1b]0;codypendent").count(), 2);
        assert!(output.ends_with("\x1b]0;\x07\x1b[23;0t"));
    }

    #[test]
    fn hyperlink_runs_keep_their_boundaries_without_per_cell_url_copies() {
        let mut bytes = Vec::new();
        let mut backend = HyperlinkBackend::new(&mut bytes);
        backend.set_regions(vec![(
            ratatui::layout::Rect::new(1, 0, 2, 1),
            "https://example.com/source".to_owned(),
        )]);
        let cells: Vec<_> = "code"
            .chars()
            .map(|c| {
                let mut cell = ratatui::buffer::Cell::default();
                cell.set_symbol(&c.to_string());
                cell
            })
            .collect();
        backend
            .draw(
                cells
                    .iter()
                    .enumerate()
                    .map(|(x, cell)| (x as u16, 0, cell)),
            )
            .unwrap();
        drop(backend);
        let output = String::from_utf8(bytes).unwrap();
        assert_eq!(
            output
                .matches("\x1b]8;;https://example.com/source\x1b\\")
                .count(),
            1
        );
        assert_eq!(output.matches("\x1b]8;;\x1b\\").count(), 1);
    }

    #[test]
    fn sanitizes_terminal_title() {
        let sanitized =
            sanitize_terminal_title("  Project\t|\nWorking\x1b\x07\u{009D}\u{009C} |  Thread  ");
        assert_eq!(sanitized, "Project | Working | Thread");
    }

    #[test]
    fn strips_invisible_format_chars_from_terminal_title() {
        let sanitized = sanitize_terminal_title(
            "Pro\u{202E}j\u{2066}e\u{200F}c\u{061C}t\u{200B} \u{FEFF}T\u{2060}itle",
        );
        assert_eq!(sanitized, "Project Title");
    }

    #[test]
    fn truncates_terminal_title() {
        let input = "a".repeat(MAX_TERMINAL_TITLE_CHARS + 10);
        let sanitized = sanitize_terminal_title(&input);
        assert_eq!(sanitized.len(), MAX_TERMINAL_TITLE_CHARS);
    }

    #[test]
    fn writes_osc_title_with_bel_terminator() {
        let mut out = String::new();
        SetWindowTitle("hello".to_string())
            .write_ansi(&mut out)
            .expect("encode terminal title");
        assert_eq!(out, "\x1b]0;hello\x07");
    }
}
