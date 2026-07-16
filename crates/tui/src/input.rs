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
        keys: "Tab",
        description: "cycle panes",
        mouse: Some("click a pane"),
    },
    KeyBinding {
        keys: "Up / k",
        description: "select previous / scroll up",
        mouse: Some("wheel up"),
    },
    KeyBinding {
        keys: "Down / j",
        description: "select next / scroll down",
        mouse: Some("wheel down"),
    },
    KeyBinding {
        keys: "PgUp / PgDn",
        description: "scroll transcript by a page",
        mouse: None,
    },
    KeyBinding {
        keys: "Enter",
        description: "open / expand selected",
        mouse: None,
    },
    KeyBinding {
        keys: "n",
        description: "new run",
        mouse: None,
    },
    KeyBinding {
        keys: "p",
        description: "pause / resume run",
        mouse: None,
    },
    KeyBinding {
        keys: "c",
        description: "cancel run (asks to confirm)",
        mouse: None,
    },
    KeyBinding {
        keys: "s",
        description: "steer: queue a message for the next safe point",
        mouse: None,
    },
    KeyBinding {
        keys: "a / A",
        description: "approve once / approve for the run",
        mouse: None,
    },
    KeyBinding {
        keys: "r",
        description: "reject approval",
        mouse: None,
    },
    KeyBinding {
        keys: "S",
        description: "open the Skill Studio (permissions verbatim)",
        mouse: None,
    },
    KeyBinding {
        keys: "M",
        description: "open the memory browser (provenance cards)",
        mouse: None,
    },
    KeyBinding {
        keys: "o",
        description: "open the focused memory's source",
        mouse: None,
    },
    KeyBinding {
        keys: "q",
        description: "detach (the run keeps going)",
        mouse: None,
    },
    KeyBinding {
        keys: "?",
        description: "toggle this help",
        mouse: None,
    },
];

/// Translate a terminal event into a semantic [`Action`].
///
/// `mode` decides whether printable keys are commands or text; `width` is the
/// current terminal width, consulted only to resolve which pane a left-click
/// hit. Keys ignore `width`. The mapping is total — anything unrecognized maps
/// to [`Action::NoOp`].
#[must_use]
pub fn map_event(event: &Event, mode: InputMode, width: u16) -> Action {
    match event {
        Event::Key(key) => map_key(key, mode),
        Event::Mouse(mouse) => map_mouse(mouse, mode, width),
        Event::Paste(text) if mode == InputMode::Editing => Action::InputPaste(text.clone()),
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

fn map_confirm_key(key: &KeyEvent) -> Action {
    match key.code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => Action::ConfirmCancel,
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => Action::Dismiss,
        _ => Action::NoOp,
    }
}

fn map_mouse(mouse: &MouseEvent, mode: InputMode, width: u16) -> Action {
    // A prompt is capturing; don't let clicks fire commands underneath it.
    if mode != InputMode::Normal {
        return Action::NoOp;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => Action::SelectPrev,
        MouseEventKind::ScrollDown => Action::SelectNext,
        MouseEventKind::Down(MouseButton::Left) => Action::FocusPane(pane_at(mouse.column, width)),
        _ => Action::NoOp,
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

        // (2) Live mapping: the actual Action a mouse gesture produces is
        // reachable from the keyboard.
        // wheel up  == Up / k  (SelectPrev)
        let wheel_up = map_event(&wheel(MouseEventKind::ScrollUp, 10), InputMode::Normal, W);
        assert_eq!(wheel_up, Action::SelectPrev);
        assert_eq!(wheel_up, map_event(&key(KeyCode::Up), InputMode::Normal, W));
        assert_eq!(wheel_up, map_event(&ch('k'), InputMode::Normal, W));

        // wheel down == Down / j (SelectNext)
        let wheel_down = map_event(&wheel(MouseEventKind::ScrollDown, 10), InputMode::Normal, W);
        assert_eq!(wheel_down, Action::SelectNext);
        assert_eq!(
            wheel_down,
            map_event(&key(KeyCode::Down), InputMode::Normal, W)
        );
        assert_eq!(wheel_down, map_event(&ch('j'), InputMode::Normal, W));

        // left-click focuses a pane; Tab is the keyboard focus control. Both are
        // focus-changing actions.
        let click = map_event(
            &wheel(MouseEventKind::Down(MouseButton::Left), 1),
            InputMode::Normal,
            W,
        );
        assert_eq!(click, Action::FocusPane(Pane::Sessions));
        assert_eq!(
            map_event(&key(KeyCode::Tab), InputMode::Normal, W),
            Action::CyclePane
        );
    }
}
