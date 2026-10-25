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

pub mod accessible;
pub mod action;
mod approval;
pub mod dag;
mod failure;
pub mod input;
pub mod markdown;
pub mod palette;
pub mod reduce;
pub mod remote_ui;
pub mod remote_ui_host;
pub mod render;
mod splash;
pub mod state;
pub mod streaming;
pub mod table_detect;
pub mod terminal;
pub mod theme;
pub mod theme_pack;
pub mod usage;
#[cfg(feature = "vt100-tests")]
pub mod vt100_backend;

pub use accessible::{accessible_snapshot, map_accessible_input, sanitize_accessible_text};
pub use action::{
    Action, Intent, KeyTarget, LearningMutation, ProjectionKind, SecretKey, WorkflowNodeUpdate,
};
pub use input::{map_event, pane_at, KeyBinding, KEY_BINDINGS};
pub use markdown::{RichLine, RichSpan, SpanRole, SyntaxRole};
pub use reduce::reduce;
pub use remote_ui::{
    project_accessibility, render_remote_ui, AccessibilityNode, AccessibilityProjection,
    DiagnosticSeverity, FocusDescriptor, FormFieldDescriptor, HitRegion, KeyboardAction, RemoteKey,
    RemoteUiRenderOptions, RemoteUiRenderOutput, RemoteUiViewState, RenderDiagnostic,
    TerminalUiCapabilities, ALL_NATIVE_PRIMITIVES,
};
pub use remote_ui_host::{
    accessible_terminal_capabilities_message, terminal_capabilities_message, RemoteUiHostState,
};
pub use render::{render, render_splash};
pub use state::{
    AddModelRow, AppState, BlackboardItemCard, CouncilBuilderState, CouncilBuilderStep,
    CouncilCard, CouncilMemberDraft, CouncilMemberSummary, CouncilProgressPhase,
    CouncilRoundSummary, CouncilRunSummary, DocBlockView, DocCard, DocEdit, DocFocus,
    DocLeaseState, DocPublishTargetKind, DocSuggestionView, GraphEdgeCard, InputMode, KanbanCard,
    KeyStatus, LayoutMode, LearningCard, MemoryCard, ModelCard, ModelListOrigin,
    ModelLocationLabel, ModelReadiness, Overlay, Pane, PatchSummary, PendingApproval, ProviderCard,
    RunView, SkillCard, StatusProjection, ThemeChoice, ToolCard, ToolStatus, TranscriptEntry,
    UnslothQuantCard, UnslothRepoCard, VoiceKeyRow, WorkflowNodeCard, DOC_PUBLISH_TARGETS,
    EDGE_PAGE_SIZE,
};
pub use terminal::TerminalGuard;
pub use theme::{ColorDepth, Theme, ThemePreferences, ThemeVariant, Tone};
pub use theme_pack::{load_theme_pack, ThemePackError, ThemePackManifest};
