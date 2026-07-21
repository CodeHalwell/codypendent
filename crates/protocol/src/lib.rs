//! Codypendent Protocol.
//!
//! Wire types, identifiers, envelopes, framing, and daemon discovery shared by
//! `codypendentd` and every client (CLI, TUI, IDE bridges, headless).
//!
//! Rules that hold for the whole protocol crate:
//! - types here are serialization contracts; behaviour lives in the daemon;
//! - fields are additive by default; breaking changes require a new major
//!   protocol version;
//! - unknown enum variants must be handled safely by receivers.

pub mod artifact;
pub mod capabilities;
pub mod catchup;
pub mod command;
pub mod discovery;
pub mod document;
pub mod envelope;
pub mod error;
pub mod events;
pub mod framing;
pub mod handshake;
pub mod ide;
pub mod ids;
pub mod input;
pub mod run;
pub mod version;

pub use artifact::{ArtifactRef, DataClassification};
pub use capabilities::ClientCapabilities;
pub use catchup::{Catchup, SessionProjection};
pub use command::{Command, CommandBody, PromotionAction};
pub use document::{
    DocumentEditLease, DocumentLeaseGrant, DocumentMutation, DocumentSync, SuggestionInput,
};
pub use envelope::{DaemonStatus, Envelope, Payload, ProtocolError};
pub use error::{CodypendentError, UserAction};
pub use events::{Actor, EventBody, SessionEvent};
pub use framing::{read_envelope, write_envelope, FrameError, MAX_FRAME_BYTES};
pub use handshake::{ClientHello, ClientRole, ResumeToken, ServerHello, Subscription};
pub use ide::{
    Diagnostic, DiagnosticSeverity, DiffRequest, DirtyBufferDigest, EditorSelection,
    IdeContextUpdate, IdeRequest, Location, Position, Range, SourceProvenance, TextEdit,
    WorkspaceEdit,
};
pub use ids::*;
pub use input::{
    transcription_allowed, AudioArtifact, ClassificationError, GitHubRefKind, GitHubReference,
    ImageArtifact, ImageRegion, InputBlock, InputEnvelope, InputSource, ModelObservation,
    OffDevicePolicy, ScopeLevel, SymbolRef, Transcript, TranscriptionMode,
    DEFAULT_MEDIA_CLASSIFICATION,
};
pub use run::{
    AgentMode, ApprovalDecision, ApprovalScope, BudgetDimension, ProposedAction, Risk, RiskLevel,
    RunDisposition, RunState, ToolOutcome,
};
pub use version::{ProtocolVersion, PROTOCOL_V1};
