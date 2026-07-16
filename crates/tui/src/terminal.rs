//! An optional, thin terminal driver (STEP 1.12: "you MAY add a thin optional
//! terminal-driver helper using crossterm").
//!
//! This is the *only* place the crate touches the real terminal, and it does so
//! synchronously — no async, no network. The CLI owns the protocol connection
//! and the event loop; it may use [`TerminalGuard`] to enter/leave raw mode and
//! the alternate screen, and to obtain a `ratatui` terminal to draw into. RAII
//! guarantees the terminal is restored even on panic.

use std::io::{self, Stdout};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{event, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// A RAII handle that puts the terminal into raw mode + alternate screen on
/// construction and restores it on drop.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Enter raw mode and the alternate screen, enabling mouse capture and
    /// bracketed paste. Returns a ready-to-draw terminal.
    ///
    /// # Errors
    /// Propagates any terminal I/O error from crossterm / ratatui.
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            event::EnableBracketedPaste
        )?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }

    /// Mutable access to the underlying `ratatui` terminal (to call `draw`).
    pub fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }

    fn restore(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            event::DisableBracketedPaste
        )?;
        self.terminal.show_cursor()
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort restore; nothing useful to do if it fails during unwind.
        let _ = self.restore();
    }
}
