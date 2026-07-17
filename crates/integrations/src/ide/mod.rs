//! IDE bridge contract and source provenance (Phase 3 STEP 3.4).
//!
//! The daemon owns the session; an IDE extension is a thin, editor-aware client
//! (Chapter 10). This module carries the *daemon-side* half of that contract:
//!
//! - [`bridge`] — the transport-agnostic [`IdeBridge`] trait the daemon calls to
//!   read the editor's live state (workspace, open documents, selection,
//!   diagnostics) and to ask the editor to act (apply an edit, reveal a
//!   location, show a diff). A [`RecordingIdeBridge`] proves the contract is
//!   usable end-to-end without a real editor.
//! - [`provenance`] — [`resolve_source`] labels every file excerpt entering
//!   model context with a [`codypendent_protocol::ide::SourceProvenance`],
//!   preferring an unsaved editor buffer over the filesystem when their digests
//!   diverge.
//! - [`debounce`] — a deterministic, time-injected coalescer that collapses a
//!   burst of [`codypendent_protocol::ide::IdeContextUpdate`]s down to the last
//!   update of each burst.

pub mod bridge;
pub mod debounce;
pub mod provenance;

pub use bridge::{IdeBridge, OpenDocument, RecordingIdeBridge, WorkspaceState};
pub use debounce::{coalesce_bursts, Debouncer};
pub use provenance::{digest_bytes, label_for, resolve_source};

/// Errors from the IDE bridge: the editor is not attached, or any other failure
/// surfaced from an implementation.
#[derive(Debug, thiserror::Error)]
pub enum IdeError {
    /// No IDE client is attached, or the attached client cannot service the
    /// request. The payload is a short human-readable reason.
    #[error("ide bridge unavailable: {0}")]
    Unavailable(String),
    /// Any other failure, carried transparently from an implementation.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
