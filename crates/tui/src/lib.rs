//! codypendent-tui.
//!
//! The Ratatui client: rendering, input handling, layout, components, and
//! themes. This crate speaks only `codypendent-protocol` types and holds no
//! database or network code — a dedicated task in the CLI owns the protocol
//! connection and translates daemon events into `Action`s (STEP 1.12).
//!
//! Architecture is a strict unidirectional loop:
//! input events → `Action` → reducer updates `AppState` → render.

// Phase 1 modules are populated by STEP 1.12.
