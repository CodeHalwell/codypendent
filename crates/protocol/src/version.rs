//! Protocol version negotiation.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

/// The current protocol version. Additive changes bump `minor`; breaking
/// changes bump `major` and require negotiation.
///
/// Phase 1 adds handshake, command, catch-up, artifact-reference, and run/tool/
/// approval event payloads — all additive over Phase 0, so `major` stays `1`
/// and `minor` advances to `1`.
///
/// The daemon-auto-restart-on-version-mismatch feature adds
/// `ServerHello.build_id` and `DaemonStatus.build_id`/`active_run_count`, all
/// `#[serde(default)]` — additive again, so `major` stays `1` and `minor`
/// advances to `2`.
///
/// The daemon-side idle-guarded shutdown (`Payload::ShutdownIfIdle` /
/// `ShutdownRefused`) that closes the auto-restart TOCTOU window adds two new
/// payload variants — additive again (old peers never emit them, and a client
/// only sends `ShutdownIfIdle` to a daemon whose negotiated minor is ≥ 3), so
/// `major` stays `1` and `minor` advances to `3`.
///
/// External ACP agents add the `ProposedAction::AcpToolCall` approval payload.
/// It is additive, so `major` remains `1` and `minor` advances to `4`.
///
/// Adoption 11 adds `CommandBody::ListSessions`, `Payload::SessionList`,
/// `CommandBody::SearchWorkspaceFiles`, and `Payload::FileSearchResults`.
/// All are additive, so `major` stays `1` and `minor` advances to `5`.
///
/// Bounded artifact retrieval adds `ReadArtifact` and `ArtifactChunk`; both are
/// additive, so `major` remains `1` and `minor` advances to `6`.
///
/// Milestone 6 adds cross-repository architecture intelligence, publication policy,
/// and federated graph wire contracts — additive, so `major` remains `1` and
/// `minor` advances to `7`.
///
/// The classified run failure adds `RunDisposition::Failed.error`, an optional
/// [`crate::CodypendentError`] beside the human `reason` carrying the code,
/// retryability and `user_action` a client turns into an affordance. It is
/// `#[serde(default)]` and skipped when absent, so an older daemon simply
/// never sends it and an older client ignores it — additive, so `major`
/// remains `1` and `minor` advances to `8`.
/// The daemon-side model probe adds `CommandBody::ProbeModel` and
/// `Payload::ModelProbes`, moving "can this model serve a run?" to the side
/// that owns `models.toml` and the credentials behind it. Both are additive
/// (an older daemon never sends the reply, and a client only sends the command
/// to a daemon whose negotiated minor is ≥ 9), so `major` remains `1` and
/// `minor` advances to `9`.
/// Session-scoped knowledge browsing moves read access behind the authenticated
/// daemon boundary; additive `ReadKnowledge` and `KnowledgePage` require 1.10.
/// Optional `RunUsage.coverage` records measured versus unmeasured requests.
/// Old records retain unknown coverage; older clients ignore this additive field.
/// Tool display refinements add `ToolUpdated`. Older clients fold this event
/// to `Unknown` while retaining the original start and completion events.
pub const PROTOCOL_V1: ProtocolVersion = ProtocolVersion {
    major: 1,
    minor: 11,
};

/// The lowest negotiated minor that understands
/// [`CommandBody::ProbeModel`](crate::CommandBody::ProbeModel). A client MUST
/// check this before sending: an older daemon folds the unknown variant into
/// `CommandBody::Unknown` and rejects it, which reads to a user as "this model
/// is broken" rather than "this daemon is older".
pub const PROBE_MODEL_MIN_MINOR: u16 = 9;

impl ProtocolVersion {
    /// Two versions are compatible when their major versions match.
    pub fn compatible_with(&self, other: &ProtocolVersion) -> bool {
        self.major == other.major
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}
