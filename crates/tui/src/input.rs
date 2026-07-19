//! Input mapping (STEP 1.12 RULE 6 keys, RULE 3 mouse-parity).
//!
//! [`map_event`] is the single, pure translation from a `crossterm` event to an
//! [`Action`]. It performs no I/O and holds no state — it takes the current
//! [`InputMode`] (so printable keys route to an open prompt instead of firing
//! commands) and the terminal width (only to resolve which pane a mouse click
//! landed in). Every mouse gesture it recognizes has a keyboard equivalent
//! (RULE 3), captured in [`KEY_BINDINGS`] and asserted by the tests below.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use codypendent_protocol::ApprovalScope;

use crate::action::Action;
use crate::state::{InputMode, Pane};

/// One documented key binding. Feeds both the help overlay and the
/// keyboard/mouse equivalence test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyBinding {
    /// Human-readable key(s), e.g. `"a / A"`.
    pub keys: &'static str,
    /// What it does.
    pub description: &'static str,
    /// The mouse gesture that does the same thing, if any. When `Some`, `keys`
    /// is the guaranteed keyboard equivalent (RULE 3).
    pub mouse: Option<&'static str>,
}

/// The full key table. Rendered in the help overlay; the source of truth for
/// the mouse-parity guarantee.
pub const KEY_BINDINGS: &[KeyBinding] = &[
    KeyBinding {
        keys: "type…",
        description: "compose a message in the bottom composer",
        mouse: None,
    },
    KeyBinding {
        keys: "Enter",
        description: "send: start a run, or steer the active one",
        mouse: None,
    },
    KeyBinding {
        keys: "/",
        description: "command palette — every command, searchable",
        mouse: None,
    },
    KeyBinding {
        keys: "PgUp / PgDn",
        description: "scroll the conversation",
        mouse: Some("wheel"),
    },
    KeyBinding {
        keys: "Ctrl-↑ / Ctrl-↓",
        description: "switch to the previous / next run",
        mouse: None,
    },
    KeyBinding {
        keys: "F2",
        description: "toggle layout: chat ⇄ workspace panes",
        mouse: None,
    },
    KeyBinding {
        keys: "a / A",
        description: "approve once / for the run (when prompted)",
        mouse: None,
    },
    KeyBinding {
        keys: "r",
        description: "reject the pending action",
        mouse: None,
    },
    KeyBinding {
        keys: "Esc",
        description: "clear the draft, or close an overlay",
        mouse: None,
    },
    KeyBinding {
        keys: "?",
        description: "show / hide this help overlay",
        mouse: None,
    },
    KeyBinding {
        keys: "↑ / ↓",
        description: "move selection in a browser or the palette",
        mouse: Some("wheel"),
    },
    KeyBinding {
        keys: "Ctrl-C",
        description: "detach (the run keeps going)",
        mouse: None,
    },
];

/// Translate a terminal event into a semantic [`Action`].
///
/// `mode` decides whether printable keys are text or navigation; `width` is the
/// current terminal width (unused by the single-column shell, kept for the mouse
/// signature). The mapping is total — anything unrecognized maps to
/// [`Action::NoOp`].
#[must_use]
pub fn map_event(event: &Event, mode: InputMode, width: u16) -> Action {
    match event {
        Event::Key(key) => map_key(key, mode),
        Event::Mouse(mouse) => map_mouse(mouse, mode, width),
        // Bracketed paste lands in whichever text buffer is capturing: the
        // composer, a prompt, or the palette filter.
        Event::Paste(text)
            if matches!(
                mode,
                InputMode::Editing | InputMode::Composer | InputMode::Palette
            ) =>
        {
            Action::InputPaste(text.clone())
        }
        Event::Paste(_) | Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => {
            Action::NoOp
        }
    }
}

fn map_key(key: &KeyEvent, mode: InputMode) -> Action {
    // Ignore key-release events (some terminals report them; acting would
    // double-fire every command).
    if key.kind == KeyEventKind::Release {
        return Action::NoOp;
    }
    match mode {
        InputMode::Editing => map_editing_key(key),
        InputMode::Confirm => map_confirm_key(key),
        InputMode::Palette => map_palette_key(key),
        InputMode::Composer => map_composer_key(key),
        InputMode::Approval => map_approval_key(key),
        InputMode::Normal => map_normal_key(key),
    }
}

fn ctrl(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
}

fn map_normal_key(key: &KeyEvent) -> Action {
    // Ctrl-C detaches gracefully rather than being read as the `c` command.
    if ctrl(key) && key.code == KeyCode::Char('c') {
        return Action::Detach;
    }
    match key.code {
        KeyCode::Tab => Action::CyclePane,
        KeyCode::Enter => Action::Expand,
        KeyCode::Up => Action::SelectPrev,
        KeyCode::Down => Action::SelectNext,
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        KeyCode::Esc => Action::Dismiss,
        KeyCode::Char(c) => map_normal_char(c),
        _ => Action::NoOp,
    }
}

fn map_normal_char(c: char) -> Action {
    match c {
        'k' => Action::SelectPrev,
        'j' => Action::SelectNext,
        'n' => Action::NewRun,
        'p' => Action::Pause,
        'c' => Action::Cancel,
        's' => Action::Steer,
        'q' => Action::Detach,
        '?' => Action::Help,
        'a' => Action::Approve(ApprovalScope::Once),
        'A' => Action::Approve(ApprovalScope::Run),
        'r' => Action::Reject,
        'S' => Action::OpenSkills,
        'M' => Action::OpenMemory,
        'o' => Action::OpenSource,
        'D' => Action::OpenDocs,
        'G' => Action::OpenEdges,
        '/' => Action::OpenPalette,
        _ => Action::NoOp,
    }
}

fn map_editing_key(key: &KeyEvent) -> Action {
    match key.code {
        KeyCode::Enter => Action::InputSubmit,
        KeyCode::Esc => Action::InputCancel,
        KeyCode::Backspace => Action::InputBackspace,
        KeyCode::Char('c') if ctrl(key) => Action::InputCancel,
        KeyCode::Char(c) if !ctrl(key) => Action::InputChar(c),
        _ => Action::NoOp,
    }
}

/// The command palette captures printable keys as a filter query but stays
/// arrow-navigable: `Up`/`Down` move the selection, `Enter` runs the highlighted
/// command, `Esc` (or `Ctrl-C`) dismisses. This mirrors [`map_editing_key`] plus
/// navigation, so a query like `docs` filters while the selection still moves.
fn map_palette_key(key: &KeyEvent) -> Action {
    match key.code {
        KeyCode::Enter => Action::InputSubmit,
        KeyCode::Esc => Action::InputCancel,
        KeyCode::Backspace => Action::InputBackspace,
        KeyCode::Up => Action::SelectPrev,
        KeyCode::Down => Action::SelectNext,
        KeyCode::Char('c') if ctrl(key) => Action::InputCancel,
        KeyCode::Char(c) if !ctrl(key) => Action::InputChar(c),
        _ => Action::NoOp,
    }
}

/// The base conversation view. The composer captures typed text; Enter sends it;
/// `/` is a literal character (the reducer opens the palette only when it lands on
/// an empty composer); PgUp/PgDn scroll the transcript; Ctrl-↑/↓ switch runs;
/// Ctrl-C detaches; Esc clears the draft.
fn map_composer_key(key: &KeyEvent) -> Action {
    match key.code {
        KeyCode::Enter => Action::InputSubmit,
        KeyCode::Esc => Action::InputCancel,
        KeyCode::Backspace => Action::InputBackspace,
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        KeyCode::Up if ctrl(key) => Action::PrevRun,
        KeyCode::Down if ctrl(key) => Action::NextRun,
        KeyCode::F(2) => Action::ToggleLayout,
        KeyCode::Char('c') if ctrl(key) => Action::Detach,
        KeyCode::Char(c) if !ctrl(key) => Action::InputChar(c),
        _ => Action::NoOp,
    }
}

/// A pending approval owns the input: the decision keys, plus arrows to move
/// between stacked approvals. Ctrl-C still detaches (the run keeps going); `F2`
/// still flips the layout underneath.
fn map_approval_key(key: &KeyEvent) -> Action {
    if ctrl(key) && key.code == KeyCode::Char('c') {
        return Action::Detach;
    }
    match key.code {
        KeyCode::Char('a') => Action::Approve(ApprovalScope::Once),
        KeyCode::Char('A') => Action::Approve(ApprovalScope::Run),
        KeyCode::Char('r') => Action::Reject,
        KeyCode::Up => Action::SelectPrev,
        KeyCode::Down => Action::SelectNext,
        KeyCode::F(2) => Action::ToggleLayout,
        _ => Action::NoOp,
    }
}

fn map_confirm_key(key: &KeyEvent) -> Action {
    match key.code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => Action::ConfirmCancel,
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => Action::Dismiss,
        // Ctrl-C backs out of the modal like Esc (never silently swallowed).
        KeyCode::Char('c') if ctrl(key) => Action::Dismiss,
        _ => Action::NoOp,
    }
}

fn map_mouse(mouse: &MouseEvent, mode: InputMode, _width: u16) -> Action {
    match mode {
        // A text prompt / confirm captures nothing from the mouse.
        InputMode::Editing | InputMode::Confirm => Action::NoOp,
        // The conversation scrolls its transcript on the wheel.
        InputMode::Composer => match mouse.kind {
            MouseEventKind::ScrollUp => Action::ScrollPageUp,
            MouseEventKind::ScrollDown => Action::ScrollPageDown,
            _ => Action::NoOp,
        },
        // List surfaces — browsers, the palette, stacked approvals — move their
        // selection on the wheel. A left click is inert (there are no panes to
        // focus in the single-column shell).
        InputMode::Normal | InputMode::Palette | InputMode::Approval => match mouse.kind {
            MouseEventKind::ScrollUp => Action::SelectPrev,
            MouseEventKind::ScrollDown => Action::SelectNext,
            MouseEventKind::Down(MouseButton::Left) => Action::NoOp,
            _ => Action::NoOp,
        },
    }
}

/// Resolve which pane a column falls in, using the same 30 / 40 / 30 split the
/// renderer lays out (see [`crate::render`]).
#[must_use]
pub fn pane_at(column: u16, width: u16) -> Pane {
    let left = width * 3 / 10;
    let right_start = width.saturating_sub(width * 3 / 10);
    if column < left {
        Pane::Sessions
    } else if column >= right_start {
        Pane::Approvals
    } else {
        Pane::Transcript
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ch(c: char) -> Event {
        key(KeyCode::Char(c))
    }

    fn wheel(kind: MouseEventKind, column: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })
    }

    const W: u16 = 90;

    #[test]
    fn normal_command_keys_map() {
        assert_eq!(
            map_event(&key(KeyCode::Tab), InputMode::Normal, W),
            Action::CyclePane
        );
        assert_eq!(map_event(&ch('n'), InputMode::Normal, W), Action::NewRun);
        assert_eq!(map_event(&ch('p'), InputMode::Normal, W), Action::Pause);
        assert_eq!(map_event(&ch('c'), InputMode::Normal, W), Action::Cancel);
        assert_eq!(map_event(&ch('s'), InputMode::Normal, W), Action::Steer);
        assert_eq!(map_event(&ch('q'), InputMode::Normal, W), Action::Detach);
        assert_eq!(map_event(&ch('?'), InputMode::Normal, W), Action::Help);
        assert_eq!(
            map_event(&key(KeyCode::Enter), InputMode::Normal, W),
            Action::Expand
        );
        assert_eq!(
            map_event(&ch('a'), InputMode::Normal, W),
            Action::Approve(ApprovalScope::Once)
        );
        assert_eq!(
            map_event(&ch('A'), InputMode::Normal, W),
            Action::Approve(ApprovalScope::Run)
        );
        assert_eq!(map_event(&ch('r'), InputMode::Normal, W), Action::Reject);
        assert_eq!(
            map_event(&ch('S'), InputMode::Normal, W),
            Action::OpenSkills
        );
        assert_eq!(
            map_event(&ch('M'), InputMode::Normal, W),
            Action::OpenMemory
        );
        assert_eq!(
            map_event(&ch('o'), InputMode::Normal, W),
            Action::OpenSource
        );
        assert_eq!(map_event(&ch('D'), InputMode::Normal, W), Action::OpenDocs);
        assert_eq!(map_event(&ch('G'), InputMode::Normal, W), Action::OpenEdges);
        assert_eq!(
            map_event(&ch('/'), InputMode::Normal, W),
            Action::OpenPalette
        );
    }

    #[test]
    fn palette_mode_filters_but_stays_navigable() {
        // Printable keys become the filter query...
        assert_eq!(
            map_event(&ch('d'), InputMode::Palette, W),
            Action::InputChar('d')
        );
        // ...while arrows still move the selection and Enter runs it.
        assert_eq!(
            map_event(&key(KeyCode::Up), InputMode::Palette, W),
            Action::SelectPrev
        );
        assert_eq!(
            map_event(&key(KeyCode::Down), InputMode::Palette, W),
            Action::SelectNext
        );
        assert_eq!(
            map_event(&key(KeyCode::Enter), InputMode::Palette, W),
            Action::InputSubmit
        );
        assert_eq!(
            map_event(&key(KeyCode::Esc), InputMode::Palette, W),
            Action::InputCancel
        );
    }

    #[test]
    fn editing_mode_routes_text_not_commands() {
        // In a prompt, 'n' is text, not "new run".
        assert_eq!(
            map_event(&ch('n'), InputMode::Editing, W),
            Action::InputChar('n')
        );
        assert_eq!(
            map_event(&key(KeyCode::Enter), InputMode::Editing, W),
            Action::InputSubmit
        );
        assert_eq!(
            map_event(&key(KeyCode::Esc), InputMode::Editing, W),
            Action::InputCancel
        );
        assert_eq!(
            map_event(&key(KeyCode::Backspace), InputMode::Editing, W),
            Action::InputBackspace
        );
        assert_eq!(
            map_event(&Event::Paste("hello".to_owned()), InputMode::Editing, W),
            Action::InputPaste("hello".to_owned())
        );
    }

    #[test]
    fn confirm_mode_yes_no() {
        assert_eq!(
            map_event(&ch('y'), InputMode::Confirm, W),
            Action::ConfirmCancel
        );
        assert_eq!(
            map_event(&key(KeyCode::Enter), InputMode::Confirm, W),
            Action::ConfirmCancel
        );
        assert_eq!(map_event(&ch('n'), InputMode::Confirm, W), Action::Dismiss);
        assert_eq!(
            map_event(&key(KeyCode::Esc), InputMode::Confirm, W),
            Action::Dismiss
        );
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut ev = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        ev.kind = KeyEventKind::Release;
        assert_eq!(
            map_event(&Event::Key(ev), InputMode::Normal, W),
            Action::NoOp
        );
    }

    #[test]
    fn pane_hit_testing_uses_the_render_split() {
        assert_eq!(pane_at(1, W), Pane::Sessions);
        assert_eq!(pane_at(W / 2, W), Pane::Transcript);
        assert_eq!(pane_at(W - 2, W), Pane::Approvals);
    }

    fn ctrl(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
    }

    #[test]
    fn composer_mode_captures_text_and_controls() {
        // Printable keys are text — including `/`, which the reducer (not the
        // mapper) turns into a palette-open only on an empty composer.
        assert_eq!(
            map_event(&ch('h'), InputMode::Composer, W),
            Action::InputChar('h')
        );
        assert_eq!(
            map_event(&ch('/'), InputMode::Composer, W),
            Action::InputChar('/')
        );
        assert_eq!(
            map_event(&key(KeyCode::Enter), InputMode::Composer, W),
            Action::InputSubmit
        );
        assert_eq!(
            map_event(&key(KeyCode::Esc), InputMode::Composer, W),
            Action::InputCancel
        );
        assert_eq!(
            map_event(&key(KeyCode::PageUp), InputMode::Composer, W),
            Action::ScrollPageUp
        );
        // Ctrl-C detaches rather than typing a 'c'; Ctrl-↑/↓ switch runs.
        assert_eq!(
            map_event(&ctrl(KeyCode::Char('c')), InputMode::Composer, W),
            Action::Detach
        );
        assert_eq!(
            map_event(&ctrl(KeyCode::Up), InputMode::Composer, W),
            Action::PrevRun
        );
        assert_eq!(
            map_event(&ctrl(KeyCode::Down), InputMode::Composer, W),
            Action::NextRun
        );
        // F2 flips the layout from the base view.
        assert_eq!(
            map_event(&key(KeyCode::F(2)), InputMode::Composer, W),
            Action::ToggleLayout
        );
    }

    #[test]
    fn approval_mode_only_decision_keys() {
        assert_eq!(
            map_event(&ch('a'), InputMode::Approval, W),
            Action::Approve(ApprovalScope::Once)
        );
        assert_eq!(
            map_event(&ch('A'), InputMode::Approval, W),
            Action::Approve(ApprovalScope::Run)
        );
        assert_eq!(map_event(&ch('r'), InputMode::Approval, W), Action::Reject);
        // Typing past an approval is swallowed, not sent to a composer.
        assert_eq!(map_event(&ch('x'), InputMode::Approval, W), Action::NoOp);
        assert_eq!(
            map_event(&key(KeyCode::Up), InputMode::Approval, W),
            Action::SelectPrev
        );
    }

    /// RULE 3: every mouse interaction has a keyboard equivalent.
    #[test]
    fn every_mouse_gesture_has_a_keyboard_equivalent() {
        // (1) Table invariant: each binding advertising a mouse gesture names a
        // non-empty key that does the same thing.
        for binding in KEY_BINDINGS {
            if binding.mouse.is_some() {
                assert!(
                    !binding.keys.is_empty(),
                    "mouse gesture {:?} has no keyboard equivalent",
                    binding.mouse
                );
            }
        }

        // (2) Live mapping. In a list surface the wheel moves the selection,
        // reachable from the arrows.
        let wheel_up = map_event(&wheel(MouseEventKind::ScrollUp, 10), InputMode::Normal, W);
        assert_eq!(wheel_up, Action::SelectPrev);
        assert_eq!(wheel_up, map_event(&key(KeyCode::Up), InputMode::Normal, W));

        let wheel_down = map_event(&wheel(MouseEventKind::ScrollDown, 10), InputMode::Normal, W);
        assert_eq!(wheel_down, Action::SelectNext);
        assert_eq!(
            wheel_down,
            map_event(&key(KeyCode::Down), InputMode::Normal, W)
        );

        // In the conversation the wheel scrolls the transcript, reachable from
        // PgUp / PgDn.
        assert_eq!(
            map_event(&wheel(MouseEventKind::ScrollUp, 10), InputMode::Composer, W),
            Action::ScrollPageUp
        );
        assert_eq!(
            map_event(&key(KeyCode::PageUp), InputMode::Composer, W),
            Action::ScrollPageUp
        );

        // A left click is inert in the single-column shell (no panes to focus).
        let click = map_event(
            &wheel(MouseEventKind::Down(MouseButton::Left), 1),
            InputMode::Normal,
            W,
        );
        assert_eq!(click, Action::NoOp);
    }
}
