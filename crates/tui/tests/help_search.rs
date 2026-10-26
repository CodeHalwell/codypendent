use codypendent_tui::state::{InputMode, Overlay};
use codypendent_tui::{
    accessible_snapshot, map_accessible_input, map_event, reduce, Action, AppState, Theme,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, Terminal};

fn draw(state: &AppState, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| codypendent_tui::render(frame, state, &Theme::dark()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn key(state: &mut AppState, code: KeyCode, modifiers: KeyModifiers) {
    let action = map_event(
        &Event::Key(KeyEvent::new(code, modifiers)),
        state.input_mode(),
        120,
        &[],
    );
    reduce(state, action);
}

#[test]
fn typing_filters_help_instead_of_running_shortcuts_or_editing_the_draft() {
    let mut state = AppState::new();
    state.composer = "keep my objective".into();
    reduce(&mut state, Action::Help);
    assert_eq!(state.input_mode(), InputMode::Help);
    for c in "model".chars() {
        key(&mut state, KeyCode::Char(c), KeyModifiers::NONE);
    }
    let graphical = draw(&state, 120, 50);
    let accessible = accessible_snapshot(&state);
    assert!(graphical.contains("Filter: model"));
    assert!(accessible.contains("Shortcut filter: model"));
    assert!(!accessible.contains("toggle voice input"));
    assert!(state.outbox.is_empty());
    assert_eq!(state.composer, "keep my objective");
    key(&mut state, KeyCode::Esc, KeyModifiers::NONE);
    assert!(matches!(state.overlay, Overlay::None));
}

#[test]
fn filtering_resets_scroll_and_clear_restores_every_binding() {
    let mut state = AppState::new();
    reduce(&mut state, Action::Help);
    draw(&state, 80, 24);
    key(&mut state, KeyCode::End, KeyModifiers::NONE);
    assert!(state.help_scroll > 0);
    key(&mut state, KeyCode::Char('z'), KeyModifiers::NONE);
    assert_eq!(state.help_scroll, 0);
    reduce(&mut state, Action::InputPaste("zzzzzzz".into()));
    assert!(draw(&state, 120, 50).contains("No matching shortcuts"));
    for action in map_accessible_input("clear", state.input_mode()) {
        reduce(&mut state, action);
    }
    assert!(state.help_query.is_empty());
    assert!(!draw(&state, 120, 50).contains("No matching shortcuts"));
    key(&mut state, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(state.should_detach);
}

#[test]
fn unicode_paste_is_bounded_and_small_help_views_remain_renderable() {
    let mut state = AppState::new();
    reduce(&mut state, Action::Help);
    reduce(&mut state, Action::InputPaste("界".repeat(1000)));
    assert_eq!(state.help_query.chars().count(), 256);
    key(&mut state, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(state.help_query.chars().count(), 255);
    for (width, height) in [(120, 50), (60, 20), (20, 5), (1, 1), (0, 0)] {
        draw(&state, width, height);
    }
}
