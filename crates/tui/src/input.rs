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
use ratatui::layout::Rect;

use codypendent_protocol::ApprovalScope;

use crate::action::Action;
use crate::remote_ui::RemoteKey;
use crate::state::{InputMode, Pane};

/// Maximum text accepted from one bracketed paste. This is deliberately much
/// larger than a normal prompt while still preventing a clipboard accident
/// from allocating/rendering an unbounded composer draft.
const MAX_PASTE_BYTES: usize = 64 * 1024;

/// Normalize clipboard text at the terminal boundary: keep useful Unicode and
/// multiline structure, normalize platform newlines, expand tabs to stable
/// cells, and remove terminal/control/bidi characters that must never become
/// invisible composer instructions. The byte cap always ends on a UTF-8
/// boundary.
fn sanitized_paste(text: &str) -> String {
    let mut clean = String::with_capacity(text.len().min(MAX_PASTE_BYTES));
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        // Normalise in one bounded pass. A large clipboard previously made
        // two full-size copies before the 64 KiB limit could take effect.
        let character = if character == '\r' {
            if characters.peek() == Some(&'\n') {
                characters.next();
            }
            '\n'
        } else {
            character
        };
        let fragment = match character {
            '\n' => "\n",
            '\t' => "    ",
            character
                if character.is_control()
                    || matches!(
                        character as u32,
                        0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069
                    ) =>
            {
                continue;
            }
            character => {
                if clean.len().saturating_add(character.len_utf8()) > MAX_PASTE_BYTES {
                    break;
                }
                clean.push(character);
                continue;
            }
        };
        if clean.len().saturating_add(fragment.len()) > MAX_PASTE_BYTES {
            break;
        }
        clean.push_str(fragment);
    }
    clean
}

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
        description: "send: start a run, or queue a follow-up for the active one",
        mouse: None,
    },
    KeyBinding {
        keys: "Shift-Enter",
        description: "insert a line break (modern terminals); Alt-Enter also works",
        mouse: None,
    },
    KeyBinding {
        keys: "/",
        description: "command palette — every command, searchable",
        mouse: None,
    },
    KeyBinding {
        keys: "PgUp / PgDn",
        description: "page the focused pane (or scroll the conversation)",
        mouse: Some("wheel"),
    },
    KeyBinding {
        keys: "Ctrl-↑ / Ctrl-↓",
        description: "switch to the previous / next run",
        mouse: None,
    },
    KeyBinding {
        keys: "F7 / F8 / F9",
        description: "steer the selected run · pause / resume · interrupt with confirmation",
        mouse: Some("click a run-control chip"),
    },
    KeyBinding {
        keys: "F2",
        description: "toggle layout: chat ⇄ workspace panes",
        mouse: None,
    },
    KeyBinding {
        keys: "a / A / p / P",
        description: "approve once / for the run / this command pattern / this repository",
        mouse: None,
    },
    KeyBinding {
        keys: "r",
        description: "reject the pending action",
        mouse: None,
    },
    KeyBinding {
        keys: "1-9 · Space · Enter · r",
        description: "question card: pick an option · toggle · confirm · reject",
        mouse: None,
    },
    KeyBinding {
        keys: "Esc",
        description: "clear the draft (Ctrl-Z restores it), or close an overlay",
        mouse: None,
    },
    KeyBinding {
        keys: "? (empty) / F1",
        description: "show / hide this help overlay",
        mouse: None,
    },
    KeyBinding {
        keys: "S / M / J / D / G",
        description: "open Skills · Memory · Journey · Docs · code-graph Edges",
        mouse: None,
    },
    KeyBinding {
        keys: "↑ / ↓",
        description: "move selection in a browser, palette, or workspace side pane",
        mouse: Some("wheel"),
    },
    KeyBinding {
        keys: "Tab / e / a / r",
        description: "Docs: switch rail · edit block · accept / reject suggestion",
        mouse: None,
    },
    KeyBinding {
        keys: "Ctrl-C",
        description: "detach (the run keeps going)",
        mouse: None,
    },
    KeyBinding {
        keys: "↑↓ + Enter",
        description: "activate the selected row in a browser or the palette",
        mouse: Some("click a row"),
    },
    KeyBinding {
        keys: "Tab",
        description: "focus the next workspace pane (same as clicking it)",
        mouse: Some("click a pane"),
    },
    KeyBinding {
        keys: "↑ / ↓ (composer)",
        description: "recall the previous / next composer message",
        mouse: None,
    },
    KeyBinding {
        keys: "Ctrl-←/→ · Alt-B/F · Ctrl-A/E",
        description: "move the cursor by word · to line start / end (composer)",
        mouse: None,
    },
    KeyBinding {
        keys: "Ctrl-W / Alt-⌫ · Ctrl-U/K",
        description: "delete word / to line start (any prompt) · to line end (composer)",
        mouse: None,
    },
    KeyBinding {
        keys: "Ctrl-R / Ctrl-G",
        description: "incremental history search: older / newer matches",
        mouse: None,
    },
    KeyBinding {
        keys: "Ctrl-F",
        description: "find in the conversation: messages, tool cards, notes, patches",
        mouse: Some("click a result row"),
    },
    KeyBinding {
        keys: "! cmd · # note",
        description: "composer prefixes: run a shell command · remember a note",
        mouse: None,
    },
    KeyBinding {
        keys: "Esc Esc",
        description: "on an empty composer: rewind / fork from an earlier turn",
        mouse: None,
    },
    KeyBinding {
        keys: "Alt-↑ / Alt-↓",
        description: "browse transcript folds: tool cards, diffs, long notes",
        mouse: Some("click a fold line"),
    },
    KeyBinding {
        keys: "Alt-Y · Shift-drag",
        description: "copy the browsed card · native terminal selection / copy",
        mouse: None,
    },
    KeyBinding {
        keys: "Alt-N / Alt-P",
        description: "next / previous hunk in the browsed patch preview",
        mouse: None,
    },
    KeyBinding {
        keys: "Alt-O",
        description: "open the reviewed file / hunk in the same session's IDE",
        mouse: None,
    },
    KeyBinding {
        keys: "Alt-Shift-N / P",
        description: "next / previous changed file in the browsed patch preview",
        mouse: None,
    },
    KeyBinding {
        keys: "Alt-R / A / M / D",
        description:
            "on a failed run: retry · re-authenticate · choose another model · disable the model",
        mouse: Some("the chips under the failure card"),
    },
    KeyBinding {
        keys: "Ctrl-Z",
        description: "restore the draft that Esc just cleared",
        mouse: None,
    },
    KeyBinding {
        keys: "@",
        description: "at the start of a word: mention a file (↑/↓ pick, Enter insert)",
        mouse: Some("a row in the file popup"),
    },
    KeyBinding {
        keys: "q",
        description: "close the open view (Ctrl-C is the key that detaches)",
        mouse: Some("click outside the view"),
    },
    KeyBinding {
        keys: "Alt-Enter",
        description: "expand / collapse the browsed fold, else insert a line break",
        mouse: Some("click a fold line"),
    },
    KeyBinding {
        keys: "F4 (default)",
        description: "push to talk; press again to stop and send the voice note",
        mouse: None,
    },
    KeyBinding {
        keys: "Delete / Ctrl-D",
        description: "remove a configured model/key, or dismiss the selected diagnostic",
        mouse: None,
    },
    KeyBinding {
        keys: "W · n / p / r / c",
        description: "executable persisted workflow: open · run/create · pause · retry · cancel",
        mouse: Some("click a workflow control"),
    },
    KeyBinding {
        keys: "B · n (Blackboard)",
        description: "open the workflow evidence/decision/artifact stream · post a question",
        mouse: Some("click post question"),
    },
    KeyBinding {
        keys: "K · n · ← / → (Kanban)",
        description: "open the repository task board · create a task · move its column",
        mouse: Some("click create or a move control"),
    },
    KeyBinding {
        keys: "P (Docs)",
        description: "publish the focused document through approval",
        mouse: None,
    },
    KeyBinding {
        keys: "/ · PgUp/PgDn (Graph)",
        description: "search graph edges · previous/next result page",
        mouse: None,
    },
    KeyBinding {
        keys: "F6 / Shift-F6 / Esc",
        description: "enter Remote UI · next extension document · return to composer",
        mouse: Some("click extension chrome"),
    },
    KeyBinding {
        keys: "C",
        description: "agent council browser: list, run, and manage persisted councils",
        mouse: None,
    },
    KeyBinding {
        keys: "n / r / d (Council)",
        description: "new council · run deliberation (prompts for objective) · delete",
        mouse: None,
    },
    KeyBinding {
        keys: "K · ← / → (Board)",
        description: "open the task board · move the focused card between columns",
        mouse: Some("click a card or a move chip"),
    },
];

/// Resolve a left click at `(col,row)` to the topmost registered rect's Action.
/// Iterates in reverse so the last-registered (top-of-z-order) rect wins.
#[must_use]
pub fn hit_test(hit_map: &[(Rect, Action)], col: u16, row: u16) -> Option<Action> {
    hit_map
        .iter()
        .rev()
        .find(|(r, _)| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height)
        .map(|(_, action)| action.clone())
}

/// Translate a terminal event into a semantic [`Action`].
///
/// `mode` decides whether printable keys are text or navigation; `width` is the
/// current terminal width (unused by the single-column shell, kept for the mouse
/// signature); `hit_map` is the render-time geometry cache ([`hit_test`]) a left
/// click resolves against. The mapping is total — anything unrecognized maps to
/// [`Action::NoOp`].
#[must_use]
pub fn map_event(event: &Event, mode: InputMode, width: u16, hit_map: &[(Rect, Action)]) -> Action {
    match event {
        Event::Key(key) => map_key(key, mode),
        Event::Mouse(mouse) => map_mouse(mouse, mode, width, hit_map),
        // Bracketed paste lands in whichever text buffer is capturing: the
        // composer, a prompt, or the palette filter.
        Event::Paste(text) if mode == InputMode::RemoteUi => {
            Action::RemoteUiPaste(sanitized_paste(text))
        }
        Event::Paste(text)
            if matches!(
                mode,
                InputMode::Editing | InputMode::Composer | InputMode::Palette | InputMode::Help
            ) =>
        {
            Action::InputPaste(sanitized_paste(text))
        }
        Event::Resize(width, height) => Action::RemoteUiViewport {
            width: *width,
            height: *height,
        },
        Event::FocusGained => Action::TerminalFocus(true),
        Event::FocusLost => Action::TerminalFocus(false),
        Event::Paste(_) => Action::NoOp,
    }
}

fn map_key(key: &KeyEvent, mode: InputMode) -> Action {
    // Ignore key-release events (some terminals report them; acting would
    // double-fire every command).
    if key.kind == KeyEventKind::Release {
        return Action::NoOp;
    }
    // Live run controls remain reachable while the composer owns printable
    // keys. Prompt and decision modes retain their own keys; navigable browser
    // modes keep the reducer's existing context-specific run-control actions.
    if matches!(mode, InputMode::Composer | InputMode::Normal) {
        match key.code {
            KeyCode::F(7) => return Action::Steer,
            KeyCode::F(8) => return Action::Pause,
            KeyCode::F(9) => return Action::Cancel,
            _ => {}
        }
    }
    if matches!(mode, InputMode::Composer | InputMode::Normal) && alt(key) {
        if key.code == KeyCode::Char('o') {
            return Action::OpenReviewedSource;
        }
        if let KeyCode::Char(c @ ('n' | 'p' | 'N' | 'P')) = key.code {
            return Action::NavigatePatch {
                forward: matches!(c, 'n' | 'N'),
                files: c.is_uppercase() || key.modifiers.contains(KeyModifiers::SHIFT),
            };
        }
    }
    match mode {
        InputMode::Editing => map_editing_key(key),
        InputMode::Confirm => map_confirm_key(key),
        InputMode::Palette => map_palette_key(key),
        InputMode::Help => map_help_key(key),
        InputMode::Composer => map_composer_key(key),
        InputMode::Approval => map_approval_key(key),
        InputMode::Question => map_question_key(key),
        InputMode::CouncilResults => map_normal_key(key),
        InputMode::RemoteUi => map_remote_ui_key(key),
        InputMode::Normal => map_normal_key(key),
    }
}

fn map_remote_ui_key(key: &KeyEvent) -> Action {
    if ctrl(key) && key.code == KeyCode::Char('c') {
        return Action::Detach;
    }
    let (key, character) = match key.code {
        KeyCode::Esc => return Action::RemoteUiSetActive(false),
        KeyCode::F(6) if key.modifiers.contains(KeyModifiers::SHIFT) => {
            return Action::RemoteUiNextDocument;
        }
        KeyCode::F(6) => return Action::RemoteUiSetActive(true),
        KeyCode::Tab => (RemoteKey::Tab, None),
        KeyCode::BackTab => (RemoteKey::ShiftTab, None),
        KeyCode::Enter => (RemoteKey::Enter, None),
        KeyCode::Up => (RemoteKey::Up, None),
        KeyCode::Down => (RemoteKey::Down, None),
        KeyCode::Left => (RemoteKey::Left, None),
        KeyCode::Right => (RemoteKey::Right, None),
        KeyCode::Home => (RemoteKey::Home, None),
        KeyCode::End => (RemoteKey::End, None),
        KeyCode::PageUp => (RemoteKey::PageUp, None),
        KeyCode::PageDown => (RemoteKey::PageDown, None),
        KeyCode::Backspace => (RemoteKey::Backspace, None),
        KeyCode::Delete => (RemoteKey::Delete, None),
        KeyCode::Char(' ') => (RemoteKey::Space, Some(' ')),
        KeyCode::Char(character) if !ctrl(key) => (RemoteKey::Character, Some(character)),
        _ => return Action::NoOp,
    };
    Action::RemoteUiKey { key, character }
}

fn ctrl(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
}

fn alt(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::ALT)
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
        // The task board's column moves (rubric 10). Horizontal arrows had no
        // meaning in a navigable overlay before, and the reducer ignores them
        // outside the board, so this costs no existing binding.
        KeyCode::Left => Action::MoveCardBack,
        KeyCode::Right => Action::MoveCardForward,
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        // Jump to the edges of the focused browser list, exactly as the
        // palette-mode pickers already do. Previously dead keys here.
        KeyCode::Home => Action::SelectFirst,
        KeyCode::End => Action::SelectLast,
        KeyCode::Delete => Action::ClearIssues,
        KeyCode::Esc => Action::Dismiss,
        KeyCode::F(1) => Action::Help,
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
        // `q` closes the open view. It used to DETACH — exit the whole client
        // — from inside every browser overlay, which is the one key every
        // other TUI uses for "close this". Ctrl-C is the detach key.
        'q' => Action::Dismiss,
        '?' => Action::Help,
        'a' => Action::Approve(ApprovalScope::Once),
        'A' => Action::Approve(ApprovalScope::Run),
        'r' => Action::Reject,
        'S' => Action::OpenSkills,
        'M' => Action::OpenMemory,
        'J' => Action::OpenJourney,
        'o' => Action::OpenSource,
        'e' => Action::EditDoc,
        'i' => Action::InsertDocBlock,
        'P' => Action::PublishDoc,
        'D' => Action::OpenDocs,
        'G' => Action::OpenEdges,
        'W' => Action::OpenWorkflow,
        'B' => Action::OpenBlackboard,
        'C' => Action::OpenCouncils,
        'K' => Action::OpenKanban,
        // Host-owned Remote UI plugin lifecycle controls. They are meaningful
        // only while the `/plugins` surface is open; the reducer ignores them
        // elsewhere.
        't' => Action::EnableUiPluginSession,
        'u' => Action::EnableUiPluginUser,
        'x' => Action::RevokeUiPlugin,
        // Council browser controls (rubric 6). `n`/`r` reuse `NewRun`/`Reject`
        // above (the reducer dispatches on the open overlay, like Workflow's
        // n/p/r/c); `d` is meaningful only while `/council` is open — the
        // reducer ignores it elsewhere.
        'd' => Action::DeleteCouncil,
        // Result workbench: copy the exact focused chair synthesis. The
        // reducer scopes this to that overlay, so `y` is inert elsewhere.
        'y' => Action::CopyFocusedCard,
        'X' => Action::DeleteDocBlock,
        '/' => Action::OpenPalette,
        _ => Action::NoOp,
    }
}

fn map_editing_key(key: &KeyEvent) -> Action {
    match key.code {
        // Modern terminals distinguish Shift+Enter; Alt+Enter also works in
        // terminals that send the same sequence for Enter and Shift+Enter.
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => Action::InsertNewline,
        KeyCode::Enter if alt(key) => Action::InputNewline,
        KeyCode::Enter => Action::InputSubmit,
        KeyCode::Esc => Action::InputCancel,
        KeyCode::Backspace if alt(key) || ctrl(key) => Action::DeleteWordBack,
        KeyCode::Backspace => Action::InputBackspace,
        // In a multi-step text wizard, Tab is an ergonomic synonym for the
        // visible Continue action. Other editing prompts ignore this reducer
        // action, so their behavior is unchanged.
        KeyCode::Tab => Action::BeginAddModel,
        KeyCode::Char('c') if ctrl(key) => Action::InputCancel,
        // The kill keys work in every prompt, not only the composer: a typo'd
        // word in a rename or an objective otherwise costs one Backspace per
        // character. Append-only prompts kill at the end of the buffer; the
        // prefilled editors (rename / learning / doc block) kill at their
        // cursor.
        KeyCode::Char('w') if ctrl(key) => Action::DeleteWordBack,
        KeyCode::Char('u') if ctrl(key) => Action::DeleteToLineStart,
        KeyCode::Char('k') if ctrl(key) => Action::DeleteToLineEnd,
        // Caret motion. Only the prefilled editors honour these (the reducer
        // ignores them in append-only prompts — the pinned contract); mapping
        // them here is what makes those three real editors instead of
        // one-Backspace-per-character.
        KeyCode::Left if ctrl(key) || alt(key) => Action::CursorWordLeft,
        KeyCode::Right if ctrl(key) || alt(key) => Action::CursorWordRight,
        KeyCode::Char('b') if alt(key) => Action::CursorWordLeft,
        KeyCode::Char('f') if alt(key) => Action::CursorWordRight,
        KeyCode::Left => Action::CursorLeft,
        KeyCode::Right => Action::CursorRight,
        KeyCode::Home => Action::CursorLineStart,
        KeyCode::End => Action::CursorLineEnd,
        KeyCode::Delete => Action::DeleteForward,
        KeyCode::Char('d') if ctrl(key) => Action::DeleteForward,
        KeyCode::Char('a') if ctrl(key) => Action::CursorLineStart,
        KeyCode::Char('e') if ctrl(key) => Action::CursorLineEnd,
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
        KeyCode::PageUp => Action::SelectPagePrev,
        KeyCode::PageDown => Action::SelectPageNext,
        KeyCode::Home => Action::SelectFirst,
        KeyCode::End => Action::SelectLast,
        KeyCode::Delete => Action::RemoveSelected,
        KeyCode::Char('c') if ctrl(key) => Action::InputCancel,
        // Ctrl-chords stay out of the query buffer: `Ctrl-T` tests the focused
        // `/keys` row's key, `Ctrl-R` re-fetches an open add-model pick-list.
        // Both are no-ops in every other palette-mode overlay.
        KeyCode::Char('t') if ctrl(key) => Action::VerifyApiKey,
        KeyCode::Char('r') if ctrl(key) => Action::RefreshProviderModels,
        KeyCode::Char('d') if ctrl(key) => Action::RemoveSelected,
        // The same kill keys the composer and prompts honour: clearing a
        // mistyped filter word-by-word (or wholesale) beats holding Backspace.
        KeyCode::Char('w') if ctrl(key) => Action::DeleteWordBack,
        KeyCode::Char('u') if ctrl(key) => Action::DeleteToLineStart,
        // Alt-chords carry the Session Library's lifecycle verbs. They stay out
        // of the query buffer (an unmodified `p` must still filter), and the
        // reducer ignores them in every other palette-mode overlay.
        KeyCode::Char('p') if alt(key) => Action::SessionLibraryTogglePin,
        KeyCode::Char('a') if alt(key) => Action::SessionLibraryToggleArchive,
        KeyCode::Char('r') if alt(key) => Action::SessionLibraryBeginRename,
        KeyCode::Char('e') if alt(key) => Action::SessionLibraryExport,
        KeyCode::Char(c) if !ctrl(key) => Action::InputChar(c),
        KeyCode::Tab => Action::BeginAddModel,
        _ => Action::NoOp,
    }
}

fn map_help_key(key: &KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('?') | KeyCode::F(1) => Action::Help,
        KeyCode::Char('c') if ctrl(key) => Action::Detach,
        KeyCode::Char('u') if ctrl(key) => Action::DeleteToLineStart,
        KeyCode::Char('w') if ctrl(key) => Action::DeleteWordBack,
        KeyCode::Char(c) if !ctrl(key) && !alt(key) => Action::InputChar(c),
        KeyCode::Backspace => Action::InputBackspace,
        KeyCode::Up => Action::ScrollLinesUp,
        KeyCode::Down => Action::ScrollLinesDown,
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        KeyCode::Home => Action::SelectFirst,
        KeyCode::End => Action::SelectLast,
        _ => Action::NoOp,
    }
}

pub(crate) fn filtered_key_bindings(query: &str) -> Vec<&'static KeyBinding> {
    let query = query.to_lowercase();
    let words: Vec<_> = query.split_whitespace().collect();
    KEY_BINDINGS
        .iter()
        .filter(|binding| {
            let searchable = format!(
                "{} {} {}",
                binding.keys,
                binding.description,
                binding.mouse.unwrap_or("")
            )
            .to_lowercase();
            words.iter().all(|word| searchable.contains(word))
        })
        .collect()
}

/// The base conversation view. The composer captures typed text; Enter sends it;
/// `/` is a literal character (the reducer opens the palette only when it lands on
/// an empty composer); PgUp/PgDn scroll the transcript; Ctrl-↑/↓ switch runs;
/// Ctrl-C detaches; Esc clears the draft.
fn map_composer_key(key: &KeyEvent) -> Action {
    match key.code {
        // Alt+Enter expands the browsed transcript fold when `Alt-↑`/`Alt-↓`
        // put one under the cursor, and otherwise inserts a manual line break
        // — the reducer owns that choice because only it knows whether the
        // transcript is being browsed (this mapper is pure). Plain Enter
        // always submits.
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => Action::InsertNewline,
        KeyCode::Enter if alt(key) => Action::InputNewline,
        KeyCode::Enter => Action::InputSubmit,
        KeyCode::Esc => Action::InputCancel,
        KeyCode::Backspace if alt(key) || ctrl(key) => Action::DeleteWordBack,
        KeyCode::Backspace => Action::InputBackspace,
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        // Tab focuses the next workspace pane (the keyboard equivalent of
        // clicking one — RULE 3); it was dead here before, which made the
        // advertised "Tab — focus a pane" binding a lie in the base view.
        KeyCode::Tab => Action::CyclePane,
        // Ctrl-↑/↓ switch runs; Alt-↑/↓ walk the transcript's folds (tool
        // cards, diffs, notes); plain ↑/↓ recall composer history, shell-style.
        KeyCode::Up if ctrl(key) => Action::PrevRun,
        KeyCode::Down if ctrl(key) => Action::NextRun,
        KeyCode::Up if alt(key) => Action::BrowseFoldPrev,
        KeyCode::Down if alt(key) => Action::BrowseFoldNext,
        // Word-wise motion. Both Ctrl and Alt chords map: terminals disagree
        // about which modifier they pass through for arrow words (many macOS
        // terminals send Alt, most Linux ones Ctrl), and neither chord had a
        // meaning here before.
        KeyCode::Left if ctrl(key) || alt(key) => Action::CursorWordLeft,
        KeyCode::Right if ctrl(key) || alt(key) => Action::CursorWordRight,
        KeyCode::Char('b') if alt(key) => Action::CursorWordLeft,
        KeyCode::Char('f') if alt(key) => Action::CursorWordRight,
        KeyCode::Char('y') if alt(key) => Action::CopyFocusedCard,
        KeyCode::Char('r') if alt(key) => Action::RetryFailedRun,
        KeyCode::Char('a') if alt(key) => Action::ReauthenticateFailedModel,
        KeyCode::Char('m') if alt(key) => Action::ChooseFailureModel,
        KeyCode::Char('d') if alt(key) => Action::DisableFailureModel,
        // ↑/↓ move between the draft's own lines first and only recall history
        // at the draft's top/bottom edge (see `reduce::composer_up`).
        KeyCode::Up => Action::HistoryPrev,
        KeyCode::Down => Action::HistoryNext,
        // Cursor editing: the draft is a real text field, not an append-only
        // buffer.
        KeyCode::Left => Action::CursorLeft,
        KeyCode::Right => Action::CursorRight,
        KeyCode::Home => Action::CursorLineStart,
        KeyCode::End => Action::CursorLineEnd,
        KeyCode::Char('w') if ctrl(key) => Action::DeleteWordBack,
        KeyCode::Char('u') if ctrl(key) => Action::DeleteToLineStart,
        // `Ctrl-Z` restores the draft the last `Esc` cleared (swap, so a
        // second press swaps back). Terminals in raw mode never see the
        // shell's SIGTSTP meaning, so the chord is free here.
        KeyCode::Char('z') if ctrl(key) => Action::DraftRestore,
        // The readline line-motion and kill-to-end trio. `Ctrl-A`/`Ctrl-E`
        // are synonyms for `Home`/`End` (shell muscle memory); `Ctrl-K`
        // deletes from the cursor to the end of its line.
        KeyCode::Char('a') if ctrl(key) => Action::CursorLineStart,
        KeyCode::Char('e') if ctrl(key) => Action::CursorLineEnd,
        KeyCode::Char('k') if ctrl(key) => Action::DeleteToLineEnd,
        KeyCode::Char('r') if ctrl(key) => Action::HistorySearchPrev,
        // `Ctrl-F` finds in the CONVERSATION (every entry of every run);
        // `Ctrl-R` searches the prompt HISTORY. Two different questions.
        KeyCode::Char('f') if ctrl(key) => Action::TranscriptSearchOpen,
        // `Ctrl-G` is the primary forward-search chord: `Ctrl-S` (kept as an
        // alias) is XOFF on any terminal that has not run `stty -ixon`, where
        // it freezes output instead of reaching the application.
        KeyCode::Char('g') if ctrl(key) => Action::HistorySearchNext,
        KeyCode::Char('s') if ctrl(key) => Action::HistorySearchNext,
        KeyCode::Delete => Action::DeleteSelectedPrompt,
        KeyCode::Char('d') if ctrl(key) => Action::DeleteForward,
        // `F1` opens help from the base view, where `?` is ordinary text
        // (the reducer additionally opens it for `?` on an EMPTY composer,
        // mirroring the `/` palette rule).
        KeyCode::F(1) => Action::Help,
        KeyCode::F(2) => Action::ToggleLayout,
        KeyCode::F(6) if key.modifiers.contains(KeyModifiers::SHIFT) => {
            Action::RemoteUiNextDocument
        }
        KeyCode::F(6) => Action::RemoteUiSetActive(true),
        KeyCode::Char('c') if ctrl(key) => Action::Detach,
        KeyCode::Char(c) if !ctrl(key) => Action::InputChar(c),
        _ => Action::NoOp,
    }
}

/// A pending approval owns the input: the decision keys, plus arrows to move
/// between stacked approvals. PgUp/PgDn page the modal BODY, not the stack —
/// `describe_action` is deliberately unbounded (every env binding, verbatim),
/// so an oversized action scrolls rather than clips; the wheel still walks
/// the stack, matching ↑/↓. Ctrl-C still detaches (the run keeps going);
/// `F2` still flips the layout underneath.
fn map_approval_key(key: &KeyEvent) -> Action {
    if ctrl(key) && key.code == KeyCode::Char('c') {
        return Action::Detach;
    }
    match key.code {
        KeyCode::Char('a') => Action::Approve(ApprovalScope::Once),
        KeyCode::Char('A') => Action::Approve(ApprovalScope::Run),
        KeyCode::Char('p') => Action::Approve(ApprovalScope::Pattern),
        KeyCode::Char('P') => Action::Approve(ApprovalScope::Repository),
        KeyCode::Char('r') => Action::Reject,
        KeyCode::Up => Action::SelectPrev,
        KeyCode::Down => Action::SelectNext,
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        KeyCode::F(2) => Action::ToggleLayout,
        _ => Action::NoOp,
    }
}

/// A pending question prompt owns the input: options selection, custom typing,
/// reject feedback. Ctrl-C still detaches; F2 flips layout.
fn map_question_key(key: &KeyEvent) -> Action {
    if ctrl(key) && key.code == KeyCode::Char('c') {
        return Action::Detach;
    }
    match key.code {
        KeyCode::Up => Action::QuestionNavigate(-1),
        KeyCode::Down => Action::QuestionNavigate(1),
        // PgUp/PgDn page the modal BODY (the approval modal's shape): a
        // prompt with more options than the card is tall scrolls instead of
        // hiding them.
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        KeyCode::Char(c @ '1'..='9') if !ctrl(key) && !alt(key) => {
            let digit = (c as u8 - b'0') as usize;
            Action::QuestionPickDigit(digit)
        }
        KeyCode::Char(' ') if !ctrl(key) && !alt(key) => Action::QuestionToggleOption,
        KeyCode::Enter => Action::QuestionSelectOrConfirm,
        KeyCode::Esc => Action::QuestionCancelReject,
        KeyCode::Backspace => Action::QuestionInputBackspace,
        KeyCode::Char('r') if !ctrl(key) && !alt(key) => Action::QuestionOpenReject,
        // `R` is ordinary text: the reducer only inserts it when a text row
        // (custom answer / feedback) owns input, so an answer beginning with
        // an uppercase R is typeable — `r` alone carries the reject shortcut.
        KeyCode::Char(c) if !ctrl(key) => Action::QuestionInputChar(c),
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

fn map_mouse(
    mouse: &MouseEvent,
    mode: InputMode,
    _width: u16,
    hit_map: &[(Rect, Action)],
) -> Action {
    // Mouse tracking makes rows clickable, but Shift is the conventional
    // terminal-native selection modifier. Never turn a Shift press/drag into
    // an application action; supported terminals intercept it for ordinary
    // selection/copy, and the NoOp fallback remains safe elsewhere.
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        return Action::NoOp;
    }
    match mode {
        // A text prompt / confirm / question captures nothing from the mouse.
        InputMode::Editing | InputMode::Confirm | InputMode::Question => Action::NoOp,
        InputMode::RemoteUi => match mouse.kind {
            MouseEventKind::ScrollUp => Action::RemoteUiKey {
                key: RemoteKey::PageUp,
                character: None,
            },
            MouseEventKind::ScrollDown => Action::RemoteUiKey {
                key: RemoteKey::PageDown,
                character: None,
            },
            MouseEventKind::Down(MouseButton::Left) => {
                hit_test(hit_map, mouse.column, mouse.row).unwrap_or(Action::NoOp)
            }
            _ => Action::NoOp,
        },
        // The conversation scrolls its transcript on the wheel; a left click
        // resolves through the hit-test map (a registered fold line, footer chip,
        // etc.), falling back to inert when nothing is registered there.
        InputMode::Composer => match mouse.kind {
            // A notch is a few lines, the conventional wheel granularity —
            // `PgUp`/`PgDn` remain the page-sized keyboard equivalent.
            MouseEventKind::ScrollUp => Action::ScrollLinesUp,
            MouseEventKind::ScrollDown => Action::ScrollLinesDown,
            MouseEventKind::Down(MouseButton::Left) => {
                hit_test(hit_map, mouse.column, mouse.row).unwrap_or(Action::NoOp)
            }
            _ => Action::NoOp,
        },
        // List surfaces — browsers, the palette, stacked approvals — move their
        // selection on the wheel. A left click resolves through the same
        // hit-test map (a registered row), falling back to inert otherwise.
        InputMode::CouncilResults | InputMode::Help => match mouse.kind {
            MouseEventKind::ScrollUp => Action::ScrollLinesUp,
            MouseEventKind::ScrollDown => Action::ScrollLinesDown,
            MouseEventKind::Down(MouseButton::Left) => {
                hit_test(hit_map, mouse.column, mouse.row).unwrap_or(Action::NoOp)
            }
            _ => Action::NoOp,
        },
        InputMode::Normal | InputMode::Palette | InputMode::Approval => match mouse.kind {
            MouseEventKind::ScrollUp => Action::SelectPrev,
            MouseEventKind::ScrollDown => Action::SelectNext,
            MouseEventKind::Down(MouseButton::Left) => {
                hit_test(hit_map, mouse.column, mouse.row).unwrap_or(Action::NoOp)
            }
            _ => Action::NoOp,
        },
    }
}

/// Resolve which pane a column falls in, using the same 26 / 48 / 26 split
/// `render_workspace` lays out (see [`crate::render`]). The doc comment and
/// the arithmetic both used to say 30 / 40 / 30, which had drifted from the
/// renderer — a click near a pane seam resolved to the neighbour.
///
/// The live mouse path resolves clicks through the renderer's own hit map, so
/// this is the geometry answer for callers outside a frame (and the assertion
/// that the two splits agree).
#[must_use]
pub fn pane_at(column: u16, width: u16) -> Pane {
    let left = width * 26 / 100;
    let right_start = width.saturating_sub(width * 26 / 100);
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
            map_event(&key(KeyCode::Tab), InputMode::Normal, W, &[]),
            Action::CyclePane
        );
        assert_eq!(
            map_event(&ch('n'), InputMode::Normal, W, &[]),
            Action::NewRun
        );
        assert_eq!(
            map_event(&ch('p'), InputMode::Normal, W, &[]),
            Action::Pause
        );
        assert_eq!(
            map_event(&ch('c'), InputMode::Normal, W, &[]),
            Action::Cancel
        );
        assert_eq!(
            map_event(&ch('s'), InputMode::Normal, W, &[]),
            Action::Steer
        );
        assert_eq!(
            map_event(&ch('q'), InputMode::Normal, W, &[]),
            Action::Dismiss
        );
        assert_eq!(map_event(&ch('?'), InputMode::Normal, W, &[]), Action::Help);
        assert_eq!(
            map_event(&key(KeyCode::Enter), InputMode::Normal, W, &[]),
            Action::Expand
        );
        assert_eq!(
            map_event(&ch('a'), InputMode::Normal, W, &[]),
            Action::Approve(ApprovalScope::Once)
        );
        assert_eq!(
            map_event(&ch('A'), InputMode::Normal, W, &[]),
            Action::Approve(ApprovalScope::Run)
        );
        assert_eq!(
            map_event(&ch('r'), InputMode::Normal, W, &[]),
            Action::Reject
        );
        assert_eq!(
            map_event(&ch('S'), InputMode::Normal, W, &[]),
            Action::OpenSkills
        );
        assert_eq!(
            map_event(&ch('M'), InputMode::Normal, W, &[]),
            Action::OpenMemory
        );
        assert_eq!(
            map_event(&ch('o'), InputMode::Normal, W, &[]),
            Action::OpenSource
        );
        assert_eq!(
            map_event(&ch('e'), InputMode::Normal, W, &[]),
            Action::EditDoc
        );
        assert_eq!(
            map_event(&ch('i'), InputMode::Normal, W, &[]),
            Action::InsertDocBlock
        );
        assert_eq!(
            map_event(&ch('X'), InputMode::Normal, W, &[]),
            Action::DeleteDocBlock
        );
        assert_eq!(
            map_event(&ch('D'), InputMode::Normal, W, &[]),
            Action::OpenDocs
        );
        assert_eq!(
            map_event(&ch('G'), InputMode::Normal, W, &[]),
            Action::OpenEdges
        );
        assert_eq!(
            map_event(&ch('W'), InputMode::Normal, W, &[]),
            Action::OpenWorkflow
        );
        assert_eq!(
            map_event(&ch('B'), InputMode::Normal, W, &[]),
            Action::OpenBlackboard
        );
        assert_eq!(
            map_event(&ch('/'), InputMode::Normal, W, &[]),
            Action::OpenPalette
        );
    }

    #[test]
    fn palette_mode_filters_but_stays_navigable() {
        // Printable keys become the filter query...
        assert_eq!(
            map_event(&ch('d'), InputMode::Palette, W, &[]),
            Action::InputChar('d')
        );
        // ...while arrows still move the selection and Enter runs it.
        assert_eq!(
            map_event(&key(KeyCode::Up), InputMode::Palette, W, &[]),
            Action::SelectPrev
        );
        assert_eq!(
            map_event(&key(KeyCode::Down), InputMode::Palette, W, &[]),
            Action::SelectNext
        );
        assert_eq!(
            map_event(&key(KeyCode::Enter), InputMode::Palette, W, &[]),
            Action::InputSubmit
        );
        assert_eq!(
            map_event(&key(KeyCode::Esc), InputMode::Palette, W, &[]),
            Action::InputCancel
        );
        assert_eq!(
            map_event(&key(KeyCode::PageDown), InputMode::Palette, W, &[]),
            Action::SelectPageNext
        );
        assert_eq!(
            map_event(&key(KeyCode::PageUp), InputMode::Palette, W, &[]),
            Action::SelectPagePrev
        );
        assert_eq!(
            map_event(&key(KeyCode::Home), InputMode::Palette, W, &[]),
            Action::SelectFirst
        );
        assert_eq!(
            map_event(&key(KeyCode::End), InputMode::Palette, W, &[]),
            Action::SelectLast
        );
        assert_eq!(
            map_event(&key(KeyCode::Delete), InputMode::Palette, W, &[]),
            Action::RemoveSelected
        );
        assert_eq!(
            map_event(&ctrl(KeyCode::Char('d')), InputMode::Palette, W, &[]),
            Action::RemoveSelected
        );
    }

    #[test]
    fn tab_in_palette_mode_begins_add_model() {
        assert_eq!(
            map_event(&key(KeyCode::Tab), InputMode::Palette, W, &[]),
            Action::BeginAddModel
        );
    }

    #[test]
    fn editing_mode_routes_text_not_commands() {
        // In a prompt, 'n' is text, not "new run".
        assert_eq!(
            map_event(&ch('n'), InputMode::Editing, W, &[]),
            Action::InputChar('n')
        );
        assert_eq!(
            map_event(&key(KeyCode::Enter), InputMode::Editing, W, &[]),
            Action::InputSubmit
        );
        assert_eq!(
            map_event(&key(KeyCode::Esc), InputMode::Editing, W, &[]),
            Action::InputCancel
        );
        assert_eq!(
            map_event(&key(KeyCode::Backspace), InputMode::Editing, W, &[]),
            Action::InputBackspace
        );
        assert_eq!(
            map_event(&key(KeyCode::Tab), InputMode::Editing, W, &[]),
            Action::BeginAddModel
        );
        assert_eq!(
            map_event(
                &Event::Paste("hello".to_owned()),
                InputMode::Editing,
                W,
                &[]
            ),
            Action::InputPaste("hello".to_owned())
        );
    }

    #[test]
    fn clipboard_paste_is_multiline_sanitized_and_bounded() {
        assert_eq!(
            map_event(
                &Event::Paste("first\r\nsecond\t\u{1b}[31m\u{202e}ok".to_owned()),
                InputMode::Composer,
                W,
                &[],
            ),
            Action::InputPaste("first\nsecond    [31mok".to_owned())
        );

        let oversized = format!("{}🚀tail", "x".repeat(MAX_PASTE_BYTES + 32));
        let Action::InputPaste(pasted) =
            map_event(&Event::Paste(oversized), InputMode::Composer, W, &[])
        else {
            panic!("composer paste must remain paste input")
        };
        assert!(pasted.len() <= MAX_PASTE_BYTES);
        assert!(std::str::from_utf8(pasted.as_bytes()).is_ok());
    }

    #[test]
    fn shift_mouse_gestures_are_reserved_for_native_text_selection() {
        let map = vec![(Rect::new(0, 0, 40, 10), Action::ActivateRow(3))];
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let event = Event::Mouse(MouseEvent {
                kind,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::SHIFT,
            });
            assert_eq!(
                map_event(&event, InputMode::Composer, W, &map),
                Action::NoOp,
                "Shift selection must never activate transcript rows"
            );
        }
    }

    #[test]
    fn confirm_mode_yes_no() {
        assert_eq!(
            map_event(&ch('y'), InputMode::Confirm, W, &[]),
            Action::ConfirmCancel
        );
        assert_eq!(
            map_event(&key(KeyCode::Enter), InputMode::Confirm, W, &[]),
            Action::ConfirmCancel
        );
        assert_eq!(
            map_event(&ch('n'), InputMode::Confirm, W, &[]),
            Action::Dismiss
        );
        assert_eq!(
            map_event(&key(KeyCode::Esc), InputMode::Confirm, W, &[]),
            Action::Dismiss
        );
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut ev = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        ev.kind = KeyEventKind::Release;
        assert_eq!(
            map_event(&Event::Key(ev), InputMode::Normal, W, &[]),
            Action::NoOp
        );
    }

    #[test]
    fn pane_hit_testing_uses_the_render_split() {
        assert_eq!(pane_at(1, W), Pane::Sessions);
        assert_eq!(pane_at(W / 2, W), Pane::Transcript);
        assert_eq!(pane_at(W - 2, W), Pane::Approvals);

        // The seams are the renderer's 26 / 48 / 26 split, not the 30 / 40 / 30
        // this function's arithmetic and doc comment had drifted to. At 200
        // columns that is a 8-column difference at each seam — enough to
        // resolve a click to the wrong pane.
        let wide = 200_u16;
        assert_eq!(
            pane_at(51, wide),
            Pane::Sessions,
            "26% of 200 is 52 columns"
        );
        assert_eq!(pane_at(52, wide), Pane::Transcript);
        assert_eq!(pane_at(147, wide), Pane::Transcript);
        assert_eq!(pane_at(148, wide), Pane::Approvals);
    }

    fn ctrl(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
    }

    #[test]
    fn composer_mode_captures_text_and_controls() {
        // Printable keys are text — including `/`, which the reducer (not the
        // mapper) turns into a palette-open only on an empty composer.
        assert_eq!(
            map_event(&ch('h'), InputMode::Composer, W, &[]),
            Action::InputChar('h')
        );
        assert_eq!(
            map_event(&ch('/'), InputMode::Composer, W, &[]),
            Action::InputChar('/')
        );
        assert_eq!(
            map_event(&key(KeyCode::Enter), InputMode::Composer, W, &[]),
            Action::InputSubmit
        );
        assert_eq!(
            map_event(&key(KeyCode::Esc), InputMode::Composer, W, &[]),
            Action::InputCancel
        );
        assert_eq!(
            map_event(&key(KeyCode::PageUp), InputMode::Composer, W, &[]),
            Action::ScrollPageUp
        );
        // Ctrl-C detaches rather than typing a 'c'; Ctrl-↑/↓ switch runs.
        assert_eq!(
            map_event(&ctrl(KeyCode::Char('c')), InputMode::Composer, W, &[]),
            Action::Detach
        );
        assert_eq!(
            map_event(&ctrl(KeyCode::Up), InputMode::Composer, W, &[]),
            Action::PrevRun
        );
        assert_eq!(
            map_event(&ctrl(KeyCode::Down), InputMode::Composer, W, &[]),
            Action::NextRun
        );
        // F2 flips the layout from the base view.
        assert_eq!(
            map_event(&key(KeyCode::F(2)), InputMode::Composer, W, &[]),
            Action::ToggleLayout
        );
    }

    #[test]
    fn run_control_shortcuts_work_without_consuming_composer_text() {
        for mode in [InputMode::Composer, InputMode::Normal] {
            assert_eq!(map_event(&key(KeyCode::F(7)), mode, W, &[]), Action::Steer);
            assert_eq!(map_event(&key(KeyCode::F(8)), mode, W, &[]), Action::Pause);
            assert_eq!(map_event(&key(KeyCode::F(9)), mode, W, &[]), Action::Cancel);
            assert_eq!(
                map_event(&ctrl(KeyCode::Char('c')), mode, W, &[]),
                Action::Detach
            );
        }
        for letter in ['p', 'c', 's'] {
            assert_eq!(
                map_event(&ch(letter), InputMode::Composer, W, &[]),
                Action::InputChar(letter)
            );
        }
        assert_eq!(
            map_event(
                &Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT)),
                InputMode::Composer,
                W,
                &[],
            ),
            Action::NavigatePatch {
                forward: false,
                files: false,
            },
            "Alt-P remains previous patch navigation"
        );
    }

    #[test]
    fn run_control_shortcuts_do_not_bypass_modal_input() {
        for mode in [
            InputMode::Editing,
            InputMode::Confirm,
            InputMode::Palette,
            InputMode::Help,
            InputMode::Approval,
            InputMode::Question,
            InputMode::CouncilResults,
            InputMode::RemoteUi,
        ] {
            for function in [7, 8, 9] {
                assert_eq!(
                    map_event(&key(KeyCode::F(function)), mode, W, &[]),
                    Action::NoOp,
                    "F{function} must not bypass {mode:?} input"
                );
            }
        }
        for mode in [InputMode::Composer, InputMode::Normal] {
            for function in [7, 8, 9] {
                let release = Event::Key(KeyEvent {
                    kind: KeyEventKind::Release,
                    ..KeyEvent::new(KeyCode::F(function), KeyModifiers::NONE)
                });
                assert_eq!(
                    map_event(&release, mode, W, &[]),
                    Action::NoOp,
                    "key release must not fire F{function} in {mode:?}"
                );
            }
        }
    }

    #[test]
    fn remote_ui_focus_contract_is_reachable_from_composer_and_component() {
        let f6 = key(KeyCode::F(6));
        let shift_f6 = Event::Key(KeyEvent::new(KeyCode::F(6), KeyModifiers::SHIFT));
        for mode in [InputMode::Composer, InputMode::RemoteUi] {
            assert_eq!(
                map_event(&f6, mode, W, &[]),
                Action::RemoteUiSetActive(true)
            );
            assert_eq!(
                map_event(&shift_f6, mode, W, &[]),
                Action::RemoteUiNextDocument
            );
        }
        assert_eq!(
            map_event(&key(KeyCode::Esc), InputMode::RemoteUi, W, &[]),
            Action::RemoteUiSetActive(false)
        );
    }

    #[test]
    fn alt_enter_inserts_a_newline_instead_of_submitting() {
        let alt_enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(
            map_event(&alt_enter, InputMode::Composer, W, &[]),
            Action::InputNewline
        );
        assert_eq!(
            map_event(&alt_enter, InputMode::Editing, W, &[]),
            Action::InputNewline
        );
        // Plain Enter still submits in both modes.
        assert_eq!(
            map_event(&key(KeyCode::Enter), InputMode::Composer, W, &[]),
            Action::InputSubmit
        );
        assert_eq!(
            map_event(&key(KeyCode::Enter), InputMode::Editing, W, &[]),
            Action::InputSubmit
        );
    }

    #[test]
    fn plain_up_down_recall_composer_history_but_ctrl_still_switches_runs() {
        // Plain ↑/↓ in the composer: history recall (previously unbound).
        assert_eq!(
            map_event(&key(KeyCode::Up), InputMode::Composer, W, &[]),
            Action::HistoryPrev
        );
        assert_eq!(
            map_event(&key(KeyCode::Down), InputMode::Composer, W, &[]),
            Action::HistoryNext
        );
        // Ctrl-↑/↓ is unaffected: still run-switching, not history.
        assert_eq!(
            map_event(&ctrl(KeyCode::Up), InputMode::Composer, W, &[]),
            Action::PrevRun
        );
        assert_eq!(
            map_event(&ctrl(KeyCode::Down), InputMode::Composer, W, &[]),
            Action::NextRun
        );
        // PgUp/PgDn scroll is unaffected.
        assert_eq!(
            map_event(&key(KeyCode::PageUp), InputMode::Composer, W, &[]),
            Action::ScrollPageUp
        );
        assert_eq!(
            map_event(&key(KeyCode::PageDown), InputMode::Composer, W, &[]),
            Action::ScrollPageDown
        );

        assert_eq!(
            map_event(
                &wheel(MouseEventKind::ScrollDown, 10),
                InputMode::CouncilResults,
                W,
                &[]
            ),
            Action::ScrollLinesDown,
            "Council Results owns a scrollable detail pane, not just a result list"
        );
    }

    /// The keyboard path that un-deads tool cards and patch diffs: Alt-↑/↓
    /// browse the transcript's folds, without disturbing the plain and Ctrl
    /// arrow bindings that share those keys.
    #[test]
    fn alt_arrows_browse_transcript_folds_in_the_composer() {
        let alt = |code| Event::Key(KeyEvent::new(code, KeyModifiers::ALT));
        assert_eq!(
            map_event(&alt(KeyCode::Up), InputMode::Composer, W, &[]),
            Action::BrowseFoldPrev
        );
        assert_eq!(
            map_event(&alt(KeyCode::Down), InputMode::Composer, W, &[]),
            Action::BrowseFoldNext
        );
        // The other two arrow bindings are untouched.
        assert_eq!(
            map_event(&key(KeyCode::Up), InputMode::Composer, W, &[]),
            Action::HistoryPrev
        );
        assert_eq!(
            map_event(&ctrl(KeyCode::Up), InputMode::Composer, W, &[]),
            Action::PrevRun
        );
        // Alt-Enter still maps to one action; the reducer decides between
        // expanding the browsed fold and inserting a line break.
        assert_eq!(
            map_event(&alt(KeyCode::Enter), InputMode::Composer, W, &[]),
            Action::InputNewline
        );
        for (key, action) in [
            ('y', Action::CopyFocusedCard),
            ('r', Action::RetryFailedRun),
            ('a', Action::ReauthenticateFailedModel),
            ('m', Action::ChooseFailureModel),
            ('d', Action::DisableFailureModel),
        ] {
            assert_eq!(
                map_event(&alt(KeyCode::Char(key)), InputMode::Composer, W, &[]),
                action
            );
        }
    }

    /// The composer is a real text field: motion and word/line kill keys map
    /// from the base view, without stealing Ctrl-C (detach) or plain letters.
    #[test]
    fn composer_cursor_keys_map() {
        for (code, action) in [
            (KeyCode::Left, Action::CursorLeft),
            (KeyCode::Right, Action::CursorRight),
            (KeyCode::Home, Action::CursorLineStart),
            (KeyCode::End, Action::CursorLineEnd),
        ] {
            assert_eq!(map_event(&key(code), InputMode::Composer, W, &[]), action);
        }
        assert_eq!(
            map_event(&ctrl(KeyCode::Char('w')), InputMode::Composer, W, &[]),
            Action::DeleteWordBack
        );
        assert_eq!(
            map_event(&ctrl(KeyCode::Char('u')), InputMode::Composer, W, &[]),
            Action::DeleteToLineStart
        );
        // Unmodified `w`/`u` are still ordinary text.
        assert_eq!(
            map_event(&ch('w'), InputMode::Composer, W, &[]),
            Action::InputChar('w')
        );
        assert_eq!(
            map_event(&ctrl(KeyCode::Char('c')), InputMode::Composer, W, &[]),
            Action::Detach
        );
    }

    /// Word motion maps from both Ctrl- and Alt-arrow chords (terminals
    /// disagree about which modifier survives), and the readline trio
    /// `Ctrl-A`/`Ctrl-E`/`Ctrl-K` joins `Home`/`End`.
    #[test]
    fn composer_word_motion_and_readline_chords_map() {
        let alt = |code| Event::Key(KeyEvent::new(code, KeyModifiers::ALT));
        for event in [ctrl(KeyCode::Left), alt(KeyCode::Left)] {
            assert_eq!(
                map_event(&event, InputMode::Composer, W, &[]),
                Action::CursorWordLeft
            );
        }
        for event in [ctrl(KeyCode::Right), alt(KeyCode::Right)] {
            assert_eq!(
                map_event(&event, InputMode::Composer, W, &[]),
                Action::CursorWordRight
            );
        }
        assert_eq!(
            map_event(&ctrl(KeyCode::Char('a')), InputMode::Composer, W, &[]),
            Action::CursorLineStart
        );
        assert_eq!(
            map_event(&ctrl(KeyCode::Char('e')), InputMode::Composer, W, &[]),
            Action::CursorLineEnd
        );
        assert_eq!(
            map_event(&ctrl(KeyCode::Char('k')), InputMode::Composer, W, &[]),
            Action::DeleteToLineEnd
        );
        // Plain arrows keep their history-recall meaning.
        assert_eq!(
            map_event(&key(KeyCode::Left), InputMode::Composer, W, &[]),
            Action::CursorLeft
        );
        // Unmodified `a`/`e`/`k` are still ordinary text.
        for c in ['a', 'e', 'k'] {
            assert_eq!(
                map_event(&ch(c), InputMode::Composer, W, &[]),
                Action::InputChar(c)
            );
        }
    }

    /// Help is reachable from the base view (`F1` any time; the reducer also
    /// opens it for `?` on an empty composer), and from Normal mode.
    #[test]
    fn f1_maps_to_help_in_composer_and_normal_modes() {
        for mode in [InputMode::Composer, InputMode::Normal] {
            assert_eq!(map_event(&key(KeyCode::F(1)), mode, W, &[]), Action::Help);
        }
        // `?` stays ordinary text in the composer — the reducer decides.
        assert_eq!(
            map_event(&ch('?'), InputMode::Composer, W, &[]),
            Action::InputChar('?')
        );
    }

    /// `Home`/`End` jump a Normal-mode browser to its edges, matching the
    /// palette-mode pickers.
    #[test]
    fn home_and_end_map_in_normal_mode() {
        assert_eq!(
            map_event(&key(KeyCode::Home), InputMode::Normal, W, &[]),
            Action::SelectFirst
        );
        assert_eq!(
            map_event(&key(KeyCode::End), InputMode::Normal, W, &[]),
            Action::SelectLast
        );
    }

    /// `Ctrl-Z` restores the last cleared draft; `Ctrl-G` is the XOFF-safe
    /// forward history-search chord (`Ctrl-S` remains an alias).
    #[test]
    fn ctrl_z_and_ctrl_g_map_in_the_composer() {
        assert_eq!(
            map_event(&ctrl(KeyCode::Char('z')), InputMode::Composer, W, &[]),
            Action::DraftRestore
        );
        for chord in ['g', 's'] {
            assert_eq!(
                map_event(&ctrl(KeyCode::Char(chord)), InputMode::Composer, W, &[]),
                Action::HistorySearchNext
            );
        }
    }

    /// The kill keys reach every text surface: prompt overlays (Editing) and
    /// filterable pickers (Palette), not only the composer.
    #[test]
    fn kill_keys_map_in_editing_and_palette_modes() {
        for mode in [InputMode::Editing, InputMode::Palette] {
            assert_eq!(
                map_event(&ctrl(KeyCode::Char('w')), mode, W, &[]),
                Action::DeleteWordBack,
                "Ctrl-W must kill a word in {mode:?}"
            );
            assert_eq!(
                map_event(&ctrl(KeyCode::Char('u')), mode, W, &[]),
                Action::DeleteToLineStart,
                "Ctrl-U must kill the line in {mode:?}"
            );
            // Unmodified letters stay ordinary text/filter input.
            assert_eq!(map_event(&ch('w'), mode, W, &[]), Action::InputChar('w'));
        }
    }

    /// The advertised "Tab — focus a pane" binding was dead in the base view
    /// (Composer mode), which made the help table lie. It now maps.
    #[test]
    fn tab_focuses_a_pane_from_the_base_view() {
        assert_eq!(
            map_event(&key(KeyCode::Tab), InputMode::Composer, W, &[]),
            Action::CyclePane
        );
    }

    #[test]
    fn workspace_side_focus_selects_the_navigation_keymap() {
        let mut state = crate::state::AppState::new();
        state.layout = crate::state::LayoutMode::Workspace;

        for pane in [Pane::Sessions, Pane::Approvals] {
            state.focus = pane;
            let mode = state.input_mode();
            assert_eq!(mode, InputMode::Normal, "{pane:?} must own keyboard input");
            assert_eq!(
                map_event(&key(KeyCode::Up), mode, W, &[]),
                Action::SelectPrev
            );
            assert_eq!(
                map_event(&key(KeyCode::Down), mode, W, &[]),
                Action::SelectNext
            );
            assert_eq!(
                map_event(&key(KeyCode::PageDown), mode, W, &[]),
                Action::ScrollPageDown
            );
        }

        state.focus = Pane::Transcript;
        assert_eq!(state.input_mode(), InputMode::Composer);
        assert_eq!(
            map_event(&key(KeyCode::Up), state.input_mode(), W, &[]),
            Action::HistoryPrev,
            "the center pane keeps composer history/editing semantics"
        );

        state.layout = crate::state::LayoutMode::Chat;
        state.focus = Pane::Sessions;
        assert_eq!(
            state.input_mode(),
            InputMode::Composer,
            "Chat ignores a retained workspace side focus"
        );
    }

    /// Every binding advertising a mouse gesture must name keys that the
    /// mapper actually produces somewhere — the help overlay renders this
    /// table verbatim, so a stale row is a lie to the user (RULE 3).
    #[test]
    fn advertised_mouse_parity_bindings_are_live_in_some_mode() {
        let modes = [
            InputMode::Composer,
            InputMode::Normal,
            InputMode::Palette,
            InputMode::Approval,
            InputMode::Editing,
        ];
        let live = |event: &Event| {
            modes
                .iter()
                .any(|mode| map_event(event, *mode, W, &[]) != Action::NoOp)
        };
        let alt = |code| Event::Key(KeyEvent::new(code, KeyModifiers::ALT));
        for (keys, event) in [
            ("Tab", key(KeyCode::Tab)),
            ("↑↓ + Enter", key(KeyCode::Enter)),
            ("Alt-↑ / Alt-↓", alt(KeyCode::Up)),
            ("Alt-Enter", alt(KeyCode::Enter)),
            ("PgUp / PgDn", key(KeyCode::PageUp)),
        ] {
            assert!(
                KEY_BINDINGS.iter().any(|b| b.keys == keys),
                "{keys} must be a documented binding"
            );
            assert!(live(&event), "{keys} is advertised but maps to nothing");
        }
    }

    #[test]
    fn approval_mode_only_decision_keys() {
        assert_eq!(
            map_event(&ch('a'), InputMode::Approval, W, &[]),
            Action::Approve(ApprovalScope::Once)
        );
        assert_eq!(
            map_event(&ch('A'), InputMode::Approval, W, &[]),
            Action::Approve(ApprovalScope::Run)
        );
        assert_eq!(
            map_event(&ch('r'), InputMode::Approval, W, &[]),
            Action::Reject
        );
        // Typing past an approval is swallowed, not sent to a composer.
        assert_eq!(
            map_event(&ch('x'), InputMode::Approval, W, &[]),
            Action::NoOp
        );
        assert_eq!(
            map_event(&key(KeyCode::Up), InputMode::Approval, W, &[]),
            Action::SelectPrev
        );
        // PgUp/PgDn page the (deliberately unbounded) modal body, not the
        // approval stack — the stack still walks with ↑/↓ and the wheel.
        assert_eq!(
            map_event(&key(KeyCode::PageDown), InputMode::Approval, W, &[]),
            Action::ScrollPageDown
        );
        assert_eq!(
            map_event(&key(KeyCode::PageUp), InputMode::Approval, W, &[]),
            Action::ScrollPageUp
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
        let wheel_up = map_event(
            &wheel(MouseEventKind::ScrollUp, 10),
            InputMode::Normal,
            W,
            &[],
        );
        assert_eq!(wheel_up, Action::SelectPrev);
        assert_eq!(
            wheel_up,
            map_event(&key(KeyCode::Up), InputMode::Normal, W, &[])
        );

        let wheel_down = map_event(
            &wheel(MouseEventKind::ScrollDown, 10),
            InputMode::Normal,
            W,
            &[],
        );
        assert_eq!(wheel_down, Action::SelectNext);
        assert_eq!(
            wheel_down,
            map_event(&key(KeyCode::Down), InputMode::Normal, W, &[])
        );

        // In the conversation the wheel scrolls the transcript a few lines per
        // notch; PgUp / PgDn are the page-sized keyboard equivalent (RULE 3 is
        // "reachable by keyboard", not "identical granularity").
        assert_eq!(
            map_event(
                &wheel(MouseEventKind::ScrollUp, 10),
                InputMode::Composer,
                W,
                &[]
            ),
            Action::ScrollLinesUp
        );
        assert_eq!(
            map_event(
                &wheel(MouseEventKind::ScrollDown, 10),
                InputMode::Composer,
                W,
                &[]
            ),
            Action::ScrollLinesDown
        );
        assert_eq!(
            map_event(&key(KeyCode::PageUp), InputMode::Composer, W, &[]),
            Action::ScrollPageUp
        );
        assert_eq!(
            map_event(&key(KeyCode::PageDown), InputMode::Composer, W, &[]),
            Action::ScrollPageDown
        );

        // A left click with nothing registered under it (an empty hit-test map)
        // falls back to inert — the actual clickable surfaces (palette/pickers/
        // runs/footer/panes/folds) are registered by the renderer (Task 8).
        let click = map_event(
            &wheel(MouseEventKind::Down(MouseButton::Left), 1),
            InputMode::Normal,
            W,
            &[],
        );
        assert_eq!(click, Action::NoOp);

        // (3) A left click resolves to the topmost registered rect's Action, and
        // each such Action is keyboard-reachable.
        use ratatui::layout::Rect;
        let map = vec![(
            Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 1,
            },
            Action::ActivateRow(0),
        )];
        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            map_event(&click, InputMode::Palette, W, &map),
            Action::ActivateRow(0)
        );
        // ActivateRow ≡ SelectNext×k then InputSubmit; SelectRun ≡ Prev/NextRun;
        // FocusPane ≡ Tab (CyclePane); Dismiss ≡ Esc — all in the keyboard table.
        assert_eq!(
            map_event(&key(KeyCode::Enter), InputMode::Palette, W, &[]),
            Action::InputSubmit
        );
        assert_eq!(
            map_event(&key(KeyCode::Tab), InputMode::Normal, W, &[]),
            Action::CyclePane
        );
    }

    #[test]
    fn hit_test_returns_the_topmost_registered_action() {
        let base = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let overlay = Rect {
            x: 2,
            y: 2,
            width: 6,
            height: 3,
        };
        let map = vec![
            (base, Action::FocusPane(Pane::Transcript)),
            (overlay, Action::ActivateRow(1)), // later-registered = topmost
        ];
        // Inside the overlay: the topmost wins.
        assert_eq!(hit_test(&map, 3, 3), Some(Action::ActivateRow(1)));
        // Over the base only: the base wins.
        assert_eq!(
            hit_test(&map, 15, 8),
            Some(Action::FocusPane(Pane::Transcript))
        );
        // Outside everything: None.
        assert_eq!(hit_test(&map, 40, 40), None);
    }

    #[test]
    fn a_left_click_over_a_registered_rect_resolves_to_its_action() {
        let map = vec![(
            Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 2,
            },
            Action::SelectRun(2),
        )];
        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            map_event(&click, InputMode::Composer, W, &map),
            Action::SelectRun(2)
        );
        // No registered rect under the click → NoOp.
        assert_eq!(map_event(&click, InputMode::Composer, W, &[]), Action::NoOp);
    }

    #[test]
    fn a_click_on_a_mention_row_resolves_to_completing_that_match() {
        let map = vec![(
            Rect {
                x: 2,
                y: 3,
                width: 20,
                height: 1,
            },
            Action::MentionSelectAt(1),
        )];
        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            map_event(&click, InputMode::Composer, W, &map),
            Action::MentionSelectAt(1)
        );
    }

    /// The mapper is stateless, so the popup routing lives in the reducer;
    /// drive the full key path (raw event -> `map_event` -> `reduce`) to keep
    /// the mapper in the loop (the same pattern as the Alt-Enter fold test in
    /// render.rs).
    #[test]
    fn composer_keys_route_into_an_open_history_search_popup() {
        let mut state = crate::state::AppState::new();
        state.prompt_history = vec!["cargo build".to_owned(), "cargo test".to_owned()];
        let drive = |state: &mut crate::state::AppState, event: &Event| {
            let action = map_event(event, state.input_mode(), W, &[]);
            crate::reduce::reduce(state, action);
        };

        drive(&mut state, &ctrl(KeyCode::Char('r')));
        assert!(state.history_search.is_some(), "Ctrl-R opens the popup");
        for c in "cargo".chars() {
            drive(&mut state, &ch(c));
        }
        assert_eq!(state.history_search.as_ref().unwrap().query, "cargo");
        assert!(
            state.composer.is_empty(),
            "the query must not land in the composer draft"
        );
        // Up walks to the older match, Enter loads it and closes the popup.
        drive(&mut state, &key(KeyCode::Up));
        drive(&mut state, &key(KeyCode::Enter));
        assert!(state.history_search.is_none(), "Enter closes the popup");
        assert_eq!(state.composer, "cargo build");

        // Esc cancels a reopened search without touching the draft.
        drive(&mut state, &ctrl(KeyCode::Char('r')));
        drive(&mut state, &key(KeyCode::Esc));
        assert!(state.history_search.is_none(), "Esc closes the popup");
        assert_eq!(state.composer, "cargo build");
    }

    #[test]
    fn composer_keys_route_into_an_open_mention_popup() {
        let file_match = |path: &str| codypendent_protocol::command::FileMatchWire {
            path: path.to_owned(),
            indices: Vec::new(),
            score: 0,
        };
        let mut state = crate::state::AppState::new();
        let drive = |state: &mut crate::state::AppState, event: &Event| {
            let action = map_event(event, state.input_mode(), W, &[]);
            crate::reduce::reduce(state, action);
        };

        for c in "see @sr".chars() {
            drive(&mut state, &ch(c));
        }
        assert!(state.mention_popup.is_some(), "typing @ opens the popup");
        crate::reduce::reduce(
            &mut state,
            Action::FileSearchResults {
                query: "sr".to_owned(),
                matches: vec![file_match("src/main.rs"), file_match("src/lib.rs")],
                truncated: false,
            },
        );

        // Up/Down navigate the matches, not composer history.
        drive(&mut state, &key(KeyCode::Down));
        assert_eq!(state.mention_popup.as_ref().unwrap().selected, 1);
        drive(&mut state, &key(KeyCode::Up));
        assert_eq!(state.mention_popup.as_ref().unwrap().selected, 0);

        // Enter completes the highlighted match.
        drive(&mut state, &key(KeyCode::Enter));
        assert!(state.mention_popup.is_none(), "Enter closes the popup");
        assert_eq!(state.composer, "see src/main.rs ");

        // Esc closes a reopened popup and leaves the draft intact.
        for c in "@li".chars() {
            drive(&mut state, &ch(c));
        }
        assert!(state.mention_popup.is_some());
        let draft = state.composer.clone();
        drive(&mut state, &key(KeyCode::Esc));
        assert!(state.mention_popup.is_none(), "Esc closes the popup");
        assert_eq!(state.composer, draft, "Esc must not eat the draft");
    }
}
