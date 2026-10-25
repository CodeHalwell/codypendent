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

pub mod analytics;
pub mod artifact;
pub mod automation;
pub mod blackboard;
pub mod bundle;
pub mod capabilities;
pub mod catchup;
/// Client-facing code-graph views (`codypendent graph {build,status,show}`).
pub mod codegraph;
pub mod command;
pub mod discovery;
pub mod document;
pub mod envelope;
pub mod error;
pub mod events;
pub mod federated_graph;
pub mod framing;
pub mod handshake;
pub mod ide;
pub mod ids;
pub mod inbox;
pub mod input;
pub mod marketplace;
pub mod memory;
pub mod model;
pub mod question;
pub mod remote_ui;
pub mod run;
pub mod secrets;
pub mod session;
pub mod version;
pub mod workflow;

pub use analytics::{
    AnalyticsBucket, AnalyticsBudget, AnalyticsBudgetDimension, AnalyticsBudgetDraft,
    AnalyticsBudgetPage, AnalyticsBudgetPatch, AnalyticsBudgetQuery, AnalyticsBudgetRequest,
    AnalyticsBudgetScope, AnalyticsBudgetWindow, AnalyticsCompletion, AnalyticsDimensionCoverage,
    AnalyticsExportFormat, AnalyticsExportRequest, AnalyticsExportResult, AnalyticsFilters,
    AnalyticsGrouping, AnalyticsMetrics, AnalyticsPage, AnalyticsQuery, AnalyticsTimeRange,
    MeasurementCoverage,
};
pub use artifact::{ArtifactRef, DataClassification};
pub use automation::*;
pub use blackboard::{board_scope_id, BlackboardItemDraft, BlackboardItemView, BlackboardScope};
pub use bundle::*;
pub use capabilities::ClientCapabilities;
pub use catchup::{Catchup, PendingApprovalProjection, SessionProjection};
pub use codegraph::{
    CodeGraphEdgeAssertion, CodeGraphEdgeView, CodeGraphGrammar, CodeGraphLanguageCount,
    CodeGraphNodeView, CodeGraphPage, CodeGraphQuery, CodeGraphScanReport,
    CodeGraphSkippedExtension, CodeGraphSourceLocation, CodeGraphStatusView, CodeGraphTally,
};
pub use command::{
    CanaryMetrics, Command, CommandBody, DaemonStore, FileMatchWire, NamedResource,
    PromotionAction, UiPluginLifecycleStatus,
};
pub use document::{
    DocumentEditLease, DocumentLeaseGrant, DocumentMutation, DocumentSync, PublishTarget,
    SuggestionInput,
};
pub use envelope::{DaemonStatus, Envelope, Payload, ProtocolError};
pub use error::{CodypendentError, UserAction};
pub use events::{Actor, EventBody, SessionEvent, UsageCoverage};
pub use federated_graph::*;
pub use framing::{read_envelope, write_envelope, FrameError, MAX_FRAME_BYTES};
pub use handshake::{ClientHello, ClientRole, ResumeToken, ServerHello, Subscription};
pub use ide::{
    Diagnostic, DiagnosticSeverity, DiffRequest, DirtyBufferDigest, EditorSelection,
    IdeContextUpdate, IdeRequest, Location, Position, Range, SourceProvenance, TextEdit,
    WorkspaceEdit,
};
pub use ids::*;
pub use inbox::*;
pub use input::{
    transcription_allowed, AudioArtifact, ClassificationError, GitHubRefKind, GitHubReference,
    ImageArtifact, ImageRegion, InputBlock, InputEnvelope, InputSource, ModelObservation,
    OffDevicePolicy, ScopeLevel, SymbolRef, Transcript, TranscriptionMode,
    DEFAULT_MEDIA_CLASSIFICATION,
};
pub use marketplace::*;
pub use memory::{MemoryEvidence, MemoryScope, MemoryScopeTier, MemoryView};
pub use model::{ModelProbe, ModelReadiness};
pub use question::{QuestionOption, QuestionOutcome, QuestionPrompt};
pub use remote_ui::*;
pub use run::{
    AgentMode, ApprovalDecision, ApprovalScope, BudgetDimension, CheckpointKind, PendingPromptView,
    PromptDelivery, ProposedAction, Risk, RiskLevel, RunDisposition, RunState, ToolOutcome,
};
pub use secrets::*;
pub use session::*;
pub use version::{ProtocolVersion, PROBE_MODEL_MIN_MINOR, PROTOCOL_V1};
pub use workflow::{
    WorkflowEvent, WorkflowNodeState, WorkflowNodeView, WorkflowRunPhase, WorkflowRunSnapshot,
};

/// A per-build identifier, computed by `build.rs` and identical across the
/// whole single binary (the client half and the daemon half are one crate
/// graph in one build). See `build.rs` for the exact precedence:
/// `CODYPENDENT_BUILD_ID` env override, else `"{version}+{git_short_hash}[-dirty]"`,
/// else the bare package version when git is unavailable.
///
/// Used by the daemon-auto-restart-on-version-mismatch feature: the client
/// compares its own `BUILD_ID` against the running daemon's reported id
/// (`ServerHello.build_id`) to detect a stale in-memory daemon after a
/// reinstall.
pub const BUILD_ID: &str = env!("CODYPENDENT_BUILD_ID");

#[cfg(test)]
mod build_id_tests {
    use super::BUILD_ID;

    #[test]
    fn build_id_is_non_empty_and_starts_with_the_package_version() {
        assert!(!BUILD_ID.is_empty(), "BUILD_ID must never be empty");
        let pkg_version = env!("CARGO_PKG_VERSION");
        assert!(
            BUILD_ID.starts_with(pkg_version),
            "BUILD_ID {BUILD_ID:?} must start with CARGO_PKG_VERSION {pkg_version:?}"
        );
    }
}
