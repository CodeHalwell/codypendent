//! codypendent-tui.
//!
//! The Ratatui client: rendering, input handling, layout, components, and
//! themes. This crate speaks only `codypendent-protocol` types and holds no
//! database or network code — a dedicated task in the CLI owns the protocol
//! connection and translates daemon events into [`Action`]s (STEP 1.12 RULE 1).
//!
//! # Architecture — a strict unidirectional loop
//!
//! ```text
//!   crossterm event ──map_event──▶ Action ─┐
//!   daemon SessionEvent ─Action::DaemonEvent┘
//!                                           │
//!                                           ▼
//!                                    reduce(&mut AppState, Action)   (pure, no I/O)
//!                                           │        │
//!                                           │        └──▶ AppState.outbox: Vec<Intent>
//!                                           ▼             (drained by the CLI, sent as Commands)
//!                                    render(frame, &AppState, &Theme)   (draw only, no I/O)
//! ```
//!
//! The CLI's loop each iteration: read a `crossterm` event (or a daemon event
//! from its connection task), map it to an [`Action`], call [`reduce`], drain
//! [`AppState::outbox`] of [`Intent`]s and dispatch them as protocol commands,
//! then [`render`]. Widgets never perform I/O (RULE 2); every mouse gesture has
//! a keyboard equivalent (RULE 3, see [`input::KEY_BINDINGS`]); colors come only
//! from [`Theme`] tokens (RULE 7).

pub mod action;
pub mod input;
pub mod reduce;
pub mod render;
pub mod state;
pub mod terminal;
pub mod theme;

pub use action::{Action, Intent};
pub use input::{map_event, pane_at, KeyBinding, KEY_BINDINGS};
pub use reduce::reduce;
pub use render::render;
pub use state::{
    AppState, InputMode, MemoryCard, Overlay, Pane, PatchSummary, PendingApproval, RunView,
    SkillCard, StatusProjection, ToolCard, ToolStatus, TranscriptEntry,
};
pub use terminal::TerminalGuard;
pub use theme::Theme;
