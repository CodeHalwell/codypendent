use codypendent_protocol::{
    Actor, EventBody, QuestionId, QuestionOption, QuestionOutcome, QuestionPrompt, RunId,
    SessionEvent,
};
use codypendent_tui::{reduce, Action, AppState, Intent, Theme};
use ratatui::{backend::TestBackend, Terminal};

fn event(state: &mut AppState, body: EventBody) {
    reduce(
        state,
        Action::DaemonEvent(Box::new(SessionEvent {
            sequence: 1,
            occurred_at: "2026-09-08T12:00:00Z".parse().unwrap(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body,
        })),
    );
}

fn ask(state: &mut AppState, id: QuestionId, count: usize, label: &str) {
    event(
        state,
        EventBody::QuestionAsked {
            question_id: id,
            run_id: RunId::new(),
            questions: (0..count)
                .map(|i| QuestionPrompt {
                    header: format!("Question {i}"),
                    question: format!("Choose {label}"),
                    options: vec![QuestionOption {
                        label: label.into(),
                        description: String::new(),
                    }],
                    multiple: true,
                    custom: true,
                })
                .collect(),
        },
    );
}

fn resolve(state: &mut AppState, id: QuestionId) {
    event(
        state,
        EventBody::QuestionResolved {
            question_id: id,
            outcome: QuestionOutcome::Rejected { feedback: None },
        },
    );
}

fn draft(state: &mut AppState) {
    reduce(state, Action::QuestionNavigate(1));
    for c in "keep this answer".chars() {
        reduce(state, Action::QuestionInputChar(c));
    }
    state.question_scroll = 4;
    state.question_max_scroll.set(8);
    *state.question_row_bounds.borrow_mut() = vec![0, 4, 8];
    state.question_body_height.set(4);
}

fn assert_reset(state: &AppState, count: usize) {
    let card = state.question_card_state.as_ref().unwrap();
    assert_eq!(card.index, 0);
    assert_eq!(card.selected, 0);
    assert_eq!(card.custom_text, vec![String::new(); count]);
    assert_eq!(card.picked, vec![Vec::<String>::new(); count]);
    assert!(!card.editing_custom);
    assert!(card.feedback.is_none());
    assert_eq!(state.question_scroll, 0);
    assert_eq!(state.question_max_scroll.get(), 0);
    assert!(state.question_row_bounds.borrow().is_empty());
    assert_eq!(state.question_body_height.get(), 0);
}

#[test]
fn same_shape_reissue_discards_answers_to_the_previous_issue() {
    for replacement in ["old option", "new option"] {
        let mut state = AppState::new();
        let id = QuestionId::new();
        ask(&mut state, id, 2, "old option");
        reduce(&mut state, Action::QuestionSelectOrConfirm);
        draft(&mut state);
        reduce(&mut state, Action::QuestionOpenReject);
        reduce(&mut state, Action::QuestionInputChar('r'));
        ask(&mut state, id, 2, replacement);
        assert_reset(&state, 2);
        assert_eq!(state.pending_questions.len(), 1);
        reduce(&mut state, Action::QuestionSelectOrConfirm);
        reduce(&mut state, Action::QuestionSelectOrConfirm);
        assert!(matches!(state.outbox.last(), Some(Intent::ResolveQuestion {
            question_id, outcome: QuestionOutcome::Answered { answers }
        }) if *question_id == id && *answers == vec![vec![replacement.to_owned()]; 2]));
    }
}

#[test]
fn background_additions_and_reissues_preserve_the_front_draft_and_viewport() {
    let mut state = AppState::new();
    let front = QuestionId::new();
    let back = QuestionId::new();
    ask(&mut state, front, 1, "front");
    draft(&mut state);
    let before = state.question_card_state.clone();
    for count in [1, 4, 2] {
        ask(&mut state, back, count, "background");
        assert_eq!(state.question_card_state, before);
        assert_eq!(state.question_scroll, 4);
        assert_eq!(*state.question_row_bounds.borrow(), vec![0, 4, 8]);
    }
    resolve(&mut state, front);
    assert_eq!(state.pending_questions[0].question_id, back);
    assert_reset(&state, 2);
}

#[test]
fn background_resolution_and_unknown_resolution_preserve_the_front_draft() {
    let mut state = AppState::new();
    let front = QuestionId::new();
    let back = QuestionId::new();
    ask(&mut state, front, 1, "front");
    ask(&mut state, back, 3, "background");
    draft(&mut state);
    let before = state.question_card_state.clone();
    for id in [back, QuestionId::new(), back] {
        resolve(&mut state, id);
        assert_eq!(state.question_card_state, before);
        assert_eq!(state.question_scroll, 4);
        assert_eq!(state.pending_questions[0].question_id, front);
    }
    resolve(&mut state, front);
    assert!(state.question_card_state.is_none());
    assert_eq!(state.question_scroll, 0);
    assert!(state.question_row_bounds.borrow().is_empty());
}

#[test]
fn reissued_or_resolved_questions_retire_only_their_unsent_decisions() {
    for reissue in [false, true] {
        let mut state = AppState::new();
        let id = QuestionId::new();
        let other = QuestionId::new();
        ask(&mut state, id, 1, "old");
        reduce(&mut state, Action::QuestionSelectOrConfirm);
        state.outbox.push(Intent::ResolveQuestion {
            question_id: other,
            outcome: QuestionOutcome::Rejected { feedback: None },
        });
        if reissue {
            ask(&mut state, id, 1, "new");
        } else {
            resolve(&mut state, id);
        }
        assert!(
            matches!(state.outbox.as_slice(), [Intent::ResolveQuestion { question_id, .. }] if *question_id == other)
        );
    }
}

#[test]
fn recovered_front_question_renders_at_small_terminal_sizes() {
    let mut state = AppState::new();
    let id = QuestionId::new();
    ask(&mut state, id, 3, "old");
    draft(&mut state);
    ask(&mut state, id, 1, "new");
    for (width, height) in [(120, 40), (30, 10), (1, 1), (0, 0)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| codypendent_tui::render(frame, &state, &Theme::dark()))
            .unwrap();
    }
}
