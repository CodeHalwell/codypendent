//! The message envelope and the Phase 0 payload set.
//!
//! Every frame on the wire is one serialized `Envelope`. The payload enum
//! grows in later phases (sessions, runs, subscriptions, approvals, ...);
//! Phase 0 ships only daemon lifecycle messages.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::blackboard::BlackboardItemView;
use crate::catchup::Catchup;
use crate::command::Command;
use crate::document::{DocumentLeaseGrant, DocumentSync};
use crate::error::CodypendentError;
use crate::events::SessionEvent;
use crate::handshake::{ClientHello, ServerHello};
use crate::ids::{
    ApprovalId, ClientId, CommandId, DaemonInstanceId, DocumentId, MessageId, RunId, SessionId,
    WorkspaceId,
};
use crate::remote_ui::UiWireMessage;
use crate::version::{ProtocolVersion, PROTOCOL_V1};
use crate::workflow::{WorkflowCatalog, WorkflowEvent, WorkflowRunSnapshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct Envelope {
    pub protocol_version: ProtocolVersion,
    pub message_id: MessageId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<MessageId>,
    pub client_id: ClientId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    pub payload: Payload,
}

impl Envelope {
    /// Build a new request envelope from a client.
    pub fn request(client_id: ClientId, payload: Payload) -> Self {
        Self {
            protocol_version: PROTOCOL_V1,
            message_id: MessageId::new(),
            correlation_id: None,
            client_id,
            workspace_id: None,
            session_id: None,
            sequence: None,
            payload,
        }
    }

    /// Build a reply correlated to `request`.
    pub fn reply_to(request: &Envelope, payload: Payload) -> Self {
        Self {
            protocol_version: PROTOCOL_V1,
            message_id: MessageId::new(),
            correlation_id: Some(request.message_id),
            client_id: request.client_id,
            workspace_id: request.workspace_id,
            session_id: request.session_id,
            sequence: None,
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(tag = "type")]
pub enum Payload {
    KnowledgePage {
        command_id: CommandId,
        page: Box<crate::knowledge::KnowledgePage>,
    },
    /// Liveness probe.
    Ping,
    Pong,
    /// Ask the daemon to describe itself.
    DaemonStatusRequest,
    DaemonStatusResponse(DaemonStatus),
    /// Ask the daemon to shut down gracefully.
    Shutdown,
    ShutdownAck,
    /// Ask the daemon to shut down ONLY if it is idle (no active runs) — the
    /// daemon-side half of the auto-restart safety gate. Unlike [`Payload::Shutdown`]
    /// (which stops unconditionally), the daemon re-checks its own
    /// `active_run_count` atomically against concurrent run admission and, when
    /// any run is active, refuses with [`Payload::ShutdownRefused`] instead of
    /// killing in-flight work. Introduced at protocol v1.3; a client only sends
    /// it to a daemon whose negotiated minor is ≥ 3.
    ShutdownIfIdle,
    /// The daemon declined a [`Payload::ShutdownIfIdle`] because a run is active;
    /// carries the count it observed so the client can warn precisely. The
    /// daemon keeps running.
    ShutdownRefused {
        #[serde(default)]
        active_run_count: u64,
    },
    /// Structured protocol-level error (never parse human text to decide
    /// behaviour).
    Error(ProtocolError),

    // --- Phase 1: handshake, commands, events, catch-up ---
    /// Client's opening handshake message.
    ClientHello(ClientHello),
    /// Daemon's handshake reply.
    ServerHello(ServerHello),
    /// A client request for a state change (idempotent).
    Command(Command),
    /// The command was accepted and applied; carries the resulting ledger
    /// sequence when the command produced events.
    CommandAccepted {
        command_id: CommandId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sequence: Option<u64>,
        /// The run a `StartRun` created, so the issuing client can bind to
        /// exactly that run (never a concurrent client's run that happened to
        /// start first). Absent on every other command; defaulted for wire
        /// compatibility with older daemons.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_run: Option<RunId>,
    },
    /// The command was rejected; carries the full structured error.
    CommandRejected(CodypendentError),
    /// An `AcquireDocumentLease` command was accepted; carries the minted lease id
    /// and expiry the client holds to renew and release (Phase 4 STEP 4.3). A
    /// distinct reply from `CommandAccepted` because the client needs the granted
    /// lease back, not just an acknowledgement.
    DocumentLeaseGranted {
        command_id: CommandId,
        grant: DocumentLeaseGrant,
    },
    /// A `CreateDocument` command was accepted; carries the new document's id
    /// (Docs Studio creation). A distinct reply from `CommandAccepted` for the
    /// same reason as `WorkflowRunStarted`: the client needs the id back to
    /// select, subscribe to, and edit the document it just created.
    DocumentCreated {
        command_id: CommandId,
        document_id: DocumentId,
    },
    /// Reply to `ForkSession`: the freshly created fork (Adoption 05).
    SessionForked {
        command_id: CommandId,
        session_id: SessionId,
    },
    /// Reply to `ListSessions`: sessions known to the daemon (Adoption 11 S1).
    SessionList {
        command_id: CommandId,
        sessions: Vec<crate::command::SessionSummary>,
    },
    /// Reply to `SearchSessions`.
    SessionSearchResults {
        command_id: CommandId,
        page: crate::session::SessionSearchPage,
    },
    /// Reply to `ReadSessionHistory`.
    SessionHistory {
        command_id: CommandId,
        session_id: SessionId,
        page: crate::session::SessionHistoryPage,
    },
    /// Authoritative projection after a lifecycle mutation.
    SessionLifecycleApplied {
        command_id: CommandId,
        session: crate::session::SessionSummary,
    },
    /// Tombstone receipt after retention-aware deletion.
    SessionDeleted {
        command_id: CommandId,
        session_id: SessionId,
        #[serde(default)]
        tombstoned: bool,
    },
    /// Artifact created by a lifecycle export action.
    SessionExported {
        command_id: CommandId,
        artifact: crate::artifact::ArtifactRef,
    },
    /// An editor-native action entered the ordinary run pipeline.
    EditorActionAccepted {
        command_id: CommandId,
        run_id: RunId,
    },
    /// Reply to `ListInbox`.
    InboxPage {
        command_id: CommandId,
        page: crate::inbox::InboxPage,
    },
    /// Authoritative projection after an inbox mutation.
    InboxEntryApplied {
        command_id: CommandId,
        entry: crate::inbox::InboxEntry,
    },
    /// Reply to `QueryAnalytics`.
    AnalyticsResults {
        command_id: CommandId,
        page: crate::analytics::AnalyticsPage,
    },
    /// Reply to `ExportAnalytics`.
    AnalyticsExported {
        command_id: CommandId,
        result: crate::analytics::AnalyticsExportResult,
    },
    /// Created, read, or updated automation binding.
    AutomationBindingResult {
        command_id: CommandId,
        binding: crate::automation::AutomationBinding,
    },
    /// Cursor page returned by an automation list request.
    AutomationBindingPage {
        command_id: CommandId,
        page: crate::automation::AutomationBindingPage,
    },
    /// Durable deletion receipt for an automation binding.
    AutomationBindingDeleted {
        command_id: CommandId,
        binding_id: crate::AutomationBindingId,
    },
    /// Created, read, or updated analytics budget.
    AnalyticsBudgetResult {
        command_id: CommandId,
        budget: crate::analytics::AnalyticsBudget,
    },
    /// Page returned by an analytics budget list request.
    AnalyticsBudgetPage {
        command_id: CommandId,
        page: crate::analytics::AnalyticsBudgetPage,
    },
    /// Durable deletion receipt for an analytics budget.
    AnalyticsBudgetDeleted {
        command_id: CommandId,
        budget_id: String,
    },
    /// Completed bundle export receipt.
    BundleExported {
        command_id: CommandId,
        receipt: crate::bundle::BundleExportReceipt,
    },
    /// Completed bundle import receipt.
    BundleImported {
        command_id: CommandId,
        receipt: crate::bundle::BundleImportReceipt,
    },
    /// Reply to `SearchWorkspaceFiles`: fuzzy matching file paths (Adoption 11 M2).
    FileSearchResults {
        command_id: CommandId,
        query: String,
        matches: Vec<crate::command::FileMatchWire>,
        #[serde(default)]
        truncated: bool,
    },
    /// A `CheckDocuments` command's reply: the staleness sweep's counts
    /// (`/update-docs` glue over the Phase 4 STEP 4.6 engine). A distinct
    /// reply from `CommandAccepted` because the client reports the counts to
    /// the operator, not just an acknowledgement.
    DocsCheckCompleted {
        command_id: CommandId,
        /// Documents the sweep examined.
        documents_checked: u64,
        /// Symbol links resolved (and persisted) against the code graph.
        links_resolved: u64,
        /// Staleness findings (signature changed / symbol disappeared).
        stale_findings: u64,
        /// Maintain-mode suggestions filed from those findings (never direct
        /// edits; each still needs a human accept).
        suggestions_filed: u64,
    },
    /// A `StartWorkflow` command was accepted; carries the new durable workflow-run
    /// id (Phase 5 STEP 5.2). A distinct reply from `CommandAccepted` because the
    /// client needs the run id back to track / show the run it just started.
    WorkflowRunStarted {
        command_id: CommandId,
        workflow_run_id: String,
    },
    /// A `PublishDocument` command was accepted: its deterministic plan was
    /// computed and a durable approval parked (Phase 4 STEP 4.4). Carries the
    /// plan's human-reviewable content **verbatim** — a short target
    /// description, the changed files, and the resulting Git action — so the
    /// client can render it immediately; the same content the parked
    /// `ApprovalRequested` event's approval card carries. Nothing is written
    /// yet: the approval must still resolve via the ordinary
    /// `ResolveApproval` command, and a rejection performs no write.
    DocumentPublishRequested {
        command_id: CommandId,
        approval_id: ApprovalId,
        target: String,
        changed_files: Vec<String>,
        git_action: String,
    },
    /// A `ProposePromotion` command was accepted; carries the new promotion
    /// candidate's id (Phase 7 STEP 7.5). A distinct reply from
    /// `CommandAccepted` for the same reason as `WorkflowRunStarted`: the
    /// client needs the id back to advance/approve/roll back that exact
    /// candidate.
    PromotionProposed {
        command_id: CommandId,
        candidate_id: String,
    },
    /// Result of a daemon-owned Remote UI plugin lifecycle command.
    UiPluginLifecycle {
        command_id: CommandId,
        plugins: Vec<crate::command::UiPluginLifecycleStatus>,
    },
    /// A `PutArtifact` command's reply (voice v1, rubric 8): the freshly minted
    /// [`ArtifactRef`](crate::artifact::ArtifactRef) for the stored bytes. A
    /// distinct reply from `CommandAccepted` because the client needs the ref
    /// back — it is what an [`InputEnvelope`](crate::input::InputEnvelope)
    /// audio block references on the next `SubmitUserInput`.
    ArtifactStored {
        command_id: CommandId,
        artifact: crate::artifact::ArtifactRef,
    },
    /// Bounded bytes returned for `ReadArtifact`. `offset` is the requested
    /// starting position; `eof` tells the caller whether another request is
    /// needed. The stored digest is repeated for end-to-end verification.
    ArtifactChunk {
        artifact_id: crate::ids::ArtifactId,
        offset: u64,
        bytes_base64: String,
        eof: bool,
        sha256: String,
    },
    /// An `InspectMemory` / `CorrectMemory` reply: the memory as it stands
    /// *now*. For a correction that is the NEW record (the correction
    /// supersedes rather than overwrites), so a client re-renders from the
    /// reply instead of guessing what the store did.
    Memory {
        command_id: CommandId,
        memory: crate::memory::MemoryView,
    },
    /// A `ForgetMemory` / `ForgetMemoryScope` reply: a content-free audit of
    /// what was removed. Ids only — the statements are gone, and echoing them
    /// back would defeat the point of forgetting them.
    MemoryForgotten {
        command_id: CommandId,
        forgotten: Vec<crate::ids::MemoryId>,
    },
    /// An `OpenMemoryEvidence` reply: the actual content behind one of a
    /// memory's provenance refs (Chapter 06's "every retrieved memory opens its
    /// source").
    MemoryEvidence {
        command_id: CommandId,
        evidence: crate::memory::MemoryEvidence,
    },
    /// A `ReadBlackboard` command's reply (Phase 5 STEP 5.3): the matching typed
    /// artifacts on the workflow run's board. A distinct reply from
    /// `CommandAccepted` because the client needs the items back, not just an
    /// acknowledgement.
    BlackboardItems {
        command_id: CommandId,
        items: Vec<BlackboardItemView>,
    },
    /// One blackboard artifact that just landed on a run's board, delivered to the
    /// clients subscribed to it (`Subscription::Blackboard`) as the run's agents
    /// post/supersede (Phase 5 STEP 5.3). The item carries its own
    /// `workflow_run_id`, so — like [`DocumentSync`](Payload::DocumentSync) — the
    /// frame is not session-scoped; a receiver merges it into the run's board by id
    /// (a superseding revision arrives as its own delivery).
    BlackboardPosted(BlackboardItemView),
    /// A `PostBlackboardItem` / `UpdateBlackboardItem` command's reply (Phase B
    /// kanban): the stored (or superseding) item. A distinct reply from
    /// `CommandAccepted` because the writing client needs the minted item id and
    /// revision back — e.g. to select the card it just created.
    BlackboardItemApplied {
        command_id: CommandId,
        item: BlackboardItemView,
    },
    /// A `ReadSessionEvents` command's reply: one ascending page of the
    /// session's durable event history. `through` is the highest sequence in
    /// the page (equal to the request's `after_sequence` when the page is
    /// empty) — the client passes it back as the next `after_sequence`;
    /// `has_more` says whether events beyond `through` existed at read time, so
    /// a pager knows when to stop without a probe read.
    SessionEventsPage {
        command_id: CommandId,
        session_id: SessionId,
        events: Vec<SessionEvent>,
        through: u64,
        has_more: bool,
    },
    /// Repository-scoped, bounded metadata for named workflow selection (protocol 1.12).
    WorkflowCatalog {
        command_id: CommandId,
        catalog: WorkflowCatalog,
    },
    /// A `ReadWorkflowRun` command's reply (Phase 5 STEP 5.2 / T9): the run's
    /// observability snapshot — its current phase plus every node's full current
    /// view. A distinct reply from `CommandAccepted` because the client needs the
    /// snapshot back as its live-stream baseline, not just an acknowledgement.
    WorkflowRunSnapshot {
        command_id: CommandId,
        snapshot: WorkflowRunSnapshot,
    },
    /// One live event on a workflow run's node-lifecycle stream, delivered to the
    /// clients subscribed to it (`Subscription::Workflow`) as the driver advances the
    /// graph (Phase 5 STEP 5.2 / T9): a node transition (carrying the node's full new
    /// view) or a run-phase change. The event carries its own `workflow_run_id`, so
    /// the frame is not session-scoped; a receiver merges a node transition into the
    /// run's graph by `node_id` (each transition is full-state, so an overlap is
    /// harmless). Wrapped in a named field so the internally-tagged event's `type`
    /// tag never collides with the payload tag (as [`Catchup`](Payload::Catchup) is).
    WorkflowEvent {
        event: WorkflowEvent,
    },
    /// A persisted session event published to a subscribed client.
    Event(SessionEvent),
    /// A collaborative document's CRDT sync update, delivered to the clients
    /// subscribed to that document (`Subscription::Document`) as the
    /// authoritative replica advances (Phase 4 STEP 4.3). Opaque CRDT bytes ride
    /// in [`DocumentSync::update`]; a receiver merges them into its local replica.
    DocumentSync(DocumentSync),
    /// A `BuildCodeGraph` reply: what the on-demand fold actually saw. A
    /// distinct reply from `CommandAccepted` because the report *is* the
    /// feature — a build that only acknowledged itself would leave an empty
    /// graph exactly as unexplained as it was before the command existed.
    CodeGraphBuilt {
        command_id: CommandId,
        report: Box<crate::codegraph::CodeGraphScanReport>,
    },
    /// A `ProbeModel` reply: one row per model probed, in the order the
    /// daemon holds them. A single-model probe answers with one row; a probe
    /// that named no model answers with every configured model, including the
    /// ones that are not ready — a client showing a readiness column needs the
    /// failures most.
    ModelProbes {
        command_id: CommandId,
        models: Vec<crate::model::ModelProbe>,
    },
    /// A `ReadCodeGraphStatus` reply: what the stored graph holds right now,
    /// and whether it still describes the working tree.
    CodeGraphStatus {
        command_id: CommandId,
        status: Box<crate::codegraph::CodeGraphStatusView>,
    },
    /// A `ReadCodeGraph` reply: one filtered page of nodes and edges.
    CodeGraphPage {
        command_id: CommandId,
        page: Box<crate::codegraph::CodeGraphPage>,
    },

    /// Attach-time catch-up (missed events or a snapshot). Wrapped in a named
    /// field so its internal `type` tag never collides with the payload tag.
    Catchup {
        catchup: Catchup,
    },

    /// Renderer-independent component traffic. The nested message has its own
    /// kind and revision semantics; keeping it inside one envelope variant lets
    /// terminal, VS Code, and future graphical clients share the ordinary
    /// authenticated connection without granting component code direct access
    /// to the daemon command channel.
    RemoteUi {
        message: Box<UiWireMessage>,
    },

    /// Reply to `MarketplaceSearch` or package listing.
    MarketplaceSearchResults {
        command_id: CommandId,
        packages: Vec<crate::marketplace::MarketplacePackageView>,
    },
    /// Reply to a marketplace package lifecycle mutation (install, update, enable, disable, revoke).
    MarketplaceLifecycle {
        command_id: CommandId,
        package: crate::marketplace::MarketplacePackageView,
    },
    /// Reply to `SecretDeclare`: the newly registered secret reference metadata.
    SecretReferenceApplied {
        command_id: CommandId,
        reference: crate::secrets::SecretReferenceView,
    },
    /// Reply to `SecretBind`: the short-lived context-bound lease handle.
    SecretLeaseBound {
        command_id: CommandId,
        lease_id: String,
        expires_at: String,
    },
    /// Reply to `SecretList`: declared secret reference metadata list.
    SecretReferenceList {
        command_id: CommandId,
        references: Vec<crate::secrets::SecretReferenceView>,
    },
    /// Reply to `SecretRevoke`: durable revocation acknowledgement.
    SecretReferenceRevoked {
        command_id: CommandId,
        reference_id: String,
    },

    // --- Milestone 6: Federation & Campaigns ---
    /// Reply to `EstablishFederatedIdentity`: the established identity view.
    FederatedIdentityEstablished {
        command_id: CommandId,
        identity: Box<crate::federated_graph::FederatedRepositoryIdentityView>,
    },
    /// Reply to `GetPublicationPolicy` or `SetPublicationPolicy`: policy view.
    PublicationPolicy {
        command_id: CommandId,
        policy: Box<crate::federated_graph::GraphPublicationPolicyView>,
    },
    /// Reply to `PublishGraphFacts`: summary of published facts batch.
    GraphFactsPublished {
        command_id: CommandId,
        summary: Box<crate::federated_graph::PublicationBatchSummary>,
    },
    /// Reply to `TombstoneGraphFacts`: confirmed tombstone id.
    GraphTombstoned {
        command_id: CommandId,
        tombstone_id: String,
    },
    /// Reply to `QueryFederatedGraph`: filtered page of shared nodes and edges.
    FederatedGraphResult {
        command_id: CommandId,
        page: Box<crate::federated_graph::FederatedGraphPage>,
    },
    /// Reply to `QueryBlastRadius`: transitive blast radius report.
    BlastRadiusResult {
        command_id: CommandId,
        report: Box<crate::federated_graph::BlastRadiusReport>,
    },
    /// Reply to `PlanMigration`: architectural migration plan report.
    MigrationPlanResult {
        command_id: CommandId,
        report: Box<crate::federated_graph::MigrationPlanReport>,
    },
    /// Reply to `SuggestReviewers`: reviewer recommendations with confidence and reasons.
    ReviewerSuggestionsResult {
        command_id: CommandId,
        suggestions: Box<crate::federated_graph::ReviewerSuggestions>,
    },
    /// Reply to `CreateCampaign`: created campaign id.
    CampaignCreated {
        command_id: CommandId,
        campaign_id: String,
    },
    /// Reply to `GetCampaign`: full campaign details and child run/approval state.
    CampaignDetail {
        command_id: CommandId,
        detail: Box<crate::federated_graph::CampaignDetailView>,
    },
    /// Reply to `ListCampaigns`: summary list of campaigns.
    CampaignList {
        command_id: CommandId,
        campaigns: Vec<crate::federated_graph::CampaignView>,
    },
    /// Reply to `ExecuteCampaign`: updated campaign execution state.
    CampaignExecuted {
        command_id: CommandId,
        campaign: Box<crate::federated_graph::CampaignView>,
    },
    /// Reply to `CancelCampaign`: cancelled campaign id.
    CampaignCancelled {
        command_id: CommandId,
        campaign_id: String,
    },

    /// Forward-compatibility fallback: a payload tag this build does not know
    /// deserializes to `Unknown` instead of failing the whole frame, so the
    /// receiver can reject it structurally and keep the connection alive
    /// (additive 1.x payloads must never break an older peer).
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod artifact_chunk_tests {
    use super::*;
    use crate::ArtifactId;

    #[test]
    fn artifact_chunk_round_trips() {
        let payload = Payload::ArtifactChunk {
            artifact_id: ArtifactId::new(),
            offset: 5,
            bytes_base64: "YWJj".into(),
            eof: true,
            sha256: "ab".repeat(32),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let decoded: Payload = serde_json::from_str(&json).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            serde_json::to_value(payload).unwrap()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::ClientCapabilities;
    use crate::command::CommandBody;
    use crate::ids::CommandId;
    use crate::run::AgentMode;

    fn round_trip_payload(payload: Payload) -> Payload {
        let envelope = Envelope::request(ClientId::new(), payload);
        let json = serde_json::to_string(&envelope).expect("serialize");
        let parsed: Envelope = serde_json::from_str(&json).expect("deserialize");
        parsed.payload
    }

    #[test]
    fn unknown_payload_tag_deserializes_to_unknown() {
        let request = Envelope::request(ClientId::new(), Payload::Ping);
        let mut value = serde_json::to_value(&request).expect("serialize");
        value["payload"] = serde_json::json!({ "type": "FromTheFuture", "detail": 42 });
        let parsed: Envelope = serde_json::from_value(value).expect("future payloads must parse");
        assert!(matches!(parsed.payload, Payload::Unknown));
    }

    #[test]
    fn phase0_payloads_still_round_trip() {
        assert!(matches!(round_trip_payload(Payload::Ping), Payload::Ping));
        assert!(matches!(round_trip_payload(Payload::Pong), Payload::Pong));
        assert!(matches!(
            round_trip_payload(Payload::DaemonStatusRequest),
            Payload::DaemonStatusRequest
        ));
        assert!(matches!(
            round_trip_payload(Payload::Shutdown),
            Payload::Shutdown
        ));
        assert!(matches!(
            round_trip_payload(Payload::ShutdownAck),
            Payload::ShutdownAck
        ));
        assert!(matches!(
            round_trip_payload(Payload::ShutdownIfIdle),
            Payload::ShutdownIfIdle
        ));
        match round_trip_payload(Payload::ShutdownRefused {
            active_run_count: 3,
        }) {
            Payload::ShutdownRefused { active_run_count } => assert_eq!(active_run_count, 3),
            other => panic!("expected ShutdownRefused, got {other:?}"),
        }
    }

    #[test]
    fn shutdown_refused_active_count_defaults_when_absent() {
        // A peer that omits `active_run_count` (or a future minimal encoder)
        // still parses — the field is `#[serde(default)]`.
        let json = serde_json::json!({ "type": "ShutdownRefused" });
        match serde_json::from_value::<Payload>(json).expect("parse legacy ShutdownRefused") {
            Payload::ShutdownRefused { active_run_count } => assert_eq!(active_run_count, 0),
            other => panic!("expected ShutdownRefused, got {other:?}"),
        }
    }

    #[test]
    fn phase1_handshake_payloads_round_trip() {
        let hello = Payload::ClientHello(ClientHello {
            client_name: "cli".to_string(),
            client_version: "0.1.0".to_string(),
            supported_protocols: vec![PROTOCOL_V1],
            capabilities: ClientCapabilities::default(),
            resume_token: None,
        });
        assert!(matches!(round_trip_payload(hello), Payload::ClientHello(_)));

        let server_hello = Payload::ServerHello(ServerHello {
            selected_protocol: PROTOCOL_V1,
            daemon_version: "0.1.0".to_string(),
            daemon_instance: DaemonInstanceId::new(),
            heartbeat_interval_ms: 15_000,
            resume_token: None,
            build_id: "0.1.0+a1b2c3d4e5f6".to_string(),
        });
        assert!(matches!(
            round_trip_payload(server_hello),
            Payload::ServerHello(_)
        ));
    }

    #[test]
    fn daemon_status_round_trips_with_the_new_additive_fields() {
        let original = DaemonStatus {
            daemon_version: "0.1.0".to_string(),
            protocol_version: PROTOCOL_V1,
            instance_id: DaemonInstanceId::new(),
            pid: 4242,
            started_at: chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            uptime_seconds: 3600,
            boot_count: 1,
            database_path: "/home/user/.local/share/codypendent/codypendent.db".to_string(),
            socket_path: "/home/user/.local/share/codypendent/run/daemon.sock".to_string(),
            session_count: 2,
            build_id: "0.1.0+a1b2c3d4e5f6".to_string(),
            active_run_count: 3,
            integration_issues: vec!["MCP server `local` failed to start".to_string()],
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: DaemonStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.build_id, original.build_id);
        assert_eq!(parsed.active_run_count, original.active_run_count);
        assert_eq!(parsed.integration_issues, original.integration_issues);
    }

    #[test]
    fn daemon_status_legacy_payload_without_new_fields_defaults() {
        // An older daemon's status response (no build_id/active_run_count on
        // the wire) still parses — both default (""/0).
        let legacy = serde_json::json!({
            "daemon_version": "0.1.0",
            "protocol_version": PROTOCOL_V1,
            "instance_id": DaemonInstanceId::new(),
            "pid": 4242,
            "started_at": "2026-01-01T00:00:00Z",
            "uptime_seconds": 3600u64,
            "boot_count": 1,
            "database_path": "/home/user/.local/share/codypendent/codypendent.db",
            "socket_path": "/home/user/.local/share/codypendent/run/daemon.sock",
            "session_count": 2,
        });
        let parsed: DaemonStatus = serde_json::from_value(legacy).expect("legacy status parses");
        assert_eq!(parsed.build_id, "");
        assert_eq!(parsed.active_run_count, 0);
        assert!(parsed.integration_issues.is_empty());
    }

    #[test]
    fn phase1_command_payloads_round_trip() {
        let command = Payload::Command(Command {
            command_id: CommandId::new(),
            idempotency_key: "idem".to_string(),
            expected_revision: None,
            body: CommandBody::StartRun {
                session_id: SessionId::new(),
                objective: "fix it".to_string(),
                mode: AgentMode::Build,
                repository: None,
                model: None,
            },
        });
        match round_trip_payload(command) {
            Payload::Command(cmd) => {
                assert!(matches!(cmd.body, CommandBody::StartRun { .. }));
            }
            other => panic!("expected Command, got {other:?}"),
        }

        let accepted = Payload::CommandAccepted {
            command_id: CommandId::new(),
            sequence: Some(7),
            created_run: None,
        };
        assert!(matches!(
            round_trip_payload(accepted),
            Payload::CommandAccepted {
                sequence: Some(7),
                ..
            }
        ));

        let rejected = Payload::CommandRejected(CodypendentError::new(
            "protocol.role-denied",
            "observers may not start runs",
            false,
        ));
        match round_trip_payload(rejected) {
            Payload::CommandRejected(error) => assert_eq!(error.code, "protocol.role-denied"),
            other => panic!("expected CommandRejected, got {other:?}"),
        }

        let cmd_id = CommandId::new();
        let sess_id = SessionId::new();
        let forked = Payload::SessionForked {
            command_id: cmd_id,
            session_id: sess_id,
        };
        match round_trip_payload(forked) {
            Payload::SessionForked {
                command_id,
                session_id,
            } => {
                assert_eq!(command_id, cmd_id);
                assert_eq!(session_id, sess_id);
            }
            other => panic!("expected SessionForked, got {other:?}"),
        }
    }

    #[test]
    fn phase1_event_and_catchup_payloads_round_trip() {
        use crate::events::{Actor, EventBody, SessionEvent};
        use chrono::Utc;

        let event = SessionEvent {
            sequence: 3,
            occurred_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            actor: Actor::System,
            body: EventBody::SessionClosed,
        };
        match round_trip_payload(Payload::Event(event)) {
            Payload::Event(ev) => assert!(matches!(ev.body, EventBody::SessionClosed)),
            other => panic!("expected Event, got {other:?}"),
        }

        let catchup = Payload::Catchup {
            catchup: Catchup::Events {
                from: 1,
                through: 3,
                events: vec![],
            },
        };
        match round_trip_payload(catchup) {
            Payload::Catchup { catchup } => {
                assert!(matches!(catchup, Catchup::Events { from: 1, .. }));
            }
            other => panic!("expected Catchup, got {other:?}"),
        }
    }

    #[test]
    fn document_lease_granted_payload_round_trips() {
        use crate::document::DocumentLeaseGrant;
        use crate::ids::DocumentId;

        let command_id = CommandId::new();
        let document_id = DocumentId::new();
        let granted = Payload::DocumentLeaseGranted {
            command_id,
            grant: DocumentLeaseGrant {
                lease_id: "lease-9".to_string(),
                document_id,
                block_id: Some("b3".to_string()),
                expires_at: Utc::now(),
            },
        };
        match round_trip_payload(granted) {
            Payload::DocumentLeaseGranted {
                command_id: id,
                grant,
            } => {
                assert_eq!(id, command_id);
                assert_eq!(grant.lease_id, "lease-9");
                assert_eq!(grant.document_id, document_id);
                assert_eq!(grant.block_id.as_deref(), Some("b3"));
            }
            other => panic!("expected DocumentLeaseGranted, got {other:?}"),
        }
    }

    #[test]
    fn document_created_payload_round_trips() {
        let command_id = CommandId::new();
        let document_id = DocumentId::new();
        match round_trip_payload(Payload::DocumentCreated {
            command_id,
            document_id,
        }) {
            Payload::DocumentCreated {
                command_id: id,
                document_id: doc,
            } => {
                assert_eq!(id, command_id);
                assert_eq!(doc, document_id);
            }
            other => panic!("expected DocumentCreated, got {other:?}"),
        }
    }

    #[test]
    fn docs_check_completed_payload_round_trips() {
        let command_id = CommandId::new();
        match round_trip_payload(Payload::DocsCheckCompleted {
            command_id,
            documents_checked: 4,
            links_resolved: 9,
            stale_findings: 2,
            suggestions_filed: 2,
        }) {
            Payload::DocsCheckCompleted {
                command_id: id,
                documents_checked,
                links_resolved,
                stale_findings,
                suggestions_filed,
            } => {
                assert_eq!(id, command_id);
                assert_eq!(documents_checked, 4);
                assert_eq!(links_resolved, 9);
                assert_eq!(stale_findings, 2);
                assert_eq!(suggestions_filed, 2);
            }
            other => panic!("expected DocsCheckCompleted, got {other:?}"),
        }
    }

    #[test]
    fn workflow_run_started_payload_round_trips() {
        let command_id = CommandId::new();
        let started = Payload::WorkflowRunStarted {
            command_id,
            workflow_run_id: "0192abcd-run".to_string(),
        };
        match round_trip_payload(started) {
            Payload::WorkflowRunStarted {
                command_id: id,
                workflow_run_id,
            } => {
                assert_eq!(id, command_id);
                assert_eq!(workflow_run_id, "0192abcd-run");
            }
            other => panic!("expected WorkflowRunStarted, got {other:?}"),
        }
    }

    #[test]
    fn document_publish_requested_payload_round_trips() {
        let command_id = CommandId::new();
        let approval_id = ApprovalId::new();
        let requested = Payload::DocumentPublishRequested {
            command_id,
            approval_id,
            target: "repository file docs/architecture.md".to_string(),
            changed_files: vec!["docs/architecture.md".to_string()],
            git_action:
                "write docs/architecture.md in the working tree (approval-gated change set)"
                    .to_string(),
        };
        match round_trip_payload(requested) {
            Payload::DocumentPublishRequested {
                command_id: id,
                approval_id: approval,
                target,
                changed_files,
                git_action,
            } => {
                assert_eq!(id, command_id);
                assert_eq!(approval, approval_id);
                assert_eq!(target, "repository file docs/architecture.md");
                assert_eq!(changed_files, vec!["docs/architecture.md".to_string()]);
                assert!(git_action.contains("docs/architecture.md"));
            }
            other => panic!("expected DocumentPublishRequested, got {other:?}"),
        }
    }

    #[test]
    fn artifact_stored_payload_round_trips() {
        use crate::artifact::{ArtifactRef, DataClassification};
        use crate::ids::ArtifactId;

        let command_id = CommandId::new();
        let artifact = ArtifactRef {
            id: ArtifactId::new(),
            media_type: "audio/wav".to_string(),
            byte_length: 64_000,
            sha256: "c".repeat(64),
            sensitivity: DataClassification::Confidential,
        };
        let stored = Payload::ArtifactStored {
            command_id,
            artifact: artifact.clone(),
        };
        match round_trip_payload(stored) {
            Payload::ArtifactStored {
                command_id: id,
                artifact: got,
            } => {
                assert_eq!(id, command_id);
                assert_eq!(got, artifact);
            }
            other => panic!("expected ArtifactStored, got {other:?}"),
        }
    }

    #[test]
    fn promotion_proposed_payload_round_trips() {
        let command_id = CommandId::new();
        let proposed = Payload::PromotionProposed {
            command_id,
            candidate_id: "cand-0192abcd".to_string(),
        };
        match round_trip_payload(proposed) {
            Payload::PromotionProposed {
                command_id: id,
                candidate_id,
            } => {
                assert_eq!(id, command_id);
                assert_eq!(candidate_id, "cand-0192abcd");
            }
            other => panic!("expected PromotionProposed, got {other:?}"),
        }
    }

    #[test]
    fn blackboard_payloads_round_trip() {
        use crate::blackboard::BlackboardItemView;
        use serde_json::json;

        let item = BlackboardItemView {
            id: "0192-item".to_string(),
            workflow_run_id: "wfrun-abc".to_string(),
            kind: "finding".to_string(),
            payload: json!({ "summary": "root cause found" }),
            author: json!({ "role": "investigator", "node_id": "diagnose" }),
            confidence: Some(0.9),
            evidence: vec![json!({ "path": "src/lib.rs", "line": 7 })],
            revision: 2,
            superseded_by: None,
            board_scope: None,
            status: None,
            assignee: None,
            ordinal: None,
        };

        // The read-command reply carries a list of items.
        let command_id = CommandId::new();
        match round_trip_payload(Payload::BlackboardItems {
            command_id,
            items: vec![item.clone()],
        }) {
            Payload::BlackboardItems {
                command_id: id,
                items,
            } => {
                assert_eq!(id, command_id);
                assert_eq!(items, vec![item.clone()]);
            }
            other => panic!("expected BlackboardItems, got {other:?}"),
        }

        // The subscription delivers one posted item.
        match round_trip_payload(Payload::BlackboardPosted(item.clone())) {
            Payload::BlackboardPosted(delivered) => assert_eq!(delivered, item),
            other => panic!("expected BlackboardPosted, got {other:?}"),
        }

        // The client-write reply carries the stored item back (Phase B kanban).
        let command_id = CommandId::new();
        match round_trip_payload(Payload::BlackboardItemApplied {
            command_id,
            item: item.clone(),
        }) {
            Payload::BlackboardItemApplied {
                command_id: id,
                item: applied,
            } => {
                assert_eq!(id, command_id);
                assert_eq!(applied, item);
            }
            other => panic!("expected BlackboardItemApplied, got {other:?}"),
        }
    }

    #[test]
    fn session_events_page_payload_round_trips() {
        use crate::events::{Actor, EventBody, SessionEvent};
        use chrono::Utc;

        let command_id = CommandId::new();
        let session_id = SessionId::new();
        let page = Payload::SessionEventsPage {
            command_id,
            session_id,
            events: vec![SessionEvent {
                sequence: 501,
                occurred_at: Utc::now(),
                causation_id: None,
                correlation_id: None,
                actor: Actor::System,
                body: EventBody::SessionClosed,
            }],
            through: 501,
            has_more: true,
        };
        match round_trip_payload(page) {
            Payload::SessionEventsPage {
                command_id: id,
                session_id: sid,
                events,
                through,
                has_more,
            } => {
                assert_eq!(id, command_id);
                assert_eq!(sid, session_id);
                assert_eq!(events.len(), 1);
                assert_eq!(through, 501);
                assert!(has_more);
            }
            other => panic!("expected SessionEventsPage, got {other:?}"),
        }
    }

    #[test]
    fn workflow_payloads_round_trip() {
        use crate::workflow::{
            WorkflowEvent, WorkflowNodeState, WorkflowNodeView, WorkflowRunPhase,
            WorkflowRunSnapshot,
        };
        use serde_json::json;

        let node = WorkflowNodeView {
            workflow_run_id: "wfrun-abc".to_string(),
            node_id: "inspect".to_string(),
            state: WorkflowNodeState::Completed,
            attempt: 1,
            cost: Some(json!({ "wall_time_secs": 3, "tool_calls": 1 })),
            error: None,
            warnings: Vec::new(),
            depends_on: Vec::new(),
        };

        // The read-command reply carries a run snapshot.
        let command_id = CommandId::new();
        let snapshot = WorkflowRunSnapshot {
            workflow_run_id: "wfrun-abc".to_string(),
            phase: WorkflowRunPhase::Running,
            nodes: vec![node.clone()],
        };
        match round_trip_payload(Payload::WorkflowRunSnapshot {
            command_id,
            snapshot: snapshot.clone(),
        }) {
            Payload::WorkflowRunSnapshot {
                command_id: id,
                snapshot: got,
            } => {
                assert_eq!(id, command_id);
                assert_eq!(got, snapshot);
            }
            other => panic!("expected WorkflowRunSnapshot, got {other:?}"),
        }

        // The subscription delivers one live event.
        let event = WorkflowEvent::NodeTransitioned(node);
        match round_trip_payload(Payload::WorkflowEvent {
            event: event.clone(),
        }) {
            Payload::WorkflowEvent { event: delivered } => assert_eq!(delivered, event),
            other => panic!("expected WorkflowEvent, got {other:?}"),
        }
    }

    #[test]
    fn document_sync_payload_round_trips() {
        use crate::document::DocumentSync;
        use crate::ids::DocumentId;

        let document_id = DocumentId::new();
        let sync = Payload::DocumentSync(DocumentSync {
            document_id,
            revision: 5,
            update: vec![1, 2, 3, 255],
        });
        match round_trip_payload(sync) {
            Payload::DocumentSync(delivered) => {
                assert_eq!(delivered.document_id, document_id);
                assert_eq!(delivered.revision, 5);
                assert_eq!(delivered.update, vec![1, 2, 3, 255]);
            }
            other => panic!("expected DocumentSync, got {other:?}"),
        }
    }

    #[test]
    fn remote_ui_payload_round_trips_without_flattening_nested_tags() {
        let message: UiWireMessage = serde_json::from_value(serde_json::json!({
            "kind": "error",
            "messageId": "ui-message-1",
            "error": {
                "code": "ui.test",
                "message": "safe diagnostic",
                "recoverable": true
            }
        }))
        .expect("remote UI message");
        match round_trip_payload(Payload::RemoteUi {
            message: Box::new(message.clone()),
        }) {
            Payload::RemoteUi { message: delivered } => assert_eq!(*delivered, message),
            other => panic!("expected RemoteUi, got {other:?}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct DaemonStatus {
    pub daemon_version: String,
    pub protocol_version: ProtocolVersion,
    pub instance_id: DaemonInstanceId,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub uptime_seconds: u64,
    pub boot_count: i64,
    pub database_path: String,
    pub socket_path: String,
    pub session_count: i64,
    /// The running daemon's per-build id
    /// ([`codypendent_protocol::BUILD_ID`](crate::BUILD_ID)). Defaulted for
    /// wire compatibility with daemons predating this field (mirrors
    /// [`ServerHello::build_id`](crate::ServerHello::build_id)).
    #[serde(default)]
    pub build_id: String,
    /// Count of runs in a non-terminal state (`Queued`, `Preparing`,
    /// `Running`, `WaitingForApproval`, `WaitingForUserInput`, `Paused`,
    /// `Recovering` — every [`RunState`](crate::RunState) except `Completed`,
    /// `Failed`, and `Cancelled`). Used to gate an automatic daemon restart:
    /// only an idle daemon (`active_run_count == 0`) is safe to restart
    /// without losing in-flight work. Defaulted for wire compatibility with
    /// daemons predating this field.
    #[serde(default)]
    pub active_run_count: u64,
    /// Sanitized, de-duplicated optional-integration failures observed by the
    /// running daemon. Defaulted so older daemon/client pairs remain wire
    /// compatible. Secrets and raw extension output must never enter this list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub integration_issues: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
