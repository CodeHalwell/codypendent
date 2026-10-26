use codypendent_protocol::{
    Actor, ApprovalDecision, ApprovalId, ApprovalScope, EventBody, ProposedAction, Risk, RiskLevel,
    SessionEvent,
};
use codypendent_tui::{accessible_snapshot, reduce, Action, AppState, Intent, Theme};
use ratatui::{backend::TestBackend, Terminal};

fn approval(action: ProposedAction) -> AppState {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::DaemonEvent(Box::new(SessionEvent {
            sequence: 1,
            occurred_at: chrono::Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body: EventBody::ApprovalRequested {
                approval_id: ApprovalId::new(),
                action,
                risk: Risk {
                    level: RiskLevel::High,
                    reasons: vec!["Review the destination".into()],
                },
                pattern: None,
            },
        })),
    );
    state
}

fn screen(state: &AppState, width: u16, height: u16) -> String {
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

#[test]
fn github_writes_and_checkpoint_restores_show_the_actual_destination() {
    for (action, evidence) in [
        (
            ProposedAction::GitHubMutation {
                repository: "owner/repo".into(),
                summary: "create draft PR".into(),
            },
            vec!["owner/repo", "create draft PR"],
        ),
        (
            ProposedAction::RestoreCheckpoint {
                run_id: "run-1".into(),
                ordinal: 4,
                worktree: "/tmp/project".into(),
                commit: "abc123".into(),
            },
            vec!["run-1", "/tmp/project", "abc123", "discarded"],
        ),
    ] {
        let mut state = approval(action);
        let graphical = screen(&state, 120, 50);
        let accessible = accessible_snapshot(&state);
        for detail in evidence {
            assert!(graphical.contains(detail), "missing {detail}: {graphical}");
            assert!(
                accessible.contains(detail),
                "missing {detail}: {accessible}"
            );
        }
        assert!(!graphical.contains("unsupported"));
        reduce(&mut state, Action::Approve(ApprovalScope::Once));
        assert!(matches!(
            state.outbox.last(),
            Some(Intent::ResolveApproval {
                decision: ApprovalDecision::Approve,
                ..
            })
        ));
    }
}

#[test]
fn unknown_actions_are_rejectable_but_never_approvable_by_keys_or_mouse() {
    let mut state = approval(ProposedAction::Unknown);
    for scope in [
        ApprovalScope::Once,
        ApprovalScope::Run,
        ApprovalScope::Pattern,
        ApprovalScope::Repository,
    ] {
        reduce(&mut state, Action::Approve(scope));
        assert!(state.outbox.is_empty());
    }
    assert!(accessible_snapshot(&state).contains("Approval controls: reject"));
    for (width, height) in [(120, 50), (60, 20), (20, 5), (1, 1), (0, 0)] {
        screen(&state, width, height);
        assert!(!state
            .hit_map
            .borrow()
            .iter()
            .any(|(_, action)| matches!(action, Action::Approve(_))));
    }
    reduce(&mut state, Action::Reject);
    assert!(matches!(
        state.outbox.last(),
        Some(Intent::ResolveApproval {
            decision: ApprovalDecision::Reject,
            ..
        })
    ));
}

#[test]
fn restored_evidence_cannot_emit_terminal_controls() {
    let mut state = approval(ProposedAction::GitHubMutation {
        repository: "owner/repo".into(),
        summary: "post".into(),
    });
    // Snapshot recovery can populate a pending action without the live event's
    // ingress sanitizer. The projection itself must be safe on that path too.
    state.pending_approvals[0].action = ProposedAction::McpToolCall {
        server: "server\x1b[2J".into(),
        tool: "write".into(),
        summary: "summary".into(),
        args: "{\"text\":\"\x1b]52;c;YQ==\x07\"}".into(),
    };
    state.pending_approvals[0].risk.reasons = vec!["\x1b[2Jwarning".into()];
    assert!(!screen(&state, 120, 50).contains('\x1b'));
    assert!(!accessible_snapshot(&state).contains('\x1b'));
}
