use codypendent_tui::{map_event, reduce, Action, AppState};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

fn key(state: &mut AppState, code: KeyCode, modifiers: KeyModifiers) {
    let action = map_event(
        &Event::Key(KeyEvent::new(code, modifiers)),
        state.input_mode(),
        100,
        &[],
    );
    reduce(state, action);
}

#[test]
fn option_word_editing_preserves_unicode_and_the_rest_of_the_draft() {
    let mut state = AppState::new();
    reduce(&mut state, Action::InputPaste("fix café errors".into()));
    key(&mut state, KeyCode::Char('b'), KeyModifiers::ALT);
    assert_eq!(state.composer_cursor, "fix café ".len());
    key(&mut state, KeyCode::Backspace, KeyModifiers::ALT);
    assert_eq!(state.composer, "fix errors");
    key(&mut state, KeyCode::Char('f'), KeyModifiers::ALT);
    assert_eq!(state.composer_cursor, state.composer.len());
    assert!(state.outbox.is_empty());
}

#[test]
fn shift_enter_edits_without_submitting_and_control_d_deletes_at_the_cursor() {
    let mut state = AppState::new();
    reduce(&mut state, Action::InputPaste("beforeafter".into()));
    key(&mut state, KeyCode::Home, KeyModifiers::NONE);
    for _ in 0..6 {
        key(&mut state, KeyCode::Right, KeyModifiers::NONE);
    }
    state.transcript_browse = true;
    key(&mut state, KeyCode::Enter, KeyModifiers::SHIFT);
    assert_eq!(state.composer, "before\nafter");
    assert!(!state.transcript_browse);
    key(&mut state, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(state.composer, "before\nfter");
    assert!(state.outbox.is_empty());
}

#[test]
fn large_clipboard_input_is_bounded_and_normalises_platform_newlines() {
    let state = AppState::new();
    let action = map_event(
        &Event::Paste(format!("one\r\ntwo\rthree\n\t{}", "é".repeat(1_000_000))),
        state.input_mode(),
        100,
        &[],
    );
    let Action::InputPaste(text) = action else {
        panic!("clipboard must produce a paste action");
    };
    assert!(text.starts_with("one\ntwo\nthree\n    "));
    assert!(text.len() <= 64 * 1024);
    assert!(text.ends_with('é'));
    assert!(state.outbox.is_empty());
}

fn agent(id: &str) -> codypendent_tui::state::ProviderCard {
    codypendent_tui::state::ProviderCard {
        id: id.into(),
        name: format!("Agent {id}"),
        protocol: "acp".into(),
        auth: "none".into(),
        local: true,
        requires_key: false,
        can_list_models: true,
        available: true,
        catalog_models: 0,
        has_key: false,
    }
}

#[test]
fn background_catalogue_refresh_keeps_the_focused_provider_in_both_pickers() {
    use codypendent_tui::state::{OnboardProviderClass, Overlay};
    for overlay in [
        Overlay::ProviderPicker {
            query: "agent".into(),
            selected: 1,
        },
        Overlay::OnboardProviderPicker {
            class: OnboardProviderClass::AcpAgent,
            query: "agent".into(),
            selected: 1,
        },
    ] {
        let mut state = AppState::new();
        state.providers = vec![agent("a"), agent("c")];
        state.overlay = overlay;
        state.selected_provider = 1;
        state.composer = "keep my draft".into();
        reduce(
            &mut state,
            Action::ProviderCatalogLoaded(vec![agent("a"), agent("b"), agent("c")]),
        );
        assert_eq!(state.providers[state.selected_provider].id, "c");
        assert!(matches!(
            state.overlay,
            Overlay::ProviderPicker { selected: 2, .. }
                | Overlay::OnboardProviderPicker { selected: 2, .. }
        ));
        assert_eq!(state.composer, "keep my draft");
        reduce(&mut state, Action::ProviderCatalogLoaded(vec![]));
        assert_eq!(state.selected_provider, 0);
        assert!(state.outbox.is_empty());
    }
}

#[test]
fn catalogue_can_be_refreshed_from_the_command_palette() {
    let mut state = AppState::new();
    reduce(&mut state, Action::OpenPalette);
    reduce(&mut state, Action::InputPaste("refresh-agents".into()));
    reduce(&mut state, Action::InputSubmit);
    assert_eq!(
        state.drain_outbox(),
        vec![codypendent_tui::Intent::RefreshAgentCatalog]
    );
}
