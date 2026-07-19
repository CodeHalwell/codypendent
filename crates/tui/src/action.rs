//! Semantic actions (STEP 1.12 RULE 1).
//!
//! Everything that can change the app funnels through [`Action`]. Two sources
//! feed it: the CLI's connection task, which wraps each daemon [`SessionEvent`]
//! as [`Action::DaemonEvent`]; and the input layer ([`crate::input::map_event`]),
//! which turns a key/mouse/paste/resize into a navigation or command action.
//! The reducer ([`crate::reduce::reduce`]) is the only place that reads an
//! `Action`, and it performs no I/O.

use codypendent_protocol::{ApprovalScope, RunId, SessionEvent};

use crate::state::Pane;

/// A semantic action the reducer folds into [`crate::state::AppState`].
///
/// The large [`SessionEvent`] is boxed so every other (small) variant does not
/// pay for it — and so the whole enum stays cheap to move.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    // --- from the connection task ---
    /// A durable daemon event to fold into state.
    DaemonEvent(Box<SessionEvent>),
    /// A catch-up *snapshot* (the session was too far behind for an event
    /// replay): seed the session title, closed flag, and its active runs as
    /// stubs so a reopened long-running session is not blank until the next live
    /// event fills a run in.
    CatchupSnapshot {
        title: String,
        closed: bool,
        runs: Vec<RunId>,
    },
    /// A periodic timer tick (spinner animation, elapsed timers). No I/O.
    Tick,
    /// A transient status-line notice from the harness (e.g. a rejected
    /// command's code + message). Cleared automatically a few seconds later.
    Notice(String),

    // --- navigation (from keys / mouse) ---
    /// Move keyboard focus to the next pane (`Tab`).
    CyclePane,
    /// Move keyboard focus to a specific pane (mouse click).
    FocusPane(Pane),
    /// Select the previous item / scroll up in the focused pane (`Up`/`k`/wheel-up).
    SelectPrev,
    /// Select the next item / scroll down in the focused pane (`Down`/`j`/wheel-down).
    SelectNext,
    /// Scroll the transcript up a page (`PageUp`).
    ScrollPageUp,
    /// Scroll the transcript down a page (`PageDown`).
    ScrollPageDown,
    /// Open / expand the selected item (`Enter`).
    Expand,

    // --- run control ---
    /// Switch the conversation to the previous run (`Ctrl-↑`).
    PrevRun,
    /// Switch the conversation to the next run (`Ctrl-↓`).
    NextRun,
    /// Open the new-run prompt (`n`).
    NewRun,
    /// Pause the selected run, or resume it if already paused (`p`).
    Pause,
    /// Ask to cancel the selected run — opens a confirm modal (`c`).
    Cancel,
    /// Confirm a pending cancel (`y`/`Enter` in the confirm modal).
    ConfirmCancel,
    /// Open the steering-input prompt (`s`).
    Steer,

    // --- approvals ---
    /// Approve the focused pending approval with the given scope
    /// (`a` = once, `A` = for the run).
    Approve(ApprovalScope),
    /// Reject the focused pending approval (`r`).
    Reject,

    // --- text entry (active only while a prompt overlay is open) ---
    /// Append a character to the open prompt.
    InputChar(char),
    /// Insert bracketed-paste text into the open prompt.
    InputPaste(String),
    /// Delete the last character of the open prompt.
    InputBackspace,
    /// Submit the open prompt (`Enter`).
    InputSubmit,
    /// Abandon the open prompt (`Esc`).
    InputCancel,

    // --- knowledge browsers (STEP 2.6) ---
    /// Toggle the Skill Studio browser (`S`).
    OpenSkills,
    /// Toggle the memory browser (`M`).
    OpenMemory,
    /// Reveal the focused memory's source in full (`o`, or `Enter` in the memory
    /// browser). The TUI does no I/O, so this surfaces the source string rather
    /// than opening a file.
    OpenSource,

    // --- Docs Studio & code intelligence (Phase 4 client wiring) ---
    /// Toggle the Docs Studio browser (`D`): tree / editor rail / review rail.
    OpenDocs,
    /// Toggle the code-graph edge inspector (`G`).
    OpenEdges,

    /// Toggle the command palette (`/`): a searchable list of every command.
    OpenPalette,
    /// Flip between the chat single-column and the workspace panes (`F2`).
    ToggleLayout,

    // --- overlays / lifecycle ---
    /// Toggle the help overlay (`?`).
    Help,
    /// Detach this client (`q`). Never kills the run.
    Detach,
    /// Dismiss the top-most overlay / modal (`Esc`).
    Dismiss,

    /// A recognized-but-inert event (e.g. an unmapped key). Kept so the input
    /// mapper can stay total and callers never juggle `Option`.
    NoOp,
}

impl Action {
    /// Convenience constructor that boxes the event for [`Action::DaemonEvent`].
    #[must_use]
    pub fn daemon_event(event: SessionEvent) -> Self {
        Action::DaemonEvent(Box::new(event))
    }
}

/// A semantic command the reducer wants sent to the daemon.
///
/// The TUI performs no I/O, so instead of talking to the daemon it appends an
/// `Intent` to [`crate::state::AppState::outbox`]. The CLI's connection task
/// drains the outbox after each reduce and turns each intent into a protocol
/// `Command`. This keeps `reduce` pure and unit-testable: a test asserts on the
/// intents produced, never on a socket.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Start a new run in the attached session.
    StartRun {
        objective: String,
        mode: codypendent_protocol::AgentMode,
    },
    /// Resolve a pending approval.
    ResolveApproval {
        approval_id: codypendent_protocol::ApprovalId,
        decision: codypendent_protocol::ApprovalDecision,
        scope: ApprovalScope,
    },
    /// Pause a run.
    PauseRun { run_id: codypendent_protocol::RunId },
    /// Resume a paused run.
    ResumeRun { run_id: codypendent_protocol::RunId },
    /// Cancel a run.
    CancelRun { run_id: codypendent_protocol::RunId },
    /// Queue steering text to apply at the next safe point.
    QueueSteering {
        run_id: codypendent_protocol::RunId,
        text: String,
    },
}
